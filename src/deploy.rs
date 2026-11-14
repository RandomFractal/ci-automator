use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{BufRead, BufReader, Read},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
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

use crate::{
    config,
    git::{Repo, short},
    interval::{self, Interval},
    log::{DEPLOY_START, Logger, now},
    project::{FILE_NAME, Project},
    state::State,
    webhook,
};

const DEFAULT_TIMEOUT: Interval = Interval::new(10, 'm');
const POLL: Duration = Duration::from_millis(100);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

const KEYWORDS: &[&str] = &[
    "RUN", "TRY", "RUNNER", "TIMEOUT", "WORKDIR", "ENV", "SLEEP", "RETRY", "WHEN", "CHANGED",
];
/// Project settings: each may appear once, before the first step.
const HEADER_KEYWORDS: &[&str] = &["LOCAL"];

#[derive(Debug, Default)]
pub struct Cia {
    /// As written; resolved by `Project`.
    pub local: Option<String>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum When {
    Always,
    Deploy,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retry {
    pub tries: u32,
    pub every: Interval,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Command {
        program: String,
        args: Vec<String>,
        may_fail: bool,
    },
    Sleep(Interval),
}

#[derive(Debug)]
pub struct Step {
    pub line: usize,
    pub action: Action,
    pub timeout: Interval,
    /// Relative to the repo root unless absolute.
    pub workdir: PathBuf,
    pub env: Vec<(String, String)>,
    pub retry: Option<Retry>,
    pub when: When,
    /// Pathspecs; empty means the step runs whatever changed.
    pub changed: Vec<String>,
}

impl Step {
    pub fn display(&self) -> String {
        match &self.action {
            Action::Command { program, args, .. } => {
                shell_words::join(std::iter::once(program).chain(args))
            }
            Action::Sleep(d) => format!("sleep {d}"),
        }
    }

    /// The settings that differ from the defaults, for dry-run and --check output.
    pub fn details(&self) -> String {
        let Action::Command { may_fail, .. } = self.action else {
            return String::new();
        };
        let mut parts = vec![format!("timeout {}", self.timeout)];
        if self.workdir != Path::new(".") {
            parts.push(format!("in {}", self.workdir.display()));
        }
        if let Some(r) = self.retry.filter(|_| !may_fail) {
            parts.push(format!("up to {} tries every {}", r.tries, r.every));
        }
        if may_fail {
            parts.push("may fail".into());
        }
        format!(" ({})", parts.join(", "))
    }

    fn runs_in(&self, mode: Mode) -> bool {
        match self.when {
            When::Always => true,
            When::Deploy => mode == Mode::Deploy,
            When::Rollback => mode == Mode::Rollback,
        }
    }
}

struct Settings {
    runner: Vec<String>,
    timeout: Interval,
    workdir: PathBuf,
    env: Vec<(String, String)>,
    retry: Option<Retry>,
    when: When,
    changed: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            runner: Vec::new(),
            timeout: DEFAULT_TIMEOUT,
            workdir: PathBuf::from("."),
            env: Vec::new(),
            retry: None,
            when: When::Always,
            changed: Vec::new(),
        }
    }
}

/// Steps run in file order. Every problem is reported, not just the first.
pub fn parse(text: &str) -> Result<Cia> {
    let mut settings = Settings::default();
    let mut cia = Cia::default();
    let mut errors = Vec::new();
    for (line, instruction) in instructions(text) {
        let (keyword, rest) = instruction
            .split_once(char::is_whitespace)
            .map_or((instruction.as_str(), ""), |(k, r)| (k, r.trim()));
        let result = if HEADER_KEYWORDS.contains(&keyword) {
            if cia.steps.is_empty() {
                parse_header(&mut cia, keyword, rest).map(|()| None)
            } else {
                Err(format!("{keyword} must come before the first step"))
            }
        } else {
            parse_instruction(&mut settings, keyword, rest)
        };
        match result {
            Ok(Some(action)) => cia.steps.push(Step {
                line,
                action,
                timeout: settings.timeout,
                workdir: settings.workdir.clone(),
                env: settings.env.clone(),
                retry: settings.retry,
                when: settings.when,
                changed: settings.changed.clone(),
            }),
            Ok(None) => {}
            Err(e) => errors.push(format!("line {line}: {e}")),
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("; "));
    }
    Ok(cia)
}

fn parse_header(cia: &mut Cia, keyword: &str, rest: &str) -> Result<(), String> {
    let value = match shell_words::split(rest).as_deref() {
        Ok([word]) => word.clone(),
        _ => return Err(format!("{keyword} takes one argument")),
    };
    let field = match keyword {
        "LOCAL" => &mut cia.local,
        _ => unreachable!("not a header keyword"),
    };
    if field.is_some() {
        return Err(format!("{keyword} is set more than once"));
    }
    *field = Some(value);
    Ok(())
}

/// Joins continuation lines and drops comments, keeping each instruction's first line number.
fn instructions(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut pending: Option<(usize, String)> = None;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.starts_with('#') || (line.is_empty() && pending.is_none()) {
            continue;
        }
        let (body, continues) = match line.strip_suffix('\\') {
            Some(body) => (body.trim_end(), true),
            None => (line, false),
        };
        let (_, text) = pending.get_or_insert_with(|| (i + 1, String::new()));
        if !text.is_empty() && !body.is_empty() {
            text.push(' ');
        }
        text.push_str(body);
        if !continues {
            out.extend(pending.take());
        }
    }
    out.extend(pending);
    out
}

/// Updates the settings, and returns the step if the instruction is one.
fn parse_instruction(
    s: &mut Settings,
    keyword: &str,
    rest: &str,
) -> Result<Option<Action>, String> {
    let words = || shell_words::split(rest).map_err(|e| format!("cannot split {rest:?}: {e}"));
    let one = || match words()?.as_slice() {
        [word] => Ok(word.clone()),
        _ => Err(format!("{keyword} takes one argument")),
    };
    let interval = || interval::parse(&one()?);
    match keyword {
        "RUN" | "TRY" => {
            let words = words()?;
            if words.is_empty() {
                return Err(format!("{keyword} needs a command"));
            }
            let mut words = s.runner.iter().cloned().chain(words);
            return Ok(Some(Action::Command {
                program: words.next().unwrap_or_default(),
                args: words.collect(),
                may_fail: keyword == "TRY",
            }));
        }
        "SLEEP" => return Ok(Some(Action::Sleep(interval()?))),
        "RUNNER" => {
            s.runner = match words()? {
                w if w.is_empty() => return Err("RUNNER needs a command prefix, or none".into()),
                w if w == ["none"] => Vec::new(),
                w => w,
            }
        }
        "TIMEOUT" => s.timeout = interval()?,
        "WORKDIR" => s.workdir = PathBuf::from(one()?),
        "ENV" => {
            let words = words()?;
            if words.is_empty() {
                return Err("ENV needs KEY=value".into());
            }
            for word in words {
                let (key, value) = word
                    .split_once('=')
                    .ok_or_else(|| format!("ENV expects KEY=value, got {word:?}"))?;
                if !is_env_name(key) {
                    return Err(format!("invalid variable name {key:?}"));
                }
                if key.starts_with("CIA_") {
                    return Err(format!("{key} is set by cia and cannot be changed"));
                }
                s.env.retain(|(k, _)| k != key);
                s.env.push((key.into(), value.into()));
            }
        }
        "RETRY" => s.retry = parse_retry(&words()?)?,
        "WHEN" => {
            s.when = match one()?.as_str() {
                "always" => When::Always,
                "deploy" => When::Deploy,
                "rollback" => When::Rollback,
                other => {
                    return Err(format!(
                        "WHEN expects deploy, rollback or always, got {other:?}"
                    ));
                }
            }
        }
        "CHANGED" => {
            let words = words()?;
            s.changed = match words.as_slice() {
                [] => return Err("CHANGED needs paths, or any".into()),
                [any] if any == "any" => Vec::new(),
                w if w.iter().any(|p| p == "any") => {
                    return Err("CHANGED any cannot be combined with paths".into());
                }
                _ => words,
            }
        }
        _ if keyword.starts_with('[') => {
            return Err(format!(
                "unknown instruction {keyword}; CIA has one instruction per line, \
                 see docs/CIA.md"
            ));
        }
        _ if KEYWORDS
            .iter()
            .chain(HEADER_KEYWORDS)
            .any(|k| *k == keyword.to_uppercase()) =>
        {
            return Err(format!(
                "instructions are uppercase: {}",
                keyword.to_uppercase()
            ));
        }
        _ => return Err(format!("unknown instruction {keyword}")),
    }
    Ok(None)
}

fn parse_retry(words: &[String]) -> Result<Option<Retry>, String> {
    let usage =
        || "RETRY expects <tries> every <interval>, e.g. RETRY 5 every 2s, or none".to_string();
    match words {
        [none] if none == "none" => Ok(None),
        [tries, every, interval] if every == "every" => {
            let tries: u32 = tries.parse().map_err(|_| usage())?;
            if tries < 2 {
                return Err("RETRY needs at least 2 tries".into());
            }
            Ok(Some(Retry {
                tries,
                every: interval::parse(interval)?,
            }))
        }
        _ => Err(usage()),
    }
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Deploy,
    Rollback,
}

struct Run {
    mode: Mode,
    commit: String,
    previous: Option<String>,
    /// The commit CHANGED compares against; None runs every step.
    base: Option<String>,
}

impl Run {
    /// `all` runs every step regardless of CHANGED, e.g. with --force or an edited CIA.
    fn new(mode: Mode, commit: &str, previous: Option<&str>, all: bool) -> Self {
        Self {
            mode,
            commit: commit.to_string(),
            previous: previous.map(String::from),
            base: previous.filter(|_| !all).map(String::from),
        }
    }

    /// Why the step is skipped as unchanged, if it is.
    fn unchanged(&self, repo: &Repo, step: &Step) -> Result<Option<String>> {
        match &self.base {
            Some(base)
                if !step.changed.is_empty()
                    && !repo.changed(base, &self.commit, &step.changed)? =>
            {
                Ok(Some(format!(
                    "nothing changed under {}",
                    step.changed.join(" ")
                )))
            }
            _ => Ok(None),
        }
    }
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
pub fn deploy(project: &Project, opts: Options) -> Result<bool> {
    if opts.dry_run {
        dry_run(project, opts.force)?;
        return Ok(true);
    }
    let log = Logger::new(&project.dir);
    let Some(_lock) = lock(&project.repo)? else {
        log.info("skipped: another cia run is in progress");
        return Ok(true);
    };
    let mut state = State::load(&project.dir)?;
    let result = check_and_deploy(project, &log, &mut state, opts.force);
    record(&project.dir, &log, state, result)
}

/// Saves the outcome of a run; an error repeated from the last run is logged quietly.
fn record(dir: &Path, log: &Logger, mut state: State, result: Result<String>) -> Result<bool> {
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
    state.save(dir)?;
    Ok(ok)
}

/// Re-reads the CIA file before every run, so edits apply without a restart.
pub fn watch(file: &Path) -> Result<()> {
    let project = Project::load(file)?;
    let dir = project.dir.clone();
    let log = Logger::new(&dir);
    let cfg = config::effective(&project.repo)?;
    let mut interval = cfg.interval;
    let (wake_tx, wake) = mpsc::channel();
    if let Some(addr) = &cfg.webhook {
        webhook::listen(
            addr,
            webhook::load_secret(&dir)?,
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
        let loaded = Project::load(file);
        let result = match &loaded {
            Ok(project) => deploy(project, Options::default()),
            Err(e) => State::load(&dir)
                .and_then(|state| record(&dir, &log, state, Err(anyhow::anyhow!("{e:#}")))),
        };
        if let Err(e) = result {
            log.error(&format!("{e:#}"));
        }
        match loaded.and_then(|p| config::effective(&p.repo)) {
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

fn check_and_deploy(
    project: &Project,
    log: &Logger,
    state: &mut State,
    force: bool,
) -> Result<String> {
    let repo = &project.repo;
    let Some(branch) = repo.branch()? else {
        return Ok(skip(log, "skipped: HEAD is detached"));
    };
    let cfg = config::effective(&project.repo)?;
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
            mark_deployed(project, state, &head)?;
            return Ok(format!("baseline {}", short(&head)));
        }
        Decision::Run => {}
    }

    let good = state
        .last_deployed
        .clone()
        .filter(|good| cfg.rollback && *good != head);
    let steps = &project.cia.steps;
    let edited = project.snapshot()?.as_deref() != Some(project.text.as_str());
    let run = Run::new(
        Mode::Deploy,
        &head,
        state.last_deployed.as_deref(),
        force || edited,
    );
    if !steps.iter().any(|s| s.runs_in(Mode::Deploy)) {
        log.event(&format!(
            "{branch} at {}: {FILE_NAME} defines nothing to run",
            short(&head)
        ));
        mark_deployed(project, state, &head)?;
        return Ok(format!("nothing to run at {}", short(&head)));
    }

    log.event(&format!(
        "{DEPLOY_START}{branch} at {}{}",
        short(&head),
        if force { " (forced)" } else { "" }
    ));
    state.last_attempted = Some(head.clone());
    let started = Instant::now();
    if let Err(e) = run_steps(repo, log, steps, &run) {
        return Err(roll_back(project, log, state, &head, good, e));
    }
    log.event(&format!(
        "deployed {branch} at {} in {}",
        short(&head),
        secs(started.elapsed())
    ));
    mark_deployed(project, state, &head)?;
    Ok(format!("deployed {}", short(&head)))
}

/// Resets to the last good commit and re-runs the CIA that deployed it.
/// Returns the deploy error with the rollback's outcome added.
fn roll_back(
    project: &Project,
    log: &Logger,
    state: &mut State,
    failed: &str,
    good: Option<String>,
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
    let result = project
        .repo
        .git(&["reset", "--hard", "--quiet", &good])
        .and_then(|_| {
            let snapshot = project.snapshot()?;
            let edited = snapshot.as_deref() != Some(project.text.as_str());
            let steps = match &snapshot {
                Some(text) => {
                    parse(text)
                        .map_err(|e| anyhow::anyhow!("invalid deployed.CIA: {e:#}"))?
                        .steps
                }
                None => parse(&project.text)?.steps,
            };
            let run = Run::new(Mode::Rollback, &good, Some(failed), edited);
            run_steps(&project.repo, log, &steps, &run)
        });
    match result {
        Ok(()) => {
            log.event(&format!("rolled back to {}", short(&good)));
            anyhow::anyhow!("{err:#}; rolled back to {}", short(&good))
        }
        Err(e) => anyhow::anyhow!("{err:#}; rollback to {} failed: {e:#}", short(&good)),
    }
}

fn dry_run(project: &Project, force: bool) -> Result<()> {
    let repo = &project.repo;
    let Some(branch) = repo.branch()? else {
        println!("HEAD is detached; nothing would run");
        return Ok(());
    };
    let cfg = config::effective(&project.repo)?;
    if !cfg.branches.contains(&branch) {
        println!("{branch} is not a target branch; nothing would run");
        return Ok(());
    }
    let upstream = upstream(repo, &branch)?;
    repo.git(&["fetch", "--quiet"])?;
    let head = repo.git(&["rev-parse", "HEAD"])?;
    let remote = repo.git(&["rev-parse", "@{u}"])?;
    let state = State::load(&project.dir)?;
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

    let steps = &project.cia.steps;
    if steps.is_empty() {
        println!("{FILE_NAME} defines nothing to run");
    }
    let edited = project.snapshot()?.as_deref() != Some(project.text.as_str());
    let run = Run::new(
        Mode::Deploy,
        &remote,
        state.last_deployed.as_deref(),
        force || edited,
    );
    for step in steps {
        let line = format!("{}{}", step.display(), step.details());
        if !step.runs_in(Mode::Deploy) {
            println!("only on rollback: {line}");
        } else if !will_run {
            println!("would not run: {line}");
        } else if let Some(why) = run.unchanged(repo, step)? {
            println!("would skip: {line}; {why}");
        } else {
            println!("would run: {line}");
        }
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

fn mark_deployed(project: &Project, state: &mut State, head: &str) -> Result<()> {
    state.last_failed = None;
    state.last_deployed = Some(head.to_string());
    state.last_deployed_at = Some(now());
    project.save_snapshot()
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

fn run_steps(repo: &Repo, log: &Logger, steps: &[Step], run: &Run) -> Result<()> {
    for step in steps.iter().filter(|s| s.runs_in(run.mode)) {
        if let Some(why) = run.unchanged(repo, step)? {
            log.event(&format!("skipped {}: {why}", step.display()));
            continue;
        }
        let may_fail = match step.action {
            Action::Sleep(d) => {
                log.event(&format!("sleeping {d}"));
                thread::sleep(d.duration());
                continue;
            }
            Action::Command { may_fail, .. } => may_fail,
        };
        let retry = step.retry.filter(|_| !may_fail);
        let tries = retry.map_or(1, |r| r.tries);
        for attempt in 1..=tries {
            let Err(e) = run_command(repo, log, step, run) else {
                break;
            };
            match retry.filter(|_| attempt < tries) {
                Some(r) => {
                    log.event(&format!(
                        "{e:#}; try {} of {tries} in {}",
                        attempt + 1,
                        r.every
                    ));
                    thread::sleep(r.every.duration());
                }
                None if may_fail => {
                    log.event(&format!("{e:#}; continuing because it is a TRY step"))
                }
                None => return Err(e),
            }
        }
    }
    Ok(())
}

fn run_command(repo: &Repo, log: &Logger, step: &Step, run: &Run) -> Result<()> {
    let Action::Command { program, args, .. } = &step.action else {
        return Ok(());
    };
    let name = step.display();
    let dir = repo.root.join(&step.workdir);
    if step.workdir == Path::new(".") {
        log.event(&format!("running {name}"));
    } else {
        log.event(&format!("running {name} in {}", step.workdir.display()));
    }
    if !dir.is_dir() {
        bail!("{name}: working directory {} does not exist", dir.display());
    }
    let started = Instant::now();
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&dir)
        .envs(step.env.iter().map(|(k, v)| (k, v)))
        .env("CIA_COMMIT", &run.commit)
        .env(
            "CIA_ROLLBACK",
            if run.mode == Mode::Rollback { "1" } else { "0" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Own process group so a timeout can kill everything the step started.
        .process_group(0);
    match &run.previous {
        Some(prev) => cmd.env("CIA_PREVIOUS_COMMIT", prev),
        None => cmd.env_remove("CIA_PREVIOUS_COMMIT"),
    };
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to start {program}; is it installed and on PATH?"))?;

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

    fn command(step: &Step) -> (&str, Vec<&str>, bool) {
        match &step.action {
            Action::Command {
                program,
                args,
                may_fail,
            } => (
                program.as_str(),
                args.iter().map(String::as_str).collect(),
                *may_fail,
            ),
            Action::Sleep(_) => panic!("not a command"),
        }
    }

    fn parse_steps(text: &str) -> Result<Vec<Step>> {
        parse(text).map(|cia| cia.steps)
    }

    fn err(text: &str) -> String {
        parse_steps(text).err().unwrap().to_string()
    }

    #[test]
    fn run_lines_split_like_a_shell_but_run_directly() {
        let steps = parse_steps("RUN migrate --to \"v2 final\"\nRUN app; rm -rf /\n").unwrap();
        assert_eq!(
            command(&steps[0]),
            ("migrate", vec!["--to", "v2 final"], false)
        );
        assert_eq!(command(&steps[1]), ("app;", vec!["rm", "-rf", "/"], false));
        assert_eq!(steps[0].timeout, DEFAULT_TIMEOUT);
        assert_eq!(steps[0].workdir, Path::new("."));
        assert_eq!(steps[0].when, When::Always);
        assert_eq!(steps[1].line, 2);
    }

    #[test]
    fn comments_blank_lines_and_continuations() {
        let text = "# deploy\n\n  RUN podman build \\\n    -t app:latest \\\n    .\nRUN next\n";
        let steps = parse_steps(text).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].display(), "podman build -t app:latest .");
        assert_eq!(steps[0].line, 3);
        assert_eq!(steps[1].line, 6);
    }

    #[test]
    fn runner_prefixes_run_and_try_until_cleared() {
        let text = "RUNNER uv run\nRUN fast restart\nTRY fast status\nRUNNER none\nRUN make\n";
        let steps = parse_steps(text).unwrap();
        assert_eq!(steps[0].display(), "uv run fast restart");
        assert_eq!(
            command(&steps[1]),
            ("uv", vec!["run", "fast", "status"], true)
        );
        assert_eq!(steps[2].display(), "make");
    }

    #[test]
    fn settings_apply_to_the_steps_below() {
        let text = "\
TIMEOUT 20m
WORKDIR prototype
ENV A=1 B=\"two words\"
RUN build
WORKDIR .
ENV A=3
TIMEOUT 30s
RETRY 5 every 2s
RUN check
RETRY none
RUN last
";
        let steps = parse_steps(text).unwrap();
        assert_eq!(steps[0].timeout, Interval::new(20, 'm'));
        assert_eq!(steps[0].workdir, Path::new("prototype"));
        assert_eq!(
            steps[0].env,
            [("A".into(), "1".into()), ("B".into(), "two words".into())]
        );
        assert_eq!(steps[0].retry, None);
        assert_eq!(steps[1].timeout, Interval::new(30, 's'));
        assert_eq!(steps[1].workdir, Path::new("."));
        assert_eq!(
            steps[1].env,
            [("B".into(), "two words".into()), ("A".into(), "3".into())]
        );
        assert_eq!(
            steps[1].retry,
            Some(Retry {
                tries: 5,
                every: Interval::new(2, 's')
            })
        );
        assert_eq!(steps[2].retry, None);
    }

    #[test]
    fn sleep_when_and_changed() {
        let text = "\
WHEN deploy
SLEEP 5s
CHANGED prototype/ 'shared lib/'
RUN a
WHEN rollback
CHANGED any
RUN b
WHEN always
RUN c
";
        let steps = parse_steps(text).unwrap();
        assert_eq!(steps[0].action, Action::Sleep(Interval::new(5, 's')));
        assert_eq!(steps[0].when, When::Deploy);
        assert_eq!(steps[1].changed, ["prototype/", "shared lib/"]);
        assert_eq!(steps[2].when, When::Rollback);
        assert!(steps[2].changed.is_empty());
        assert_eq!(steps[3].when, When::Always);
        assert!(steps[0].runs_in(Mode::Deploy) && !steps[0].runs_in(Mode::Rollback));
        assert!(steps[2].runs_in(Mode::Rollback) && !steps[2].runs_in(Mode::Deploy));
        assert!(steps[3].runs_in(Mode::Rollback) && steps[3].runs_in(Mode::Deploy));
    }

    #[test]
    fn rejects_bad_instructions() {
        for text in [
            "RUN\n",
            "RUN   \n",
            "TRY\n",
            "RUN a \"b\n",
            "run app\n",
            "FROM alpine\n",
            "TIMEOUT\n",
            "TIMEOUT 10\n",
            "TIMEOUT 1m 2m\n",
            "SLEEP soon\n",
            "RUNNER\n",
            "WORKDIR\n",
            "WORKDIR a b\n",
            "ENV\n",
            "ENV A\n",
            "ENV 1A=x\n",
            "ENV CIA_COMMIT=x\n",
            "RETRY 5\n",
            "RETRY 1 every 2s\n",
            "RETRY five every 2s\n",
            "RETRY 5 each 2s\n",
            "WHEN sometimes\n",
            "CHANGED\n",
            "CHANGED any src/\n",
        ] {
            assert!(parse_steps(text).is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn reports_every_problem_with_line_numbers() {
        let e = err("RUN ok\nBOGUS x\n# comment\nTIMEOUT never\nrun app\n");
        assert!(e.contains("line 2: unknown instruction BOGUS"), "{e}");
        assert!(e.contains("line 4: invalid interval 'never'"), "{e}");
        assert!(e.contains("line 5: instructions are uppercase: RUN"), "{e}");
    }

    #[test]
    fn points_old_toml_files_at_the_reference() {
        let e = err("[uv]\nrun = ['fast restart']\n");
        assert!(e.contains("line 1: unknown instruction [uv]"), "{e}");
        assert!(e.contains("docs/CIA.md"), "{e}");
    }

    #[test]
    fn empty_file_has_no_steps() {
        assert!(parse_steps("").unwrap().is_empty());
        assert!(parse_steps("# nothing\n\nTIMEOUT 1m\n").unwrap().is_empty());
    }

    #[test]
    fn details_show_non_default_settings() {
        let steps = parse_steps("RUN a\nWORKDIR web\nRETRY 3 every 1s\nRUN b\nTRY c\n").unwrap();
        assert_eq!(steps[0].details(), " (timeout 10m)");
        assert_eq!(
            steps[1].details(),
            " (timeout 10m, in web, up to 3 tries every 1s)"
        );
        assert_eq!(steps[2].details(), " (timeout 10m, in web, may fail)");
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
