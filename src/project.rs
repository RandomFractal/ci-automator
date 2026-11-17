use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    config::{self, Effective, Overrides},
    deploy::{self, Cia},
    git::{self, Repo},
};

pub const FILE_NAME: &str = "CIA";
const SNAPSHOT: &str = "deployed.CIA";

/// A loaded CIA file and the clone it deploys. State and logs live next to the file.
pub struct Project {
    pub file: PathBuf,
    pub dir: PathBuf,
    pub text: String,
    pub cia: Cia,
    pub repo: Repo,
    pub overrides: Overrides,
}

impl Project {
    pub fn load(file: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let cia = parse(file, &text)?;
        match cia.branch.as_deref() {
            None => bail!(
                "{} needs a BRANCH line with the branch to deploy",
                file.display()
            ),
            Some(b) if !git::is_valid_branch_name(b)? => {
                bail!("BRANCH {b} is not a valid branch name")
            }
            Some(_) => {}
        }
        let dir = dir_of(file);
        let repo = open_local(&dir, cia.local.as_deref())?;
        Ok(Self {
            file: file.to_path_buf(),
            dir,
            text,
            cia,
            repo,
            overrides: Overrides::default(),
        })
    }

    pub fn with(self, overrides: Overrides) -> Self {
        Self { overrides, ..self }
    }

    pub fn config(&self) -> Result<Effective> {
        config::effective(&self.cia, &self.overrides)
    }

    pub fn branch(&self) -> &str {
        self.cia.branch.as_deref().unwrap_or_default()
    }

    /// Fails if REPOSITORY is set and origin points somewhere else.
    pub fn verify_repository(&self) -> Result<()> {
        let Some(expected) = &self.cia.repository else {
            return Ok(());
        };
        let origin = self.repo.origin_url()?;
        match &origin {
            Some(url) if same_repository(url, expected) => Ok(()),
            Some(url) => bail!(
                "{} pulls from {url}, but {FILE_NAME} says REPOSITORY {expected}",
                self.repo.root.display()
            ),
            None => bail!("{} has no origin remote", self.repo.root.display()),
        }
    }

    /// The CIA text that last deployed successfully, which a rollback re-runs.
    pub fn snapshot(&self) -> Result<Option<String>> {
        let path = self.dir.join(SNAPSHOT);
        match fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    pub fn save_snapshot(&self) -> Result<()> {
        let path = self.dir.join(SNAPSHOT);
        fs::write(&path, &self.text).with_context(|| format!("cannot write {}", path.display()))
    }
}

/// `arg` is the CIA file or a directory holding one; None means the current directory.
pub fn locate(arg: Option<&Path>) -> Result<PathBuf> {
    let cwd = env::current_dir().context("cannot read the current directory")?;
    let path = match arg {
        Some(p) => cwd.join(p),
        None => cwd.clone(),
    };
    let path = if path.is_dir() {
        path.join(FILE_NAME)
    } else {
        path
    };
    if path.is_file() {
        return Ok(path);
    }
    match arg {
        Some(_) => bail!("{} does not exist", path.display()),
        None => bail!(
            "no {FILE_NAME} in {}; pass its path: cia path/to/{FILE_NAME}",
            cwd.display()
        ),
    }
}

/// Compares GitHub-style URLs, ignoring the scheme, user, case, a trailing slash and .git:
/// https://github.com/o/r, git@github.com:o/r.git and ssh://git@github.com/o/r are the same.
pub fn same_repository(a: &str, b: &str) -> bool {
    fn key(url: &str) -> String {
        let url = url.trim().trim_end_matches('/');
        let url = url.strip_suffix(".git").unwrap_or(url);
        let rest = match url.split_once("://") {
            Some((_, rest)) => rest.to_string(),
            None => url.replacen(':', "/", 1),
        };
        let rest = rest.rsplit_once('@').map_or(rest.as_str(), |(_, r)| r);
        rest.to_lowercase()
    }
    key(a) == key(b)
}

pub fn dir_of(file: &Path) -> PathBuf {
    file.parent().map_or_else(PathBuf::new, Path::to_path_buf)
}

pub fn parse(file: &Path, text: &str) -> Result<Cia> {
    deploy::parse(text).map_err(|e| anyhow!("invalid {}: {e:#}", file.display()))
}

pub fn open_local(dir: &Path, local: Option<&str>) -> Result<Repo> {
    let local = local.with_context(|| {
        format!("{FILE_NAME} needs a LOCAL line with the path of the clone to deploy")
    })?;
    Repo::open(&resolve_local(dir, local)?)
}

/// A leading ~ is the home directory; a relative path is relative to the CIA file.
pub fn resolve_local(dir: &Path, local: &str) -> Result<PathBuf> {
    let home = || {
        env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .context("HOME is not set")
    };
    let path = match local.strip_prefix('~') {
        Some("") => home()?,
        Some(rest) if rest.starts_with('/') => home()?.join(&rest[1..]),
        _ => PathBuf::from(local),
    };
    Ok(dir.join(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_repository_urls() {
        let https = "https://github.com/RandomFractal/rto-track";
        for same in [
            "https://github.com/RandomFractal/rto-track.git",
            "https://github.com/randomfractal/rto-track/",
            "git@github.com:RandomFractal/rto-track.git",
            "ssh://git@github.com/RandomFractal/rto-track",
        ] {
            assert!(same_repository(https, same), "{same}");
        }
        assert!(!same_repository(
            https,
            "https://github.com/RandomFractal/other"
        ));
        assert!(!same_repository(
            https,
            "https://gitlab.com/RandomFractal/rto-track"
        ));
    }

    #[test]
    fn resolves_local_paths() {
        let dir = Path::new("/home/a/.cia/app");
        assert_eq!(
            resolve_local(dir, "/srv/app").unwrap(),
            Path::new("/srv/app")
        );
        assert_eq!(
            resolve_local(dir, "../clone").unwrap(),
            Path::new("/home/a/.cia/app/../clone")
        );
        let home = PathBuf::from(env::var_os("HOME").unwrap());
        assert_eq!(resolve_local(dir, "~").unwrap(), home);
        assert_eq!(
            resolve_local(dir, "~/src/app").unwrap(),
            home.join("src/app")
        );
        assert_eq!(
            resolve_local(dir, "~other").unwrap(),
            Path::new("/home/a/.cia/app/~other")
        );
    }
}
