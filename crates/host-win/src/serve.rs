//! 局域网传输层（POC）：WebSocket 广播 + 极简静态页托管。
//!
//! 用同步阻塞实现（不引入异步运行时），因为采集/编码链路本身就是阻塞的。
//! WebSocket 走标准的 `tungstenite` 握手与分帧，浏览器端可直接用原生 WebSocket 接收。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tungstenite::{accept, Message, WebSocket};

type Client = Arc<Mutex<WebSocket<TcpStream>>>;

/// 客户端集合：负责把编码后的访问单元广播给所有接收端。
#[derive(Clone, Default)]
pub struct Hub {
    clients: Arc<Mutex<Vec<Client>>>,
    /// 最近一个关键帧访问单元：新客户端接入时立即补发，避免它等到下一个 GOP。
    last_keyframe: Arc<Mutex<Option<Vec<u8>>>>,
    /// 客户端发来的文本消息（信令回传：answer / ICE candidate）。
    inbox: Arc<Mutex<Vec<String>>>,
    /// 客户端代数：每次有新客户端登记就 +1，主线程据此重建 WebRTC 会话。
    ///
    /// 这是**替代 offer 缓存**的机制。旧设计把 offer 存起来给新客户端补发，
    /// 但 PeerConnection 只能协商一次，旧 offer 交给新客户端只会卡在 ICE checking；
    /// 正确做法是新客户端接入 → 重建 pc → 重新生成 offer。
    peer_gen: Arc<AtomicU64>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(Vec::new())),
            last_keyframe: Arc::new(Mutex::new(None)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            peer_gen: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 登记新客户端：补发最近关键帧，并把客户端代数 +1（触发会话重建）。
    ///
    /// 不再补发旧的 offer：一个 PeerConnection 只能协商一次，
    /// 把旧 offer 交给新客户端只会让它卡在 ICE checking（实测复现）。
    /// 新 offer 由主线程检测到代数变化后重建会话时重新生成。
    fn add(&self, client: Client) {
        {
            let kf = self.last_keyframe.lock().unwrap().clone();
            if let Some(data) = kf {
                if let Ok(mut ws) = client.lock() {
                    let _ = ws.send(Message::Binary(data));
                }
            }
        }
        self.clients.lock().unwrap().push(client);
        self.peer_gen.fetch_add(1, Ordering::SeqCst);
    }

    /// 当前客户端代数。主线程每次轮询都比较，变化即重建会话。
    pub fn peer_generation(&self) -> u64 {
        self.peer_gen.load(Ordering::SeqCst)
    }

    /// 记录一条客户端发来的文本消息（信令）。
    fn push_inbox(&self, text: String) {
        self.inbox.lock().unwrap().push(text);
    }

    /// 非阻塞取出所有待处理的信令消息（主线程轮询）。
    pub fn poll_inbox(&self) -> Vec<String> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }

    /// 广播一段文本（JSON 信令），不写缓存。返回 (成功数, 断开数)。
    ///
    /// offer 与 candidate 都走这里：补发不靠缓存，靠"客户端代数 → 重建会话"
    /// （见 `peer_gen`）。这样不存在把过期 offer 塞给新客户端的问题。
    pub fn broadcast_text(&self, text: &str) -> (usize, usize) {
        self.broadcast_raw(Message::Text(text.to_string()))
    }

    /// 更新关键帧缓存。
    pub fn set_keyframe(&self, data: &[u8]) {
        *self.last_keyframe.lock().unwrap() = Some(data.to_vec());
    }

    pub fn client_count(&self) -> usize {
        self.clients.lock().unwrap().len()
    }

    /// 广播一个二进制访问单元，返回 (成功数, 断开的客户端数)。
    pub fn broadcast(&self, data: &[u8]) -> (usize, usize) {
        self.broadcast_raw(Message::Binary(data.to_vec()))
    }

    /// 通用广播：发送指定消息，清理断开的客户端。
    fn broadcast_raw(&self, msg: Message) -> (usize, usize) {
        let mut clients = self.clients.lock().unwrap();
        let mut alive = Vec::with_capacity(clients.len());
        let mut ok = 0usize;
        let mut dead = 0usize;
        for c in clients.drain(..) {
            let sent = {
                let mut ws = c.lock().unwrap();
                match ws.send(msg.clone()) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("[ws] 发送失败，移除客户端: {e}");
                        false
                    }
                }
            };
            if sent {
                ok += 1;
                alive.push(c);
            } else {
                dead += 1;
            }
        }
        *clients = alive;
        (ok, dead)
    }
}

/// 启动 WebSocket 服务，返回 (Hub, 实际端口)。
pub fn start_ws(port: u16) -> std::io::Result<(Hub, u16)> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    let bound = listener.local_addr()?.port();
    let hub = Hub::new();
    let hub2 = hub.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let hub = hub2.clone();
            std::thread::spawn(move || {
                if let Ok(ws) = accept(stream) {
                    // 读必须带超时。此前 read() 在**持锁**状态下无限阻塞，
                    // 而广播要拿同一把锁 → 广播线程被永久卡死。
                    // 症状：采集回调停住、帧数不再增长、offer 发不出去、客户端一直等 offer。
                    let _ = ws
                        .get_ref()
                        .set_read_timeout(Some(Duration::from_millis(20)));
                    let client: Client = Arc::new(Mutex::new(ws));
                    hub.add(client.clone());
                    // 持续读取客户端消息（信令回传：answer / ICE candidate）
                    loop {
                        let msg = {
                            let mut guard = match client.lock() {
                                Ok(g) => g,
                                Err(_) => break,
                            };
                            match guard.read() {
                                Ok(m) => m,
                                Err(tungstenite::Error::Io(ref e))
                                    if e.kind() == std::io::ErrorKind::WouldBlock
                                        || e.kind() == std::io::ErrorKind::TimedOut =>
                                {
                                    // 没有新消息：释放锁让广播有机会拿锁发送
                                    continue;
                                }
                                Err(_) => break, // 连接断开
                            }
                        };
                        match msg {
                            Message::Text(t) => hub.push_inbox(t),
                            Message::Binary(b) => {
                                hub.push_inbox(String::from_utf8_lossy(&b).to_string())
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                }
            });
        }
    });
    Ok((hub, bound))
}

/// 启动极简静态文件服务（托管接收端目录）。
///
/// - 根路径返回 `index.html`，其余按 URL 路径映射到 `root_dir` 下的文件；
/// - **安全**：拒绝含 `..`、`\`、`:` 或以 `/` 开头的请求，防止目录穿越；
/// - `Content-Type` 按扩展名推断（PWA 需要 js/css/webmanifest 等多种类型）。
pub fn start_http(port: u16, root_dir: &str) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    let bound = listener.local_addr()?.port();
    let root = std::path::PathBuf::from(root_dir);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let root = root.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");

                if !first_line.starts_with("GET") {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    let _ = stream.flush();
                    return;
                }

                // 解析请求路径（去掉查询串）
                let raw = first_line.split_whitespace().nth(1).unwrap_or("/");
                let rel = raw.split('?').next().unwrap_or("/").trim_start_matches('/');

                // 目录穿越防护
                let unsafe_req = rel.contains("..") || rel.contains('\\') || rel.contains(':');
                let rel = if rel.is_empty() { "index.html" } else { rel };

                let (status, ctype, body) = if unsafe_req {
                    ("403 Forbidden", "text/plain; charset=utf-8", b"forbidden".to_vec())
                } else {
                    let target = root.join(rel);
                    match std::fs::read(&target) {
                        Ok(b) if target.is_file() => ("200 OK", content_type(rel), b),
                        _ => (
                            "404 Not Found",
                            "text/plain; charset=utf-8",
                            format!("not found: {}", rel).into_bytes(),
                        ),
                    }
                };

                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            });
        }
    });
    Ok(bound)
}

/// 按扩展名推断 Content-Type。
fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "webmanifest" => "application/manifest+json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// 取本机第一个非回环 IPv4 地址（用于打印访问地址）。
pub fn local_ipv4() -> Option<String> {
    // 不引入额外依赖：用一次 UDP "连接"来问系统默认路由，不产生任何流量
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let addr = sock.local_addr().ok()?;
    let ip = addr.ip().to_string();
    if ip.starts_with("127.") {
        None
    } else {
        Some(ip)
    }
}
