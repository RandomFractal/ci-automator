# `CIA` reference

A `CIA` file tells cia which clone to deploy, which branch to watch, and what `cia --deploy` runs after it pulls a new commit. It lives on the deploy host, not in the repo. The suggested layout is one folder per project, so one host can deploy several:

```
~/.cia/
  rto-track/CIA
  blog/CIA
```

It reads like a Dockerfile: one instruction per line. Project settings come first; after them, every step setting applies to the steps below it until it is changed.

```dockerfile
# ~/.cia/app/CIA
LOCAL ~/src/app
BRANCH main
REPOSITORY https://github.com/me/app

TIMEOUT 20m
WORKDIR prototype
RUN podman build -t localhost/app:latest .
WORKDIR .
RUN systemctl --user restart app
```

Pass the file, or its folder, to every command: `cia ~/.cia/app --deploy`. Run from the folder that holds it, `cia --deploy` finds `./CIA` on its own. `cia --check` checks the file and the clone, and `cia --deploy --dry-run` shows which steps the next deploy would run or skip.

State, logs, the webhook secret and `deployed.CIA` (see [Rollback](#rollback)) are kept next to the file.

## File rules

- Each line holds one instruction: a keyword, then its arguments. Keywords are uppercase.
- A line that starts with `#` is a comment. A `#` at the start of a word also starts a comment, so `RUN make  # build it` runs `make`. Quote it to pass a literal `#`.
- A `\` at the end of a line joins it to the next line.
- Blank lines are ignored.
- Arguments are split like a shell command: quotes group words, and `\` escapes a character.
- **Nothing runs through a shell.** `$VAR`, `*`, `|`, `&&`, `;` and `>` are passed to the program as plain text. To use shell features, run a shell: `RUN sh -c 'make 2>&1 | tee build.log'`.
- Steps run in file order and stop at the first failure.
- An error names its line number, and every error in the file is reported at once.
- A file with no steps deploys nothing.

## Project settings

Each appears at most once, before the first step.

| Setting | Required | Meaning |
|---|---|---|
| `LOCAL <path>` | yes | The top folder of the clone to deploy. `~` is your home folder; a relative path starts from the folder holding `CIA`. |
| `BRANCH <name>` | yes | The branch to deploy. cia fetches it from `origin` and refuses to run if the clone has another branch checked out. |
| `REPOSITORY <url>` | no | The repo `origin` must point at. `https://github.com/o/r`, `git@github.com:o/r.git` and `ssh://git@github.com/o/r` all count as the same. |
| `INTERVAL <interval>` | no | How often `--watch` checks. Default `5m`, or the global config's. |
| `ROLLBACK on\|off` | no | Roll back when a deploy fails. Default `on`, or the global config's. |
| `WEBHOOK <addr>` | no | Also deploy on signed GitHub push webhooks received on this address, e.g. `127.0.0.1:9000`. See [Webhooks](../README.md#webhooks). |

`--interval`, `--rollback` and `--webhook` override `INTERVAL`, `ROLLBACK` and `WEBHOOK` for one run, for example `cia --watch --interval 30s`.

When `BRANCH` changes, the next deploy runs every step, and if it fails, cia doesn't roll back to the old branch's commit. Check out the new branch in the clone first.

## Steps

### `RUN <command>`

Runs a command. A non-zero exit, or running past the timeout, fails the step and the deploy.

```dockerfile
RUN make build
RUN migrate --to "v2 final"
```

### `TRY <command>`

Runs a command like `RUN`, but a failure is logged and the deploy carries on. `RETRY` does not apply to it. Use it for cleanup that may have nothing to do.

```dockerfile
TRY podman rm -f old-app
```

### `SLEEP <interval>`

Waits before the next step. The wait isn't counted against any step's timeout. `WHEN` and `CHANGED` apply to it the same way they apply to a step.

```dockerfile
RUN systemctl --user restart app
SLEEP 5s
RUN uv run fast status
```

## Settings

Each setting applies to every step below it, until the same setting appears again.

| Setting | Default | Clear with |
|---|---|---|
| `RUNNER <prefix>` | none | `RUNNER none` |
| `TIMEOUT <interval>` | `10m` | `TIMEOUT 10m` |
| `WORKDIR <path>` | clone's top folder | `WORKDIR .` |
| `ENV KEY=value ...` | none | (adds only) |
| `RETRY <n> every <interval>` | none | `RETRY none` |
| `WHEN deploy\|rollback\|always` | `always` | `WHEN always` |
| `CHANGED <path> ...` | any change | `CHANGED any` |

An interval is a number followed by `s`, `m`, `h` or `d`, for example `30s` or `20m`.

### `RUNNER <prefix>`

Puts the prefix in front of each `RUN` and `TRY` command below it. `RUNNER none` clears it.

```dockerfile
RUNNER uv run
RUN fast migrate          # runs: uv run fast migrate
RUN fast restart          # runs: uv run fast restart

RUNNER podman exec app
RUN sqlite3 /data/app.db ".backup /data/pre-deploy.db"

RUNNER none
RUN systemctl --user restart app
```

### `TIMEOUT <interval>`

How long each step below it may run before cia kills it, along with every process it started. With `RETRY`, the timeout applies to each try.

### `WORKDIR <path>`

Where the steps below it run. A relative path always starts from the top of the clone (`LOCAL`), not from the previous `WORKDIR`, so `WORKDIR .` returns to the top. An absolute path is allowed, for example `/srv/app`. A program given as a path, such as `./bin/app`, is found from the working directory. The step fails if the folder doesn't exist when it runs.

### `ENV KEY=value ...`

Adds environment variables for the steps below it. Each line adds to the earlier ones, and setting a name again replaces its value. One line can set several variables, and quotes allow spaces: `ENV MODE=production GREETING="hello there"`.

Values are stored in plain text in `CIA` and `deployed.CIA`, so keep secrets in the app's own config or env file, or in the cia service's environment. Names starting with `CIA_` are reserved.

### `RETRY <n> every <interval>`

Tries a failing step up to n times in total (at least 2), waiting the interval between tries. The step fails, and the deploy rolls back, only when the last try fails. `RETRY none` clears it.

```dockerfile
RUN systemctl --user restart app
RETRY 10 every 3s
RUN curl -fsS http://localhost:8000/health
RETRY none
```

Retry only helps when the command exits non-zero while the app isn't ready yet. If a check exits 0 with incomplete output, use `SLEEP` before it instead.

### `WHEN deploy|rollback|always`

Picks when the steps below it run.

- `deploy`: only in a normal deploy.
- `rollback`: only when cia re-runs the steps during a rollback.
- `always`: in both. This is the default.

```dockerfile
WHEN deploy
RUN sqlite3 /data/app.db ".backup /data/pre-deploy.db"

WHEN rollback
RUN sqlite3 /data/app.db ".restore /data/pre-deploy.db"

WHEN always
RUN systemctl --user restart app
```

Without `WHEN`, a rollback runs every step again, so the backup above would overwrite the pre-deploy copy with the data the failed deploy just changed.

### `CHANGED <path> ...`

The steps below it run only if a file matching one of the paths changed since the last successful deploy. `CHANGED any` clears it.

Paths are git pathspecs relative to the top of the clone: `relay/` matches everything under that folder, and `*.py` matches Python files at any depth.

```dockerfile
CHANGED web/ shared/
RUN systemctl --user restart app-web

CHANGED worker/ shared/
RUN systemctl --user restart app-worker

CHANGED any
RUN systemctl --user is-active app-web app-worker
```

The rules:

- It compares against the last **successful** deploy. If a deploy fails, the next one still sees the changes it carried.
- Every step runs on the first deploy, with `--force`, after `BRANCH` changes, and whenever `CIA` was edited since the last successful deploy.
- In a rollback, it compares the failed commit with the commit cia rolls back to, so only what the failed deploy touched is restored.
- If every step is skipped, the commit still counts as deployed.
- List shared code under every service that uses it, like `shared/` above. A missing path leaves a service running old code. `cia --check` warns about a path that matches no tracked file, and `--dry-run` shows which steps would be skipped.

## Variables cia sets

Every step gets these in its environment:

| Variable | Value |
|---|---|
| `CIA_COMMIT` | The full hash of the commit being deployed. In a rollback, the commit cia is rolling back to. |
| `CIA_PREVIOUS_COMMIT` | The commit being moved away from: the last successful deploy, or in a rollback, the commit that failed. Unset when there is none. |
| `CIA_ROLLBACK` | `1` during a rollback, otherwise `0`. |

They are environment variables only. A `RUN` line doesn't expand them, so use a script or `sh -c` to read them:

```dockerfile
RUN sh -c 'podman tag localhost/app:latest localhost/app:$CIA_COMMIT'
```

## Rollback

Each successful deploy saves a copy of `CIA` as `deployed.CIA` next to it. When a step fails, cia resets the clone to the last successful deploy and runs the steps from `deployed.CIA` in rollback mode, so the old commit gets the steps that last deployed it, even if `CIA` has been edited since. `WHEN` and `CHANGED` decide which of its steps run. Rollback restores the repo, not the app's data: use `WHEN rollback` steps to undo what a deploy changed outside the repo, such as a database. See [`--deploy`](../README.md#--deploy) for the rest.

## Examples

Each example starts with the project settings it needs, such as:

```dockerfile
LOCAL ~/src/app
BRANCH main
```

### A uv app

```dockerfile
RUNNER uv run
RUN fast migrate
RUN fast restart
SLEEP 5s
RUN fast status
```

### A podman service with a database backup

```dockerfile
TIMEOUT 20m

WHEN deploy
RUN podman exec app sqlite3 /data/app.db ".backup /data/pre-deploy.db"
WHEN rollback
RUN podman exec app sqlite3 /data/app.db ".restore /data/pre-deploy.db"
WHEN always

TRY podman image prune -f
RUN podman build -t localhost/app:latest prototype
TIMEOUT 1m
RUN systemctl --user restart app
RETRY 10 every 3s
RUN systemctl --user is-active app
```

### Two services in one repo

```dockerfile
TIMEOUT 20m

CHANGED prototype/ shared/
WORKDIR prototype
RUN podman build -t localhost/app-track:latest .
WORKDIR .
RUN systemctl --user restart app-track

CHANGED relay/ shared/
RUN systemctl --user restart app-relay

CHANGED any
TIMEOUT 1m
SLEEP 5s
RUN systemctl --user is-active app-track app-relay
```
