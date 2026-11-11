use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufRead, BufReader, Read},
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{self, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{
    config,
    git::{Repo, short},
    interval::Interval,
    log::{DEPLOY_START, Logger, now},
    state::State,
    webhook,
};

pub const DEPLOY_FILE: &str = "DEPLOY.cia";
const DEFAULT_TIMEOUT: Interval = Interval::new(10, 'm');
const POLL: Duration = Duration::from_millis(100);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Section {
    run: Vec<String>,
    timeout: Option<Interval>,
}

pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    pub timeout: Interval,
}

impl Step {
    pub fn display(&self) -> String {
        shell_words::join(std::iter::once(&self.program).chain(&self.args))
    }
}

/// Sections run in the order they appear in the file. Every problem is reported, not just the first.
pub fn parse_steps(text: &str) -> Result<Vec<Step>> {
    let table: toml::Table =
        toml::from_str(text).with_context(|| format!("invalid {DEPLOY_FILE}"))?;
    let mut steps = Vec::new();
    let mut errors = Vec::new();
    for (name, value) in table {
        let prefix: &[&str] = match name.as_str() {
            "uv" => &["uv", "run"],
            "run" => &[],
            _ => {
                errors.push(format!("unknown section [{name}]"));
                continue;
            }
        };
        let section = match Section::deserialize(value) {
            Ok(section) => section,
            Err(e) => {
                errors.push(format!("invalid [{name}] section: {}", e.message()));
                continue;
            }
        };
        let timeout = section.timeout.unwrap_or(DEFAULT_TIMEOUT);
        for entry in &section.run {
            let words = match shell_words::split(entry) {
                Ok(words) if words.is_empty() => {
                    errors.push(format!("empty [{name}] run entry"));
                    continue;
                }
                Ok(words) => words,
                Err(e) => {
                    errors.push(format!("invalid [{name}] run entry {entry:?}: {e}"));
                    continue;
                }
            };
            let mut words = prefix.iter().map(|w| w.to_string()).chain(words);
            steps.push(Step {
                program: words.next().unwrap_or_default(),
                args: words.collect(),
                timeout,
            });
        }
    }
    if !errors.is_empty() {
        bail!("invalid {DEPLOY_FILE}: {}", errors.join("; "));
    }
    Ok(steps)
}

#[derive(Default, Clone, Copy)]
pub struct Options {
    pub force: bool,
    pub dry_run: bool,
}

enum Decision {
    AlreadyDeployed,
    RecordBaseline,
    Run,
}

/// A fresh install that pulled nothing records HEAD instead of redeploying it,
/// so starting the service doesn't restart an app that is already current.
fn decide(state: &State, head: &str, pulled: bool, force: bool) -> Decision {
    match state.last_deployed.as_deref() {
        _ if force => Decision::Run,
        Some(deployed) if deployed == head => Decision::AlreadyDeployed,
        None if !pulled && state.last_attempted.is_none() => Decision::RecordBaseline,
        _ => Decision::Run,
    }
}

/// Returns Ok(false) when the run failed; the failure is already logged.
pub fn deploy(repo: &Repo, opts: Options) -> Result<bool> {
    if opts.dry_run {
        dry_run(repo, opts.force)?;
        return Ok(true);
    }
    let log = Logger::new(repo);
    let Some(_lock) = lock(repo)? else {
        log.info("skipped: another cia run is in progress");
        return Ok(true);
    };
    let mut state = State::load(repo)?;
    let result = check_and_deploy(repo, &log, &mut state, opts.force);
    state.last_run_at = Some(now());
    let ok = match result {
        Ok(summary) => {
            state.last_result = Some(summary);
            state.last_error = None;
            true
        }
        Err(e) => {
            let msg = format!("{e:#}");
            if state.last_error.as_deref() == Some(&msg) {
                log.info(&format!("error (unchanged since last run): {msg}"));
            } else {
                log.error(&msg);
            }
            state.last_result = Some("error".into());
            state.last_error = Some(msg);
            false
        }
    };
    state.save(repo)?;
    Ok(ok)
}

pub fn watch(repo: &Repo) -> Result<()> {
    let log = Logger::new(repo);
    let cfg = config::effective(repo)?;
    let mut interval = cfg.interval;
    let (wake_tx, wake) = mpsc::channel();
    if let Some(addr) = &cfg.webhook {
        webhook::listen(
            addr,
            webhook::load_secret(repo)?,
            log.clone(),
            wake_tx.clone(),
        )?;
    }
    log.event(&format!(
        "watch started (pid {}, every {interval}{})",
        process::id(),
        cfg.webhook
            .as_ref()
            .map_or(String::new(), |a| format!(", webhook on {a}"))
    ));
    loop {
        if let Err(e) = deploy(repo, Options::default()) {
            log.error(&format!("{e:#}"));
        }
        match config::effective(repo) {
            Ok(cfg) if cfg.interval != interval => {
                log.event(&format!(
                    "interval changed from {interval} to {}",
                    cfg.interval
                ));
                interval = cfg.interval;
            }
            Ok(_) => {}
            Err(e) => log.info(&format!(
                "cannot reload config, keeping interval {interval}: {e:#}"
            )),
        }
        if wake.recv_timeout(interval.duration()).is_ok() {
            // Several pushes in a row need only one check.
            while wake.try_recv().is_ok() {}
            log.info("woken by a webhook push");
        }
    }
}

fn check_and_deploy(repo: &Repo, log: &Logger, state: &mut State, force: bool) -> Result<String> {
    let Some(branch) = repo.branch()? else {
        return Ok(skip(log, "skipped: HEAD is detached"));
    };
    let cfg = config::effective(repo)?;
    if !cfg.branches.contains(&branch) {
        return Ok(skip(
            log,
            &format!("skipped: {branch} is not a target branch"),
        ));
    }
    let upstream = upstream(repo, &branch)?;
    repo.git(&["fetch", "--quiet"])?;
    let before = repo.git(&["rev-parse", "HEAD"])?;
    let remote = repo.git(&["rev-parse", "@{u}"])?;
    if !force && state.last_failed.as_deref() == Some(remote.as_str()) {
        bail!(
            "{upstream} is still at {}, which failed and was rolled back; \
             push a fix or use cia --deploy --force",
            short(&remote)
        );
    }
    let pulled = before != remote;
    if pulled {
        ensure_fast_forward(repo, &branch, &upstream, &before, &remote)?;
        if let Some(text) = repo.read_file_at(&remote, DEPLOY_FILE)? {
            parse_steps(&text).with_context(|| {
                format!(
                    "{DEPLOY_FILE} at {} is invalid; refusing to pull",
                    short(&remote)
                )
            })?;
        }
        repo.git(&["pull", "--ff-only", "--quiet"])?;
    }
    let head = repo.git(&["rev-parse", "HEAD"])?;
    if pulled {
        log.event(&format!(
            "pulled {upstream}: {} -> {}",
            short(&before),
            short(&head)
        ));
    }

    match decide(state, &head, pulled, force) {
        Decision::AlreadyDeployed => {
            return Ok(skip(
                log,
                &format!("up to date: {branch} at {}", short(&head)),
            ));
        }
        Decision::RecordBaseline => {
            log.event(&format!(
                "recorded {branch} at {} as the deployed baseline without running anything; \
                 use cia --deploy --force to deploy it now",
                short(&head)
            ));
            mark_deployed(state, &head);
            return Ok(format!("baseline {}", short(&head)));
        }
        Decision::Run => {}
    }

    let good = state
        .last_deployed
        .clone()
        .filter(|good| cfg.rollback && *good != head);
    let steps = match read_steps(repo) {
        Ok(steps) => steps,
        Err(e) => return Err(roll_back(repo, log, state, &head, good, false, e)),
    };
    if steps.is_empty() {
        log.event(&format!(
            "{branch} at {}: {DEPLOY_FILE} defines nothing to run",
            short(&head)
        ));
        mark_deployed(state, &head);
        return Ok(format!("nothing to run at {}", short(&head)));
    }

    log.event(&format!(
        "{DEPLOY_START}{branch} at {}{}",
        short(&head),
        if force { " (forced)" } else { "" }
    ));
    state.last_attempted = Some(head.clone());
    let started = Instant::now();
    if let Err(e) = steps.iter().try_for_each(|step| run_step(repo, log, step)) {
        return Err(roll_back(repo, log, state, &head, good, true, e));
    }
    log.event(&format!(
        "deployed {branch} at {} in {}",
        short(&head),
        secs(started.elapsed())
    ));
    mark_deployed(state, &head);
    Ok(format!("deployed {}", short(&head)))
}

fn read_steps(repo: &Repo) -> Result<Vec<Step>> {
    match fs::read_to_string(repo.root.join(DEPLOY_FILE)) {
        Ok(text) => parse_steps(&text),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {DEPLOY_FILE}")),
    }
}

/// Resets to the last good commit and re-runs its steps, unless no step ran.
/// Returns the deploy error with the rollback's outcome added.
fn roll_back(
    repo: &Repo,
    log: &Logger,
    state: &mut State,
    failed: &str,
    good: Option<String>,
    steps_ran: bool,
    err: anyhow::Error,
) -> anyhow::Error {
    let Some(good) = good else {
        return err;
    };
    log.event(&format!(
        "{err:#}; rolling back from {} to {}",
        short(failed),
        short(&good)
    ));
    state.last_failed = Some(failed.to_string());
    let result = repo
        .git(&["reset", "--hard", "--quiet", &good])
        .and_then(|_| {
            if steps_ran {
                read_steps(repo)?
                    .iter()
                    .try_for_each(|step| run_step(repo, log, step))?;
            }
            Ok(())
        });
    match result {
        Ok(()) => {
            log.event(&format!("rolled back to {}", short(&good)));
            anyhow::anyhow!("{err:#}; rolled back to {}", short(&good))
        }
        Err(e) => anyhow::anyhow!("{err:#}; rollback to {} failed: {e:#}", short(&good)),
    }
}

fn dry_run(repo: &Repo, force: bool) -> Result<()> {
    let Some(branch) = repo.branch()? else {
        println!("HEAD is detached; nothing would run");
        return Ok(());
    };
    let cfg = config::effective(repo)?;
    if !cfg.branches.contains(&branch) {
        println!("{branch} is not a target branch; nothing would run");
        return Ok(());
    }
    let upstream = upstream(repo, &branch)?;
    repo.git(&["fetch", "--quiet"])?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    let remote = repo.git(&["rev-parse", "@{u}"])?;
    let state = State::load(repo)?;
    if !force && state.last_failed.as_deref() == Some(remote.as_str()) {
        println!(
            "{upstream} is still at {}, which failed and was rolled back; nothing would run (use --force)",
            short(&remote)
        );
        return Ok(());
    }
    let pulled = head != remote;
    if pulled {
        ensure_fast_forward(repo, &branch, &upstream, &head, &remote)?;
        let commits = repo.git(&["log", "--oneline", "HEAD..@{u}"])?;
        println!(
            "would pull {} commit(s) from {upstream}:",
            commits.lines().count()
        );
        for line in commits.lines() {
            println!("  {line}");
        }
    } else {
        println!("{branch} is up to date with {upstream}");
    }

    let will_run = match decide(&state, &remote, pulled, force) {
        Decision::AlreadyDeployed => {
            println!(
                "{} is already deployed; nothing would run (use --force)",
                short(&remote)
            );
            false
        }
        Decision::RecordBaseline => {
            println!(
                "no deploy recorded yet; would record {} as the baseline without running anything",
                short(&remote)
            );
            false
        }
        Decision::Run => true,
    };

    let text = if pulled {
        repo.read_file_at(&remote, DEPLOY_FILE)?
    } else {
        match fs::read_to_string(repo.root.join(DEPLOY_FILE)) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("cannot read {DEPLOY_FILE}")),
        }
    };
    let steps = text
        .map(|t| parse_steps(&t))
        .transpose()?
        .unwrap_or_default();
    if steps.is_empty() {
        println!("{DEPLOY_FILE} defines nothing to run");
    }
    let label = if will_run {
        "would run"
    } else {
        "would not run"
    };
    for step in &steps {
        println!("{label}: {} (timeout {})", step.display(), step.timeout);
    }
    if !will_run {
        return Ok(());
    }
    if let Some(good) = state
        .last_deployed
        .as_deref()
        .filter(|g| cfg.rollback && *g != remote)
    {
        println!("if a step fails, would roll back to {}", short(good));
    }
    Ok(())
}

fn skip(log: &Logger, msg: &str) -> String {
    log.info(msg);
    msg.to_string()
}

fn mark_deployed(state: &mut State, head: &str) {
    state.last_failed = None;
    state.last_deployed = Some(head.to_string());
    state.last_deployed_at = Some(now());
}

fn upstream(repo: &Repo, branch: &str) -> Result<String> {
    repo.upstream()?.with_context(|| {
        format!("{branch} has no upstream branch; set one with: git branch --set-upstream-to=origin/{branch}")
    })
}

fn ensure_fast_forward(
    repo: &Repo,
    branch: &str,
    upstream: &str,
    head: &str,
    remote: &str,
) -> Result<()> {
    let base = repo.git(&["merge-base", head, remote])?;
    if base == remote {
        bail!("{branch} has local commits that are not on {upstream}; refusing to deploy");
    }
    if base != head {
        bail!("{branch} has diverged from {upstream}; refusing to pull");
    }
    if repo.is_dirty()? {
        bail!("working tree has uncommitted changes; refusing to pull");
    }
    Ok(())
}

fn lock(repo: &Repo) -> Result<Option<File>> {
    let dir = repo.cia_dir();
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dir.join("lock"))
        .context("cannot open lock file")?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e).context("cannot lock"),
    }
}

fn run_step(repo: &Repo, log: &Logger, step: &Step) -> Result<()> {
    let name = step.display();
    log.event(&format!("running {name}"));
    let started = Instant::now();
    let mut child = Command::new(&step.program)
        .args(&step.args)
        .current_dir(&repo.root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so a timeout can kill everything the step started.
        .process_group(0)
        .spawn()
        .with_context(|| {
            format!(
                "failed to start {}; is it installed and on PATH?",
                step.program
            )
        })?;

    let discard = Arc::new(AtomicBool::new(false));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_reader(
            out,
            log.clone(),
            name.clone(),
            discard.clone(),
        ));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_reader(
            err,
            log.clone(),
            name.clone(),
            discard.clone(),
        ));
    }

    let deadline = started + step.timeout.duration();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break Some(status);
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
            child.wait()?;
            break None;
        }
        thread::sleep(POLL);
    };

    // A step that backgrounds a server leaves it holding our pipes. Keep draining
    // so it never blocks or gets SIGPIPE, but stop logging what it writes.
    let grace = Instant::now() + OUTPUT_DRAIN_GRACE;
    while readers.iter().any(|r| !r.is_finished()) && Instant::now() < grace {
        thread::sleep(POLL);
    }
    if readers.iter().any(|r| !r.is_finished()) {
        discard.store(true, Ordering::Relaxed);
        log.info(&format!(
            "{name}: a background process still holds its output; ignoring further output"
        ));
    }

    let elapsed = secs(started.elapsed());
    match status {
        Some(s) if s.success() => {
            log.event(&format!("{name} finished in {elapsed}"));
            Ok(())
        }
        Some(s) => bail!("{name} failed ({}) after {elapsed}", describe(s)),
        None => bail!("{name} timed out after {} and was killed", step.timeout),
    }
}

fn spawn_reader(
    stream: impl Read + Send + 'static,
    log: Logger,
    source: String,
    discard: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            if !discard.load(Ordering::Relaxed) {
                log.output(&source, String::from_utf8_lossy(&buf).trim_end());
            }
        }
    })
}

fn describe(status: ExitStatus) -> String {
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(sig)) => format!("killed by signal {sig}"),
        (None, None) => "unknown status".into(),
    }
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uv_entries_become_uv_run_commands() {
        let steps =
            parse_steps("[uv]\nrun = ['fast restart', 'migrate --to \"v2 final\"']\n").unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].program, "uv");
        assert_eq!(steps[0].args, ["run", "fast", "restart"]);
        assert_eq!(steps[1].args, ["run", "migrate", "--to", "v2 final"]);
        assert_eq!(steps[0].timeout, DEFAULT_TIMEOUT);
    }

    #[test]
    fn run_entries_run_directly() {
        let steps = parse_steps("[run]\nrun = ['systemctl --user restart app']\n").unwrap();
        assert_eq!(steps[0].program, "systemctl");
        assert_eq!(steps[0].args, ["--user", "restart", "app"]);
    }

    #[test]
    fn sections_run_in_file_order() {
        let text = "[run]\nrun = ['make']\ntimeout = '1m'\n[uv]\nrun = ['migrate']\n";
        let steps = parse_steps(text).unwrap();
        assert_eq!(steps[0].display(), "make");
        assert_eq!(steps[0].timeout, Interval::new(1, 'm'));
        assert_eq!(steps[1].display(), "uv run migrate");
        assert_eq!(steps[1].timeout, DEFAULT_TIMEOUT);
        let reversed = "[uv]\nrun = ['migrate']\n[run]\nrun = ['make']\n";
        assert_eq!(parse_steps(reversed).unwrap()[0].program, "uv");
    }

    #[test]
    fn shell_syntax_is_not_interpreted() {
        let steps = parse_steps("[uv]\nrun = ['app; rm -rf /']\n").unwrap();
        assert_eq!(steps[0].args, ["run", "app;", "rm", "-rf", "/"]);
    }

    #[test]
    fn custom_timeout() {
        let steps = parse_steps("[uv]\nrun = ['x']\ntimeout = '30s'\n").unwrap();
        assert_eq!(steps[0].timeout, Interval::new(30, 's'));
    }

    #[test]
    fn rejects_unknown_sections_and_keys() {
        assert!(parse_steps("[uvv]\nrun = ['x']\n").is_err());
        assert!(parse_steps("[uv]\nruns = ['x']\n").is_err());
        assert!(parse_steps("[uv]\nrun = ['']\n").is_err());
        assert!(parse_steps("[run]\nrun = ['  ']\n").is_err());
        assert!(parse_steps("uv = 'x'\n").is_err());
    }

    #[test]
    fn reports_every_problem() {
        let err = parse_steps("[uvv]\nrun = ['x']\n[run]\nrun = ['ok', '', 'a \"b']\n")
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("unknown section [uvv]"), "{err}");
        assert!(err.contains("empty [run] run entry"), "{err}");
        assert!(err.contains("invalid [run] run entry"), "{err}");
    }

    #[test]
    fn empty_file_has_no_steps() {
        assert!(parse_steps("").unwrap().is_empty());
    }

    #[test]
    fn decisions() {
        let fresh = State::default();
        let deployed = State {
            last_deployed: Some("abc".into()),
            ..State::default()
        };
        let failed_first = State {
            last_attempted: Some("abc".into()),
            ..State::default()
        };
        assert!(matches!(
            decide(&fresh, "abc", false, false),
            Decision::RecordBaseline
        ));
        assert!(matches!(decide(&fresh, "abc", true, false), Decision::Run));
        assert!(matches!(decide(&fresh, "abc", false, true), Decision::Run));
        assert!(matches!(
            decide(&deployed, "abc", false, false),
            Decision::AlreadyDeployed
        ));
        assert!(matches!(
            decide(&deployed, "abc", false, true),
            Decision::Run
        ));
        assert!(matches!(
            decide(&deployed, "def", false, false),
            Decision::Run
        ));
        assert!(matches!(
            decide(&failed_first, "abc", false, false),
            Decision::Run
        ));
    }
}
