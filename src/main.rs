mod config;

use anyhow::Result;
use async_trait::async_trait;
use clap::Parser;
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use serde::Serialize;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use agent_core::agent_loop::{AgentLoop, AgentLoopResult, DryRunCommand, ToolPermitter};
use agent_core::governor::{BudgetEnforcer, TokenCounter, TokenGovernor};
use agent_core::memory::HybridMemory;
use agent_core::plugin::PluginManager;
use agent_core::protocol::*;
use agent_core::providers::embeddings::OpenAiEmbeddings;
use agent_core::providers::openai::OpenAiProvider;
use agent_core::sandbox::{ProcessSandbox, SecurityTier, classify_command};
use chrono::Utc;
use uuid::Uuid;

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ── CLI Argument Definition ─────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "dig",
    version,
    about = "Natural language agentic terminal executor",
    long_about = "Ask questions in plain English, get bash commands executed for you.\n\n\
                   Examples:\n  \
                   dig what is my ip\n  \
                   dig find all rust files larger than 10KB\n  \
                   cat file.txt | dig summarize this\n  \
                   dig  (interactive REPL mode)",
    trailing_var_arg = true
)]
struct Cli {
    /// Enable debug traces (full LLM request/response dumps to stderr).
    /// Equivalent to setting DIG_DEBUG=1.
    #[arg(short, long)]
    debug: bool,

    /// Show generated bash commands without executing them.
    #[arg(long)]
    dry_run: bool,

    /// Override LLM model for this invocation (e.g. "openai/gpt-4o").
    #[arg(short, long, value_name = "MODEL")]
    model: Option<String>,

    /// Auto-approve all security tier prompts (Safe, Confirm, Sandbox).
    /// WARNING: Dangerous — commands execute without confirmation.
    #[arg(short, long)]
    yes: bool,

    /// Output results as structured JSON (one-shot mode only).
    #[arg(long)]
    json: bool,

    /// Path to config file (overrides default search order).
    #[arg(short, long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Restrict to read-only commands.
    #[arg(long)]
    read_only: bool,

    /// Only allow Safe-tier commands. Deny Confirm and Sandbox tiers.
    #[arg(long)]
    safe_only: bool,

    /// Skip Jaccard command cache — force a fresh LLM call.
    #[arg(long)]
    no_cache: bool,

    /// Disable LanceDB semantic history for this session.
    #[arg(long)]
    no_memory: bool,

    /// Plugin management: list, enable <name>, disable <name>, install <path>.
    #[arg(long, value_name = "ACTION", num_args = 1..=2)]
    plugin: Vec<String>,

    /// Natural language query. Everything not a recognized flag becomes
    /// part of the query. Use `--` to pass flag-like words as query text.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    query: Vec<String>,
}

// ── JSON Output Types ───────────────────────────────────────────

#[derive(Serialize)]
struct JsonSuccess {
    success: bool,
    result: String,
}

#[derive(Serialize)]
struct JsonError {
    success: bool,
    error: String,
}

#[derive(Serialize)]
struct JsonDryRun {
    success: bool,
    dry_run: bool,
    commands: Vec<JsonDryRunCommand>,
}

#[derive(Serialize)]
struct JsonDryRunCommand {
    command: String,
    purpose: String,
    run_as: String,
    destructive: bool,
}

// ── Permitter ───────────────────────────────────────────────────

struct CliPermitter {
    auto_approve: bool,
    safe_only: bool,
}

#[async_trait]
impl ToolPermitter for CliPermitter {
    async fn check_permission(&self, command: &str, args: &[String]) -> Result<bool> {
        // --safe-only takes precedence: deny anything not Safe
        if self.safe_only {
            let tier = classify_command(command);
            if tier != SecurityTier::Safe {
                eprintln!(
                    "  ✗ Denied: '{}' is {:?} tier (--safe-only active)",
                    command, tier
                );
                return Ok(false);
            }
            return Ok(true);
        }

        // --yes: auto-approve everything
        if self.auto_approve {
            return Ok(true);
        }

        // Default interactive behavior
        let full_cmd = format!("{} {}", command, args.join(" "));
        let tier = classify_command(command);

        let proceed = tokio::task::spawn_blocking(move || -> Result<bool> {
            match tier {
                SecurityTier::Safe => Ok(true),
                SecurityTier::Confirm => {
                    let proceed = Confirm::with_theme(&ColorfulTheme::default())
                        .with_prompt(format!("⚠️  Run `{}`?", full_cmd))
                        .default(true)
                        .interact()?;
                    Ok(proceed)
                }
                SecurityTier::Sandbox => {
                    let choices = &["Allow once", "Allow and remember", "Abort"];
                    let selection = Select::with_theme(&ColorfulTheme::default())
                        .with_prompt(format!("🛡️  Destructive: `{}`. Authorize?", full_cmd))
                        .items(choices)
                        .default(2)
                        .interact()?;
                    Ok(selection != 2)
                }
            }
        })
        .await??;

        if !proceed {
            eprintln!("  ✗ Aborted by user.");
        }
        Ok(proceed)
    }

}

// ── Model Validation ────────────────────────────────────────────

fn validate_model_format(model: &str) -> bool {
    !model.is_empty()
        && !model.contains(' ')
        && model
            .chars()
            .all(|c| c.is_alphanumeric() || "/-_.:".contains(c))
}

fn is_model_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    (msg.contains("model") && (msg.contains("not found") || msg.contains("invalid")))
        || msg.contains("does not exist")
        || msg.contains("unsupported model")
}

// ── Pipeline Builder ────────────────────────────────────────────

struct Pipeline {
    agent_loop: AgentLoop,
    governor: TokenGovernor,
    tools: Vec<agent_core::models::ToolManifestEntry>,
    token_budget: u64,
}

async fn build_pipeline(
    app_config: &agent_core::models::AppConfig,
    model_override: Option<&str>,
    permitter: Arc<dyn ToolPermitter>,
    dry_run: bool,
    no_cache: bool,
    no_memory: bool,
    plugin_manager: Option<PluginManager>,
) -> Result<Pipeline> {
    let mut openai_cfg = app_config
        .providers
        .iter()
        .find(|p| p.name == "openai")
        .cloned()
        .unwrap_or_else(|| agent_core::models::ProviderConfig {
            name: "openai".into(),
            api_base: "https://api.openai.com/v1".into(),
            api_key_env: Some("OPENAI_API_KEY".into()),
            model: app_config.default_model.clone(),
        });

    if let Some(m) = model_override {
        openai_cfg.model = m.to_string();
    }

    let api_key = std::env::var(
        openai_cfg
            .api_key_env
            .as_deref()
            .unwrap_or("OPENAI_API_KEY"),
    )
    .unwrap_or_else(|_| "dummy_key".into());

    let embedder = Arc::new(OpenAiEmbeddings::new(
        api_key.clone(),
        openai_cfg.api_base.clone(),
        "text-embedding-3-small".into(),
    )?);

    let provider: Arc<dyn agent_core::traits::LlmProvider> = Arc::new(OpenAiProvider::new(
        openai_cfg.name,
        api_key,
        openai_cfg.api_base,
        openai_cfg.model,
    )?);

    let token_counter = Arc::new(TokenCounter::new("cl100k_base"));
    let governor = TokenGovernor::new(app_config.tokens_per_minute, app_config.token_budget)?;
    let enforcer = BudgetEnforcer::new(app_config.max_ttl, app_config.max_turns_per_agent);

    let process_sandbox = Arc::new(ProcessSandbox::new(
        app_config.max_concurrent_processes,
        app_config.global_timeout_secs,
    ));

    let memory = Arc::new(
        HybridMemory::new(
            "./lance_history",
            token_counter.clone(),
            8000,
            embedder,
            provider.clone(),
        )
        .await?,
    );

    let config_dir = match &std::env::var("DIG_CONFIG_DIR") {
        Ok(d) => Some(std::path::Path::new(d.as_str()).to_path_buf()),
        Err(_) => None,
    };
    let tools = config::load_tools(config_dir.as_deref())?;

    let mut agent_loop = AgentLoop::new(
        provider,
        memory,
        process_sandbox,
        governor.clone(),
        enforcer,
        permitter,
        tools.clone(),
        plugin_manager,
    );
    agent_loop.dry_run = dry_run;
    agent_loop.no_cache = no_cache;
    agent_loop.no_memory = no_memory;

    Ok(Pipeline {
        agent_loop,
        governor,
        tools,
        token_budget: app_config.token_budget,
    })
}

// ── Envelope Builder ────────────────────────────────────────────

fn build_envelope(
    query: &str,
    pipe_position: &str,
    interactive: bool,
    tty_available: bool,
    read_only: bool,
) -> MessageEnvelope<ExecutorPayload> {
    MessageEnvelope {
        proto_version: "1.1".to_string(),
        msg_id: Uuid::new_v4().to_string(),
        session_id: Uuid::new_v4().to_string(),
        parent_id: None,
        timestamp: Utc::now(),
        direction: Direction::ExecToBrain,
        priority: Priority::Normal,
        tags: vec![],
        payload: ExecutorPayload::Query(agent_core::protocol::QueryPayload {
            intent: Intent::Freeform,
            context: agent_core::protocol::harvest_context(
                pipe_position.to_string(),
                interactive,
                tty_available,
            ),
            query: query.to_string(),
            attachments: vec![],
            constraints: Constraints {
                read_only,
                no_reboot: true,
                no_service_restart: vec![],
                max_downtime: "0s".into(),
                approved_scope: vec![],
                forbidden: vec![],
            },
            history_refs: vec![],
        }),
    }
}

// ── Dry-Run Formatting ──────────────────────────────────────────

fn format_dry_run(commands: &[DryRunCommand]) -> String {
    let mut out = String::from("[dry-run] Would execute:\n");
    for (i, cmd) in commands.iter().enumerate() {
        out.push_str(&format!(
            "  {}. {}",
            i + 1,
            cmd.command
        ));
        if !cmd.purpose.is_empty() {
            out.push_str(&format!("    ({})", cmd.purpose));
        }
        if cmd.destructive {
            out.push_str("  [DESTRUCTIVE]");
        }
        if cmd.run_as != "user" {
            out.push_str(&format!("  [run_as: {}]", cmd.run_as));
        }
        out.push('\n');
    }
    out
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // --debug: set env var so agent_loop.rs and memory.rs pick it up
    if cli.debug {
        unsafe { std::env::set_var("DIG_DEBUG", "1"); }
    }

    // ── Handle --plugin actions (early exit, before any I/O) ──
    if !cli.plugin.is_empty() {
        let plugin_manager = config::plugins_dir().and_then(|dir| PluginManager::load(&dir));
        let action = cli.plugin[0].as_str();
        match action {
            "list" => {
                match plugin_manager {
                    Some(pm) => pm.print_list(),
                    None => println!("No plugins directory found. Create ~/.config/dig/plugins/ with a plugins.toml to get started."),
                }
                return Ok(());
            }
            "enable" => {
                let name = cli.plugin.get(1).map(|s| s.as_str()).unwrap_or_else(|| {
                    eprintln!("Usage: dig --plugin enable <name>");
                    std::process::exit(1);
                });
                match plugin_manager {
                    Some(mut pm) => {
                        pm.enable(name)?;
                        println!("Plugin '{}' enabled.", name);
                    }
                    None => eprintln!("No plugins directory found."),
                }
                return Ok(());
            }
            "disable" => {
                let name = cli.plugin.get(1).map(|s| s.as_str()).unwrap_or_else(|| {
                    eprintln!("Usage: dig --plugin disable <name>");
                    std::process::exit(1);
                });
                match plugin_manager {
                    Some(mut pm) => {
                        pm.disable(name)?;
                        println!("Plugin '{}' disabled.", name);
                    }
                    None => eprintln!("No plugins directory found."),
                }
                return Ok(());
            }
            "install" => {
                let script_path = cli.plugin.get(1).map(|s| s.as_str()).unwrap_or_else(|| {
                    eprintln!("Usage: dig --plugin install <script-path>");
                    std::process::exit(1);
                });
                let plugins_dir = config::plugins_dir().unwrap_or_else(|| {
                    // Create the directory if it doesn't exist
                    let dir = dirs::home_dir()
                        .expect("Cannot determine home directory")
                        .join(".config")
                        .join("dig")
                        .join("plugins");
                    std::fs::create_dir_all(&dir).expect("Failed to create plugins directory");
                    dir
                });
                let mut pm = PluginManager::load(&plugins_dir).unwrap_or_else(|| {
                    // Create empty manifest
                    let manifest_path = plugins_dir.join("plugins.toml");
                    std::fs::write(&manifest_path, "").expect("Failed to create plugins.toml");
                    PluginManager::load(&plugins_dir).expect("Failed to initialize plugin manager")
                });
                pm.install(std::path::Path::new(script_path))?;
                return Ok(());
            }
            other => {
                eprintln!("Unknown plugin action: '{}'. Use: list, enable, disable, install", other);
                std::process::exit(1);
            }
        }
    }

    // ── Determine pipe / TTY state ──
    let stdout_is_pipe = !std::io::stdout().is_terminal();
    let stdin_is_pipe = !std::io::stdin().is_terminal();
    let has_tty = std::fs::File::open("/dev/tty").is_ok();

    let pipe_position = match (stdout_is_pipe, stdin_is_pipe) {
        (false, false) => "none",
        (true, false) => "head",
        (false, true) => "tail",
        (true, true) => "middle",
    }
    .to_string();

    let interactive = has_tty;
    let tty_available = has_tty;

    // ── Read piped stdin ──
    let piped_input = if stdin_is_pipe {
        use std::io::Read;
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).ok();
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        None
    };

    // ── Build query from positional args + piped input ──
    let oneshot_query = if cli.query.is_empty() && piped_input.is_none() {
        None
    } else {
        let query_part = cli.query.join(" ");
        let full_query = match (&piped_input, query_part.is_empty()) {
            (Some(piped), false) => {
                format!("Given this input:\n```\n{}\n```\n\n{}", piped, query_part)
            }
            (Some(piped), true) => format!("Process this input:\n```\n{}\n```", piped),
            (None, false) => query_part,
            (None, true) => return Ok(()),
        };
        Some(full_query)
    };

    // ── --json warning for REPL mode ──
    if cli.json && oneshot_query.is_none() {
        eprintln!("Warning: --json is only supported in one-shot mode. Ignoring.");
    }

    // ── Initialize structured logging ──
    if oneshot_query.is_none() {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_level(false)
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_level(false)
            .with_max_level(tracing::Level::WARN)
            .init();
    }

    info!("Starting Dig v{}", VERSION);

    // ── Load configuration ──
    let app_config = config::load_config(cli.config.as_deref())?;

    // ── Load plugins ──
    let plugin_manager = config::plugins_dir().and_then(|dir| PluginManager::load(&dir));

    // ── Validate --model if provided ──
    let model_override = cli.model.as_deref();
    let used_model_override = model_override.is_some();
    let config_default_model = app_config.default_model.clone();

    let effective_model_override = match model_override {
        Some(m) if !validate_model_format(m) => {
            eprintln!(
                "Warning: Invalid model format '{}'. Falling back to: {}",
                m, config_default_model
            );
            None
        }
        other => other,
    };

    // ── Build permitter ──
    let permitter: Arc<dyn ToolPermitter> = Arc::new(CliPermitter {
        auto_approve: cli.yes,
        safe_only: cli.safe_only,
    });

    // ── Build pipeline ──
    let _tools_dir = cli.config.as_ref().and_then(|p| p.parent().map(|d| d.to_path_buf()));

    let mut pipeline = build_pipeline(
        &app_config,
        effective_model_override,
        permitter.clone(),
        cli.dry_run,
        cli.no_cache,
        cli.no_memory,
        plugin_manager,
    )
    .await?;

    pipeline.agent_loop.set_io_context(pipe_position.clone(), interactive, tty_available);

    // ── Register SIGINT handler ──
    let cancel_token = CancellationToken::new();
    let cancel_clone = cancel_token.clone();
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            cancel_clone.cancel();
        }
    });

    // ══════════════════════════════════════════════════════════
    //  ONE-SHOT MODE: `dig <query>`
    // ══════════════════════════════════════════════════════════
    if let Some(query) = oneshot_query {
        let dig_debug = std::env::var("DIG_DEBUG").is_ok();

        let envelope = build_envelope(&query, &pipe_position, interactive, tty_available, cli.read_only);

        if dig_debug {
            eprintln!("[DIG_DEBUG] One-shot query: {}", query);
        }

        let mut result = pipeline.agent_loop.run_turn(&envelope, &cancel_token).await;

        // Model fallback: if --model was used and API rejected it, rebuild with config default
        if let Err(ref e) = result {
            if used_model_override && is_model_error(e) {
                eprintln!(
                    "Warning: Model '{}' rejected by API. Falling back to '{}'.",
                    cli.model.as_deref().unwrap_or("?"),
                    config_default_model
                );
                pipeline = build_pipeline(
                    &app_config,
                    None,
                    permitter.clone(),
                    cli.dry_run,
                    cli.no_cache,
                    cli.no_memory,
                    None, // plugins already consumed by first pipeline
                )
                .await?;
                pipeline.agent_loop.set_io_context(pipe_position.clone(), interactive, tty_available);
                let retry_envelope = build_envelope(&query, &pipe_position, interactive, tty_available, cli.read_only);
                result = pipeline.agent_loop.run_turn(&retry_envelope, &cancel_token).await;
            }
        }

        let mut clarification_turns = 0;

        loop {
            match result {
                Ok(AgentLoopResult::Completed(summary)) => {
                    if cli.json {
                        let out = JsonSuccess { success: true, result: summary };
                        println!("{}", serde_json::to_string(&out)?);
                    } else {
                        println!("{}", summary);
                    }
                    break;
                }
                Ok(AgentLoopResult::DryRun(commands)) => {
                    if cli.json {
                        let out = JsonDryRun {
                            success: true,
                            dry_run: true,
                            commands: commands.iter().map(|c| JsonDryRunCommand {
                                command: c.command.clone(),
                                purpose: c.purpose.clone(),
                                run_as: c.run_as.clone(),
                                destructive: c.destructive,
                            }).collect(),
                        };
                        println!("{}", serde_json::to_string(&out)?);
                    } else {
                        print!("{}", format_dry_run(&commands));
                    }
                    break;
                }
                Ok(AgentLoopResult::NeedsHuman(clarification)) => {
                    clarification_turns += 1;
                    if clarification_turns >= 5 {
                        if cli.json {
                            let out = JsonError {
                                success: false,
                                error: "Exceeded maximum clarification turns (5)".into(),
                            };
                            println!("{}", serde_json::to_string(&out)?);
                        } else {
                            eprintln!("Error: Exceeded maximum clarification turns (5).");
                        }
                        std::process::exit(1);
                    }

                    if dig_debug {
                        eprintln!(
                            "[DIG_DEBUG] NeedsHuman: {:?}",
                            clarification
                                .needs
                                .iter()
                                .map(|n| &n.what)
                                .collect::<Vec<_>>()
                        );
                    }

                    let mut question = String::new();
                    if let Some(pa) = &clarification.partial_analysis {
                        question.push_str(&format!("Analysis: {}\n", pa));
                    }

                    if !interactive {
                        let mut all_defaults = Vec::new();
                        let mut missing_default = false;
                        for n in &clarification.needs {
                            if let Some(def) = &n.default {
                                all_defaults.push(def.clone());
                            } else {
                                missing_default = true;
                                eprintln!(
                                    "Error: Cannot resolve '{}' without TTY and no default provided.",
                                    n.what
                                );
                            }
                        }
                        if missing_default {
                            std::process::exit(1);
                        }
                        let answer = all_defaults.join(" ");
                        eprintln!("[dig] auto-answering: {} (no tty)", answer);
                        result = pipeline
                            .agent_loop
                            .submit_clarification_answer(&answer, &cancel_token)
                            .await;
                        continue;
                    }

                    use dialoguer::console::Term;
                    for n in &clarification.needs {
                        let default_suffix = if let Some(dl) = &n.default_label {
                            format!(" [{}]", dl)
                        } else if let Some(d) = &n.default {
                            format!(" [{}]", d)
                        } else {
                            String::new()
                        };
                        let p = n.prompt.as_deref().unwrap_or(&n.what);
                        question.push_str(&format!("\n- {}{}", p, default_suffix));
                    }

                    let answer: String = match tokio::task::spawn_blocking({
                        let q = question.clone();
                        move || {
                            Input::<String>::with_theme(&ColorfulTheme::default())
                                .with_prompt(format!("🤖 {}", q))
                                .interact_on(&Term::stderr())
                        }
                    })
                    .await?
                    {
                        Ok(ans) => {
                            if ans.trim().is_empty() {
                                let defs: Vec<_> = clarification
                                    .needs
                                    .iter()
                                    .filter_map(|n| n.default.clone())
                                    .collect();
                                defs.join(" ")
                            } else {
                                ans
                            }
                        }
                        Err(_) => {
                            eprintln!("\n🤖 Agent needs clarification:\n{}", question);
                            break;
                        }
                    };

                    let trimmed_ans = answer.trim();
                    if trimmed_ans == "exit" || trimmed_ans == "quit" {
                        break;
                    }

                    result = pipeline
                        .agent_loop
                        .submit_clarification_answer(trimmed_ans, &cancel_token)
                        .await;
                }
                Err(e) => {
                    if cli.json {
                        let out = JsonError {
                            success: false,
                            error: e.to_string(),
                        };
                        println!("{}", serde_json::to_string(&out)?);
                    } else {
                        println!("Error: {}", e);
                    }
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // ══════════════════════════════════════════════════════════
    //  REPL MODE: `dig` (no args)
    // ══════════════════════════════════════════════════════════
    println!(
        "\n🤖 dig v{} — Type 'exit' to quit, 'help' for commands\n",
        VERSION
    );

    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        let input: String = match tokio::task::spawn_blocking(|| {
            Input::<String>::with_theme(&ColorfulTheme::default())
                .with_prompt("dig ❯")
                .interact_text()
        })
        .await?
        {
            Ok(s) => s,
            Err(_) => break,
        };

        let trimmed = input.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        match trimmed.as_str() {
            "exit" | "quit" => break,
            "help" => {
                print_help();
                continue;
            }
            "tools" => {
                println!("\n📦 Available tools:");
                for tool in &pipeline.tools {
                    println!("  • {} [{}] — {}", tool.name, tool.tier, tool.description);
                }
                println!();
                continue;
            }
            "budget" => {
                println!(
                    "  Token budget: {} used / {} remaining / {} total",
                    pipeline.governor.total_used(),
                    pipeline.governor.remaining(),
                    pipeline.token_budget
                );
                continue;
            }
            _ => {}
        }

        let dig_debug = std::env::var("DIG_DEBUG").is_ok();

        let envelope = build_envelope(&trimmed, &pipe_position, interactive, tty_available, cli.read_only);

        if dig_debug {
            eprintln!("[DIG_DEBUG] REPL query: {}", trimmed);
        }

        let mut result = pipeline.agent_loop.run_turn(&envelope, &cancel_token).await;

        loop {
            match result {
                Ok(AgentLoopResult::Completed(summary)) => {
                    println!("\n🤖 {}", summary);
                    break;
                }
                Ok(AgentLoopResult::DryRun(commands)) => {
                    print!("\n{}", format_dry_run(&commands));
                    break;
                }
                Ok(AgentLoopResult::NeedsHuman(clarification)) => {
                    if dig_debug {
                        eprintln!(
                            "[DIG_DEBUG] NeedsHuman: {:?}",
                            clarification
                                .needs
                                .iter()
                                .map(|n| &n.what)
                                .collect::<Vec<_>>()
                        );
                    }

                    let mut question = String::new();
                    if let Some(pa) = &clarification.partial_analysis {
                        question.push_str(&format!("Analysis: {}\n", pa));
                    }

                    if !interactive {
                        let mut all_defaults = Vec::new();
                        let mut missing_default = false;
                        for n in &clarification.needs {
                            if let Some(def) = &n.default {
                                all_defaults.push(def.clone());
                            } else {
                                missing_default = true;
                                eprintln!("Error: Cannot resolve '{}' without TTY.", n.what);
                            }
                        }
                        if missing_default {
                            break;
                        }
                        let answer = all_defaults.join(" ");
                        eprintln!("[dig] auto-answering: {} (no tty)", answer);
                        result = pipeline
                            .agent_loop
                            .submit_clarification_answer(&answer, &cancel_token)
                            .await;
                        continue;
                    }

                    use dialoguer::console::Term;
                    for n in &clarification.needs {
                        let default_suffix = if let Some(dl) = &n.default_label {
                            format!(" [{}]", dl)
                        } else if let Some(d) = &n.default {
                            format!(" [{}]", d)
                        } else {
                            String::new()
                        };
                        let p = n.prompt.as_deref().unwrap_or(&n.what);
                        question.push_str(&format!("\n- {}{}", p, default_suffix));
                    }

                    let answer: String = match tokio::task::spawn_blocking({
                        let q = question.clone();
                        move || {
                            Input::<String>::with_theme(&ColorfulTheme::default())
                                .with_prompt(format!("🤖 {}", q))
                                .interact_on(&Term::stderr())
                        }
                    })
                    .await?
                    {
                        Ok(ans) => {
                            if ans.trim().is_empty() {
                                let defs: Vec<_> = clarification
                                    .needs
                                    .iter()
                                    .filter_map(|n| n.default.clone())
                                    .collect();
                                defs.join(" ")
                            } else {
                                ans
                            }
                        }
                        Err(_) => break,
                    };

                    let trimmed_ans = answer.trim();
                    if trimmed_ans == "exit" || trimmed_ans == "quit" {
                        break;
                    }

                    result = pipeline
                        .agent_loop
                        .submit_clarification_answer(trimmed_ans, &cancel_token)
                        .await;
                }
                Err(e) => {
                    println!("  ✗ Error: {}", e);
                    pipeline.agent_loop.reset();
                    break;
                }
            }
        }
    }

    println!("\n👋 Goodbye.");
    Ok(())
}

fn print_help() {
    println!(
        r#"
  Usage:
    dig [OPTIONS] <query>  — One-shot: ask a question, get the answer, exit
    dig [OPTIONS]          — Interactive REPL mode

  Options:
    -d, --debug            Enable debug traces (LLM request/response)
        --dry-run          Show generated commands without executing
    -m, --model <MODEL>    Override LLM model for this invocation
    -y, --yes              Auto-approve all security prompts (DANGEROUS)
        --json             Output as structured JSON (one-shot only)
    -c, --config <PATH>    Path to config file
        --read-only        Restrict to read-only commands
        --safe-only        Only allow Safe-tier commands
        --no-cache         Skip command cache, force LLM call
        --no-memory        Disable semantic history
        --plugin <ACTION>  Plugin management (list, enable, disable, install)
    -h, --help             Print help
    -V, --version          Print version

  REPL Commands:
    exit, quit       — Shut down
    help             — Show this help
    tools            — List available tools
    budget           — Show token usage
    <any>            — Natural language query
"#
    );
}
