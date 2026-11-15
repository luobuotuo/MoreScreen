"""WebRTC 信令探针：连接 rs-host 的 WS，收信令消息并报告类型。

用于在无浏览器环境验证 stream-webrtc 的信令通道（offer/answer/candidate）。
用法: python scripts/signal_probe.py --port 8787 --wait 12
"""
import argparse, base64, os, socket, struct, sys, time


def handshake(sock, host, port, path="/"):
    key = base64.b64encode(os.urandom(16)).decode()
    req = (f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
           "Upgrade: websocket\r\nConnection: Upgrade\r\n"
           f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n")
    sock.sendall(req.encode())
    resp = bytearray()
    while b"\r\n\r\n" not in bytes(resp):
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("握手失败")
        resp += chunk
    head = bytes(resp).split(b"\r\n\r\n", 1)[0].decode(errors="replace")
    if "101" not in head.splitlines()[0]:
        raise RuntimeError(f"握手失败: {head.splitlines()[0]}")
    return bytearray(bytes(resp).split(b"\r\n\r\n", 1)[1])


def read_frame(sock, pending):
    def need(n):
        nonlocal pending
        while len(pending) < n:
            chunk = sock.recv(65536)
            if not chunk:
                raise ConnectionError("连接被关闭")
            pending += chunk
        out = bytes(pending[:n])
        del pending[:n]
        return out

    b1, b2 = need(2)
    opcode = b1 & 0x0F
    masked = b2 & 0x80
    length = b2 & 0x7F
    if length == 126:
        length = struct.unpack(">H", need(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", need(8))[0]
    mask = need(4) if masked else None
    payload = need(length)
    if mask:
        payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    return opcode, payload, pending


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8787)
    ap.add_argument("--wait", type=float, default=12)
    args = ap.parse_args()

    sock = socket.create_connection((args.host, args.port), timeout=args.wait + 5)
    pending = handshake(sock, args.host, args.port)
    print(f"已连接 ws://{args.host}:{args.port}/，等待 {args.wait}s …")

    texts, binaries = [], 0
    deadline = time.time() + args.wait
    while time.time() < deadline:
        sock.settimeout(max(0.5, deadline - time.time()))
        try:
            opcode, payload, pending = read_frame(sock, pending)
        except (socket.timeout, TimeoutError):
            continue
        except ConnectionError:
            print("服务端关闭连接")
            break
        if opcode == 0x1:      # text
            texts.append(payload.decode("utf-8", "replace"))
            print(f"  [文本] {len(payload)} 字节: {payload[:110].decode('utf-8','replace')}…")
        elif opcode == 0x2:    # binary
            binaries += 1
            if binaries <= 2:
                print(f"  [二进制] {len(payload)} 字节 (RTP 数据不应出现在 WS 通道)")
        elif opcode == 0x8:
            print("收到 Close 帧")
            break

    print(f"\n汇总: 文本消息 {len(texts)} 条, 二进制 {binaries} 条")
    offers = [t for t in texts if '"webrtc-offer"' in t]
    if offers:
        print(f"✅ 收到 offer {len(offers)} 条（信令通道正常）")
        print(f"   offer 前 90 字符: {offers[0][:90]}")
        return 0
    print("⚠️ 未收到 offer")
    return 1


if __name__ == "__main__":
    sys.exit(main())
