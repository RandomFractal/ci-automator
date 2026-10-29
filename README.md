# ci-automator

CI Automation Service - for when Actions isn't available - Built in Rust

`cia` runs CI automations on a Linux host when GitHub Actions, or an equivalent, isn't available. It ships as one static binary and works on RHEL 7+, Fedora and ublue images such as Bazzite, Aurora and ucore. The first automation is `--deploy`: it pulls target branches and runs the steps in `DEPLOY.cia`.

## Install

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
install -m 755 target/x86_64-unknown-linux-musl/release/cia ~/.local/bin/
```

The musl build is fully static, so it does not depend on the host's glibc (RHEL 7 ships 2.17). On ostree-based systems, put it in `~/.local/bin` or `/usr/local/bin`; no layering is needed. The host needs `git`, plus `uv` if `DEPLOY.cia` uses `[uv]`.

## Usage

Run these inside the repo clone on the deploy host.

```bash
cia --add-branch main        # deploy when the clone is on main
cia --interval 5m            # how often --watch checks: NN followed by s, m, h or d
cia --install-service        # start a systemd service that runs --watch for this repo
```

| Command | Description |
|---|---|
| `--add-branch <name>` / `--rm-branch <name>` / `--reset-branches` | Manage target branches |
| `--interval <NN{s\|m\|h\|d}>` | Set how often `--watch` runs (default `5m`) |
| `--show-config` | Show the global, repo and effective config |
| `--deploy [--force] [--dry-run]` | Run one deploy check |
| `--watch` | Run `--deploy` every interval, re-reading the config each time |
| `--status` | Show the last run, last deploy, any error, and service state |
| `--log [-n N] [--activity]` | Print the end of the events log, or the activity log |
| `--install-service` / `--uninstall-service` | Manage this repo's systemd service |

`--global` applies config commands to the global config, and service commands to a system-wide service.

## Config

- **Repo:** `.git/cia/config.toml`. It is per clone and never committed, so each host chooses its own branches.
- **Global:** `$XDG_CONFIG_HOME/cia/config.toml`, by default `~/.config/cia/config.toml`.

Branches from both files are combined, and a repo interval overrides the global one.

## `--deploy`

1. If the current branch is not a target branch, or HEAD is detached, it does nothing.
2. It fetches, then pulls with `--ff-only` if upstream has new commits. It refuses to pull if the branch has diverged, has local commits, or has uncommitted changes to tracked files.
3. It reads `DEPLOY.cia` from the repo root and runs its steps in order from the root, stopping at the first failure.

Steps run only when HEAD differs from the last commit deployed successfully. A failed deploy is therefore retried on the next run. `--force` runs the steps anyway.

The first run in a clone that pulls nothing records HEAD as the deployed baseline without running anything. That way, installing the service does not restart an app that is already current.

Runs take a lock, so a manual `--deploy` cannot overlap the service.

## `DEPLOY.cia`

```toml
[uv]
run = ['fast restart']   # runs: uv run fast restart
timeout = '10m'          # optional, per step; default 10m
```

Each entry is split like a shell command, so quotes group words, and it runs directly, never through a shell. When a step exceeds its timeout, cia kills it and every process it started. Unknown sections or keys are an error.

A step may leave a server running in the background. cia stops recording the server's output a couple of seconds after the step exits.

## Logs

Logs are kept in `.git/cia/`:

| File | Contents | Rotation |
|---|---|---|
| `activity.log` | Every run, including routine checks that do nothing, with one-line summaries of events | 256 KB, 1 backup |
| `events.log` | Deploys with full step output, errors, timeouts, refused pulls, config changes and service changes | 10 MB, 5 backups |

An error that repeats unchanged on later runs is recorded in `events.log` only the first time. Changes to the global config are printed but not logged.

## Service

`cia --install-service` writes the unit `~/.config/systemd/user/cia-<repo-path>.service`, enables and starts it, and turns on linger so it keeps running after you log out. The unit captures your current `PATH`, so `uv` in `~/.local/bin` is found. Install again after moving the `cia` binary or changing your `PATH`.

`sudo cia --install-service --global` writes the unit under `/etc/systemd/system/` instead. It runs as the user who owns the repo, because git refuses to operate on repos owned by another user.

Both units use `KillMode=process`, so stopping or restarting cia does not stop processes that deploy steps started.

```bash
journalctl --user -u cia-home-me-app.service -f
```

Git commands in the service cannot prompt for anything. Use a deploy key without a passphrase or stored HTTPS credentials for the remote.
