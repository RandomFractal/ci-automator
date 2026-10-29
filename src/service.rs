use std::{env, fmt, fs, os::unix::fs::MetadataExt, path::PathBuf, process::Command};

use anyhow::{Context, Result, bail};

use crate::{config, git::Repo, log::Logger};

#[derive(Clone, Copy)]
pub enum Scope {
    User,
    System,
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Scope::User => "user",
            Scope::System => "system",
        })
    }
}

pub fn install(repo: &Repo, scope: Scope) -> Result<()> {
    require_root(scope)?;
    let name = unit_name(repo);
    let path = unit_dir(scope)?.join(&name);
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    fs::write(&path, render_unit(repo, scope)?)
        .with_context(|| format!("cannot write {}", path.display()))?;
    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", &name])?;
    systemctl(scope, &["restart", &name])?;
    if let Scope::User = scope {
        enable_linger();
    }
    Logger::new(repo).event(&format!("installed {scope} service {name}"));
    println!("unit file: {}", path.display());
    println!(
        "follow it with: journalctl {}-u {name} -f",
        journal_flag(scope)
    );
    Ok(())
}

pub fn uninstall(repo: &Repo, scope: Scope) -> Result<()> {
    require_root(scope)?;
    let name = unit_name(repo);
    let path = unit_dir(scope)?.join(&name);
    if !path.exists() {
        println!("no {scope} service is installed for this repo");
        return Ok(());
    }
    systemctl(scope, &["stop", &name])?;
    systemctl(scope, &["disable", &name])?;
    fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    systemctl(scope, &["daemon-reload"])?;
    Logger::new(repo).event(&format!("uninstalled {scope} service {name}"));
    Ok(())
}

/// Describes each installed service for this repo, e.g. "user cia-home-x-app.service: active".
pub fn describe(repo: &Repo) -> Vec<String> {
    let name = unit_name(repo);
    [Scope::User, Scope::System]
        .into_iter()
        .filter(|&scope| unit_dir(scope).is_ok_and(|dir| dir.join(&name).exists()))
        .map(|scope| {
            let state = systemctl_command(scope)
                .args(["is-active", &name])
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .unwrap_or_else(|_| "unknown".into());
            format!("{scope} {name}: {state}")
        })
        .collect()
}

fn unit_name(repo: &Repo) -> String {
    let path = repo.root.to_string_lossy();
    let slug: String = path
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("cia-{}.service", slug.trim_matches('-'))
}

fn unit_dir(scope: Scope) -> Result<PathBuf> {
    Ok(match scope {
        Scope::User => config::xdg_config_home()?.join("systemd/user"),
        Scope::System => PathBuf::from("/etc/systemd/system"),
    })
}

fn render_unit(repo: &Repo, scope: Scope) -> Result<String> {
    let exe = env::current_exe().context("cannot locate the cia binary")?;
    let root = repo.root.to_string_lossy();
    // The service manager's PATH rarely includes ~/.local/bin, where uv usually lives.
    let path = env::var("PATH").unwrap_or_default();
    let (unit_extra, service_extra, wanted_by) = match scope {
        Scope::User => (String::new(), String::new(), "default.target"),
        Scope::System => (
            "After=network-online.target\nWants=network-online.target\n".to_string(),
            // Run as the repo owner: git refuses repos owned by someone else.
            format!("User={}\n", fs::metadata(&repo.root)?.uid()),
            "multi-user.target",
        ),
    };
    Ok(format!(
        "\
[Unit]
Description=cia watcher for {root}
{unit_extra}
[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart=\"{exe}\" --watch
Environment=\"PATH={path}\"
{service_extra}Restart=on-failure
RestartSec=30
# Processes started by deploy steps belong to this service; stopping cia must not stop them.
KillMode=process

[Install]
WantedBy={wanted_by}
",
        root = escape_specifiers(&root),
        workdir = escape_specifiers(&root),
        exe = escape_quoted(&exe.to_string_lossy()),
        path = escape_quoted(&path),
    ))
}

fn escape_specifiers(s: &str) -> String {
    s.replace('%', "%%")
}

fn escape_quoted(s: &str) -> String {
    escape_specifiers(&s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn require_root(scope: Scope) -> Result<()> {
    if let Scope::System = scope
        && unsafe { libc::geteuid() } != 0
    {
        bail!("installing or removing the system service (--global) must be done as root");
    }
    Ok(())
}

fn systemctl_command(scope: Scope) -> Command {
    let mut cmd = Command::new("systemctl");
    if let Scope::User = scope {
        cmd.arg("--user");
    }
    cmd
}

fn systemctl(scope: Scope, args: &[&str]) -> Result<()> {
    let status = systemctl_command(scope)
        .args(args)
        .status()
        .context("failed to run systemctl; is systemd available?")?;
    if !status.success() {
        bail!("systemctl {} failed", args.join(" "));
    }
    Ok(())
}

fn journal_flag(scope: Scope) -> &'static str {
    match scope {
        Scope::User => "--user ",
        Scope::System => "",
    }
}

fn enable_linger() {
    let uid = unsafe { libc::getuid() }.to_string();
    let ok = Command::new("loginctl")
        .args(["enable-linger", &uid])
        .status()
        .is_ok_and(|s| s.success());
    if !ok {
        eprintln!(
            "warning: could not enable linger, so the service stops when you log out; \
             an admin can fix this with: loginctl enable-linger {uid}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(root: &str) -> Repo {
        Repo {
            root: root.into(),
            git_dir: format!("{root}/.git").into(),
        }
    }

    #[test]
    fn unit_name_is_readable() {
        assert_eq!(
            unit_name(&repo("/home/andrew/apps/my.repo")),
            "cia-home-andrew-apps-my-repo.service"
        );
    }

    #[test]
    fn renders_user_and_system_units() {
        let dir = tempfile::tempdir().unwrap();
        let r = Repo {
            root: dir.path().into(),
            git_dir: dir.path().join(".git"),
        };
        let user = render_unit(&r, Scope::User).unwrap();
        assert!(user.contains(&format!("WorkingDirectory={}\n", dir.path().display())));
        assert!(user.contains("--watch\n"));
        assert!(user.contains("KillMode=process"));
        assert!(user.contains("WantedBy=default.target"));
        assert!(!user.contains("User="));
        let system = render_unit(&r, Scope::System).unwrap();
        assert!(system.contains("User="));
        assert!(system.contains("WantedBy=multi-user.target"));
        assert!(system.contains("After=network-online.target"));
    }

    #[test]
    fn escapes_systemd_values() {
        assert_eq!(escape_quoted(r#"/a "b"\%c"#), r#"/a \"b\"\\%%c"#);
    }
}
