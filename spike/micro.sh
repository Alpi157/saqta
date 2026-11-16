#!/usr/bin/env bash
# Per-operation interception cost on tmpfs (removes disk noise).
# Variants: bare, v1 notify-only, v1 capture, v2 capture (O_EXCL/O_APPEND fast
# paths), v2 capture with synchronous same-CPU wake-up (Linux 6.6+).
set -u
S=$(cd "$(dirname "$0")" && pwd)
T=/dev/shm/saqta-micro
N=${N:-20000}
REPS=${REPS:-5}

mk() { rm -rf "$T"; mkdir -p "$T/d"; python3 -c "
import os
for i in range($N): open('$T/d/f%06d' % i, 'w').write('x' * 100)"; }

run() {  # run <variant> <cmd>
  local v=$1 cmd=$2 t0 t1
  rm -rf "$T/q" "$T/j.tsv"
  t0=$(date +%s%N)
  case $v in
    bare) bash -c "$cmd" ;;
    v1-notify) "$S/prewrite" -n -q "$T/q" -j "$T/j.tsv" -- bash -c "$cmd" 2>/dev/null ;;
    v1-capture) "$S/prewrite" -q "$T/q" -j "$T/j.tsv" -- bash -c "$cmd" 2>/dev/null ;;
    v2-capture) "$S/prewrite2" -q "$T/q" -j "$T/j.tsv" -- bash -c "$cmd" 2>/dev/null ;;
    v2-sync) PREWRITE_SYNC=1 "$S/prewrite2" -q "$T/q" -j "$T/j.tsv" -- bash -c "$cmd" 2>/dev/null ;;
  esac
  t1=$(date +%s%N)
  echo $(( (t1 - t0) / 1000 ))
}

bench() {  # bench <label> <setup: mk|none> <cmd>
  local label=$1 setup=$2 cmd=$3
  echo "## $label (N=$N, median of $REPS; total ms and extra µs per file vs bare)"
  local base=0
  for v in bare v1-notify v1-capture v2-capture v2-sync; do
    local times=()
    for ((r = 0; r < REPS; r++)); do
      [ "$setup" = mk ] && mk || { rm -rf "$T"; mkdir -p "$T/d"; }
      times+=("$(run "$v" "$cmd")")
    done
    local med=$(printf "%s\n" "${times[@]}" | sort -n | sed -n "$(( (REPS + 1) / 2 ))p")
    [ "$v" = bare ] && base=$med
    printf "%-11s %8.1f ms  %+7.1f µs/file\n" "$v" "$(echo "$med/1000" | bc -l)" "$(echo "($med-$base)/$N" | bc -l)"
  done
}

bench "delete N files (rm -rf)" mk "rm -rf $T/d"
bench "create N files with open('w') (O_TRUNC)" none "python3 -c \"
for i in range($N): open('$T/d/n%06d' % i, 'w').write('x' * 100)\""
bench "create N files with O_EXCL (like git objects, cp to new paths)" none "python3 -c \"
import os
for i in range($N):
    fd = os.open('$T/d/n%06d' % i, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644); os.write(fd, b'x' * 100); os.close(fd)\""
bench "rename N files over existing targets (atomic-save pattern)" mk "python3 -c \"
import os
for i in range(0, $N, 2): os.replace('$T/d/f%06d' % i, '$T/d/f%06d' % (i + 1))\""
bench "stat + read N files (read-only, never intercepted)" mk "python3 -c \"
import os
for i in range($N):
    os.stat('$T/d/f%06d' % i); open('$T/d/f%06d' % i).read()\""
rm -rf "$T"
