# VDD 驱动加载的四条出路

背景见 `docs/01-progress.md` 第七节。设备节点 `ROOT\MTTVDD\0000` **已建立**，
但 UMDF 驱动 `MttVDD.dll` 被系统拒绝加载：

```text
CM_PROB_FAILED_START / 0x8007001f (ERROR_NOT_SUPPORTED)
```

## 路线 A：启用 Windows 测试模式（让 SignPath 签名的驱动可加载）

```cmd
bcdedit /set testsigning on
:: 重启后生效；桌面右下角会显示"测试模式"水印
:: 回滚： bcdedit /set testsigning off
```

- 代价：系统安全性下降，内核启动有水印；内核更新后可能被重置。
- 适用：仅开发/测试机可接受时。

## 路线 B：直接用本机已有的、能正常工作的 IddCx 驱动（推荐先走这条）

本机已装 3 个第三方 IddCx 虚拟显示器驱动，状态均为"已启动"：

| 适配器 | 枚举器 | 节点数 |
|---|---|---|
| Parsec Virtual Display Adapter | `Root\Parsec\VDA` | 16 |
| Todesk Virtual Display Adapter | `Root\tdIdd` | 3 |
| AskLink Display Adapter (SDR) | `Root\AskLinkDisplayAdapter` | 8 |

它们不暴露模式的原因是**客户端未建立会话**——IddCx 虚拟显示器由客户端通过
IddCx API 动态创建/销毁，不是驱动常驻创建。

操作：启动 ToDesk / Parsec 客户端并建立一次连接（连到本机或任一在线设备均可），
其虚拟显示器就会出现；此时 `rs-host displays` 应能看到**有分辨率**（非 0x0）的虚拟屏。

- 代价：零安装、零风险，但依赖第三方客户端在场。
- 收益：立刻能拿到一块真实虚拟屏，完成"扩展屏采集"闭环。

## 路线 C：换成 Microsoft attestation 签名的官方 IddCx 驱动

微软官方商用 **Virtual Display Driver**（Microsoft Store 分发，MSIX 安装，Microsoft 签名）。

- 代价：商业授权；需 Store 账号。
- 适用：产品化阶段（正式版不应该依赖测试模式或第三方客户端）。

## 路线 D（长期）：自研 IddCx 驱动

自研 + 微软认证签名，完全自主可控。属于产品化阶段的工作量（3~6 个月 + 签名成本）。

## 验证顺序建议

1. 先做 **路线 B**（零成本、最快拿到可采集的虚拟屏）→ 立刻完成"扩展屏采集"闭环；
2. 同时在测试机或接受水印的机器上试 **路线 A**，验证 MttVDD 能否在测试模式下加载并自建显示器；
3. 产品化走 **路线 C 或 D**。
