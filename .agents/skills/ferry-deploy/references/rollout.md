# Rollout and recovery

Preflight owns namespace creation and rejects an existing namespace unless it
has the configured release or the ferry-deploy ownership label. It streams the
credentials Secret to server-side kubectl apply. Tokens never enter remote files
or Helm values. A resource-version change leaves a restart marker until a new
pod loads the credentials, including across interrupted runs.

The temporary preflight pod and PVC are cleaned up even on failure. Discovered
cache permissions and telemetry transport go to `state/preflight.yaml`; owner
values override them. An explicit unworkable owner setting stops the run.

Rollout chooses the published digest for the current commit, or accepts
`--digest sha256:<64 hex> --version <12 hex>`. It lints locally, bundles only
chart and values, performs a silent server-side Helm dry run, and upgrades with
wait, timeout, and bounded history. Inspect failure diagnostics before any
further action. The owner values file is never edited by the skill.

`restart` restarts the release Deployment and waits. `rollback` selects the
previous deployed revision, or accepts `--revision N`; it refuses a failed or
pending target. Verify again after either operation. Rollback restores the chart, values, and image, but leaves the credentials Secret at its last applied contents.

A pending upgrade needs a rollback to the last deployed revision on the control
host with explicit `--kubeconfig`, release, and namespace. Do not delete Helm
release Secrets by hand. A pending or failed first install has no rollback
target: with owner authorization, uninstall that first release on the control
host and rerun rollout. This removes its still-empty PVC and is acceptable only
before anything has been mirrored. Keep the namespace and credentials Secret.
The skill has no command to delete the persistent release PVC.

After GitOps is enabled, recover from the workstation by checking out the fix,
publishing, and rolling out. A later CI deploy of main can supersede an older
workstation version.
