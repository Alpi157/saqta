import os, random, sys, string
random.seed(42)
root = sys.argv[1]
n_dirs, per_dir = int(sys.argv[2]), int(sys.argv[3])
os.makedirs(root, exist_ok=True)
words = ["".join(random.choice(string.ascii_lowercase) for _ in range(random.randint(3,9))) for _ in range(2000)]
def text(n):
    return "\n".join(" ".join(random.choice(words) for _ in range(10)) for _ in range(n)) + "\n"
for d in range(n_dirs):
    p = os.path.join(root, "src", f"pkg{d//100:02d}", f"mod{d:04d}")
    os.makedirs(p, exist_ok=True)
    for f in range(per_dir):
        with open(os.path.join(p, f"file{f:03d}.py"), "w") as fh:
            fh.write(text(random.randint(10, 60)))
# ignored regenerable tree
nm = os.path.join(root, "node_modules")
for d in range(300):
    p = os.path.join(nm, f"dep{d:03d}", "lib")
    os.makedirs(p, exist_ok=True)
    for f in range(100):
        with open(os.path.join(p, f"x{f:03d}.js"), "w") as fh:
            fh.write(text(5))
with open(os.path.join(root, ".gitignore"), "w") as fh:
    fh.write("node_modules/\n.env\ndata/\n*.log\n")
