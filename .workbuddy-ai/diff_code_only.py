import subprocess, re, os, difflib

ROOT = r"E:\work\rust\core-rs"
files = [
    "src/app.rs", "src/state.rs",
    "src/config/sections/auth.rs", "src/config/sections/server.rs",
    "src/middleware/auth.rs", "src/middleware/csrf.rs",
]

def strip(l):
    return re.sub(r"\s//.*$", "", l).rstrip()

for f in files:
    old = subprocess.run(["git", "-C", ROOT, "show", f"HEAD:{f}"],
                         capture_output=True).stdout.decode("utf-8", "replace")
    cur = open(os.path.join(ROOT, f.replace("/", os.sep)), "rb").read().decode("utf-8", "replace")
    a = [strip(x) for x in old.replace("\r\n", "\n").split("\n")]
    b = [strip(x) for x in cur.replace("\r\n", "\n").split("\n")]
    print("=" * 25, f)
    for line in difflib.unified_diff(a, b, lineterm="", n=0):
        if line.startswith(("---", "+++", "@@")):
            continue
        if line.strip() == "":
            continue
        print(line[:150])
