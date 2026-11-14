# ci-automator

CI Automation Service - for when Actions isn't available - Built in Rust

`cia` runs CI automations on a Linux host when GitHub Actions, or an equivalent, isn't available. It ships as one static binary and works on RHEL 7+, Fedora and ublue images such as Bazzite, Aurora and ucore. The first automation is `--deploy`: it pulls a branch and runs the steps in a `CIA` file kept on the host, outside the repo.

## Install

Download `cia-x86_64-unknown-linux-musl` or `cia-aarch64-unknown-linux-musl` from the [latest release](https://github.com/RandomFractal/ci-automator/releases/latest), check it against `SHA256SUMS`, and install it as `cia`:

```bash
install -m 755 cia-x86_64-unknown-linux-musl ~/.local/bin/cia
```

To build from source instead, use `x86_64-unknown-linux-musl` or `aarch64-unknown-linux-musl` as the target:

```bash
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
install -m 755 target/x86_64-unknown-linux-musl/release/cia ~/.local/bin/
```

The musl build is fully static, so it does not depend on the host's glibc (RHEL 7 ships 2.17). On ostree-based systems, put it in `~/.local/bin` or `/usr/local/bin`; no layering is needed. The host needs `git`, plus the programs your steps run.

## Usage

Give each project a folder with a `CIA` file, for example `~/.cia/app/CIA`:

```dockerfile
LOCAL ~/src/app
BRANCH main

RUN podman build -t localhost/app:latest .
RUN systemctl --user restart app
```

Then run cia from that folder, or pass the folder or file: `cia ~/.cia/app --deploy`.

```bash
cd ~/.cia/app
cia --check                  # check the file, the clone and every step
cia --deploy --dry-run       # show what a deploy would do
cia --install-service        # start a systemd service that runs --watch
```

| Command | Description |
|---|---|
| `--show-config` | Show the global, CIA and effective settings |
| `--check` | Check the config, the `CIA` file and the clone, and that every step's program is on `PATH`; exits non-zero on errors |
| `--deploy [--force] [--dry-run]` | Run one deploy check |
| `--watch` | Run `--deploy` every interval, re-reading `CIA` each time |
| `--status` | Show the last run, last deploy, any error, and service state |
| `--log [-n N] [--activity] [--last]` | Print the end of the events log, or the activity log; `--last` prints only the last deploy |
| `--install-service` / `--uninstall-service` | Manage the systemd service for this `CIA` file |

These flags override a setting for one run: `--interval <NN{s|m|h|d}>` (`INTERVAL`), `--rollback <on|off>` (`ROLLBACK`) and `--webhook <ADDR|off>` (`WEBHOOK`), e.g. `cia --watch --interval 30s`.

`--global` with service commands manages a system-wide service.

## `CIA`

`CIA` names the clone (`LOCAL`) and the branch (`BRANCH`), optionally the repository, interval, rollback and webhook, and then lists the steps a deploy runs. The steps read like a Dockerfile: one instruction per line, and each setting applies to the steps below it.

```dockerfile
LOCAL ~/src/app
BRANCH main
REPOSITORY https://github.com/me/app
INTERVAL 2m

TIMEOUT 20m
RUN podman build -t localhost/app:latest prototype
RUN systemctl --user restart app

RUNNER uv run
RUN fast status
```

Steps run directly, never through a shell. Settings cover the prefix (`RUNNER`), time limit, working directory, environment, retries, deploy-only or rollback-only steps (`WHEN`), and steps that run only when certain paths changed (`CHANGED`). See the **[reference](docs/CIA.md)** for every instruction and complete examples. Run `cia --check` to see every problem at once.

## Config

Settings are chosen in this order: a flag for this run, then `CIA`, then the global config at `$XDG_CONFIG_HOME/cia/config.toml` (by default `~/.config/cia/config.toml`), then the defaults (`INTERVAL 5m`, `ROLLBACK on`). Set host-wide defaults with `cia --global --interval 10m` or `cia --global --rollback off`.

## `--deploy`

1. It reads `CIA`. If the file is invalid, it stops before touching the clone.
2. It checks that the clone has `BRANCH` checked out and, with `REPOSITORY`, that `origin` points there. Otherwise it fails without pulling.
3. It fetches `BRANCH` from `origin` and fast-forwards to it. It refuses if the branch has diverged, has local commits, or has uncommitted changes to tracked files.
4. It runs the steps in order, stopping at the first failure.

Steps run only when HEAD differs from the last commit deployed successfully. `--force` runs the steps anyway. `--dry-run` prints what a run would do, including every step, without pulling or running anything.

If a step fails or times out, cia rolls back: it resets the clone to the last commit that deployed successfully (`git reset --hard`) and runs the steps that deployed it (saved as `deployed.CIA`) again in rollback mode, where [`WHEN`](docs/CIA.md#when-deployrollbackalways) and [`CHANGED`](docs/CIA.md#changed-path-) pick which steps run. It then skips the failed commit until upstream moves past it or you run `--deploy --force`. Rollback restores the repo, not the app's data: anything a failed step changed outside the repo, such as a database migration, stays changed unless a `WHEN rollback` step undoes it, and changes steps made to tracked files are discarded. Untracked and ignored files are kept. With no successful deploy to go back to, after `BRANCH` changed, or with `ROLLBACK off`, a failed deploy stays on the new commit and is retried on the next run.

The first run of a `CIA` file that pulls nothing records HEAD as the deployed baseline without running anything. That way, installing the service does not restart an app that is already current.

Runs take a lock in the clone's `.git/cia/`, so a manual `--deploy` cannot overlap the service, even from another `CIA` file.

A step may leave a server running in the background. cia stops recording the server's output a couple of seconds after the step exits.

## Webhooks

By default `--watch` polls every interval. To deploy as soon as you push, add a `WEBHOOK` line to `CIA`:

```dockerfile
WEBHOOK 127.0.0.1:9000
```

`--install-service` or `--watch` then creates a random secret in `webhook-secret` next to `CIA` (readable only by you). In the GitHub repo, add a webhook under Settings > Webhooks with content type `application/json`, the push event only, and that secret. Restart `--watch` or the service after adding or changing `WEBHOOK`.

cia checks every request's `X-Hub-Signature-256` HMAC against the secret and ignores anything unsigned or wrongly signed. A valid push to `BRANCH` (and to `REPOSITORY`, when it is set) starts a deploy check right away; pushes to other branches are ignored. Polling carries on as a fallback, so a missed webhook only delays a deploy until the next interval.

cia speaks plain HTTP, so don't expose the port directly: bind it to `127.0.0.1` and put a reverse proxy with TLS or a tunnel (for example Tailscale Funnel or cloudflared) in front of it. `--webhook off` turns it off for one run.

## Logs

Logs are kept next to `CIA`:

| File | Contents | Rotation |
|---|---|---|
| `activity.log` | Every run, including routine checks that do nothing, with one-line summaries of events | 256 KB, 1 backup |
| `events.log` | Deploys with full step output, errors, timeouts, refused pulls and service changes | 10 MB, 5 backups |

An error that repeats unchanged on later runs is recorded in `events.log` only the first time.

## Service

`cia --install-service` writes the unit `~/.config/systemd/user/cia-<folder>.service`, named after the folder holding `CIA` (`cia-app.service` for `~/.cia/app/CIA`), enables and starts it, and turns on linger so it keeps running after you log out. The unit captures your current `PATH`, so `uv` in `~/.local/bin` is found. Install again after moving the `cia` binary or changing your `PATH`.

`sudo cia --install-service --global` writes the unit under `/etc/systemd/system/` instead. It runs as the user who owns the clone, because git refuses to operate on repos owned by another user.

Both units use `KillMode=process`, so stopping or restarting cia does not stop processes that deploy steps started.

```bash
journalctl --user -u cia-app.service -f
```

Git commands in the service cannot prompt for anything. Use a deploy key without a passphrase or stored HTTPS credentials for the remote.

## Releasing

Bump `version` in `Cargo.toml`, merge, then push a matching tag, e.g. `git tag v0.1.0 && git push origin v0.1.0`. The release workflow runs CI, then publishes both static binaries and `SHA256SUMS` to a GitHub release. It fails if the tag doesn't match `Cargo.toml`.
