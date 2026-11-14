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
        _ => {
            println!("用法: rs-host <displays [--json] | selftest>");
        }
    }
}
