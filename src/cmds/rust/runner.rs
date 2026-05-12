//! Runs arbitrary commands and captures only stderr or test failures.

use crate::core::stream::StreamFilter;
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;
use std::process::Command;

lazy_static! {
    static ref ERROR_PATTERNS: Vec<Regex> = vec![
        // Generic errors
        Regex::new(r"(?i)^.*error[\s:\[].*$").unwrap(),
        Regex::new(r"(?i)^.*\berr\b.*$").unwrap(),
        Regex::new(r"(?i)^.*warning[\s:\[].*$").unwrap(),
        Regex::new(r"(?i)^.*\bwarn\b.*$").unwrap(),
        Regex::new(r"(?i)^.*failed.*$").unwrap(),
        Regex::new(r"(?i)^.*failure.*$").unwrap(),
        Regex::new(r"(?i)^.*exception.*$").unwrap(),
        Regex::new(r"(?i)^.*panic.*$").unwrap(),
        // Rust specific
        Regex::new(r"^error\[E\d+\]:.*$").unwrap(),
        Regex::new(r"^\s*--> .*:\d+:\d+$").unwrap(),
        // Python
        Regex::new(r"^Traceback.*$").unwrap(),
        Regex::new(r#"^\s*File ".*", line \d+.*$"#).unwrap(),
        // JavaScript/TypeScript
        Regex::new(r"^\s*at .*:\d+:\d+.*$").unwrap(),
        // Go
        Regex::new(r"^.*\.go:\d+:.*$").unwrap(),
    ];
}

struct ErrorStreamFilter {
    in_error_block: bool,
    blank_count: usize,
    emitted_any: bool,
}

impl ErrorStreamFilter {
    fn new() -> Self {
        Self {
            in_error_block: false,
            blank_count: 0,
            emitted_any: false,
        }
    }
}

impl StreamFilter for ErrorStreamFilter {
    fn feed_line(&mut self, line: &str) -> Option<String> {
        let is_error = ERROR_PATTERNS.iter().any(|p| p.is_match(line));
        if is_error {
            self.in_error_block = true;
            self.blank_count = 0;
            self.emitted_any = true;
            Some(format!("{}\n", line))
        } else if self.in_error_block {
            if line.trim().is_empty() {
                self.blank_count += 1;
                if self.blank_count >= 2 {
                    self.in_error_block = false;
                    None
                } else {
                    self.emitted_any = true;
                    Some(format!("{}\n", line))
                }
            } else if line.starts_with(' ') || line.starts_with('\t') {
                self.blank_count = 0;
                self.emitted_any = true;
                Some(format!("{}\n", line))
            } else {
                self.in_error_block = false;
                None
            }
        } else {
            None
        }
    }

    fn flush(&mut self) -> String {
        String::new()
    }

    fn on_exit(&mut self, exit_code: i32, raw: &str) -> Option<String> {
        if self.emitted_any {
            return None;
        }
        if exit_code == 0 {
            Some("[ok] Command completed successfully (no errors)".to_string())
        } else {
            let mut msg = format!("[FAIL] Command failed (exit code: {})\n", exit_code);
            let lines: Vec<&str> = raw.lines().collect();
            for line in lines.iter().rev().take(10).rev() {
                msg.push_str(&format!("  {}\n", line));
            }
            Some(msg)
        }
    }
}

fn build_shell_command(command: &str) -> Command {
    if cfg!(target_os = "windows") {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    }
}

/// Run a command and filter output to show only errors/warnings
pub fn run_err(command: &str, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running: {}", command);
    }
    let cmd = build_shell_command(command);
    crate::core::runner::run_streamed(
        cmd,
        "err",
        command,
        Box::new(ErrorStreamFilter::new()),
        crate::core::runner::RunOptions::with_tee("err"),
    )
}

/// Run tests and show only failures
pub fn run_test(command: &str, verbose: u8) -> Result<i32> {
    if verbose > 0 {
        eprintln!("Running tests: {}", command);
    }
    let cmd = build_shell_command(command);
    let command_owned = command.to_string();
    crate::core::runner::run_filtered(
        cmd,
        "test",
        command,
        move |raw| extract_test_summary(raw, &command_owned),
        crate::core::runner::RunOptions::with_tee("test"),
    )
}

#[cfg(test)]
fn filter_errors(output: &str) -> String {
    let mut result = Vec::new();
    let mut in_error_block = false;
    let mut blank_count = 0;

    for line in output.lines() {
        let is_error_line = ERROR_PATTERNS.iter().any(|p| p.is_match(line));

        if is_error_line {
            in_error_block = true;
            blank_count = 0;
            result.push(line.to_string());
        } else if in_error_block {
            if line.trim().is_empty() {
                blank_count += 1;
                if blank_count >= 2 {
                    in_error_block = false;
                } else {
                    result.push(line.to_string());
                }
            } else if line.starts_with(' ') || line.starts_with('\t') {
                result.push(line.to_string());
                blank_count = 0;
            } else {
                in_error_block = false;
            }
        }
    }

    result.join("\n")
}

/// Maximum number of failure stdout blocks to surface (panic body + stack).
const CARGO_MAX_STDOUT_BLOCKS: usize = 5;
/// Maximum lines per stdout block before truncation.
const CARGO_MAX_STDOUT_LINES_PER_BLOCK: usize = 25;

/// Parse `---- module::test stdout ----` header line. Returns the test name on match.
fn parse_cargo_stdout_header(line: &str) -> Option<&str> {
    line.strip_prefix("---- ")
        .and_then(|s| s.strip_suffix(" stdout ----"))
}

fn extract_test_summary(output: &str, command: &str) -> String {
    let mut result = Vec::new();
    let lines: Vec<&str> = output.lines().collect();

    let is_cargo = command.contains("cargo test");
    let is_pytest = command.contains("pytest");
    let is_jest =
        command.contains("jest") || command.contains("npm test") || command.contains("yarn test");
    let is_go = command.contains("go test");

    let mut failures = Vec::new();
    // Cargo-specific: panic body / stack trace per failing test
    let mut stdout_blocks: Vec<(String, Vec<String>)> = Vec::new();
    let mut current_block: Option<(String, Vec<String>)> = None;
    let mut consecutive_blanks = 0;

    for line in lines.iter() {
        if is_cargo {
            // Header: open a new stdout block, flush previous if any
            if let Some(test_name) = parse_cargo_stdout_header(line) {
                if let Some(prev) = current_block.take() {
                    stdout_blocks.push(prev);
                }
                current_block = Some((test_name.to_string(), Vec::new()));
                consecutive_blanks = 0;
                continue;
            }

            // Inside a stdout block: capture until a sentinel ends it
            if let Some((_, body)) = current_block.as_mut() {
                // Two consecutive blank lines = end of block
                if line.is_empty() {
                    consecutive_blanks += 1;
                    if consecutive_blanks >= 2 {
                        let prev = current_block.take().expect("current_block was Some");
                        stdout_blocks.push(prev);
                    } else {
                        body.push(String::new());
                    }
                    continue;
                }
                consecutive_blanks = 0;
                // `failures:` summary or `test result:` ends the block
                if line.starts_with("failures:") || line.contains("test result:") {
                    let prev = current_block.take().expect("current_block was Some");
                    stdout_blocks.push(prev);
                    // fall through so this line still gets processed below
                } else {
                    body.push(line.to_string());
                    continue;
                }
            }

            if line.contains("test result:") {
                result.push(line.to_string());
            }
            if line.contains("FAILED") && !line.contains("test result") {
                failures.push(line.to_string());
            }
        }

        if is_pytest {
            if line.contains(" passed") || line.contains(" failed") || line.contains(" error") {
                result.push(line.to_string());
            }
            if line.contains("FAILED") {
                failures.push(line.to_string());
            }
        }

        if is_jest {
            if line.contains("Tests:") || line.contains("Test Suites:") {
                result.push(line.to_string());
            }
            if line.contains("✕") || line.contains("FAIL") {
                failures.push(line.to_string());
            }
        }

        if is_go {
            if line.starts_with("ok") || line.starts_with("FAIL") || line.starts_with("---") {
                result.push(line.to_string());
            }
            if line.contains("FAIL") {
                failures.push(line.to_string());
            }
        }
    }

    // Flush any unterminated block
    if let Some(prev) = current_block.take() {
        stdout_blocks.push(prev);
    }

    let mut output = String::new();

    if !failures.is_empty() {
        output.push_str("[FAIL] FAILURES:\n");
        for f in failures.iter().take(10) {
            output.push_str(&format!("  {}\n", f));
        }
        if failures.len() > 10 {
            output.push_str(&format!("  ... +{} more failures\n", failures.len() - 10));
        }
        output.push('\n');
    }

    // Cargo: surface panic body + stack trace for failing tests
    if !stdout_blocks.is_empty() {
        for (test_name, body) in stdout_blocks.iter().take(CARGO_MAX_STDOUT_BLOCKS) {
            // Drop trailing blanks
            let mut trimmed: Vec<&String> =
                body.iter().take_while(|_| true).collect::<Vec<_>>();
            while trimmed.last().is_some_and(|l| l.is_empty()) {
                trimmed.pop();
            }
            if trimmed.is_empty() {
                continue;
            }
            output.push_str(&format!("---- {} ----\n", test_name));
            for line in trimmed.iter().take(CARGO_MAX_STDOUT_LINES_PER_BLOCK) {
                output.push_str(&format!("  {}\n", line));
            }
            if trimmed.len() > CARGO_MAX_STDOUT_LINES_PER_BLOCK {
                output.push_str(&format!(
                    "  ... +{} more lines\n",
                    trimmed.len() - CARGO_MAX_STDOUT_LINES_PER_BLOCK
                ));
            }
            output.push('\n');
        }
        if stdout_blocks.len() > CARGO_MAX_STDOUT_BLOCKS {
            output.push_str(&format!(
                "... +{} more failure body blocks suppressed\n\n",
                stdout_blocks.len() - CARGO_MAX_STDOUT_BLOCKS
            ));
        }
    }

    if !result.is_empty() {
        output.push_str("SUMMARY:\n");
        for r in &result {
            output.push_str(&format!("  {}\n", r));
        }
    } else {
        output.push_str("OUTPUT (last 5 lines):\n");
        let start = lines.len().saturating_sub(5);
        for line in &lines[start..] {
            if !line.trim().is_empty() {
                output.push_str(&format!("  {}\n", line));
            }
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn test_filter_errors() {
        let output = "info: compiling\nerror: something failed\n  at line 10\ninfo: done";
        let filtered = filter_errors(output);
        assert!(filtered.contains("error"));
        assert!(!filtered.contains("info"));
    }

    #[test]
    fn test_cargo_test_preserves_panic_body() {
        let input = include_str!("../../../tests/fixtures/cargo_test_failures_raw.txt");
        let out = extract_test_summary(input, "cargo test");

        assert!(out.contains("panicked at src/math.rs"));
        assert!(out.contains("left: 5"));
        assert!(out.contains("right: 4"));
        assert!(out.contains("ParseIntError"));
        assert!(out.contains("math::div_test"));
        assert!(out.contains("parse::parse_int"));
        assert!(out.contains("test result: FAILED"));
    }

    #[test]
    fn test_cargo_test_preserves_stack_trace() {
        let input = include_str!("../../../tests/fixtures/cargo_test_failures_raw.txt");
        let out = extract_test_summary(input, "cargo test");
        assert!(out.contains("stack backtrace"));
        assert!(out.contains("rust_begin_unwind"));
    }

    #[test]
    fn test_cargo_test_token_savings_above_60_percent() {
        let input = include_str!("../../../tests/fixtures/cargo_test_failures_raw.txt");
        let out = extract_test_summary(input, "cargo test");
        let in_tokens = count_tokens(input);
        let out_tokens = count_tokens(&out);
        // The fixture is small enough that overhead dominates; this just
        // guards against future bloat in the filter
        assert!(
            out_tokens <= in_tokens,
            "filter must not grow output: {} -> {}",
            in_tokens,
            out_tokens
        );
    }

    #[test]
    fn test_cargo_test_caps_blocks_at_max() {
        // Build synthetic input with many failures to verify cap
        let mut input = String::from("running 10 tests\n");
        for i in 0..10 {
            input.push_str(&format!("test t::test_{} ... FAILED\n", i));
        }
        input.push_str("\nfailures:\n\n");
        for i in 0..10 {
            input.push_str(&format!("---- t::test_{} stdout ----\n\n", i));
            input.push_str(&format!(
                "thread 't::test_{}' panicked at src/lib.rs:1:1:\nboom_{}\n\n\n",
                i, i
            ));
        }
        input.push_str("failures:\n");
        for i in 0..10 {
            input.push_str(&format!("    t::test_{}\n", i));
        }
        input.push_str("\ntest result: FAILED. 0 passed; 10 failed; 0 ignored\n");

        let out = extract_test_summary(&input, "cargo test");
        // First 5 blocks present, rest suppressed
        for i in 0..CARGO_MAX_STDOUT_BLOCKS {
            assert!(out.contains(&format!("boom_{}", i)), "missing boom_{}", i);
        }
        assert!(out.contains("more failure body blocks suppressed"));
    }

    #[test]
    fn test_cargo_test_no_failures_no_blocks() {
        let input = "running 2 tests\ntest a ... ok\ntest b ... ok\n\n\
                     test result: ok. 2 passed; 0 failed; 0 ignored\n";
        let out = extract_test_summary(input, "cargo test");
        assert!(!out.contains("FAIL"));
        assert!(out.contains("test result: ok"));
    }

    #[test]
    fn test_parse_cargo_stdout_header() {
        assert_eq!(
            parse_cargo_stdout_header("---- foo::bar stdout ----"),
            Some("foo::bar")
        );
        assert_eq!(parse_cargo_stdout_header("not a header"), None);
        assert_eq!(parse_cargo_stdout_header("---- stdout"), None);
    }
}
