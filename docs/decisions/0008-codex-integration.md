# Codex project hooks and namespace lifetime

## Context

Codex runs trusted hooks outside its sandbox, but rewritten shell commands still
run inside it. Writable roots are separate mounts. Its wrapper may be PID 1,
whose exit kills the supervisor and background processes in that namespace.

## Decision

Use three project-local hooks. Bash PreToolUse rewrites the complete command as
one single-quoted argument to an absolute Saqta binary. Choose bash or zsh from
the passwd login shell, falling back to bash. Hook errors are silent, return zero,
and append reasons to hook.log.

Patch PreToolUse uses M2's capture, copy limit and journal directly. Record only
named paths, with copies rather than shared hard links. A sidecar records session
and tool-use IDs. PostToolUse reopens that journal and writes post-state and end.
Missing post events remain incomplete. Denied patch paths are logged without
blocking the patch.

When W is PID 1, S finishes the journal before sending detached status. W's existing
signal re-raise fallback returns 128 plus the signal when PID 1 ignores it.
Cross-mount hard-link failures keep bounded copies and an explicit capture label.

Init merges project settings using serde_json and toml_edit, keeps backups and
installation receipts, and records canonical projects. Uninstall restores exact
original bytes when possible. For later edits it removes proved owned spans or
structurally recognized entries. Ambiguity refuses before settings change.

## Alternatives considered

Global hooks would affect unrelated projects. A hook-side supervisor would run
the command outside the intended sandbox. Blindly restoring backups would lose
later settings changes. Waiting for background jobs would change shell behavior.

## Consequences

Users must trust three hooks once. Separate mounts increase copying. Hooks cannot
make Codex stop abandoned commands. Existing M2 capture races and crash limits
remain. Installation ownership receipts must be kept for safe uninstall.
