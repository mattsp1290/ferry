#!/usr/bin/env bash
# Executing, recording local doubles. No remote network or real cluster.
stubs_init() {
  STUB_ROOT=$(mktemp -d)
  export STUB_ROOT HOME="$STUB_ROOT/home" FERRY_DEPLOY_CONFIG="$STUB_ROOT/config"
  mkdir -p "$HOME" "$STUB_ROOT/bin" "$STUB_ROOT/records" "$STUB_ROOT/remote"
  export STUB_REAL_PATH=$PATH STUB_REAL_HELM=$(command -v helm) STUB_REAL_GIT=$(command -v git)
  export STUB_COMMIT=$($STUB_REAL_GIT rev-parse --short=12 HEAD)
  export STUB_DIGEST=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
  export FERRY_BIN="$STUB_ROOT/bin/ferry" FERRY_DEPLOY_BUILD_CMD="$STUB_ROOT/bin/build" FERRY_DEPLOY_ASSEMBLE_CMD="$STUB_ROOT/bin/assemble"
  cp "$(dirname "${BASH_SOURCE[0]}")/stub.py" "$STUB_ROOT/bin/stub.py"
  chmod +x "$STUB_ROOT/bin/stub.py"
  for tool in ssh helm kubectl crane docker curl git jq base64 sed awk tr cat python3 cargo ferry build assemble; do ln -s stub.py "$STUB_ROOT/bin/$tool"; done
  # Use an absolute interpreter to avoid recursion through the python3 wrapper.
  stub_python=$(python3 -c 'import sys; print(sys.executable)')
  "$stub_python" - "$STUB_ROOT/bin/stub.py" "$stub_python" <<'PY'
import pathlib,sys
path=pathlib.Path(sys.argv[1]); text=path.read_text(); path.write_text(text.replace('#!/usr/bin/env python3','#!'+sys.argv[2],1))
PY
  export PATH="$STUB_ROOT/bin:$PATH"
  bash "$skill/scripts/deploy.sh" init >/dev/null
  printf 'no-token-test-UNIQUE-58f814bd\n' > "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
  chmod 600 "$FERRY_DEPLOY_CONFIG/secrets/forgejo-token"
}
