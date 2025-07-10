use anyhow::Context as _;
use crossterm::style::Stylize as _;
use notify::Watcher as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let test_runner = TestRunner::new()?;
    let mut modified_path_rx = watch_throttled(
        &test_runner.root_directory,
        std::time::Duration::from_millis(500),
    )?;

    while let Some(path) = modified_path_rx.recv().await {
        let path = path?;
        if !path.try_exists()? || !path.metadata().is_ok_and(|m| m.is_file() && m.len() > 0) {
            continue;
        }
        Log::Modified(path.clone()).print();
        let Some(kind) = path.extension().and_then(FileKind::from_extension) else {
            continue;
        };
        match kind {
            FileKind::Source => {
                test_runner.run_after_compiling(&path).await?;
            }
            FileKind::TestCase => {
                let Some(source) = get_source_file(&path)? else {
                    Log::SourceNotFound(path.to_path_buf()).print();
                    continue;
                };
                test_runner
                    .run_only_one_test_case(&source, TestCase::read(&path)?)
                    .await?;
            }
        }
    }
    Ok(())
}

fn watch_throttled(
    dir: &std::path::Path,
    min_interval: std::time::Duration,
) -> anyhow::Result<tokio::sync::mpsc::UnboundedReceiver<notify::Result<std::path::PathBuf>>> {
    let (tx_raw, rx_raw) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(tx_raw)?;
    watcher
        .watch(dir, notify::RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {dir:?}"))?;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let _watcher = watcher;
        let mut last_sent_times = std::collections::HashMap::new();
        while let Ok(res) = rx_raw.recv() {
            let now = std::time::Instant::now();
            match res {
                Ok(ev) => {
                    for path in ev.paths {
                        if last_sent_times
                            .get(&path)
                            .is_some_and(|&t| (now - t) < min_interval)
                        {
                            continue;
                        }
                        last_sent_times.insert(path.clone(), now);
                        if tx.send(Ok(path.clone())).is_err() {
                            return;
                        }
                    }
                }
                Err(err) => {
                    if tx.send(Err(err)).is_err() {
                        return;
                    }
                }
            };
        }
    });

    Ok(rx)
}

struct TestRunner {
    root_directory: std::path::PathBuf,
    bin_directory: std::path::PathBuf,
}

impl TestRunner {
    fn new() -> anyhow::Result<Self> {
        let root_directory = std::env::current_dir()?;
        let bin_directory = std::env::temp_dir().join(env!("CARGO_PKG_NAME"));
        if bin_directory.try_exists()? {
            std::fs::remove_dir_all(&bin_directory)?;
        }
        std::fs::create_dir_all(&bin_directory)?;
        Ok(Self {
            root_directory,
            bin_directory,
        })
    }

    async fn run_only_one_test_case(
        &self,
        source: &std::path::Path,
        case: TestCase,
    ) -> anyhow::Result<()> {
        let exe_path = self.get_exe_path(source);
        if !exe_path.try_exists()? && !compile_cpp(source, &exe_path)? {
            return Ok(());
        }
        Log::Testing(source.to_path_buf()).print();
        self.run_test(&exe_path, case).await?;
        Ok(())
    }

    async fn run_after_compiling(&self, source: &std::path::Path) -> anyhow::Result<()> {
        let exe_path = self.get_exe_path(source);
        if !compile_cpp(source, &exe_path)? {
            return Ok(());
        }
        Log::Testing(source.to_path_buf()).print();
        for case in get_test_cases(source)? {
            self.run_test(&exe_path, case).await?;
        }
        Ok(())
    }

    async fn run_test(&self, exe_path: &std::path::Path, case: TestCase) -> anyhow::Result<()> {
        if !exe_path.try_exists()? {
            return Ok(());
        }
        let mut child = tokio::process::Command::new(exe_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {exe_path:?}"))?;
        let cstdout = child.stdout.take().context("could not take stdout")?;
        // let cstderr = child.stderr.take().context("could not take stderr")?;
        let mut cstdin = child.stdin.take().context("could not take stdin")?;

        cstdin
            .write_all(case.input.as_bytes())
            .await
            .context("failed to write into stdin")?;

        let mut stdout_rx = create_line_rx(tokio::io::BufReader::new(cstdout));
        // let mut stderr_rx = create_line_rx(tokio::io::BufReader::new(cstderr));
        let time_limit = std::time::Duration::from_secs(3);
        let wait_result = tokio::time::timeout(time_limit, child.wait()).await;
        let mut stdout_bytes = Vec::new();
        while let Some(res) = stdout_rx.recv().await {
            let mut res = res?;
            if res.get(res.len() - 2..) == Some(b"\r\n") {
                res.pop();
                *res.last_mut().unwrap() = b'\n';
            }
            stdout_bytes.extend(res);
        }
        let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
        let Ok(exit_status) = wait_result else {
            child.kill().await?;
            Log::TestResult(TestResult::TimeLimitExceeded { stdout }).print();
            return Ok(());
        };
        let status = exit_status.with_context(|| format!("failed to execute {exe_path:?}"))?;
        if !status.success() {
            Log::TestResult(TestResult::ExecutionFailed { stdout, status }).print();
            return Ok(());
        }
        if let Some(expected_output) = case.output {
            if stdout_bytes == expected_output.as_bytes() {
                Log::TestResult(TestResult::AssertionSucceeded).print();
            } else {
                Log::TestResult(TestResult::AssertionFailed {
                    expected: expected_output,
                    actual: stdout,
                })
                .print();
            }
        } else {
            Log::TestResult(TestResult::ExecutionSucceeded { stdout }).print();
        }
        Ok(())
    }

    fn get_exe_path(&self, source: &std::path::Path) -> std::path::PathBuf {
        let mut exe_path = self.bin_directory.join(source.file_stem().unwrap());
        exe_path.set_extension(std::env::consts::EXE_EXTENSION);
        exe_path
    }
}

fn create_line_rx<R>(mut r: R) -> tokio::sync::mpsc::UnboundedReceiver<std::io::Result<Vec<u8>>>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
{
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut buf = Vec::new();
        loop {
            let res = match r.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => Ok(std::mem::take(&mut buf)),
                Err(e) => Err(e),
            };
            if tx.send(res).is_err() {
                break;
            }
        }
    });
    rx
}

#[tokio::test]
async fn test_line_receiver() {
    let mut rx = create_line_rx(std::io::Cursor::new("hello\nworld\n!"));
    assert_eq!(
        "hello\n".as_bytes().to_vec(),
        rx.recv().await.unwrap().unwrap()
    );
    assert_eq!(
        "world\n".as_bytes().to_vec(),
        rx.recv().await.unwrap().unwrap()
    );
    assert_eq!("!".as_bytes().to_vec(), rx.recv().await.unwrap().unwrap());
    assert!(rx.recv().await.is_none());
}

fn get_source_file(test_case: &std::path::Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut paths = Vec::new();
    let dir = test_case.parent().unwrap();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read directory {dir:?}"))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if entry.path().extension().and_then(FileKind::from_extension) == Some(FileKind::Source) {
            paths.push(entry.path());
        }
    }
    Ok(filter_source_file(paths, test_case))
}

fn filter_source_file(
    sources: Vec<std::path::PathBuf>,
    test_case: &std::path::Path,
) -> Option<std::path::PathBuf> {
    let mut sources: Vec<_> = sources
        .into_iter()
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            Some((p, stem))
        })
        .collect();
    let test_case_name = test_case.file_stem()?.to_str()?.to_string();
    for prefix in (1..).map_while(|i| test_case_name.get(..i)) {
        sources.retain(|p| p.1.starts_with(prefix));
        if sources.len() <= 1 {
            return sources.pop().map(|p| p.0);
        }
    }
    None
}

#[test]
fn test_filter_source_file() {
    assert_eq!(
        Some("foo/cd.cpp".into()),
        filter_source_file(
            vec![
                "foo/ab.cpp".into(),
                "foo/bc.cpp".into(),
                "foo/cd.cpp".into(),
                "foo/de.cpp".into(),
            ],
            std::path::Path::new("foo/cd2.io"),
        )
    );
    assert_eq!(
        None,
        filter_source_file(
            vec!["foo/abc.cpp".into(), "foo/abd.cpp".into()],
            std::path::Path::new("foo/ab.io"),
        )
    );
}

fn get_test_cases(source: &std::path::Path) -> anyhow::Result<Vec<TestCase>> {
    let mut paths = Vec::new();
    let dir = source.parent().unwrap();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read directory {dir:?}"))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        if entry.path().extension().and_then(FileKind::from_extension) != Some(FileKind::TestCase) {
            continue;
        }
        paths.push(entry.path());
    }
    filter_test_cases(paths, source)
        .into_iter()
        .map(|p| TestCase::read(&p))
        .collect()
}

fn filter_test_cases(
    test_cases: Vec<std::path::PathBuf>,
    source: &std::path::Path,
) -> Vec<std::path::PathBuf> {
    let Some(prefix) = source.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    test_cases
        .into_iter()
        .filter(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with(prefix))
        })
        .collect()
}

#[test]
fn test_filter_test_cases() {
    assert_eq!(
        vec![
            std::path::PathBuf::from("foo/bar.io"),
            "foo/bar0.io".into(),
            "foo/bar1.io".into(),
        ],
        filter_test_cases(
            vec![
                "foo/bar.io".into(),
                "foo/bar0.io".into(),
                "foo/bar1.io".into(),
                "foo/baz.io".into(),
            ],
            std::path::Path::new("foo/bar.cpp"),
        ),
    );
}

fn compile_cpp(source: &std::path::Path, out: &std::path::Path) -> anyhow::Result<bool> {
    Log::Compiling(source.to_path_buf()).print();
    let mut cc = std::process::Command::new("g++");
    cc.args(["-Wall", "-Wextra", "-fdiagnostics-color=always"]);
    cc.args(["-x", "c++", "-g", "-O2", "-std=gnu++20", "-static"]);
    cc.arg("-o").arg(out).arg(source);
    let output = cc
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim().to_string();
    if !output.status.success() {
        Log::CompileResult(Err(stderr)).print();
        return Ok(false);
    } else if !stderr.is_empty() {
        Log::CompileResult(Ok(Some(stderr))).print();
    }
    Ok(true)
}

#[derive(Debug, PartialEq, Eq)]
enum FileKind {
    Source,
    TestCase,
}

impl FileKind {
    fn from_extension(ext: &std::ffi::OsStr) -> Option<Self> {
        match ext.to_str()? {
            "c++" | "cpp" | "cxx" => Some(Self::Source),
            "io" => Some(Self::TestCase),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct TestCase {
    input: String,
    output: Option<String>,
}

impl TestCase {
    fn read(path: &std::path::Path) -> anyhow::Result<Self> {
        Ok(Self::parse(&std::fs::read_to_string(path)?))
    }

    fn parse(content: &str) -> Self {
        let mut lines = content.lines();
        let sep = lines.next().unwrap_or_default();
        let mut has_output = false;
        let mut input = String::new();
        for line in &mut lines {
            if line == sep {
                has_output = true;
                break;
            }
            input += line;
            input += "\n";
        }
        let output = if has_output {
            let mut output = String::new();
            for line in &mut lines {
                output += line;
                output += "\n";
            }
            Some(output)
        } else {
            None
        };
        Self { input, output }
    }
}

#[test]
fn test_parse_test_case() {
    assert_eq!(
        TestCase {
            input: String::from(
                "\
Hello
World
"
            ),
            output: None,
        },
        TestCase::parse(
            "\
===
Hello
World
"
        ),
    );
    assert_eq!(
        TestCase {
            input: String::from(
                "\
Hello
World
"
            ),
            output: Some(String::from(
                "\
HELLO WORLD
"
            )),
        },
        TestCase::parse(
            "\
===
Hello
World
===
HELLO WORLD
"
        ),
    );
}

enum Log {
    Modified(std::path::PathBuf),
    SourceNotFound(std::path::PathBuf),
    Compiling(std::path::PathBuf),
    CompileResult(Result<Option<String>, String>),
    Testing(std::path::PathBuf),
    TestResult(TestResult),
}

enum TestResult {
    ExecutionSucceeded {
        stdout: String,
    },
    ExecutionFailed {
        stdout: String,
        status: std::process::ExitStatus,
    },
    TimeLimitExceeded {
        stdout: String,
    },
    AssertionSucceeded,
    AssertionFailed {
        expected: String,
        actual: String,
    },
}

impl Log {
    fn print(&self) {
        match self {
            Log::Modified(p) => {
                print!("  {} ", "Modified".grey());
                println!("{}", Self::format_path(p));
            }
            Log::SourceNotFound(p) => {
                println!(
                    "=== {}{}",
                    "Could not detect source file of ".dark_yellow().bold(),
                    Self::format_path(p)
                );
            }
            Log::Compiling(p) => {
                print!(" {} ", "Compiling".cyan());
                println!("{}...", Self::format_path(p));
            }
            Log::CompileResult(r) => match r {
                Ok(warning) => {
                    println!("=== {}", "Compilation Succeeded".dark_green().bold());
                    if let Some(warning) = warning {
                        println!("{}: {warning}", "warning".dark_yellow().bold());
                    }
                }
                Err(e) => {
                    println!("=== {}", "Compilation Failed".dark_red().bold());
                    println!("{e}");
                }
            },
            Log::Testing(p) => {
                print!("   {} ", "Testing".cyan());
                println!("{}...", Self::format_path(p));
            }
            Log::TestResult(r) => match r {
                TestResult::ExecutionSucceeded { stdout } => {
                    println!("=== {}", "Execution Succeeded".dark_green().bold());
                    Self::format_output(stdout, None);
                }
                TestResult::ExecutionFailed { stdout, status } => {
                    println!("=== {}", "Execution Failed".dark_red().bold());
                    println!(" status: {status}");
                    Self::format_output(stdout, None);
                }
                TestResult::TimeLimitExceeded { stdout } => {
                    println!("=== {}", "Time Limit Exceeded".dark_red().bold());
                    Self::format_output(stdout, None);
                }
                TestResult::AssertionSucceeded => {
                    println!("=== {}", "Assertion Succeeded".dark_green().bold());
                }
                TestResult::AssertionFailed { expected, actual } => {
                    println!("=== {}", "Assertion Failed".dark_red().bold());
                    Self::format_output(actual, Some(expected));
                }
            },
        }
    }

    fn format_path(p: &std::path::Path) -> String {
        format!("'{}'", p.to_string_lossy()).magenta().to_string()
    }

    fn format_output(output: &str, diff_with_expected: Option<&str>) {
        let indent = "  │";
        let indent_bold = "  ┃";
        if let Some(expected) = diff_with_expected {
            let diff = similar::TextDiff::from_lines(expected, output);
            let mut exp = String::from(" expected:\n");
            let mut act = String::from(" actual:\n");
            for change in diff.iter_all_changes() {
                match change.tag() {
                    similar::ChangeTag::Equal => {
                        exp += indent;
                        exp += change.as_str().unwrap();
                        act += indent;
                        act += change.as_str().unwrap();
                    }
                    similar::ChangeTag::Delete => {
                        exp += &indent_bold.dark_red().to_string();
                        exp += &change.as_str().unwrap().dark_red().bold().to_string();
                    }
                    similar::ChangeTag::Insert => {
                        act += &indent_bold.dark_green().to_string();
                        act += &change.as_str().unwrap().dark_green().bold().to_string();
                    }
                }
            }
            println!("{exp}");
            println!("{act}");
        } else {
            let mut out = String::from(" stdout:\n");
            for line in output.lines() {
                out += line;
                out += "\n";
            }
            println!("{out}");
        }
    }
}
