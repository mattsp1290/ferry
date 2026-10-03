# ferry

Ferry keeps an explicit allowlist of GitHub repositories mirrored into a
Forgejo instance. It is a small Rust worker that polls GitHub, moves git and
Git LFS data itself, and reports metrics, logs, and traces to a Datadog Agent.

> **Ferry is an exact mirror, and GitHub is the source of truth.**
> On every repository it manages, ferry force-updates branches and tags to
> match GitHub and deletes branches and tags that GitHub no longer has.
> Commits pushed only to a mirrored branch on Forgejo are destroyed by the
> next sync. Do not use a ferry-managed Forgejo repository as a place to
> work.

## What it syncs

| Synced | Not synced |
|---|---|
| `refs/heads/*` and `refs/tags/*` | Pull request refs, wiki, issues, releases |
| Git LFS objects (per entry, on by default) | Repository settings other than the two below |
| Description and default branch | Anything from Forgejo back to GitHub |

Ferry creates a missing Forgejo repository as private, with Actions disabled
unless the entry asks for them. Ferry never deletes a Forgejo repository.

## Which repositories ferry will write to

Ferry writes only to Forgejo repositories that are named in the allowlist
**and** carry the topic `ferry-mirror`.

- Ferry adds the topic to a repository it creates, and to an existing
  repository that has no branches or tags.
- An existing repository with content and without the topic is refused. The
  entry reports `dest_unmanaged` until its allowlist entry sets
  `adopt = true`.
- **Stop switch:** remove the `ferry-mirror` topic from a repository in the
  Forgejo UI and ferry stops writing to it (`dest_unmanaged`). Add the topic
  back to resume.
- A Forgejo pull mirror is never written to (`dest_is_pull_mirror`).

## Configuration

One TOML file. `examples/ferry.toml` is a complete example.

```toml
[sync]
poll_interval_seconds = 300      # minimum 30
metadata_interval_seconds = 3600 # minimum 300; how often the description is re-read
max_concurrency = 2              # 1..=8 repositories in flight
git_timeout_seconds = 1800       # per git child process
cache_dir = "/var/lib/ferry/cache"

[github]
api_url = "https://api.github.com"
git_url = "https://github.com"

[forgejo]                        # required
url = "https://git.example.com"
username = "ferry"               # username git pairs with the token

[health]
listen = "0.0.0.0:8080"

[[repos]]
github = "example-owner/example-repo"   # required
forgejo = "example-owner/example-repo"  # required
lfs = true                              # default true
actions = false                         # default false
adopt = false                           # default false
```

Unknown keys are rejected. The file is read once at start; there is no reload.
`ferry check-config` reports every violation at once and does no network I/O.

Credentials come from files named by environment variables, never from the
config file:

| Variable | Required | Meaning |
|---|---|---|
| `FERRY_FORGEJO_TOKEN_FILE` | for `run` and `sync` | Forgejo token. Scopes: `write:repository`, plus `write:user` or `write:organization` for the destination owners. |
| `FERRY_GITHUB_TOKEN_FILE` | no | Read-only GitHub token. Unset, missing, or empty means unauthenticated access, which is enough for public repositories. |
| `FERRY_CONFIG` | no | Config path when `--config` is absent. Default `/etc/ferry/ferry.toml`. |

Telemetry uses the standard Datadog variables. Each signal is off when its
URL is unset.

| Variable | Default | Meaning |
|---|---|---|
| `DD_DOGSTATSD_URL` | unset | `udp://host:8125` or `unix:///path/dsd.socket` |
| `DD_TRACE_AGENT_URL` | unset | `http://host:8126` or `unix:///path/apm.socket` |
| `DD_SERVICE` / `DD_ENV` / `DD_VERSION` | `ferry` / unset / crate version | Unified service tags |
| `FERRY_LOG_FORMAT` | `json` | `json` or `text` |
| `FERRY_LOG_LEVEL` | `info` | A `tracing` filter directive |

## Commands

```sh
ferry run --config ferry.toml                    # poll forever; health server on [health].listen
ferry sync --once --config ferry.toml            # one pass over every entry, then exit
ferry sync --once --config ferry.toml --repo example-owner/example-repo
ferry check-config --config ferry.toml
ferry --version
```

Exit codes: `0` success, `1` runtime failure (including any entry that did
not end `synced`, `noop`, or `empty` in `sync --once`), `2` configuration or
usage error.

`run` serves `GET /healthz` (the scheduler loop is alive) and `GET /readyz`
(startup checks passed, including one successful Forgejo API call).

## Running locally

Requirements: `git`, and `git-lfs` when any entry has `lfs = true`.

```sh
cargo build --release
# Put the Forgejo token in a 0600 file outside the repository, then:
FERRY_FORGEJO_TOKEN_FILE=~/.config/ferry/forgejo-token \
FERRY_LOG_FORMAT=text \
  target/release/ferry sync --once --config my-ferry.toml
```

Set `cache_dir` in a local config to a directory you own. The cache holds
bare repositories and LFS objects. It is safe to delete while ferry is
stopped; the next pass re-fetches.

## Results and error kinds

Each pass over a repository ends with one result: `synced`, `noop`, `empty`
(GitHub and Forgejo both have no branches), or `error` with one of these
kinds:

| `error_kind` | Meaning |
|---|---|
| `source_missing`, `source_auth` | GitHub repository not found or not readable with the token. A renamed repository reports `source_missing`; edit the allowlist. |
| `source_empty` | GitHub reports no branches while Forgejo has refs. Ferry refuses to prune everything. |
| `dest_unmanaged` | The Forgejo repository has content and no `ferry-mirror` topic. |
| `dest_is_pull_mirror` | The Forgejo repository is a Forgejo pull mirror. |
| `dest_auth`, `dest_rejected` | Forgejo refused the token or the push. |
| `lfs` | LFS fetch or push failed. No ref was pushed. |
| `metadata` | Updating the default branch or description on Forgejo failed. |
| `verify_mismatch` | After the push, Forgejo's refs did not match what was fetched. |
| `timeout`, `network`, `rate_limited`, `internal` | As named. |

A failing entry backs off exponentially from the poll interval up to one hour
and never blocks other entries.

## Tests

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
FERRY_TEST_LFS=1 cargo test --test git_mirror   # LFS cases; needs git-lfs
```

`tests/live_acceptance.rs` runs against real GitHub and Forgejo and is
ignored by default; its header lists the environment it needs.

## Deployment

- `charts/ferry` is the Helm chart: one replica, a cache volume, no Service.
- `docker/` and `scripts/` build the runtime image.
- `datadog/` holds the monitors and the dashboard, applied with `pup`.

The design and its rationale are in
`.agents/plans/github-forgejo-sync-worker/`.
