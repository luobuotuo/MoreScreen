"""抓取桌面截图（用于验证真实浏览器里的解码画面）。"""
import sys, mss
out = sys.argv[1] if len(sys.argv) > 1 else "out/desktop.png"
with mss.mss() as sct:
    mon = sct.monitors[1] if len(sct.monitors) > 1 else sct.monitors[0]
    img = sct.grab(mon)
    mss.tools.to_png(img.rgb, img.size, output=out)
print("saved", out)
