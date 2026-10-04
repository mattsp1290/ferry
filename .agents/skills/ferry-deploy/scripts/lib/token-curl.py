#!/usr/bin/env python3
"""Feed curl's header via a pipe descriptor; never argv or environment."""
import json
import os
import subprocess
import sys

url, kind, expected = sys.argv[1:]
try:
    with os.fdopen(3, 'rb', closefd=False) as stream:
        token = stream.read().decode('utf-8').rstrip()
except (UnicodeError, OSError):
    sys.exit('Invalid token file contents')
if not token or any(char in token for char in '\r\n\x00'):
    sys.exit('Invalid token file contents')
header = 'Authorization: ' + ('token ' if kind == 'forgejo' else 'Bearer ') + token
configuration = ('header = "' + header.replace('\\', '\\\\').replace('"', '\\"') + '"\n').encode()
reader, writer = os.pipe()
process = subprocess.Popen(['curl', '--disable', '--silent', '--show-error', '--proto', '=https', '--connect-timeout', '15', '--max-time', '30', '--config', '/dev/fd/' + str(reader), '--write-out', '\n%{http_code}', url], pass_fds=(reader,), stdout=subprocess.PIPE)
os.close(reader)
with os.fdopen(writer, 'wb') as stream:
    stream.write(configuration)
output = process.communicate()[0]
if process.returncode:
    sys.exit(1)
body, _, status = output.rpartition(b'\n')
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
