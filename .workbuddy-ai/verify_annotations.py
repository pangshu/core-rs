import subprocess, re, os, sys

ROOT = r"E:\work\rust\core-rs"

def git_show_head(path):
    out = subprocess.run(["git", "-C", ROOT, "show", f"HEAD:{path}"], capture_output=True)
    if out.returncode != 0:
        return None
    return out.stdout.decode("utf-8", "replace")

def strip_trailing_comment(line):
    # 去掉首个「空白 + //」到行尾的内容（对称处理，用于比对代码本体）
    return re.sub(r"\s//.*$", "", line).rstrip()

files = subprocess.run(["git", "-C", ROOT, "diff", "--name-only", "HEAD"],
                       capture_output=True).stdout.decode().split()
rs = [f for f in files if f.endswith(".rs")]

bad = []
total_added = 0
for f in rs:
    old = git_show_head(f)
    if old is None:
        bad.append((f, "NO_HEAD", "", ""))
        continue
    with open(os.path.join(ROOT, f.replace("/", os.sep)), "rb") as fh:
        cur = fh.read().decode("utf-8", "replace")
    old_l = [strip_trailing_comment(l) for l in old.replace("\r\n", "\n").split("\n")]
    cur_l = [strip_trailing_comment(l) for l in cur.replace("\r\n", "\n").split("\n")]
    if len(old_l) != len(cur_l):
        bad.append((f, f"LINE_COUNT old={len(old_l)} new={len(cur_l)}", "", ""))
        continue
    for i in range(len(old_l)):
        if old_l[i] != cur_l[i]:
            bad.append((f, i + 1, old_l[i][:80], cur_l[i][:80]))
            break

print(f"modified .rs files: {len(rs)}")
print(f"mismatch files: {len(bad)}")
for b in bad[:60]:
    print(b)
