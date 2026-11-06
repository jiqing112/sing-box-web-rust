#!/usr/bin/env python3
"""把 bat 从 UTF-8 转成 GBK（cp936）就地写回。

deploy/panel.bat 必须保持 GBK 编码（中文 Windows 默认控制台直接可用，
chcp 65001 + UTF-8 在部分 Win10 上会让 cmd 批处理解析器字节错位，实测必坏）。
维护方式：先在别处按 UTF-8 编辑好，再运行
    python tools/bat_to_gbk.py deploy/panel.bat
"""
import sys
from pathlib import Path


def main() -> int:
    if len(sys.argv) != 2:
        print(__doc__)
        return 2
    p = Path(sys.argv[1])
    data = p.read_bytes()
    if data.startswith(b"\xef\xbb\xbf"):
        data = data[3:]  # 去掉 BOM
    text = data.decode("utf-8")
    p.write_bytes(text.encode("gbk"))
    print(f"{p}: utf-8 -> gbk, {p.stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
