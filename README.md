# Saqta

Saqta is an undo layer for AI coding agents on Linux and WSL2. It runs commands under a seccomp user notification supervisor so destructive filesystem operations can be preserved before they happen. M1 implements command supervision only; it does not capture files or provide undo.

Status: pre-alpha. Don't rely on it yet.
