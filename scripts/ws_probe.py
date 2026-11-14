"""极简 WebSocket 客户端探针：连上 rs-host 的 WS 服务，收 N 个二进制消息并统计。

不依赖第三方库，用于在没有浏览器的环境里验证"编码 -> 传输"这一段。
用法: python scripts/ws_probe.py --port 8787 --messages 10
"""
import argparse, base64, os, socket, struct, sys, time


def handshake(sock, host, port, path="/"):
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    sock.sendall(req.encode())
    resp = bytearray()
    while b"\r\n\r\n" not in bytes(resp):
        chunk = sock.recv(4096)
        if not chunk:
            raise ConnectionError("握手失败：连接被关闭")
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
    ap.add_argument("--messages", type=int, default=10)
    ap.add_argument("--timeout", type=float, default=15)
    ap.add_argument("--save", default="", help="把收到的首个二进制消息保存到该文件")
    args = ap.parse_args()

    sock = socket.create_connection((args.host, args.port), timeout=args.timeout)
    pending = handshake(sock, args.host, args.port)
    total, n = 0, 0
    t0 = time.time()
    while n < args.messages and time.time() - t0 < args.timeout:
        opcode, payload, pending = read_frame(sock, pending)
        if opcode == 0x2:  # binary
            n += 1
            total += len(payload)
            if n == 1 and args.save:
                with open(args.save, "wb") as f:
                    f.write(payload)
                print(f"  已保存首个访问单元 -> {args.save}")
            print(f"  #{n}: {len(payload)} 字节  起始码={payload[:4].hex()}")
        elif opcode == 0x8:
            print("服务端关闭连接")
            break
    dt = time.time() - t0
    print(f"收到 {n} 个消息，合计 {total} 字节，{dt:.2f}s，"
          f"平均 {total/max(dt,0.001)/1024:.1f} KB/s")
    sock.close()
    return 0 if n > 0 else 1


if __name__ == "__main__":
    sys.exit(main())
