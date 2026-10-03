# Datadog monitors and dashboard

These files are the source of truth for ferry's three monitors and one
dashboard in the Datadog org on `us3.datadoghq.com`. Edit the file, run
`update`, then run `diff` to confirm no drift. Do not edit the objects in the
Datadog UI.

| File | Object |
|---|---|
| `monitors/repo-stale.json` | Repository has not synced successfully (per `repo`) |
| `monitors/sync-failing.json` | Repository sync keeps failing (per `repo`) |
| `monitors/heartbeat-missing.json` | Worker is not reporting a heartbeat |
| `dashboard.json` | "Ferry: GitHub to Forgejo mirror" |

`tests/datadog_assets.rs` checks the files offline: they parse, every
`ferry.*` metric they query is in `METRIC_NAMES`, every metric is used, tags and
thresholds match the table below, and no message carries an `@` handle.

## Prerequisites

Gate G1: authenticate `pup` against the us3 site.

```sh
DD_SITE=us3.datadoghq.com pup auth login
```

Alternatively set `DD_API_KEY`, `DD_APP_KEY`, and `DD_SITE=us3.datadoghq.com`.

`--no-agent` is a global `pup` flag ("Disable agent mode"). Without it, `pup`
wraps output in a `{status, data, metadata}` envelope when it detects an AI
coding assistant. With it, you get the raw API body, so the `jq` filters below
work for everyone. Put it before the subcommand.

## First apply

Run from the repository root. Each `create` prints the new object; record its
`id` in the table at the end of this file.

```sh
export DD_SITE=us3.datadoghq.com
pup --no-agent monitors create --file datadog/monitors/repo-stale.json
pup --no-agent monitors create --file datadog/monitors/sync-failing.json
pup --no-agent monitors create --file datadog/monitors/heartbeat-missing.json
pup --no-agent dashboards create --file datadog/dashboard.json
```

### Enable percentiles for `ferry.sync.duration`

`ferry.sync.duration` is a distribution. Datadog computes `p50` and `p95` for
a distribution only after percentile aggregation is enabled for that metric,
and the "Sync duration" widget stays empty until then. Once the metric has
reported at least once: Metrics → Summary → `ferry.sync.duration` →
Advanced → enable percentiles. This is a one-time step per org.

## Update and diff

```sh
export DD_SITE=us3.datadoghq.com
pup --no-agent monitors update <monitor-id> --file datadog/monitors/<name>.json
pup --no-agent monitors diff <monitor-id> datadog/monitors/<name>.json

pup --no-agent dashboards update <dashboard-id> --file datadog/dashboard.json
pup --no-agent dashboards diff <dashboard-id> datadog/dashboard.json
```

`diff` compares the file with the live object and is read-only. It reports
nothing when they match. `--ignore <field.path>` and `--only <field.path>`
narrow the comparison. The files carry no server-assigned fields (`id`,
`created`, `modified`, `creator`, `overall_state`), so those never show up.

## Recorded IDs

Fill in after the first apply.

| Object | File | ID |
|---|---|---|
| Monitor: repo stale | `monitors/repo-stale.json` | `<fill in>` |
| Monitor: sync failing | `monitors/sync-failing.json` | `<fill in>` |
| Monitor: heartbeat missing | `monitors/heartbeat-missing.json` | `<fill in>` |
| Dashboard | `dashboard.json` | `<fill in>` |

## Thresholds

| Monitor | Query | Critical | Warning | `notify_no_data` |
|---|---|---|---|---|
| Repo stale | `min(last_10m):min:ferry.repo.last_success_age_seconds{service:ferry} by {repo} > 3600` | 3600 | 1800 | `false` |
| Sync failing | `min(last_10m):min:ferry.repo.consecutive_failures{service:ferry} by {repo} >= 3` | 3 | none | `false` |
| Heartbeat missing | `sum(last_10m):sum:ferry.heartbeat{service:ferry} < 1` | 1 | none | `true` (`no_data_timeframe: 10`) |

Rule: stale critical = `12 x poll_interval`. The values above assume the
default 300 s poll interval. If `poll_interval` changes, change the stale
critical (and warning, at half) and the number in the query together; the test
requires the query threshold to equal `options.thresholds.critical`.

The `min(last_10m)` aggregation means a monitor alerts only when the metric
stayed above the threshold for the whole 10-minute window, so a single late
datapoint does not flap it.

### Why the heartbeat monitor uses `sum < 1`

`ferry.heartbeat` has value 1 and is emitted every 30 s only while the
scheduler loop is ticking. A healthy worker gives a sum of about 20 over 10
minutes, which is not below 1, so the monitor is OK. If the worker dies or
wedges, Datadog receives no datapoints at all. A metric with no data evaluates
to no-data, not to 0, so the alert comes through `notify_no_data: true` after
`no_data_timeframe: 10` minutes. The `< 1` comparison covers the other case:
datapoints that arrive with a zero sum. The query contains no `default_zero()`
on purpose, because that would turn no-data into 0 and hide the no-data path.

## Expected time to alert

For one allowlist entry that always fails, with default settings:

- Sync failing: failures occur at about 0, 5, and 15 minutes (backoff 300 s,
  then 600 s), so the monitor alerts about 25 minutes after the first failure.
- Repo stale: about 70 minutes after the last success, or after a restart. The
  age gauge counts from process start until the first success, so a restart can
  delay a stale alert but cannot suppress it.
- Recovery: after the fault is removed, the next attempt can be up to 60
  minutes away (backoff cap). A pod restart retries immediately.

### One ferry per org

The heartbeat query is scoped by `service:ferry` only. A second ferry that
reports to the same Datadog org (another cluster, or a local run with
`DD_DOGSTATSD_URL` set) would keep the sum above zero and hide a dead worker.
The design runs exactly one replica. If that changes, add the `env` tag to the
scope of all three monitors.

## Removed allowlist entries

A `repo` group that stops reporting (an entry removed from the allowlist) keeps
its last state until Datadog drops the group. The stale and failing monitors
do not rely on missing-data resolution, so resolve such a group by hand in the
Datadog UI (mute or resolve the monitor group for that `repo`).

## Per-repository stop switch

Removing the `ferry-mirror` topic from a repository on Forgejo stops ferry
writing to it. Each sync then fails with `error_kind:dest_unmanaged`, and the
stale and failing monitors alert for that `repo` only. The monitor messages
say so. Restore the topic and restart the pod to recover quickly.

## Notification handle

The monitor messages contain no `@` notification handle on purpose: the owner
supplies one later. To add it, append a handle such as `@slack-<channel>` or
`@<email>` to the `message` of each monitor file (for example inside each
`{{#is_alert}}` and `{{#is_recovery}}` block), then `update` and `diff`. The
test `monitors_have_tags_scope_and_no_notification_handle` rejects any `@` in a
message, so change that assertion in the same commit.
