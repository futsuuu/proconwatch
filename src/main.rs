use std::io::Write as _;

use anyhow::Context as _;
use crossterm::style::Stylize as _;
use notify_debouncer_mini::notify;

fn main() -> anyhow::Result<()> {
    let test_runner = TestRunner::new()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let debounce_timeout = std::time::Duration::from_millis(300);
    let mut debouncer = notify_debouncer_mini::new_debouncer(debounce_timeout, tx)?;
    debouncer
        .watcher()
        .watch(
            &test_runner.root_directory,
            notify::RecursiveMode::NonRecursive,
        )
        .with_context(|| format!("failed to watch {:?}", test_runner.root_directory))?;
    for res in rx {
        let path = &res?[0].path;
        if !path.try_exists()? || !path.is_file() {
            continue;
        }
        Log::Modified(path.clone()).print();
        let Some(kind) = path.extension().and_then(FileKind::from_extension) else {
            continue;
        };
        match kind {
            FileKind::SourceCode => {
                test_runner.run_after_compiling(path)?;
            }
            FileKind::TestCase => {
                let Some(source) = get_source_code(path)? else {
                    Log::SourceCodeNotFound(path.to_path_buf()).print();
                    continue;
                };
                test_runner.run_only_one_test_case(&source, TestCase::read(path)?)?;
            }
        }
    }
    Ok(())
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

    fn run_only_one_test_case(
        &self,
        source: &std::path::Path,
        case: TestCase,
    ) -> anyhow::Result<()> {
        let exe_path = self.get_exe_path(source);
        if !exe_path.try_exists()? && !compile_cpp(source, &exe_path)? {
            return Ok(());
        }
        Log::Testing(source.to_path_buf()).print();
        self.run_test(&exe_path, case)?;
        Ok(())
    }

    fn run_after_compiling(&self, source: &std::path::Path) -> anyhow::Result<()> {
        let exe_path = self.get_exe_path(source);
        if !compile_cpp(source, &exe_path)? {
            return Ok(());
        }
        Log::Testing(source.to_path_buf()).print();
        for case in get_test_cases(source)? {
            self.run_test(&exe_path, case)?;
        }
        Ok(())
    }

    fn run_test(&self, exe_path: &std::path::Path, case: TestCase) -> anyhow::Result<()> {
        if !exe_path.try_exists()? {
            return Ok(());
        }
        let mut child = std::process::Command::new(exe_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {exe_path:?}"))?;
        let mut stdin = child.stdin.take().context("cannot take stdin")?;
        stdin
            .write_all(case.input.as_bytes())
            .context("failed to write into stdin")?;
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout)
            .replace("\r\n", "\n")
            .replace("\r", "\n");
        if let Some(expected_output) = case.output {
            if stdout == expected_output {
                Log::TestResult(Ok(None)).print();
            } else {
                Log::TestResult(Err((expected_output, stdout))).print();
            }
        } else {
            Log::TestResult(Ok(Some(stdout))).print();
        }
        Ok(())
    }

    fn get_exe_path(&self, source: &std::path::Path) -> std::path::PathBuf {
        let mut exe_path = self.bin_directory.join(source.file_stem().unwrap());
        exe_path.set_extension(std::env::consts::EXE_EXTENSION);
        exe_path
    }
}

fn get_source_code(test_case_path: &std::path::Path) -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut paths = Vec::new();
    let dir = test_case_path.parent().unwrap();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read directory {dir:?}"))?
    {
        let entry = entry?;
        if entry.path().extension().and_then(FileKind::from_extension) == Some(FileKind::SourceCode)
        {
            let p = entry.path();
            let stem = p.file_stem().unwrap().to_str().unwrap().to_string();
            paths.push((p, stem));
        }
    }
    let mut name = test_case_path
        .file_stem()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    while 2 <= paths.len() {
        paths.retain(|p| p.1.starts_with(&name));
        name.pop();
    }
    Ok(paths.pop().map(|t| t.0))
}

fn get_test_cases(source: &std::path::Path) -> anyhow::Result<Vec<TestCase>> {
    let stem = source.file_stem().unwrap().to_str().unwrap();
    let mut v = Vec::new();
    let dir = source.parent().unwrap();
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read directory {dir:?}"))?
    {
        let entry = entry?;
        if entry.path().extension().and_then(FileKind::from_extension) != Some(FileKind::TestCase) {
            continue;
        }
        if entry
            .file_name()
            .to_str()
            .is_some_and(|s| s.starts_with(stem))
        {
            v.push(TestCase::read(&entry.path())?);
        }
    }
    Ok(v)
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
    SourceCode,
    TestCase,
}

impl FileKind {
    fn from_extension(ext: &std::ffi::OsStr) -> Option<Self> {
        match ext.to_str()? {
            "c++" | "cpp" | "cxx" => Some(Self::SourceCode),
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
    SourceCodeNotFound(std::path::PathBuf),
    Compiling(std::path::PathBuf),
    CompileResult(Result<Option<String>, String>),
    Testing(std::path::PathBuf),
    TestResult(Result<Option<String>, (String, String)>),
}

impl Log {
    fn print(&self) {
        match self {
            Log::Modified(p) => {
                print!("  {} ", "Modified".grey());
                println!("{}", Self::format_path(p));
            }
            Log::SourceCodeNotFound(p) => {
                println!("=== {}{}", "Cannot detect source code of ".dark_yellow().bold(), Self::format_path(p));
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
                Ok(None) => {
                    println!("=== {}", "Assertion Succeeded".dark_green().bold());
                }
                Ok(Some(output)) => {
                    println!("=== {}", "Execution Succeeded".dark_green().bold());
                    println!(" output:");
                    for line in output.lines() {
                        println!("  │{line}");
                    }
                    println!();
                }
                Err((expected, actual)) => {
                    println!("=== {}", "Assertion Failed".dark_red().bold());
                    Self::diff(expected, actual);
                },
            }
        }
    }

    fn format_path(p: &std::path::Path) -> String {
        format!("'{}'", p.to_string_lossy()).magenta().to_string()
    }

    fn diff(expected: &str, actual: &str) {
        let diff = similar::TextDiff::from_lines(expected, actual);
        let mut exp = String::from(" expected:\n");
        let mut act = String::from(" actual:\n");
        for change in diff.iter_all_changes() {
            match change.tag() {
                similar::ChangeTag::Equal => {
                    exp += "  │";
                    exp += change.as_str().unwrap();
                    act += "  │";
                    act += change.as_str().unwrap();
                }
                similar::ChangeTag::Delete => {
                    exp += &"  ┃".dark_red().to_string();
                    exp += &change.as_str().unwrap().dark_red().bold().to_string();
                }
                similar::ChangeTag::Insert => {
                    act += &"  ┃".dark_green().to_string();
                    act += &change.as_str().unwrap().dark_green().bold().to_string();
                }
            }
        }
        println!("{exp}");
        println!("{act}");
    }
}
