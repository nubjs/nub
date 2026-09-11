use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

fn root() -> Option<PathBuf> {
    cfg!(windows)
        .then(|| std::env::var_os("NUB_SANDBOX_TOOL_MSYS_ROOT").map(PathBuf::from))
        .flatten()
}

pub fn grant(fs: &mut Map<String, Value>) {
    if let Some(root) = root() {
        fs.insert(
            root.to_string_lossy().into_owned(),
            Value::String("r".into()),
        );
    }
}

pub fn command(program: &Path, args: Vec<String>, cwd: &Path) -> (PathBuf, Vec<String>) {
    let Some(root) = root() else {
        return (program.to_owned(), args);
    };
    let shell = root.join("usr/bin/bash.exe");
    assert!(
        shell.is_file(),
        "MSYS shell is provisioned: {}",
        shell.display()
    );
    // Rust's Windows argv encoding and MSYS's -c decoding disagree on embedded
    // quotes. Transfer the command as a script, not through that second parser.
    let invocation = std::iter::once(program.to_string_lossy().replace('\\', "/"))
        .chain(args)
        .map(|arg| format!("'{}'", arg.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let script = cwd.join(".sandbox-msys-command.sh");
    std::fs::write(
        &script,
        // These are already native argv values, including embedded JS/Python.
        // Keep Bash alive to wait rather than replacing it with exec.
        format!("export MSYS2_ARG_CONV_EXCL='*'\n{invocation}\n"),
    )
    .expect("MSYS fixture script is written inside its granted project");
    (
        shell,
        vec![
            "--noprofile".into(),
            "--norc".into(),
            script.to_string_lossy().replace('\\', "/"),
        ],
    )
}
