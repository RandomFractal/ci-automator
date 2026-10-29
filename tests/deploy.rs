use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::Instant,
};

use tempfile::TempDir;

struct Env {
    tmp: TempDir,
}

impl Env {
    /// origin (bare) <- dev (pushes) ; host (deploys) clones origin.
    fn new(fake_uv: &str) -> Self {
        let env = Self {
            tmp: tempfile::tempdir().unwrap(),
        };
        fs::create_dir_all(env.path("home")).unwrap();
        fs::create_dir_all(env.path("bin")).unwrap();
        let uv = env.path("bin/uv");
        fs::write(
            &uv,
            format!(
                "#!/bin/sh\necho \"$@\" >> \"{}\"\n{fake_uv}\n",
                env.path("uv-calls").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&uv, fs::Permissions::from_mode(0o755)).unwrap();

        env.git(&env.path(""), &["init", "--bare", "-q", "origin.git"]);
        env.git(&env.path(""), &["init", "-q", "dev"]);
        env.git(
            &env.path("dev"),
            &["remote", "add", "origin", "../origin.git"],
        );
        env.git(&env.path("dev"), &["checkout", "-q", "-b", "main"]);
        env.commit("README", "hello");
        env.git(&env.path("dev"), &["push", "-q", "-u", "origin", "main"]);
        env.git(
            &env.path(""),
            &["clone", "-q", "-b", "main", "origin.git", "host"],
        );
        env
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn command(&self, program: &str, dir: &Path) -> Command {
        let mut cmd = Command::new(program);
        let path = format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap()
        );
        cmd.current_dir(dir)
            .env("HOME", self.path("home"))
            .env_remove("XDG_CONFIG_HOME")
            .env("PATH", path)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        cmd
    }

    fn git(&self, dir: &Path, args: &[&str]) {
        let out = self.command("git", dir).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn commit(&self, file: &str, content: &str) {
        let dev = self.path("dev");
        fs::write(dev.join(file), content).unwrap();
        self.git(&dev, &["add", file]);
        self.git(&dev, &["commit", "-q", "-m", file]);
    }

    fn push(&self, file: &str, content: &str) {
        self.commit(file, content);
        self.git(&self.path("dev"), &["push", "-q"]);
    }

    fn cia(&self, args: &[&str]) -> Output {
        self.command(env!("CARGO_BIN_EXE_cia"), &self.path("host"))
            .args(args)
            .output()
            .unwrap()
    }

    fn uv_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("uv-calls"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    fn log(&self, name: &str) -> String {
        fs::read_to_string(self.path("host/.git/cia").join(name)).unwrap_or_default()
    }
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn deploys_only_when_there_is_something_new() {
    let env = Env::new("");
    assert!(env.cia(&["--add-branch", "main"]).status.success());

    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stderr(&out).contains("baseline"));
    assert!(env.uv_calls().is_empty());

    env.push("DEPLOY.cia", "[uv]\nrun = ['fast restart']\n");
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.uv_calls(), ["run fast restart"]);
    assert!(env.path("host/DEPLOY.cia").exists());

    let out = env.cia(&["--deploy"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("up to date"));
    assert_eq!(env.uv_calls().len(), 1);

    assert!(env.cia(&["--deploy", "--force"]).status.success());
    assert_eq!(env.uv_calls().len(), 2);

    let events = env.log("events.log");
    let activity = env.log("activity.log");
    assert!(events.contains("deployed main"));
    assert!(!events.contains("up to date"));
    assert!(activity.contains("up to date"));
    assert!(activity.contains("deployed main"));
}

#[test]
fn skips_branches_that_are_not_targets() {
    let env = Env::new("");
    env.push("DEPLOY.cia", "[uv]\nrun = ['app']\n");
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success());
    assert!(stderr(&out).contains("not a target branch"));
    assert!(env.uv_calls().is_empty());
    assert!(
        !env.path("host/DEPLOY.cia").exists(),
        "must not pull a non-target branch"
    );
}

#[test]
fn global_branches_apply_to_repos() {
    let env = Env::new("");
    assert!(
        env.cia(&["--add-branch", "main", "--global"])
            .status
            .success()
    );
    env.push("DEPLOY.cia", "[uv]\nrun = ['app']\n");
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls(), ["run app"]);
}

#[test]
fn failed_deploy_is_retried_on_the_next_run() {
    let env = Env::new("exit 3");
    env.cia(&["--add-branch", "main"]);
    env.push("DEPLOY.cia", "[uv]\nrun = ['app']\n");

    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("exit code 3"));

    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert_eq!(
        env.uv_calls().len(),
        2,
        "the failed commit must be retried, not recorded as a baseline"
    );
    assert_eq!(
        env.log("events.log").matches("ERROR").count(),
        1,
        "a repeated error is logged once"
    );
}

#[test]
fn timed_out_steps_are_killed() {
    let env = Env::new("sleep 30");
    env.cia(&["--add-branch", "main"]);
    env.push("DEPLOY.cia", "[uv]\nrun = ['app']\ntimeout = '1s'\n");
    let started = Instant::now();
    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("timed out"));
    assert!(started.elapsed().as_secs() < 10);
}

#[test]
fn refuses_to_pull_over_local_changes() {
    let env = Env::new("");
    env.cia(&["--add-branch", "main"]);
    fs::write(env.path("host/README"), "edited").unwrap();
    env.push("DEPLOY.cia", "[uv]\nrun = ['app']\n");
    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("uncommitted changes"));
    assert!(env.uv_calls().is_empty());
}

#[test]
fn dry_run_changes_nothing() {
    let env = Env::new("");
    env.cia(&["--add-branch", "main"]);
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "[uv]\nrun = ['fast restart']\n");
    let out = env.cia(&["--deploy", "--dry-run"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("would pull 1 commit(s)"), "{stdout}");
    assert!(
        stdout.contains("would run: uv run fast restart"),
        "{stdout}"
    );
    assert!(!env.path("host/DEPLOY.cia").exists());
    assert!(env.uv_calls().is_empty());
}

#[test]
fn config_commands() {
    let env = Env::new("");
    assert!(env.cia(&["--add-branch", "main"]).status.success());
    assert!(env.cia(&["--add-branch", "staging"]).status.success());
    assert!(env.cia(&["--rm-branch", "main"]).status.success());
    assert!(env.cia(&["--interval", "30s"]).status.success());
    assert!(!env.cia(&["--interval", "30"]).status.success());
    assert!(!env.cia(&["--add-branch", "bad..name"]).status.success());
    let cfg = fs::read_to_string(env.path("host/.git/cia/config.toml")).unwrap();
    assert!(
        cfg.contains("staging") && !cfg.contains("main") && cfg.contains("30s"),
        "{cfg}"
    );
    assert!(
        env.log("events.log")
            .contains("config: added target branch staging")
    );
    assert!(env.cia(&["--reset-branches"]).status.success());
    assert!(
        !fs::read_to_string(env.path("host/.git/cia/config.toml"))
            .unwrap()
            .contains("staging")
    );
    assert!(!env.cia(&["--deploy", "--global"]).status.success());
}
