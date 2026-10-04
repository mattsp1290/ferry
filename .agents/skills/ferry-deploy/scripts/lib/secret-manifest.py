#!/usr/bin/env python3
"""Validate descriptor tokens, then emit a whole Secret or send it to SSH."""
import base64
import json
import os
import sys
import subprocess


def token(descriptor, required):
    try:
        with os.fdopen(descriptor, "rb", closefd=False) as stream:
            value = stream.read().decode("utf-8").rstrip()
    except (UnicodeError, OSError):
        sys.exit("Invalid token file contents")
    if any(char in value for char in "\r\n\x00") or (required and not value):
        sys.exit("Invalid token file contents")
    return base64.b64encode(value.encode("utf-8")).decode("ascii") if value else None


namespace, name = sys.argv[1:3]
data = {"forgejo-token": token(8, True)}
github = token(9, False)
if github:
    data["github-token"] = github
manifest = json.dumps({"apiVersion": "v1", "kind": "Secret", "type": "Opaque",
                            "metadata": {"name": name, "namespace": namespace}, "data": data})
if len(sys.argv) == 3:
    sys.stdout.write(manifest)
else:
    destination, command = sys.argv[3:]
    sys.exit(subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15", destination, "--", command], input=manifest.encode("utf-8")).returncode)
