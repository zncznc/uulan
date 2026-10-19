# uulan —— 内网（物理隔离）远程桌面系统 · 设计文档

> 目标：在**完全无法访问公网**的内网里，给**银河麒麟 V10 SP1 x86_64** 做一套「UU 远程」级别的远控：
> 远程桌面 / 键鼠 / 剪贴板 / 文件传输 / 终端 / 端口映射，
> 控制端同时支持 **麒麟(Linux)** 和 **Windows**。

参考项目：[VallierDev/uu-remote-linux](https://github.com/VallierDev/uu-remote-linux)（GPL-3.0）。
只借鉴技术选型与工程经验，**不复制其协议栈**——因为它整套协议是跟网易云绑死的，隔离网里一行都跑不起来。

---

## 1. 为什么不能直接移植参考项目

参考项目是「非官方 UU 远程 Linux **被控端**」。它的命脉全部挂在网易云：

| 环节 | 依赖 |
|---|---|
| 登录 | `POST /qrcode/gen/login` + 轮询 `/qrcode/login/status` + `/login/by_qrcode`，扫码要连网易 |
| 信令 | Engine.IO v4 / Socket.IO 长连到 UU 云，房间号、`ice_id` 全部由云下发 |
| NAT 穿透 | 云下发的 ICE / TURN 服务器 |
| 远程协助 | 即使不登录，也要 `POST /api/v1/guest/create` 打网易接口 |
| 设备注册 | `POST /api/v1/device/windows/init`，把自己伪装成 Windows PC（`x-param-plat=1`） |

**结论**：物理隔离网里，它编译得出来，但连不上任何东西。所以必须自建信令 + 自建中继，
等于重做一个远控产品。可以复用的只有「怎么做」：X11 采集思路、输入注入、FFmpeg 编码参数、
以及它在文件传输/终端/端口映射上踩过的坑。

## 2. 已知的参考项目踩坑清单（直接吸收，不要重踩）

从它的 `AGENTS.md` / `README.md` 提炼，逐条都是我们设计时要规避的：

1. **WebRTC 在纯内网是纯负担**。它的复杂度全来自 NAT 穿透、SDP 协商、DTLS/SRTP、ICE。
   内网延迟 <1ms、无丢包、无 NAT —— 这些机制一个都用不上，还引入了 webrtc-rs 在
   Windows 上支持不佳的风险（我们要做 Windows 控制端）。
   → **决策：不用 WebRTC，自建 TCP 多路复用协议。**
2. **X11 采集用 SHM + DAMAGE 才不卡**。它专门写了 `c_engine/x11_engine.c`（415 行），
   因为朴素 `XGetImage` 全屏轮询会吃满 CPU。
   → 我们第一版用 `ffmpeg -f x11grab`（它自己是外部 FFmpeg 进程路径），因为 ffmpeg 的
   x11grab 内部已用 SHM；后续若延迟不够再上 DAMAGE + 增量编码。
3. **FFmpeg 参数**：`-preset ultrafast -tune zerolatency`，GOP ≈ 0.5s，**不要 all-intra 60fps**
   （它实测会打爆 WiFi/网卡）。静止桌面单靠码率上限不改画质，还得调 CQ。
4. **按键连发**：远控按键在 X11 下会因 X autorepeat 产生连发。它在压缩包键期间关闭全局 autorepeat。
   → 我们同样处理。
5. **交互式 shell 忽略 SIGTERM**：PTY 会话退出时要 HUP/KILL。
6. **明确的 DISPLAY 打不开就是错误**，不能静默回退到 `:0`（会把画面抓到错误的会话）。
7. **锁屏 / 登录界面（greeter）抓不到**：X11 下会被授权拦住，KMS 抓出来是黑屏。
   → 第一版明确不支持 greeter 和锁屏解锁，文档写清楚。
8. **GTK 窗口在锁屏/息屏时停止重绘** → 所以控制端主窗口不要用 GTK，我们用 winit。
9. **麒麟/UKUI 有自研防截图协议**（openKylin 的 wlcom 系），Wayland 会话下可能直接拦屏。
   → 所以优先 X11 会话路径。
10. **SCTP 消息大小坑**（webrtc 假设 64KB）——我们自建协议，帧头显式带长度，没这个坑。

## 3. 目标环境

- **被控端**：银河麒麟桌面操作系统 V10 SP1，x86_64（兆芯 / 海光 / Intel / AMD）。
  - 桌面：UKUI 3.x（`ukui-kwin`），**默认 X11 会话**。V10 老版是 MATE（marco/mutter）。
  - 内核/glibc：V10 SP1 属 Debian/Ubuntu 血统，glibc 较新，x86_64 二进制可直接跑。
- **控制端**：麒麟 V10 SP1 x86_64（同样 X11） + Windows 10/11 x86_64。
- **网络**：完全物理隔离，纯二层/三层内网，无 NAT，无公网 DNS。
- **合规**：无硬性要求，开源组件可用。

## 4. 总体架构

```
                        ┌──────────────────────────────┐
                        │  uulan-server（中继/信令）    │
                        │  · 设备注册表 (device_id)      │
                        │  · 在线状态 / 配对             │
                        │  · 字节级盲转发（看不懂内容）   │
                        └───────┬──────────────┬───────┘
                                │              │
                  注册/保活(出) │              │ 查找/配对(出)
                                │              │
                     ┌──────────┴───┐    ┌─────┴────────────┐
                     │ uulan-host   │    │ uulan-client     │
                     │ (麒麟 被控)   │    │ (麒麟 / Windows) │
                     │ ·X11 采集     │    │ ·winit 窗口       │
                     │ ·FFmpeg 编码  │    │ ·FFmpeg 解码      │
                     │ ·XTest 注入   │    │ ·键鼠上报         │
                     │ ·PTY / 文件    │    │ ·工具面板         │
                     │ ·端口代理     │    │                  │
                     └──────────────┘    └──────────────────┘
```

三层连接，全部由 **被控端和控制端主动拨出**（不需要任何入站端口，防火墙友好）：

| 连接 | 承载 | 为什么单独开 |
|---|---|---|
| `control` | 输入事件、剪贴板、终端 I/O、各种 RPC（JSON 帧） | 小而密，必须低延迟，不能被大包堵住 |
| `video` | H.264 Annex-B 访问单元 + 时间戳 | 高带宽、可丢弃；拥塞时只影响画面 |
| `bulk` | 文件传输、端口映射的数据 | 突发大流量，绝不能和输入抢 TCP 队列 |

三条连接走同一个 `session_token` 配对，server 端只做字节管道（`tokio::io::copy_bidirectional`）。

**直连模式**（可选）：`uulan-client --direct <host>:<port>` 直接连被控端，不经过 server。
适合只有两台机器、或 server 不方便部署的场景。

## 5. 协议设计（`crates/uulan-protocol`）

### 5.1 帧格式

所有连接统一用长度前缀帧：

```
+--------+--------+---------------------------+
| u32 BE | u8     | payload                   |
| length | kind   |                           |
+--------+--------+---------------------------+
```

- `length`：payload 字节数，上限 8 MiB（防 OOM）。
- `kind`：见下表。

| kind | 名称 | 方向 | 说明 |
|---|---|---|---|
| 0x01 | `Json` | 双向 | UTF-8 JSON 控制消息。**所有控制类消息都走它**，靠 payload 里的 `type` 区分（含画面参数 `video_cfg`） |
| 0x02 | `VideoAu` | host→client | 一个 H.264 访问单元 + 8 字节小端时间戳(µs) |
| ~~0x03~~ | ~~`VideoCfg`~~ | — | **已废除**。见下方说明 |
| 0x04 | `Blob` | 双向 | 文件/端口映射的原始字节 |
| 0x05 | `Ping` / 0x06 `Pong` | 双向 | 心跳 + RTT 测量 |
| 0x07 | `Bye` | 双向 | 主动断开原因 |

> **为什么废除 0x03**：曾经「画面参数变化」既有一个 0x03 帧型，又被实现成 Json 帧里的
> `H2C::VideoCfg`。被控端走的是后者，控制端等的是前者 —— 两边对同一件事有两套表达，
> 结果控制端静默忽略那一帧、一直等到视频通道 EOF，表现为「连不上画面」。
> 现在同一件事只有一种线上表达：**控制类消息一律 Json**。0x03 保留不分配，
> 避免以后有人复用造成同样的错位。

### 5.2 控制消息（JSON，`type` 字段区分）

握手：
```
C→H  {"type":"hello","client":{"os":"windows|kylin","version":".."},"caps":["clipboard","files","terminal","portmap","audio"]}
H→C  {"type":"hello_ok","host":{"device_id":"..","os":"kylin","name":"..","screen":{"w":1920,"h":1080},"caps":[...]}}
C→H  {"type":"auth","password":".."}           # 8-16位，或一次性临时码
H→C  {"type":"auth_ok"} | {"type":"auth_fail","reason":"bad_password"|"locked","retry_after_ms":N}
```

画面设置（对应参考项目 CaptureSettingRequest 的语义，但用我们自己的 JSON）：
```
C→H  {"type":"video_set","fps":30,"max_bitrate_kbps":3000,"quality":"auto|fast|high|hd|bluray","scale":1.0}
H→C  {"type":"video_cfg","w":1920,"h":1080,"codec":"h264","fps":30,"encoder":"libx264|h264_nvenc|h264_vaapi"}
```
`scale` 只作用于编码器缩放，**不改被控端显示器分辨率**（参考项目同款行为）。

输入（走 control 连接）：
```
C→H  {"type":"mouse","action":"move_abs","x":0.512,"y":0.340}     # 归一化 0..1，跨分辨率安全
C→H  {"type":"mouse","action":"button","button":"left","down":true}
C→H  {"type":"mouse","action":"wheel","dx":0,"dy":-120}
C→H  {"type":"key","vk":65,"down":true,"repeat":false}            # Windows VK 码，host 侧映射到 keysym
```
**为什么用 Windows VK 而不是 keysym**：控制端（含麒麟端）只需要一张 VK 表，host 侧做
VK→keysym→keycode 映射；这样两端不会各自实现一套键位逻辑（参考项目也是这个思路）。

剪贴板：
```
双向  {"type":"clip","text":"..."}        # 文本；只同步文本，不做图片（第一版）
```

文件传输（控制类消息走 control，数据走 bulk）：
```
C→H  {"type":"fs","op":"list","id":1,"path":"~/"}
C→H  {"type":"fs","op":"get","id":2,"path":"~/a.txt"}
C→H  {"type":"fs","op":"put","id":3,"path":"~/b.txt","size":12345}
C→H  {"type":"fs","op":"mkdir"|"remove"|"rename","id":4,...}
H→C  {"type":"fs_result","id":1,"ok":true,"entries":[{"name":"a.txt","dir":false,"size":123,"mtime":1700000000}]}
```
根目录显示成 `:/`（此电脑），家目录 `~/`。大文件用 `bulk` 连接流式传输，带 sha256 校验。

终端：
```
C→H  {"type":"term","op":"open","id":1,"cols":120,"rows":30,"shell":"/bin/bash"}
C→H  {"type":"term","op":"input","id":1,"data":"ls\r"}     # 控制帧，base64 或直接 UTF-8
C→H  {"type":"term","op":"resize","id":1,"cols":140,"rows":40}
C→H  {"type":"term","op":"close","id":1}
H→C  {"type":"term_out","id":1,"data":"<base64>"}          # 输出可能含任意字节 → base64
H→C  {"type":"term_exit","id":1,"code":0}
```
PTY 用 `portable-pty`。关闭时先 SIGHUP 再 SIGKILL（参考项目踩过的坑）。

端口映射：
```
C→H  {"type":"portmap","op":"open","id":1,"target_host":"127.0.0.1","target_port":22}
H→C  {"type":"portmap_ok","id":1} | {"type":"portmap_err","id":1,"error":"refused|denied"}
```
然后 bulk 连接上以 `id` 区分流，双向裸字节。host 侧有目标地址白名单（默认只允许 `127.0.0.1` 与
本机所在网段，可用配置放开）。控制端只监听 `127.0.0.1:<local_port>`。

系统信息 / 动作：
```
H→C  {"type":"metrics","cpu":12.3,"mem_used":...,"mem_total":...,"net_rx":...,"net_tx":...}   # 2 秒一次
C→H  {"type":"power","action":"lock"|"reboot"|"shutdown"|"logout"}
```

### 5.3 安全模型

**Server 是「看不懂内容的盲中继」**，不持有任何设备凭据。访问控制由被控端自己把关。

第一阶段（先跑通）：
- 被控端持有 `device_id`（9 位数字，server 首次注册时下发）+ 永久密码（随机 8-16 位，含字母数字）。
- 密码校验在**被控端**做，错 5 次锁 60 秒（指数退避），失败只记日志、不打印密码。
- 支持一次性临时验证码（用一次即失效 / 或按配置每小时刷新）。

第二阶段（叠加，不改协议骨架）：
- 在三条连接建立后、任何业务帧之前，插入一个握手：
  `ClientHello{eph_pub_c, nonce}` → `HostHello{eph_pub_h, nonce}` → 双方
  `K = HKDF-SHA256(X25519(eph_priv, peer_pub), salt=nonce_c||nonce_h)`。
- 之后每条连接的每个帧用 `ChaCha20-Poly1305`、以连接内单调计数器为 nonce 加密。
- 密码证明改为 `HMAC-SHA256(K, "uulan-auth-v1" || password)` 在加密通道内发送。
- 这样 **server 即使被拿下也看不到屏幕内容、键盘、剪贴板、文件**。

> 关键是这套东西是**分层叠加**的：先把明文版本跑通，再在帧编解码外面套一层 AEAD，
> 不需要改任何业务代码。

## 6. 技术选型（为什么是这些）

| 用途 | 选择 | 理由 |
|---|---|---|
| 语言 | Rust | 参考项目同款；单二进制、无运行时、交叉编译友好、Windows/Linux 一套代码 |
| 异步 | tokio | 事实标准 |
| 序列化 | serde / serde_json | 控制消息可读、易调试；量大的是视频帧，不走 JSON |
| X11 | x11rb | 纯 Rust，不需要写 C 引擎、不需要 libX11 头文件；带 `xtest` / `shm` feature |
| 采集+编码 | FFmpeg CLI（`x11grab` → `libx264`） | 零编译依赖；麒麟源里就有 ffmpeg；内部已用 SHM |
| 解码 | FFmpeg CLI（`-f h264 -i - -pix_fmt bgra -f rawvideo -`） | 跨平台一致，不用折腾 FFmpeg 开发库 |
| 窗口 | winit + softbuffer | **纯 Rust，无 C 依赖**，Windows/Linux 通吃；软渲染对 1080p30 够用 |
| PTY | portable-pty | 跨平台 PTY |
| 加密（二期） | x25519-dalek + chacha20poly1305 + hkdf | 少而精 |
| 目录监听（可选） | notify | 桌面环境自适应 |

**明确不用**：
- ❌ WebRTC（内网用不上 NAT 穿透，且 webrtc-rs 在 Windows 上不省心）
- ❌ GTK（锁屏/息屏不重绘；Windows 上还要额外装运行时）
- ❌ Qt（体积大、许可与交叉编译都更麻烦）
- ❌ VNC/RFB 协议（画面效率差、没硬件编码；且我们要的是「像 UU 一样」的体验）
- ❌ x11rb 之外的 C 引擎（第一版不写 C）

## 7. 工程结构

```
uulan/
├─ Cargo.toml                  # workspace
├─ docs/
│  ├─ DESIGN.md                # 本文件
│  ├─ DEPLOY-KYLIN.md          # 麒麟被控端安装/开机自启
│  └─ PROTOCOL.md              # 协议细节（从本文件拆出后维护）
├─ crates/
│  ├─ uulan-protocol/          # 帧编解码、消息类型、AEAD 层（无 IO 依赖，可单独测）
│  ├─ uulan-server/            # 中继/信令，bin: uulan-server
│  ├─ uulan-host/              # 被控端，bin: uulan-host（Linux only）
│  ├─ uulan-client/            # 控制端，bin: uulan-client（Windows + Linux）
│  └─ uulan-x11/               # X11 采集参数、输入注入、剪贴板（host 专用，隔离平台代码）
└─ tools/
   └─ xvfb-test.sh             # 在无头 Linux 上用 Xvfb 起一个假的麒麟桌面来测
```

## 8. 里程碑

状态：✅ 已完成并自测通过 · 🔄 进行中 · ⬜ 未开始

| 阶段 | 状态 | 交付 | 验收标准（实际达成的） |
|---|---|---|---|
| **M1 骨架** | ✅ | workspace 能编译；protocol 单测过；server 能注册设备并配对转发 | `cargo test` 全绿；本机起 server，两端能配对三条通道并盲转发 |
| **M2 能看见** | ✅ | 被控端采集编码、控制端解码；端到端视频通路 | 进程内端到端测试全绿；**并在真麒麟 V10 SP1 上跑通**：Windows 控制端看到 1920x1080 真实桌面，首帧 216ms、零丢帧、画面与实机截图相关系数 0.9995 |
| **M2.5 画好** | 🔄 | 键鼠注入、剪贴板双向、实测帧率上报、分辨率变化重建解码器 | 能远程操作并双向粘文字 |
| **M3 能操作** | ⬜ | X11 输入注入（XTest + uinput）、按键连发处理 | 能在控制端敲命令、拖窗口 |
| **M4 工具齐全** | ⬜ | 文件传输、终端、端口映射 | 三项功能逐个实测通过 |
| **M5 麒麟落地** | ⬜ | 麒麟安装包 + systemd 开机自启 + 部署文档 | 在真麒麟 V10 SP1 上跑通 M2–M4 |
| **M6 加固** | ⬜ | E2E 加密、临时验证码轮换、日志脱敏 | 中继抓包看不到明文内容 |

### M2 实际验收方式（重要）

M2 分两层验收，**两层都做完了**。

**第一层：自动化测试。** `crates/uulan-client/tests/end_to_end.rs` 在同一个进程里
起真 server、真被控端会话、真 FFmpeg 编码与解码，断言帧数、帧尺寸、画面非全黑、
关键帧存在、零丢帧。这一层可在 Windows 开发机上重复跑，用于防回归。

**第二层：真机实测。** 在银河麒麟 V10 SP1（海光 C86 + Glenfly 显卡，X11/UKUI）上
原生编译并运行，由 Windows 控制端连过去取回真实桌面：

- 首帧 216 ms，零丢帧，1920x1080 @ 30fps，约 10 Mbps
- **画面正确性用逐像素比对证明**：控制端存出的帧与麒麟本机 `import -window root`
  的截图，缩到 480x270 后平均绝对误差 1.23/255（0.48%）、相关系数 0.9995 ——
  0.48% 就是 H.264 有损压缩本身的代价，说明送过去的确实是真实桌面

真机上的完整部署过程、实测数字、以及踩到的每个坑（含一个「控制端一连接、
被控端就自杀」的 P0 缺陷）都记在 [`docs/DEPLOY-KYLIN.md`](DEPLOY-KYLIN.md)。

唯一仍未被真实覆盖的是……没有了：`x11grab` 这个输入源已经在真机上跑过。
`lavfi` 合成源仍然保留，用于无 X11 环境下的快速自测。

## 9. 明确不做 / 已知限制（照实写，不吹）

- 不支持系统登录界面（greeter）和锁屏解锁 —— X11 授权模型就是不给，强行要得用 root/KMS，
  在内网里反而是更大的口子。
- 不支持摄像头、麦克风。
- 不支持多显示器切换（第一版只采主屏）。
- 不支持音频（第二版考虑，走 PulseAudio monitor → Opus，独立第 4 条连接）。
- 不支持 Wayland 原生会话的完整桌面 —— 只在 X11 会话下保证。麒麟默认 X11，
  若用户切到 Wayland，host 会明确报错并提示切回 X11，而不是黑屏不报错。
- 不做 HDR、不做 4:4:4 真彩（第一版 4:2:0，局域网码率不紧张，画质够）。

## 10. 与参考项目的差异一览

| 维度 | 参考项目 | 本项目 |
|---|---|---|
| 信令 | 网易云 Socket.IO | 自建 `uulan-server`，纯内网 |
| 传输 | WebRTC（ICE/DTLS/SRTP/SCTP） | 自建 TCP 三通道 + 自定义帧 |
| 登录 | 网易账号扫码 | 无账号，device_id + 本机密码/临时码 |
| 中继 | 网易 TURN | 自建盲转发，可直连 |
| UI | GTK3（受锁屏影响） | winit + softbuffer（纯 Rust） |
| 平台 | Linux 被控 + 实验性 Linux 控制 | 麒麟被控 + 麒麟/Windows 控制 |
| 许可 | GPL-3.0 | 待定（若复用其代码片段则必须 GPL-3.0） |