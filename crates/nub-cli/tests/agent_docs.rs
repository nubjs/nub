use std::process::{Command, Output};

fn docs(args: &[&str]) -> Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_nub"))
        .args(["agent", "docs"])
        .args(args)
        .current_dir(dir.path())
        .output()
        .expect("run nub agent docs")
}

fn stdout(args: &[&str]) -> String {
    let output = docs(args);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn default_output_is_help_and_toc_without_page_content() {
    let output = stdout(&[]);
    assert!(output.starts_with("nub agent docs —"));
    for heading in ["Usage:", "Options:", "Example:", "Table of contents:"] {
        assert!(output.contains(heading), "missing {heading}: {output}");
    }
    let list = stdout(&["--list"]);
    let (help, toc) = output.split_once("Table of contents:").unwrap();
    assert_eq!(format!("Table of contents:{toc}"), list);
    assert!(help.lines().count() <= 14, "help should remain concise");
    assert!(!output.contains("all-in-one toolkit"));
    assert!(!output.contains("```"));
}

#[test]
fn list_aliases_print_only_page_paths_and_titles() {
    let list = stdout(&["--list"]);
    assert_eq!(list, stdout(&["--toc"]));
    assert_eq!(list.lines().next(), Some("Table of contents:"));
    assert!(list.contains("  /docs — Introduction"));
    for line in list.lines().skip(1) {
        assert!(line.starts_with("  /docs"), "unexpected content: {line}");
        assert!(line.contains(" — "), "missing page title: {line}");
    }
}

#[test]
fn explicit_pages_still_print_markdown_and_accept_link_targets() {
    let overview = stdout(&["--page", "/docs"]);
    assert!(overview.contains("all-in-one toolkit"));
    assert!(overview.contains("```bash"));
    for alias in ["/", "docs", "/docs#install"] {
        assert_eq!(overview, stdout(&["--page", alias]));
    }
    assert_eq!(overview, stdout(&["--page=/docs"]));
    let page = stdout(&["--page", "/docs/run"]);
    assert_eq!(page, stdout(&["--page", "run"]));
    assert!(!page.starts_with("nub agent docs —"));
}

#[test]
fn help_and_invalid_arguments_keep_their_exit_statuses() {
    for flag in ["--help", "-h"] {
        assert!(stdout(&[flag]).contains("Usage: nub agent <command>"));
    }
    for args in [vec!["--page"], vec!["--page", "/missing"], vec!["--bogus"]] {
        let output = docs(&args);
        assert!(!output.status.success(), "{args:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(!output.stderr.is_empty(), "{output:?}");
    }
}
