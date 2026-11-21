# 输入回传实测与坐标映射修复（P2 最后一块）

日期：2026-11-21
范围：P2.5/P2.6 鼠标/触摸/键盘回传链路的坐标映射部分

## 1. 为什么要实测

输入回传的代码在 P2 阶段已写完，但一直只做了编译验证，没有实测过
「接收端归一化坐标 → 目标屏绝对像素 → SendInput 0~65535」这条映射。
坐标类问题不实测无法确认真对：差一块屏的偏移量在代码审查里看不出来。

## 2. 实测方法

新增 `crates/host-win/examples/verify_input.rs`：

1. 枚举本机所有有效显示器，逐块把它的 `bounds` 登记为目标区域；
2. 把光标依次送到 (0,0)、(0.5,0.5)、(1,1)、(0.25,0.75) 四个归一化点；
3. 用 `GetCursorPos` 读回真实落点，与理论像素坐标对比，容差 2px；
4. SendInput 是异步注入，读取前必须轮询到光标连续 3 次位置不变，
   否则读到上一个目标点，会把采样竞态误判成映射错误；
5. 另附 `SetCursorPos` 写入回读基准与接缝扫描，用于区分「算法错」与「OS 行为」。

运行：

```bash
source scripts/env.sh
cargo run --release -p remote-screen-host-win --example verify_input
```

## 3. 发现的问题与修复

### 3.1 虚拟桌面原点未减去（真实缺陷，已修复）

`bounds.x/y` 来自 `EnumDisplaySettingsExW` 的 `dmPosition`，是虚拟桌面绝对坐标，
当某块屏被放在主屏左侧/上方时可为负；而 `MOUSEEVENTF_VIRTUALDESK` 的 0~65535
是相对**整个虚拟桌面矩形**归一化的。原实现直接 `abs_x / 虚拟桌面宽度`，
没有减 `SM_XVIRTUALSCREEN/SM_YVIRTUALSCREEN`，屏在左侧时会整体偏移一块屏宽度。

修复：`virtual_desktop_size()` → `virtual_desktop_geometry()` 返回 (原点x, 原点y, 宽, 高)，
`map_to_absolute()` 改为 `(abs - origin) / size * 65535`。

本机原点为 (0,0)，所以这项修复在当前布局下不改变实测数值，但消除了负坐标布局的系统性错误。

### 3.2 进程 DPI 无关导致坐标口径分裂（真实缺陷，已修复）

现象（修复前）：

```text
主屏 GetSystemMetrics : 1536x864      ← DPI 虚拟化（125%）
主屏 dmPosition/bounds: 1920x1080     ← 物理像素
虚拟桌面 GetSystemMetrics: 3200x864   ← 宽物理、高虚拟，混合口径
主屏 (1,1)     期望 (1920,1080) 实际 (1535,690)   FAIL
虚拟屏 (0,0)   期望 (1920,0)    实际 (1535,0)     FAIL
SetCursorPos(1919,540) → 读回 (1535,432)          ← OS 自己重定位
SetCursorPos(960,1000) → 读回 (1920,719)          ← 屏幕下部不可达
```

根因：进程未声明 DPI 感知。GDI 类 API 返回缩放后的逻辑坐标，
`EnumDisplaySettings` 返回物理坐标，两套口径混用。

修复：在 `rs-host` 的 `main()` 最开头（任何 GDI/SendInput 之前）调用
`input::init_dpi_awareness()` → `SetProcessDPIAware()`。

修复后：

```text
主屏 (1,1)  期望 (1920,1080) 实际 (1919,1079) 误差 (1,1) PASS
SetCursorPos(960,1079) → 读回 (959,1079)     ← 屏幕底部恢复可达
```

## 4. 最终结果

```text
结果: 7 PASS / 1 DPI_SHIFT（OS 跨屏重定位，已知限制）/ 0 其它 FAIL，共 8
```

七个点误差 ≤1px，包含此前完全不可达的主屏右下角。

## 5. 未修复项（明确列出）

**跨 DPI 屏移动时的光标重定位**：虚拟屏 (0.25,0.75) 期望 (2240,540)，
实际 (1791,431)，恰为期望 ×0.8（125%→100% 缩放系数）。

- 位置：`crates/host-win/src/input.rs::mouse_move` 使用 `MOUSEEVENTF_ABSOLUTE |
  MOUSEEVENTF_VIRTUALDESK` 的路径；本机主屏 125% DPI、虚拟屏 100% DPI。
- 影响：光标从虚拟屏向主屏方向移动、跨越接缝时可能跳一下；平板端会看到光标位置突跳。
  屏内操作（绝大多数场景）不受影响。
- 可复现：稳定复现，`verify_input` 中标为 `DPI_SHIFT`。
- 尝试过但无效：同一绝对坐标连续注入两次（已回退，避免无意义重复事件）。
- 建议后续动作：改用「当前光标位置 + 相对移动（`MOUSEEVENTF_MOVE`，不带 ABSOLUTE）」
  避开绝对坐标的 DPI 重解释；或把虚拟屏 DPI 与主屏设为一致。
  这两项都超出本次「5 分钟实测」范围，未实施。

## 6. 影响面覆盖表

| 改动点 | 受影响流程 | 验证方式 | 状态 |
|---|---|---|---|
| `input.rs` 虚拟桌面原点 | 输入回传坐标映射 | `verify_input` 8 点逐像素读回 | 已验证（7 PASS，1 OS 限制） |
| `input.rs` DPI 感知初始化 | 所有 `rs-host` 子命令 | `cargo build --release` + `rs-host displays` | 已验证 |
| `main.rs` 启动时调用 DPI 初始化 | WebRTC 主链路 | `stream-webrtc --allow-input --seconds 4` 短跑：模式切换、QSV、输入回传目标登记均正常 | 已验证 |
| 浏览器 → DataChannel → `handle_ctl` → `input` | 真实触摸/键盘回传 | **未验证**：需要平板端实际触摸操作 | 未验证（原因：本次只做本机硬验证） |
| 键盘 `key` / 滚轮 `wheel` 的实际界面效果 | 远端打字/滚页 | **未验证**：只验证了编译与消息解析路径 | 未验证（原因：需目标窗口配合） |
| 负坐标虚拟桌面布局 | 屏在左侧/上方 | **未验证**：本机布局原点为 (0,0) | 未验证（原因：需改用户桌面布局，不擅自改） |

## 7. 交付物

- `crates/host-win/src/input.rs`：DPI 感知初始化、虚拟桌面原点修正
- `crates/host-win/src/main.rs`：启动时调用 DPI 感知初始化
- `crates/host-win/examples/verify_input.rs`：可重复运行的坐标映射实测探针
