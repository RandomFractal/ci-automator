# `DEPLOY.cia` reference

`DEPLOY.cia` sits at the root of the repo and lists what `cia --deploy` runs after it pulls a new commit. It reads like a Dockerfile: one instruction per line, and every setting applies to the steps below it until it is changed.

```dockerfile
# Deploy a podman service
TIMEOUT 20m
WORKDIR prototype
RUN podman build -t localhost/app:latest .
WORKDIR .
RUN systemctl --user restart app
```

Run `cia --check` in the clone to check the file, or `cia --check path/to/DEPLOY.cia` to check a file before committing it. `cia --deploy --dry-run` shows which steps the next deploy would run or skip.

## File rules

- Each line holds one instruction: a keyword, then its arguments. Keywords are uppercase.
- A line that starts with `#` is a comment. A `#` at the start of a word also starts a comment, so `RUN make  # build it` runs `make`. Quote it to pass a literal `#`.
- A `\` at the end of a line joins it to the next line.
- Blank lines are ignored.
- Arguments are split like a shell command: quotes group words, and `\` escapes a character.
- **Nothing runs through a shell.** `$VAR`, `*`, `|`, `&&`, `;` and `>` are passed to the program as plain text. To use shell features, run a shell: `RUN sh -c 'make 2>&1 | tee build.log'`.
- Steps run in file order and stop at the first failure.
- An error names its line number, and every error in the file is reported at once.
- An empty file, or one with no steps, deploys nothing.

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
| `WORKDIR <path>` | repo root | `WORKDIR .` |
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

Where the steps below it run. A relative path always starts from the repo root, not from the previous `WORKDIR`, so `WORKDIR .` returns to the root. An absolute path is allowed, for example `/srv/app`. A program given as a path, such as `./bin/app`, is found from the working directory. The step fails if the folder doesn't exist when it runs.

### `ENV KEY=value ...`

Adds environment variables for the steps below it. Each line adds to the earlier ones, and setting a name again replaces its value. One line can set several variables, and quotes allow spaces: `ENV MODE=production GREETING="hello there"`.

Values are committed to the repo, so don't put secrets here. Keep secrets in the app's own config or env file on the host, or in the cia service's environment. Names starting with `CIA_` are reserved.

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
- `rollback`: only when cia re-runs this file during a rollback.
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

Paths are git pathspecs relative to the repo root: `relay/` matches everything under that folder, and `*.py` matches Python files at any depth.

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
- Every step runs on the first deploy, with `--force`, and whenever `DEPLOY.cia` itself changed.
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

When a step fails, cia resets the clone to the last successful deploy and runs that commit's `DEPLOY.cia` in rollback mode. `WHEN` and `CHANGED` decide which of its steps run. Rollback restores the repo, not the app's data: use `WHEN rollback` steps to undo what a deploy changed outside the repo, such as a database. See [`--deploy`](../README.md#--deploy) for the rest.

## Examples

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
