#!/usr/bin/env bash
set -euo pipefail

usage() {
    echo 'Usage: scripts/flake-hunt.sh <iterations> [--mode default|ci4|stress] [--test <binary> --filter <name>] [--busy <n>]' >&2
    exit 2
}

iterations=${1:-}
[[ $iterations =~ ^[1-9][0-9]*$ && $((iterations)) == "$iterations" ]] || usage
shift
mode=default
test_binary=
filter=
busy=0
while (($#)); do
    [[ $# -ge 2 && -n $2 && $2 != --* ]] || usage
    case $1 in
        --mode) mode=$2 ;;
        --test) test_binary=$2 ;;
        --filter) filter=$2 ;;
        --busy) busy=$2 ;;
        *) usage ;;
    esac
    shift 2
done
[[ $busy =~ ^(0|[1-9][0-9]*)$ && $((busy)) == "$busy" ]] || usage
[[ -z $test_binary && -z $filter || -n $test_binary && -n $filter ]] || usage

runner=()
case $mode in
    default) ;;
    ci4) runner=(taskset -c 0-3) ;;
    stress) runner=(taskset -c 0,1 env SAQTA_NO_SYNC_WAKE=1) ;;
    *) usage ;;
esac
"${runner[@]}" true

cd -- "$(dirname -- "$0")/.."
root=$(pwd -P)
fixture_root="$root/target/saqta-tests"
mkdir -p "$fixture_root" target/flake-hunt
log_dir=$(mktemp -d "target/flake-hunt/$(date -u +%Y%m%dT%H%M%SZ).XXXXXX")
printf 'Flake hunt logs are in %s\n' "$log_dir" >&2

burners=()
test_pid=
stop_burners() {
    if ((${#burners[@]})); then
        kill "${burners[@]}" 2>/dev/null || :
        wait "${burners[@]}" 2>/dev/null || :
        burners=()
    fi
}
cleanup() {
    stop_burners
    if [[ -n $test_pid ]]; then
        kill "$test_pid" 2>/dev/null || :
        wait "$test_pid" 2>/dev/null || :
    fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

table_header() {
    printf '| Test name | Failures out of %s | First panic line |\n' "$iterations"
    printf '| --- | ---: | --- |\n'
}
if ! cargo test --locked --no-run --color never > "$log_dir/build.log" 2>&1; then
    cat "$log_dir/build.log" >&2
    {
        table_header
        printf '| Build failed before tests ran | 0 runs | See build.log |\n'
    } | tee "$log_dir/summary.md"
    exit 1
fi

seed_file=tests/harness.proptest-regressions
last_seeds="$log_dir/seeds-before.proptest-regressions"
if [[ -f $seed_file ]]; then
    cp -- "$seed_file" "$last_seeds"
else
    : > "$last_seeds"
fi

test_command=(cargo test --locked --no-fail-fast --color never)
if [[ -n $test_binary ]]; then
    test_command+=(--test "$test_binary" -- "$filter")
fi
failed_logs=()
shopt -s nullglob
for ((run=0; run<iterations;)); do
    ((run+=1))
    log="$log_dir/run-$run.log"
    fixture_dir=$(mktemp -d "$fixture_root/r.XXXXXX")
    for ((i=0; i<busy; i++)); do
        "${runner[@]}" yes > /dev/null &
        burners+=("$!")
    done
    # Bash ignores INT and QUIT for background jobs. Give libtest foreground semantics.
    "${runner[@]}" env --default-signal=INT,QUIT SAQTA_TEST_RUN_DIR="$fixture_dir" \
        "${test_command[@]}" > "$log" 2>&1 &
    test_pid=$!
    if wait "$test_pid"; then
        result=PASS
    else
        result="FAIL (exit $?)"
        failed_logs+=("$log")
    fi
    test_pid=
    stop_burners
    printf 'Run %d/%d %s (%s)\n' "$run" "$iterations" "$result" "$log" >&2

    if [[ -f $seed_file ]] && ! cmp -s -- "$last_seeds" "$seed_file"; then
        last_seeds="$log_dir/seeds-after-run-$run.proptest-regressions"
        cp -- "$seed_file" "$last_seeds"
        printf 'WARNING: proptest seeds changed in run %d. The seed file is kept in place and copied next to %s.\n' "$run" "$log" >&2
    fi

    # The fixtures remove their own data on success. Leave panic evidence intact.
    for test_dir in "$fixture_dir"/*/; do
        rmdir -- "$test_dir" 2>/dev/null || :
    done
    if ! rmdir -- "$fixture_dir" 2>/dev/null; then
        printf 'Run %d retained fixtures at %s\n' "$run" "$fixture_dir" >&2
    fi
done

summarize_failed_tests() {
    awk '
        function cell(text) {
            gsub(/&/, "\\&amp;", text)
            gsub(/</, "\\&lt;", text)
            gsub(/\|/, "\\&#124;", text)
            return text
        }
        function read_run(file,    line, name, context, list, failed, panics, first_panic) {
            while ((getline line < file) > 0) {
                if (line ~ /^test [^[:space:]]+ \.\.\. FAILED[[:space:]]*$/) {
                    split(line, fields, /[[:space:]]+/)
                    failed[fields[2]] = 1
                }
                if (line ~ /^---- [^[:space:]]+ (stdout|stderr) ----$/) {
                    split(line, fields, /[[:space:]]+/)
                    context = fields[2]
                }
                if (line == "failures:") {
                    list = 1
                    context = ""
                } else if (list && line ~ /^    [[:alnum:]_:]+$/) {
                    failed[substr(line, 5)] = 1
                } else if (line != "") {
                    list = 0
                }
                if (line ~ /panicked at/) {
                    if (!first_panic) first_panic = line
                    name = context
                    if (!name && match(line, /thread \047[^\047]+\047/)) {
                        name = substr(line, RSTART + 8, RLENGTH - 9)
                    }
                    if (name && !(name in panics)) panics[name] = line
                }
            }
            close(file)
            if (length(failed) == 0) {
                name = "(cargo test exited without a named failure)"
                failed[name] = 1
                panics[name] = first_panic
            }
            for (name in failed) {
                counts[name]++
                if (!first[name]) first[name] = panics[name]
            }
        }
        BEGIN {
            for (i = 1; i < ARGC; i++) read_run(ARGV[i])
            for (name in counts) {
                printf "| %s | %d | %s |\n", cell(name), counts[name], (first[name] ? cell(first[name]) : "(none found)")
            }
        }
    ' "$@" | LC_ALL=C sort
}

if ((${#failed_logs[@]} == 0)); then
    { table_header; printf '| None | 0 | |\n'; } | tee "$log_dir/summary.md"
else
    { table_header; summarize_failed_tests "${failed_logs[@]}"; } | tee "$log_dir/summary.md"
    exit 1
fi
