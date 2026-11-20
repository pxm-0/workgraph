//! Bounded shell commands: one process group, SIGKILL on timeout, exit 124, a 40-line tail.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const TAIL_LINES: usize = 40;

/// The last 40 lines of `text`.
pub fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n")
}

/// Run one shell command with stdout and stderr interleaved; kill its whole process group on timeout.
pub fn run_shell(cmd: &str, timeout: Duration) -> (i64, String) {
    shell(cmd, None, timeout)
}

/// `run_shell` in `dir` with exactly `env` (nothing inherited).
pub fn run_shell_in(
    cmd: &str,
    dir: &std::path::Path,
    env: &[(String, String)],
    timeout: Duration,
) -> (i64, String) {
    shell(cmd, Some((dir, env)), timeout)
}

fn shell(
    cmd: &str,
    within: Option<(&std::path::Path, &[(String, String)])>,
    timeout: Duration,
) -> (i64, String) {
    let mut command = Command::new("sh");
    command
        .arg("-c")
        .arg(format!("{{ {cmd}\n}} 2>&1"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .process_group(0);
    if let Some((dir, env)) = within {
        command
            .current_dir(dir)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)));
    }
    let child = command.spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (127, format!("failed to start: {e}")),
    };
    let mut stdout = child.stdout.take().expect("stdout is piped");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.code().map(i64::from).unwrap_or(-1),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = Command::new("kill")
                    .args(["-9", &format!("-{}", child.id())])
                    .status();
                let _ = child.wait();
                timed_out = true;
                break 124;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break -1,
        }
    };
    let mut out = String::from_utf8_lossy(&reader.join().unwrap_or_default()).into_owned();
    if timed_out {
        out.push_str(&format!("\n[wg: timed out after {}s]", timeout.as_secs()));
    }
    (code, tail(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_40_lines() {
        let text: String = (1..=50).map(|i| format!("l{i}\n")).collect();
        let t = tail(&text);
        assert_eq!(t.lines().count(), 40);
        assert!(t.starts_with("l11\n") && t.ends_with("l50"));
        assert_eq!(tail("one"), "one");
        assert_eq!(tail(""), "");
    }
}
