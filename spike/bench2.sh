#!/usr/bin/env bash
# Destructive and write-heavy workloads: bare vs interception-only vs capture.
# Each run gets a fresh tree (hard-link copy of the pristine repo, so preparing
# is cheap), a warm page cache, and a settled disk. Reports the median of N runs.
set -u
S=$(cd "$(dirname "$0")" && pwd)
PW=$S/prewrite
WORK=$S/work
REPS=${REPS:-3}

prep() {
  rm -rf "$WORK" "$S"/qb "$S"/jb.tsv
  if [ "${1:-link}" = copy ]; then cp -a "$S/pristine" "$WORK"; else cp -al "$S/pristine" "$WORK"; fi
  (cd "$WORK" && find . -type f -newer /dev/null >/dev/null 2>&1 && git status --porcelain >/dev/null 2>&1)
  sync; sleep 1
}

one() {  # one <variant> <command> -> prints elapsed ms, stores stats line
  local v=$1 cmd=$2 t0 t1
  cd "$WORK"; t0=$(date +%s%N)
  case $v in
    bare) bash -c "$cmd" >/dev/null 2>&1 ;;
    notify) "$PW" -n -q "$S/qb" -j "$S/jb.tsv" -- bash -c "$cmd" >/dev/null 2>"$S/last.err" ;;
    capture) "$PW" -q "$S/qb" -j "$S/jb.tsv" -- bash -c "$cmd" >/dev/null 2>"$S/last.err" ;;
    skipregen) "$PW" -s -q "$S/qb" -j "$S/jb.tsv" -- bash -c "$cmd" >/dev/null 2>"$S/last.err" ;;
  esac
  t1=$(date +%s%N); cd "$S"
  echo $(( (t1 - t0) / 1000000 ))
}

workload() {  # workload <label> <prep-mode> <command> <variants...>
  local label=$1 mode=$2 cmd=$3; shift 3
  echo "## $label"
  for v in "$@"; do
    local times=() stats=""
    for ((r = 0; r < REPS; r++)); do
      prep "$mode"
      times+=("$(one "$v" "$cmd")")
      [ "$v" != bare ] && stats=$(grep -o 'notifs=[0-9]* links=[0-9]* copies=[0-9]* copied_bytes=[0-9]*' "$S/last.err")
    done
    local med=$(printf "%s\n" "${times[@]}" | sort -n | sed -n "$(( (REPS + 1) / 2 ))p")
    printf "%-10s %7s ms  (runs: %s)  %s\n" "$v" "$med" "${times[*]}" "$stats"
  done
}

FILES200=$(cd "$S/pristine" && git status --porcelain | awk '$1=="M"{print $2}' | head -200 | tr '\n' ' ')

workload "git checkout -- . && git clean -fdx (31k deletions incl. node_modules, .env, 50 MB db)" link \
  "git checkout -- . && git clean -fdxq" bare notify capture skipregen
workload "rm -rf src (100k+ files, home-wipe scale)" link "rm -rf src" bare notify capture
workload "cp -r src/pkg00 src/pkgcopy (10k new files)" link "cp -r src/pkg00 src/pkgcopy" bare notify capture
workload "sed -i on 200 modified files (rename-over)" link "sed -i 's/a/b/' $FILES200" bare notify capture
workload "write 30k new files into node_modules (npm-install-like)" link \
  "python3 -c \"import os
for d in range(300):
    p='node_modules/new%03d'%d; os.makedirs(p, exist_ok=True)
    for f in range(100): open(p+'/m%03d.js'%f,'w').write('x')\"" bare notify capture skipregen
workload "overwrite 200 existing files in place with > (needs content copies)" copy \
  "for f in $FILES200; do echo x > \$f; done" bare notify capture
