use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result};

use crate::git::Repo;

const ACTIVITY_MAX_BYTES: u64 = 256 * 1024;
const ACTIVITY_BACKUPS: u32 = 1;
const EVENTS_MAX_BYTES: u64 = 10 * 1024 * 1024;
const EVENTS_BACKUPS: u32 = 5;

/// `activity.log` is the full, short-lived timeline including no-op checks.
/// `events.log` keeps only deploys, errors and other significant events, for longer.
#[derive(Clone)]
pub struct Logger {
    pub activity: PathBuf,
    pub events: PathBuf,
}

impl Logger {
    pub fn new(repo: &Repo) -> Self {
        let dir = repo.cia_dir();
        Self {
            activity: dir.join("activity.log"),
            events: dir.join("events.log"),
        }
    }

    pub fn info(&self, msg: &str) {
        eprintln!("{msg}");
        self.activity("INFO", msg);
    }

    pub fn event(&self, msg: &str) {
        eprintln!("{msg}");
        self.events("EVENT", msg);
        self.activity("EVENT", msg);
    }

    pub fn error(&self, msg: &str) {
        eprintln!("error: {msg}");
        self.events("ERROR", msg);
        self.activity("ERROR", msg);
    }

    pub fn output(&self, source: &str, line: &str) {
        eprintln!("[{source}] {line}");
        self.events("OUT", &format!("[{source}] {line}"));
    }

    fn activity(&self, level: &str, msg: &str) {
        append(
            &self.activity,
            ACTIVITY_MAX_BYTES,
            ACTIVITY_BACKUPS,
            level,
            msg,
        );
    }

    fn events(&self, level: &str, msg: &str) {
        append(&self.events, EVENTS_MAX_BYTES, EVENTS_BACKUPS, level, msg);
    }
}

pub fn now() -> String {
    humantime::format_rfc3339_seconds(SystemTime::now()).to_string()
}

pub fn tail(path: &Path, lines: usize) -> Result<Option<Vec<String>>> {
    let text = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let all: Vec<&str> = text.lines().collect();
    Ok(Some(
        all[all.len().saturating_sub(lines)..]
            .iter()
            .map(|l| l.to_string())
            .collect(),
    ))
}

fn append(path: &Path, max_bytes: u64, backups: u32, level: &str, msg: &str) {
    let line = format!("{} {level:<5} {msg}\n", now());
    if let Err(e) = try_append(path, max_bytes, backups, &line) {
        eprintln!("cia: cannot write {}: {e}", path.display());
    }
}

fn try_append(path: &Path, max_bytes: u64, backups: u32, line: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let size = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    if size > 0 && size + line.len() as u64 > max_bytes {
        rotate(path, backups)?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

fn rotate(path: &Path, backups: u32) -> io::Result<()> {
    for i in (1..backups).rev() {
        let from = numbered(path, i);
        if from.exists() {
            fs::rename(from, numbered(path, i + 1))?;
        }
    }
    fs::rename(path, numbered(path, 1))
}

fn numbered(path: &Path, i: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{i}"));
    name.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_caps_backups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        for i in 0..20 {
            try_append(&path, 30, 2, &format!("line number {i:04}\n")).unwrap();
        }
        assert!(path.exists());
        assert!(numbered(&path, 1).exists());
        assert!(numbered(&path, 2).exists());
        assert!(!numbered(&path, 3).exists());
        assert!(fs::read_to_string(&path).unwrap().contains("0019"));
    }

    #[test]
    fn tail_returns_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.log");
        fs::write(&path, "a\nb\nc\n").unwrap();
        assert_eq!(tail(&path, 2).unwrap().unwrap(), vec!["b", "c"]);
        assert!(tail(&dir.path().join("missing"), 2).unwrap().is_none());
    }
}
