#!/usr/bin/env python3
"""Validate descriptor tokens, then emit a whole Secret or send it to SSH."""
import base64
import json
import sys
import subprocess


sys.dont_write_bytecode = True
from credentials import read_token

namespace, name = sys.argv[1:3]
data = {"forgejo-token": base64.b64encode(read_token(3).encode()).decode("ascii")}
github = read_token(4, required=False)
if github:
    data["github-token"] = base64.b64encode(github.encode()).decode("ascii")
manifest = json.dumps({"apiVersion": "v1", "kind": "Secret", "type": "Opaque",
                            "metadata": {"name": name, "namespace": namespace}, "data": data})
if len(sys.argv) == 3:
    sys.stdout.write(manifest)
else:
    destination, command = sys.argv[3:]
    sys.exit(subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", destination, "--", command], input=manifest.encode("utf-8")).returncode)
