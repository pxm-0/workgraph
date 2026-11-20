//! Spawning a child with a cleared environment, the prompt on stdin, its own process group and a
//! wall-clock timeout that kills the whole group.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// One child to run. `env` is the complete environment (nothing is inherited).
#[derive(Clone, Debug)]
pub struct Spawn {
    pub label: String,
    pub program: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub stdin: String,
    pub timeout: Duration,
}

#[derive(Clone, Debug, Default)]
pub struct RawOutput {
    /// `None` when the child was killed or never started.
    pub exit: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
}

/// The process port: the real one spawns, test fakes answer by label.
pub trait Proc: Sync {
    fn run(&self, s: &Spawn) -> RawOutput;
}

pub struct RealProc;

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            // A read error ends the capture; whatever arrived is still classified.
            let _ = r.read_to_end(&mut buf);
        }
        buf
    })
}

impl Proc for RealProc {
    fn run(&self, s: &Spawn) -> RawOutput {
        let child = Command::new(&s.program)
            .args(&s.argv)
            .env_clear()
            .envs(s.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&s.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                return RawOutput {
                    stderr: format!("failed to start {}: {e}", s.program),
                    ..RawOutput::default()
                }
            }
        };
        let stdin = child.stdin.take();
        let prompt = s.stdin.clone();
        let writer = std::thread::spawn(move || {
            if let Some(mut w) = stdin {
                // A child that exits early closes the pipe; that is its answer, not ours to fix.
                let _ = w.write_all(prompt.as_bytes());
            }
        });
        let out = drain(child.stdout.take());
        let err = drain(child.stderr.take());
        let start = Instant::now();
        let (exit, timed_out) = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (status.code(), false),
                Ok(None) if start.elapsed() >= s.timeout => {
                    kill_group(child.id());
                    let _ = child.wait();
                    break (None, true);
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break (None, false),
            }
        };
        let _ = writer.join();
        let text = |h: std::thread::JoinHandle<Vec<u8>>| {
            String::from_utf8_lossy(&h.join().unwrap_or_default()).into_owned()
        };
        RawOutput {
            exit,
            timed_out,
            stdout: text(out),
            stderr: text(err),
        }
    }
}

/// SIGKILL to the whole process group.
pub fn kill_group(pid: u32) {
    let _ = Command::new("kill")
        .args(["-9", &format!("-{pid}")])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout_ms: u64) -> RawOutput {
        RealProc.run(&Spawn {
            label: "t".into(),
            program: "/bin/sh".into(),
            argv: vec!["-c".into(), script.into()],
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("X".into(), "1".into()),
            ],
            cwd: std::env::temp_dir(),
            stdin: "hello".into(),
            timeout: Duration::from_millis(timeout_ms),
        })
    }

    #[test]
    fn stdin_env_and_exit_code() {
        let o = sh("cat; echo \" $X $HOME\"; echo err >&2; exit 3", 5000);
        assert_eq!(o.exit, Some(3));
        assert_eq!(
            o.stdout, "hello 1 \n",
            "only the given env reaches the child"
        );
        assert_eq!(o.stderr, "err\n");
        assert!(!o.timed_out);
    }

    #[test]
    fn timeout_kills_the_group() {
        let o = sh("sleep 30 & sleep 30", 200);
        assert!(o.timed_out);
        assert_eq!(o.exit, None);
        let missing = RealProc.run(&Spawn {
            program: "/nonexistent/claude".into(),
            argv: vec![],
            ..Spawn {
                label: "t".into(),
                program: String::new(),
                argv: vec![],
                env: vec![],
                cwd: std::env::temp_dir(),
                stdin: String::new(),
                timeout: Duration::from_secs(1),
            }
        });
        assert_eq!(missing.exit, None);
        assert!(missing.stderr.contains("failed to start"));
    }
}
