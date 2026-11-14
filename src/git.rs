use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{Context, Result, bail};

pub struct Repo {
    pub root: PathBuf,
    pub git_dir: PathBuf,
}

impl Repo {
    pub fn discover() -> Result<Self> {
        let cwd = env::current_dir().context("cannot read the current directory")?;
        let root = PathBuf::from(
            run(&cwd, &["rev-parse", "--show-toplevel"]).context("not inside a git repository")?,
        );
        let git_dir = PathBuf::from(run(&root, &["rev-parse", "--git-dir"])?);
        let git_dir = if git_dir.is_absolute() {
            git_dir
        } else {
            root.join(git_dir)
        };
        Ok(Self { root, git_dir })
    }

    pub fn cia_dir(&self) -> PathBuf {
        self.git_dir.join("cia")
    }

    pub fn git(&self, args: &[&str]) -> Result<String> {
        run(&self.root, args)
    }

    pub fn branch(&self) -> Result<Option<String>> {
        self.try_git(&["symbolic-ref", "-q", "--short", "HEAD"])
    }

    pub fn upstream(&self) -> Result<Option<String>> {
        self.try_git(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])
    }

    pub fn is_dirty(&self) -> Result<bool> {
        Ok(!self
            .git(&["status", "--porcelain", "--untracked-files=no"])?
            .is_empty())
    }

    pub fn read_file_at(&self, rev: &str, path: &str) -> Result<Option<String>> {
        if self
            .git(&["ls-tree", "--name-only", rev, "--", path])?
            .is_empty()
        {
            return Ok(None);
        }
        Ok(Some(self.git(&["show", &format!("{rev}:{path}")])?))
    }

    /// Whether any file matching the pathspecs differs between the two commits.
    pub fn changed(&self, from: &str, to: &str, paths: &[String]) -> Result<bool> {
        let mut args = vec!["diff", "--name-only", from, to, "--"];
        args.extend(paths.iter().map(String::as_str));
        Ok(!self.git(&args)?.is_empty())
    }

    /// Whether any tracked file matches the pathspecs.
    pub fn tracks(&self, paths: &[String]) -> Result<bool> {
        let mut args = vec!["ls-files", "--"];
        args.extend(paths.iter().map(String::as_str));
        Ok(!self.git(&args)?.is_empty())
    }

    /// Runs git and returns None instead of an error when it exits non-zero.
    fn try_git(&self, args: &[&str]) -> Result<Option<String>> {
        let out = command(&self.root, args)
            .output()
            .context("failed to run git; is it installed?")?;
        Ok(out
            .status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string()))
    }
}

pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(10)]
}

pub fn is_valid_branch_name(name: &str) -> Result<bool> {
    let status = command(
        Path::new("."),
        &["check-ref-format", &format!("refs/heads/{name}")],
    )
    .status()
    .context("failed to run git; is it installed?")?;
    Ok(status.success())
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    cmd
}

fn run(dir: &Path, args: &[&str]) -> Result<String> {
    let out = command(dir, args)
        .output()
        .context("failed to run git; is it installed?")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
