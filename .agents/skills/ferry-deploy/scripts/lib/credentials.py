"""Shared descriptor-only token validation and authenticated HTTPS requests."""
import os
import subprocess
import sys


def read_token(descriptor, required=True):
    try:
        with os.fdopen(descriptor, "rb", closefd=False) as stream:
            value = stream.read().decode("utf-8").rstrip()
    except (UnicodeError, OSError):
        sys.exit("Invalid token file contents")
    if any(char in value for char in "\r\n\x00") or (required and not value):
        sys.exit("Invalid token file contents")
    return value


def authenticated_get(token, scheme, url):
    """Return body/status while keeping the header on curl's config descriptor."""
    header = "Authorization: " + scheme + " " + token
    configuration = ('header = "' + header.replace('\\', '\\\\').replace('"', '\\"') + '"\n').encode()
    reader, writer = os.pipe()
    try:
        process = subprocess.Popen(
            ["curl", "--disable", "--silent", "--show-error", "--proto", "=https",
             "--connect-timeout", "15", "--max-time", "30", "--config",
             "/dev/fd/" + str(reader), "--write-out", "\n%{http_code}", url],
            pass_fds=(reader,), stdout=subprocess.PIPE)
    finally:
        os.close(reader)
    with os.fdopen(writer, "wb") as stream:
        stream.write(configuration)
    output = process.communicate()[0]
    if process.returncode:
        sys.exit(1)
    body, _, status = output.rpartition(b"\n")
    return body, status
