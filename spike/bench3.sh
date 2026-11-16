#!/usr/bin/env bash
set -u
S=$(cd "$(dirname "$0")" && pwd); WORK=$S/work; REPS=3
prep() { rm -rf "$WORK" "$S/qb" "$S/jb.tsv"; cp -al "$S/pristine" "$WORK"; (cd "$WORK" && git status --porcelain >/dev/null 2>&1); sync; sleep 1; }
one() { local v=$1 cmd=$2 t0 t1; cd "$WORK"; t0=$(date +%s%N)
  case $v in
    bare) bash -c "$cmd" >/dev/null 2>&1 ;;
    v2-sync) PREWRITE_SYNC=1 "$S/prewrite2" -q "$S/qb" -j "$S/jb.tsv" -- bash -c "$cmd" >/dev/null 2>"$S/last.err" ;;
  esac; t1=$(date +%s%N); cd "$S"; echo $(( (t1 - t0) / 1000000 )); }
wl() { local label=$1 cmd=$2; echo "## $label"
  for v in bare v2-sync; do local times=() st=""
    for ((r = 0; r < REPS; r++)); do prep; times+=("$(one "$v" "$cmd")"); [ "$v" != bare ] && st=$(grep -o 'notifs=[0-9]* links=[0-9]* copies=[0-9]*' "$S/last.err"); done
    printf "%-8s %6s ms (runs: %s) %s\n" "$v" "$(printf "%s\n" "${times[@]}" | sort -n | sed -n 2p)" "${times[*]}" "$st"; done; }
wl "git checkout -- . && git clean -fdx (31k deletions)" "git checkout -- . && git clean -fdxq"
wl "rm -rf src (100k+ files)" "rm -rf src"
wl "git status -uall" "git status --porcelain -uall"
