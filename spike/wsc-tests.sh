#!/bin/bash
# Scenario checks for the W/S/C prototype. Run as a normal user.
W=${WSC:-./wsc}
T=$(mktemp -d /tmp/wsc.XXXX); export WSC_LOG=$T/s.log
ms() { echo $(( $(date +%s%N) / 1000000 )); }
live() { ps -u $(id -u) -o pid=,stat=,comm= | awk -v n="$1" '$3==n && $2 !~ /Z/'; }
echo "## 1 exit codes"; $W 'exit 3'; echo "exit=$? (want 3)"
$W 'kill -TERM $$'; echo "exit=$? (want 143, killed by SIGTERM)"
(cd $T && $W 'kill -SEGV $$'); echo "exit=$? (want 139)"; ls $T/core* 2>/dev/null && echo "FAIL: core file"
echo "## 2 stdin and stdout pass through"; echo hello | $W 'tr a-z A-Z'
echo "## 3 background job: wrapper returns fast, supervisor keeps serving"
touch $T/f1; t0=$(ms); $W "sh -c 'sleep 1; rm $T/f1' >/dev/null 2>&1 & echo started"; t1=$(ms)
echo "wrapper returned in $((t1 - t0)) ms (want < 100)"
sleep 1.5; [ -e $T/f1 ] && echo "FAIL: f1 still exists" || echo "ok: background rm was served after the wrapper returned"
sleep 0.3; [ -n "$(live wsc)" ] && { live wsc; echo "FAIL: supervisor still alive"; } || echo "ok: supervisor exited after the last filtered process"
echo "## 4 supervisor does not hold the agent's stdout open"
t0=$(ms); out=$($W 'sleep 0.2; echo hi'); t1=$(ms); echo "got '$out' after $((t1 - t0)) ms (want ~200)"
echo "## 5 command joins the wrapper's process group; supervisor has its own"
$W 'echo "C pgid=$(ps -o pgid= -p $$ | tr -d " ") S pgid=$(ps -o pgid= -p $PPID | tr -d " ")"'; echo "W pgid=$(ps -o pgid= -p $$ | tr -d ' ') (the calling shell's group)"
echo "## 6 kill -9 the wrapper: supervisor kills the whole tree, including a setsid'd grandchild"
$W "setsid sh -c 'sleep 30; touch $T/escaped' & sleep 30; touch $T/finished" & wpid=$!
sleep 0.5; before=$(live sleep | wc -l); kill -9 $wpid; sleep 0.5
after=$(live sleep | wc -l); echo "live sleeps before=$before after=$after (want 2 then 0)"
[ -n "$(live wsc)" ] && { live wsc; echo "FAIL: supervisor still alive"; } || echo "ok: supervisor gone"
echo "## 7 nested: a second listener in the same chain"
$W "$W 'true'"; echo "exit=$? (expect EBUSY: nested sessions need pass-through)"
sleep 0.3; [ -n "$(live wsc)" ] && { live wsc; echo "FAIL: stuck supervisors"; } || echo "ok: nothing stuck"
echo "## 8 overhead of the model (median and p95 of 50 runs of 'true')"
for i in $(seq 50); do t0=$(date +%s%N); $W true; t1=$(date +%s%N); echo $(( (t1 - t0) / 1000 )); done | sort -n | sed -n '25p;48p' | paste -sd' ' | awk '{print "wsc true:       median " $1 " us, p95 " $2 " us"}'
for i in $(seq 50); do t0=$(date +%s%N); /bin/sh -c true; t1=$(date +%s%N); echo $(( (t1 - t0) / 1000 )); done | sort -n | sed -n '25p;48p' | paste -sd' ' | awk '{print "sh -c true:     median " $1 " us, p95 " $2 " us"}'
echo "## supervisor log"; sort $WSC_LOG | uniq -c | sort -rn | head -20
rm -rf $T
