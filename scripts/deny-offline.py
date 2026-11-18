#!/usr/bin/env python3
"""Audit against a disposable copy of the cached DB, without locking HOME."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile

root = Path(__file__).resolve().parent.parent
cache = Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')) / 'advisory-dbs'
parent = root / 'target/saqta-tests'
parent.mkdir(parents=True, exist_ok=True)
with tempfile.TemporaryDirectory(prefix='review-deny-', dir=parent) as temp:
    temp = Path(temp)
    database = temp / 'advisory-dbs'
    shutil.copytree(cache, database)
    config = temp / 'deny.toml'
    text = (root / 'deny.toml').read_text()
    text = text.replace('[advisories]\n', '[advisories]\ndb-path = ' + json.dumps(str(database)) + '\n', 1)
    config.write_text(text)
    print(f'Using a disposable snapshot of {cache}', flush=True)
    result = subprocess.run(['cargo', 'deny', '--offline', '--config', str(config), 'check'], cwd=root)
    raise SystemExit(result.returncode)
