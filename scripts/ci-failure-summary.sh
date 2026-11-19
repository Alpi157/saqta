#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 1 ]]; then
    echo 'Usage: scripts/ci-failure-summary.sh <log dir>' >&2
    exit 2
fi

shopt -s nullglob
logs=("$1"/*.log)
printf '## Cargo test failures\n\n'
if ((${#logs[@]} == 0)); then
    echo 'No cargo test logs were saved.'
    exit 0
fi

# Show test names and minimal inputs before the panics from individual shrink attempts.
awk '
    function emit(line) {
        if (++lines <= 296) print line
    }
    function show(title, line,    name) {
        if (!section) {
            emit("")
            emit("### " title)
            section = 1
        }
        if (!heading) {
            name = FILENAME
            sub(/^.*\//, "", name)
            emit("")
            emit("**" name "**")
            emit("")
            heading = 1
        }
        emit("    " line)
    }
    FNR == 1 { heading = context = input = listing = 0 }
    phase != previous_phase { section = 0; previous_phase = phase }
    phase == "tests" {
        if (/^test [^[:space:]]+ \.\.\. FAILED[[:space:]]*$/) {
            show("Failed tests", $0)
            seen[FILENAME, $2] = 1
        }
        # With --nocapture, other output can break up the test result line.
        if ($0 == "failures:") {
            listing = 1
        } else if (listing && /^    [[:alnum:]_:]+$/) {
            if (!seen[FILENAME, $1]++) show("Failed tests", "test " $1 " ... FAILED")
        } else if (NF) {
            listing = 0
        }
        next
    }
    phase == "inputs" {
        if (/minimal failing input:/) input = 1
        if (/^[[:space:]]*successes:[[:space:]]*[0-9]+/) input = 0
        if (input) show("Proptest minimal failing inputs", $0)
        next
    }
    /panicked at/ { context = 7 }
    context { show("Panics", $0); context-- }
    END {
        if (!lines) print "No failed tests, panics or proptest inputs found in the saved logs."
        if (lines > 296) print "\nSummary truncated. See the full logs in the test evidence artifact."
    }
' phase=tests "${logs[@]}" phase=inputs "${logs[@]}" phase=panics "${logs[@]}"
