//! 局域网传输层（POC）：WebSocket 广播 + 极简静态页托管。
//!
//! 用同步阻塞实现（不引入异步运行时），因为采集/编码链路本身就是阻塞的。
//! WebSocket 走标准的 `tungstenite` 握手与分帧，浏览器端可直接用原生 WebSocket 接收。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
}

impl Hub {
    pub fn new() -> Self {
        Self {
            clients: Arc::new(Mutex::new(Vec::new())),
            last_keyframe: Arc::new(Mutex::new(None)),
        }
    }

    /// 登记新客户端，并立即补发最近的关键帧。
    fn add(&self, client: Client) {
        let kf = self.last_keyframe.lock().unwrap().clone();
        if let Some(data) = kf {
            if let Ok(mut ws) = client.lock() {
                let _ = ws.send(Message::Binary(data));
            }
        }
        self.clients.lock().unwrap().push(client);
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
        let mut clients = self.clients.lock().unwrap();
        let mut alive = Vec::with_capacity(clients.len());
        let mut ok = 0usize;
        let mut dead = 0usize;
        for c in clients.drain(..) {
            let sent = {
                let mut ws = c.lock().unwrap();
                match ws.send(Message::Binary(data.to_vec())) {
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
                    let client: Client = Arc::new(Mutex::new(ws));
                    hub.add(client);
                }
            });
        }
    });
    Ok((hub, bound))
}

/// 启动极简静态文件服务（用于托管接收端页面）。
pub fn start_http(port: u16, file_path: &str) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("0.0.0.0", port))?;
    let bound = listener.local_addr()?.port();
    let path = file_path.to_string();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            let path = path.clone();
            std::thread::spawn(move || {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                let body = std::fs::read(&path).unwrap_or_default();
                if first_line.starts_with("GET") {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(header.as_bytes());
                    let _ = stream.write_all(&body);
                } else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                }
                let _ = stream.flush();
            });
        }
    });
    Ok(bound)
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
