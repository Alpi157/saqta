# Saqta spike: catching destruction at the system call (November 1 and 2, 2026)

A throwaway prototype, not Saqta. It answers one question: can a seccomp
user-notification supervisor keep the old version of every file an agent
command deletes or overwrites, cheaply enough to run on every command?

## Files
- `prewrite.c`: first version. Records every write-capable open, including new files.
- `prewrite2.c`: faster version. New files opened with O_EXCL and appends skip the
  supervisor, and `PREWRITE_SYNC=1` turns on the synchronous same-CPU wake-up
  (Linux 6.6 and later), which made a 3 to 4 times difference.
- `restore.py`: undoes a journal, newest entry first.
- `treehash.py`: hashes a whole tree (including .git) and compares two snapshots.
- `gen_repo.py`, `make_dirty.py`: build the synthetic 100k-file repo with at-risk state.
- `bench2.sh`, `bench3.sh`, `micro.sh`: the benchmarks. `results/` has the numbers from this run.
- `wsc.c`, `wsc-tests.sh` (November 2): the wrapper, supervisor and command process model
  that `saqta exec` uses. The supervisor is a child subreaper in its own process group,
  outlives the wrapper while background processes run, kills the whole tree if the
  wrapper dies early, and tells the wrapper whether anything is still running so the
  wrapper can reap it. Build with `gcc -O2 -o wsc wsc.c`, run `./wsc-tests.sh` as a
  normal user. All eight scenarios passed; overhead was about 1.4 ms per command.

## Build and try
    gcc -O2 -o prewrite prewrite.c
    gcc -O2 -o prewrite2 prewrite2.c
    mkdir -p /tmp/t && cd /tmp/t && echo hi > a && echo keep > b
    PREWRITE_SYNC=1 /path/to/prewrite2 -q /tmp/q -j /tmp/j.tsv -- bash -c 'rm a; echo new > b'
    python3 /path/to/restore.py /tmp/j.tsv /tmp/q    # a and b are back

## Reproduce the benchmarks
    python3 gen_repo.py pristine 1000 100
    cd pristine && git init -q && git add -A && git commit -qm init && python3 ../make_dirty.py && cd ..
    gcc -O2 -o prewrite prewrite.c && gcc -O2 -o prewrite2 prewrite2.c
    ./bench2.sh; ./bench3.sh; ./micro.sh

The scripts expect `pristine/` next to them and about 3 GB of free disk.

## Caveats
x86_64 only. Not hardened: path reads from the target's memory can be raced by a
hostile program, so this is protection against mistakes, not a security boundary.
Measured as root in a 2-vCPU VM on ext4, so Ubuntu's ptrace (Yama) and AppArmor
limits were not exercised. Single-threaded supervisor. The prewrite tools have no
background-process subreaper (wsc.c shows the fix), no circuit breaker, no rules. The restore does not yet check for
newer user edits before overwriting.
