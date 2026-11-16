# Protect a Codex project

Run `saqta init --codex` in your project's git repository, outside Codex.
You can also pass the project directory explicitly. Saqta uses the canonical
git root. It refuses its own repository because its tests run supervisors and
the kernel allows one seccomp listener per process chain.

Init adds three project hooks in `.codex/hooks.json`: Bash before execution,
apply_patch before execution, and apply_patch after execution. Each points at
the absolute path of the binary you ran and displays `saqta` as its status.
Existing hook entries and keys survive. The JSON may be reformatted.

Init adds the resolved SAQTA_HOME to `sandbox_workspace_write.writable_roots`
in `.codex/config.toml`. It preserves other values and comments. This makes the
store writable inside Codex's default sandbox. It does not grant the shell
additional permissions or bypass approvals.

Existing files get `.saqta-backup` copies. Repeated init keeps the first backup.
Init excludes these two files through `.git/info/exclude`. If either file is
already tracked, it prints a warning and leaves that file out of the exclude
list. It records the project in `$SAQTA_HOME/projects`. Installation receipts
in `$SAQTA_HOME/codex-installs.json` retain the original bytes for uninstall.

Open Codex in this project, type /hooks, and trust the three saqta hooks. Then run
`saqta doctor` through Codex to check that protection works inside the sandbox.
New and changed hooks need trust once. Moving the binary requires reinstalling
the hooks. Use `saqta status` to see hook paths, writable roots and recent commands.

# What is protected

Bash commands run through `saqta exec` using your bash or zsh login shell, or
`/bin/bash` otherwise. Quoting preserves the command string, including heredocs.
Normal sandbox restrictions and Codex approvals still apply. The hooks run
outside the sandbox as you. Set SAQTA_OFF=1 in the hook environment to bypass them.
Errors leave the tool unchanged and go to `$SAQTA_HOME/logs/hook.log`.

The patch hook copies existing named files and records new names before
apply_patch runs. It saves post-state after the matching tool call. Missing
post events leave incomplete commands, just as an interrupted supervised command
does. Use `saqta log`, `saqta show ID` and `saqta undo ID` outside the supervised
shell to review and restore. M2's conflict checks and copy size limit still apply.
Paths in the store and agent settings get no patch pre-images. The patch is not
blocked. Other tools have no hook coverage.

Codex binds each writable root separately. A delete usually needs a copy rather
than a hard link, even when both roots use the same filesystem. The journal
labels that fallback `copied (cross-mount)`. Large files and unsupported special
files can remain unprotected; inspect the journal's reasons.

The wrapper can be PID 1 in Codex's namespace. The supervisor writes post-state
and the end record before returning status, including when background jobs remain.
Background jobs die when the namespace exits, as they do without Saqta.
Stopping or quitting Codex may leave the whole command tree running. Saqta cannot
fix that from inside the sandbox. It keeps the existing process behavior.

Doctor tests capture in a temporary `.saqta-doctor/` folder in the current project
and removes the fixture. An existing Saqta supervisor triggers this check even
when the host uses systemd as PID 1. It verifies the real
creation and deletion effects in that supervisor's plain log. Otherwise it checks
that the captured pre-image matches the sample bytes. It reports
`inside Codex sandbox: capture works` or the reason capture failed.

# Uninstall

Run `saqta uninstall --codex [PROJECT]` outside Codex. Unchanged installation
files return to their exact original bytes. Files created by init are removed.
Later unrelated edits are retained where ownership can be proved. An ambiguous
edit causes refusal before settings are changed. Keep both versions and review
the named file, rather than discarding newer settings. Backups remain available.
The project leaves the protected-project list. Journals and kept file versions
remain available for undo.
