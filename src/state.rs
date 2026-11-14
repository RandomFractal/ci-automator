use std::{
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
pub struct State {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_attempted: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_deployed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_deployed_at: Option<String>,
    /// A commit that failed and was rolled back; not retried until upstream moves or --force.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failed: Option<String>,
}

impl State {
    pub fn load(dir: &Path) -> Result<Self> {
        let path = path(dir);
        match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("invalid state file {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let path = path(dir);
        fs::write(&path, toml::to_string(self)?)
            .with_context(|| format!("cannot write {}", path.display()))
    }
}

fn path(dir: &Path) -> PathBuf {
    dir.join("state.toml")
}
