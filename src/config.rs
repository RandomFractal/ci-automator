use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{git::Repo, interval::Interval};

pub const DEFAULT_INTERVAL: Interval = Interval::new(5, 'm');

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<bool>,
    /// Address `--watch` listens on for GitHub push webhooks. Repo config only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub webhook: Option<String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        fs::write(path, toml::to_string(self)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

pub struct Effective {
    pub interval: Interval,
    pub rollback: bool,
    pub webhook: Option<String>,
}

pub fn effective(repo: &Repo) -> Result<Effective> {
    let global = Config::load(&global_path()?)?;
    let local = Config::load(&repo_path(repo))?;
    Ok(Effective {
        interval: local
            .interval
            .or(global.interval)
            .unwrap_or(DEFAULT_INTERVAL),
        rollback: local.rollback.or(global.rollback).unwrap_or(true),
        webhook: local.webhook,
    })
}

pub fn repo_path(repo: &Repo) -> PathBuf {
    repo.cia_dir().join("config.toml")
}

pub fn global_path() -> Result<PathBuf> {
    Ok(xdg_config_home()?.join("cia").join("config.toml"))
}

pub fn xdg_config_home() -> Result<PathBuf> {
    match env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(PathBuf::from(
            env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .context("HOME is not set")?,
        )
        .join(".config")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_toml() {
        let cfg = Config {
            interval: Some(Interval::new(30, 's')),
            rollback: Some(false),
            webhook: Some("127.0.0.1:9000".into()),
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.interval, cfg.interval);
        assert_eq!(back.rollback, cfg.rollback);
        assert_eq!(back.webhook, cfg.webhook);
    }

    #[test]
    fn empty_config_serializes_to_nothing() {
        assert_eq!(toml::to_string(&Config::default()).unwrap().trim(), "");
    }

    #[test]
    fn rejects_bad_interval_and_unknown_keys() {
        assert!(toml::from_str::<Config>("interval = \"5x\"").is_err());
        assert!(toml::from_str::<Config>("branches = [\"main\"]").is_err());
    }
}
