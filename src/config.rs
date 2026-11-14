use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{deploy::Cia, interval::Interval};

pub const DEFAULT_INTERVAL: Interval = Interval::new(5, 'm');

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval: Option<Interval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<bool>,
}

/// Flags given for one run; they beat the CIA file, which beats the global config.
#[derive(Clone, Default)]
pub struct Overrides {
    pub interval: Option<Interval>,
    pub rollback: Option<bool>,
    /// An address, or "off".
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

pub fn effective(cia: &Cia, overrides: &Overrides) -> Result<Effective> {
    let global = Config::load(&global_path()?)?;
    Ok(Effective {
        interval: overrides
            .interval
            .or(cia.interval)
            .or(global.interval)
            .unwrap_or(DEFAULT_INTERVAL),
        rollback: overrides
            .rollback
            .or(cia.rollback)
            .or(global.rollback)
            .unwrap_or(true),
        webhook: match overrides.webhook.as_deref() {
            Some("off") => None,
            Some(addr) => Some(addr.to_string()),
            None => cia.webhook.clone(),
        },
    })
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
        };
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.interval, cfg.interval);
        assert_eq!(back.rollback, cfg.rollback);
    }

    #[test]
    fn empty_config_serializes_to_nothing() {
        assert_eq!(toml::to_string(&Config::default()).unwrap().trim(), "");
    }

    #[test]
    fn rejects_bad_interval_and_unknown_keys() {
        assert!(toml::from_str::<Config>("interval = \"5x\"").is_err());
        assert!(toml::from_str::<Config>("branches = [\"main\"]").is_err());
        assert!(toml::from_str::<Config>("webhook = \"127.0.0.1:9000\"").is_err());
    }

    #[test]
    fn flags_beat_the_cia_file() {
        let cia = Cia {
            interval: Some(Interval::new(1, 'h')),
            rollback: Some(false),
            webhook: Some("127.0.0.1:9000".into()),
            ..Cia::default()
        };
        let eff = effective(&cia, &Overrides::default()).unwrap();
        assert_eq!(eff.interval, Interval::new(1, 'h'));
        assert!(!eff.rollback);
        assert_eq!(eff.webhook.as_deref(), Some("127.0.0.1:9000"));
        let flags = Overrides {
            interval: Some(Interval::new(30, 's')),
            rollback: Some(true),
            webhook: Some("off".into()),
        };
        let eff = effective(&cia, &flags).unwrap();
        assert_eq!(eff.interval, Interval::new(30, 's'));
        assert!(eff.rollback);
        assert_eq!(eff.webhook, None);
    }
}
