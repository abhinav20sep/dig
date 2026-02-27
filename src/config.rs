use agent_core::models::{AppConfig, ToolManifestEntry};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// Search order for config files:
/// 1. Current working directory
/// 2. ~/.config/dig/
/// 3. Directory containing the executable
fn find_config_file(filename: &str) -> Option<PathBuf> {
    // 1. CWD
    let cwd = Path::new(filename).to_path_buf();
    if cwd.exists() {
        return Some(cwd);
    }

    // 2. ~/.config/dig/
    if let Some(home) = dirs::home_dir() {
        let xdg = home.join(".config").join("dig").join(filename);
        if xdg.exists() {
            return Some(xdg);
        }
    }

    // 3. Next to the executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let beside_exe = exe_dir.join(filename);
            if beside_exe.exists() {
                return Some(beside_exe);
            }

            // 4. Cargo project root (useful for running ./target/debug/dig)
            if exe_dir.ends_with("target/debug") || exe_dir.ends_with("target/release") {
                if let Some(project_root) = exe_dir.parent().and_then(|p| p.parent()) {
                    let project_config = project_root.join(filename);
                    if project_config.exists() {
                        return Some(project_config);
                    }
                }
            }
        }
    }

    None
}

/// Load the application configuration.
pub fn load_config() -> Result<AppConfig> {
    match find_config_file("config.toml") {
        Some(path) => {
            tracing::info!("Loading config from {}", path.display());
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let config: AppConfig = toml::from_str(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))?;
            Ok(config)
        }
        None => {
            tracing::warn!("config.toml not found — using default configuration");
            Ok(AppConfig::default())
        }
    }
}

/// Load the tool manifest.
pub fn load_tools() -> Result<Vec<ToolManifestEntry>> {
    match find_config_file("tools.toml") {
        Some(path) => {
            tracing::info!("Loading tools from {}", path.display());
            let contents = std::fs::read_to_string(&path)
                .with_context(|| format!("Failed to read {}", path.display()))?;

            #[derive(serde::Deserialize)]
            struct ToolsFile {
                #[serde(default)]
                tools: Vec<ToolManifestEntry>,
            }

            let file: ToolsFile = toml::from_str(&contents)
                .with_context(|| format!("Failed to parse {}", path.display()))?;
            Ok(file.tools)
        }
        None => {
            tracing::warn!("tools.toml not found — no tools loaded");
            Ok(vec![])
        }
    }
}
