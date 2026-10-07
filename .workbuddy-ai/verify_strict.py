import subprocess, os

ROOT = r"E:\work\rust\core-rs"


def strip_comments(src: str) -> str:
    """字符串感知地移除 Rust 注释，保留字符串字面量原文。"""
    out = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        # 原始字符串 r"..." / r#"..."# （含 br# 变体：b 会被当普通字符先复制）
        if c == "r" and i + 1 < n and src[i + 1] in ('"', "#"):
            j = i + 1
            hashes = 0
            while j < n and src[j] == "#":
                hashes += 1
                j += 1
            if j < n and src[j] == '"':
                term = '"' + "#" * hashes
                k = src.find(term, j + 1)
                k = n if k == -1 else k + len(term)
                out.append(src[i:k])
                i = k
                continue
        # 普通字符串 "..."（含转义）
        if c == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    j += 1
                    break
                j += 1
            out.append(src[i:j])
            i = j
            continue
        # 字符字面量 'x' / '\n'（区别于生命周期 'a）
        if c == "'":
            if i + 1 < n and src[i + 1] == "\\":
                j = i + 2
                while j < n and src[j] != "'":
                    j += 1
                out.append(src[i:j + 1])
                i = j + 1
                continue
            if i + 2 < n and src[i + 2] == "'":
                out.append(src[i:i + 3])
                i += 3
                continue
            out.append(c)
            i += 1
            continue
        # 行注释
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            i = n if j == -1 else j
            continue
        # 块注释（可嵌套）
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            depth, j = 1, i + 2
            while j < n and depth > 0:
                if src[j:j + 2] == "/*":
                    depth += 1
                    j += 2
                elif src[j:j + 2] == "*/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            i = j
            continue
        out.append(c)
        i += 1
    return "".join(out)


KNOWN_PREEXISTING = {
    "src/app.rs", "src/state.rs", "src/config/sections/auth.rs",
    "src/config/sections/server.rs", "src/middleware/auth.rs", "src/middleware/csrf.rs",
}

files = subprocess.run(["git", "-C", ROOT, "diff", "--name-only", "HEAD"],
                       capture_output=True).stdout.decode().split()
rs = [f for f in files if f.endswith(".rs")]

bad, ok = [], 0
for f in rs:
    old = subprocess.run(["git", "-C", ROOT, "show", f"HEAD:{f}"],
                         capture_output=True).stdout.decode("utf-8", "replace")
    cur = open(os.path.join(ROOT, f.replace("/", os.sep)), "rb").read().decode("utf-8", "replace")
    a = [l.rstrip() for l in strip_comments(old.replace("\r\n", "\n")).split("\n")]
    b = [l.rstrip() for l in strip_comments(cur.replace("\r\n", "\n")).split("\n")]
    if a == b:
        ok += 1
    else:
        bad.append(f)

print(f"total .rs: {len(rs)}   pure-annotation(代码字节级一致): {ok}")
print(f"有代码差异: {len(bad)} -> {bad}")
print(f"其中属于既有未提交改动(已确认): {sorted(set(bad) & KNOWN_PREEXISTING)}")
print(f"未知差异: {sorted(set(bad) - KNOWN_PREEXISTING)}")
