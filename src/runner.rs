use std::path::Path;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::sleep;

/// Result of a single check run.
#[derive(Debug)]
pub struct RunOutcome {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

impl RunOutcome {
    pub fn ok(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// Run a check script via its `#!` shebang in its own session (process group),
/// kill the whole group on timeout.
pub async fn run_check(
    path: &Path,
    timeout: Duration,
    vars: &[(String, String)],
    libexec_dir: Option<&Path>,
) -> RunOutcome {
    let mut cmd = Command::new(path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // own process group so we can kill children (ssh, curl, ...) too
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    // helper tools dir goes first in PATH
    if let Some(libexec) = libexec_dir {
        let new_path = match std::env::var("PATH") {
            Ok(p) => format!("{}:{p}", libexec.display()),
            Err(_) => libexec.display().to_string(),
        };
        cmd.env("PATH", new_path);
    }

    for (k, v) in vars {
        cmd.env(k, v);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunOutcome {
                code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
            };
        }
    };

    let pid = child.id();
    let wait = child.wait_with_output();
    tokio::pin!(wait);

    tokio::select! {
        out = &mut wait => match out {
            Ok(out) => RunOutcome {
                code: out.status.code(),
                timed_out: false,
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            },
            Err(e) => RunOutcome {
                code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("wait failed: {e}"),
            },
        },
        _ = sleep(timeout) => {
            // negative pid = whole process group (we did setsid)
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            // the dropped wait_with_output future drops the Child; kill_on_drop(true)
            // makes tokio send SIGKILL on top of our killpg
            RunOutcome {
                code: None,
                timed_out: true,
                stdout: String::new(),
                stderr: String::new(),
            }
        }
    }
}
