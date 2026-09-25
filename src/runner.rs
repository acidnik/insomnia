use std::path::Path;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::sleep;

/// Result of a single check run.
#[derive(Debug)]
pub struct RunOutcome {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
}

impl RunOutcome {
    pub fn ok(&self) -> bool {
        !self.timed_out && self.code == Some(0)
    }
}

/// drain a pipe to the end; used both for normal completion and after a
/// timeout kill (the dead process's pipes close, leaving whatever it
/// managed to print in the buffer — that is the error message we must not
/// lose)
async fn drain<R: tokio::io::AsyncRead + Unpin>(mut pipe: Option<R>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(p) = pipe.as_mut() {
        let _ = p.read_to_end(&mut buf).await;
    }
    buf
}

/// Run a check script via its `#!` shebang in its own session (process group),
/// kill the whole group on timeout. Output printed before the kill is kept.
pub async fn run_check(
    path: &Path,
    timeout: Duration,
    vars: &[(String, String)],
    libexec_dir: Option<&Path>,
) -> RunOutcome {
    let started = Instant::now();
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

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            return RunOutcome {
                code: None,
                timed_out: false,
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
                duration: started.elapsed(),
            };
        }
    };

    let pid = child.id();
    // take the pipes so readers can drain them independently of the child
    let out_reader = tokio::spawn(drain(child.stdout.take()));
    let err_reader = tokio::spawn(drain(child.stderr.take()));

    let (code, timed_out, wait_err) = tokio::select! {
        status = child.wait() => match status {
            Ok(s) => (s.code(), false, None),
            Err(e) => (None, false, Some(format!("wait failed: {e}"))),
        },
        _ = sleep(timeout) => {
            // negative pid = whole process group (we did setsid)
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            (None, true, None)
        }
    };

    // the killed processes' pipes are closed by now — readers return with
    // whatever was printed before the kill
    let stdout = out_reader.await.unwrap_or_default();
    let stderr = err_reader.await.unwrap_or_default();

    RunOutcome {
        code,
        timed_out,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: match (wait_err, String::from_utf8_lossy(&stderr).is_empty()) {
            (Some(e), _) => e,
            (None, true) => String::new(),
            (None, false) => String::from_utf8_lossy(&stderr).into_owned(),
        },
        duration: started.elapsed(),
    }
}
