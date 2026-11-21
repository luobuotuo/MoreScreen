//! `rs-host`：Windows 宿主端命令行工具。
//!
//! 子命令：
//!   displays            枚举显示目标（适配器 + 显示器），标注哪些是虚拟屏
//!   displays --json     同上，输出 JSON
//!   selftest            运行不依赖硬件的自检（状态机 / 指标）

use remote_screen_core::json_escape;
use remote_screen_host_win::displays::enumerate_displays;

fn main() {
    // 必须在任何 GDI/SendInput 调用前声明 DPI 感知，否则高 DPI 主机上
    // GetSystemMetrics 与 EnumDisplaySettings 坐标口径不一致，输入落点会系统性偏移。
    remote_screen_host_win::input::init_dpi_awareness();

    // webrtc-rs / DTLS 层日志。默认只显示 warn，避免刷屏；
    // 排查 DTLS 握手失败等问题时用环境变量打开，例如：
    //   RUST_LOG=webrtc_dtls=debug,webrtc=debug rs-host stream-webrtc ...
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn"),
    )
    .try_init();

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
                println!("  可用模式 {} 种：", modes.len());
                for m in modes.iter() {
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
        "capture-diag" => {
            let displays = enumerate_displays();
            let want = arg_value(&args, "--display");
            let index = arg_value(&args, "--index")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            let (_, target) = match pick_display(&displays, index, want.as_deref()) {
                Some(v) => v,
                None => {
                    eprintln!("未找到显示目标");
                    std::process::exit(2);
                }
            };
            let seconds = arg_value(&args, "--seconds").and_then(|v| v.parse::<u64>().ok()).unwrap_or(30);
            let fps = arg_value(&args, "--fps").and_then(|v| v.parse::<u32>().ok()).unwrap_or(20);
            let out = arg_value(&args, "--out").unwrap_or_else(|| "out/capture_diag.tsv".to_string());
            println!("目标: {}", target.label());
            match remote_screen_host_win::capture::diagnose_capture(
                target.bounds.x, target.bounds.y, target.mode.width, target.mode.height,
                seconds * 1000, fps, std::path::Path::new(&out),
            ) {
                Ok(msg) => println!("{msg}"),
                Err(e) => { eprintln!("采集诊断失败: {e}"); std::process::exit(1); }
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
            let page = "web/receiver";
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
                fps: 24,
                bitrate_kbps: 6000,
                gop: 30,
                // 文件模式读 NV12 文件（capture-dump 产物）；实时模式输出 BGRA
                pix_fmt: if arg_value(&args, "--nv12").is_some() { "nv12".into() } else { "bgra".into() },
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
                    if let Err(e) = enc.push_frame(&data) {
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
                let mut dropped_frames: u64 = 0;
                let res = remote_screen_host_win::capture::capture_stream(
                    target.bounds.x,
                    target.bounds.y,
                    target.mode.width,
                    target.mode.height,
                    seconds * 1000,
                    cfg.fps,
                    true, // 旧 WS 路径固定包含光标
                    |bgra| {
                        let t_capture = t_origin.elapsed().as_nanos() as u64;
                        let queued = enc2.push_frame(bgra)?;
                        if !queued {
                            dropped_frames += 1;
                        }
                        // 送帧时间戳入队：出流时按序弹出，保证指标对应到同一帧
                        push_ts.push_back(t_capture);
                        let chunks = enc2.read_chunks()?;
                        for c in chunks {
                            let t_enc_out = t_origin.elapsed().as_nanos() as u64;
                            // ffmpeg 内部可能丢帧/重排，AU 无法与送帧序列一一对应。
                            // 用最近一次送帧时刻作基准：编码器跟得上时该值≈单帧编码耗时，
                            // 落后时该值即"输出滞后于输入"的积压深度。
                            let t_frame = *push_ts.back().unwrap_or(&t_capture);
                            push_ts.clear();
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
                if dropped_frames > 0 {
                    println!("背压丢帧: {} 帧（编码器消费不过来，主动丢弃以控制延迟）", dropped_frames);
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
        "stream-webrtc" => {
            // WebRTC 路线（P1.2）：WGC 采集 -> QSV 编码 -> webrtc-rs -> 浏览器 <video>
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
            // 帧率必须能整除显示器刷新率（虚拟屏是 60Hz）。
            // 24fps 需要 41.6ms 间隔，而 60Hz 只给 16.67/33.3/50ms 三个落点 →
            // 实测永远只出 20fps（60/3），且 RTP 时间戳仍按 24fps 递增，
            // 时间基比实际快 20%，接收端 jitterBuffer 因此堆到 90ms+。
            // 20fps = 60/3，时间戳与实际完全对齐。
            let fps: u32 = arg_value(&args, "--fps")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(20);
            // 码率必须显式给：QSV 在 6000kbps 目标下遇到 I 帧密集会冲到 2.6Mbps，
            // 直接把平板 Wi-Fi（实测有效 ~1.6Mbps）塞满 → 1% 丢包 + 掉帧。
            // 800x600 静态桌面 800kbps 够用。
            let bitrate: u32 = arg_value(&args, "--bitrate")
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(800);

            let ffmpeg = match resolve_ffmpeg() {
                Some(p) => p,
                None => {
                    eprintln!("未找到 ffmpeg：请设置 RS_FFMPEG 或放到 tools/ffmpeg/bin/");
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
            let http_bound = remote_screen_host_win::serve::start_http(http_port, "web/receiver")
                .map(|p| p)
                .unwrap_or(http_port);
            let ip = remote_screen_host_win::serve::local_ipv4()
                .unwrap_or_else(|| "127.0.0.1".to_string());
            println!("ffmpeg      : {}", ffmpeg);
            println!("WS 信令     : ws://{ip}:{ws_bound}");
            println!("接收端页面  : http://{ip}:{http_bound}/webrtc.html");
            // 配对页：展示二维码，平板扫一下即可，免手输 IP（P2.4）
            println!("扫码配对页  : http://{ip}:{http_bound}/pair.html   ← 在平板上打开此页扫二维码");
            println!("等待接收端连接…（页面需点「开始接收」）");

            let displays = enumerate_displays();
            // 打印全部目标：不列出来只能靠猜
            for (i, d) in displays.iter().enumerate() {
                println!("  [{i}] {}", d.label());
            }
            let want = arg_value(&args, "--display");
            let (index, target_ref) = match pick_display(&displays, index, want.as_deref()) {
                Some(v) => v,
                None => {
                    eprintln!(
                        "未找到匹配的显示目标（--display {:?}，共 {} 个）",
                        want,
                        displays.len()
                    );
                    std::process::exit(2);
                }
            };
            // 取 owned 副本：P2.3 的 --mode 切换后需要用它承载"重新枚举后的新尺寸"
            let mut target = target_ref.clone();
            // ---- P2.3 显示模式协商（v1：显式 --mode 触发）----
            // 按显示目标设备路径切换分辨率/刷新率。**必须显式传入 `--mode` 才执行**，
            // 默认绝不动用户桌面布局（项目铁律 5）。
            // 虚拟屏（MttVDD）支持 800x600 ~ 3840x2160、30~165Hz 全档，
            // 因此可以让它匹配接收端设备，而不是把 800x600 拉伸给平板看。
            if let Some(spec) = arg_value(&args, "--mode") {
                let (w, h, hz) = match parse_mode_spec(&spec) {
                    Some(v) => v,
                    None => {
                        eprintln!("--mode 格式应为 WxH[@Hz]，例如 1920x1080@60，收到: {spec}");
                        std::process::exit(2);
                    }
                };
                println!(
                    "模式切换    : {} → {w}x{h}@{hz}Hz（显式 --mode 请求）",
                    target.label()
                );
                let r = unsafe {
                    remote_screen_host_win::displays::apply_display_mode(
                        &target.device_path,
                        w,
                        h,
                        hz,
                        target.bounds.x,
                        target.bounds.y,
                    )
                };
                match r {
                    Ok(()) => println!("模式切换    : 成功"),
                    Err(e) => eprintln!("模式切换失败（继续用原模式）: {e}"),
                }
                // 等系统应用新模式，然后重新枚举拿到真实生效的尺寸
                std::thread::sleep(std::time::Duration::from_millis(600));
                if let Some(t) = enumerate_displays()
                    .into_iter()
                    .find(|d| d.device_path == target.device_path)
                {
                    target = t;
                    println!(
                        "模式切换    : 当前生效 {}x{}@{}Hz",
                        target.mode.width, target.mode.height, target.mode.refresh_hz
                    );
                }
            }
            // 0x0 的未点亮目标采不出画面：以前会静默退成 16x16 空转，看着像在跑
            if !target.is_active || target.mode.width == 0 || target.mode.height == 0 {
                eprintln!(
                    "显示目标 [{index}] {} 未点亮（{}x{}），无法采集",
                    target.label(),
                    target.mode.width,
                    target.mode.height
                );
                std::process::exit(2);
            }
            println!("采集目标    : {}", target.label());

            // 是否把鼠标光标合成进画面（WGC 默认 true）。
            // 某些虚拟显示器上光标位置会抖动，用 `--no-cursor` 可对照排查。
            let include_cursor = !args.iter().any(|a| a == "--no-cursor");
            println!(
                "光标捕获    : {}",
                if include_cursor { "开启（默认）" } else { "关闭（--no-cursor）" }
            );

            let cfg = remote_screen_host_win::encode::EncodeConfig {
                width: target.mode.width.max(16),
                height: target.mode.height.max(16),
                fps,
                bitrate_kbps: bitrate,
                // GOP 保持 1 秒。曾缩到 0.5 秒想改善首帧，结果 I 帧翻倍把码率推到
                // 2.6Mbps、丢包恶化 —— 首帧该在"会话重建时强制 IDR"上解决，不是拿稳态码率换。
                gop: fps,
                pix_fmt: "bgra".into(),
            };
            let encoder = remote_screen_host_win::encode::detect_encoder(&ffmpeg);
            println!(
                "编码器      : {}  目标帧率: {} fps  目标码率: {} kbps  GOP: {} 帧",
                encoder, fps, bitrate, cfg.gop
            );
            let enc = match remote_screen_host_win::encode::FfmpegEncoder::start(
                &cfg, &ffmpeg, &encoder,
            ) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("编码器启动失败: {e}");
                    std::process::exit(6);
                }
            };

            // WebRTC 会话管理器：每个客户端一个独立 PeerConnection，
            // 新客户端接入只新建会话，不影响已在观看的客户端。
            let rtc = std::sync::Arc::new(
                remote_screen_host_win::webrtc::WebRtcHub::new(fps),
            );

            // ---- 输入回传（P2.5 / P2.6）：默认关闭，必须显式 --allow-input ----
            if args.iter().any(|a| a == "--allow-input") {
                remote_screen_host_win::input::enable();
                remote_screen_host_win::input::set_target_rect(
                    target.bounds.x,
                    target.bounds.y,
                    target.bounds.width.max(1),
                    target.bounds.height.max(1),
                );
                println!(
                    "输入回传    : 已启用（接收端可控制本机；目标区域 {},{} {}x{}）",
                    target.bounds.x, target.bounds.y, target.bounds.width, target.bounds.height
                );
            } else {
                println!("输入回传    : 关闭（默认；如需控制请加 --allow-input）");
            }

            // ---- 信令泵：独立线程 ----
            // 不能挂在采集回调上：虚拟屏静止时 WGC 可能很久才给一帧，
            // 而"新客户端接入 → 建会话 → 下发 offer"绝不能等帧
            // （实测：屏静止时回调几分钟不触发，客户端会一直卡在"等待 offer"）。
            // 另一个好处：广播变慢也不会拖住采集主循环。
            {
                let rtc = rtc.clone();
                let hub_pump = hub.clone();
                std::thread::spawn(move || {
                    use remote_screen_host_win::webrtc::SignalOut;
                    loop {
                        // 1) 新客户端接入 → 为它单独建一个会话
                        for id in hub_pump.poll_joined() {
                            println!("[webrtc] 客户端 #{id} 接入，建立独立会话");
                            if let Err(e) = rtc.join(id) {
                                eprintln!("[webrtc] 客户端 #{id} 建会话失败: {e}");
                            }
                        }
                        // 2) 客户端断开 → 销毁它的会话（其他会话不受影响）
                        for id in hub_pump.poll_disconnected() {
                            println!("[webrtc] 客户端 #{id} 断开，销毁会话");
                            rtc.leave(id);
                        }
                        // 3) 会话 → 浏览器：offer / candidate，**定向**发给所属客户端
                        let mut sigs = Vec::new();
                        while let Some(s) = rtc.poll_signal() {
                            sigs.push(s);
                        }
                        for (id, sig) in sigs {
                            match sig {
                                SignalOut::Offer { sdp } => {
                                    let ok = hub_pump.send_to(
                                        id,
                                        &format!(
                                            // cid 让接收端能显示自己对应宿主日志里的哪个会话号
                                            "{{\"t\":\"webrtc-offer\",\"cid\":{},\"sdp\":\"{}\"}}",
                                            id,
                                            json_escape(&sdp)
                                        ),
                                    );
                                    println!(
                                        "[webrtc] 客户端 #{id} offer 已发送：{}",
                                        if ok { "成功" } else { "失败（连接已断）" }
                                    );
                                }
                                SignalOut::Candidate {
                                    candidate,
                                    sdp_mid,
                                    sdp_mline_index,
                                } => {
                                    hub_pump.send_to(
                                        id,
                                        &format!(
                                            "{{\"t\":\"webrtc-cand\",\"candidate\":\"{}\",\"sdpMid\":\"{}\",\"sdpMLineIndex\":{}}}",
                                            json_escape(&candidate),
                                            sdp_mid.clone().unwrap_or_default(),
                                            sdp_mline_index.unwrap_or(0)
                                        ),
                                    );
                                }
                                SignalOut::State(s) => println!("[webrtc] #{id} {s}"),
                                SignalOut::Ctl(text) => handle_ctl(id, &text),
                            }
                        }
                        // 4) 浏览器 → 会话：answer / candidate，定向投递给所属会话
                        for (id, text) in hub_pump.poll_inbox() {
                            if let Some(sig) = parse_signal_json(&text) {
                                rtc.deliver(id, sig);
                            }
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                });
            }

            let mut enc2 = enc;
            let mut au_count: u64 = 0;
            let mut dropped: u64 = 0;

            let res = remote_screen_host_win::capture::capture_stream(
                target.bounds.x,
                target.bounds.y,
                target.mode.width,
                target.mode.height,
                seconds * 1000,
                fps,
                include_cursor,
                |bgra| {
                    let queued = enc2.push_frame(bgra)?;
                    if !queued {
                        dropped += 1;
                    }
                    let chunks = enc2.read_chunks()?;
                    for c in chunks {
                        // 编码只做一次，分发给所有活跃会话（各会话独立打包成 RTP）
                        rtc.push_frame(&c.data);
                        au_count += 1;
                    }
                    Ok(())
                },
            );

            rtc.close_all();
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
            println!("访问单元数: {au_count}   背压丢帧: {dropped}");
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
            let want = arg_value(&args, "--display");
            let (index, target) = match pick_display(&displays, index, want.as_deref()) {
                Some(v) => v,
                None => {
                    eprintln!(
                        "未找到匹配的显示目标（--display {:?}，共 {} 个）",
                        want,
                        displays.len()
                    );
                    std::process::exit(2);
                }
            };
            println!("目标: [{index}] {}", target.label());
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
            println!("用法: rs-host <displays [--json] | modes --index N | selftest | capture-probe [--display SUBSTR] | capture-diag [--display SUBSTR] [--seconds N] [--fps N] [--out FILE] | capture-dump | stream [--index N] [--display SUBSTR] [--port P] [--seconds S] [--fps N] [--bitrate K] [--nv12 file] | stream-webrtc [--display SUBSTR] [--mode WxH[@Hz]] [--fps N] [--bitrate K] [--seconds S] [--no-cursor] [--allow-input] | vdd-install [--inf PATH] [--hw Root\\MttVDD]>");
        }
    }
}

/// 读取 `--key value` 形式的参数值。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == key)?;
    args.get(pos + 1).cloned()
}

// ---------- DataChannel 控制消息（P2.1 / P2.3v2 / P2.5 / P2.6）----------
//
// 客户端通过 DataChannel `ctl` 上报三类消息（JSON，字段极简，手写解析即可）：
//   {"t":"device","w":CSS宽,"h":CSS高,"dpr":DPR,"portrait":bool}
//   {"t":"stat","fps":上屏帧率,"loss":丢包百分比,"rtt":毫秒,"jb":缓冲毫秒}
//   {"t":"input","k":"m","x":0~1,"y":0~1}          鼠标移动（归一化画面坐标）
//   {"t":"input","k":"b","btn":"left","d":true}    按键按下/抬起
//   {"t":"input","k":"w","d":120}                  滚轮（120 = 一格）
//   {"t":"input","k":"key","vk":65,"d":true}       键盘（Windows 虚拟键码）

/// 从扁平 JSON 文本里取数字字段（足够应付上面的固定结构，不引入 serde）。
fn json_num(s: &str, key: &str) -> Option<f64> {
    let pat = format!("\"{key}\":");
    let i = s.find(&pat)? + pat.len();
    let rest = s[i..].trim_start();
    let end = rest
        .find(|c: char| {
            !(c.is_ascii_digit() || c == '-' || c == '+' || c == '.' || c == 'e' || c == 'E')
        })
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// 取字符串字段。
fn json_str(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let i = s.find(&pat)? + pat.len();
    let j = s[i..].find('"')? + i;
    Some(s[i..j].to_string())
}

/// 取布尔字段。
fn json_bool(s: &str, key: &str) -> Option<bool> {
    let pat = format!("\"{key}\":");
    let i = s.find(&pat)? + pat.len();
    let rest = s[i..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

/// 按平板物理像素推荐虚拟屏模式（只给建议，是否应用由用户用 `--mode` 决定）。
fn recommend_mode(phys_w: f64, phys_h: f64) -> Option<String> {
    // 虚拟屏可用档位（实测评测枚举所得）
    const CANDIDATES: &[(u32, u32)] = &[
        (800, 600),
        (1280, 720),
        (1366, 768),
        (1920, 1080),
        (2560, 1440),
        (3840, 2160),
    ];
    let portrait = phys_h > phys_w;
    // 先按"面积最接近且不超过平板物理像素"挑选，其次取比例最接近的
    let target_ratio = phys_w / phys_h;
    let mut best: Option<(&(u32, u32), f64)> = None;
    for c in CANDIDATES {
        let (cw, ch) = if portrait { (c.1, c.0) } else { (c.0, c.1) };
        let area = (cw as f64) * (ch as f64);
        let ratio_err = ((cw as f64 / ch as f64) - target_ratio).abs();
        let over = if area > phys_w * phys_h { 1000.0 } else { 0.0 };
        let score = ratio_err * 1000.0 + over + (area / (phys_w * phys_h) - 1.0).abs() * 10.0;
        if best.map(|(_, b)| score < b).unwrap_or(true) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| {
        let (cw, ch) = if portrait { (c.1, c.0) } else { (c.0, c.1) };
        format!("{cw}x{ch}@60")
    })
}

/// 处理一条 DataChannel 控制消息。
fn handle_ctl(client_id: u64, text: &str) {
    let t = json_str(text, "t").unwrap_or_default();
    match t.as_str() {
        "device" => {
            let w = json_num(text, "w").unwrap_or(0.0);
            let h = json_num(text, "h").unwrap_or(0.0);
            let dpr = json_num(text, "dpr").unwrap_or(1.0);
            let portrait = json_bool(text, "portrait").unwrap_or(false);
            println!(
                "[ctl] #{client_id} 设备：CSS {w:.0}x{h:.0} · DPR {dpr:.2} · {} · 物理 {:.0}x{:.0}",
                if portrait { "竖屏" } else { "横屏" },
                w * dpr,
                h * dpr
            );
            if let Some(m) = recommend_mode(w * dpr, h * dpr) {
                println!("[ctl] #{client_id} 建议虚拟屏模式：{m}（用 --mode {m} 应用）");
            }
        }
        "stat" => {
            let fps = json_num(text, "fps").unwrap_or(0.0);
            let loss = json_num(text, "loss").unwrap_or(0.0);
            let rtt = json_num(text, "rtt").unwrap_or(0.0);
            let jb = json_num(text, "jb").unwrap_or(0.0);
            // 页面在后台时接收端解码被浏览器暂停，上报的 fps 是冻结值，
            // 必须标注出来，否则会被误判成"链路掉到 2fps"。
            let bg = json_num(text, "bg").unwrap_or(0.0) > 0.5;
            // 画质建议：丢包是主要判据（>3% 已经影响观感）
            let advice = if loss > 3.0 {
                "建议降码率（丢包 >3%）"
            } else if loss < 0.5 && fps > 25.0 {
                "链路良好，可尝试提码率"
            } else {
                "维持当前码率"
            };
            let note = if bg { "（页面后台，帧率冻结）" } else { "" };
            println!(
                "[ctl] #{client_id} 质量：上屏 {fps:.1}fps{note} 丢包 {loss:.2}% RTT {rtt:.0}ms 缓冲 {jb:.0}ms → {advice}"
            );
        }
        "input" => {
            if !remote_screen_host_win::input::input_enabled() {
                return; // 未开启输入回传：静默忽略
            }
            let k = json_str(text, "k").unwrap_or_default();
            match k.as_str() {
                "m" => {
                    let x = json_num(text, "x").unwrap_or(0.0);
                    let y = json_num(text, "y").unwrap_or(0.0);
                    remote_screen_host_win::input::mouse_move(x, y);
                }
                "b" => {
                    let btn = json_str(text, "btn").unwrap_or_default();
                    let down = json_bool(text, "d").unwrap_or(false);
                    remote_screen_host_win::input::mouse_button(&btn, down);
                }
                "w" => {
                    let d = json_num(text, "d").unwrap_or(0.0) as i32;
                    remote_screen_host_win::input::wheel(d);
                }
                "hw" => {
                    let d = json_num(text, "d").unwrap_or(0.0) as i32;
                    remote_screen_host_win::input::hwheel(d);
                }
                "key" => {
                    let vk = json_num(text, "vk").unwrap_or(0.0) as u16;
                    let down = json_bool(text, "d").unwrap_or(false);
                    remote_screen_host_win::input::key(vk, down);
                }
                _ => {}
            }
        }
        _ => {}
    }
}

/// 解析 `--mode` 参数：`WxH@Hz` 或 `WxH`（省略刷新率则用 60Hz）。
///
/// 例：`1920x1080@60`、`1280x720`、`2560x1440@120`。
fn parse_mode_spec(s: &str) -> Option<(u32, u32, u32)> {
    let (res_part, hz) = match s.split_once('@') {
        Some((r, h)) => (r, h.trim().parse::<u32>().ok()?),
        None => (s, 60),
    };
    let (w, h) = res_part
        .split_once('x')
        .or_else(|| res_part.split_once('X'))?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?, hz))
}

/// 按**稳定身份**选屏，不要只靠下标：显示器枚举顺序会随插拔/驱动重载漂移
/// （实测同一块 MttVDD 虚拟屏的 index 从 5 漂到 43，用户按旧下标跑会采到 0x0 的空设备）。
///
/// `--display <子串>` 匹配 label / 设备路径 / 适配器 ID，例如
/// `--display MttVDD`、`--display DISPLAY76`、`--display ToDeskIdd`。
fn pick_display<'a>(
    displays: &'a [remote_screen_core::DisplayInfo],
    index: usize,
    want: Option<&str>,
) -> Option<(usize, &'a remote_screen_core::DisplayInfo)> {
    match want {
        Some(sub) if !sub.trim().is_empty() => {
            let s = sub.trim().to_ascii_lowercase();
            displays.iter().enumerate().find(|(_, d)| {
                let hay = format!("{} {} {}", d.label(), d.device_path, d.adapter_device_id)
                    .to_ascii_lowercase();
                hay.contains(&s)
            })
        }
        _ => displays.get(index).map(|d| (index, d)),
    }
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

/// 从 JSON 文本中提取一个字符串字段的值（已做反转义）。
fn json_get_string(text: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = text.find(&pat)? + pat.len();
    let rest = &text[start..];
    let mut out = String::new();
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next() {
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('t') => out.push('\t'),
                Some('"') => out.push('"'),
                Some('\\') => out.push('\\'),
                Some('/') => out.push('/'),
                Some('u') => {
                    let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        if let Some(ch) = char::from_u32(n) { out.push(ch); }
                    }
                }
                _ => {}
            },
            c => out.push(c),
        }
    }
    None
}

/// 从 JSON 文本中提取一个整数字段。
fn json_get_int(text: &str, key: &str) -> Option<i64> {
    let pat = format!("\"{key}\":");
    let start = text.find(&pat)? + pat.len();
    let rest = text[start..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse::<i64>().ok()
}

/// 解析浏览器回传的信令 JSON，转成 `SignalIn`。
fn parse_signal_json(text: &str) -> Option<remote_screen_host_win::webrtc::SignalIn> {
    use remote_screen_host_win::webrtc::SignalIn;
    let t = json_get_string(text, "t")?;
    match t.as_str() {
        "webrtc-answer" => {
            let sdp = json_get_string(text, "sdp")?;
            Some(SignalIn::Answer { sdp })
        }
        "webrtc-cand" => Some(SignalIn::Candidate {
            candidate: json_get_string(text, "candidate").unwrap_or_default(),
            sdp_mid: json_get_string(text, "sdpMid"),
            sdp_mline_index: json_get_int(text, "sdpMLineIndex").map(|v| v as u16),
        }),
        _ => None,
    }
}
