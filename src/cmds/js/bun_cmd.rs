//! Filters bun output across install, test, run, and build subcommands.

use crate::core::runner;
use crate::core::utils::resolved_command;
use anyhow::Result;
use lazy_static::lazy_static;
use regex::Regex;

/// Known bun subcommands that should NOT have "run" auto-injected.
const BUN_SUBCOMMANDS: &[&str] = &[
    "install",
    "i",
    "add",
    "a",
    "remove",
    "rm",
    "update",
    "outdated",
    "audit",
    "link",
    "unlink",
    "pm",
    "run",
    "test",
    "x",
    "create",
    "init",
    "build",
    "repl",
    "exec",
    "upgrade",
    "patch",
    "publish",
    "info",
    "why",
];

lazy_static! {
    static ref BUN_ANSI: Regex = Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap();
    // bun install per-package line: " + name@version" or " - name@version"
    static ref BUN_INSTALL_PKG: Regex = Regex::new(r"^\s*[+\-]\s+\S+@").unwrap();
    // bun test "(pass)" line
    static ref BUN_TEST_PASS: Regex = Regex::new(r"^\s*\(pass\)").unwrap();
    // bun test failure marker
    static ref BUN_TEST_FAIL: Regex = Regex::new(r"^\s*\(fail\)|^\s*FAIL\b").unwrap();
}

pub fn run(args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let first_arg = args.first().map(|s| s.as_str());
    let is_run_explicit = first_arg == Some("run");
    let is_bun_subcommand = first_arg
        .map(|a| BUN_SUBCOMMANDS.contains(&a) || a.starts_with('-'))
        .unwrap_or(false);

    let mut effective_args: Vec<String> = Vec::with_capacity(args.len() + 1);
    if is_run_explicit || is_bun_subcommand {
        effective_args.extend_from_slice(args);
    } else {
        // "rtk bun dev" → "bun run dev"
        effective_args.push("run".to_string());
        effective_args.extend_from_slice(args);
    }

    run_filtered("bun", &effective_args, verbose, skip_env)
}

/// Dispatch bunx through the same pipeline.
pub fn exec(args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    run_filtered("bunx", args, verbose, skip_env)
}

fn run_filtered(name: &str, args: &[String], verbose: u8, skip_env: bool) -> Result<i32> {
    let mut cmd = resolved_command(name);
    for arg in args {
        cmd.arg(arg);
    }

    if skip_env {
        cmd.env("SKIP_ENV_VALIDATION", "1");
    }

    let args_display = args.join(" ");
    if verbose > 0 {
        eprintln!("Running: {} {}", name, args_display);
    }

    // Choose filter based on subcommand
    let subcmd = args.first().map(|s| s.as_str()).unwrap_or("");
    let filter: fn(&str) -> String = match subcmd {
        "test" => filter_bun_test_output,
        "install" | "i" | "add" | "a" => filter_bun_install_output,
        _ => filter_bun_generic,
    };

    runner::run_filtered(
        cmd,
        name,
        &args_display,
        filter,
        runner::RunOptions::default(),
    )
}

/// Strip ANSI sequences, drop blank lines and "..." progress dots.
fn filter_bun_generic(output: &str) -> String {
    let mut result = Vec::new();

    for line in output.lines() {
        let stripped = BUN_ANSI.replace_all(line, "").to_string();
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "..." {
            continue;
        }
        result.push(stripped);
    }

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
    }
}

/// bun install: drop per-package "+ name@version" lines, keep summary.
fn filter_bun_install_output(output: &str) -> String {
    let mut result = Vec::new();
    let mut dropped = 0;

    for line in output.lines() {
        let stripped = BUN_ANSI.replace_all(line, "").to_string();
        let trimmed = stripped.trim();
        if trimmed.is_empty() {
            continue;
        }
        if BUN_INSTALL_PKG.is_match(&stripped) {
            dropped += 1;
            continue;
        }
        result.push(stripped);
    }

    if dropped > 0 {
        result.push(format!("[rtk] suppressed {} per-package install lines", dropped));
    }

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
    }
}

/// bun test: keep failure blocks + summary, drop "(pass)" lines.
fn filter_bun_test_output(output: &str) -> String {
    let mut result = Vec::new();
    let mut in_failure = false;
    let mut failure_indent: Option<usize> = None;
    let mut passes = 0;
    let mut last_blank = false;

    for line in output.lines() {
        let stripped = BUN_ANSI.replace_all(line, "").to_string();
        let trimmed = stripped.trim();

        // Track summary lines unconditionally
        if trimmed.contains(" pass")
            && (trimmed.contains(" fail") || trimmed.contains("expect()"))
        {
            result.push(stripped);
            in_failure = false;
            continue;
        }
        if trimmed.starts_with("Ran ") && trimmed.contains("test") {
            result.push(stripped);
            in_failure = false;
            continue;
        }

        if BUN_TEST_PASS.is_match(&stripped) {
            passes += 1;
            in_failure = false;
            continue;
        }

        if BUN_TEST_FAIL.is_match(&stripped) {
            in_failure = true;
            failure_indent = Some(stripped.chars().take_while(|c| c.is_whitespace()).count());
            result.push(stripped);
            last_blank = false;
            continue;
        }

        if in_failure {
            // Stop on a blank line followed by a less-indented line
            let indent = stripped.chars().take_while(|c| c.is_whitespace()).count();
            if trimmed.is_empty() {
                if last_blank {
                    in_failure = false;
                } else {
                    last_blank = true;
                    result.push(stripped);
                }
                continue;
            }
            last_blank = false;
            // De-indent rule: if current indent <= failure header indent and line
            // doesn't start with whitespace, the failure body has ended.
            if let Some(base) = failure_indent {
                if indent <= base && !stripped.starts_with(' ') && !stripped.starts_with('\t') {
                    in_failure = false;
                } else {
                    result.push(stripped);
                    continue;
                }
            } else {
                result.push(stripped);
                continue;
            }
        }
    }

    if passes > 0 {
        result.insert(0, format!("[rtk] {} passing tests omitted", passes));
    }

    if result.is_empty() {
        "ok".to_string()
    } else {
        result.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_tokens(s: &str) -> usize {
        s.split_whitespace().count()
    }

    #[test]
    fn test_bun_subcommand_routing() {
        fn needs_run_injection(args: &[&str]) -> bool {
            let first = args.first().copied();
            let is_run_explicit = first == Some("run");
            let is_subcommand = first
                .map(|a| BUN_SUBCOMMANDS.contains(&a) || a.starts_with('-'))
                .unwrap_or(false);
            !is_run_explicit && !is_subcommand
        }

        for subcmd in BUN_SUBCOMMANDS {
            assert!(
                !needs_run_injection(&[subcmd]),
                "'bun {}' should NOT inject 'run'",
                subcmd
            );
        }
        for script in &["dev", "lint", "typecheck", "deploy"] {
            assert!(
                needs_run_injection(&[script]),
                "'bun {}' SHOULD inject 'run'",
                script
            );
        }
        assert!(!needs_run_injection(&["--version"]));
        assert!(!needs_run_injection(&["run", "build"]));
    }

    #[test]
    fn test_filter_bun_generic_drops_blank_and_dots() {
        let input = "\n\n  ...\n\nDone in 12ms\n";
        let out = filter_bun_generic(input);
        assert_eq!(out, "Done in 12ms");
    }

    #[test]
    fn test_filter_bun_install_drops_pkg_lines() {
        let input = " + react@18.2.0\n + react-dom@18.2.0\n + lodash@4.17.21\n\n3 packages installed [123.00ms]\n";
        let out = filter_bun_install_output(input);
        assert!(!out.contains("react@"));
        assert!(!out.contains("lodash@"));
        assert!(out.contains("3 packages installed"));
        assert!(out.contains("suppressed 3 per-package"));
    }

    #[test]
    fn test_filter_bun_install_savings() {
        let mut input = String::new();
        for i in 0..50 {
            input.push_str(&format!(" + pkg{}@1.0.0\n", i));
        }
        input.push_str("50 packages installed [200.00ms]\n");

        let out = filter_bun_install_output(&input);
        let savings =
            100.0 - (count_tokens(&out) as f64 / count_tokens(&input) as f64 * 100.0);
        assert!(
            savings >= 60.0,
            "bun install filter: expected >=60% savings, got {:.1}%",
            savings
        );
    }

    #[test]
    fn test_filter_bun_test_drops_passes() {
        let input = "\
bun test v1.0.0

src/foo.test.ts:
(pass) foo > works [1.00ms]
(pass) foo > also works [0.50ms]
(pass) foo > yet again [0.30ms]

 3 pass
 0 fail
Ran 3 tests across 1 files. [10.00ms]
";
        let out = filter_bun_test_output(input);
        assert!(!out.contains("(pass)"));
        assert!(out.contains("Ran 3 tests"));
        assert!(out.contains("3 passing tests omitted"));
    }

    #[test]
    fn test_filter_bun_test_keeps_failure_body() {
        let input = "\
bun test v1.0.0

src/foo.test.ts:
(pass) foo > works [1.00ms]
(fail) foo > broken [0.20ms]
  Expected: 1
  Received: 2
      at <anonymous> (src/foo.test.ts:5:12)

 1 pass
 1 fail
Ran 2 tests across 1 files.
";
        let out = filter_bun_test_output(input);
        assert!(out.contains("(fail)"));
        assert!(out.contains("Expected: 1"));
        assert!(out.contains("Received: 2"));
        assert!(out.contains("at <anonymous>"));
        assert!(!out.contains("(pass)"));
    }
}
