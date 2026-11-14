//! `rs-host`：Windows 宿主端命令行工具。
//!
//! 子命令：
//!   displays            枚举显示目标（适配器 + 显示器），标注哪些是虚拟屏
//!   displays --json     同上，输出 JSON
//!   selftest            运行不依赖硬件的自检（状态机 / 指标）

use remote_screen_core::json_escape;
use remote_screen_host_win::displays::enumerate_displays;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("help");
    let json = args.iter().any(|a| a == "--json");

    match cmd {
        "displays" => {
            let displays = enumerate_displays();
            if json {
                let items: Vec<String> = displays
                    .iter()
                    .map(|d| {
                        format!(
                            "{{\"id\":\"{}\",\"device_path\":\"{}\",\"adapter\":\"{}\",\"virtual\":{},\
\"active\":{},\"primary\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"refresh_hz\":{},\"bpp\":{}}}",
                            json_escape(&d.id.0),
                            json_escape(&d.device_path),
                            json_escape(&d.adapter_device_id),
                            d.is_virtual,
                            d.is_active,
                            d.is_primary,
                            d.bounds.x,
                            d.bounds.y,
                            d.mode.width,
                            d.mode.height,
                            d.mode.refresh_hz,
                            d.mode.bits_per_pixel,
                        )
                    })
                    .collect();
                println!("[{}]", items.join(","));
            } else {
                println!(
                    "{:<3} {:<9} {:<9} {:<10} {:<12} {:>5}x{:<5} @{:<4} {:<6} {}",
                    "#", "类型", "活动", "主屏", "适配器", "宽", "高", "Hz", "设备", "实例 ID"
                );
                for (i, d) in displays.iter().enumerate() {
                    let adapter_kind = if d.is_virtual { "虚拟" } else { "物理" };
                    println!(
                        "{:<3} {:<9} {:<9} {:<10} {:<12} {:>5}x{:<5} @{:<4} {:<6} {}",
                        i,
                        adapter_kind,
                        if d.is_active { "是" } else { "否" },
                        if d.is_primary { "是" } else { "否" },
                        &d.adapter_device_id.chars().take(10).collect::<String>(),
                        d.mode.width,
                        d.mode.height,
                        d.mode.refresh_hz,
                        d.device_path,
                        d.id.short(),
                    );
                }
                let virtual_count = displays.iter().filter(|d| d.is_virtual).count();
                let active_virtual = displays
                    .iter()
                    .find(|d| d.is_virtual && d.is_active)
                    .map(|d| d.id.short().to_string());
                println!();
                println!("总计 {} 个显示目标，其中虚拟 {} 个", displays.len(), virtual_count);
                match active_virtual {
                    Some(id) => println!("可用的虚拟屏（已点亮）: {id}"),
                    None => println!("当前没有已点亮的虚拟屏（虚拟适配器存在但未连接/未点亮）"),
                }
            }
        }
        "selftest" => {
            let mut machine = remote_screen_core::SessionMachine::new();
            let steps = [
                remote_screen_core::SessionEvent::DisplayReady,
                remote_screen_core::SessionEvent::CaptureStarted,
                remote_screen_core::SessionEvent::EncoderReady,
                remote_screen_core::SessionEvent::TransportStarted,
                remote_screen_core::SessionEvent::PeerConnected,
            ];
            for s in steps {
                machine.apply(s).expect("状态迁移应当合法");
            }
            assert_eq!(machine.state(), remote_screen_core::SessionState::Connected);
            println!("selftest: OK（状态机走到 connected）");
        }
        "modes" => {
            // 只读探测：列出某显示设备支持的模式
            let displays = enumerate_displays();
            let index = arg_value(&args, "--index")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let target = match displays.get(index) {
                Some(d) => d,
                None => {
                    eprintln!("显示目标 #{index} 不存在（共 {} 个）", displays.len());
                    std::process::exit(2);
                }
            };
            println!("目标: {}  设备 {}", target.label(), target.device_path);
            let modes = unsafe { remote_screen_host_win::displays::list_modes(&target.device_path) };
            if modes.is_empty() {
                println!("  该设备未暴露任何模式（虚拟适配器通常需先由其客户端建立会话）");
            } else {
                println!("  可用模式 {} 种，前 12 个：", modes.len());
                for m in modes.iter().take(12) {
                    println!("    {}x{}@{}Hz {}bpp", m.width, m.height, m.refresh_hz, m.bits_per_pixel);
                }
            }
        }
        "capture-dump" => {
            // 采集 + 导出首帧 NV12（Stage 3 编码器输入）
            let displays = enumerate_displays();
            let index = arg_value(&args, "--index")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let out = arg_value(&args, "--out").unwrap_or_else(|| "out/frame.nv12".to_string());
            let target = match displays.get(index) {
                Some(d) => d,
                None => {
                    eprintln!("显示目标 #{index} 不存在（共 {} 个）", displays.len());
                    std::process::exit(2);
                }
            };
            println!("目标: {}", target.label());
            match remote_screen_host_win::capture::probe_capture(
                target.bounds.x,
                target.bounds.y,
                target.mode.width,
                target.mode.height,
                1500,
                Some(std::path::Path::new(&out)),
            ) {
                Ok(s) => {
                    println!("帧数 = {}", s.frames);
                    println!("内容尺寸 = {}x{}", s.content_width, s.content_height);
                    if let Some(c) = &s.content {
                        println!(
                            "内容自检 = 平均亮度 {:.1} / 峰值 {} / 非黑占比 {:.1}%（{}）",
                            c.luma_avg,
                            c.luma_max,
                            c.non_black_ratio * 100.0,
                            if c.is_black { "判定为黑画面" } else { "画面有效" }
                        );
                    }
                    match &s.nv12_path {
                        Some(p) => println!("NV12 已导出: {}", p),
                        None => println!("未导出 NV12"),
                    }
                }
                Err(e) => {
                    eprintln!("采集失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        "stream" => {
            // 采集 -> H.264 编码 -> WebSocket 广播 -> 浏览器解码
            let ws_port = arg_value(&args, "--port")
                .and_then(|v| v.parse::<u16>().ok())
                .unwrap_or(8787);
            let http_port = ws_port + 1;
            let index = arg_value(&args, "--index")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let seconds = arg_value(&args, "--seconds")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            let ffmpeg = match resolve_ffmpeg() {
                Some(p) => p,
                None => {
                    eprintln!("未找到 ffmpeg：请设置环境变量 RS_FFMPEG 指向 ffmpeg.exe，或放到 tools/ffmpeg/bin/");
                    std::process::exit(4);
                }
            };
            let (hub, ws_bound) = match remote_screen_host_win::serve::start_ws(ws_port) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("WebSocket 服务启动失败: {e}");
                    std::process::exit(5);
                }
            };
            let page = "web/receiver/index.html";
            let http_bound = remote_screen_host_win::serve::start_http(http_port, page)
                .map(|p| p)
                .unwrap_or(http_port);
            let ip = remote_screen_host_win::serve::local_ipv4()
                .unwrap_or_else(|| "127.0.0.1".to_string());
            println!("ffmpeg      : {}", ffmpeg);
            println!("WS 服务     : ws://{ip}:{ws_bound}");
            println!("接收端页面  : http://{ip}:{http_bound}/");
            println!("等待接收端连接…");

            let displays = enumerate_displays();
            let target = match displays.get(index) {
                Some(d) => d,
                None => {
                    eprintln!("显示目标 #{index} 不存在（共 {} 个）", displays.len());
                    std::process::exit(2);
                }
            };
            println!("采集目标    : {}", target.label());

            let cfg = remote_screen_host_win::encode::EncodeConfig {
                width: target.mode.width.max(16),
                height: target.mode.height.max(16),
                fps: 30,
                bitrate_kbps: 6000,
                gop: 30,
            };
            let encoder = remote_screen_host_win::encode::detect_encoder(&ffmpeg);
            println!("编码器      : {}", encoder);
            let mut enc = match remote_screen_host_win::encode::FfmpegEncoder::start(&cfg, &ffmpeg, &encoder) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("编码器启动失败: {e}");
                    std::process::exit(6);
                }
            };

            // 文件模式：循环推送同一帧（用于无虚拟屏时验证编码/传输/解码链路）
            if let Some(path) = arg_value(&args, "--nv12") {
                let data = match std::fs::read(&path) {
                    Ok(d) => d,
                    Err(e) => {
                        eprintln!("读取 NV12 文件失败: {e}");
                        std::process::exit(7);
                    }
                };
                println!("文件模式    : {} ({} 字节)", path, data.len());
                let frame_time = std::time::Duration::from_millis(1000 / cfg.fps as u64);
                let mut next = std::time::Instant::now();
                loop {
                    if let Err(e) = enc.push_nv12(&data) {
                        eprintln!("推帧失败: {e}");
                        break;
                    }
                    match enc.read_chunks() {
                        Ok(chunks) => {
                            for c in chunks {
                                let mut framed = Vec::with_capacity(c.data.len() + 1);
                                framed.push(if c.key { 1u8 } else { 0u8 });
                                framed.extend_from_slice(&c.data);
                                if c.key {
                                    hub.set_keyframe(&framed);
                                }
                                let (_ok, dead) = hub.broadcast(&framed);
                                if dead > 0 {
                                    println!("清理断开客户端: {}", dead);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("读码流失败: {e}");
                            break;
                        }
                    }
                    next += frame_time;
                    let now = std::time::Instant::now();
                    if next > now {
                        std::thread::sleep(next - now);
                    } else {
                        next = now;
                    }
                }
            } else {
                // 实时模式：采集 -> 编码 -> 广播
                let hub2 = hub.clone();
                let mut enc2 = enc;
                // 分段指标：采集就绪 → 提交编码 → 编码出流 → 发送完成
                let metric_path = arg_value(&args, "--metrics")
                    .unwrap_or_else(|| "out/metrics.jsonl".to_string());
                let mut recorder =
                    remote_screen_core::MetricsRecorder::create(std::path::Path::new(&metric_path), 1_000_000_000.0, 1)
                        .ok();
                let t_origin = std::time::Instant::now();
                let mut frame_id: u64 = 0;
                let mut last_sent: u64 = 0;
                let mut push_ts: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
                let res = remote_screen_host_win::capture::capture_stream(
                    target.bounds.x,
                    target.bounds.y,
                    target.mode.width,
                    target.mode.height,
                    seconds * 1000,
                    cfg.fps,
                    |nv12| {
                        let t_capture = t_origin.elapsed().as_nanos() as u64;
                        enc2.push_nv12(nv12)?;
                        // 送帧时间戳入队：出流时按序弹出，保证指标对应到同一帧
                        push_ts.push_back(t_capture);
                        let chunks = enc2.read_chunks()?;
                        for c in chunks {
                            let t_enc_out = t_origin.elapsed().as_nanos() as u64;
                            let t_frame = push_ts.pop_front().unwrap_or(t_capture);
                            let mut framed = Vec::with_capacity(c.data.len() + 1);
                            framed.push(if c.key { 1u8 } else { 0u8 });
                            framed.extend_from_slice(&c.data);
                            if c.key {
                                hub2.set_keyframe(&framed);
                            }
                            hub2.broadcast(&framed);
                            let t_send = t_origin.elapsed().as_nanos() as u64;
                            frame_id += 1;
                            if let Some(rec) = recorder.as_mut() {
                                let _ = rec.record(&remote_screen_core::FrameRecord {
                                    session_id: 1,
                                    display_id: target.id.0.clone(),
                                    frame_id,
                                    capture_src_ts: t_frame,
                                    capture_deq_ts: t_frame,
                                    enc_submit_ts: t_frame,
                                    enc_out_ts: t_enc_out,
                                    send_ts: t_send,
                                    bytes: framed.len() as u32,
                                    keyframe: c.key,
                                    drop_reason: None,
                                });
                            }
                            last_sent = t_send;
                        }
                        if let Some(rec) = recorder.as_mut() {
                            let _ = rec.flush();
                        }
                        let _ = last_sent;
                        Ok(())
                    },
                );
                if let Some(mut rec) = recorder.take() {
                    let _ = rec.flush();
                    println!("分段指标已写入: {}", metric_path);
                }
                match res {
                    Ok(stats) => {
                        println!(
                            "采集结束: {} 帧 / {:.1} fps / 首帧 {:.1} ms",
                            stats.frames,
                            if stats.avg_interval_ms > 0.0 { 1000.0 / stats.avg_interval_ms } else { 0.0 },
                            stats.first_frame_delay_ms
                        );
                    }
                    Err(e) => eprintln!("采集失败: {e}"),
                }
            }
        }
        "vdd-install" => {
            // 创建 root-enumerated 设备节点并绑定 VDD 驱动（需管理员）
            let log_path = arg_value(&args, "--log");
            let inf = arg_value(&args, "--inf")
                .unwrap_or_else(|| "tools/vdd-driver/VirtualDisplayDriver/MttVDD.inf".to_string());
            let hw = arg_value(&args, "--hw").unwrap_or_else(|| "Root\\MttVDD".to_string());
            println!("管理员权限   : {}", remote_screen_host_win::vdd::is_admin());
            println!("INF          : {}", inf);
            println!("硬件 ID      : {}", hw);
            let mut report = String::new();
            let push = |r: &mut String, line: &str| {
                r.push_str(line);
                r.push('\n');
            };
            push(&mut report, &format!("管理员权限   : {}", remote_screen_host_win::vdd::is_admin()));
            push(&mut report, &format!("INF          : {}", inf));
            push(&mut report, &format!("硬件 ID      : {}", hw));

            let outcome = remote_screen_host_win::vdd::install_vdd_device(
                std::path::Path::new(&inf),
                &hw,
            );
            let exit_code = match outcome {
                Ok(r) => {
                    push(&mut report, &format!("节点原已存在 : {}", r.node_existed));
                    push(&mut report, &format!("本次创建节点 : {}", r.node_created));
                    push(&mut report, &format!("驱动绑定成功 : {}", r.driver_bound));
                    push(&mut report, &format!("需要重启     : {}", r.reboot_required));
                    push(
                        &mut report,
                        &format!(
                            "实例 ID      : {}",
                            r.instance_id.clone().unwrap_or_else(|| "(未知)".to_string())
                        ),
                    );
                    for m in &r.messages {
                        push(&mut report, &format!("  · {}", m));
                    }
                    match remote_screen_host_win::vdd::umdf_driver_installed() {
                        Some((path, len)) => {
                            push(&mut report, &format!("UMDF 驱动    : {} ({} 字节)", path, len))
                        }
                        None => push(
                            &mut report,
                            "UMDF 驱动    : 未落地到 System32\\drivers\\UMDF（可能需要重启）",
                        ),
                    }
                    push(&mut report, "下一步：运行 `rs-host displays`，应出现新的 Virtual Display Driver");
                    0
                }
                Err(e) => {
                    push(&mut report, &format!("VDD 安装失败: {}", e));
                    1
                }
            };

            print!("{}", report);
            if let Some(path) = log_path {
                if let Some(parent) = std::path::Path::new(&path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&path, report.as_bytes());
                println!("报告已写入: {}", path);
            }
            std::process::exit(exit_code);
        }
        "capture-probe" => {
            // 采集探测：抓取指定显示器若干秒，输出帧率与内容尺寸
            let displays = enumerate_displays();
            let index = arg_value(&args, "--index")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or_else(|| {
                    displays
                        .iter()
                        .position(|d| d.is_active)
                        .unwrap_or(0)
                });
            let seconds = arg_value(&args, "--seconds")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5);
            let target = match displays.get(index) {
                Some(d) => d,
                None => {
                    eprintln!("显示目标 #{index} 不存在（共 {} 个）", displays.len());
                    std::process::exit(2);
                }
            };
            println!("目标: {}", target.label());
            println!(
                "  位置=({},{}) 模式={}x{}@{}Hz 虚拟={}",
                target.bounds.x,
                target.bounds.y,
                target.mode.width,
                target.mode.height,
                target.mode.refresh_hz,
                target.is_virtual
            );
            match remote_screen_host_win::capture::probe_capture(
                target.bounds.x,
                target.bounds.y,
                target.mode.width,
                target.mode.height,
                seconds * 1000,
                None,
            ) {
                Ok(s) => {
                    let fps = if s.avg_interval_ms > 0.0 {
                        1000.0 / s.avg_interval_ms
                    } else {
                        0.0
                    };
                    println!("采集结果:");
                    println!("  帧数          = {}", s.frames);
                    println!("  首帧延迟      = {:.1} ms", s.first_frame_delay_ms);
                    println!(
                        "  平均帧间隔    = {:.2} ms (≈ {:.1} fps)",
                        s.avg_interval_ms, fps
                    );
                    println!("  最小/最大间隔 = {:.2} / {:.2} ms", s.min_interval_ms, s.max_interval_ms);
                    println!("  内容尺寸      = {}x{}", s.content_width, s.content_height);
                    if let Some(c) = &s.content {
                        println!(
                            "  内容自检      = 平均亮度 {:.1} / 峰值 {} / 非黑占比 {:.1}%（{}）",
                            c.luma_avg,
                            c.luma_max,
                            c.non_black_ratio * 100.0,
                            if c.is_black { "判定为黑画面" } else { "画面有效" }
                        );
                    }
                    if s.frames == 0 {
                        eprintln!("警告: 未捕获到任何帧");
                        std::process::exit(3);
                    }
                }
                Err(e) => {
                    eprintln!("采集失败: {e}");
                    std::process::exit(1);
                }
            }
        }
        _ => {
            println!("用法: rs-host <displays [--json] | modes --index N | selftest | capture-probe | capture-dump | stream [--index N] [--port P] [--seconds S] [--nv12 file] | vdd-install [--inf PATH] [--hw Root\\MttVDD]>");
        }
    }
}

/// 读取 `--key value` 形式的参数值。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == key)?;
    args.get(pos + 1).cloned()
}

/// 定位 ffmpeg 可执行文件：环境变量 → tools/ffmpeg/bin → PATH → WinGet 链接目录。
fn resolve_ffmpeg() -> Option<String> {
    if let Ok(p) = std::env::var("RS_FFMPEG") {
        let p = p.trim().to_string();
        if !p.is_empty() && std::path::Path::new(&p).exists() {
            return Some(p);
        }
    }
    let candidates = [
        "tools/ffmpeg/bin/ffmpeg.exe",
        "tools/ffmpeg/ffmpeg.exe",
        "C:/Users/sz-ws016/AppData/Local/Microsoft/WinGet/Links/ffmpeg.exe",
    ];
    for c in candidates {
        if std::path::Path::new(c).exists() {
            return Some(c.to_string());
        }
    }
    if which("ffmpeg") {
        Some("ffmpeg".to_string())
    } else {
        None
    }
}

/// 在 PATH 中查找可执行文件（Windows 下用 PATHEXT 逐个扩展名尝试）。
fn which(name: &str) -> bool {
    let path = match std::env::var("PATH") {
        Ok(p) => p,
        Err(_) => return false,
    };
    let exts = if cfg!(windows) {
        vec![".exe", ".cmd", ".bat"]
    } else {
        vec![""]
    };
    for dir in path.split(';') {
        if dir.is_empty() {
            continue;
        }
        for ext in &exts {
            let full = format!("{}/{}{}", dir.trim_end_matches('\\'), name, ext);
            if std::path::Path::new(&full).exists() {
                return true;
            }
        }
    }
    false
}
