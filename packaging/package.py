"""打包发布 zip：python -I packaging/package.py
先执行 cargo build --release。产物：dist/SwiftDL-v<版本>-windows-x64.zip
"""
import pathlib
import re
import sys
import zipfile

root = pathlib.Path(__file__).resolve().parent.parent
version = re.search(r'^version\s*=\s*"([^"]+)"', (root / "Cargo.toml").read_text("utf-8"), re.M).group(1)
exe = root / "target" / "release" / "swiftdl.exe"
if not exe.exists():
    sys.exit("找不到 target/release/swiftdl.exe，请先运行 cargo build --release")

name = f"SwiftDL-v{version}-windows-x64"
out = root / "dist" / f"{name}.zip"
out.parent.mkdir(exist_ok=True)

# 说明文件转为 UTF-8 BOM + CRLF，记事本打开不乱码
guide = (root / "packaging" / "使用说明.txt").read_text("utf-8").replace("\r\n", "\n").replace("\n", "\r\n")

with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
    z.write(exe, f"{name}/SwiftDL.exe")
    z.writestr(f"{name}/使用说明.txt", "﻿" + guide)
    ext = root / "extension"
    for f in sorted(ext.rglob("*")):
        rel = f.relative_to(ext)
        if f.is_file() and rel.parts[0] != "tests":
            z.write(f, f"{name}/extension/{rel.as_posix()}")

print(f"{out}  {out.stat().st_size / 1048576:.2f} MB")
with zipfile.ZipFile(out) as z:
    for i in z.infolist():
        print(f"  {i.file_size:>10}  {i.filename}")
