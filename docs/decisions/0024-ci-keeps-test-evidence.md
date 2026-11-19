# CI keeps test evidence

## Context

PR #7 failed `cargo test` on Ubuntu 24.04 and passed on rerun. The first
attempt left no artifact or summary. Its log was initially unavailable and
was later recovered through the GitHub connector. Ordinary CI runs the
filesystem harness with 64 fresh proptest cases per property. A failing seed
is written to the runner checkout and vanishes when the job ends unless CI
saves it. [PR #7 CI run](https://github.com/Alpi157/saqta/actions/runs/44281430824)

## Decision

Use Bash with `pipefail` and tee each Cargo test step into its own log. Run
the full suite with `--no-fail-fast`. On failure, publish a short step summary
and upload the full test logs, seed file, diagnostic logs and harness fixtures.
Include the run attempt in the artifact name and retain it for 14 days. Use
a manual workflow for repeated runs under selectable CPU and wakeup settings.

## Alternatives considered

Rerunning on failure would lose the original evidence. Pinning all ordinary
CI seeds would reduce coverage of new cases. Archiving the entire test tree
would also include unrelated fixtures and quarantine stores.

## Consequences

Reviewers can identify a failed test and recover a new seed before rerunning.
Ordinary CI still explores fresh cases. Repeated runs consume runner time only
when dispatched. A long shrink can put more detail in the full logs than fits
in the summary.
