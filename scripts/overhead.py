#!/usr/bin/env python3
"""Fresh-fixture release timings. Setup, verification and cleanup are untimed."""
import hashlib
import json
import os
import pathlib
import platform
import shutil
import statistics
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent
COUNT = int(os.environ.get('N', '20000'))
REPS = int(os.environ.get('REPS', '3'))
RESCUE_DIRS = int(os.environ.get('RESCUE_DIRS', '1800'))
if RESCUE_DIRS <= 0 or RESCUE_DIRS % 3:
    raise SystemExit('RESCUE_DIRS must be positive and divisible by three')
BIN = pathlib.Path(os.environ['SAQTA_BIN']).resolve()
WORKLOADS = ('deletes', 'creates', 'rename-overs', 'read-only stats', 'git status',
             'clean cargo build', 'rescue', 'rescue-cross-mount')
ENV = os.environ.copy()
ENV.setdefault('RUSTUP_HOME', str(pathlib.Path.home() / '.rustup'))
ENV.setdefault('CARGO_HOME', str(pathlib.Path.home() / '.cargo'))
for key in ('SAQTA_SUPERVISED', 'SAQTA_STRICT', 'SAQTA_NO_SYNC_WAKE',
            'SAQTA_FORCE_PROC_MEM', 'SAQTA_MAX_COPY', 'SAQTA_DEBUG'):
    ENV.pop(key, None)
PARENT = ROOT / 'target/saqta-tests'
PARENT.mkdir(parents=True, exist_ok=True)


def execute(argv, cwd, env):
    subprocess.run(argv, cwd=cwd, env=env, check=True, stdout=subprocess.DEVNULL)


def snapshot(project):
    result = {}
    for path in project.rglob('*'):
        st = path.lstat()
        if path.is_file():
            result[str(path.relative_to(project))] = (
                st.st_mode, st.st_mtime_ns, hashlib.sha256(path.read_bytes()).hexdigest())
    return result


def rescue(project, env):
    execute(['git', '--no-optional-locks', 'init', '-q'], project, env)
    (project / '.gitignore').write_text('node_modules/\n')
    sizes = (0, 64, 256, 1024, 4096)
    block = RESCUE_DIRS // 3
    for d in range(RESCUE_DIRS):
        directory = project / ('src', 'scratch', 'node_modules')[d // block] / f'd{d}'
        directory.mkdir(parents=True)
        for j in range(33 + (d < block)):
            size = sizes[j % len(sizes)] if j else (65536 if d % 10 == 0 else 128)
            (directory / f'f{j}').write_bytes(b'x' * size)
    execute(['git', '--no-optional-locks', 'add', '.gitignore', *[f'src/d{d}/f0' for d in range(block)]], project, env)
    for path in [project / f'src/d{d}/f0' for d in range(block)]:
        path.write_bytes(b'dirty\n' + path.read_bytes())
    return ['sh', '-c', 'git --no-optional-locks checkout -- . && git --no-optional-locks clean -fdx']


def prepare(directory, workload):
    project = directory / 'HOME/project'
    project.mkdir(parents=True)
    env = ENV.copy()
    for key in ('HOME', 'XDG_DATA_HOME', 'XDG_CONFIG_HOME', 'XDG_STATE_HOME', 'SAQTA_HOME'):
        path = directory / key
        path.mkdir(exist_ok=True)
        env[key] = str(path)
    env['SAQTA_STRICT'] = '1'
    if workload in ('deletes', 'rename-overs', 'read-only stats'):
        for i in range(COUNT):
            (project / f'f{i}').write_bytes(b'original\n')
    if workload == 'deletes':
        argv = ['sh', '-c', 'find . -type f -delete']
    elif workload == 'creates':
        argv = ['python3', '-c', f"from pathlib import Path\nfor i in range({COUNT}): Path(f'f{{i}}').write_bytes(bytes([110, 101, 119, 10]))"]
    elif workload == 'rename-overs':
        for i in range(COUNT):
            (project / f'n{i}').write_bytes(b'replacement\n')
        argv = ['python3', '-c', f"import os\nfor i in range({COUNT}): os.replace(f'n{{i}}', f'f{{i}}')"]
    elif workload == 'read-only stats':
        argv = ['python3', '-c', f"import os\nfor i in range({COUNT}): os.stat(f'f{{i}}')"]
    elif workload == 'git status':
        execute(['git', '--no-optional-locks', 'init', '-q'], project, env)
        for i in range(COUNT):
            (project / f'f{i}').write_bytes(b'tracked\n')
        execute(['git', '--no-optional-locks', 'add', '.'], project, env)
        argv = ['git', '--no-optional-locks', 'status', '--porcelain']
    elif workload.startswith('rescue'):
        argv = rescue(project, env)
    else:
        for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
            shutil.copy2(ROOT / name, project / name)
        shutil.copytree(ROOT / 'src', project / 'src')
        env['CARGO_TARGET_DIR'] = str(directory / 'build-target')
        argv = ['cargo', 'build', '--offline', '--locked', '--quiet']
    return project, env, argv


def measured(argv, project, env):
    start = time.perf_counter_ns()
    execute(argv, project, env)
    return (time.perf_counter_ns() - start) / 1_000_000


def run():
    if COUNT <= 0 or REPS <= 0:
        raise SystemExit('N and REPS must be positive')
    output = pathlib.Path(os.environ.get('RESULTS', ROOT / 'target/overhead.json'))
    report = dict(kernel=platform.release(), machine=platform.machine(), binary=str(BIN),
                  count=COUNT, reps=REPS, rescue_directories=RESCUE_DIRS, rescue_files=RESCUE_DIRS // 3 * 100, rows=[])
    selected = os.environ.get('WORKLOADS', '|'.join(WORKLOADS)).split('|')
    if any(w not in WORKLOADS for w in selected):
        raise SystemExit('WORKLOADS contains an unknown workload')
    output.parent.mkdir(parents=True, exist_ok=True)
    print(f'Kernel: {report["kernel"]}; binary: {BIN}; N={COUNT}; REPS={REPS}', flush=True)
    for affinity in os.environ.get('AFFINITIES', 'all|0,1').split('|'):
        prefix = [] if affinity == 'all' else ['taskset', '-c', affinity]
        try:
            probe = subprocess.run(prefix + ['true'], capture_output=True)
        except FileNotFoundError as e:
            reason = str(e)
            print(f'SKIP: affinity {affinity}: {reason}', flush=True)
            report['rows'].append(dict(affinity=affinity, skip=reason))
            continue
        if probe.returncode:
            print(f'SKIP: affinity {affinity}: {probe.stderr.decode().strip()}', flush=True)
            report['rows'].append(dict(affinity=affinity, skip=probe.stderr.decode()))
            continue
        for workload in selected:
            if workload == 'rescue-cross-mount':
                try:
                    probe = subprocess.run(['unshare', '--user', '--map-root-user', '--mount',
                                            'true'], capture_output=True)
                    reason = probe.stderr.decode().strip() if probe.returncode else None
                except FileNotFoundError as e:
                    reason = str(e)
                if reason:
                    print(f'SKIP: {workload} ({affinity}): {reason}', flush=True)
                    report['rows'].append(dict(affinity=affinity, workload=workload, skip=reason))
                    output.write_text(json.dumps(report, indent=2))
                    continue
            for variant in os.environ.get('VARIANTS', 'bare|no-sync|sync').split('|'):
                samples = []
                for repetition in range(REPS):
                    with tempfile.TemporaryDirectory(prefix='overhead-', dir=PARENT) as temp:
                        directory = pathlib.Path(temp)
                        project, env, argv = prepare(directory, workload)
                        before = snapshot(project) if workload.startswith('rescue') and variant != 'bare' else None
                        if variant != 'bare':
                            if variant == 'no-sync':
                                env['SAQTA_NO_SYNC_WAKE'] = '1'
                            argv = [str(BIN), 'exec', '--', *argv]
                        # Both commands stay in the bind namespace. The worker writes its timings.
                        if workload == 'rescue-cross-mount':
                            config = directory / 'worker.json'
                            config.write_text(json.dumps(dict(argv=argv, binary=str(BIN),
                                                              undo=variant != 'bare')))
                            worker = ['unshare', '--user', '--map-root-user', '--mount',
                                      'sh', '-c', 'mount --bind "$SAQTA_HOME" "$SAQTA_HOME" || exit 77; exec python3 "$1" --worker "$2"',
                                      'sh', str(pathlib.Path(__file__).resolve()), str(config)]
                            result = subprocess.run(prefix + worker, cwd=project, env=env, stdout=subprocess.DEVNULL)
                            if result.returncode == 77:
                                print('SKIP: separate bind mount unavailable', flush=True)
                                samples = []
                                break
                            result.check_returncode()
                            sample = json.loads(config.read_text())
                        else:
                            sample = dict(capture_ms=measured(prefix + argv, project, env))
                            if before is not None:
                                sample['undo_ms'] = measured(prefix + [str(BIN), 'undo', '1'], project, env)
                        if before is not None:
                            assert before == snapshot(project), 'rescue undo changed the original tree'
                        samples.append(sample)
                        print(f'{affinity:>4} {workload:<20} {variant:<7} {repetition + 1}: {sample}', flush=True)
                row = dict(affinity=affinity, workload=workload, variant=variant, samples=samples)
                if samples:
                    row['median'] = {key: statistics.median(s[key] for s in samples) for key in samples[0]}
                else:
                    row['skip'] = 'bind mount unavailable'
                report['rows'].append(row)
                output.write_text(json.dumps(report, indent=2))
                print(f'MEDIAN {affinity} {workload} {variant}: {row.get("median", row.get("skip"))}', flush=True)


if __name__ == '__main__':
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == '--worker':
        config = pathlib.Path(sys.argv[2])
        request = json.loads(config.read_text())
        result = dict(capture_ms=measured(request['argv'], pathlib.Path.cwd(), os.environ))
        if request['undo']:
            result['undo_ms'] = measured([request['binary'], 'undo', '1'], pathlib.Path.cwd(), os.environ)
        config.write_text(json.dumps(result))
    else:
        run()
