---
name: ferry-deploy
description: Publish and deploy Ferry to Kubernetes through an SSH control host, then verify or recover its Helm release.
---

Use the entry point `.agents/skills/ferry-deploy/scripts/deploy.sh` from this checkout.
Configuration lives outside git, by default in `$HOME/.local/config/ferry`, or
at `FERRY_DEPLOY_CONFIG`. Read [config](references/config.md) before initializing
or changing an environment. The control host must have Helm 3, kubectl, and its
own kubeconfig. This skill supports SSH control hosts only.

A request to edit this skill does not authorize a deployment. Before shared
writes, obtain the owner's go-ahead for registry and cluster writes and for
Ferry to create and force-update the allowlisted Forgejo repositories, unless
this session already authorizes that rollout. Confirm the allowlist and source
revision. Tokens are supplied as owner-only files; never paste their contents.

Run these steps in order, stopping and reporting on any failed step:

1. `deploy.sh check`: validate configuration, tools, host, and Ferry config.
   Use `--forgejo` to check the token's account; `--github` when needed for
   private sources. Stop on failure.
2. Confirm the go-ahead above before proceeding.
3. `deploy.sh publish`: publish the checked-out clean commit and record its
   digest. Read [publishing](references/publish.md).
4. `deploy.sh preflight`: namespace ownership, credentials, and a temporary
   pod that checks image, cache, routing, and telemetry.
5. `deploy.sh rollout`: dry-run, then upgrade the Helm release. Read
   [rollout and recovery](references/rollout.md).
6. `deploy.sh verify --wait`: validate the running release and mirror parity.
   Read [verification](references/verification.md).
7. Report commit, digest, Helm revision, verification results, and manual
   acceptance work remaining.

The skill never deletes a namespace, a persistent release PVC, a registry tag
or repository, or a Forgejo repository. Do not retry failures with different
flags. `status`, `restart`, and `rollback` support inspection and recovery;
rollback targets a previous deployed revision, never a guessed image tag.
After GitOps is live, this workstation procedure remains the recovery path:
Ferry cannot mirror a GitOps fix while Ferry is down.

First live deployment additionally requires owner inputs and the acceptance
procedure in the owner's private implementation plan. GitOps bootstrap is a
separate gated phase; skill editing or plan approval does not authorize it.
