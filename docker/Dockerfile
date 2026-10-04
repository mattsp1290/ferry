# Local testing only. CI does not use this file; scripts/assemble-image.sh is
# the release path and must keep the same entrypoint, cmd, and user.
#
#   docker build --platform linux/amd64 -f docker/Dockerfile \
#     --build-arg BASE=<base image reference with digest> .
#
# Needs dist/linux-<arch>/ferry from scripts/build-binaries.sh.
ARG BASE=""
FROM ${BASE}
ARG BASE
ARG TARGETARCH
# An empty BASE already makes FROM fail; this check gives a readable message
# in case a builder accepts the empty reference.
RUN test -n "${BASE}" || { echo "BASE build argument is required (base image reference with digest)" >&2; exit 1; }
COPY --chmod=0755 dist/linux-${TARGETARCH}/ferry /usr/local/bin/ferry
USER 10001
ENTRYPOINT ["/usr/bin/tini","--","/usr/local/bin/ferry"]
CMD ["run"]
