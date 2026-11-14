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
            println!("用法: rs-host <displays [--json] | modes --index N | selftest | capture-probe [--index N] [--seconds S] | capture-dump [--index N] [--out file.nv12]>");
        }
    }
}

/// 读取 `--key value` 形式的参数值。
fn arg_value(args: &[String], key: &str) -> Option<String> {
    let pos = args.iter().position(|a| a == key)?;
    args.get(pos + 1).cloned()
}
