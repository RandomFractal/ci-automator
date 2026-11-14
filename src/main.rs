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
    config::Config,
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
#[command(group(ArgGroup::new("action").required(true).args([
    "add_branch", "rm_branch", "reset_branches", "interval", "rollback", "webhook", "show_config",
    "check", "deploy", "watch", "status", "log", "install_service", "uninstall_service",
])))]
struct Cli {
    /// The CIA file, or a directory holding one (default: ./CIA)
    #[arg(value_name = "CIA")]
    cia: Option<PathBuf>,

    /// Add a branch that automations apply to
    #[arg(long, value_name = "BRANCH")]
    add_branch: Option<String>,

    /// Remove a branch that automations apply to
    #[arg(long, value_name = "BRANCH")]
    rm_branch: Option<String>,

    /// Remove all branches that automations apply to
    #[arg(long)]
    reset_branches: bool,

    /// How often --watch runs automations, e.g. 30s, 5m, 2h, 1d
    #[arg(long, value_name = "NN{s|m|h|d}", value_parser = interval::parse)]
    interval: Option<Interval>,

    /// Roll back to the last good commit when a deploy fails (default on)
    #[arg(long, value_name = "on|off", value_parser = parse_on_off)]
    rollback: Option<bool>,

    /// Listen for GitHub push webhooks on ADDR (e.g. 127.0.0.1:9000) in --watch, or "off"
    #[arg(long, value_name = "ADDR|off", conflicts_with = "global")]
    webhook: Option<String>,

    /// Show the global, repo and effective config
    #[arg(long)]
    show_config: bool,

    /// Check the config and the CIA file, and that each step can run
    #[arg(long)]
    check: bool,

    /// Pull the current branch if it is a target and run the CIA steps on changes
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

    /// Use the global config, or a system-wide service, instead of the repo's or a user service
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
    let file = || project::locate(cli.cia.as_deref());
    let load = || Project::load(&file()?);
    if let Some(branch) = &cli.add_branch {
        add_branch(cli.cia.as_deref(), branch, cli.global)?;
    } else if let Some(branch) = &cli.rm_branch {
        rm_branch(cli.cia.as_deref(), branch, cli.global)?;
    } else if cli.reset_branches {
        edit_config(cli.cia.as_deref(), cli.global, |cfg| {
            cfg.branches.clear();
            Some("removed all target branches".into())
        })?;
    } else if let Some(interval) = cli.interval {
        edit_config(cli.cia.as_deref(), cli.global, |cfg| {
            cfg.interval = Some(interval);
            Some(format!("interval set to {interval}"))
        })?;
    } else if let Some(on) = cli.rollback {
        edit_config(cli.cia.as_deref(), cli.global, |cfg| {
            cfg.rollback = Some(on);
            Some(format!("rollback {}", on_off(on)))
        })?;
    } else if let Some(addr) = &cli.webhook {
        set_webhook(cli.cia.as_deref(), addr)?;
    } else if cli.show_config {
        show_config(cli.cia.as_deref())?;
    } else if cli.check {
        return check(&file()?);
    } else if cli.deploy {
        let opts = deploy::Options {
            force: cli.force,
            dry_run: cli.dry_run,
        };
        return deploy::deploy(&load()?, opts);
    } else if cli.watch {
        deploy::watch(&file()?)?;
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
    }
    Ok(true)
}

fn add_branch(cia: Option<&Path>, branch: &str, global: bool) -> Result<()> {
    if !git::is_valid_branch_name(branch)? {
        bail!("'{branch}' is not a valid branch name");
    }
    edit_config(cia, global, |cfg| {
        if cfg.branches.iter().any(|b| b == branch) {
            println!("{branch} is already a target branch");
            return None;
        }
        cfg.branches.push(branch.to_string());
        Some(format!("added target branch {branch}"))
    })
}

fn rm_branch(cia: Option<&Path>, branch: &str, global: bool) -> Result<()> {
    let set_globally = !global
        && Config::load(&config::global_path()?)?
            .branches
            .iter()
            .any(|b| b == branch);
    edit_config(cia, global, |cfg| {
        let before = cfg.branches.len();
        cfg.branches.retain(|b| b != branch);
        if cfg.branches.len() < before {
            return Some(format!("removed target branch {branch}"));
        }
        if set_globally {
            println!("{branch} is set in the global config; use --global to remove it");
        } else {
            println!("{branch} is not a target branch");
        }
        None
    })
}

/// Applies `change` and saves; it returns the change description, or None for no change.
/// Repo-level changes go to the events log. Global ones have no repo to log to.
fn edit_config(
    cia: Option<&Path>,
    global: bool,
    change: impl FnOnce(&mut Config) -> Option<String>,
) -> Result<()> {
    let project = if global {
        None
    } else {
        Some(Project::load(&project::locate(cia)?)?)
    };
    let path = match &project {
        Some(p) => config::repo_path(&p.repo),
        None => config::global_path()?,
    };
    let mut cfg = Config::load(&path)?;
    let Some(msg) = change(&mut cfg) else {
        return Ok(());
    };
    cfg.save(&path)?;
    match &project {
        Some(p) => Logger::new(&p.dir).event(&format!("config: {msg}")),
        None => println!("{msg} (global)"),
    }
    Ok(())
}

fn set_webhook(cia: Option<&Path>, addr: &str) -> Result<()> {
    if addr == "off" {
        return edit_config(cia, false, |cfg| {
            cfg.webhook.take().map(|_| "webhook off".to_string())
        });
    }
    if addr.parse::<std::net::SocketAddr>().is_err() {
        bail!("'{addr}' is not an address like 127.0.0.1:9000");
    }
    edit_config(cia, false, |cfg| {
        cfg.webhook = Some(addr.to_string());
        Some(format!("webhook on {addr}"))
    })?;
    let dir = project::dir_of(&project::locate(cia)?);
    let (secret, created) = webhook::ensure_secret(&dir)?;
    if created {
        println!(
            "created the webhook secret {}",
            webhook::secret_path(&dir).display()
        );
    }
    println!(
        "In GitHub, add a webhook under Settings > Webhooks with content type application/json, \
         the push event, and this secret:\n  {secret}\n\
         Point it at a public URL that forwards to {addr}, such as a reverse proxy or tunnel. \
         Restart --watch or the service to start listening."
    );
    Ok(())
}

fn show_config(cia: Option<&Path>) -> Result<()> {
    let print = |label: &str, path: &std::path::Path, cfg: &Config| {
        println!("{label} ({})", path.display());
        println!("  branches: {}", list(&cfg.branches));
        println!(
            "  interval: {}",
            cfg.interval.map_or("-".into(), |i| i.to_string())
        );
        println!("  rollback: {}", cfg.rollback.map_or("-", on_off));
        println!("  webhook:  {}", cfg.webhook.as_deref().unwrap_or("-"));
    };
    let global_path = config::global_path()?;
    print("global", &global_path, &Config::load(&global_path)?);
    let project = match project::locate(cia) {
        Ok(file) => Project::load(&file)?,
        Err(e) if cia.is_none() => {
            println!("({e:#})");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let repo_path = config::repo_path(&project.repo);
    print("repo", &repo_path, &Config::load(&repo_path)?);
    let eff = config::effective(&project.repo)?;
    println!("effective");
    println!("  branches: {}", list(&eff.branches));
    println!("  interval: {}", eff.interval);
    println!("  rollback: {}", on_off(eff.rollback));
    println!("  webhook:  {}", eff.webhook.as_deref().unwrap_or("off"));
    Ok(())
}

fn status(project: &Project) -> Result<()> {
    let repo = &project.repo;
    let eff = config::effective(repo)?;
    let state = State::load(&project.dir)?;
    let branch = match repo.branch()? {
        Some(b) if eff.branches.contains(&b) => format!("{b} (target)"),
        Some(b) => format!("{b} (not a target)"),
        None => "detached HEAD".into(),
    };
    let or_never = |v: &Option<String>| v.clone().unwrap_or_else(|| "never".into());
    println!("cia file:      {}", project.file.display());
    println!("local:         {}", repo.root.display());
    println!("branch:        {branch}");
    println!("targets:       {}", list(&eff.branches));
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
fn check(file: &Path) -> Result<bool> {
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
    if let Err(e) = Config::load(&config::repo_path(&repo)) {
        error(format!("{e:#}"));
    }
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

    if let Ok(eff) = config::effective(&repo) {
        if let Some(addr) = &eff.webhook {
            match webhook::load_secret(&dir) {
                Ok(_) => println!("webhook: {addr}"),
                Err(e) => error(format!("{e:#}; run cia --webhook {addr} to create it")),
            }
        }
        match repo.branch()? {
            None => println!("warning: HEAD is detached; deploys will do nothing"),
            Some(b) if !eff.branches.contains(&b) => {
                println!("warning: {b} is not a target branch; deploys will do nothing")
            }
            Some(b) => match repo.upstream()? {
                Some(u) => println!("branch: {b} (target), upstream {u}"),
                None => error(format!("{b} has no upstream branch")),
            },
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

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(", ")
    }
}
