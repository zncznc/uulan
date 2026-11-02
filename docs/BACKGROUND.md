# uulan · 背景与外部借鉴

> 记录时间：2026-11-01。项目处于暂停状态（夜间批已停），本文只是归档背景，不含代码改动。

## 一、竞品动态：UU 远程传出商业化

来源：X（Twitter）用户 @cheery9998 的推文（2026-11-01），属**第三方推测，不是官方公告**，
记录时请保持这个定性，不要在后来的文档里当成事实传播。

要点：

1. 跨境远控可能收费 —— 成本摆在那里；
2. 终端（PTY）功能可能收费 —— 算是硬核功能；
3. **Linux 版本排期靠后**，作者的说法是"多点人力出来搞 Linux 版本，快点端上来"；
4. 无广告工具免费运营两三年难以持续，商业化本身被这位用户期待。

### 对 uulan 的意义（为什么值得记）

这三点恰好落在 uulan 已经做完并**在真麒麟上验过**的地方：

| UU 的短板 | uulan 现状（真机实测） |
|---|---|
| Linux 版本靠后 | 被控端本来就是麒麟 V10 SP1；控制端在麒麟上也能编、能装、能跑（29.7 fps / `painted=870` / 相关系数 0.9998） |
| 终端可能收费 | PTY 终端已通：真 ConPTY（Windows）与真 `/dev/pts`+termios+SIGHUP→SIGKILL（麒麟），退出码原样回来 |
| 跨境成本高 | uulan 定位物理隔离**内网**，中继自建、可直连，不存在跨境成本 |

**不要过度解读**：这是市场动向的参考，不是立项理由。uulan 的立项理由是内网环境里没有可用的合规远控。

## 二、技术借鉴：vercel-labs/agent-desktop

来源：同上那条推文，2026-11-01 已核实。

- 仓库 `vercel-labs/agent-desktop`，Rust，Apache-2.0，⭐7029，创建 2026-02-19，最近推送 2026-11-01，102 个 open issue；
- npm `agent-desktop@1.0.2`，零依赖，自带 x64/ARM64 原生二进制；
- 60 个命令名，56 个可用（`key-down`/`key-up`/`mouse-down`/`mouse-up` 四个在"守护进程能可靠释放按住状态"之前拒绝运行）。

它的路线与 Hermes 自己的 `computer_use`（底层 cua-driver）**完全相同**：读 OS 无障碍树 → 每个元素一个 ref →
按 ref 操作 → 再快照确认，全程不做像素匹配，默认不移动指针、不抢焦点（`--headed` 才做物理输入）。

### 值得借鉴的三处设计（不是搬架构，是搬契约）

1. **显式的重试契约**：每个错误都带 `error.disposition.retry`，**只有写着 `safe` 才允许重试**。
   这比"先试一下再看返回值"干净——把"这次失败能不能重来"变成接口的一部分，而不是调用方的猜测。
2. **ref 绑定快照命名空间**：ref 形如 `@s8f3k2p9:e8`，快照过期返回 `STALE_REF`、多窗口返回
   `AMBIGUOUS_TARGET`；macOS 上还会常驻一个私有命令宿主保留原生对象，避免"被删除的行静默解析到同名的幸存行"。
3. **能力面**：`list-notifications` / `dismiss-notification` / `notification-action` 与
   `clipboard-get` / `clipboard-set` / `clipboard-clear`，以及 `check`/`uncheck`/`expand`/`collapse`
   这类语义动作。Hermes 的 `computer_use` 目前没有通知类动作，剪贴板要靠 PowerShell 绕。

### 对 uulan 注入层的直接启示

第 1 条与 **M3 的键鼠注入**是同一个问题：注入动作必须**可安全重放**，且"这次注入到底生效没有"
不能靠调用方猜。uulan 现在被控端用 `InputStats`（移动/按键/点击/滚轮 + 未送出 + 被控端拒绝）回报，
方向是对的；若要继续做，可以照这个思路把"**这次操作是否幂等、失败能否重试**"写进协议回执，
而不是让控制端凭经验重发。

## 三、参考

- 推文：`https://x.com/cheery9998/status/2116795015141990864`（内容为二手转述 + 一条引用推文）
- 仓库：`https://github.com/vercel-labs/agent-desktop`
- 我们的对应实现：Hermes skill `computer-use`（`computer_use` 动作词表）、uulan 的
  `crates/uulan-protocol/src/msg.rs`（`C2H::Mouse*`/`Key*`）与 `crates/uulan-host/src/input/`