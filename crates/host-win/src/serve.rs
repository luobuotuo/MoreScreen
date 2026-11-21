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

/// 客户端唯一标识。用于把信令/会话精确路由到某一个接收端。
pub type ClientId = u64;

/// 客户端集合：负责把编码后的访问单元广播给所有接收端，
/// 并为每个客户端提供**定向**信令发送能力（多客户端独立 WebRTC 会话的基础）。
#[derive(Clone, Default)]
pub struct Hub {
    clients: Arc<Mutex<Vec<(ClientId, Client)>>>,
    next_id: Arc<AtomicU64>,
    /// 最近一个关键帧访问单元：新客户端接入时立即补发，避免它等到下一个 GOP。
    /// （仅旧的 WS+WebCodecs 路径使用；WebRTC 路径靠独立会话重建关键帧）
    last_keyframe: Arc<Mutex<Option<Vec<u8>>>>,
    /// 客户端发来的文本消息（信令回传：answer / ICE candidate），带来源客户端 ID。
    inbox: Arc<Mutex<Vec<(ClientId, String)>>>,
    /// 新接入的客户端 ID（主线程轮询后为其建立独立 WebRTC 会话）。
    joined: Arc<Mutex<Vec<ClientId>>>,
    /// 已断开但尚未清理的客户端 ID（主线程据此销毁对应会话）。
    disconnected: Arc<Mutex<Vec<ClientId>>>,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(Vec::new())),
            next_id: Arc::new(AtomicU64::new(1)),
            last_keyframe: Arc::new(Mutex::new(None)),
            inbox: Arc::new(Mutex::new(Vec::new())),
            joined: Arc::new(Mutex::new(Vec::new())),
            disconnected: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 登记新客户端，返回其 ID；并把它加入"待建会话"队列。
    ///
    /// 每个客户端都会得到一个**独立的 PeerConnection**（见 webrtc::WebRtcHub），
    /// 因此新增客户端不再需要"重建全局会话"，也就不会踢掉正在观看的其他客户端
    /// —— 这是旧 peer_gen 机制反复互相踢的根因。
    fn add(&self, client: Client) -> ClientId {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        {
            // 旧的 WS 二进制路径仍需要关键帧补发；WebRTC 路径不受影响
            let kf = self.last_keyframe.lock().unwrap().clone();
            if let Some(data) = kf {
                if let Ok(mut ws) = client.lock() {
                    let _ = ws.send(Message::Binary(data));
                }
            }
        }
        self.clients.lock().unwrap().push((id, client));
        self.joined.lock().unwrap().push(id);
        id
    }

    /// 取出所有新接入、尚未建立会话的客户端 ID。
    pub fn poll_joined(&self) -> Vec<ClientId> {
        std::mem::take(&mut *self.joined.lock().unwrap())
    }

    /// 取出所有已断开、尚未清理会话的客户端 ID（去重）。
    pub fn poll_disconnected(&self) -> Vec<ClientId> {
        let mut v = std::mem::take(&mut *self.disconnected.lock().unwrap());
        v.sort_unstable();
        v.dedup();
        v
    }

    /// 标记某客户端已断开（读循环结束、或广播发送失败时调用）。
    fn mark_disconnected(&self, id: ClientId) {
        self.disconnected.lock().unwrap().push(id);
    }

    /// 记录一条客户端发来的文本消息（信令），带来源 ID。
    fn push_inbox(&self, id: ClientId, text: String) {
        self.inbox.lock().unwrap().push((id, text));
    }

    /// 非阻塞取出所有待处理的信令消息及其来源客户端（主线程轮询）。
    pub fn poll_inbox(&self) -> Vec<(ClientId, String)> {
        std::mem::take(&mut *self.inbox.lock().unwrap())
    }

    /// 定向发送一段文本（JSON 信令）给指定客户端。返回是否成功。
    pub fn send_to(&self, id: ClientId, text: &str) -> bool {
        let clients = self.clients.lock().unwrap();
        let Some((_, c)) = clients.iter().find(|(cid, _)| *cid == id) else {
            return false;
        };
        let mut ws = match c.lock() {
            Ok(g) => g,
            Err(_) => return false,
        };
        ws.send(Message::Text(text.to_string())).is_ok()
    }

    /// 广播一段文本（JSON 信令），不写缓存。返回 (成功数, 断开数)。
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
        for (id, c) in clients.drain(..) {
            let sent = {
                let mut ws = c.lock().unwrap();
                match ws.send(msg.clone()) {
                    Ok(()) => true,
                    Err(e) => {
                        eprintln!("[ws] 发送失败，移除客户端 #{id}: {e}");
                        false
                    }
                }
            };
            if sent {
                ok += 1;
                alive.push((id, c));
            } else {
                dead += 1;
                self.mark_disconnected(id);
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
                    let id = hub.add(client.clone());
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
                            Message::Text(t) => hub.push_inbox(id, t),
                            Message::Binary(b) => {
                                hub.push_inbox(id, String::from_utf8_lossy(&b).to_string())
                            }
                            Message::Close(_) => break,
                            _ => {}
                        }
                    }
                    // 读循环结束 = 客户端断开：登记到 disconnected，
                    // 主线程据此销毁该客户端独有的 WebRTC 会话。
                    hub.mark_disconnected(id);
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
            let port_bound = bound;   // u16 Copy：给 /api/addr 用，告诉前端真实端口
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
                } else if rel == "api/addr" {
                    // 告诉前端宿主的局域网地址：用 localhost 打开配对页时，
                    // location.hostname 对平板毫无意义（那是平板自己的 localhost）。
                    let ip = local_ipv4().unwrap_or_else(|| "127.0.0.1".to_string());
                    (
                        "200 OK",
                        "application/json; charset=utf-8",
                        format!("{{\"ip\":\"{}\",\"port\":{}}}", ip, port_bound).into_bytes(),
                    )
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

                // 必须禁缓存：接收端页面是实时从磁盘读的，但浏览器对没有
                // Cache-Control / ETag 的 HTML 会做启发式缓存，导致「改了前端、
                // 刷新后仍跑旧代码」。局域网文件很小，每次重传成本可忽略。
                let header = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store, no-cache, must-revalidate\r\nPragma: no-cache\r\nExpires: 0\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n",
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
