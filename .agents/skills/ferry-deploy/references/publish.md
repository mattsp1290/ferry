# Publishing

`publish` requires a clean checkout. The image tag and version are the commit's
12-character SHA; the release uses a digest. Existing commit tags are reused.
The base tag is the first 12 characters of the Dockerfile's SHA-256. A missing
base is built for linux/amd64 with provenance disabled, saved for that platform,
and uploaded with crane. Its platform and single-image manifest are checked.
The existing build-binaries and assemble-image scripts produce the application.
The manifest digest read back from the registry must match assembly output.

Only `BASE_PUSH_PATH` and `IMAGE_PUSH_PATH` under `REGISTRY_PUSH` are written.
`REGISTRY_INSECURE=true` permits HTTP for that registry. No registry configuration
is changed, no tags or repositories are deleted, and authenticated registries
are outside this skill's contract. Stop if credentials are required.

Before the first shared deployment, test real publishing against a throwaway
local `crane registry serve`: verify both digests, the linux/amd64 base manifest,
`docker run --platform linux/amd64 <reference> --version`, and tag reuse on a
second publish. Remove only the throwaway registry process afterward.

Only an explicit registry `MANIFEST_UNKNOWN` or `NAME_UNKNOWN` response is treated as an absent tag. Authentication, connection, and invalid-digest failures stop before building or pushing. Temporary base archives and state records are removed on failure or interruption.

The Dockerfile hash intentionally reuses the previously built package set. To refresh git, git-lfs and CA certificates, update the pinned Debian base digest in `docker/base.Dockerfile`, commit it, and publish again; the changed Dockerfile generates a new base tag. Release and local helper builds use Cargo's `--locked` mode so dependency resolution cannot rewrite the checked-out lockfile.
