use miette::miette;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

#[derive(Debug, usage_rs::Args)]
pub struct ActivateArgs {
    /// Shell to emit activation code for
    pub shell: ActivateShell,
}

#[derive(Debug, Clone, Copy, usage_rs::ValueEnum, strum::Display, strum::EnumString)]
#[strum(serialize_all = "kebab-case")]
pub enum ActivateShell {
    Bash,
    Fish,
    Zsh,
}

pub fn run(args: ActivateArgs) -> miette::Result<()> {
    let shim_dir = crate::tool_shims::shim_dir().ok_or_else(|| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "could not locate the aube shim dir"
        )
    })?;
    ensure_shims(&shim_dir)?;
    println!("{}", render_activation(args.shell, &shim_dir));
    Ok(())
}

fn ensure_shims(shim_dir: &Path) -> miette::Result<()> {
    std::fs::create_dir_all(shim_dir).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to create shim dir {}: {e}",
            shim_dir.display()
        )
    })?;
    #[cfg(not(unix))]
    let exe = std::env::current_exe().map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to locate current executable for shim creation: {e}"
        )
    })?;
    #[cfg(unix)]
    for name in crate::tool_shims::TOOL_SHIMS {
        write_shim(shim_dir, &crate::tool_shims::shim_file_name(name))?;
    }
    #[cfg(not(unix))]
    for name in crate::tool_shims::TOOL_SHIMS {
        write_shim(shim_dir, &crate::tool_shims::shim_file_name(name), &exe)?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_shim(shim_dir: &Path, name: &str) -> miette::Result<()> {
    let dest = shim_dir.join(name);
    write_dispatcher_shim(&dest, name, aube_util::prog()).map_err(|e| {
        miette!(
            code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
            "failed to create shim {}: {e}",
            dest.display(),
        )
    })
}

#[cfg(unix)]
fn write_dispatcher_shim(dest: &Path, name: &str, program: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

    let program = shell_double_quote(program);
    let script = format!(
        "#!/bin/sh\n# aube-tool-shim v1\nexec {program} {} {name} \"$@\"\n",
        crate::tool_shims::DISPATCH_ARG
    );
    let (tmp, mut file) = loop {
        let tmp = aube_util::fs_atomic::sibling_tempdir(dest);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    };
    let result = (|| {
        file.write_all(script.as_bytes())?;
        file.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        drop(file);
        // The temp file lives beside the destination, so Unix rename replaces
        // the old executable atomically. Readers see either complete shim,
        // never the remove/write/chmod window activation previously exposed.
        std::fs::rename(&tmp, dest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(not(unix))]
fn write_shim(shim_dir: &Path, name: &str, exe: &Path) -> miette::Result<()> {
    let dest = shim_dir.join(name);
    let _ = std::fs::remove_file(&dest);
    std::fs::hard_link(exe, &dest)
        .or_else(|_| std::fs::copy(exe, &dest).map(|_| ()))
        .map_err(|e| {
            miette!(
                code = aube_codes::errors::ERR_AUBE_SHIM_CREATE_FAILED,
                "failed to create shim {} -> {}: {e}",
                dest.display(),
                exe.display()
            )
        })
}

fn render_activation(shell: ActivateShell, shim_dir: &Path) -> String {
    let quoted = shell_double_quote(&shim_dir.display().to_string());
    match shell {
        ActivateShell::Bash | ActivateShell::Zsh => format!(
            "export {env}={quoted}\n\
             case \":$PATH:\" in\n\
             *\":${env}:\"*) ;;\n\
             *) export PATH=\"${env}:$PATH\" ;;\n\
             esac",
            env = crate::tool_shims::SHIM_DIR_ENV,
        ),
        ActivateShell::Fish => format!(
            "set -gx {env} {quoted}\n\
             if not contains -- ${env} $PATH\n\
                 set -gx PATH ${env} $PATH\n\
             end",
            env = crate::tool_shims::SHIM_DIR_ENV,
        ),
    }
}

fn shell_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '$' => out.push_str("\\$"),
            '`' => out.push_str("\\`"),
            '\n' => out.push_str("\\n"),
            _ => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path() -> PathBuf {
        PathBuf::from("/tmp/aube shims")
    }

    #[test]
    fn renders_bash_activation() {
        let out = render_activation(ActivateShell::Bash, &path());
        assert!(out.contains("export AUBE_SHIM_DIR=\"/tmp/aube shims\""));
        assert!(out.contains("case \":$PATH:\" in"));
        assert!(out.contains("export PATH=\"$AUBE_SHIM_DIR:$PATH\""));
    }

    #[test]
    fn renders_zsh_activation() {
        let out = render_activation(ActivateShell::Zsh, &path());
        assert!(out.contains("export AUBE_SHIM_DIR=\"/tmp/aube shims\""));
        assert!(out.contains("*\":$AUBE_SHIM_DIR:\"*) ;;"));
    }

    #[test]
    fn renders_fish_activation() {
        let out = render_activation(ActivateShell::Fish, &path());
        assert!(out.contains("set -gx AUBE_SHIM_DIR \"/tmp/aube shims\""));
        assert!(out.contains("if not contains -- $AUBE_SHIM_DIR $PATH"));
        assert!(out.contains("set -gx PATH $AUBE_SHIM_DIR $PATH"));
    }

    #[test]
    fn quotes_shell_metacharacters() {
        assert_eq!(
            shell_double_quote(r#"/tmp/a"b$c\d`e"#),
            r#""/tmp/a\"b\$c\\d\`e""#
        );
    }

    #[cfg(unix)]
    #[test]
    fn writes_path_resolved_dispatcher_shims() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("shim tempdir should be created");
        ensure_shims(dir.path()).expect("shims should be created");
        let node = dir.path().join("node");
        assert!(!node.is_symlink());
        assert_ne!(
            std::fs::metadata(&node)
                .expect("node shim should have metadata")
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            std::fs::read_to_string(&node).expect("node shim should be readable after creation"),
            "#!/bin/sh\n# aube-tool-shim v1\nexec \"aube\" __aube-shim node \"$@\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_shim_uses_embedder_program() {
        let dir = tempfile::tempdir().expect("shim tempdir should be created");
        let node = dir.path().join("node");
        write_dispatcher_shim(&node, "node", "nublike").expect("embedder shim should be created");
        assert_eq!(
            std::fs::read_to_string(node).expect("embedder shim should be readable"),
            "#!/bin/sh\n# aube-tool-shim v1\nexec \"nublike\" __aube-shim node \"$@\"\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dispatcher_shim_atomically_replaces_existing_file() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().expect("shim tempdir should be created");
        let node = dir.path().join("node");
        std::fs::write(&node, "old shim").expect("old shim should be created");
        let old_inode = std::fs::metadata(&node)
            .expect("old shim should have metadata")
            .ino();

        write_dispatcher_shim(&node, "node", "aube").expect("shim should be replaced");

        let metadata = std::fs::metadata(&node).expect("new shim should have metadata");
        assert_ne!(metadata.ino(), old_inode);
        assert_ne!(metadata.permissions().mode() & 0o111, 0);
        assert_eq!(
            std::fs::read_to_string(&node).expect("new shim should be complete"),
            "#!/bin/sh\n# aube-tool-shim v1\nexec \"aube\" __aube-shim node \"$@\"\n"
        );
        let entries = std::fs::read_dir(dir.path())
            .expect("shim dir should be readable")
            .count();
        assert_eq!(entries, 1, "temporary shim should be renamed away");
    }
}
