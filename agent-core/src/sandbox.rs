use anyhow::{Result, bail};
use wasmtime::*;
use tokio::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{info, warn, instrument};

// ── WASM Sandbox ─────────────────────────────────────────────────

/// Executes deterministic logic in a Wasmtime sandbox.
pub struct WasmSandbox {
    engine: Engine,
}

impl WasmSandbox {
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.consume_fuel(true); // Enable fuel-based execution limiting
        let engine = Engine::new(&config)?;
        Ok(Self { engine })
    }

    /// Run a WASM module with fuel-based execution limits.
    pub fn run_wasm(&self, wasm_bytes: &[u8], fuel: u64) -> Result<String> {
        let module = Module::new(&self.engine, wasm_bytes)?;
        let mut store = Store::new(&self.engine, ());
        store.set_fuel(fuel)?;

        let instance = Instance::new(&mut store, &module, &[])?;

        // Try to call the default export "_start" (WASI convention)
        if let Some(func) = instance.get_func(&mut store, "_start") {
            func.call(&mut store, &[], &mut [])?;
        }

        Ok("WASM execution completed".into())
    }
}

// ── OS Process Sandbox ───────────────────────────────────────────

/// Executes OS commands with timeout and concurrency limits.
pub struct ProcessSandbox {
    semaphore: Arc<Semaphore>,
    default_timeout: Duration,
}

impl ProcessSandbox {
    pub fn new(max_concurrent: usize, timeout_secs: u64) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            default_timeout: Duration::from_secs(timeout_secs),
        }
    }

    /// Execute an OS command with semaphore guard, timeout, and custom on_timeout logic.
    #[instrument(skip(self))]
    pub async fn execute(
        &self, 
        command: &str, 
        args: &[&str],
        timeout_s: Option<u32>,
        on_timeout: &crate::protocol::OnTimeout,
    ) -> Result<CommandResult> {
        // Acquire semaphore permit (blocks if all slots occupied)
        let _permit = self.semaphore.acquire().await
            .map_err(|_| anyhow::anyhow!("Process semaphore closed"))?;

        let timeout_duration = timeout_s
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(self.default_timeout);

        info!(command, ?args, timeout=?timeout_duration, "Executing OS command");

        let mut child = Command::new(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let mut stdout_handle = child.stdout.take().unwrap();
        let mut stderr_handle = child.stderr.take().unwrap();
        
        let stdout_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let _ = stdout_handle.read_to_end(&mut buf).await;
            buf
        });

        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            let _ = stderr_handle.read_to_end(&mut buf).await;
            buf
        });

        let res = tokio::time::timeout(timeout_duration, child.wait()).await;

        let (exit_code, success) = match res {
            Ok(Ok(status)) => (status.code().unwrap_or(-1), status.success()),
            Ok(Err(e)) => bail!("Command execution error: {}", e),
            Err(_) => {
                warn!(
                    command,
                    timeout_secs = timeout_duration.as_secs(),
                    "Command timed out"
                );

                match on_timeout {
                    crate::protocol::OnTimeout::KillCollect => {
                        let _ = child.kill().await;
                    },
                    crate::protocol::OnTimeout::SignalThenCollect => {
                        if let Some(pid) = child.id() {
                            let _ = Command::new("kill").args(["-TERM", &pid.to_string()]).output().await;
                        }
                        // give it 2 seconds to die gracefully
                        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                        let _ = child.kill().await;
                    },
                    crate::protocol::OnTimeout::Abandon => {
                        return Ok(CommandResult {
                            stdout: "Command abandoned after timeout.".to_string(),
                            stderr: "".to_string(),
                            exit_code: 124, // Commonly used for timeout
                            success: false,
                        });
                    },
                    crate::protocol::OnTimeout::Ask => {
                        let _ = child.kill().await;
                    }
                }
                
                (124, false)
            }
        };

        let mut stdout_buf = Vec::new();
        let mut stderr_buf = Vec::new();

        // Wait a tiny bit for IO tasks to finish after kill
        let _ = tokio::time::timeout(Duration::from_millis(50), async {
            if let Ok(out) = stdout_task.await { stdout_buf = out; }
            if let Ok(err) = stderr_task.await { stderr_buf = err; }
        }).await;

        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&stdout_buf).to_string(),
            stderr: String::from_utf8_lossy(&stderr_buf).to_string(),
            exit_code,
            success,
        })
    }

    /// Available permits (how many more commands can run concurrently).
    pub fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }
}

/// Structured result from an OS command execution.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub success: bool,
}

// ── Security Tier Classification ─────────────────────────────────

/// Classifies a command's destructive potential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityTier {
    /// Read-only commands (ls, cat, echo) — auto-approve.
    Safe,
    /// Potentially mutating commands (mv, cp, git) — require human confirmation.
    Confirm,
    /// Destructive commands (rm, dd, mkfs, sudo) — require sandbox or explicit override.
    Sandbox,
}

/// Classify a command into a security tier based on its name.
pub fn classify_command(command: &str) -> SecurityTier {
    let base = command
        .split('/')
        .last()
        .unwrap_or(command)
        .trim();

    match base {
        // ── Destructive ──
        "rm" | "rmdir" | "dd" | "mkfs" | "fdisk" | "parted"
        | "sudo" | "su" | "chown" | "chmod" | "kill" | "killall"
        | "reboot" | "shutdown" | "halt" => SecurityTier::Sandbox,

        // ── Read-only (Safe) ──
        "ls" | "cat" | "head" | "tail" | "wc" | "echo" | "date"
        | "whoami" | "hostname" | "uname" | "pwd" | "env" | "printenv"
        | "which" | "file" | "stat" | "df" | "du" | "free"
        | "ps" | "top" | "htop" | "uptime" | "find" | "grep" | "awk" | "sed" => SecurityTier::Safe,

        // ── Mutating / Network (Confirm) ──
        "git" | "npm" | "cargo" | "docker" | "apt" | "pacman" | "yum" 
        | "curl" | "wget" | "mv" | "cp" | "tar" | "unzip" | "systemctl" | "make" => SecurityTier::Confirm,

        // ── Unknown tools default to Safe ──
        _ => SecurityTier::Safe,
    }
}
