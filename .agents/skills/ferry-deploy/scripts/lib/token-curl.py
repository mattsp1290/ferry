#!/usr/bin/env python3
"""Feed curl's header via a pipe descriptor; never argv or environment."""
import json
import sys

sys.dont_write_bytecode = True
from credentials import authenticated_get, read_token

url, kind, expected = sys.argv[1:]
body, status = authenticated_get(read_token(3), 'Bearer' if kind == 'github' else 'token', url)
if kind == 'forgejo-get':
    if status != b'200':
        sys.exit(1)
    sys.stdout.buffer.write(body)
    sys.exit(0)
if kind == 'github':
    print('GitHub HTTP status: ' + status.decode())
    sys.exit(0 if status == b'200' else 1)
if status != b'200':
    sys.exit('Forgejo token check failed (HTTP ' + status.decode() + ')')
try:
    login = json.loads(body)['login']
except (ValueError, KeyError, TypeError):
    sys.exit('Forgejo token check returned an invalid user response')
if login != expected:
    sys.exit('Forgejo token login differs from config.forgejo.username')
print('Forgejo login: ' + login)
