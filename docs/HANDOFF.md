# uulan 项目交接给接手 agent

> 这份文档是**自包含**的：接手方（Google Antigravity 上的 agent）没有历史上下文，
> 读完这一份就能接上。写于 2026-11-23，对应仓库 `1849b01`。

## 0. 一句话

在**完全物理隔离的内网**里自建的远程控制工具（对标 UU 远程 / 向日葵），
被控端面向**银河麒麟 V10 SP1**，控制端支持麒麟与 Windows。
不依赖任何公网服务：信令、中继、加密全部自己实现，一台内网机器当服务器即可。

- 仓库：`https://github.com/zncznc/uulan`（**公开**，MIT）
- 规模：9 个 crate、约 4.4 万行 Rust、200 个提交
- 测试：`cargo test --workspace` → **578 通过 / 0 失败 / 1 忽略**（45 个目标）
- 本地路径（原开发机）：`D:\default-workspace\uulan`
- 真机：麒麟 V10 SP1 @ `192.168.223.15`，用户 `mis`

## 1. 你现在接手的是什么状态

**能用的**（都真机验过，`README.md` 的功能状态表有逐项验证程度）：
远程桌面（1920x1080@30fps、首帧 216ms）、键鼠注入、剪贴板文本双向、
文件传输（含目录树、哈希校验）、端口映射（ssh -L 语义、全双工）、
终端（PTY/ConPTY）、音频（Opus 第 4 条通道）、端到端加密（E2E + PAKE）、
断线重连、开机自启、离线安装包与就地升级、直连与中继两开、每人独立口令。

**最新一轮（M8）**：bulk 通道改成**按流滑动窗口 + 握手协商窗口帧数**，
解决"一条流卡住拖住整条连接"；补了多控制端并发、会话开关泄漏、双端延迟台架。

**近期新增的认证模式**（用户最新要求，已实现并真机验过）：
- 主路：**被控机用户名 + 该系统账户的登录密码**（走 PAM，跳过 PAKE）
- 备路：**一次性临时码**（走原有 PAKE，用户明确要求保留为备用）

## 2. 用户的硬性要求（都是本人原话，请严格遵守）

1. **中文回复。** 一律中文。
2. **省 token。** 用户明确说过"以后 uulan 不需要输出太多的结果文字给我"、
   "尽量精简输入输出"。**只给结论、关键数字、失败点**，不要长篇复述过程。
3. **GUI 是交付要求。** 用户原话："一定要用命令才能使用吗？我需要的是和原版一样的
   图形化界面"、"可不可以改成原生那种，直接随系统启动，主控端直接输 IP，
   用户名密码就可以进行远程被控端"。双击可用、随系统启动是必须的。
4. **每个里程碑自动提交并推 GitHub。** 用户原话："以后每完成一个里程碑自动更新
   GitHub"。仓库是**公开**的（用户要求调成 public），MIT 许可。
5. **关键里程碑用 Telegram 推送**给他（`tools/notify.sh` 已实现）。
6. **被控端使用流程**：被控机上装客户端 → 对方把**设备 ID + 临时码**告诉你 →
   你输进去连。用户也要求过"生成一个安装包，发给远程麒麟机让他安装"。
7. **改文件要原地修改**，不要新建一堆副本文件。
8. **内网互联为主**：用户说"我们平时只是内网互联"，所以不要过度设计公网穿透。

## 3. 架构（改代码前必须知道）

```
被控端（麒麟 V10 SP1）          中继（任一内网机器）        控制端（麒麟 / Windows）
  X11 采集 ──► libx264 ──┐                              ┌── ffmpeg 解码 ──► 窗口
  XTest 注入 ◄───────────┤   四条 TCP 通道，盲转发        ├── 键鼠事件
  剪贴板读写 ◄───────────┘  control/video/bulk/audio    └── 剪贴板读写
```

| crate | 职责 |
|---|---|
| `uulan-protocol` | 线格式、帧编解码、加密会话、PAKE（CPace/Ristretto255）、中继协议 |
| `uulan-server` | 中继：设备台账 + 盲转发（不认识内容，只 `copy_bidirectional`） |
| `uulan-host` | 被控端：采集/编码/注入/剪贴板/PTY/文件/端口映射/音频 |
| `uulan-client` | 控制端：解码显示、事件上报、窗口、终端、文件 |
| `uulan-gui` | 图形面板（egui，Windows 与麒麟同一份代码） |
| `uulan-media` | ffmpeg 子进程、AU 切分 |
| `uulan-clip` / `uulan-fs` / `uulan-bulk` | 剪贴板后端 / 文件系统 / 多路复用 |

**六条关键设计决策（踩过坑的结论，理由在 `docs/DESIGN.md`）：**
1. **不用 WebRTC** —— 内网延迟 <1ms、无 NAT，ICE/SDP/DTLS/SRTP 是纯负担。
2. **不链接 libavcodec** —— 走 FFmpeg CLI 子进程，麒麟源里本来就有 ffmpeg。
3. **中继是盲转发** —— 服务器被拿下也拿不到屏幕或凭据。
4. **AU 切分靠 AUD** —— 编码端显式 `-aud 1`；`first_mb_in_slice == 0` 绝不能当主判据，
   否则一帧会被劈成 16 个假 AU。
5. **会合连接读写必须分开** —— `select!` 会丢弃未选中的 future，读帧不是取消安全的。
6. **OS 认证模式不用 PAKE** —— 见下节，这是密码学硬约束不是偷懒。

## 4. 认证体系（最近改动最多的部分，务必读懂）

三条认证路径，按需组合：

| 模式 | 凭据 | 走 PAKE？ | 适用 |
|---|---|---|---|
| 口令（默认） | 12 位访问口令 | ✅ | 长期无人值守接入 |
| 一次性临时码 | 屏幕上的大字码 | ✅ | 日常让人协助（用完即废、每整点轮换） |
| OS 模式 | 系统账户名 + 该账户登录密码 | ❌ 跳过 | 用户最终要的"就输用户名密码" |

**为什么 OS 模式不能用 PAKE（硬约束，不要试图"优化"回去）：**
PAKE 要求**双方都知道那个秘密**；被控端只能**验证**系统密码、不能**复述**它。
所以 OS 模式的会话密钥只能是纯 DH。代价必须写在代码里也要告诉用户：
**中继在理论上能中间人并看到系统登录密码**；直连没有中继，不受影响。

**实现要点（改动前必看）：**
- 新增明文一帧 `AuthMode { os: bool }`（只含布尔、不含秘密），DH 之后先声明走哪条路；
  被控端据此决定跑 PAKE 还是收 `PamAuth`。**已验通的 PAKE 流程一行未改。**
- OS 分支：允许名单 → PAM → `AuthOk{idx:0, proof:""}`，且**跳过 rekey**
  （同一把密钥把计数器归零 = nonce 重用），控制端同步不 rekey。
- `auth_os` 的主机上 P2 第二因素**整套退场**，否则临时码备路还要再输一次系统密码，
  那就不是备用入口而是同一个门。`HelloOk.pam_required = pam.enabled && !auth_os`。
- **允许名单外绝不调用 PAM** —— 否则任何拿到口令的人都能拿 root 反复触发系统账户锁定。
- `os_checker(Option<Checker>)` 遇 `None` 必须回落 `system_checker`。
  （踩过：我一度把 `None` 当致命错误回 `AuthFail(NotAllowed)`，
  症状是"账户明明在名单里却被拒绝、日志还看不出原因"。）

## 5. 构建与测试

```bash
# Windows（本机）：无 MSVC/SDK，用 gnu 工具链
export TEMP='C:\Users\chaoc\AppData\Local\Temp'; export TMP="$TEMP"
export PATH="/d/tools/w64devkit/w64devkit/bin:$HOME/.cargo/bin:$PATH"
cargo test --workspace

# 麒麟（192.168.223.15）
export PATH=$HOME/.cargo/bin:$PATH
cargo test --workspace
```

**测试口径（吃过亏，务必照做）：**
- 用 `cargo test --workspace` 当门槛，不能只 `cargo build`（它不编译集成测试）。
- **统计不要用管道聚合**：本层 shell 里 `awk`/`grep -c` 汇总 `test result` 会返回空或错值。
  一律写文件后用 Python `re.findall(r"test result: ok\. (\d+) passed")` 统计。
- 部署后**立刻比哈希**（`sha256sum`），别信 `cp` 成功的回显。
- 单条测试的重复跑要 `--test-threads=1`（真剪贴板测试还需 `UULAN_CLIPBOARD_TESTS=1`）。

## 6. 部署与真机

```bash
# 离线包（含 GUI）
bash deploy/make-package.sh --with-gui     # 产出 uulan-<日期>-x86_64-linux.tar.gz

# 麒麟上装（普通用户，无需 root）
bash install.sh --userland
bash scripts/install-kylin-gui.sh          # 装桌面图标 + 菜单项

# 被控端服务（用户单元）
systemctl --user status uulan-host
# 中继（系统单元）
systemctl status uulan-server
```

真机 `192.168.223.15` 的现状：`uulan-host.service`（用户单元，`ExecStart` 带
`--listen 0.0.0.0:21117`，中继+直连两开）、`uulan-server.service`（系统单元）。
配置 `~/.config/uulan/host.json`（`auth_os=true`、`pam.allow_users=["mis"]`）。

## 7. 已知限制（不要当 bug 修）

- **Wayland 会话抓不到**，只支持 X11。
- **锁屏/登录界面不出画面**：锁屏时被控端会在进 Mapper 前丢弃输入并松开已按下的键，
  这是**刻意的安全设计**，不是缺陷。
- **无人值守会话拿不到 Windows 剪贴板**（`OpenClipboard` Win32 错误 5），环境限制。
- **PAKE 是本仓库自研（CPace/Ristretto255），没有第三方审计。**
- 端口映射没有单独的麒麟真机记录（真二进制本机全链路验过）。

## 8. 交接时**尚未完成 / 需要你接手的**

1. **`mis` 真实密码的成功路径没验过。** 失败路径验了（用必然不存在的账户），
   成功路径需要用户本人的系统登录密码 —— **不要向用户索要，让他自己点一次**。
2. **麒麟真机 192.168.223.15 当时被我压垮了**：我在它上面并发跑了两个 release 构建，
   sshd 收连接后不给 banner、中继与直连都 `early eof`（端口本身照常 accept，
   说明是用户态资源耗尽）。恢复命令：
   ```bash
   rm -rf ~/.local/uulan/srcE/target ~/.local/uulan/srcF/target
   systemctl --user restart uulan-host
   sudo systemctl restart sshd      # 若仍不给 banner
   ```
   **教训：那台是海光 8 核，不要并发构建，一次只跑一个，先看 `free -h` / `df -h`。**
3. **D 盘整理被用户喊停**（他说"暂停"）。原开发机 D 盘根有 24 个 `uulan-*` 目录约 12 GB，
   多为带 `target/` 的陈旧源码副本。**未动任何文件**。用户如果要继续，做法是
   "先列名单 → 只搬不删 → 搬完逐项核对"，动手前先把清单给他看。
4. 余项是硬件与调参边界（见 `docs/milestones/` 里 M8 之后各篇的"未做的"段落）。

## 9. 工作方式（原 agent 的规矩，建议沿用）

- **失败必须显眼**：GUI 有 `alert` 红字 + 子进程秒退检测 + 会话日志落盘。
  教训是"反馈藏在角落等于没有反馈"——用户曾因为口令打错一个字符而以为程序坏了。
- **报障前先拿证据**：真机日志、二进制哈希、进程状态。不要凭推理下断言。
  用户对错误结论零容忍，会用"再仔细看看"指正。
- **多行提交信息一律写文件再 `git commit -F`**：内联进 `-m` 时若含引号/中文标点
  会让 shell 提前闭合参数，提交信息被截断，且后面的 `&& git push` **静默不跑**。
- **里程碑文档**写成 `docs/milestones/<主题>-<日期>.txt`，要含：需求口径的演变、
  真机证据原文、抓到的 bug 与根因、**未验的部分**（这一条尤其重要）。
- 每次改完密码学/认证相关代码，跑全量测试 + 真机复验，两样都要。

## 10. 从哪里开始读

1. `README.md` —— 功能状态表、实测数据、已知限制
2. `docs/DESIGN.md` —— 总体设计、协议、为什么不用 WebRTC/GTK
3. `docs/DEPLOY-KYLIN.md` —— 真机部署全过程、每一个坑的根因
4. `docs/milestones/` —— 90 篇里程碑实测记录（按日期）
5. `crates/uulan-host/src/session.rs` —— 会话与认证主线（OS 模式改动都在这）
6. `crates/uulan-host/src/pam.rs` —— PAM 第二因素
7. `crates/uulan-gui/src/main.rs` + `local.rs` —— 图形面板两页

## 11. 开源与许可

仓库公开、MIT。为什么能选 MIT：没有链接 GPL 库，ffmpeg 只作为**子进程**调用
（命令行调用不传染），详见 `README.md` 的"依赖许可证"一节。**改依赖时要重新核对这条。**
