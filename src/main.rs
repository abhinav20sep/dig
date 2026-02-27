mod config;

use anyhow::Result;
use async_trait::async_trait;
use dialoguer::{Confirm, Input, Select, theme::ColorfulTheme};
use std::io::IsTerminal;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use tracing::info;
use tracing_subscriber;

use agent_core::agent_loop::{AgentLoop, AgentLoopResult, ToolPermitter};
use agent_core::governor::{BudgetEnforcer, TokenCounter, TokenGovernor};
use agent_core::memory::HybridMemory;
use agent_core::protocol::*;
use agent_core::providers::embeddings::OpenAiEmbeddings;
use agent_core::providers::openai::OpenAiProvider;
use agent_core::sandbox::{ProcessSandbox, SecurityTier, classify_command};
use chrono::Utc;
use uuid::Uuid;

struct CliPermitter;

#[async_trait]
impl ToolPermitter for CliPermitter {
    async fn check_permission(&self, command: &str, args: &[String]) -> Result<bool> {
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

#[tokio::main]
async fn main() -> Result<()> {
    // ── Determine mode: one-shot (args) or REPL (no args) ──
    let cli_args: Vec<String> = std::env::args().skip(1).collect();

    use std::io::IsTerminal;

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

    // Check if stdin is a pipe (not a terminal)
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

    let oneshot_query = if cli_args.is_empty() && piped_input.is_none() {
        None
    } else {
        let query_part = cli_args.join(" ");
        let full_query = match (&piped_input, query_part.is_empty()) {
            (Some(piped), false) => {
                format!("Given this input:\n```\n{}\n```\n\n{}", piped, query_part)
            }
            (Some(piped), true) => format!("Process this input:\n```\n{}\n```", piped),
            (None, false) => query_part,
            (None, true) => return Ok(()), // shouldn't happen, but guard
        };
        Some(full_query)
    };

    // ── Initialize structured logging ──
    // In one-shot mode, suppress tracing to stderr so only the result shows
    if oneshot_query.is_none() {
        tracing_subscriber::fmt()
            .with_target(false)
            .with_level(false)
            .init();
    } else {
        // Quiet logging for one-shot
        tracing_subscriber::fmt()
            .with_target(false)
            .with_level(false)
            .with_max_level(tracing::Level::WARN)
            .init();
    }

    info!("Starting Dig v0.1.0");

    // ── Load configuration ──
    let app_config = config::load_config()?;

    // ── Load tool manifest ──
    let tools = config::load_tools()?;

    // ── Create infrastructure ──
    let cancel_token = CancellationToken::new();
    let process_sandbox = Arc::new(ProcessSandbox::new(
        app_config.max_concurrent_processes,
        app_config.global_timeout_secs,
    ));

    let token_counter = Arc::new(TokenCounter::new("cl100k_base"));
    let governor = TokenGovernor::new(app_config.tokens_per_minute, app_config.token_budget)?;
    let enforcer = BudgetEnforcer::new(app_config.max_ttl, app_config.max_turns_per_agent);

    // Initialize Provider config
    let openai_cfg = app_config
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
    let api_key = std::env::var(
        openai_cfg
            .api_key_env
            .as_deref()
            .unwrap_or("OPENAI_API_KEY"),
    )
    .unwrap_or_else(|_| "dummy_key".into());

    // Initialize Memory (needs embedder + summarizer)
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

    let permitter = Arc::new(CliPermitter);

    let mut agent_loop = AgentLoop::new(
        provider,
        memory,
        process_sandbox,
        governor.clone(),
        enforcer,
        permitter,
        tools.clone(),
    );
    agent_loop.set_io_context(pipe_position.clone(), interactive, tty_available);

    // ── Register SIGINT handler ──
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
        let session_id = Uuid::new_v4().to_string();
        let dig_debug = std::env::var("DIG_DEBUG").is_ok();

        let envelope = MessageEnvelope {
            proto_version: "1.1".to_string(),
            msg_id: Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            parent_id: None,
            timestamp: Utc::now(),
            direction: Direction::ExecToBrain,
            priority: Priority::Normal,
            tags: vec![],
            payload: ExecutorPayload::Query(agent_core::protocol::QueryPayload {
                intent: Intent::Freeform,
                context: agent_core::protocol::harvest_context(
                    pipe_position.clone(),
                    interactive,
                    tty_available,
                ),
                query: query.clone(),
                attachments: vec![],
                constraints: Constraints {
                    read_only: false,
                    no_reboot: true,
                    no_service_restart: vec![],
                    max_downtime: "0s".into(),
                    approved_scope: vec![],
                    forbidden: vec![],
                },
                history_refs: vec![],
            }),
        };

        if dig_debug {
            eprintln!("[DIG_DEBUG] One-shot query: {}", query);
        }

        let mut result = agent_loop.run_turn(&envelope, &cancel_token).await;
        let mut clarification_turns = 0;

        loop {
            match result {
                Ok(AgentLoopResult::Completed(summary)) => {
                    println!("{}", summary);
                    break;
                }
                Ok(AgentLoopResult::NeedsHuman(clarification)) => {
                    clarification_turns += 1;
                    if clarification_turns >= 5 {
                        eprintln!("Error: Exceeded maximum clarification turns (5).");
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
                        result = agent_loop
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

                    result = agent_loop
                        .submit_clarification_answer(trimmed_ans, &cancel_token)
                        .await;
                }
                Err(e) => {
                    println!("Error: {}", e);
                    std::process::exit(1);
                }
            }
        }
        return Ok(());
    }

    // ══════════════════════════════════════════════════════════
    //  REPL MODE: `dig` (no args)
    // ══════════════════════════════════════════════════════════
    println!("\n🤖 dig v0.1.0 — Type 'exit' to quit, 'help' for commands\n");

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
                for tool in &tools {
                    println!("  • {} [{}] — {}", tool.name, tool.tier, tool.description);
                }
                println!();
                continue;
            }
            "budget" => {
                println!(
                    "  Token budget: {} used / {} remaining / {} total",
                    governor.total_used(),
                    governor.remaining(),
                    app_config.token_budget
                );
                continue;
            }
            _ => {}
        }

        let session_id = Uuid::new_v4().to_string();
        let dig_debug = std::env::var("DIG_DEBUG").is_ok();

        let envelope = MessageEnvelope {
            proto_version: "1.1".to_string(),
            msg_id: Uuid::new_v4().to_string(),
            session_id: session_id.clone(),
            parent_id: None,
            timestamp: Utc::now(),
            direction: Direction::ExecToBrain,
            priority: Priority::Normal,
            tags: vec![],
            payload: ExecutorPayload::Query(agent_core::protocol::QueryPayload {
                intent: Intent::Freeform,
                context: agent_core::protocol::harvest_context(
                    pipe_position.clone(),
                    interactive,
                    tty_available,
                ),
                query: trimmed.clone(),
                attachments: vec![],
                constraints: Constraints {
                    read_only: false,
                    no_reboot: true,
                    no_service_restart: vec![],
                    max_downtime: "0s".into(),
                    approved_scope: vec![],
                    forbidden: vec![],
                },
                history_refs: vec![],
            }),
        };

        if dig_debug {
            eprintln!("[DIG_DEBUG] REPL query: {}", trimmed);
        }

        let mut result = agent_loop.run_turn(&envelope, &cancel_token).await;

        loop {
            match result {
                Ok(AgentLoopResult::Completed(summary)) => {
                    println!("\n🤖 {}", summary);
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
                        result = agent_loop
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

                    result = agent_loop
                        .submit_clarification_answer(trimmed_ans, &cancel_token)
                        .await;
                }
                Err(e) => {
                    println!("  ✗ Error: {}", e);
                    agent_loop.reset();
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
    dig <query>      — One-shot: ask a question, get the answer, exit
    dig              — Interactive REPL mode

  REPL Commands:
    exit, quit       — Shut down
    help             — Show this help
    tools            — List available tools
    budget           — Show token usage
    <any>            — Natural language query
"#
    );
}
