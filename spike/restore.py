#!/usr/bin/env python3
"""Undo everything recorded in a prewrite journal, newest entry first.

Journal columns: seq, pid, kind, path, quarantine_name, extra
kinds: link/copy (pre-image of path), created (path did not exist),
       rmdir (directory existed), rename (path moved to extra)
"""
import os
import sys


def restore(journal, qdir):
    rows = [line.rstrip("\n").split("\t") for line in open(journal) if line.strip()]
    stats = {"restored": 0, "removed": 0, "dirs": 0, "unrenamed": 0, "problems": 0}
    for seq, pid, kind, path, qname, extra in reversed(rows):
        path = os.path.normpath(path)
        try:
            if kind in ("link", "copy"):
                os.makedirs(os.path.dirname(path), exist_ok=True)
                tmp = path + ".saqta-restore"
                src = os.path.join(qdir, qname)
                try:
                    os.link(src, tmp)  # keep the quarantine copy intact
                except OSError:
                    with open(src, "rb") as s, open(tmp, "wb") as d:
                        d.write(s.read())
                    os.chmod(tmp, os.stat(src).st_mode & 0o7777)
                if os.path.isdir(path) and not os.path.islink(path):
                    os.rmdir(path)
                os.replace(tmp, path)
                stats["restored"] += 1
            elif kind == "created":
                if os.path.lexists(path) and not os.path.isdir(path):
                    os.unlink(path)
                    stats["removed"] += 1
            elif kind == "rmdir":
                os.makedirs(path, exist_ok=True)
                stats["dirs"] += 1
            elif kind == "rename":
                new = os.path.normpath(extra)
                if os.path.lexists(new) and not os.path.lexists(path):
                    os.makedirs(os.path.dirname(path), exist_ok=True)
                    os.rename(new, path)
                    stats["unrenamed"] += 1
        except OSError as e:
            stats["problems"] += 1
            print(f"problem undoing {kind} {path}: {e}", file=sys.stderr)
    return stats


if __name__ == "__main__":
    print(restore(sys.argv[1], sys.argv[2]))
