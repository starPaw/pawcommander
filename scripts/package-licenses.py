#!/usr/bin/env python3
"""Include corresponding source for MPL dependencies in the binary distribution."""
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
    if package['id'] not in resolved or 'MPL-' not in (package['license'] or ''):
        continue
    source = Path(package['manifest_path']).parent
    destination = output / 'third-party-sources' / f"{package['name']}-{package['version']}"
    shutil.copytree(source, destination, ignore=shutil.ignore_patterns('.cargo*', '.git', 'target'))
