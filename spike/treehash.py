#!/usr/bin/env python3
"""Hash every file, symlink and directory under a root (including .git).
Prints a JSON map path -> digest, or compares two such maps."""
import hashlib
import json
import os
import stat
import sys


def snapshot(root):
    out = {}
    for d, dirs, files in os.walk(root):
        rel_d = os.path.relpath(d, root)
        out[rel_d + "/"] = "dir:%o" % (os.lstat(d).st_mode & 0o7777)
        for f in files:
            p = os.path.join(d, f)
            st = os.lstat(p)
            rel = os.path.relpath(p, root)
            if stat.S_ISLNK(st.st_mode):
                out[rel] = "link:" + os.readlink(p)
            elif stat.S_ISREG(st.st_mode):
                h = hashlib.sha1()
                with open(p, "rb") as fh:
                    for chunk in iter(lambda: fh.read(1 << 20), b""):
                        h.update(chunk)
                out[rel] = "%s:%o" % (h.hexdigest(), st.st_mode & 0o7777)
    return out


if __name__ == "__main__":
    if sys.argv[1] == "snap":
        json.dump(snapshot(sys.argv[2]), open(sys.argv[3], "w"))
    else:
        a, b = json.load(open(sys.argv[2])), json.load(open(sys.argv[3]))
        missing = [k for k in a if k not in b]
        extra = [k for k in b if k not in a]
        changed = [k for k in a if k in b and a[k] != b[k]]
        print(f"entries={len(a)} missing={len(missing)} extra={len(extra)} changed={len(changed)}")
        for name, lst in (("missing", missing), ("extra", extra), ("changed", changed)):
            for k in lst[:10]:
                print(f"  {name}: {k}")
