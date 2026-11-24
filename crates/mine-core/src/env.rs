//! The login shell's environment. The desktop app launched from the Dock gets launchd's bare environment (a short `PATH`, none of
//! the user's shell settings), so it reads the login shell's environment once and starts the daemon with it.
//!
//! The values can hold tokens and secrets, so nothing here logs or prints them.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

/// The command the login shell runs. The marker separates the environment from whatever the profile and rc files print first.
const COMMAND: &str = r"printf '\0__MINE_ENV__\0'; env -0";
const MARKER: &[u8] = b"\0__MINE_ENV__\0";

/// Splits `env -0` output into `(name, value)` pairs. Entries are NUL-terminated and split at the first `=`.
/// Entries that aren't UTF-8, or whose name isn't a shell variable name (`[A-Za-z_][A-Za-z0-9_]*`), are skipped.
pub fn parse_env0(bytes: &[u8]) -> Vec<(String, String)> {
    bytes
        .split(|&b| b == 0)
        .filter_map(|entry| {
            let (name, value) = std::str::from_utf8(entry).ok()?.split_once('=')?;
            let valid = name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            valid.then(|| (name.to_owned(), value.to_owned()))
        })
        .collect()
}

/// The environment of an interactive login shell (`shell -i -l -c`), so `PATH` entries set in `.zshrc` (nvm, npm globals,
/// `~/.local/bin`) come along too. `None` if the shell can't start, fails, never prints the marker (an rc file exec'd something
/// else), or doesn't finish within `timeout`.
///
/// The shell leads a new session with no controlling tty. When the app was started from a terminal, an interactive zsh that
/// can still reach it stops itself as a background job. Without a tty it can't turn on job control either, so whatever the
/// rc files start stays in the shell's group, and on timeout that whole group is killed. A thread reads the output and
/// reaps the shell, so a slow shell never blocks past `timeout`.
// ponytail: a process that starts its own session (setsid, a daemonizing tool) escapes the timeout kill. Nothing tracks it.
pub fn login_env(shell: &str, timeout: Duration) -> Option<Vec<(String, String)>> {
    let mut cmd = Command::new(shell);
    cmd.args(["-i", "-l", "-c", COMMAND])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // SAFETY: after fork and before exec, only setsid, which is async-signal-safe, is called.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().ok()?;
    let pid = child.id() as libc::pid_t;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let read = stdout.read_to_end(&mut out);
        let ok = child.wait().is_ok_and(|status| status.success());
        let _ = tx.send((read.is_ok() && ok).then_some(out));
    });
    match rx.recv_timeout(timeout) {
        Ok(out) => {
            let out = out?;
            let at = out.windows(MARKER.len()).position(|w| w == MARKER)?;
            Some(parse_env0(&out[at + MARKER.len()..]))
        }
        Err(_) => {
            // SAFETY: kill takes only integer arguments. `-pid` is the group the shell leads (setsid made it the session's
            // only group). Killing it also ends anything the rc files started that still holds the output open, and the
            // reading thread then reaps the shell.
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    /// Writes an executable `/bin/sh` script at `dir/name`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn parse_env0_splits_nul_and_first_equals() {
        assert_eq!(
            parse_env0(b"PATH=/a:/b\0X=1=2\0"),
            [("PATH".into(), "/a:/b".into()), ("X".into(), "1=2".into())]
        );
    }

    #[test]
    fn parse_env0_skips_names_a_shell_cannot_have() {
        // Text printed before the first entry is glued onto its name.
        assert_eq!(
            parse_env0(b"Welcome\nHOME=/x\0A-B=1\0_ok1=2\0=3\0"),
            [("_ok1".into(), "2".into())]
        );
    }

    #[test]
    fn login_env_reads_fake_shell() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(
            tmp.path(),
            "sh",
            r"printf '\0__MINE_ENV__\0PATH=/fake/bin\0'",
        );
        let env = login_env(shell.to_str().unwrap(), Duration::from_secs(10)).unwrap();
        assert!(
            env.contains(&("PATH".into(), "/fake/bin".into())),
            "{env:?}"
        );
    }

    #[test]
    fn login_env_skips_what_rc_files_print() {
        // Like an rc file, the fake shell prints first (one line has an `=`), then runs the command it was given (`-i -l -c <command>`).
        let tmp = tempfile::tempdir().unwrap();
        let body = "printf 'Welcome\\nGREETING=hi there\\n'\nexport PATH=/fake/bin:/usr/bin:/bin\neval \"$4\"";
        let shell = script(tmp.path(), "sh", body);
        let env = login_env(shell.to_str().unwrap(), Duration::from_secs(10)).unwrap();
        // The rest is this test process's environment, so the failure message gives only the names.
        let names: Vec<&str> = env.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            env.contains(&("PATH".into(), "/fake/bin:/usr/bin:/bin".into())),
            "{names:?}"
        );
        assert!(
            !names
                .iter()
                .any(|n| n.contains("Welcome") || n.contains("GREETING")),
            "{names:?}"
        );
    }

    #[test]
    fn login_env_without_marker_is_none() {
        // The rc file exec'd something else, or the shell died before the command ran.
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(tmp.path(), "sh", r"printf 'PATH=/fake/bin\0'");
        assert_eq!(
            login_env(shell.to_str().unwrap(), Duration::from_secs(10)),
            None
        );
    }

    #[test]
    fn login_env_times_out() {
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(tmp.path(), "sh", "sleep 60");
        let started = Instant::now();
        assert_eq!(
            login_env(shell.to_str().unwrap(), Duration::from_millis(200)),
            None
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// An app started from a terminal has a controlling tty, and an interactive zsh in a background group of that tty stops
    /// itself. Reruns this test binary in a pty (`script`) to run [`login_env_under_a_terminal`] there.
    #[test]
    fn login_env_does_not_stop_under_a_terminal() {
        let out = Command::new("/usr/bin/script")
            .args(["-q", "/dev/null"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "env::tests::login_env_under_a_terminal"])
            .args(["--ignored", "--color", "never"])
            .env("MINE_ENV_PTY_TEST", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{log}");
        assert!(log.contains("login_env_under_a_terminal ... ok"), "{log}");
    }

    #[test]
    #[ignore = "run in a pty by login_env_does_not_stop_under_a_terminal"]
    fn login_env_under_a_terminal() {
        if std::env::var_os("MINE_ENV_PTY_TEST").is_none() {
            return;
        }
        assert!(fs::File::open("/dev/tty").is_ok(), "no controlling tty");
        // A real interactive zsh that reads no rc files (`-f`).
        let tmp = tempfile::tempdir().unwrap();
        let shell = script(tmp.path(), "zsh", "exec /bin/zsh -f \"$@\"");
        let started = Instant::now();
        let env = login_env(shell.to_str().unwrap(), Duration::from_secs(3));
        let took = started.elapsed();
        assert!(env.is_some(), "no environment after {took:?}");
        assert!(took < Duration::from_secs(2), "took {took:?}");
    }
}
