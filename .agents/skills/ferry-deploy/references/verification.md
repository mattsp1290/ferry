# Verification

Verify reads expected image and version from Helm in the cluster, rather than
local publishing state. It checks deployed status, exactly one Running Ready
pod on the configured node selector, digest and DD_VERSION, the startup log,
telemetry send failures, and branch/tag parity for every allowlist mapping.
`--wait` polls until the configured interval plus 60 seconds; `--repo` narrows
parity to one GitHub slug. Credentials reach git through Ferry's askpass mode
and token-file paths. No repository stores a remote or a credential.

Status reports revision, release state, pod UID, image/version, node, restart
count, and the local deployment log. A noop sync logs only at debug, so its
absence in an info log does not prove failed mirroring. Startup or parity
failures still require investigation.

Verification does not prove Datadog ingestion (use `datadog/README.md` after `pup` login), LFS object bytes, or behavior over days. To check LFS separately, use a fresh credential-safe clone and run `git lfs fsck` in it. First deployment acceptance separately
proves push propagation, a second publish/rollout without pod replacement,
credential rotation, LFS when applicable, and rollback. Record that evidence
and any checks the owner declines. A Ready pod alone is insufficient.
