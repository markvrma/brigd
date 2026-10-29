//! Stray agents: `claude` typed in a brigd terminal tab goes through the ~/.brigd/bin shim
//! to `brigd stray`, which records the session under <thread>/stray agents/ and then execs claude.

use crate::diffs::record_base;
use crate::store::{root, thread_dir, valid_name, THREAD_MAX};
use crate::{runner, tree, Res};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io};

/// `claude` in a brigd terminal tab runs this (via the ~/.brigd/bin shim). In thread
/// $BRIGD_THREAD it records a new session under <thread>/stray agents/stray-<n>/
/// {cwd,session,base,pid,term} with the flow agents' hooks, then becomes the real claude
/// (exec keeps the pid). Anything else (no thread, -p, --resume, …) is plain claude.
pub(crate) fn stray(args: &[String]) -> Res<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(real_claude().ok_or("no claude on PATH (outside ~/.brigd/bin)")?);
    let thread = env::var("BRIGD_THREAD").unwrap_or_default();
    if valid_name(&thread, THREAD_MAX) && thread_dir(&thread).is_dir() && !stray_passthrough(args) {
        let setup = || -> Res<[String; 4]> {
            let dir = new_stray_dir(&thread_dir(&thread).join(tree::STRAY))?;
            let cwd = env::current_dir()?;
            fs::write(dir.join("cwd"), cwd.to_string_lossy().as_bytes())?;
            record_base(&cwd, &dir);
            fs::write(dir.join("pid"), std::process::id().to_string())?;
            fs::write(dir.join("term"), env::var("BRIGD_TERM").unwrap_or_default())?; // the terminal row shows this claude
            runner::session_args(&dir)
        };
        match setup() {
            Ok(a) => {
                cmd.args(a).args(["--permission-mode", "auto"]); // before the user's args, so their own --permission-mode wins
            }
            Err(e) => eprintln!("brigd: not recording this session: {e}"),
        }
    }
    Err(format!("exec claude: {}", cmd.args(args).exec()).into())
}

/// Args that pick their own session, print, or don't start a chat: no stray, plain claude.
fn stray_passthrough(args: &[String]) -> bool {
    const SUBCOMMANDS: &[&str] = &[
        "mcp",
        "plugin",
        "doctor",
        "update",
        "install",
        "setup-token",
        "config",
        "auth",
    ];
    const FLAGS: &[&str] = &[
        "-p",
        "--print",
        "-r",
        "--resume",
        "-c",
        "--continue",
        "--session-id",
        "-v",
        "--version",
        "-h",
        "--help",
    ];
    args.first()
        .is_some_and(|a| SUBCOMMANDS.contains(&a.as_str()))
        || args
            .iter()
            .any(|a| FLAGS.contains(&a.split('=').next().unwrap_or(a)))
}

/// Creates and returns <parent>/stray-<n>, the first n not taken (create_dir, so two at once never share one).
fn new_stray_dir(parent: &Path) -> Res<PathBuf> {
    fs::create_dir_all(parent)?;
    for n in 1.. {
        let d = parent.join(format!("stray-{n}"));
        match fs::create_dir(&d) {
            Ok(()) => return Ok(d),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    unreachable!()
}

/// ~/.brigd/bin: holds the `claude` shim brigd terminals put first on PATH.
fn shim_dir() -> PathBuf {
    root().join("bin")
}

/// The first executable `claude` on PATH outside the shim dir (and not brigd itself).
fn real_claude() -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let (shim, me) = (
        shim_dir(),
        env::current_exe().ok().and_then(|p| p.canonicalize().ok()),
    );
    let shim_c = shim.canonicalize().ok();
    env::split_paths(&env::var_os("PATH")?)
        .filter(|d| *d != shim && (shim_c.is_none() || d.canonicalize().ok() != shim_c))
        .map(|d| d.join("claude"))
        .find(|c| {
            c.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                && c.canonicalize().ok() != me
        })
}

/// Writes the `claude` shim and the zsh startup wrappers (an rc that prepends
/// ~/.local/bin would hide the shim; the wrappers re-prepend it after the user's
/// rc). Returns the shim dir.
pub(crate) fn ensure_shim() -> Res<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let dir = shim_dir();
    let shim = "#!/bin/sh\n[ -n \"$BRIGD_BIN\" ] && exec \"$BRIGD_BIN\" stray \"$@\"\n\
                PATH=$(printf %s \"$PATH\" | tr : '\\n' | grep -vxF \"$(dirname \"$0\")\" | paste -sd: -)\nexec claude \"$@\"\n";
    let zenv = "_brigd=$ZDOTDIR; ZDOTDIR=${BRIGD_ZDOTDIR:-$HOME}\n[ -f \"$ZDOTDIR/.zshenv\" ] && . \"$ZDOTDIR/.zshenv\"\nZDOTDIR=$_brigd; unset _brigd\n";
    let zrc = "if [ -n \"$BRIGD_ZDOTDIR\" ]; then ZDOTDIR=$BRIGD_ZDOTDIR; else unset ZDOTDIR; fi\n\
               [ -f \"${ZDOTDIR:-$HOME}/.zshrc\" ] && . \"${ZDOTDIR:-$HOME}/.zshrc\"\nPATH=\"$BRIGD_SHIM:$PATH\"\n";
    let z = root().join("zsh");
    for (path, text) in [
        (dir.join("claude"), shim),
        (z.join(".zshenv"), zenv),
        (z.join(".zshrc"), zrc),
    ] {
        if fs::read_to_string(&path).ok().as_deref() != Some(text) {
            fs::create_dir_all(path.parent().unwrap_or(&dir))?;
            fs::write(&path, text)?;
        }
    }
    fs::set_permissions(dir.join("claude"), fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stray_names_and_passthrough() {
        let d = env::temp_dir()
            .join(format!("brigd-stray-{}", std::process::id()))
            .join(tree::STRAY);
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("stray-1")).unwrap();
        fs::create_dir_all(d.join("stray-3")).unwrap();
        assert_eq!(new_stray_dir(&d).unwrap(), d.join("stray-2"));
        assert_eq!(new_stray_dir(&d).unwrap(), d.join("stray-4"));
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!stray_passthrough(&v(&[])));
        assert!(!stray_passthrough(&v(&[
            "--model",
            "opus",
            "fix the mcp bug"
        ])));
        for a in [
            &["-p", "hi"][..],
            &["--resume=abc"],
            &["-c"],
            &["--session-id", "x"],
            &["--version"],
            &["mcp", "list"],
            &["--model", "opus", "-h"],
        ] {
            assert!(stray_passthrough(&v(a)), "{a:?}");
        }
        let _ = fs::remove_dir_all(d.parent().unwrap());
    }
}
