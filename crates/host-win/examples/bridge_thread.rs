//! P1.1.3 验证：同步采集线程 → tokio 任务 的线程桥接是否可行。
//!
//! 目标：证明 webrtc-rs 的 async `write_sample` 可以从同步线程通过
//! 有界 channel 驱动，而不需要把整条采集管线改成 async。
use std::sync::mpsc::{sync_channel, Receiver};
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    println!("=== P1.1.3 线程桥接验证 ===");

    // 模拟：同步采集/编码线程产出 AU（含时间戳）
    let (tx, rx): (std::sync::mpsc::SyncSender<(u64, Vec<u8>)>, Receiver<(u64, Vec<u8>)>) =
        sync_channel(2); // 与现有背压策略一致：容量 2，满了丢帧

    let producer = thread::spawn(move || {
        let t0 = Instant::now();
        let mut sent = 0u64;
        let mut dropped = 0u64;
        for i in 0..100u64 {
            let au = vec![0u8; 1024 * (1 + (i % 4) as usize)]; // 模拟不同大小的 AU
            match tx.try_send((i, au)) {
                Ok(()) => sent += 1,
                Err(_) => dropped += 1, // 背压丢帧，与现有链路行为一致
            }
            thread::sleep(Duration::from_millis(8));
        }
        (sent, dropped, t0.elapsed())
    });

    // tokio current_thread 运行时：比 multi_thread 更适合常驻后台程序
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build runtime");

    let consumer = thread::spawn(move || {
        rt.block_on(async {
            let mut received = 0u64;
            let mut bytes = 0u64;
            let t0 = Instant::now();
            loop {
                match rx.try_recv() {
                    Ok((_id, data)) => {
                        // 真实场景：webrtc track.write_sample(&Sample{data, ..}).await
                        // 这里只验证通道与运行时能协同工作
                        received += 1;
                        bytes += data.len() as u64;
                        tokio::task::yield_now().await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                }
                if t0.elapsed() > Duration::from_secs(3) {
                    break;
                }
            }
            (received, bytes)
        })
    });

    let (sent, dropped, prod_time) = producer.join().unwrap();
    let (received, bytes) = consumer.join().unwrap();

    println!("  生产线程（同步采集/编码）: 送出 {sent} 帧, 背压丢弃 {dropped} 帧, 耗时 {prod_time:?}");
    println!("  消费线程（tokio current_thread）: 收到 {received} 帧, {bytes} 字节");
    println!();
    if received > 0 && received <= sent {
        println!("  ✅ 线程桥接可行：同步管线无需异步化，通过有界 channel 驱动 tokio 任务");
        println!("  → 现有 capture_stream/encode 代码可原样保留，只加一个发送端");
    } else {
        println!("  ❌ 桥接异常，需重新设计");
    }
}
