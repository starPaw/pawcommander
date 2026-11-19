#!/usr/bin/env python3
"""Include source for MPL dependencies and attribution for vendored libgit2."""
import json
from pathlib import Path
import shutil
import subprocess
import sys

output = Path(sys.argv[1])
target = sys.argv[2]
metadata = json.loads(subprocess.check_output([
    'cargo', 'metadata', '--locked', '--format-version', '1', '--filter-platform', target,
]))
resolved = {node['id'] for node in metadata['resolve']['nodes']}
for package in metadata['packages']:
    if package['id'] not in resolved:
        continue
    source = Path(package['manifest_path']).parent
    if package['name'] == 'libgit2-sys':
        copying = (source / 'libgit2' / 'COPYING').read_text(encoding='utf-8')
        with (output / 'THIRD_PARTY_NOTICES.txt').open('a', encoding='utf-8') as notices:
            notices.write(f"\n\nlibgit2 (vendored by {package['name']} {package['version']})\n\n{copying}")
    if 'MPL-' not in (package['license'] or ''):
        continue
    destination = output / 'third-party-sources' / f"{package['name']}-{package['version']}"
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns('.cargo*', '.git', 'target'))
