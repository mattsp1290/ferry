# Local configuration

## Directory layout

`$CFG` = `${FERRY_DEPLOY_CONFIG:-$HOME/.local/config/ferry}`.

```text
$CFG/                      0700
├── deploy.env             0600   non-secret settings (keys below)
├── values.yaml            0600   owner's live Helm values: datadog, cache, config.forgejo, config.repos
├── secrets/               0700
│   ├── forgejo-token      0600   required
│   └── github-token       0600   optional
└── state/                 0700   written only by the skill
    ├── last-publish.json  0600   commit, image digest, base digest, time
    ├── preflight.json  0600   successful probe target, digest and values fingerprint
    ├── preflight.yaml     0600   values preflight discovered
    └── deployments.jsonl  0600   log: time, commit, digest, revision, action, result
```

`values.yaml` is not secret but is private: the allowlist may name private repositories. It gets the same protection.

`values.yaml` must not set `image.repository`, `image.digest`, or `image.version`. The skill supplies them. `deploy check` fails when the file sets any of them.

The skill never writes `values.yaml`, `deploy.env`, or anything under `secrets/` after `init`.

## `deploy.env` keys

Plain `KEY=value` lines, `#` comments, blank lines. The loader parses the file line by line. It never `source`s or `eval`s it. A value that does not match its key's pattern is an error that names the key and does not echo the value. No pattern admits whitespace, quotes, `$`, backticks, `;`, `&`, `|`, `<`, `>`, parentheses, or a backslash, which is what lets the skill place values in a remote command line safely.

| Key | Required | Meaning | Pattern | Example placeholder |
|---|---|---|---|---|
| `CONTROL_SSH` | yes | SSH destination of the control host. | `[A-Za-z0-9._-]+@[A-Za-z0-9._-]+` or a host alias `[A-Za-z0-9._-]+`; the first character is never `-` | `admin@control.example.internal` |
| `KUBECONFIG_PATH` | yes | Kubeconfig path on the control host. | absolute path, `[A-Za-z0-9._/-]+` | `/etc/kubernetes/admin.conf` |
| `NAMESPACE` | yes | Target namespace. | DNS label | `ferry` |
| `RELEASE` | yes | Helm release name. | DNS label | `ferry` |
| `REGISTRY_PUSH` | yes | Registry authority the workstation pushes to. Must contain `.` or `:`, or be `localhost` (the image assembler). | `[A-Za-z0-9.-]+(:[0-9]+)?` | `registry.example.internal:5000` |
| `REGISTRY_INSECURE` | yes | `true` when `REGISTRY_PUSH` is plain HTTP. | `true` or `false` | `false` |
| `IMAGE_PULL_REPOSITORY` | yes | Repository reference the node's runtime pulls. | `[A-Za-z0-9.-]+(:[0-9]+)?(/[a-z0-9._-]+)+` | `localhost:5000/ferry/ferry` |
| `IMAGE_PUSH_PATH` | no | Repository path under `REGISTRY_PUSH`. Default `ferry/ferry`. | `[a-z0-9._-]+(/[a-z0-9._-]+)*` | |
| `BASE_PUSH_PATH` | no | Default `ferry/base`. | same | |
| `HELM_TIMEOUT` | no | Default `5m`. | `[0-9]+[smh]` | |

GitOps keys and state directories are unsupported until the GitOps phase is implemented.

Unknown keys are an error. A missing required key is an error that names the key.

## Validation rules

`config.sh` runs these before any other action, in this order. Each failure prints one line that names the path and the rule, and exits with status 2. Nothing is read from a file that fails.

1. `$CFG` exists and is a directory, not a symlink.
2. `$CFG` and every entry under it are owned by the current user.
3. `$CFG`, `secrets/`, `state/` have mode `0700`. Every regular file has mode `0600`.
4. No entry under `$CFG` is a symlink. An unexpected entry (for example a `.DS_Store` created by Finder) fails with a message that names it and says to remove it; the skill does not delete it.
5. `$CFG` is not inside a git work tree: `git -C "$CFG" rev-parse --is-inside-work-tree` must fail.
6. `$CFG` is not under the ferry checkout.
7. `deploy.env` parses, has every required key, has no unknown key, and every value matches its pattern.
8. `secrets/forgejo-token` exists and is non-empty.
9. `values.yaml` exists.

Mode and owner checks must work with both BSD and GNU `stat`. Detect the flavor once. Every workstation script must run on bash 3.2 (no associative arrays, no `mapfile`, no `${var,,}`).

## Reading values

`merged_values` runs, on the workstation:

```text
helm template values-reader <skill>/resources/values-reader \
  -f charts/ferry/values.yaml -f "$CFG/state/preflight.yaml" (when present) -f "$CFG/values.yaml"
```

and prints the JSON object from the rendered `values.json` (strip the `---` and `# Source:` lines). Callers use `jq`. This is the only way the skill reads `config.forgejo.url`, `config.forgejo.username`, `config.repos`, `config.sync.poll_interval_seconds`, `datadog.transport`, `datadog.agentService`, and the `image.*` prohibition. The file order equals the order `rollout` passes to Helm, so the skill and Helm see the same values.

The helper chart emits JSON and Helm merges YAML; both flow and block YAML work.

## Behavior

- `deploy init` creates `$CFG` with the layout above and the right modes, copies the two example files into place, and prints the paths the owner must edit. It never overwrites an existing file. It creates no token file.
- `deploy check` runs the validation rules, then the tool checks, the control-host checks, `merged_values`, and the `ferry check-config` render check. Flags `--forgejo` and `--github` add the token checks. Without a flag it does not open a token file.
- The ferry binary for `check-config` and for askpass is `$FERRY_BIN` when set, otherwise `target/debug/ferry` after a `cargo build --locked` that `check` runs itself.
- Token reading: one helper, `with_token_fd <path> <fd> <command…>` (descriptor 3 for Forgejo, 4 for optional GitHub), opens the file on a file descriptor for the child. No shell function returns a token as a string and no shell variable holds one, and no token is exported into an environment. This is what makes success criterion 3 checkable.
- Output: the skill prints setting names and non-secret values. It never prints the allowlist unless the operator passes `--show-values`.
- `state/deployments.jsonl` is append-only and is a log, not a source of truth.

The shared `credentials.py` reader validates both descriptor inputs as UTF-8, strips trailing Unicode whitespace, and rejects an empty required token or embedded CR, LF or NUL. An empty optional GitHub token is omitted. The entire manifest is produced in memory before SSH starts; a malformed token causes no remote call. Tokens and their base64 form never become shell variables, arguments, environment values, or temporary files. API requests disable curl startup configuration, require HTTPS, and have connection and total timeouts.

The chart probes port 8080. Ferry uses its default `health.listen` address `0.0.0.0:8080`; `check` refuses a values override with a different port. The local acceptance tests additionally require `rg` (ripgrep).

## Internal Forgejo routing

`allowInsecureUrls: true` in owner values explicitly permits Ferry to use an
internal HTTP Forgejo URL. It sets `FERRY_ALLOW_INSECURE_URLS=1` in the pod,
local config validator and parity binary. The default is false; HTTPS remains
required without this opt-in. Use it only for a trusted internal route.

Optional `FORGEJO_CHECK_URL` in `deploy.env` selects a separate HTTPS base URL
for workstation account, mirror parity and mirror-topic checks. This is needed when
`config.forgejo.url` is an internal Kubernetes Service that the workstation
cannot reach. Its pattern permits HTTPS host, optional port and path only;
credentials and query strings are forbidden. The verifier renders a local-only config with this URL for read-only parity; it never changes the pod's URL.
