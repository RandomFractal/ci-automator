use std::{
    env, fs, io,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    deploy::{self, Cia},
    git::Repo,
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
}

impl Project {
    pub fn load(file: &Path) -> Result<Self> {
        let text =
            fs::read_to_string(file).with_context(|| format!("cannot read {}", file.display()))?;
        let cia = parse(file, &text)?;
        let dir = dir_of(file);
        let repo = open_local(&dir, cia.local.as_deref())?;
        Ok(Self {
            file: file.to_path_buf(),
            dir,
            text,
            cia,
            repo,
        })
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
