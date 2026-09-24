use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Defaults {
    pub period: Option<String>,
    pub timeout: Option<String>,
    /// how often to re-check while the alert is active
    pub recheck: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub checks_dir: PathBuf,
    pub state_dir: Option<PathBuf>,
    /// dir with helper tools (parse_df, parse_curl, ...) prepended to PATH of checks
    pub libexec_dir: Option<PathBuf>,
    pub telegram: Option<TelegramConfig>,
    #[serde(default)]
    pub defaults: Defaults,
}

impl Config {
    pub fn load(path: Option<&PathBuf>) -> Result<Config> {
        let path = match path {
            Some(p) => p.clone(),
            None => {
                if let Ok(p) = std::env::var("INSOMNIA_CONFIG") {
                    PathBuf::from(p)
                } else {
                    dirs::config_dir()
                        .context("cannot resolve config dir")?
                        .join("insomnia/config.toml")
                }
            }
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("cannot parse config {}", path.display()))?;
        let cfg = Self::apply_env(cfg);
        Ok(cfg)
    }

    /// allow secrets via env: INSOMNIA_TG_TOKEN overrides/creates [telegram].bot_token
    fn apply_env(mut cfg: Config) -> Config {
        if let Ok(token) = std::env::var("INSOMNIA_TG_TOKEN") {
            if !token.is_empty() {
                let tg = cfg.telegram.get_or_insert_with(|| TelegramConfig {
                    bot_token: String::new(),
                    chat_id: String::new(),
                });
                tg.bot_token = token;
            }
        }
        cfg
    }

    pub fn state_dir(&self) -> PathBuf {
        self.state_dir.clone().unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"))
                .join("insomnia")
        })
    }

    pub fn expanded(&mut self) {
        self.checks_dir = expand_tilde(&self.checks_dir);
        if let Some(d) = self.state_dir.as_mut() {
            *d = expand_tilde(d);
        }
        if let Some(d) = self.libexec_dir.as_mut() {
            *d = expand_tilde(d);
        }
    }
}

fn expand_tilde(p: &PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    p.clone()
}
