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
│  ├─ uulan-clip/              # 剪贴板：平台无关的去重状态机 + Win32/X11 后端
│  └─ uulan-x11/               # X11 采集参数、输入注入、剪贴板（host 专用，隔离平台代码）
└─ tools/
   ├─ xvfb-test.sh             # 在无头 Linux 上用 Xvfb 起一个假的麒麟桌面来测
   └─ smoke-local.sh           # 本机冒烟（含 CLI、配置落盘）
└─ scripts/
   ├─ verify-window-e2e.sh     # 拷出工作区跑全链路 + 断言窗口真的呈现了帧
   └─ verify-clipboard-e2e.sh  # 真实系统剪贴板改一次，断言它经协议到了对端
```

## 8. 里程碑

状态：✅ 已完成并自测通过 · 🔄 进行中 · ⬜ 未开始

| 阶段 | 状态 | 交付 | 验收标准（实际达成的） |
|---|---|---|---|
| **M1 骨架** | ✅ | workspace 能编译；protocol 单测过；server 能注册设备并配对转发 | `cargo test` 全绿；本机起 server，两端能配对三条通道并盲转发 |
| **M2 能看见** | ✅ | 被控端采集编码、控制端解码；端到端视频通路 | 进程内端到端测试全绿；**并在真麒麟 V10 SP1 上跑通**：Windows 控制端看到 1920x1080 真实桌面，首帧 216ms、零丢帧、画面与实机截图相关系数 0.9995 |
| **M2.5 画好** | 🔄 | 窗口 ✅；**键鼠注入 ✅（两端已接，本机全链路验证）**；**实测帧率上报 ✅**；**剪贴板文本双向 ✅（本机全链路 + 真 Win32 剪贴板实测；X11 后端待真机验）**；分辨率变化重建解码器 ⬜ | 能远程操作并双向粘文字（键鼠部分已单测+全链路覆盖，真机注入待验；剪贴板文本双向已在本机实测通过） |
| **M3 能操作** | 🔄 | XTest 注入 + 按键连发处理**已实现**（Linux 目标编译通过，逻辑单测覆盖）；uinput 兜底未做 | 能在控制端敲命令、拖窗口 —— **必须在麒麟 X11 上实测**，本机无法验证 |
| **M4 工具齐全** | ⬜ | 文件传输、终端、端口映射 | 三项功能逐个实测通过 |
| **M5 麒麟落地** | ⬜ | 麒麟安装包 + systemd 开机自启 + 部署文档 | 在真麒麟 V10 SP1 上跑通 M2–M4 |
| **M6 加固** | ⬜ | E2E 加密、临时验证码轮换、日志脱敏 | 中继抓包看不到明文内容 |

### M2.5 进行中：控制端窗口（已完成）

控制端不再是 headless —— **默认开窗口**显示画面，加 `--headless` 回到命令行模式
（原来的统计输出格式一字未变，端到端测试仍走那条路）。

实现都在 `crates/uulan-client/src/window.rs`，四个要点：

- **winit 的事件循环必须在主线程。** `#[tokio::main]` 的 async 主体本来就在主线程，
  所以直接在里头同步调用；**不要**为了「把主线程留给窗口」去 spawn 线程 ——
  winit 0.30 会直接 panic（*Initializing the event loop outside of the main thread…*）。
  画面不受影响：收流、喂解码器、取帧都是 tokio 任务，跑在多线程运行时的 worker 上。
- **用 softbuffer，不用 GTK / OpenGL。** 参考项目在 GTK 上栽过：窗口在锁屏/息屏时停止重绘。
- **帧走 `watch` 通道而不是队列。** 远控要的是最新画面；追不上时应丢旧帧，而不是把延迟越堆越高。
- **缩放取整数倍 + 最近邻，且从不缩小。** 远程桌面上要看清的是文字，缩过的字比重采样锯齿更难受。

实测（本机合成源 1280x720@30，**release 二进制**，14 秒会话）：

```
窗口已就绪
painted=653             # 10 秒里窗口真的呈现了 653 帧
收到 AU 4154 / 关键帧 277 / 解出帧 1999 / 丢帧 1482 / 首帧 56ms
```

`--seconds N` 到点自动关窗，便于脚本化验证。

### M2.5 进行中：键鼠注入（两端已接上 · 真机待验）

协议侧的消息早就在 `C2H` 里躺着，这一轮把它们真正接到了两条主循环上：

```
控制端窗口事件 (window.rs)
  → uulan_client::input：坐标归一化 / 滚轮折算 / 物理键查表 / 输入法分流
  → uulan_client::control::ControlLink（一半任务写、一半任务读）
  → C2H 帧 → 中继 → 被控端 control_loop (session.rs)
  → uulan_host::input::Mapper（纯状态机）→ Inject → InputSink
  → Linux: XTest (x11.rs)；其他平台: NullSink 如实报 InputError
```

#### 几个不是「照着协议填一遍」的地方

- **点击位置和画面几何共用一份算法**。`input::fit_rect` 同时被 `window::blit`（画）
  和光标换算（点）调用。两边各算一遍就会在留黑边时点偏 —— 远控里这是最招骂的一类 bug。
  黑边里的点击夹到边上，不丢弃。
- **滚轮单位是 120**。`LineDelta` 直接乘 120；触摸板给的 `PixelDelta` 先攒够一行（40px）
  再发，否则一次轻扫会发出几十条滚动消息。不足一格一律丢弃。
- **物理键与字符二选一**。查表（`VK_TABLE`）把 winit 的 `KeyCode` 翻成 Windows VK；
  输入法已上屏的非 ASCII 文本走 `KeyChar`，**不再发那一下物理键** —— 否则会打出
  「拼音字母 + 汉字」两份。按着 Ctrl/Alt/Win 时一律按快捷键处理（`Ctrl+C` 不能变成打 c）。
- **连发不叠加**。被控端在会话开始时 `XAutoRepeatOff`（退出时恢复），连发只来自控制端
  的 `Key{repeat:true}`：收到一次就再发一次「按下」、**不配松开**。若两端都连发，速率会翻倍。
- **绝不把 VK 当 keycode**。未知 VK 直接不注入。这条有跨 crate 测试钉住：
  `VK_TABLE` 里每个 VK 都必须被 `uulan-protocol::vk` 认识 —— 写完第一次跑就抓出一个
  `VK_OEM_102 (0xe2)` 没映射，表现会是「这个键按下去没反应」。
- **收尾要松开所有键**。按「后按先松」的顺序释放（`Vec` 保序，不是 `BTreeSet` 的字典序），
  修饰键最后起来。网络断了、客户端崩了都不能在被控端留下一个「一直按住 Ctrl」的键盘。
- **注入失败要能看见**。`NullSink`（非 Linux / 合成画面）和 XTest 的失败都会回
  `InputError`，但**按 2 秒节流**：注入不了的时候每个鼠标移动都回一条会把控制通道刷爆。
- **中文靠 Unicode KeySym**。`0x01000000 + 码点`，布局里没有，由后端临时占一个
  全空的 keycode 行注册，退出时按原样写回（改的是 X 的全局键盘映射）。
- **一批指令只 flush 一次**。鼠标 100+ 次/秒，每条都 flush 会把往返打成瓶颈。

#### 实测（本机，可重复）

```
cargo test --workspace        181 passed / 0 failed   （原基线 146，新增 35）
cargo build --release         0 警告
cargo check -p uulan-host --target x86_64-unknown-linux-gnu   0 警告（X11 后端在 Linux 目标下编译通过）
```

新增的两条**两端全链路**测试在 `crates/uulan-client/tests/input_path.rs`：真 server、
真被控端会话、真 wire 报文，被控端只把 XTest 换成「记录型 sink」，逐条比对注入指令：

```
keys_and_mouse_reach_the_host_injector
  MoveAbs{160,120} / Button{1,down} / Button{1,up} / Key{0x61,down} / Key{0x61,up}
  / Wheel{4,1} / Key{0x0100_4e2d,down} / Key{0x0100_4e2d,up}   ✅ 逐条相等
unknown_vk_and_sub_notch_wheel_inject_nothing
  未知 VK、不足一格的滚轮、没按过的松开 → 0 条注入            ✅
```

真实窗口跑一遍（合成源被控端，10 秒，release）：

```
窗口已就绪
窗口已关闭 painted=657 input_sent=1
输入上报：移动 1 / 按键 0 / 文本 0 / 点击 0 / 滚轮 0（未送出 0；被控端拒绝 1）
收到 AU 4281 / 关键帧 286 / 解出 2064 / 丢帧 1519 / 首帧 53ms
```

那句「被控端拒绝 1」是**故意要看到的**：窗口起来时光标落在里面，产生一次
`CursorMoved` → `MouseMoveAbs` → 被控端收到 → 合成源没有可注入的桌面 → 回 `InputError`。
这一来一回恰好证明整条控制通道（含回报方向）是通的。

#### 实测帧率上报（已完成）

被控端自己数 AU（`FpsMeter`，1 秒窗口），每 2 秒用 `VideoCfg.actual_fps` 推一次；
控制端存进 `WatchStats.host_fps_x10`（原子量存整数），窗口标题显示「被控端 N fps」。
两个刻意的决定：

- **不数控制端自己的解码帧**：丢帧/跳帧会让它偏离真实编码帧率；用户想知道的是「对面产了多少帧」。
- **编码器停了要归零**：超过 3 个窗口没有新帧就报 0。一直显示「30 fps」而画面冻住，比不显示更坏。

实测（合成源，10 秒）：控制端打印 `被控端实测 433 fps`。
**这个数字是真的，不是 bug**：`lavfi` 合成源不按实时限速（没加 `-re`），编码器会尽可能快地出帧
（同一轮里「收到 AU 4300 / 10s」也是同一个原因）。真机上 `x11grab` 是实时的，才会看到 30 上下。
验证脚本因此只断言「有上报」，不断言数值。

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机 `cargo test` 可复现） | 必须在真麒麟 X11 上验 |
|---|---|
| 键位表覆盖 + 与协议 VK 表一致 | XTest 真的能动指针、能敲键 |
| 归一化坐标↔像素（含黑边、NaN、越界） | 多屏 / 缩放下的坐标一致性 |
| 滚轮 120 单位、像素累积成行 | 滚轮按钮号在 UKUI 应用里的实际效果 |
| 按下/松开配对、连发不叠加、收尾全释放 | `XAutoRepeatOff` 是否真的只由控制端驱动连发、退出能否恢复 |
| 字符 + Shift 包裹、CJK Unicode KeySym | 动态注册的 keycode 能否在 UKUI 里打出中文（输入法可能截走 keysym） |
| 未知 VK 不注入、子格滚轮不注入 | 构建机需要 `libx11-dev` / `libxtst-dev`（链接期依赖） |

**没有假装验过的**：XTest 的每一个 syscall 在本机一行都没执行过。上面「已验」的
是「被控端打算注入什么」，不是「X 真的照做了」。

### M2.5 进行中：剪贴板双向（文本已完成）

独立 crate `crates/uulan-clip`，沿用键鼠那套分层：

```
ClipSync（纯状态机：该不该发 / 该不该写 / 该不该闭嘴）        ← 任何平台都能 cargo test
  ├─ Win32 后端（控制端在 Windows 上）：user32/kernel32 直接 FFI，CF_UNICODETEXT
  └─ X11 后端（麒麟被控端 + Linux 控制端）：CLIPBOARD/PRIMARY selection
```

两端各一条 500ms 轮询（[`uulan_clip::POLL_INTERVAL`]），走的是**已有的 control 连接**，
协议一行没改 —— `C2H::Clip` / `H2C::Clip` 早就在 `msg.rs` 里躺着。

#### 几个不是「照协议填一遍」的地方

- **防回声靠「已知内容指纹」，不是靠标志位**。远端内容写进本机剪贴板后会被自己的轮询读到，
  若原样回发就成无限循环。做法是记住最近一次已知内容（不论来源），哈希相同就不再发。
  指纹**先做换行归一化**（丢掉 `\r`）：Windows 把 `\n` 存成 `\r\n`，不归一化就会
  「A 发 B、B 又发 A」地刷一晚上。这个变换幂等，所以回路一定收敛。
- **绝不传播「剪贴板被清空」**。X11 读剪贴板是一次跨进程请求，所有者可能不回应 ——
  超时读出来是 `None`。若把 `None` 当成「内容变成空」，一次超时就会把对面剪贴板擦掉。
  所以只同步非空文本，`None` 一律当「没读到」（`ClipSync::outbound(None) == None`，有测试钉住）。
- **会话建立那一刻的内容不外推**。两端都在会话开头读一次基线并记成「已知」：
  否则一连上就把对面正在用的剪贴板换成「我这边的历史内容」。代价是连接后几百毫秒内
  复制的内容不会同步（下次变化照常），这条写进了测试 `session_start_does_not_push_pre_existing_clipboard`。
- **超限整条丢弃，不截断**。单条上限 192 KiB（X11 单个请求约 256 KiB，第一版不做 INCR 发送）。
  截断的剪贴板比不同步更坑人，所以选择「不发 + 记数」。
- **X11 写入必须一直有人守着**。`set_text` 之后，别的程序来取数据是**我们**要回答的 ——
  所以 X11 后端专门起一个 service 线程（独占第二条 `Display*` 连接、一个 1x1 隐藏窗口当
  owner）回答 `SelectionRequest`；`CLIPBOARD` 与 `PRIMARY` 一起拿下（只同步 CLIPBOARD 会出现
  「Ctrl+V 可以、中键不行」）。别的程序把 selection 抢走时收到 `SelectionClear`，
  必须立刻丢掉手里的 `own_text`，否则 `get_text` 会永远返回旧内容。
- **读取要处理 INCR**。大内容时所有者分段送：先 `XDeleteProperty` 表示「收到了，继续」，
  再按 `PropertyNotify` 一段段收，收到空段为止，整体 1.5 秒超时。
- **控制通道拆成读/写两半**。原来 `control_loop` 拿着 `&mut TcpStream` 死等一帧；
  现在要「读帧」和「每 500ms 轮询剪贴板」并行，共用一个 `&mut` 既借不动也会互相阻塞，
  所以 `into_split()` 后 `select!`。
- **能力如实声明**：`HelloOk.caps` 只在后端真的可用时才带 `Clipboard`。
  没桌面（`UU_DESKTOP_BACKEND=none`）时后端是 `NoClipboard`，`set_text` **报错而不是装作成功**。

#### 实测（本机，可重复）

```
cargo test --workspace                                         200 passed / 0 failed  （基线 187，+13）
cargo build --release                                          0 警告
cargo check -p uulan-host -p uulan-clip --target x86_64-unknown-linux-gnu   0 警告
```

两端全链路（`crates/uulan-client/tests/clipboard_path.rs`，真 server + 真会话 + 真 wire，
只把「系统剪贴板」换成内存版）：

```
clipboard_syncs_both_ways_without_echo
  中文/换行/emoji 双向各一次都到达；再等 1.6 秒，两端剪贴板各自仍只被对面写过 1 次（无回声）
session_start_does_not_push_pre_existing_clipboard
  连上之前的内容两边都没被覆盖；之后用户新复制的仍照常同步
```

真实系统剪贴板（`scripts/verify-clipboard-e2e.sh`，本机同时当被控端与控制端，
会话中途用 PowerShell 改一次剪贴板，跑完还原原内容）：

```
被控端：backend="win32" available=true
被控端统计：sent=1 recv=0 errors=0 skipped_same=23
控制端：剪贴板同步：发送 0 / 接收 1（本机写入失败 0）
```

即「真实剪贴板变化 → 协议 → 对端真实写回」这条完整链路是通的（发送数为 0 是因为
两端共享同一个剪贴板，被控端先看到了这次变化）。窗口同时跑着 `painted=1350`。

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机 `cargo test` 可复现） | 必须在真麒麟 X11 上验 |
|---|---|
| 去重、防回声（含 Windows CRLF 改写）、空内容不发 | `XConvertSelection` / `SelectionNotify` 的真实往返 |
| 超限整条丢弃（不截断）、基线不外推 | 大内容的 INCR 分段接收 |
| 「读不到 ≠ 被清空」 | 别的程序（浏览器/编辑器）粘贴我们写的内容 |
| Win32 后端真实读写（UTF-16、中文、emoji、写后还原） | `PRIMARY` 中键粘贴在 UKUI 下的实际行为 |
| 两端全链路两个方向（真 wire） | 所有者进程退出后剪贴板内容失效，用户观感如何 |

**没有假装验过的**：`x11.rs` 里每一个 Xlib 调用在本机一行都没执行过 ——
它只被 `cargo check --target x86_64-unknown-linux-gnu` 类型检查过。

#### 本机开发环境的两个坑（会重复踩）

1. **链接 `windows-sys` 需要 `dlltool.exe`**，它不在 PATH 上。在
   `%LOCALAPPDATA%\Microsoft\WinGet\Packages\BrechtSanders.WinLibs.POSIX.UCRT_*\mingw64\bin`。
   不加就报 `error calling dlltool 'dlltool.exe': program not found`。
2. **位于 `D:/default-workspace/` 下的 exe 写不了任何文件**（换到 `D:/` 下立刻正常）。
   这是目录级执行策略，不是代码问题，但会让 `cargo test` 里所有涉及写盘的断言失真。
   诊断工具：`cargo run -q -p uulan-host --example fs_probe`，把它拷到 `D:/` 下再跑一遍对照。

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
- **键鼠只在 X11 下工作**。注入走 XTest，需要能打开被控端用户的那个 X 显示
  （`$DISPLAY` 或配置里的显示号）；Wayland 会话下没有 XTest 语义。
- **输入法只处理「已上屏的字符」**。拼音的中间态（还没选词的字母）会作为普通物理键
  发给被控端 —— 中文输入时对面会闪一下字母。要彻底解决得接 IME 的 preedit 协议，
  第一版不做。
- **被控端构建需要 `libx11-dev` / `libxtst-dev`**（链接期）。只装运行时库
  `libXtst.so.6` 能跑但编不过；这条写进了 [`docs/DEPLOY-KYLIN.md`](DEPLOY-KYLIN.md)。
- 注入失败不会让会话中断：控制端只在状态里看到「被控端拒绝 N 条」，不会弹窗。
- **剪贴板只同步文本**（第一版）。图片、文件列表（`text/uri-list`）不同步；
  单条上限 192 KiB，超过就整条不同步（不截断）；「清空剪贴板」不传播
  （X11 上读不到不等于内容变空，按「没读到」处理）。
- **被控端进程退出后，它写进 X11 剪贴板的内容随之失效** —— 这是 X11 的固有行为
  （selection 归所有者进程所有），不是缺陷；控制端退出同理，Windows 一侧会保留最后一次写的内容。

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