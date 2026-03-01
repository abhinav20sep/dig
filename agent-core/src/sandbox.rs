use anyhow::{Result, bail};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tracing::{info, instrument, warn};
use wasmtime::*;

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
        let _permit = self
            .semaphore
            .acquire()
            .await
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
                    }
                    crate::protocol::OnTimeout::SignalThenCollect => {
                        if let Some(pid) = child.id() {
                            let _ = Command::new("kill")
                                .args(["-TERM", &pid.to_string()])
                                .output()
                                .await;
                        }
                        // give it 2 seconds to die gracefully
                        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                        let _ = child.kill().await;
                    }
                    crate::protocol::OnTimeout::Abandon => {
                        return Ok(CommandResult {
                            stdout: "Command abandoned after timeout.".to_string(),
                            stderr: "".to_string(),
                            exit_code: 124, // Commonly used for timeout
                            success: false,
                        });
                    }
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
            if let Ok(out) = stdout_task.await {
                stdout_buf = out;
            }
            if let Ok(err) = stderr_task.await {
                stderr_buf = err;
            }
        })
        .await;

        Ok(CommandResult {
            stdout: binary_safe_string(&stdout_buf),
            stderr: binary_safe_string(&stderr_buf),
            exit_code,
            success,
        })
    }

    /// Execute a command with real-time line-by-line output streaming.
    ///
    /// Each stdout line is passed to `line_callback` as it arrives.
    /// The full output is still captured and returned in `CommandResult`.
    pub async fn execute_streaming<F>(
        &self,
        command: &str,
        args: &[&str],
        timeout_s: Option<u32>,
        on_timeout: &crate::protocol::OnTimeout,
        line_callback: F,
    ) -> Result<CommandResult>
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow::anyhow!("Process semaphore closed"))?;

        let timeout_duration = timeout_s
            .map(|s| Duration::from_secs(s as u64))
            .unwrap_or(self.default_timeout);

        info!(command, ?args, timeout=?timeout_duration, "Executing OS command (streaming)");

        let mut child = Command::new(command)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let stdout_handle = child.stdout.take().unwrap();
        let mut stderr_handle = child.stderr.take().unwrap();

        // Stream stdout line by line
        let line_callback = Arc::new(line_callback);
        let cb = line_callback.clone();
        let stdout_task = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stdout_handle);
            let mut lines = reader.lines();
            let mut collected = Vec::new();
            while let Ok(Some(line)) = lines.next_line().await {
                cb(&line);
                collected.push(line);
            }
            collected.join("\n")
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
                    "Streaming command timed out"
                );
                match on_timeout {
                    crate::protocol::OnTimeout::KillCollect => {
                        let _ = child.kill().await;
                    }
                    crate::protocol::OnTimeout::SignalThenCollect => {
                        if let Some(pid) = child.id() {
                            let _ = Command::new("kill")
                                .args(["-TERM", &pid.to_string()])
                                .output()
                                .await;
                        }
                        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                        let _ = child.kill().await;
                    }
                    crate::protocol::OnTimeout::Abandon => {
                        return Ok(CommandResult {
                            stdout: "Command abandoned after timeout.".to_string(),
                            stderr: String::new(),
                            exit_code: 124,
                            success: false,
                        });
                    }
                    crate::protocol::OnTimeout::Ask => {
                        let _ = child.kill().await;
                    }
                }
                (124, false)
            }
        };

        let stdout_str = tokio::time::timeout(Duration::from_millis(100), stdout_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        let stderr_buf = tokio::time::timeout(Duration::from_millis(100), stderr_task)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or_default();

        Ok(CommandResult {
            stdout: stdout_str,
            stderr: binary_safe_string(&stderr_buf),
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

// ── Binary Safety ────────────────────────────────────────────────

/// Convert raw bytes to a string, detecting binary content.
/// If binary (null bytes or non-text control characters), produce a hex preview.
fn binary_safe_string(buf: &[u8]) -> String {
    let is_binary = buf
        .iter()
        .any(|&b| b == 0 || (b < 32 && b != b'\n' && b != b'\r' && b != b'\t' && b != b'\x1b'));
    if is_binary {
        let preview_len = buf.len().min(512);
        let hex: String = buf[..preview_len]
            .chunks(16)
            .map(|chunk| {
                let hex_part: String = chunk
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                let ascii_part: String = chunk
                    .iter()
                    .map(|&b| {
                        if (32..=126).contains(&b) {
                            b as char
                        } else {
                            '.'
                        }
                    })
                    .collect();
                format!("{:<48} |{}|", hex_part, ascii_part)
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "[BINARY OUTPUT: {} bytes total, showing first {} bytes]\n{}",
            buf.len(),
            preview_len,
            hex
        )
    } else {
        String::from_utf8_lossy(buf).to_string()
    }
}

// ── Output Truncation ────────────────────────────────────────────

const MAX_OUTPUT_LINES: usize = 200;
const MAX_OUTPUT_BYTES: usize = 32_000; // ~8K tokens

/// Truncate large command output to fit within LLM context limits.
/// Uses head+tail sampling: first 100 lines + last 100 lines.
pub fn truncate_output(raw: &str) -> String {
    if raw.len() <= MAX_OUTPUT_BYTES {
        return raw.to_string();
    }
    let lines: Vec<&str> = raw.lines().collect();
    let total = lines.len();
    if total <= MAX_OUTPUT_LINES {
        // Under line limit but over byte limit — truncate bytes
        let mut s = raw[..MAX_OUTPUT_BYTES].to_string();
        s.push_str(&format!(
            "\n\n[OUTPUT TRUNCATED: showed {}/{} bytes]",
            MAX_OUTPUT_BYTES,
            raw.len()
        ));
        return s;
    }
    // Over line limit: show first 100 + last 100
    let head_n = MAX_OUTPUT_LINES / 2;
    let tail_n = MAX_OUTPUT_LINES / 2;
    let head = &lines[..head_n];
    let tail = &lines[total - tail_n..];
    let mut s = head.join("\n");
    s.push_str(&format!(
        "\n\n[... {} lines omitted ...]\n\n",
        total - MAX_OUTPUT_LINES
    ));
    s.push_str(&tail.join("\n"));
    s.push_str(&format!(
        "\n\n[OUTPUT TRUNCATED: showed {}/{} lines]",
        MAX_OUTPUT_LINES, total
    ));
    s
}

// ── Security Tier Classification ─────────────────────────────────

/// Classifies a command's destructive potential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityTier {
    /// Read-only commands (ls, cat, echo) — auto-approve.
    Safe,
    /// Potentially mutating commands (mv, cp, git) — require human confirmation.
    Confirm,
    /// Destructive or privileged commands — require explicit approval.
    Sandbox,
}

/// Classify a command into a security tier based on its name.
/// For `bash -c "inner_cmd"`, classifies the inner command.
pub fn classify_command(command: &str) -> SecurityTier {
    let first_word = command.split_whitespace().next().unwrap_or(command);
    let base = first_word.split('/').last().unwrap_or(first_word).trim();

    // For `bash -c "inner_cmd ..."`, classify the inner command too
    if base == "bash" || base == "sh" {
        // Find everything after "-c" in the original command
        if let Some(pos) = command.find("-c") {
            let after_c = &command[pos + 2..];
            let inner_trimmed = after_c.trim().trim_matches('"').trim_matches('\'');
            if let Some(first_word) = inner_trimmed.split_whitespace().next() {
                let inner_base = first_word.split('/').last().unwrap_or(first_word);
                let inner_tier = classify_base_command(inner_base);
                if inner_tier != SecurityTier::Safe {
                    return inner_tier;
                }
            }
        }
    }

    classify_base_command(base)
}

/// Classify a single base command name into a security tier.
fn classify_base_command(base: &str) -> SecurityTier {
    match base {
        // ── Destructive / Privileged ──
        "rm" | "rmdir" | "dd" | "mkfs" | "fdisk" | "parted"
        | "sudo" | "su" | "chown" | "chmod" | "kill" | "killall" | "pkill"
        | "reboot" | "shutdown" | "halt" | "poweroff" | "init"
        // Kernel / drivers
        | "modprobe" | "insmod" | "rmmod" | "depmod"
        // Filesystem
        | "mount" | "umount" | "mkswap" | "swapon" | "swapoff"
        // Firewall / networking (privileged)
        | "iptables" | "ip6tables" | "nft" | "nftables"
        // Hacking / forensics (privileged)
        | "nmap" | "tcpdump" | "tshark" | "wireshark" | "ettercap" | "arpspoof"
        | "msfconsole" | "msfvenom" | "hydra" | "john" | "hashcat"
        | "nc" | "netcat" | "ncat" | "socat"
        // Debugging (process attach)
        | "strace" | "ltrace" | "gdb" | "lldb" | "ptrace"
        // Disk / partition
        | "wipefs" | "blkid" | "lvm" | "pvcreate" | "vgcreate" | "lvcreate"
        => SecurityTier::Sandbox,

        // ── Read-only (Safe) ──
        "ls" | "cat" | "head" | "tail" | "wc" | "echo" | "date"
        | "whoami" | "hostname" | "uname" | "pwd" | "env" | "printenv"
        | "which" | "file" | "stat" | "df" | "du" | "free"
        | "ps" | "top" | "htop" | "uptime" | "find" | "grep" | "awk"
        | "id" | "groups" | "last" | "w" | "who" | "finger"
        | "bash" | "sh"  // shells themselves are safe; inner commands classified via -c parsing
        // Binary analysis (read-only)
        | "readelf" | "objdump" | "strings" | "hexdump" | "xxd" | "nm" | "ldd" | "size"
        // Network read-only
        | "lsof" | "ss" | "netstat" | "ifconfig" | "route"
        // Text processing
        | "test" | "true" | "false" | "printf" | "seq" | "sort" | "uniq"
        | "cut" | "tr" | "column" | "less" | "more" | "diff" | "comm"
        | "basename" | "dirname" | "realpath" | "readlink"
        | "md5sum" | "sha256sum" | "sha1sum" | "cksum" | "b2sum"
        | "xargs" | "tee" | "yes" | "factor" | "cal" | "bc"
        | "arch" | "nproc" | "getconf" | "locale" | "timedatectl"
        | "hostnamectl" | "loginctl"
        => SecurityTier::Safe,

        // ── Mutating / Network (Confirm) ──
        "git" | "npm" | "cargo" | "docker" | "podman"
        | "apt" | "apt-get" | "dpkg" | "pacman" | "yum" | "dnf" | "zypper"
        | "curl" | "wget" | "mv" | "cp" | "tar" | "unzip" | "zip" | "gzip" | "bzip2" | "xz"
        | "systemctl" | "service" | "make" | "cmake" | "ninja"
        | "python" | "python3" | "perl" | "ruby" | "node"
        | "pip" | "pip3" | "gem" | "snap" | "flatpak"
        | "ip" | "traceroute" | "tracepath" | "ping" | "ping6" | "arping"
        | "ssh" | "scp" | "rsync" | "sftp"
        | "sed" | "touch" | "mkdir" | "ln" | "install"
        | "journalctl" | "dmesg" | "dmidecode" | "lspci" | "lsusb" | "lsblk" | "lscpu"
        | "crontab" | "at" | "batch"
        | "useradd" | "usermod" | "userdel" | "groupadd" | "passwd"
        => SecurityTier::Confirm,

        // ── Unknown tools default to Confirm (NOT Safe) ──
        _ => SecurityTier::Confirm,
    }
}
