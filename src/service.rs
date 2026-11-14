use std::{
    env, fmt, fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail};

use crate::{
    config,
    log::Logger,
    project::{self, Project},
    webhook,
};

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

pub fn install(project: &Project, scope: Scope) -> Result<()> {
    require_root(scope)?;
    if let Some(addr) = project.config()?.webhook {
        webhook::ensure_secret_logged(&project.dir, &Logger::new(&project.dir))?;
        println!(
            "webhook on {addr}; its secret is in {}",
            webhook::secret_path(&project.dir).display()
        );
    }
    let name = unit_name(&project.dir);
    let path = unit_dir(scope)?.join(&name);
    fs::create_dir_all(path.parent().unwrap_or(&path))?;
    fs::write(
        &path,
        render_unit(&project.file, &project.repo.root, scope)?,
    )
    .with_context(|| format!("cannot write {}", path.display()))?;
    systemctl(scope, &["daemon-reload"])?;
    systemctl(scope, &["enable", &name])?;
    systemctl(scope, &["restart", &name])?;
    if let Scope::User = scope {
        enable_linger();
    }
    Logger::new(&project.dir).event(&format!("installed {scope} service {name}"));
    println!("unit file: {}", path.display());
    println!(
        "follow it with: journalctl {}-u {name} -f",
        journal_flag(scope)
    );
    Ok(())
}

pub fn uninstall(dir: &Path, scope: Scope) -> Result<()> {
    require_root(scope)?;
    let name = unit_name(dir);
    let path = unit_dir(scope)?.join(&name);
    if !path.exists() {
        println!("no {scope} service {name} is installed");
        return Ok(());
    }
    systemctl(scope, &["stop", &name])?;
    systemctl(scope, &["disable", &name])?;
    fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
    systemctl(scope, &["daemon-reload"])?;
    Logger::new(dir).event(&format!("uninstalled {scope} service {name}"));
    Ok(())
}

/// Describes each installed service for this CIA, e.g. "user cia-app.service: active".
pub fn describe(dir: &Path) -> Vec<String> {
    let name = unit_name(dir);
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

/// Named after the directory holding the CIA file, e.g. ~/.cia/rto-track -> cia-rto-track.service.
fn unit_name(dir: &Path) -> String {
    let name = dir
        .file_name()
        .map_or_else(|| dir.to_string_lossy(), |n| n.to_string_lossy());
    let slug: String = name
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

fn render_unit(file: &Path, clone: &Path, scope: Scope) -> Result<String> {
    let exe = env::current_exe().context("cannot locate the cia binary")?;
    let file_str = file.to_string_lossy();
    let dir = project::dir_of(file);
    // The service manager's PATH rarely includes ~/.local/bin, where uv usually lives.
    let path = env::var("PATH").unwrap_or_default();
    let (unit_extra, service_extra, wanted_by) = match scope {
        Scope::User => (String::new(), String::new(), "default.target"),
        Scope::System => (
            "After=network-online.target\nWants=network-online.target\n".to_string(),
            // Run as the clone's owner: git refuses repos owned by someone else.
            format!("User={}\n", fs::metadata(clone)?.uid()),
            "multi-user.target",
        ),
    };
    Ok(format!(
        "\
[Unit]
Description=cia watcher for {desc}
{unit_extra}
[Service]
Type=simple
WorkingDirectory={workdir}
ExecStart=\"{exe}\" \"{file}\" --watch
Environment=\"PATH={path}\"
{service_extra}Restart=on-failure
RestartSec=30
# Processes started by deploy steps belong to this service; stopping cia must not stop them.
KillMode=process

[Install]
WantedBy={wanted_by}
",
        desc = escape_specifiers(&file_str),
        workdir = escape_specifiers(&dir.to_string_lossy()),
        exe = escape_quoted(&exe.to_string_lossy()),
        file = escape_quoted(&file_str),
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

    #[test]
    fn unit_name_is_the_cia_directory() {
        assert_eq!(
            unit_name(Path::new("/home/andrew/.cia/rto.track")),
            "cia-rto-track.service"
        );
    }

    #[test]
    fn renders_user_and_system_units() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("CIA");
        let user = render_unit(&file, dir.path(), Scope::User).unwrap();
        assert!(user.contains(&format!("WorkingDirectory={}\n", dir.path().display())));
        assert!(user.contains(&format!("\" \"{}\" --watch\n", file.display())));
        assert!(user.contains("KillMode=process"));
        assert!(user.contains("WantedBy=default.target"));
        assert!(!user.contains("User="));
        let system = render_unit(&file, dir.path(), Scope::System).unwrap();
        assert!(system.contains("User="));
        assert!(system.contains("WantedBy=multi-user.target"));
        assert!(system.contains("After=network-online.target"));
    }

    #[test]
    fn escapes_systemd_values() {
        assert_eq!(escape_quoted(r#"/a "b"\%c"#), r#"/a \"b\"\\%%c"#);
    }
}
