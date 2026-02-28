use anyhow::Result;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::sandbox::CommandResult;

/// Unique marker used to detect end of command output.
const CMD_DONE_MARKER: &str = "___DIG_CMD_DONE___";

/// A persistent shell session that maintains state (cwd, env vars, aliases)
/// across multiple commands. Commands are sent to a single bash process
/// via stdin, and output is captured until a sentinel marker appears.
pub struct PersistentShell {
    child: Child,
    stdin: tokio::process::ChildStdin,
    stdout_reader: BufReader<tokio::process::ChildStdout>,
    stderr_reader: BufReader<tokio::process::ChildStderr>,
}

impl PersistentShell {
    /// Spawn a new persistent bash shell.
    pub fn new() -> Result<Self> {
        let mut child = Command::new("bash")
            .arg("--norc")
            .arg("--noprofile")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture shell stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture shell stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("Failed to capture shell stderr"))?;

        let stdout_reader = BufReader::new(stdout);
        let stderr_reader = BufReader::new(stderr);

        info!("Persistent shell session started");

        Ok(Self {
            child,
            stdin,
            stdout_reader,
            stderr_reader,
        })
    }

    /// Execute a command in the persistent shell and return its output.
    ///
    /// The command is sent to the running bash process via stdin,
    /// followed by a sentinel `echo` to mark the end of output.
    /// We read stdout line-by-line until we see the sentinel.
    pub async fn run_command(&mut self, cmd: &str) -> Result<CommandResult> {
        // Send the command followed by the exit code capture + sentinel
        let full_cmd = format!(
            "{}\n__dig_ec__=$?\necho \"{} $__dig_ec__\"\necho \"{}\" >&2\n",
            cmd.trim(),
            CMD_DONE_MARKER,
            CMD_DONE_MARKER,
        );
        self.stdin.write_all(full_cmd.as_bytes()).await?;
        self.stdin.flush().await?;

        // Read stdout until we see the sentinel
        let mut stdout_lines = Vec::new();
        let mut exit_code: i32 = 0;

        loop {
            let mut line = String::new();
            let bytes_read = tokio::time::timeout(
                std::time::Duration::from_secs(120),
                self.stdout_reader.read_line(&mut line),
            )
            .await;

            match bytes_read {
                Ok(Ok(0)) => {
                    warn!("Persistent shell stdout closed unexpectedly");
                    break;
                }
                Ok(Ok(_)) => {
                    let trimmed = line.trim_end();
                    if trimmed.starts_with(CMD_DONE_MARKER) {
                        // Parse exit code from "___DIG_CMD_DONE___ <exit_code>"
                        if let Some(ec_str) = trimmed.strip_prefix(CMD_DONE_MARKER) {
                            exit_code = ec_str.trim().parse().unwrap_or(0);
                        }
                        break;
                    }
                    stdout_lines.push(trimmed.to_string());
                }
                Ok(Err(e)) => {
                    warn!("Error reading from persistent shell: {}", e);
                    break;
                }
                Err(_) => {
                    warn!("Persistent shell command timed out (120s)");
                    exit_code = 124;
                    break;
                }
            }
        }

        // Drain stderr (non-blocking, best effort)
        let mut stderr_lines = Vec::new();
        loop {
            let mut line = String::new();
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                self.stderr_reader.read_line(&mut line),
            )
            .await;

            match result {
                Ok(Ok(0)) => break,
                Ok(Ok(_)) => {
                    let trimmed = line.trim_end();
                    if trimmed.starts_with(CMD_DONE_MARKER) {
                        break;
                    }
                    stderr_lines.push(trimmed.to_string());
                }
                _ => break,
            }
        }

        let stdout = stdout_lines.join("\n");
        let stderr = stderr_lines.join("\n");
        let success = exit_code == 0;

        Ok(CommandResult {
            stdout,
            stderr,
            exit_code,
            success,
        })
    }

    /// Check if the underlying shell process is still running.
    pub fn is_alive(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(Some(_)) => false, // Process has exited
            Ok(None) => true,     // Still running
            Err(_) => false,      // Error checking — assume dead
        }
    }
}

impl Drop for PersistentShell {
    fn drop(&mut self) {
        info!("Persistent shell session dropped — killing child process");
        // kill_on_drop is set, so this is just logging
    }
}
