# Runtime base for ferry: git, git-lfs, CA roots, tini, and the unprivileged
# `ferry` user. It holds no ferry binary, so it changes rarely and is
# published once as ferry/base. Pinned by the multi-arch index digest.
FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251

RUN apt-get update \
 && apt-get install -y --no-install-recommends git git-lfs ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && groupadd --gid 10001 ferry \
 && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin ferry
