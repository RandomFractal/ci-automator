mod config;
mod deploy;
mod git;
mod interval;
mod log;
mod service;
mod state;

use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{ArgGroup, Parser};

use crate::{
    config::Config,
    git::{Repo, short},
    interval::Interval,
    log::Logger,
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
    "add_branch", "rm_branch", "reset_branches", "interval", "show_config",
    "deploy", "watch", "status", "log", "install_service", "uninstall_service",
])))]
struct Cli {
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

    /// Show the global, repo and effective config
    #[arg(long)]
    show_config: bool,

    /// Pull the current branch if it is a target and run DEPLOY.cia on changes
    #[arg(long)]
    deploy: bool,

    /// Run --deploy every interval until stopped
    #[arg(long)]
    watch: bool,

    /// Show the last run, last deploy and service state for this repo
    #[arg(long)]
    status: bool,

    /// Print the end of the events log
    #[arg(long)]
    log: bool,

    /// Install and start a systemd service that runs --watch for this repo
    #[arg(long)]
    install_service: bool,

    /// Stop and remove this repo's systemd service
    #[arg(long)]
    uninstall_service: bool,

    /// Use the global config, or the system-wide service, instead of this repo's
    #[arg(long, conflicts_with_all = ["show_config", "deploy", "watch", "status", "log"])]
    global: bool,

    /// With --deploy: run DEPLOY.cia even if this commit is already deployed
    #[arg(long, requires = "deploy")]
    force: bool,

    /// With --deploy: show what would happen without pulling or running anything
    #[arg(long, requires = "deploy")]
    dry_run: bool,

    /// With --log: print the activity log, which includes routine checks
    #[arg(long, requires = "log")]
    activity: bool,

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
    if let Some(branch) = &cli.add_branch {
        add_branch(branch, cli.global)?;
    } else if let Some(branch) = &cli.rm_branch {
        rm_branch(branch, cli.global)?;
    } else if cli.reset_branches {
        edit_config(cli.global, |cfg| {
            cfg.branches.clear();
            Some("removed all target branches".into())
        })?;
    } else if let Some(interval) = cli.interval {
        edit_config(cli.global, |cfg| {
            cfg.interval = Some(interval);
            Some(format!("interval set to {interval}"))
        })?;
    } else if cli.show_config {
        show_config()?;
    } else if cli.deploy {
        let opts = deploy::Options {
            force: cli.force,
            dry_run: cli.dry_run,
        };
        return deploy::deploy(&Repo::discover()?, opts);
    } else if cli.watch {
        deploy::watch(&Repo::discover()?)?;
    } else if cli.status {
        status(&Repo::discover()?)?;
    } else if cli.log {
        let logger = Logger::new(&Repo::discover()?);
        let path = if cli.activity {
            logger.activity
        } else {
            logger.events
        };
        match log::tail(&path, cli.lines)? {
            Some(lines) => lines.iter().for_each(|l| println!("{l}")),
            None => println!("no log yet at {}", path.display()),
        }
    } else if cli.install_service {
        service::install(&Repo::discover()?, scope)?;
    } else if cli.uninstall_service {
        service::uninstall(&Repo::discover()?, scope)?;
    }
    Ok(true)
}

fn add_branch(branch: &str, global: bool) -> Result<()> {
    if !git::is_valid_branch_name(branch)? {
        bail!("'{branch}' is not a valid branch name");
    }
    edit_config(global, |cfg| {
        if cfg.branches.iter().any(|b| b == branch) {
            println!("{branch} is already a target branch");
            return None;
        }
        cfg.branches.push(branch.to_string());
        Some(format!("added target branch {branch}"))
    })
}

fn rm_branch(branch: &str, global: bool) -> Result<()> {
    let set_globally = !global
        && Config::load(&config::global_path()?)?
            .branches
            .iter()
            .any(|b| b == branch);
    edit_config(global, |cfg| {
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
fn edit_config(global: bool, change: impl FnOnce(&mut Config) -> Option<String>) -> Result<()> {
    let repo = if global {
        None
    } else {
        Some(Repo::discover()?)
    };
    let path = match &repo {
        Some(repo) => config::repo_path(repo),
        None => config::global_path()?,
    };
    let mut cfg = Config::load(&path)?;
    let Some(msg) = change(&mut cfg) else {
        return Ok(());
    };
    cfg.save(&path)?;
    match &repo {
        Some(repo) => Logger::new(repo).event(&format!("config: {msg}")),
        None => println!("{msg} (global)"),
    }
    Ok(())
}

fn show_config() -> Result<()> {
    let print = |label: &str, path: &std::path::Path, cfg: &Config| {
        println!("{label} ({})", path.display());
        println!("  branches: {}", list(&cfg.branches));
        println!(
            "  interval: {}",
            cfg.interval.map_or("-".into(), |i| i.to_string())
        );
    };
    let global_path = config::global_path()?;
    print("global", &global_path, &Config::load(&global_path)?);
    let Ok(repo) = Repo::discover() else {
        println!("(not inside a git repository)");
        return Ok(());
    };
    let repo_path = config::repo_path(&repo);
    print("repo", &repo_path, &Config::load(&repo_path)?);
    let eff = config::effective(&repo)?;
    println!("effective");
    println!("  branches: {}", list(&eff.branches));
    println!("  interval: {}", eff.interval);
    Ok(())
}

fn status(repo: &Repo) -> Result<()> {
    let eff = config::effective(repo)?;
    let state = State::load(repo)?;
    let branch = match repo.branch()? {
        Some(b) if eff.branches.contains(&b) => format!("{b} (target)"),
        Some(b) => format!("{b} (not a target)"),
        None => "detached HEAD".into(),
    };
    let or_never = |v: &Option<String>| v.clone().unwrap_or_else(|| "never".into());
    println!("repo:          {}", repo.root.display());
    println!("branch:        {branch}");
    println!("targets:       {}", list(&eff.branches));
    println!("interval:      {}", eff.interval);
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
    }
    match (&state.last_deployed, &state.last_deployed_at) {
        (Some(sha), at) => println!("last deployed: {} at {}", short(sha), or_never(at)),
        (None, _) => println!("last deployed: never"),
    }
    let services = service::describe(repo);
    if services.is_empty() {
        println!("service:       not installed");
    }
    for s in services {
        println!("service:       {s}");
    }
    let logger = Logger::new(repo);
    println!("events log:    {}", logger.events.display());
    println!("activity log:  {}", logger.activity.display());
    Ok(())
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "-".into()
    } else {
        items.join(", ")
    }
}
