#!/usr/bin/env bash
# Build a review bundle for one milestone: the source at HEAD, the diff since the
# base, every check's full output, and REPORT.md. Runs every check even when one
# fails, then exits non-zero if any failed. Read-only for .git (works inside
# Codex's sandbox, where .git is mounted read-only).
#
#   scripts/review-bundle.sh <milestone> [base-ref]
#   e.g. scripts/review-bundle.sh m1-exec main
set -uo pipefail

milestone=${1:?usage: scripts/review-bundle.sh <milestone> [base-ref]}
case $milestone in *[!A-Za-z0-9._-]*) echo "milestone name: letters, digits, . _ - only" >&2; exit 2 ;; esac
root=$(git --no-optional-locks rev-parse --show-toplevel) || exit 2
cd "$root" || exit 2
g() { git --no-optional-locks "$@"; }
# Keep review builds separate from ad hoc snapshots and normal terminal builds.
# This path belongs to this checkout, including when root is a git worktree.
export CARGO_TARGET_DIR="$root/target/review-build"

base=${2:-}
if [ -z "$base" ]; then
  for b in origin/main main; do g rev-parse -q --verify "$b^{commit}" >/dev/null && base=$b && break; done
fi
[ -n "$base" ] || base=$(g rev-list --max-parents=0 HEAD | tail -n 1)
[ -f review/REPORT.md ] || { echo "review/REPORT.md is missing: write it first (template in AGENTS.md)" >&2; exit 2; }

stamp=$(date +%Y%m%d-%H%M)
out=review/$stamp-$milestone
mkdir -p "$out/checks"
summary=$out/SUMMARY.txt
failed=0

check() {  # check <name> <command...>: run, keep full output, record PASS or FAIL
  local name=$1; shift
  local start=$SECONDS
  { echo "\$ $*"; echo; } > "$out/checks/$name.txt"
  "$@" >> "$out/checks/$name.txt" 2>&1
  local rc=$?
  if [ $rc -eq 0 ]; then printf '%-8s PASS  (%ss)\n' "$name" $((SECONDS - start)) >> "$summary"
  else printf '%-8s FAIL  exit %s (%ss), see checks/%s.txt\n' "$name" $rc $((SECONDS - start)) "$name" >> "$summary"; failed=1; fi
}

{
  echo "Review bundle: $milestone"
  echo "Created: $(date -R)"
  echo "HEAD: $(g rev-parse --short HEAD) $(g log -1 --format=%s)"
  echo "Base: $base ($(g rev-parse --short "$base"))"
  echo
} > "$summary"

check fmt cargo fmt --all -- --check
check clippy cargo clippy --all-targets --locked -- -D warnings
check test cargo test --locked
if [[ "$milestone" == m2-capture* || "$milestone" == m2b-undo* || "$milestone" == m3-codex* || "$milestone" == m4-harness* ]]; then check proc-mem env SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture; fi
if [[ "$milestone" == m4-harness-r1* || "$milestone" == m4-harness-r2* ]]; then
  check release cargo check --release --locked
  if command -v taskset >/dev/null 2>&1 && taskset -c 0,1 true; then
    check bulk-two-cpu taskset -c 0,1 env SAQTA_NO_SYNC_WAKE=1 cargo test --locked --test codex separate_bind_mount_five_thousand_deletes_restore -- --exact --nocapture --test-threads=1
    check timeouts-two-cpu taskset -c 0,1 env SAQTA_NO_SYNC_WAKE=1 cargo test --locked --test timeouts -- --test-threads=1
  else
    echo "SKIP: taskset or CPUs 0,1 unavailable for constrained checks" >> "$summary"
  fi
  if [ -d target/r1-evidence ]; then
    mkdir -p "$out/evidence"
    cp target/r1-evidence/*.log target/r1-evidence/*.patch "$out/evidence/"
  fi
fi
if [[ "$milestone" == m4-harness-r2* && -d target/r2-evidence ]]; then
  mkdir -p "$out/evidence/r2"
  cp target/r2-evidence/*.log target/r2-evidence/*.json target/r2-evidence/*.py "$out/evidence/r2/"
fi
doctor_check() {
  local doctor_dir
  mkdir -p target/saqta-tests
  doctor_dir=$(mktemp -d "$root/target/saqta-tests/review-doctor.XXXXXX") || return 1
  env HOME="$doctor_dir/HOME" XDG_DATA_HOME="$doctor_dir/data" \
    XDG_CONFIG_HOME="$doctor_dir/config" XDG_STATE_HOME="$doctor_dir/state" \
    SAQTA_HOME="$doctor_dir/saqta" \
    CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
    cargo run -q --locked -- doctor
  local rc=$?
  rm -rf -- "$doctor_dir"
  return "$rc"
}
if cargo run -q --locked -- doctor --help >/dev/null 2>&1; then check doctor doctor_check; fi
if command -v cargo-deny >/dev/null 2>&1; then
  advisory_dbs=("${CARGO_HOME:-$HOME/.cargo}"/advisory-dbs/*/.git)
  if [ -d "${advisory_dbs[0]}" ]; then
    check deny cargo deny --offline check
  else
    check deny cargo deny --offline check licenses bans sources
    echo "SKIP: advisory database not fetched; run cargo deny fetch outside the sandbox" >> "$summary"
  fi
fi

# Source and history. Include the current working tree so uncommitted work is reviewable.
g log --stat "$base..HEAD" > "$out/log.txt"
g diff --stat "$base...HEAD" > "$out/diffstat.txt"
g diff "$base...HEAD" > "$out/diff.patch"
g diff HEAD > "$out/uncommitted.patch"
g status --porcelain=v1 > "$out/status.txt"
mkdir -p "$out/source"
g ls-files -z --cached --others --exclude-standard | tar --null --files-from=- -cf - | tar -xf - -C "$out/source"
if [ -s "$out/uncommitted.patch" ] || grep -q '^??' "$out/status.txt"; then
  echo "WARNING: uncommitted or untracked changes exist (see uncommitted.patch and status.txt); source/ includes the working tree." >> "$summary"
fi

# Environment, including whether we ran inside a sandbox
{
  uname -a
  grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release 2>/dev/null
  rustc -V; cargo -V
  echo "ptrace_scope: $(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo 'n/a (no Yama)')"
  grep -E '^(Seccomp|Seccomp_filters|NoNewPrivs):' /proc/self/status
  grep -qi microsoft /proc/version && echo "WSL: yes" || echo "WSL: no"
  [ -w .git ] && echo ".git writable: yes" || echo ".git writable: no (probably Codex's sandbox)"
} > "$out/env.txt" 2>&1

cp review/REPORT.md "$out/REPORT.md"
{ echo; [ $failed -eq 0 ] && echo "ALL CHECKS PASSED" || echo "SOME CHECKS FAILED"; } >> "$summary"

zip_path=$out.zip
if command -v zip >/dev/null 2>&1; then (cd review && zip -qr - "$(basename "$out")") > "$zip_path"
else python3 -m zipfile -c "$zip_path" "$out"; fi
cat "$summary"
rm -rf "$out"
echo "Bundle: $zip_path ($(du -h "$zip_path" | cut -f1))"
exit $failed
