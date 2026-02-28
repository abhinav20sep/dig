# dig

**Natural language agentic terminal executor** — ask questions in plain English, get bash commands executed for you.

```
$ dig what is my ip
192.168.1.6

$ dig find all rust files larger than 10KB sorted by size
agent-core/src/agent_loop.rs    44K
agent-core/src/memory.rs        21K
src/main.rs                     21K

$ echo "Hello World" | dig reverse this text
dlroW olleH
```

---

## What It Does

`dig` sits between you and your terminal. You type natural language — it translates to bash, executes it, and gives you the result. No more Stack Overflow. No more `man` pages.

**One tool. One prompt. Done.**

## Capabilities

### 🔧 Linux System Administration

| Task | Example |
|------|---------|
| File operations | `dig find all log files modified in the last hour` |
| Process management | `dig show me the top 5 memory-consuming processes` |
| Disk & filesystem | `dig show disk usage by directory sorted by size` |
| sysfs / procfs | `dig what CPU governor is active` |
| Kernel & drivers | `dig search dmesg for USB errors` |
| Systemd & services | `dig restart the nginx service` |
| Package management | `dig install htop` |
| User management | `dig list all users with login shells` |
| Cron & scheduling | `dig show me all cron jobs for the current user` |

### 🌐 Network Administration

| Task | Example |
|------|---------|
| IP & interfaces | `dig what is my IPv4 ip` |
| Host discovery | `dig what hosts are reachable on my network` |
| Port scanning | `dig scan open ports on 192.168.1.1` |
| DNS & resolution | `dig resolve google.com to its IP addresses` |
| Firewall rules | `dig show all iptables rules` |
| Routing | `dig show the routing table` |
| Traffic capture | `dig capture 10 packets on eth0` |
| Connection tracking | `dig show all established connections to port 443` |

### 🔐 Security & Penetration Testing

| Task | Example |
|------|---------|
| Privilege escalation checks | `dig find all SUID binaries on this system` |
| Open port enumeration | `dig run a full TCP scan on 10.0.0.0/24` |
| Log analysis | `dig search auth.log for failed login attempts today` |
| File integrity | `dig compute sha256 of all binaries in /usr/bin` |
| Network sniffing | `dig listen for ARP requests on the local network` |
| SSL/TLS inspection | `dig check the TLS certificate of example.com` |

> **`dig` can execute any bash command the LLM can conceive.** The limit is the LLM's knowledge, not the tool.

---

## Architecture

```
┌──────────────┐     JSON v1.1      ┌──────────────┐
│   Terminal    │ ←────────────────→ │   LLM Brain  │
│  (Executor)  │   MessageEnvelope  │  (OpenRouter) │
└──────┬───────┘                    └──────┬───────┘
       │                                   │
       │  ┌────────────────────────────────┘
       │  │
  ┌────▼──▼─────┐    ┌──────────────┐    ┌──────────────┐
  │  Agent Loop  │───→│   Sandbox    │───→│  bash -c ... │
  │  (Rust Core) │    │ Safe/Confirm │    └──────────────┘
  └──────┬───────┘    └──────────────┘
         │
    ┌────▼────────┐
    │ HybridMemory │
    │  (LanceDB)   │
    │  • Jaccard    │
    │    Cache      │
    │  • Semantic   │
    │    History    │
    └──────────────┘
```

### Key Components

| Component | File | Purpose |
|-----------|------|---------|
| CLI Entry | `src/main.rs` | One-shot & REPL modes, config loading, stdin pipe detection |
| Agent Loop | `agent-core/src/agent_loop.rs` | LLM ↔ Executor conversation loop, cache check, action dispatch |
| Memory | `agent-core/src/memory.rs` | Jaccard command cache, semantic history (LanceDB), context injection |
| Sandbox | `agent-core/src/sandbox.rs` | Security tier classification (Safe/Confirm/Sandbox), process execution |
| Protocol | `agent-core/src/protocol.rs` | JSON v1.1 MessageEnvelope schema, typed payloads |
| Governor | `agent-core/src/governor.rs` | Token budget, turn limits, rate limiting |
| LLM Provider | `agent-core/src/providers/openai.rs` | OpenAI-compatible API client with exponential backoff |

---

## Features

- **Dual Mode** — `dig <query>` for one-shot, `dig` for interactive REPL
- **Stdin Pipe** — `cat file.txt | dig summarize this`
- **Security Tiers** — 80+ classified commands across Safe/Confirm/Sandbox tiers; unknown commands default to **Confirm**
- **`bash -c` Inner Parsing** — `bash -c "rm -rf /"` is correctly classified as **Sandbox**, not Safe
- **Jaccard Command Cache** — Repeated queries bypass the LLM entirely (no API calls)
- **30-Turn Context Memory** — Remembers your last 30 queries for conversational continuity  
- **Semantic History** — LanceDB vector search for relevant past interactions
- **Token Governor** — Hard budget limits prevent runaway API costs
- **Output Truncation** — Large outputs (e.g. `find /`, `dmesg`) are capped at 200 lines / 32KB before feeding to the LLM; head+tail sampling preserves context
- **Binary Data Safety** — Non-UTF8 output (hex dumps, raw binary) is auto-converted to a hex preview, preventing UTF-8 decode errors
- **`run_as: root` / sudo** — LLM can request privilege escalation; `dig` auto-prepends `sudo` when needed
- **CWD Tracking** — Current working directory is included in every executor context for accurate relative-path commands
- **Streaming Execution** — `ExecMode: Streaming` prints output line-by-line in real-time
- **Persistent Shell Session** — `shell_session.rs` spawns a single `bash` process; `cd`, `export`, and aliases persist across commands
- **DIG_DEBUG** — Set `DIG_DEBUG=1` to see full LLM request/response traces

---

## Installation

### Prerequisites

- **Rust** (1.85+) — `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- **OpenRouter API key** — Sign up at [openrouter.ai](https://openrouter.ai)

### Build

```bash
git clone https://github.com/abhinav20sep/dig.git
cd dig
export OPENROUTER_API_KEY="sk-or-..."
cargo build --release
```

The binary is at `./target/release/dig`. Copy it to your PATH:

```bash
sudo cp ./target/release/dig /usr/local/bin/
```

### Configuration

Copy and edit `config.toml`:

```bash
cp config.toml ~/.config/dig/config.toml
```

```toml
default_model = "openai/gpt-4o-mini"
token_budget = 500000

[[providers]]
name = "openai"
api_base = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "openai/gpt-4o-mini"
```

Config lookup order: `./config.toml` → `~/.config/dig/config.toml` → next to binary.

---

## Usage

```bash
# One-shot
dig show me disk usage by directory

# Piped input
cat /var/log/syslog | dig find all error lines from today

# Interactive REPL
dig
> what services are running
> restart apache2
> show me the last 20 lines of the error log

# Debug mode
DIG_DEBUG=1 dig what is my ip
```

### Security Tiers

| Tier | Behavior | Examples |
|------|----------|---------|
| **Safe** | Auto-executes | `ls`, `cat`, `grep`, `find`, `ps`, `df`, `uname`, `readelf`, `objdump`, `strings`, `hexdump`, `nm`, `bash`, `sh` |
| **Confirm** | Prompts before executing | `git`, `docker`, `curl`, `wget`, `mv`, `cp`, `apt`, `systemctl`, `python`, `dmesg`, `journalctl`, `ssh`, `ping`, `ip` |
| **Sandbox** | Requires explicit approval | `rm`, `dd`, `sudo`, `chmod`, `chown`, `kill`, `reboot`, `nmap`, `tcpdump`, `strace`, `gdb`, `nc`, `modprobe`, `iptables`, `hashcat` |
| **Unknown** | Defaults to **Confirm** | Any command not in the above lists |

---

## Testing

```bash
# Unit tests (20 tests)
cargo test -p agent-core

# CLI integration tests (25 tests, 31 assertions)
./test_cli.sh
```

---

## Project Structure

```
dig/
├── src/
│   ├── main.rs          # CLI entry, REPL loop, one-shot dispatch
│   └── config.rs        # TOML config loading
├── agent-core/
│   └── src/
│       ├── agent_loop.rs    # Core LLM ↔ Executor loop
│       ├── memory.rs        # Jaccard cache + LanceDB semantic memory
│       ├── sandbox.rs       # Process execution + security tiers (80+ tools)
│       ├── shell_session.rs # Persistent bash shell session (stateful env)
│       ├── protocol.rs      # JSON v1.1 message envelope schema
│       ├── governor.rs      # Token budget + rate limiter
│       ├── sanitizer.rs     # LLM output sanitization
│       ├── models.rs        # Core data models
│       ├── traits.rs        # Provider traits
│       └── providers/
│           ├── openai.rs    # OpenAI-compatible LLM client
│           └── embeddings.rs # Embedding provider
├── config.toml          # Default configuration
├── tools.toml           # Tool definitions
├── test_cli.sh          # Integration test suite
└── Cargo.toml           # Workspace root
```

---

## License

MIT
