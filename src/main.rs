mod config;
mod deploy;
mod git;
mod interval;
mod log;
mod project;
mod service;
mod state;
mod webhook;

use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Result, bail};
use clap::{ArgGroup, Parser};

use crate::{
    config::{Config, Overrides},
    git::short,
    interval::Interval,
    log::Logger,
    project::{FILE_NAME, Project},
    service::Scope,
    state::State,
};

#[derive(Parser)]
#[command(
    name = "cia",
    version,
    about = "CI automations for hosts where hosted CI is not available"
)]
#[command(group(ArgGroup::new("action").args([
    "show_config", "check", "deploy", "watch", "status", "log", "install_service",
    "uninstall_service",
])))]
struct Cli {
    /// The CIA file, or a directory holding one (default: ./CIA)
    #[arg(value_name = "CIA")]
    cia: Option<PathBuf>,

    /// For this run, how often --watch runs, e.g. 30s, 5m, 2h, 1d (CIA: INTERVAL).
    /// With --global and no action, sets the host default instead
    #[arg(long, value_name = "NN{s|m|h|d}", value_parser = interval::parse,
          conflicts_with_all = ["log", "install_service", "uninstall_service"])]
    interval: Option<Interval>,

    /// For this run, roll back to the last good commit when a deploy fails (CIA: ROLLBACK).
    /// With --global and no action, sets the host default instead
    #[arg(long, value_name = "on|off", value_parser = parse_on_off,
          conflicts_with_all = ["log", "install_service", "uninstall_service"])]
    rollback: Option<bool>,

    /// For this run, listen for GitHub push webhooks on ADDR in --watch, or "off" (CIA: WEBHOOK)
    #[arg(long, value_name = "ADDR|off", value_parser = parse_webhook,
          conflicts_with_all = ["global", "log", "install_service", "uninstall_service"])]
    webhook: Option<String>,

    /// Show the global, CIA and effective settings
    #[arg(long)]
    show_config: bool,

    /// Check the config and the CIA file, and that each step can run
    #[arg(long)]
    check: bool,

    /// Pull BRANCH and run the CIA steps if there is something new
    #[arg(long)]
    deploy: bool,

    /// Run --deploy every interval until stopped
    #[arg(long)]
    watch: bool,

    /// Show the last run, last deploy and service state
    #[arg(long)]
    status: bool,

    /// Print the end of the events log
    #[arg(long)]
    log: bool,

    /// Install and start a systemd service that runs --watch for this CIA file
    #[arg(long)]
    install_service: bool,

    /// Stop and remove this CIA file's systemd service
    #[arg(long)]
    uninstall_service: bool,

    /// With --interval or --rollback and no action, set host defaults; with service commands, use a system-wide service
    #[arg(long, conflicts_with_all = ["show_config", "check", "deploy", "watch", "status", "log"])]
    global: bool,

    /// With --deploy: run the steps even if this commit is already deployed
    #[arg(long, requires = "deploy")]
    force: bool,

    /// With --deploy: show what would happen without pulling or running anything
    #[arg(long, requires = "deploy")]
    dry_run: bool,

    /// With --log: print the activity log, which includes routine checks
    #[arg(long, requires = "log")]
    activity: bool,

    /// With --log: print only the last deploy, from its start to the end of the log
    #[arg(long, requires = "log", conflicts_with = "activity")]
    last: bool,

    /// With --log: number of lines to print
    #[arg(
        short = 'n',
        value_name = "LINES",
        default_value_t = 50,
        requires = "log"
    )]
    lines: usize,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("cia: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<bool> {
    let scope = if cli.global {
        Scope::System
    } else {
        Scope::User
    };
    let overrides = Overrides {
        interval: cli.interval,
        rollback: cli.rollback,
        webhook: cli.webhook.clone(),
    };
    let file = || project::locate(cli.cia.as_deref());
    let load = || Project::load(&file()?).map(|p| p.with(overrides.clone()));
    if cli.show_config {
        show_config(cli.cia.as_deref(), &overrides)?;
    } else if cli.check {
        return check(&file()?, &overrides);
    } else if cli.deploy {
        let opts = deploy::Options {
            force: cli.force,
            dry_run: cli.dry_run,
        };
        return deploy::deploy(&load()?, opts);
    } else if cli.watch {
        deploy::watch(&file()?, &overrides)?;
    } else if cli.status {
        status(&load()?)?;
    } else if cli.log {
        let logger = Logger::new(&project::dir_of(&file()?));
        let path = if cli.activity {
            logger.activity
        } else {
            logger.events
        };
        let lines = if cli.last {
            log::last_deploy(&path)?
        } else {
            log::tail(&path, cli.lines)?
        };
        match lines {
            Some(lines) if lines.is_empty() => println!("no deploy in {}", path.display()),
            Some(lines) => lines.iter().for_each(|l| println!("{l}")),
            None => println!("no log yet at {}", path.display()),
        }
    } else if cli.install_service {
        service::install(&load()?, scope)?;
    } else if cli.uninstall_service {
        service::uninstall(&project::dir_of(&file()?), scope)?;
    } else if cli.global && (cli.interval.is_some() || cli.rollback.is_some()) {
        set_global(cli.interval, cli.rollback)?;
    } else {
        bail!(
            "nothing to do: pass an action such as --deploy or --watch, \
             or --global to set host defaults (see --help)"
        );
    }
    Ok(true)
}

fn set_global(interval: Option<Interval>, rollback: Option<bool>) -> Result<()> {
    let path = config::global_path()?;
    let mut cfg = Config::load(&path)?;
    if let Some(interval) = interval {
        cfg.interval = Some(interval);
        println!("interval set to {interval} (global)");
    }
    if let Some(on) = rollback {
        cfg.rollback = Some(on);
        println!("rollback {} (global)", on_off(on));
    }
    cfg.save(&path)
}

fn show_config(cia: Option<&Path>, overrides: &Overrides) -> Result<()> {
    let print = |label: String,
                 interval: Option<Interval>,
                 rollback: Option<bool>,
                 webhook: Option<&str>| {
        println!("{label}");
        println!(
            "  interval: {}",
            interval.map_or("-".into(), |i| i.to_string())
        );
        println!("  rollback: {}", rollback.map_or("-", on_off));
        if let Some(webhook) = webhook {
            println!("  webhook:  {webhook}");
        }
    };
    let global_path = config::global_path()?;
    let global = Config::load(&global_path)?;
    print(
        format!("global ({})", global_path.display()),
        global.interval,
        global.rollback,
        None,
    );
    let project = match project::locate(cia) {
        Ok(file) => Project::load(&file)?.with(overrides.clone()),
        Err(e) if cia.is_none() => {
            println!("({e:#})");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let c = &project.cia;
    print(
        format!("cia ({})", project.file.display()),
        c.interval,
        c.rollback,
        Some(c.webhook.as_deref().unwrap_or("-")),
    );
    if overrides.interval.is_some() || overrides.rollback.is_some() || overrides.webhook.is_some() {
        print(
            "flags".into(),
            overrides.interval,
            overrides.rollback,
            Some(overrides.webhook.as_deref().unwrap_or("-")),
        );
    }
    let eff = project.config()?;
    println!("effective");
    println!("  interval: {}", eff.interval);
    println!("  rollback: {}", on_off(eff.rollback));
    println!("  webhook:  {}", eff.webhook.as_deref().unwrap_or("off"));
    Ok(())
}

fn status(project: &Project) -> Result<()> {
    let repo = &project.repo;
    let eff = project.config()?;
    let state = State::load(&project.dir)?;
    let checked_out = match repo.branch()? {
        Some(b) if b == project.branch() => b,
        Some(b) => format!("{b} (not BRANCH; deploys will fail)"),
        None => "detached HEAD (deploys will fail)".into(),
    };
    let or_never = |v: &Option<String>| v.clone().unwrap_or_else(|| "never".into());
    println!("cia file:      {}", project.file.display());
    println!("local:         {}", repo.root.display());
    println!("branch:        {}", project.branch());
    println!("checked out:   {checked_out}");
    if let Some(url) = &project.cia.repository {
        println!("repository:    {url}");
    }
    println!("interval:      {}", eff.interval);
    if let Some(addr) = &eff.webhook {
        println!("webhook:       {addr} (while --watch runs)");
    }
    println!(
        "last run:      {}{}",
        or_never(&state.last_run_at),
        state
            .last_result
            .as_ref()
            .map_or(String::new(), |r| format!(" ({r})"))
    );
    if let Some(err) = &state.last_error {
        println!("last error:    {err}");
        println!("               run cia --log --last for the last deploy's output");
    }
    if let Some(sha) = &state.last_failed {
        println!(
            "rolled back:   from {}; it is skipped until upstream moves",
            short(sha)
        );
    }
    match (&state.last_deployed, &state.last_deployed_at) {
        (Some(sha), at) => println!("last deployed: {} at {}", short(sha), or_never(at)),
        (None, _) => println!("last deployed: never"),
    }
    let services = service::describe(&project.dir);
    if services.is_empty() {
        println!("service:       not installed");
    }
    for s in services {
        println!("service:       {s}");
    }
    let logger = Logger::new(&project.dir);
    println!("events log:    {}", logger.events.display());
    println!("activity log:  {}", logger.activity.display());
    Ok(())
}

/// Prints every problem it finds; returns false if any of them would make a deploy fail.
fn check(file: &Path, overrides: &Overrides) -> Result<bool> {
    let mut errors = 0;
    let mut error = |msg: String| {
        println!("error: {msg}");
        errors += 1;
    };
    if let Err(e) = Config::load(&config::global_path()?) {
        error(format!("{e:#}"));
    }
    let dir = project::dir_of(file);
    let text = fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", file.display()))?;
    let cia = match project::parse(file, &text) {
        Ok(cia) => cia,
        Err(e) => {
            error(format!("{e:#}"));
            return Ok(false);
        }
    };
    println!("{}: {} step(s)", file.display(), cia.steps.len());
    let repo = match project::open_local(&dir, cia.local.as_deref()) {
        Ok(repo) => repo,
        Err(e) => {
            error(format!("{e:#}"));
            return Ok(false);
        }
    };
    println!("local: {}", repo.root.display());
    for name in ["DEPLOY.cia", FILE_NAME] {
        if repo.tracks(&[name.into()])? {
            println!(
                "  warning: the clone tracks {name}, which cia ignores; steps come from {}",
                file.display()
            );
        }
    }

    let mut seen_dirs = Vec::new();
    let mut seen_changed = Vec::new();
    for step in &cia.steps {
        let step_dir = repo.root.join(&step.workdir);
        if !seen_dirs.contains(&step.workdir) {
            seen_dirs.push(step.workdir.clone());
            if !step_dir.is_dir() {
                let msg = format!(
                    "line {}: WORKDIR {} does not exist",
                    step.line,
                    step.workdir.display()
                );
                if step.workdir.is_absolute() {
                    println!("  warning: {msg}; fine if an earlier step creates it");
                } else {
                    error(msg);
                }
            }
        }
        if !step.changed.is_empty() && !seen_changed.contains(&step.changed) {
            seen_changed.push(step.changed.clone());
            if !repo.tracks(&step.changed)? {
                println!(
                    "  warning: line {}: CHANGED {} matches no tracked file",
                    step.line,
                    step.changed.join(" ")
                );
            }
        }
        let deploy::Action::Command { program, .. } = &step.action else {
            println!("  ok: {}", step.display());
            continue;
        };
        match find_program(&step_dir, program) {
            Program::Found => println!("  ok: {}{}", step.display(), step.details()),
            Program::MissingPath => println!(
                "  warning: {program} does not exist yet; fine if an earlier step creates it"
            ),
            Program::NotOnPath => error(format!(
                "line {}: {program} is not on PATH ({})",
                step.line,
                step.display()
            )),
        }
    }

    if let Ok(eff) = config::effective(&cia, overrides)
        && let Some(addr) = &eff.webhook
    {
        let secret = webhook::secret_path(&dir);
        if secret.exists() {
            println!("webhook: {addr}, secret {}", secret.display());
        } else {
            println!(
                "webhook: {addr}; --watch or --install-service will create the secret {}",
                secret.display()
            );
        }
    }
    let branch = cia.branch.as_deref().unwrap_or_default();
    match (cia.branch.is_some(), repo.branch()?) {
        (false, _) => error(format!("{FILE_NAME} needs a BRANCH line")),
        (true, Some(b)) if b == branch => println!("branch: {b}"),
        (true, Some(b)) => error(format!(
            "{} is on {b}, but BRANCH is {branch}",
            repo.root.display()
        )),
        (true, None) => error(format!(
            "{} has a detached HEAD, but BRANCH is {branch}",
            repo.root.display()
        )),
    }
    if let Some(expected) = &cia.repository {
        match repo.origin_url()? {
            Some(url) if project::same_repository(&url, expected) => {
                println!("repository: {url}")
            }
            Some(url) => error(format!("origin is {url}, but REPOSITORY is {expected}")),
            None => error(format!("{} has no origin remote", repo.root.display())),
        }
    }
    let services = service::describe(&dir);
    for s in &services {
        println!("service: {s}");
    }
    if !services.is_empty() {
        println!(
            "note: the service uses the PATH captured at install, which may differ from this shell's"
        );
    }
    if errors > 0 {
        println!("{errors} error(s)");
    }
    Ok(errors == 0)
}

enum Program {
    Found,
    MissingPath,
    NotOnPath,
}

/// A program given as a path is resolved from the step's working directory.
fn find_program(dir: &Path, program: &str) -> Program {
    let executable = |p: &Path| {
        fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return if executable(&dir.join(program)) {
            Program::Found
        } else {
            Program::MissingPath
        };
    }
    let path = env::var_os("PATH").unwrap_or_default();
    if env::split_paths(&path).any(|dir| executable(&dir.join(program))) {
        Program::Found
    } else {
        Program::NotOnPath
    }
}

fn parse_webhook(s: &str) -> Result<String, String> {
    if s == "off" || s.parse::<std::net::SocketAddr>().is_ok() {
        Ok(s.to_string())
    } else {
        Err("expected an address like 127.0.0.1:9000, or off".into())
    }
}

fn parse_on_off(s: &str) -> Result<bool, String> {
    match s {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err("expected on or off".into()),
    }
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}
