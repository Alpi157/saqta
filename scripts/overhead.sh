#!/usr/bin/env bash
# Release measurements. Every destructive workload uses a fresh disposable tree.
set -euo pipefail
root=$(git --no-optional-locks rev-parse --show-toplevel)
cd "$root"
if [ -z "${SAQTA_BIN:-}" ]; then
  cargo build --release --locked
  export SAQTA_BIN="$root/target/release/saqta"
fi
exec python3 "$root/scripts/overhead.py" "$@"
