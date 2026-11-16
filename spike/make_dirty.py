#!/usr/bin/env python3
"""Give a generated repo some at-risk state: 200 uncommitted edits, 300
untracked files, .env, a 50 MB local database and some logs.
Run inside the repo after the initial commit."""
import os
import random

random.seed(7)
files = [os.path.join(r, f) for r, _, fs in os.walk("src") for f in fs]
for p in random.sample(files, 200):
    with open(p, "a") as fh:
        fh.write("# uncommitted user edit\n" * 5)
os.makedirs("src/newfeature", exist_ok=True)
for i in range(300):
    with open(f"src/newfeature/new{i:03d}.py", "w") as fh:
        fh.write("def f():\n    return %d\n" % i * 20)
with open(".env", "w") as fh:
    fh.write("API_KEY=sk-test-123\nDB_URL=postgres://localhost/dev\n")
os.makedirs("data", exist_ok=True)
with open("data/dev.db", "wb") as fh:
    fh.write(os.urandom(50 * 1024 * 1024))
for i in range(20):
    with open(f"build{i}.log", "w") as fh:
        fh.write("log line\n" * 100)
