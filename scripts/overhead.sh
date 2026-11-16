#!/usr/bin/env bash
# Release measurements. Every destructive workload uses a fresh disposable tree.
set -euo pipefail
root=$(git --no-optional-locks rev-parse --show-toplevel)
cd "$root"
cargo build --release --locked
exec python3 - "$root" "$@" <<'PY'
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time

root = pathlib.Path(sys.argv[1])
count = int(os.environ.get('N', '20000'))
reps = int(os.environ.get('REPS', '3'))
if count <= 0 or reps <= 0:
    raise SystemExit('N and REPS must be positive')
variants = ('bare', 'saqta no-sync', 'saqta sync')
# Resolve the rustup proxy while HOME still points at the user's toolchain.
env_base = os.environ.copy()
env_base.setdefault('RUSTUP_HOME', str(pathlib.Path.home() / '.rustup'))
env_base.setdefault('CARGO_HOME', str(pathlib.Path.home() / '.cargo'))
for key in ('SAQTA_SUPERVISED', 'SAQTA_STRICT', 'SAQTA_NO_SYNC_WAKE', 'SAQTA_FORCE_PROC_MEM', 'SAQTA_MAX_COPY'):
    env_base.pop(key, None)
parent = root / 'target/saqta-tests'
parent.mkdir(parents=True, exist_ok=True)
print(f'Kernel: {platform.release()} ({platform.machine()})', flush=True)
print(f'N={count}; median of {reps} fresh runs; release supervisor; milliseconds', flush=True)
print(f'{"workload":<20} {"bare":>12} {"saqta no-sync":>16} {"saqta sync":>14}', flush=True)

def execute(argv, cwd, env):
    subprocess.run(argv, cwd=cwd, env=env, check=True, stdout=subprocess.DEVNULL)

def prepare(directory, workload):
    project = directory / 'HOME/project'
    project.mkdir(parents=True)
    env = env_base.copy()
    for key in ('HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'SAQTA_HOME'):
        path = directory / key
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    if workload in ('deletes', 'rename-overs', 'read-only stats'):
        for i in range(count):
            (project / f'f{i}').write_bytes(b'original\n')
    if workload == 'deletes':
        argv = ['sh', '-c', 'find . -type f -delete']
    elif workload == 'creates':
        argv = ['python3', '-c', f"from pathlib import Path\nfor i in range({count}): Path(f'f{{i}}').write_bytes(b'new\\n')"]
    elif workload == 'rename-overs':
        for i in range(count):
            (project / f'n{i}').write_bytes(b'replacement\n')
        argv = ['python3', '-c', f"import os\nfor i in range({count}): os.replace(f'n{{i}}', f'f{{i}}')"]
    elif workload == 'read-only stats':
        argv = ['python3', '-c', f"import os\nfor i in range({count}): os.stat(f'f{{i}}')"]
    elif workload == 'git status':
        execute(['git', '--no-optional-locks', 'init', '-q'], project, env)
        for i in range(count):
            (project / f'f{i}').write_bytes(b'tracked\n')
        execute(['git', '--no-optional-locks', 'add', '.'], project, env)
        argv = ['git', '--no-optional-locks', 'status', '--porcelain']
    else:
        # Copy just this crate's build inputs, never the working tree's .git or target.
        for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
            shutil.copy2(root / name, project / name)
        shutil.copytree(root / 'src', project / 'src')
        env['CARGO_TARGET_DIR'] = str(directory / 'build-target')
        argv = ['cargo', 'build', '--offline', '--locked', '--quiet']
    return project, env, argv

for workload in ('deletes', 'creates', 'rename-overs', 'read-only stats', 'git status', 'clean cargo build'):
    times = []
    for variant in variants:
        samples = []
        for _ in range(reps):
            # TemporaryDirectory supplies a unique, nonempty path and owns cleanup.
            with tempfile.TemporaryDirectory(prefix='overhead-', dir=parent) as temp:
                project, env, argv = prepare(pathlib.Path(temp), workload)
                if variant != 'bare':
                    if variant == 'saqta no-sync':
                        env['SAQTA_NO_SYNC_WAKE'] = '1'
                    argv = [str(root / 'target/release/saqta'), 'exec', '--', *argv]
                start = time.perf_counter_ns()
                execute(argv, project, env)
                samples.append((time.perf_counter_ns() - start) / 1_000_000)
        times.append(statistics.median(samples))
    print(f'{workload:<20} {times[0]:12.1f} {times[1]:16.1f} {times[2]:14.1f}', flush=True)
PY
