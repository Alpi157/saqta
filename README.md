# Saqta

Saqta is an undo layer for AI coding agents on Linux and WSL2. M2 preserves files before deletion, replacement and in-place writes, records creations, and restores a command's effects in reverse order.

```sh
cargo build --release --locked
saqta doctor
saqta exec -c 'sed -i s/before/after/ file'
saqta log
saqta show 1
saqta undo 1 --dry-run
saqta undo 1
```

Undo runs in your own terminal. It refuses while a command's background tree is running, after a previous undo, or when a touched path has changed since the command. It preserves displaced current files under the undo quarantine with a recovery map. `log` and `show` also accept `--json`.

The default store is `$XDG_DATA_HOME/saqta` or `$HOME/.local/share/saqta`. `SAQTA_HOME` overrides it. `SAQTA_MAX_COPY` limits individual copies in bytes, defaulting to 256 MiB. Hard-linked captures have no size limit. Failed captures are reported in the journal as not protected. Kernel and volatile filesystems are skipped. Pure append opens without creation or truncation bypass capture.

Two hard links modified in place may return as separate files. Capture does not promise power-loss durability or protect against hostile filesystem races. Status: pre-alpha. Don't rely on it yet.

Run `cargo test --locked` and `SAQTA_FORCE_PROC_MEM=1 cargo test --locked --test capture` for the normal and forced memory fallback suites. `scripts/overhead.sh` measures a release build on fresh disposable fixtures. Its defaults are 20,000 operations and three repetitions; `N` and `REPS` override them. It is not run in CI.

The design is described in [capture](docs/decisions/0004-capture.md), [journaling](docs/decisions/0005-journal.md), and [undo safety](docs/decisions/0006-undo-safety.md).
