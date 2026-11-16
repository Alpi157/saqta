# Fail open unless strict

## Context
Some kernels and sandboxes cannot create seccomp user notification listeners. Nested listeners can return EBUSY. Saqta should remain useful as a command runner without silently claiming protection.

## Decision
Resource and listener setup failures run the prepared command directly with one explicit not-protected warning. SAQTA_STRICT=1 exits 125 instead. EBUSY has a specific warning about another active supervisor. A supervised environment marker together with /proc/self/status filter mode allows silent direct execution for normal nesting.

A child that sends no listener closes the handoff socket on exec or exit. S observes EOF, continues tracking C's status and exits after reporting it. It does not wait for a listener that can never arrive. W restores its original INT and QUIT dispositions before a supervisor initialization fallback.

## Alternatives considered
Always refusing execution would break existing workflows. Silent fallback would create false confidence. Using only an environment marker would trust stale state.

## Consequences
The warning is part of the user contract. M1 never captures data, including when supervision works. A failed filter installation may leave no_new_privs enabled in C.
