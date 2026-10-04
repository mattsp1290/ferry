# Rollout and recovery

Preflight owns namespace creation and rejects an existing namespace unless it
has the configured release or the ferry-deploy ownership label. It streams the
credentials Secret to server-side kubectl apply. Tokens never enter remote files
or Helm values. A resource-version change leaves a restart marker until a new
pod loads the credentials, including across interrupted runs.

The temporary preflight pod and PVC are cleaned up even on failure. Discovered
cache permissions and telemetry transport go to `state/preflight.yaml`. A paired
`state/preflight.json` binds that successful result to namespace, release and image
digest, control host, kubeconfig, image pull repository and a fingerprint of
owner values plus chart defaults; a new preflight invalidates both records before doing any checks. Owner
values override them. An explicit unworkable owner setting stops the run.

Rollout requires a successful preflight for the same namespace, release and digest
and checks namespace ownership again before any write. It chooses the published digest for the current commit, or accepts
`--digest sha256:<64 hex> --version <12 hex>`. For an externally published image, first run `preflight` with those same options, then `rollout` with them. It lints locally, bundles only
chart and values, performs a silent server-side Helm dry run before applying credentials, and upgrades with
wait, timeout, and bounded history. Inspect failure diagnostics before any
further action. The owner values file is never edited by the skill.

`restart` restarts the release Deployment and waits. A healthy release rolls back
to the newest superseded revision. A failed or pending release recovers the newest
revision that is still deployed or superseded, including the last deployed
revision after a failed upgrade. `rollback --revision N` accepts a deployed or
superseded target and refuses a failed or pending target. The control host
rechecks the selected history head under the release lock and refuses a stale
selection before calling Helm rollback. Verify again after either operation. Rollback restores the chart, values, and image, but leaves the credentials Secret at its last applied contents.

A pending upgrade needs a rollback to the last deployed revision on the control
host with explicit `--kubeconfig`, release, and namespace. Do not delete Helm
release Secrets by hand. A pending or failed first install has no rollback
target: with owner authorization, uninstall that first release on the control
host and rerun rollout. This removes its still-empty PVC and is acceptable only
before anything has been mirrored. Keep the namespace and credentials Secret.
The skill has no command to delete the persistent release PVC.

Interrupted credential rotation leaves a durable restart marker. `status` reports
it, and preflight or rollout exit warns to run `deploy restart` until a new pod
has loaded the credentials. Failed rollout attempts are recorded in the local
deployment log.
