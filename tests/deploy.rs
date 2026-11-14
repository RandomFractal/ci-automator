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

    env.push("DEPLOY.cia", "RUN uv run fast restart\n");
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
    env.push("DEPLOY.cia", "RUN uv run app\n");
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
    env.push("DEPLOY.cia", "RUN uv run app\n");
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls(), ["run app"]);
}

#[test]
fn failed_deploy_is_retried_on_the_next_run() {
    let env = Env::new("exit 3");
    env.cia(&["--add-branch", "main"]);
    env.push("DEPLOY.cia", "RUN uv run app\n");

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
    env.push("DEPLOY.cia", "TIMEOUT 1s\nRUN uv run app\n");
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
    env.push("DEPLOY.cia", "RUN uv run app\n");
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
    env.push("DEPLOY.cia", "RUN uv run fast restart\n");
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

    env.cia(&["--deploy"]);
    let out = env.cia(&["--deploy", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("already deployed"), "{stdout}");
    assert!(
        stdout.contains("would not run: uv run fast restart"),
        "{stdout}"
    );
    assert_eq!(env.uv_calls().len(), 1);
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

#[test]
fn runner_applies_to_the_steps_below_it() {
    let env = Env::new("");
    assert!(env.cia(&["--add-branch", "main"]).status.success());
    assert!(env.cia(&["--deploy"]).status.success());

    env.push(
        "DEPLOY.cia",
        "RUN uv build\nRUNNER uv run\nRUN fast restart\n",
    );
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.uv_calls(), ["build", "run fast restart"]);
}

#[test]
fn refuses_to_pull_an_invalid_deploy_file() {
    let env = Env::new("");
    env.cia(&["--add-branch", "main"]);
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "BOGUS app\n");
    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("refusing to pull"),
        "{}",
        stderr(&out)
    );
    assert!(!env.path("host/DEPLOY.cia").exists(), "must not pull");
    assert!(env.uv_calls().is_empty());
}

#[test]
fn check_reports_problems() {
    let env = Env::new("");
    env.cia(&["--add-branch", "main"]);
    env.push(
        "DEPLOY.cia",
        "RUN no-such-program-xyz\nRUN ./build/app\nRUN uv run app\n",
    );
    env.cia(&["--deploy"]);
    let out = env.cia(&["--check"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(
        stdout.contains("error: line 1: no-such-program-xyz is not on PATH"),
        "{stdout}"
    );
    assert!(
        stdout.contains("warning: ./build/app does not exist yet"),
        "{stdout}"
    );
    assert!(stdout.contains("ok: uv run app"), "{stdout}");
    assert!(stdout.contains("upstream origin/main"), "{stdout}");

    fs::write(env.path("good.cia"), "RUN uv run app\n").unwrap();
    let good = env.path("good.cia");
    assert!(
        env.cia(&["--check", good.to_str().unwrap()])
            .status
            .success()
    );
    fs::write(env.path("bad.cia"), "RUNS app\n").unwrap();
    let bad = env.path("bad.cia");
    assert!(
        !env.cia(&["--check", bad.to_str().unwrap()])
            .status
            .success()
    );
}

const FAIL_ON_BAD: &str = "case \"$2\" in bad) exit 3;; esac";

#[test]
fn failed_deploy_rolls_back_to_the_last_good_commit() {
    let env = Env::new(FAIL_ON_BAD);
    env.cia(&["--add-branch", "main"]);
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "RUN uv run good\n");
    assert!(env.cia(&["--deploy"]).status.success());

    env.push("DEPLOY.cia", "RUN uv run bad\n");
    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("rolled back to"), "{}", stderr(&out));
    assert_eq!(env.uv_calls(), ["run good", "run bad", "run good"]);
    assert!(
        fs::read_to_string(env.path("host/DEPLOY.cia"))
            .unwrap()
            .contains("good")
    );

    let out = env.cia(&["--deploy"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("failed and was rolled back"),
        "{}",
        stderr(&out)
    );
    assert_eq!(env.uv_calls().len(), 3, "the failed commit is not retried");
    let status = String::from_utf8_lossy(&env.cia(&["--status"]).stdout).into_owned();
    assert!(status.contains("rolled back:"), "{status}");

    let out = env.cia(&["--deploy", "--force"]);
    assert!(!out.status.success());
    assert_eq!(
        env.uv_calls().len(),
        5,
        "--force retries it and rolls back again"
    );

    env.push("DEPLOY.cia", "RUN uv run fixed\n");
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.uv_calls().last().unwrap(), "run fixed");
    let status = String::from_utf8_lossy(&env.cia(&["--status"]).stdout).into_owned();
    assert!(!status.contains("rolled back:"), "{status}");
}

#[test]
fn rollback_can_be_turned_off() {
    let env = Env::new(FAIL_ON_BAD);
    env.cia(&["--add-branch", "main"]);
    assert!(env.cia(&["--rollback", "off"]).status.success());
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "RUN uv run good\n");
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "RUN uv run bad\n");
    assert!(!env.cia(&["--deploy"]).status.success());
    assert!(!env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls(), ["run good", "run bad", "run bad"]);
}

#[test]
fn log_last_shows_only_the_last_deploy() {
    let env = Env::new("echo \"output of $2\"");
    env.cia(&["--add-branch", "main"]);
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "RUN uv run first\n");
    env.cia(&["--deploy"]);
    env.push("DEPLOY.cia", "RUN uv run second\n");
    env.cia(&["--deploy"]);
    let out = env.cia(&["--log", "--last"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(stdout.contains("output of second"), "{stdout}");
    assert!(!stdout.contains("output of first"), "{stdout}");
}

impl Env {
    /// Commits several files in one commit and pushes it.
    fn push_files(&self, files: &[(&str, &str)]) {
        let dev = self.path("dev");
        for (file, content) in files {
            let path = dev.join(file);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
            self.git(&dev, &["add", file]);
        }
        self.git(&dev, &["commit", "-q", "-m", "files"]);
        self.git(&dev, &["push", "-q"]);
    }

    /// Records a baseline, so later pushes deploy.
    fn ready(&self) {
        assert!(self.cia(&["--add-branch", "main"]).status.success());
        assert!(self.cia(&["--deploy"]).status.success());
    }
}

#[test]
fn retry_runs_a_failing_step_again_until_it_passes() {
    let env = Env::new(
        "n=$(cat \"$HOME/tries\" 2>/dev/null || echo 0); n=$((n+1)); echo $n > \"$HOME/tries\"; [ $n -ge 3 ]",
    );
    env.ready();
    env.push("DEPLOY.cia", "RETRY 3 every 1s\nRUN uv run check\n");
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(env.uv_calls().len(), 3);
    assert!(env.log("events.log").contains("try 3 of 3 in 1s"));

    fs::remove_file(env.path("home/tries")).unwrap();
    env.push("DEPLOY.cia", "RETRY 2 every 1s\nRUN uv run check\n");
    let out = env.cia(&["--deploy"]);
    assert!(stderr(&out).contains("rolled back to"), "runs out of tries");
    assert_eq!(
        env.uv_calls().len(),
        6,
        "2 failed tries, then the rollback's"
    );
}

#[test]
fn a_failing_try_step_does_not_fail_the_deploy() {
    let env = Env::new(FAIL_ON_BAD);
    env.ready();
    env.push(
        "DEPLOY.cia",
        "RETRY 3 every 1s\nTRY uv run bad\nRUN uv run good\n",
    );
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        env.uv_calls(),
        ["run bad", "run good"],
        "TRY is not retried"
    );
    assert!(
        env.log("events.log")
            .contains("continuing because it is a TRY step")
    );
}

#[test]
fn when_picks_steps_for_deploys_and_rollbacks() {
    let env = Env::new(FAIL_ON_BAD);
    env.ready();
    env.push(
        "DEPLOY.cia",
        "WHEN deploy\nRUN uv run backup\nWHEN rollback\nRUN uv run restore\nWHEN always\nRUN uv run good\n",
    );
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls(), ["run backup", "run good"]);

    env.push(
        "DEPLOY.cia",
        "WHEN deploy\nRUN uv run backup\nRUN uv run bad\n",
    );
    let out = env.cia(&["--deploy"]);
    assert!(stderr(&out).contains("rolled back to"), "{}", stderr(&out));
    assert_eq!(
        env.uv_calls()[2..],
        ["run backup", "run bad", "run restore", "run good"]
    );
}

#[test]
fn steps_get_workdir_env_and_cia_variables() {
    let env = Env::new(
        "echo \"$(basename \"$PWD\") $CIA_ROLLBACK $GREETING ${CIA_PREVIOUS_COMMIT:+prev} ${#CIA_COMMIT}\" >> \"$HOME/env\"",
    );
    env.ready();
    env.push_files(&[
        ("web/index.html", "hi"),
        (
            "DEPLOY.cia",
            "ENV GREETING=\"hello there\"\nWORKDIR web\nRUN uv run a\n",
        ),
    ]);
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        fs::read_to_string(env.path("home/env")).unwrap(),
        "web 0 hello there prev 40\n"
    );
    assert!(env.log("events.log").contains("running uv run a in web"));
}

#[test]
fn changed_runs_steps_only_when_their_paths_changed() {
    let env = Env::new("");
    env.ready();
    let file = "CHANGED a/\nRUN uv run a\nCHANGED b/ shared/\nRUN uv run b\nCHANGED any\nRUN uv run always\n";
    env.push_files(&[("a/x", "1"), ("DEPLOY.cia", file)]);
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(
        env.uv_calls(),
        ["run a", "run b", "run always"],
        "a changed DEPLOY.cia runs everything"
    );

    env.push_files(&[("shared/lib", "1")]);
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls()[3..], ["run b", "run always"]);

    env.push("README", "docs only");
    let out = env.cia(&["--deploy", "--dry-run"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("would skip: uv run a (timeout 10m); nothing changed under a/"),
        "{stdout}"
    );
    assert!(stdout.contains("would run: uv run always"), "{stdout}");
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls()[5..], ["run always"]);
    assert!(
        env.log("events.log")
            .contains("skipped uv run b: nothing changed under b/ shared/")
    );

    assert!(env.cia(&["--deploy", "--force"]).status.success());
    assert_eq!(env.uv_calls()[6..], ["run a", "run b", "run always"]);
}

#[test]
fn changed_compares_against_the_last_successful_deploy() {
    let env = Env::new("[ -e fail ] && [ \"$2\" = always ] && exit 3; true");
    env.ready();
    let file =
        "CHANGED a/\nRUN uv run a\nCHANGED b/\nRUN uv run b\nCHANGED any\nRUN uv run always\n";
    env.push("DEPLOY.cia", file);
    assert!(env.cia(&["--deploy"]).status.success());
    assert_eq!(env.uv_calls().len(), 3);

    env.push_files(&[("a/x", "1"), ("fail", "")]);
    let out = env.cia(&["--deploy"]);
    assert!(stderr(&out).contains("rolled back to"), "{}", stderr(&out));
    assert_eq!(
        env.uv_calls()[3..],
        ["run a", "run always", "run a", "run always"],
        "the rollback restores only what the failed commit touched"
    );

    env.push_files(&[("fail", "fixed")]);
    fs::remove_file(env.path("dev/fail")).unwrap();
    env.git(&env.path("dev"), &["rm", "-q", "fail"]);
    env.git(&env.path("dev"), &["commit", "-q", "-m", "fix"]);
    env.git(&env.path("dev"), &["push", "-q"]);
    let out = env.cia(&["--deploy"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        env.uv_calls()[7..],
        ["run a", "run always"],
        "a/ changed since the last successful deploy, even though that change first failed"
    );
}

#[test]
fn check_reports_workdir_and_changed_problems() {
    let env = Env::new("");
    env.ready();
    let file = env.path("check.cia");
    fs::write(
        &file,
        "WORKDIR nope\nRUN uv run app\nWORKDIR .\nCHANGED zzz/\nRUN uv run x\n",
    )
    .unwrap();
    let out = env.cia(&["--check", file.to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(
        stdout.contains("error: line 2: WORKDIR nope does not exist"),
        "{stdout}"
    );
    assert!(
        stdout.contains("warning: line 5: CHANGED zzz/ matches no tracked file"),
        "{stdout}"
    );
}

struct Watch(std::process::Child);

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn post(addr: &str, event: &str, signature: &str, body: &str) -> String {
    use std::io::{Read, Write};
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    let mut stream = loop {
        match std::net::TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50))
            }
            Err(e) => panic!("cannot connect to {addr}: {e}"),
        }
    };
    write!(
        stream,
        "POST /hook HTTP/1.1\r\nX-GitHub-Event: {event}\r\nX-Hub-Signature-256: {signature}\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    response
}

fn sign(secret: &str, body: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let bytes = mac.finalize().into_bytes();
    format!(
        "sha256={}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

#[test]
fn signed_push_webhook_triggers_a_deploy() {
    let env = Env::new("");
    env.cia(&["--add-branch", "main"]);
    env.cia(&["--interval", "1h"]);
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}");
    let out = env.cia(&["--webhook", &addr]);
    assert!(out.status.success(), "{}", stderr(&out));
    let secret = fs::read_to_string(env.path("host/.git/cia/webhook-secret")).unwrap();
    let mode = fs::metadata(env.path("host/.git/cia/webhook-secret"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&secret));

    let _watch = Watch(
        env.command(env!("CARGO_BIN_EXE_cia"), &env.path("host"))
            .arg("--watch")
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while !env.log("events.log").contains("deployed baseline") && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let body = r#"{"ref":"refs/heads/main"}"#;
    assert!(post(&addr, "ping", &sign(&secret, body), body).starts_with("HTTP/1.1 200"));
    env.push("DEPLOY.cia", "RUN uv run app\n");
    assert!(post(&addr, "push", &sign("wrong", body), body).starts_with("HTTP/1.1 401"));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        env.uv_calls().is_empty(),
        "an unsigned push must not deploy"
    );

    assert!(post(&addr, "push", &sign(&secret, body), body).starts_with("HTTP/1.1 202"));
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    while env.uv_calls().is_empty() && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(env.uv_calls(), ["run app"]);

    assert!(env.cia(&["--webhook", "off"]).status.success());
    assert!(!env.cia(&["--webhook", "nonsense"]).status.success());
}
