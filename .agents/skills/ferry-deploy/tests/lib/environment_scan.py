"""Scan tracked text for deployment addresses; never print config values."""
import ipaddress
import os
import pathlib
import re
import subprocess
import sys
import urllib.parse

public = {'github.com', 'api.github.com', 'raw.githubusercontent.com', 'crates.io',
          'index.docker.io', 'example.com', 'datadoghq.com', 'forgejo.org', 'codeberg.org'}
private = [ipaddress.ip_network(net) for net in
           ((0x0A000000, 8), (0xAC100000, 12), (0xC0A80000, 16), (0x64400000, 10))]
files = subprocess.check_output(['git', 'ls-files', '-z']).decode().split('\0')
private_values = []
config = pathlib.Path(os.environ.get('FERRY_DEPLOY_CONFIG', pathlib.Path.home() / '.local/config/ferry'))
if config.exists():
    # Validate before reading any environment file. Print only non-secret keys.
    script = '''source .agents/skills/ferry-deploy/scripts/lib/config.sh
source "$SKILL_DIR/scripts/lib/values.sh"
for key in CONTROL_SSH REGISTRY_PUSH IMAGE_PULL_REPOSITORY; do
  printf '%s=%s\\n' "$key" "${!key-}"
done
merged_values | jq -r '"FORGEJO_URL=" + .config.forgejo.url'
'''
    result = subprocess.run(['bash', '-c', script], capture_output=True, text=True)
    if result.returncode:
        print('environment scan: existing config failed validation', file=sys.stderr)
        sys.exit(1)
    for setting in result.stdout.splitlines():
        key, _, value = setting.partition('=')
        if key == 'CONTROL_SSH':
            value = value.rsplit('@', 1)[-1]
        elif key == 'IMAGE_PULL_REPOSITORY':
            value = value.split('/', 1)[0]
        elif key == 'FORGEJO_URL':
            value = urllib.parse.urlsplit(value).hostname or ''
        host = value.split(':', 1)[0]
        if (not value or host == 'localhost' or 'example.' in value
                or host.endswith(('.example', '.invalid', '.test', '.internal'))):
            continue
        private_values.append(value)
failed = False
for name in filter(None, files):
    path = pathlib.Path(name)
    if not path.is_file():
        continue
    try:
        content = path.read_text()
    except UnicodeError:
        continue
    for line_number, line in enumerate(content.splitlines(), 1):
        bad = any(value in line for value in private_values)
        for match in re.finditer(r'(?<![\d.])(?:\d{1,3}\.){3}\d{1,3}(?![\d.])', line):
            try:
                addr = ipaddress.ip_address(match.group())
                bad |= any(addr in network for network in private)
            except ValueError:
                pass
        for match in re.finditer(r'(?:https?|ssh|git|udp)://[^\s\x22\x27<>`\\]+', line):
            if '[' in match.group() or '{' in match.group():
                continue
            try:
                host = urllib.parse.urlsplit(match.group()).hostname
            except ValueError:
                bad = True
                continue
            if not host:
                continue
            host = host.rstrip('.,;:')
            if '[' in host or '{' in host:
                continue
            try:
                if ipaddress.ip_address(host).is_loopback:
                    continue
            except ValueError:
                pass
            if ('.' not in host or host.endswith('.local') or host == 'localhost'
                    or host in public or host.endswith('.datadoghq.com')
                    or host.endswith(('.example', '.invalid', '.test', '.internal',
                                      '.example.com', '.svc.cluster.local'))):
                continue
            bad = True
        if bad:
            print(f'{name}:{line_number}: deployment address is not allowed', file=sys.stderr)
            failed = True
sys.exit(int(failed))
