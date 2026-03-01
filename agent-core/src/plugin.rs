use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

// ── Plugin Configuration ──────────────────────────────────────────

/// A single plugin entry from plugins.toml
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PluginConfig {
    pub name: String,
    pub description: String,
    pub usage: String,
    pub script: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_enabled() -> bool {
    true
}

fn default_timeout_ms() -> u64 {
    2000
}

/// The top-level plugins.toml structure
#[derive(Debug, Clone, Deserialize, Serialize)]
struct PluginsManifest {
    #[serde(default, rename = "plugins")]
    entries: Vec<PluginConfig>,
}

// ── Plugin Input (JSON sent to script on stdin) ───────────────────

#[derive(Debug, Serialize)]
struct PluginInput {
    args: String,
    cwd: String,
    query: String,
}

// ── Plugin Manager ────────────────────────────────────────────────

pub struct PluginManager {
    plugins_dir: PathBuf,
    plugins: Vec<PluginConfig>,
}

impl PluginManager {
    /// Load plugins from the given directory.
    /// Reads `plugins.toml` from `plugins_dir` and filters to enabled plugins.
    /// Returns `None` if the directory or manifest doesn't exist.
    pub fn load(plugins_dir: &Path) -> Option<Self> {
        let manifest_path = plugins_dir.join("plugins.toml");
        if !manifest_path.exists() {
            info!("No plugins.toml found at {}", manifest_path.display());
            return None;
        }

        let contents = match std::fs::read_to_string(&manifest_path) {
            Ok(c) => c,
            Err(e) => {
                warn!("Failed to read plugins.toml: {}", e);
                return None;
            }
        };

        let manifest: PluginsManifest = match toml::from_str(&contents) {
            Ok(m) => m,
            Err(e) => {
                warn!("Failed to parse plugins.toml: {}", e);
                return None;
            }
        };

        info!(
            "Loaded {} plugin(s) from {}",
            manifest.entries.len(),
            plugins_dir.display()
        );

        Some(Self {
            plugins_dir: plugins_dir.to_path_buf(),
            plugins: manifest.entries,
        })
    }

    /// Returns only enabled plugins.
    pub fn enabled_plugins(&self) -> Vec<&PluginConfig> {
        self.plugins.iter().filter(|p| p.enabled).collect()
    }

    /// Returns all plugins (enabled and disabled).
    pub fn all_plugins(&self) -> &[PluginConfig] {
        &self.plugins
    }

    /// Build the capability description block to append to the LLM system prompt.
    /// Returns empty string if no plugins are enabled.
    pub fn capability_prompt(&self) -> String {
        let enabled: Vec<&PluginConfig> = self.enabled_plugins();
        if enabled.is_empty() {
            return String::new();
        }

        let mut out = String::from(
            "\n\n[AVAILABLE PLUGINS — MANDATORY]\n\
             You have access to these plugin capabilities via actions with command \"dig-plugin <name> <args>\".\n\
             Plugins are free (no sandbox cost), always permitted, and execute instantly.\n\n\
             RULE: When the user references a LOCAL FILE by DESCRIPTION rather than exact filename \
             (e.g. \"the test script\", \"the config file\", \"that log\"), you MUST invoke \
             the contextual-search-guess plugin as your FIRST action BEFORE attempting to run \
             any file. NEVER guess or assume a filename. The plugin is your search tool.\n\
             EXCEPTION: Software/package/product names are NOT file descriptions. Words like \
             \"claude code\", \"neovim\", \"node\", \"docker\", \"rust\" refer to installed software, \
             not local files. For software tasks (install/update/remove/check), use system commands \
             (which, apt, npm, pip, snap, cargo) — NOT this plugin.\n\n\
             Search modes:\n\
             - Keywords:  dig-plugin contextual-search-guess \"test script\"  (matches filenames)\n\
             - Glob:      dig-plugin contextual-search-guess \"glob:*.sh\"   (glob pattern)\n\
             - Find:      dig-plugin contextual-search-guess \"find:*.py:3\" (recursive, depth 3)\n\
             - List all:  dig-plugin contextual-search-guess \"\"            (list CWD)\n\n\
             WORKFLOW — two phases:\n\
             Phase 1 (File Discovery): Use the plugin to search. Review the results. Pick the ONE \
             best matching file. Propose ONLY that one file to the user for confirmation.\n\
             Phase 2 (Invocation Discovery): After identifying the file, figure out HOW to run it. \
             First try running it with --help. If that doesn't clarify usage, read the file's first \
             ~30 lines to understand its arguments. Then construct the correct command that maps the \
             user's intent to the script's actual flags/arguments.\n\n\
             Example — user says \"run the test script for test 3\":\n\
             Step 1: {\"command\": \"dig-plugin contextual-search-guess \\\"test script\\\"\", ...}\n\
             Step 2: Pick best match (e.g. test_cli.sh) → {\"command\": \"./test_cli.sh --help\", ...}\n\
             Step 3: See --help shows --test flag → {\"command\": \"./test_cli.sh --test 3\", ...}\n\n",
        );

        for p in &enabled {
            out.push_str(&format!(
                "- {}: {}\n  Usage: {}\n\n",
                p.name, p.description, p.usage
            ));
        }

        out
    }

    /// Execute a plugin by name with given arguments.
    /// Spawns the plugin script, sends JSON on stdin, captures stdout.
    /// Returns the plugin's stdout output.
    pub async fn execute(&self, name: &str, args: &str, cwd: &str, query: &str) -> Result<String> {
        let plugin = self
            .plugins
            .iter()
            .find(|p| p.name == name)
            .with_context(|| format!("Plugin '{}' not found", name))?;

        if !plugin.enabled {
            bail!("Plugin '{}' is disabled", name);
        }

        let script_path = self.plugins_dir.join(&plugin.script);
        if !script_path.exists() {
            bail!(
                "Plugin script not found: {}",
                script_path.display()
            );
        }

        let input = PluginInput {
            args: args.to_string(),
            cwd: cwd.to_string(),
            query: query.to_string(),
        };
        let input_json = serde_json::to_string(&input)?;

        info!(plugin = %name, args = %args, "Executing plugin");

        let mut child = tokio::process::Command::new("bash")
            .arg(&script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(cwd)
            .spawn()
            .with_context(|| format!("Failed to spawn plugin '{}'", name))?;

        // Write JSON input to stdin
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input_json.as_bytes()).await?;
            // Drop stdin to close it so the script can proceed
        }

        // Wait with timeout
        let timeout = std::time::Duration::from_millis(plugin.timeout_ms);
        let result = tokio::time::timeout(timeout, child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if !output.status.success() {
                    warn!(
                        plugin = %name,
                        exit_code = output.status.code().unwrap_or(-1),
                        stderr = %stderr,
                        "Plugin exited with error"
                    );
                }

                // Return stdout even on non-zero exit (LLM can see the error info)
                if stdout.is_empty() && !stderr.is_empty() {
                    Ok(format!("Plugin error: {}", stderr))
                } else {
                    Ok(stdout)
                }
            }
            Ok(Err(e)) => {
                bail!("Plugin '{}' execution failed: {}", name, e);
            }
            Err(_) => {
                // Timeout — try to kill the child
                warn!(plugin = %name, timeout_ms = plugin.timeout_ms, "Plugin timed out");
                bail!(
                    "Plugin '{}' timed out after {}ms",
                    name,
                    plugin.timeout_ms
                );
            }
        }
    }

    /// Parse a `dig-plugin <name> <args>` command string.
    /// Returns `(plugin_name, args_string)` or None if not a plugin command.
    pub fn parse_plugin_command(command: &str) -> Option<(String, String)> {
        let trimmed = command.trim();
        if !trimmed.starts_with("dig-plugin ") {
            return None;
        }

        let rest = trimmed["dig-plugin ".len()..].trim();
        if rest.is_empty() {
            return None;
        }

        // Split into name and args (args may be quoted)
        let (name, args) = if let Some(space_idx) = rest.find(' ') {
            let name = &rest[..space_idx];
            let args = rest[space_idx + 1..].trim();
            // Strip surrounding quotes if present
            let args = if (args.starts_with('"') && args.ends_with('"'))
                || (args.starts_with('\'') && args.ends_with('\''))
            {
                &args[1..args.len() - 1]
            } else {
                args
            };
            (name.to_string(), args.to_string())
        } else {
            (rest.to_string(), String::new())
        };

        Some((name, args))
    }

    // ── CLI Management Operations ─────────────────────────────────

    /// Enable a plugin by name. Modifies plugins.toml on disk.
    pub fn enable(&mut self, name: &str) -> Result<()> {
        self.set_enabled(name, true)
    }

    /// Disable a plugin by name. Modifies plugins.toml on disk.
    pub fn disable(&mut self, name: &str) -> Result<()> {
        self.set_enabled(name, false)
    }

    fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<()> {
        let plugin = self
            .plugins
            .iter_mut()
            .find(|p| p.name == name)
            .with_context(|| format!("Plugin '{}' not found", name))?;

        plugin.enabled = enabled;
        self.save_manifest()
    }

    /// Install a plugin script: copy it to the plugins directory and add a manifest entry.
    pub fn install(&mut self, script_path: &Path) -> Result<()> {
        if !script_path.exists() {
            bail!("Script not found: {}", script_path.display());
        }

        let filename = script_path
            .file_name()
            .with_context(|| "Invalid script path")?
            .to_string_lossy()
            .to_string();

        // Derive plugin name from filename (strip extension)
        let name = script_path
            .file_stem()
            .with_context(|| "Invalid script filename")?
            .to_string_lossy()
            .to_string();

        // Check for duplicate
        if self.plugins.iter().any(|p| p.name == name) {
            bail!("Plugin '{}' already exists", name);
        }

        // Copy script to plugins directory
        let dest = self.plugins_dir.join(&filename);
        std::fs::copy(script_path, &dest)
            .with_context(|| format!("Failed to copy script to {}", dest.display()))?;

        // Make executable
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o755);
            std::fs::set_permissions(&dest, perms)?;
        }

        // Add manifest entry
        self.plugins.push(PluginConfig {
            name: name.clone(),
            description: format!("Plugin: {}", name),
            usage: format!("dig-plugin {} \"<args>\"", name),
            script: filename,
            enabled: true,
            timeout_ms: default_timeout_ms(),
        });

        self.save_manifest()?;

        println!("Installed plugin '{}' — edit plugins.toml to customize description and usage.", name);
        Ok(())
    }

    /// Write current plugin state back to plugins.toml
    fn save_manifest(&self) -> Result<()> {
        let manifest = PluginsManifest {
            entries: self.plugins.clone(),
        };
        let content = toml::to_string_pretty(&manifest)
            .context("Failed to serialize plugins.toml")?;
        let manifest_path = self.plugins_dir.join("plugins.toml");
        std::fs::write(&manifest_path, content)
            .with_context(|| format!("Failed to write {}", manifest_path.display()))?;
        Ok(())
    }

    /// Print plugin list to stdout (for `dig --plugin list`)
    pub fn print_list(&self) {
        if self.plugins.is_empty() {
            println!("No plugins installed.");
            return;
        }

        println!("\nInstalled plugins:");
        for p in &self.plugins {
            let status = if p.enabled { "enabled" } else { "disabled" };
            println!("  {} [{}]", p.name, status);
            println!("    {}", p.description);
            println!("    Script: {}", p.script);
            println!("    Timeout: {}ms", p.timeout_ms);
            println!();
        }
    }
}
