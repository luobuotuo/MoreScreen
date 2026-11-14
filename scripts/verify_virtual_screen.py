#!/usr/bin/env python
# -*- coding: utf-8 -*-
"""虚拟屏就绪检测 + 扩展屏采集闭环验证。

用途
----
在"虚拟屏被点亮"之后（本机有两条路径：
  A. ToDesk/Parsec 客户端建立一次远程连接 —— 会话型 IddCx 驱动会在连接时创建显示器；
  B. MttVDD 常驻型驱动被系统加载 —— vdd_settings.xml 里 <monitors><count>1</count>，
     驱动加载即自动创建 1 块显示器），
一次性完成：

  1. 轮询等待「分辨率非 0x0」的虚拟屏出现；
  2. 对该屏做 WGC 采集探测（帧率 / 首帧延迟 / 亮度自检）；
  3. 导出首帧 NV12 并转成 PNG，用于肉眼确认画面不是黑屏。

用法
----
    python scripts/verify_virtual_screen.py                 # 默认等待 120 秒
    python scripts/verify_virtual_screen.py --timeout 300   # 等更久
    python scripts/verify_virtual_screen.py --no-wait       # 只检查当前状态

环境变量
--------
    RS_HOST_BIN   指定 rs-host.exe 路径（否则自动查找）
"""

import argparse
import io as io_open
import json
import os
import struct
import subprocess
import sys
import time
import zlib

DEFAULT_OUT = "out"


# --------------------------------------------------------------------------- 工具
def posix_to_win(p):
    """把 Git Bash 的 POSIX 路径转成 Windows 路径：/c/Users/x -> C:\\Users\\x"""
    if len(p) > 2 and p[0] == "/" and p[2] == ":":
        return p[2].upper() + ":" + p[3:].replace("/", "\\")
    return p


def target_dir_from_envsh():
    """从 scripts/env.sh 解析 CARGO_TARGET_DIR（最可靠，且跨机器通用）。"""
    here = os.path.dirname(os.path.abspath(__file__))
    env_sh = os.path.join(here, "env.sh")
    if not os.path.isfile(env_sh):
        return None
    try:
        import re
        txt = io_open(env_sh, encoding="utf-8").read()
        m = re.search(r'CARGO_TARGET_DIR="\$\{CARGO_TARGET_DIR:-([^"]+)\}"', txt)
        if m:
            return posix_to_win(m.group(1))
        m = re.search(r'CARGO_TARGET_DIR="?([^"\n]+)"?', txt)
        if m:
            return posix_to_win(m.group(1).strip())
    except Exception:
        pass
    return None


def find_host_bin(explicit=""):
    """定位 rs-host.exe。"""
    cands = []
    if explicit:
        cands.append(explicit)
    env = os.environ.get("RS_HOST_BIN", "").strip()
    if env:
        cands.append(env)
    # 首选：scripts/env.sh 里声明的 CARGO_TARGET_DIR
    td = target_dir_from_envsh()
    if td:
        cands.append(os.path.join(td, "release", "rs-host.exe"))
    # 兜底：常见临时目录与仓库内 target
    for t in (os.environ.get("TEMP", ""), os.environ.get("TMP", ""),
              r"C:\Users\sz-ws016\AppData\Local\Temp"):
        if t:
            cands.append(os.path.join(posix_to_win(t), "rs-target", "release", "rs-host.exe"))
    here = os.path.dirname(os.path.abspath(__file__))
    cands.append(os.path.join(here, "..", "target", "release", "rs-host.exe"))
    cands.append(os.path.join(here, "..", "target", "debug", "rs-host.exe"))
    for c in cands:
        c = os.path.abspath(c)
        if os.path.isfile(c):
            return c
    return None


def run(bin_path, args, timeout=60):
    """执行 rs-host 并返回 (returncode, stdout+stderr)。"""
    try:
        p = subprocess.run([bin_path] + args, capture_output=True, timeout=timeout)
        out = (p.stdout or b"").decode("utf-8", errors="replace")
        err = (p.stderr or b"").decode("utf-8", errors="replace")
        return p.returncode, out + err
    except subprocess.TimeoutExpired:
        return -1, f"命令超时: {' '.join(args)}"
    except OSError as e:
        return -2, f"无法执行: {e}"


def list_displays(bin_path):
    """返回显示目标列表；失败返回空列表。"""
    rc, out = run(bin_path, ["displays", "--json"], timeout=30)
    if rc != 0:
        return []
    # 从输出里找 JSON 数组（前面可能混有日志）
    start = out.find("[")
    end = out.rfind("]")
    if start < 0 or end <= start:
        return []
    try:
        return json.loads(out[start:end + 1])
    except json.JSONDecodeError:
        return []


def describe(displays):
    """打印显示目标概览。"""
    virt = [d for d in displays if d.get("virtual")]
    phys = [d for d in displays if not d.get("virtual")]
    live_virt = [d for d in virt if d.get("width", 0) > 0 and d.get("height", 0) > 0]
    print(f"  物理屏 {len(phys)} 个 / 虚拟目标 {len(virt)} 个 / "
          f"已点亮的虚拟屏 {len(live_virt)} 个")
    for i, d in enumerate(displays):
        if d.get("virtual") and d.get("width", 0) > 0:
            print(f"    [{i}] ✅ {d['id']:<52} {d['width']}x{d['height']}@{d.get('refresh_hz', 0)}"
                  f"  active={d.get('active')}")
    if not live_virt:
        # 打印几个候选，让用户知道还差什么
        ids = {}
        for d in virt:
            ids.setdefault(d["id"], 0)
            ids[d["id"]] += 1
        print("    当前虚拟目标（均未点亮，分辨率 0x0）：")
        for k, v in ids.items():
            print(f"      - {k}  x{v}")
    return live_virt


def pick_target(displays):
    """选出采集目标：优先活动 + 虚拟，其次分辨率最大。返回 (索引, 显示信息)。"""
    live = [(i, d) for i, d in enumerate(displays)
            if d.get("virtual") and d.get("width", 0) > 0 and d.get("height", 0) > 0]
    if not live:
        live = [(i, d) for i, d in enumerate(displays)
                if d.get("width", 0) > 0 and d.get("height", 0) > 0]
    if not live:
        return None, None
    live.sort(key=lambda t: (0 if t[1].get("active") else 1,
                             -(t[1].get("width", 0) * t[1].get("height", 0))))
    return live[0]


# --------------------------------------------------------------------------- NV12 → PNG
COMMON_RES = [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900),
               (1366, 768), (1280, 720), (1024, 768), (800, 600)]


def resolve_nv12_size(total, width=0, height=0):
    """确定 NV12 帧的真实分辨率。

    NV12 文件本身不带尺寸头，必须外部提供或按已知分辨率表匹配——
    **不能只靠像素总数反推**（总数可被任意高度整除，会得到荒谬的宽高）。
    """
    if width and height and total == width * height * 3 // 2:
        return width, height, "来自显示目标模式"
    for w, h in COMMON_RES:
        if total == w * h * 3 // 2:
            return w, h, "按已知分辨率表匹配"
    return None, None, ""


def nv12_to_png(nv12_path, png_path, width=0, height=0):
    """把 NV12 帧转成 PNG（等比降采样），用于肉眼确认。"""
    data = open(nv12_path, "rb").read()
    total = len(data)
    W, H, how = resolve_nv12_size(total, width, height)
    if W is None:
        print(f"  [FAIL] 无法确定 NV12 分辨率（{total} 字节）；"
              f"请确认该分辨率在已知表内或已提供 width/height")
        return False
    print(f"  分辨率: {W}x{H}（{how}，{total} 字节）")
    cw, ch = W // 2, H // 2
    if len(data) < W * H + 2 * cw * ch:
        print("  [FAIL] NV12 数据长度不足")
        return False
    Y = data[:W * H]
    U = data[W * H:W * H + cw * ch]
    V = data[W * H + cw * ch:W * H + 2 * cw * ch]
    ow, oh = W // 2, H // 2
    rows = []
    for oy in range(oh):
        row = bytearray(b"\x00")
        sy = oy * 2
        for ox in range(ow):
            sx = ox * 2
            yv = Y[sy * W + sx]
            u = U[(sy // 2) * cw + (sx // 2)] - 128
            v = V[(sy // 2) * cw + (sx // 2)] - 128
            r = max(0, min(255, int(yv + 1.402 * v)))
            g = max(0, min(255, int(yv - 0.344136 * u - 0.714136 * v)))
            b = max(0, min(255, int(yv + 1.772 * u)))
            row += bytes((r, g, b))
        rows.append(bytes(row))
    raw = b"".join(rows)

    def chunk(tag, payload):
        return (struct.pack(">I", len(payload)) + tag + payload
                + struct.pack(">I", zlib.crc32(tag + payload) & 0xffffffff))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", ow, oh, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw, 6))
    png += chunk(b"IEND", b"")
    open(png_path, "wb").write(png)
    print(f"  [OK] PNG 已生成: {png_path}  ({ow}x{oh}, {os.path.getsize(png_path)/1024:.0f} KB)")
    return True


# --------------------------------------------------------------------------- 主流程
def main():
    ap = argparse.ArgumentParser(description="虚拟屏就绪检测 + 扩展屏采集闭环验证")
    ap.add_argument("--bin", default="", help="rs-host.exe 路径")
    ap.add_argument("--timeout", type=int, default=120, help="等待虚拟屏出现的秒数")
    ap.add_argument("--no-wait", action="store_true", help="只检查当前状态，不等待")
    ap.add_argument("--seconds", type=int, default=5, help="采集探测时长（秒）")
    ap.add_argument("--out", default=DEFAULT_OUT, help="输出目录")
    args = ap.parse_args()

    bin_path = find_host_bin(args.bin)
    if not bin_path:
        print("[FAIL] 找不到 rs-host.exe，请先 cargo build --release，"
              "或用 --bin / 环境变量 RS_HOST_BIN 指定路径")
        return 2
    print(f"[INFO] rs-host: {bin_path}")

    os.makedirs(args.out, exist_ok=True)

    # ---- 步骤 1：等待虚拟屏出现 ----
    print("\n=== 步骤 1/3：检测显示目标 ===")
    deadline = time.time() + (0 if args.no_wait else args.timeout)
    displays, live = [], []
    while True:
        displays = list_displays(bin_path)
        if not displays:
            print("  rs-host displays 执行失败")
            return 3
        live = describe(displays)
        if live:
            break
        if time.time() >= deadline:
            print(f"\n[FAIL] 等待 {args.timeout}s 内没有出现有分辨率的虚拟屏。")
            print("       请确认：")
            print("       · 路线 A：ToDesk/Parsec 客户端已建立远程连接（会话型驱动才会建显示器）")
            print("       · 路线 B：MttVDD 已加载（常驻型驱动，加载即自动建 1 块显示器）")
            return 4
        print("       等待中…（每 3 秒重查）")
        time.sleep(3)

    idx, target = pick_target(displays)
    print(f"\n  采集目标: 索引 {idx}  {target['id']}  "
          f"{target['width']}x{target['height']}@{target.get('refresh_hz', 0)}")

    # ---- 步骤 2：采集探测 ----
    print("\n=== 步骤 2/3：WGC 采集探测 ===")
    rc, out = run(bin_path, ["capture-probe", "--index", str(idx),
                             "--seconds", str(args.seconds)], timeout=args.seconds + 30)
    print("  " + "\n  ".join(l for l in out.splitlines() if l.strip()))

    # ---- 步骤 3：导出首帧并肉眼验证 ----
    print("\n=== 步骤 3/3：导出首帧 NV12 + 转 PNG ===")
    nv12 = os.path.join(args.out, "vscreen.nv12")
    png = os.path.join(args.out, "vscreen_preview.png")
    rc, out = run(bin_path, ["capture-dump", "--index", str(idx), "--out", nv12],
                  timeout=60)
    print("  " + "\n  ".join(l for l in out.splitlines() if l.strip()))
    if os.path.isfile(nv12):
        ok = nv12_to_png(nv12, png, target.get("width", 0), target.get("height", 0))
        if ok:
            print(f"\n[完成] 请打开 {png} 肉眼确认画面内容是否为真实桌面（而非全黑）。")
            return 0
    print("\n[FAIL] 首帧导出失败")
    return 5


if __name__ == "__main__":
    sys.exit(main())
