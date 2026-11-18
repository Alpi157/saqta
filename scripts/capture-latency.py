#!/usr/bin/env python3
"""Per-operation distributions complement overhead.sh's end-to-end timings."""
import importlib.util
import json
import os
from pathlib import Path
import statistics
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parent.parent
os.environ.setdefault('SAQTA_BIN', str(ROOT / 'target/m5-speed/release/saqta'))
spec = importlib.util.spec_from_file_location('overhead', ROOT / 'scripts/overhead.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
if bench.COUNT <= 0 or bench.REPS <= 0:
    raise SystemExit('N and REPS must be positive')
bins = {'M4': Path(os.environ.get('BASELINE_BIN', ROOT / 'target/baseline-m4/release/saqta')),
        'M5': bench.BIN}
output = Path(os.environ.get('RESULTS', ROOT / 'target/m5-evidence/latency.json'))
output.parent.mkdir(parents=True, exist_ok=True)
report = dict(count=bench.COUNT, reps=bench.REPS, binaries={k: str(v) for k, v in bins.items()}, rows=[])

for affinity in ('all', '0,1'):
    prefix = [] if affinity == 'all' else ['taskset', '-c', affinity]
    try:
        probe = subprocess.run(prefix + ['true'], capture_output=True)
    except FileNotFoundError as e:
        print(f'SKIP: affinity {affinity}: {e}', flush=True)
        continue
    if probe.returncode:
        print(f'SKIP: affinity {affinity}: {probe.stderr.decode().strip()}', flush=True)
        continue
    for workload in ('deletes', 'creates', 'rename-overs'):
        for variant in os.environ.get('VARIANTS', 'bare|M4 no-sync|M4 sync|M5 no-sync|M5 sync').split('|'):
            samples = []
            for repetition in range(bench.REPS):
                with tempfile.TemporaryDirectory(prefix='call-latency-', dir=bench.PARENT) as temp:
                    project, env, _ = bench.prepare(Path(temp), workload)
                    operation = {
                        'deletes': 'os.unlink(name)',
                        'creates': "fd=os.open(name,os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o644);os.write(fd,b'new');os.close(fd)",
                        'rename-overs': 'os.replace(source,name)',
                    }[workload]
                    code = ('import os,time,json\nvalues=[]\n'
                            f'for i in range({bench.COUNT}):\n'
                            " name=f'f{i}';source=f'n{i}'\n"
                            ' start=time.perf_counter_ns()\n'
                            f' {operation}\n'
                            ' values.append(time.perf_counter_ns()-start)\n'
                            'print(json.dumps(values))')
                    argv = ['python3', '-c', code]
                    if variant != 'bare':
                        label, mode = variant.split(' ')
                        if mode == 'no-sync':
                            env['SAQTA_NO_SYNC_WAKE'] = '1'
                        argv = [str(bins[label]), 'exec', '--', *argv]
                    started = time.perf_counter_ns()
                    result = subprocess.run(prefix + argv, cwd=project, env=env,
                                            check=True, capture_output=True)
                    elapsed = (time.perf_counter_ns() - started) / 1_000_000
                    values = json.loads(result.stdout)
                    assert len(values) == bench.COUNT and all(v >= 0 for v in values)
                    ordered = sorted(values)
                    sample = dict(elapsed_ms=elapsed,
                                  median_us=statistics.median(values) / 1000,
                                  p95_us=ordered[(len(ordered) * 95 + 99) // 100 - 1] / 1000,
                                  durations_ns=values)
                    samples.append(sample)
                    print(f'{affinity} {workload} {variant} {repetition+1}: '
                          f'median={sample["median_us"]:.3f} us; p95={sample["p95_us"]:.3f} us', flush=True)
            row = dict(affinity=affinity, workload=workload, variant=variant, samples=samples,
                       median_us=statistics.median(s['median_us'] for s in samples))
            report['rows'].append(row)
            output.write_text(json.dumps(report, separators=(',', ':')))
            print(f'MEDIAN {affinity} {workload} {variant}: {row["median_us"]:.3f} us', flush=True)
