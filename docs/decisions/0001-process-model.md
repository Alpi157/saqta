# W, S and C lifetimes

## Context
A shell can exit while its background jobs still use a seccomp listener. Terminal signals must reach the command while the supervisor stays alive.

## Decision
W remains the caller's process and forks S before changing its signal handling. S becomes a subreaper in a separate process group, then forks C into W's group. C installs the listener and transfers it to S with SCM_RIGHTS. S immediately replaces standard streams with /dev/null and changes directory to /.

S polls the listener, SIGCHLD signalfd and W socket. On listener POLLHUP, S reaps children with blocking waits before sending status. C's exit alone does not mean all filtered tasks have exited. S sends a single done-or-detached message with C's wait status and the listener's current hangup state. W reaps S only for done; detached S continues serving background jobs.

W forwards TERM and HUP requests through the socket. S signals descendants and escalates after two seconds. If W disappears while C runs, S repeatedly stops descendants found through /proc PPid links until the stopped snapshot stabilizes, then kills them. Subreaping preserves ancestry across setsid and double forks.

## Alternatives considered
One supervisor in the caller's process group would receive terminal signals and keep output pipes open. Waiting for every background job in W would change shell behavior.

## Consequences
S can outlive W. Run files expose live supervisors. Unexpected supervisor failure remains an explicit wrapper error.
