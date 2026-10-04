# Contributor rules

Ferry mirrors an allowlist of GitHub repositories into Forgejo. The design, the
decisions behind it, and the work-package breakdown are in the implementation
plan. The plan is not part of this repository: it describes the deployment
environment, so the owner keeps it locally under `.agents/plans/` (ignored by
git). Ask the owner for it before changing behavior.

## Quality gate

Run all three before every commit. Each must exit 0.

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Additional checks for the areas they cover:

| Area | Command | Needs |
|---|---|---|
| Deployment skill | `.agents/skills/ferry-deploy/tests/run.sh` | workstation tools listed by the skill, `rg` |
| LFS mirroring | `FERRY_TEST_LFS=1 cargo test --test git_mirror` | `git-lfs` |
| Chart | `helm lint charts/ferry --set image.repository=x --set image.digest=sha256:0 && scripts/check-chart.sh` | `helm` |
| Cross-build | `scripts/build-binaries.sh` | `zig`, `cargo-zigbuild` |
| Live acceptance | `cargo test --test live_acceptance -- --ignored` | real tokens, LAN; see the test's header |
| Secret hygiene | `git grep -nE 'ghp_\|github_pat_\|-----BEGIN' -- ':!AGENTS.md'` returns nothing | — |

Without `FERRY_TEST_LFS=1` the LFS cases print a skip line instead of running.

## Secret handling

A token value must never appear in:

- process arguments, a URL, or `.git/config` (git gets credentials through
  askpass mode: `GIT_ASKPASS` points at the ferry binary itself);
- a log line, a span attribute, or a metric tag;
- git history, a test fixture, Helm values, or an image layer.

Rules that keep this true:

- Hold credentials in `config::Token`. Its `Debug` and `Display` print
  `[redacted]`. Call `expose()` only at the point of use.
- Refer to credentials by name: `FERRY_GITHUB_TOKEN_FILE`,
  `FERRY_FORGEJO_TOKEN_FILE`, Kubernetes Secret `ferry-credentials`.
- Git stderr is truncated to 4 KiB and has loaded token values replaced with
  `[redacted]` before it enters an error or a log.
- Test tokens are obviously fake strings. Never paste a real one.

## Git operations go through `src/git/`

No other module spawns `git` or `git-lfs`. `GitRunner` owns the child's
environment, process group, timeout, stderr redaction, and error
classification. Remote URLs are passed per command and never contain userinfo.
The cache repository stores no remote and no credential.

## Invariants

1. Ferry writes only to Forgejo repositories that are in the allowlist and
   carry the `ferry-mirror` topic.
2. Ferry never calls a Forgejo delete-repository endpoint. `src/forge/` must
   not contain a `DELETE` request; a test enforces this.
3. Ferry never prunes when the GitHub repository reports zero branches.
4. Ferry never writes to GitHub.
5. At most one sync runs per repository at a time. At most one replica runs.
6. The secret-handling rules above.

## Telemetry contract

- Metric names are constants in `src/telemetry/metrics.rs` (`METRIC_NAMES`).
  The files in `datadog/` query them; `tests/datadog_assets.rs` checks that.
- `SyncResult::as_str` and `ErrorKind::as_str` are metric tag values. Changing
  one is a telemetry contract change.
- The OpenTelemetry crate versions follow `datadog-opentelemetry`, not
  "latest": `datadog-opentelemetry 0.5.2` needs `opentelemetry 0.32.x`,
  `opentelemetry_sdk 0.32.x`, and `tracing-opentelemetry 0.33.x`.
  `cargo tree -d` must list no duplicate `opentelemetry` or `opentelemetry_sdk`.
- Every repository sync is one trace with root span `ferry.sync_repo`.
  Create spans with `tracing::info_span!`; span fields become Datadog tags.
  Never put a URL with a query, a header, a token, or git stderr in a field.
- Datadog's operation name is not the OpenTelemetry span name. The tracer
  provider installs `OperationNameProcessor`, which copies the span name into
  the `operation.name` attribute. Without it every span is named `internal`
  and `operation_name:ferry.sync_repo` matches nothing.
- Log and trace correlation: `dd.trace_id` is the low 64 bits of the 128-bit
  OpenTelemetry trace ID as a decimal string, and `dd.span_id` is the 64-bit
  span ID as a decimal string. This is what `datadog-opentelemetry 0.5.2`
  sends to the Agent as the Datadog trace and span IDs
  (`src/mappings/transform/mod.rs`: `otel_trace_id_to_dd_id`,
  `otel_span_id_to_dd_id`). `telemetry::logging::dd_ids` implements it.
- `FERRY_LOG_LEVEL` filters log lines only. Spans at `info` and above are
  exported whatever the log level is.

### Tracing spike record (plan WP6, step 0)

| Item | Result | Evidence |
|---|---|---|
| 1. Versions resolve without duplicates | pass | `cargo tree -d` lists no `opentelemetry*` crate |
| 2. A `tracing` span reaches the Datadog exporter | pass | `tests/trace_export.rs`: `datadog_provider_exports_to_an_http_agent` |
| 3. `http://` and `unix://` agent URLs | pass | `tests/trace_export.rs`: `datadog_provider_exports_to_an_http_agent`, `datadog_provider_exports_to_a_unix_socket_agent` |
| 4. Correlation ID encoding | recorded above | `json_log_flattens_span_fields_and_adds_dd_ids_matching_the_exported_span` |
| 5. `cargo zigbuild` for `x86_64-unknown-linux-gnu.2.36` | pass | `scripts/build-binaries.sh` on macOS arm64 |
| 6. A span arrives in Datadog through the real Agent | **not run** | needs gate G1 (`pup auth login`) and a port-forward to the Agent |

The plan requires item 6 to pass before WP6 merges (observability step 0 and
the execution handoff). It is owner-gated: do not merge this
work to `main` without it unless Matt decides otherwise.

## Code conventions

- Single crate, no workspace, no cargo features, no `unsafe`
  (`unsafe_code = "forbid"`).
- Exit codes: `0` success, `1` runtime failure, `2` configuration or usage
  error.
- Tests never touch real GitHub or Forgejo unless they are `#[ignore]`d.

Deployment goes through `.agents/skills/ferry-deploy/SKILL.md`.
