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
| 0x04 | `Blob` | 双向 | 文件/端口映射的原始字节（帧内再带 `id|flags` 头，见 5.2） |
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
C→H  {"type":"fs","op":"mkdir"|"remove"|"rename"|"hash","id":4,...}
H→C  {"type":"fs_result","id":1,"ok":true,"entries":[{"name":"a.txt","dir":false,"size":123,"mtime":1700000000}]}
H→C  {"type":"fs_stream","id":3,"stage":"begin"|"done"|"failed","total":12345,"done":0}
```
根目录显示成 `:/`（此电脑），家目录 `~/`。大文件用 `bulk` 连接流式传输，带 sha256 校验。

**一次传输的时序（`FsResult` 只有一条，且永远最后发）：**

```
get:  C→H fs(get)                H→C fs_stream{begin,total}     ← 之后才开始来数据
                                 H→C Blob(id) … Blob(id,END)
                                 H→C fs_stream{done} + fs_result{ok,hash}

put:  C→H fs(put,size)           H→C fs_stream{begin}           ← 当「放行信号」用
      C→H Blob(id) … Blob(id,END)
                                 H→C fs_stream{done} + fs_result{ok,hash}
```

`begin` 兼作上传放行：客户端收到它才开始灌数据（此时被控端已校验过路径、建好临时文件）。
把「完成」放进数据流本身（`Blob` 帧的 END 标志）而不是只靠控制通道，是因为
control 与 bulk 是两条独立 TCP，谁先到没有保证 —— 见 5.1 的 `Blob` 帧头。

**bulk 帧头**：`u32 LE id | u8 flags | data`，`flags & 0x01` 即 END。同一个 id 上 END 之后
不应再有帧；换个 id 就是另一条流（端口映射会用同一种头）。

终端：
```
C→H  {"type":"term","op":"open","id":1,"cols":120,"rows":30,"shell":"/bin/bash"}
C→H  {"type":"term","op":"input","id":1,"data":"ls\r"}     # 控制帧，base64 或直接 UTF-8
C→H  {"type":"term","op":"resize","id":1,"cols":140,"rows":40}
C→H  {"type":"term","op":"close","id":1}
H→C  {"type":"term_out","id":1,"data":"<base64>"}          # 输出可能含任意字节 → base64
H→C  {"type":"term_exit","id":1,"code":0}                     # 被信号杀时按 128+signo 报（HUP→129、KILL→137）
H→C  {"type":"term_error","id":1,"error":"..."}               # 开不出 shell / id 不认识 / 输入不是合法 base64
```
PTY 用 `portable-pty`。关闭时先 SIGHUP 再 SIGKILL（参考项目踩过的坑）。

端口映射：
```
C→H  {"type":"portmap","op":"open","id":1,"target_host":"127.0.0.1","target_port":22}
H→C  {"type":"portmap_ok","id":1} | {"type":"portmap_err","id":1,"error":"refused|denied"}
```
`close` 用来主动拆一条流（会话收尾时被控端也会全部拆掉）。
然后 bulk 连接上以 `id` 区分流，双向裸字节（bulk 帧头 `id|flags`，`END` 表示该方向结束）。

**目标白名单在被控端判定**，默认只允许本机回环（`127.0.0.0/8`、`::1`）——
端口映射天然是内网跳板，放开到网段要显式写配置 `portmap_allow`（`10.0.0.0/8`、`192.168.1.5`、`*`）。
控制端监听地址由 `--forward [绑定地址:]本地端口:目标主机:目标端口` 决定，
默认绑 `127.0.0.1`（不写绑定地址时）。

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
| PTY | portable-pty 0.9 | 跨平台 PTY（unix `/dev/ptmx`、Windows ConPTY）；硬杀另用 libc（仅 unix） |
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
│  ├─ uulan-bulk/              # bulk 连接的多路复用（读分派 / 写串行 / 每流一个任务）
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
| **M2.5 画好** | 🔄 | 窗口 ✅；**键鼠注入 ✅（两端已接，本机全链路验证）**；**实测帧率上报 ✅**；**剪贴板文本双向 ✅（本机全链路 + 真 Win32 剪贴板实测；X11 后端待真机验）**；**分辨率变化重建解码器 ✅（两端都做了，本机全链路实测）** | 能远程操作并双向粘文字、被控端换分辨率后画面自动跟上（键鼠/剪贴板/分辨率的真机行为待验；本机均已实测通过） |
| **M3 能操作** | 🔄 | XTest 注入 + 按键连发处理**已实现**（Linux 目标编译通过，逻辑单测覆盖）；uinput 兜底未做 | 能在控制端敲命令、拖窗口 —— **必须在麒麟 X11 上实测**，本机无法验证 |
| **M4 工具齐全** | 🔄 | **文件传输 ✅**（列/取/送/建/删/改名/哈希 + 路径沙箱 + sha256，真二进制实测 6 MiB 往返一致）；**端口映射 ✅**（bulk 多路复用 + `--forward`，真二进制实测 256 KiB 单流 + 两条并发流逐字节一致）；**终端（PTY）✅ 本机全链路**（15 项状态机 + 4 项真 TCP 端到端，PTY 后端可注入） | 三项都用真 server + 真被控端 + 真客户端链路验过。终端唯一没验的是**真 PTY 本身**（termios / SIGHUP→SIGKILL / 真 shell 交互）—— 本机没有可用的 PTY 路径，必须上麒麟 |
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

### M2.5 进行中：分辨率变化重建解码器（已完成）

被控端换了分辨率（`xrandr` 改模式、插拔外接屏、虚拟机窗口缩放），画面必须自己跟上 ——
两边都要改，而且两边是**两个不同的物理事实**：

| 端 | 问题 | 做法 |
|---|---|---|
| 被控端 | x11grab 的抓取区域尺寸在 ffmpeg 启动时就定死了 | 1 秒一次探针；尺寸变了就停源、按新尺寸重开、发新的 `VideoCfg` |
| 控制端 | ffmpeg 解码的 rawvideo 每帧字节数也定死了 | 收到新尺寸的 `VideoCfg` → 杀旧解码器、起新解码器、**socket 不断**接着收 |
| 被控端 | 注入的坐标按屏幕尺寸换算 | 同一个信号同步给 `Mapper`，否则「画面变了、点哪儿都不对」 |

#### 控制端：按「代」重建解码器（`crates/uulan-client/src/watch.rs`）

```
supervise（总管，一个任务）
  loop {
    解码器(本代尺寸) ← feed ← 有界队列 ← pump(socket 读半)
                            └→ watch(最新一帧)
    pump 报「分辨率变了」→ kill 旧解码器 → 下一代
  }
```

- **socket 不能断**：`TcpStream::into_split()` 的两个半端在换代之间靠 pump 任务的
  **返回值**交接（`JoinHandle<T>` 的返回值就是这条通道）。用 `Arc<Mutex<TcpStream>>`
  也能编过，但会把「一个读、一个写」硬生生串行化。
- **只报帧率的 `VideoCfg` 绝不能触发重建**：那是每 2 秒一次的常规上报，重建一次就要
  重开 ffmpeg、丢掉当前 GOP，等于每 2 秒卡一下。`apply_video_cfg` 的返回值只表达「画幅变了」。
- **换代时旧代整代丢掉**：`feed`/`decoded` 两个任务 abort、解码进程 kill。
  留着旧解码器只会让它把新码流按旧尺寸读，画出斜条纹而且永不自愈。
  代价是换代边界上可能丢几帧 —— 远控里这比花屏好得多。
- **收尾不抢跑**：通道正常关闭时先让 ffmpeg 把已解出的帧冲刷完（2 秒超时）再 kill，
  否则最后一帧永远出不来（这个坑在 M2 就踩过：`喂入 20 个 AU、解出 0 帧`）。
- 统计里新增 `resizes` / `cfg_w` / `cfg_h`：`--headless` 的统计会打
  「解码器重建 N 次」「当前画幅 WxH」，窗口模式打在关窗那行。

#### 被控端：探针 + 重开编码源（`crates/uulan-host/src/session.rs`）

- **探针是可注入的**（`ScreenProbe = Arc<dyn Fn() -> Option<Screen> + Send + Sync>`）：
  生产环境是 `xdpyinfo`（放进 `spawn_blocking`，不占 tokio worker），测试塞一个按剧本
  返回尺寸的闭包 —— 本机没有 X11，这是唯一能验这条路的方式。
- **配置里钉死了 `screen_width/screen_height` 就不跟随**：那是用户明确要求「就抓这一块」，
  跟随反而会把他钉的区域改掉。
- **探针失败 = 没变**：一次 `xdpyinfo` 读不到不能把好好的画面重开一遍。
- **两条触发路径**：1 秒一次的常规探测；以及编码源报错时先探一次 —— 分辨率变小时
  x11grab 往往**先**崩（抓取区域越界），等不到下一拍。两条都要求「尺寸真的变了」才重开。
- **合成源跟随尺寸**：`lavfi:testsrc2=size=WxH` 里的 `size=` 会被改写成本次的画幅
  （`capture::with_size`）。不改的话「重启编码源」只改了 `VideoCfg`、画面还是旧尺寸，
  测试就成了自欺欺人。`size=hd720` 这种别名不动。

#### 实测（本机，可重复）

```
cargo test --workspace                                          206 passed / 0 failed（基线 200，+6）
cargo build --release                                           0 警告
cargo check -p uulan-host --target x86_64-unknown-linux-gnu     0 警告
```

控制端单独一代（`crates/uulan-client/tests/resize_path.rs`，假发流器 + 真 ffmpeg 解码）：
先 320x240 一段、再插一条**只报帧率**的 `VideoCfg`、再 480x360 一段。

```
帧：320x240 3 帧 / 480x360 1 帧；重建 1 次；解出 4 帧 / 喂入 2 AU / 丢帧 0
断言：三条 VideoCfg 只重建了一次（帧率上报不触发）；两代帧字节数各自正确；三类画幅之外不出现别的
```

两端全链路（`crates/uulan-client/tests/resize_host.rs`，真 server + 真被控端会话 +
真 lavfi 采集 + 真 x264 + 真协议 + 真解码；替身只有探针与注入 sink）：

```
帧：320x240 3 帧 / 480x360 3 帧；重建 1 次；解出 349 / 喂入 456 AU / 丢帧 4386
断言：换之前只出 320x240；换之后出 480x360；
      换之后 (0.5,0.5) 注入成 MoveAbs{240,180}（坐标按新尺寸换算，不是 160,120）
（丢帧 4386 是 lavfi 不限速跑满 CPU 的结果，真机 x11grab 是 30fps 不会这样）
```

窗口全链路回归（`scripts/verify-window-e2e.sh`，release，8 秒）：

```
窗口已关闭 painted=528 input_sent=1
收到 AU 2611 / 关键帧 175 / 解出帧 1483 / 丢帧 690 / 首帧 78ms / 重建解码器 0 次
```

#### 只能真麒麟验

- 真的 `xrandr` 改分辨率后，1 秒内的探针能不能读到新尺寸（`xdpyinfo` 在 UKUI 下的输出一致性）
- x11grab 在分辨率变小/变大时的真实表现（报错退出？补黑边？只抓左上角？）—— 三种都要试
- 换分辨率那一瞬间，控制端窗口（winit + softbuffer）改尺寸/重绘是否正常，有没有黑屏一拍
- 输入映射器换尺寸后，真实点击落点是否与画面一致（多屏 / 缩放场景尤其要看）

### M4 进行中：文件传输（已完成 · 本机已实测）

新 crate `crates/uulan-fs`，沿用键鼠/剪贴板那套分层：**能在本机钉住的逻辑，绝不留给真机去发现**。

| 层 | 位置 | 本机可测 |
|---|---|---|
| 路径解析（`:/`、`~/`、绝对路径、`..` 不许越界） | `uulan_fs::path` | ✅ 纯函数 |
| 「此电脑」条目（盘符 / 挂载点） | `uulan_fs::mounts` | ✅ 传一段 `/proc/self/mounts` 文本进去测 |
| 文件服务（列/取/送/建/删/改名/哈希 + bulk 传输） | `uulan_fs::server` | ✅ 收发只依赖 `AsyncRead/AsyncWrite`，用 `duplex` 就能测 |
| 控制端 CLI（`--ls/--get/--put/--rm/--mkdir`） | `uulan_client::fsx` | ✅ 真 socket + 真文件 |
| 被控端接线 | `uulan_host::session` 的 `fs_loop` | ✅ 端到端集成测试 |

#### 这一轮里被迫想清楚的几件事

- **控制通道改成了「一根读任务 + 一根写任务」。** 以前 `control_loop` 直接拿
  `&mut rd / &mut wr`，靠一个 `select!` 同时等帧和等剪贴板定时器。加入文件服务后，
  写控制通道的不再只有主循环（传输的中途通知也得写）。而 `read_frame()` **不是
  cancel-safe** 的（两次 `read_exact`），一旦让它和「写一条别人的消息」并列在 `select!`
  里，另一支被选中就会吞掉半个帧头 —— 症状是「偶发协议解析错」，几乎无法复现。
  现在：读帧只在 `read_loop`，写帧只在 `write_loop`，主循环两边都只排队列
  （mpsc 的 `recv/send` 是 cancel-safe 的）。这条对 M4 后面的终端 I/O 是必需的。
- **上传先写 `.uulan-part`，校验过了才 rename。** 直接往目标写，传到一半断线会留下
  「大小对、内容半截」的文件 —— 用户看不出来，比传失败更坏。测试钉住了这条：
  声明 100 字节只发 50 字节 → 目标文件**不存在**、临时文件也不存在。
- **不自动建上级目录。** 静默把文件落进一个本来不存在的目录，用户回头找不到。
  控制端要显式 `--mkdir`（测试删掉了「自动 mkdir -p」这个便利）。
- **删除目录不递归。** 远控里「删错一个目录」不可挽回，非空就报错让人自己进去清。
- **`~/` 永远出不了家目录，绝对路径默认可开关。** `~/../../etc/passwd` 词法归一化后
  越界即拒；绝对路径由 `fs_allow_absolute` 控制（默认允许 —— 远控用户的预期就是能翻全盘，
  这是产品语义不是漏洞）。不用 `canonicalize()`：它会跟随符号链接，而且对「还不存在的
  上传目标」直接失败，而我们要解析的恰恰包括这种路径。
- **一次只跑一条传输。** `fs_loop` 串行处理 `FsOp`；协议里的 id 多路复用留着给端口映射。
  单条 6 MiB 在本机 loopback 上是 0.27 秒，串行的代价远小于并发分派的复杂度。

#### 实测（本机，可重复）

```
cargo test --workspace        256 passed / 0 failed   （上一轮基线 206，新增 50）
cargo build --release         0 警告
cargo check -p uulan-host -p uulan-fs --target x86_64-unknown-linux-gnu   0 警告
bash scripts/verify-files-e2e.sh
```

`verify-files-e2e.sh` 跑的是**真二进制**（拷到 `D:/uulan-bin/fs-*` 下执行，见坑 2）：

```
① --ls :/            2 项（C:\ 、D:\）
② --mkdir            被控端磁盘上真的建出来了
③ --put 6 MiB        real 0m0.267s   sha256 一致（2fb40c0ef380…）  无 .uulan-part 残留
④ --get               real 0m0.219s   下载内容与源文件一致
⑤ --ls 远端目录      列表里能看到 upload.bin
⑥ --rm               远端文件已删除
OK：文件传输全链路通过（6 MiB，sha256 往返一致）
```

进程内集成测试 `crates/uulan-client/tests/fs_path.rs` 另有三条（真 server + 真被控端会话 +
真 wire）：8 MiB 上传/下载/`Hash` 三方 sha256 一致；`~/../..` 被拒；**上传中途断开 bulk，
目标文件与临时文件都不留**。

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机可复现） | 只能在真麒麟上验 |
|---|---|
| 路径解析与越界拒绝（含 Windows 风格 `~\a\b`） | `/proc/self/mounts` 的真实内容、外接盘是否出现在「此电脑」里 |
| 上传/下载逐字节 + sha256、中断不留半截 | 往 `/home`、U 盘、NTFS 挂载点上真的读写（权限位、大小写、中文文件名） |
| 列目录排序、mtime、大文件分块 | UKUI 文件管理器里同时操作同一目录时的行为 |
| 目录删除必须为空、改名拒绝覆盖 | 被 SELinux/AppArmor 限制的路径会报什么错 |

**没有假装验过的**：麒麟上的真实文件系统语义。本机跑的是 NTFS，
大小写不敏感、路径分隔符两套都在用，这些差异只有真机能暴露。

### M4 进行中：端口映射（已完成 · 本机已实测）

语义等价于 `ssh -L`：控制端在本机监听一个端口，字节经被控端转到**被控端所在网络**的目标上。
`uulan-client --forward 2222:127.0.0.1:22` 就是「连被控端本机的 22 端口」。

#### 先把 bulk 拆成多路复用（`crates/uulan-bulk`）

端口映射和文件传输对通道的要求不一样：文件传输是「传完就走」，端口映射是「一条 SSH 长占一个 id」。
原来的 `fs_loop` 独占 bulk、一次处理一条 `FsOp`，会把端口映射堵在队列里。所以先把连接拆成三个角色
（协议一行没改，`id|flags` 头早就支持多路复用）：

```
读任务   read_frame → BulkFrame::decode → HashMap<id, Sender> 分派   （唯一读者）
写任务   Receiver<BulkFrame> → write_frame                          （唯一写者，天然有序）
业务任务 每条流一个：文件传输 / 端口映射的 socket 桥，注册 id → 干活 → Drop 时注销
```

- **写只能有一个任务**。几个任务各拿半条 `TcpStream` 直接写会把帧切碎交错，而帧是长度前缀的
  —— 交错一次整条连接就报废，症状是「偶发协议解析错」。所有写出排队列。
- **`read_frame()` 只能在一个任务里 await**。它不是 cancel-safe 的（两次 `read_exact`），
  被 `select!` 取消就吞掉半个帧头。这条在 M4 文件传输那轮已经踩明白了，多路复用继续这么办。
- **id 的生死由 `Drop` 管**：`Stream` 一没了就把 id 从表里摘掉。业务任务 panic / 提前 return
  都不会在表里留下一个「永远收不到数据」的死 id。
- **反压是有代价的，写在明处**：读任务是单线程分派，一条流的消费端卡住会把反压传给**整条**
  bulk 连接。流队列有界（8 帧），所以卡住的是「所有流」而不是「内存」。第一版接受 ——
  文件传输和端口映射的消费端都是本地 socket，不会无故停读。
- **未知 id 的帧丢弃 + 记日志**，绝不能让一个陌生 id 把整条连接搞断（对面可能正在收尾）。

#### 被控端：白名单是安全边界（`crates/uulan-host/src/portmap.rs`）

端口映射天然是「内网跳板」：拿到密码的人能用它访问被控端所在的整个内网。所以判定放在**被控端**，
而且默认最严：

- **默认只允许本机回环**（`127.0.0.0/8`、`::1`）。远控端口映射最常见的用途正是「连被控端本机的服务」，
  这个默认值既能用又不危险。
- 要放开到网段必须**显式写进配置** `portmap_allow`（`10.0.0.0/8`、`192.168.1.5`、`*`）。
  配置写错（前缀超界、不是 IP）**报错并退回回环**，不是悄悄放宽。
- **解析出的多个地址逐个检查**，有一个不合规就整条拒绝 —— 否则一个名字可以被做成
  「一半合规一半不合规」绕过检查。连上之后再核对一次 `peer_addr()`（以实际连到的为准）。
- **白名单判定发生在「通道还活着吗」之前**。反过来的话，一条「bulk 已断开」会把真正的原因
  （策略拒绝）埋掉，排障时就只能猜。

#### 建映射的顺序：先连目标，再注册流，最后回 Ok

```
控制端 --forward 收到本机连接
  → 分配 id → C2H::PortMap(Open)
  → 被控端 portmap_loop：解析 → 白名单 → TcpStream::connect（最长 5s）
      成功：注册 bulk 流 → H2C::PortMapOk{id} → 起桥任务
      失败：H2C::PortMapErr{id,error}（此时**没有**任何流被注册）
  → 控制端收到 Ok 才在 bulk 上注册同 id 的流，开始搬字节
```

`PortMapOk` 兼作「放行信号」（和文件传输里 `FsStream{Begin}` 的角色一样）：控制端在收到 Ok
之前一个字节都不发，所以不存在「本地应用已经发了一堆、对端还不知道往哪儿送」。

#### 半关闭：两端对称，两边都结束才退

- 本地方向读到 EOF → 发 `END`；收到对端 `END` → 关掉本地写半边（`TcpStream::shutdown`）。
- 只关一半会把「对面还在发数据、我已经不听了」的数据丢掉（SSH 这类交互式会话里尤其明显），
  所以两个方向的结束各自独立，两个都到了才退出。
- 会话收尾时被控端 `PortMap::shutdown()` 把所有桥任务 abort ——
  **用户以为断了、其实内网连接还开着**是这类功能最坏的收尾方式。

#### 控制端：`--forward`（`crates/uulan-client/src/forward.rs`）

- 建映射要 DNS + TCP，最长几秒，所以走独立的 `portmap_loop` 队列，
  **绝不在 `control_loop` 里 await** —— 那条循环还负责键鼠和剪贴板。
- 目标连不上时立刻关掉本机连接，而不是让它挂着等自己的超时（`PortMapErr` → 直接 drop）。
- 这一版是**独立会话**（和 `--ls/--put` 一样，不开窗口、不收画面）。代价写清楚：
  边看画面边转发要跑两个客户端进程（被控端允许并存，不校验「第二个人」）。
- 只接受 IPv4 绑定地址与 IPv4/主机名目标；IPv6 字面量第一版不支持（与其猜分隔符，不如明确报错）。

#### 实测（本机，可重复）

```
cargo test --workspace        278 passed / 0 failed   （上一轮基线 256，新增 22）
cargo build --release         0 警告
bash scripts/verify-portmap-e2e.sh
```

`verify-portmap-e2e.sh` 跑的是**真二进制**（拷到 `D:/uulan-bin/pm-*` 下执行，见坑 2）：
真 server + 真被控端（lavfi 合成画面）+ 真控制端 `--forward` + 本机真 TCP echo 服务当目标。

```
① 控制端「转发已就绪」          本机 127.0.0.1:21989 → 被控端 127.0.0.1:21988
② 单流 262144 字节往返          逐字节一致（sha256 625d8ffe332e03f7）
③ 两条并发流 131072 / 196608    各自逐字节一致，没有串流
④ 控制端统计                    连接 3 / 上行 589824 字节 / 下行 589824 字节
⑤ 被控端日志                    白名单 allow=127.0.0.1/8, ::1/128
                               端口映射已建立 id=1..3（三个 id 各自 up/down 对称）
```

进程内集成测试 `crates/uulan-client/tests/portmap_path.rs` 另有四条（真 server + 真被控端会话 +
真 wire + 真 TCP，替身只有画面源）：

```
forwards_bytes_to_a_loopback_target              19 字节往返；统计上行/下行各 19
concurrent_streams_do_not_mix                    3 条流（4/11/18 KiB）同时跑，各自对称
dead_target_fails_the_local_connection_quickly   目标没人听 → 本机连接立刻被关，不计入转发数
target_outside_whitelist_is_rejected_by_the_host 192.0.2.1 被拒，错误里必须出现「白名单」
```

`uulan-bulk` 自己的 8 条单元测试钉住多路复用的语义：两条流不串、END 按流送达、未知 id 丢弃、
同 id 不能注册两次、Drop 释放 id、对端 EOF 唤醒所有流、`close()` 之后不能注册、64 KiB 往返。

回归（这一轮改了 bulk 的所有权，所以这两个必须一起跑）：

```
bash scripts/verify-files-e2e.sh     6 MiB 往返 sha256 一致（2fb40c0ef380）   ✅
bash scripts/verify-window-e2e.sh    窗口呈现 506 帧 / 首帧 74ms              ✅
```

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机可复现） | 只能在真麒麟上验 |
|---|---|
| 多路复用：并发流不串、END 按流送达、id 复用与释放 | 真实内网链路（MTU / 丢包 / 长肥管道）下的吞吐 |
| 真 TCP 转发：回显逐字节一致、半关闭、快速失败 | 连被控端本机 sshd / VNC / 数据库长连接的实际表现 |
| 白名单：默认回环、CIDR 匹配、多地址逐个检查、判定顺序 | `/etc/resolv.conf` 决定的内网域名解析行为 |
| 统计与日志：上下行字节、每条流 up/down 对称 | 麒麟上 `ss`/`netstat` 看到的连接是否也一一对应 |

**没有假装验过的**：真麒麟所在的内网拓扑。本机跑的是 loopback，延迟 <0.1ms、无丢包；
内网里 `--forward` 能不能顶住长连接（比如一路 SSH 挂几小时），只有真机能回答。

### M4 完成：终端（PTY）

协议（`term` / `term_out` / `term_exit` / `term_list`）早就定好了，这一轮把**两端接上**：

- 被控端 `crates/uulan-host/src/term.rs`：`PtyFactory` / `PtyHandle` 是可注入的后端
  （生产 `portable-pty`，测试用内存假 shell），`run()` 是纯逻辑 —— id 注册表、会话数上限（8）、
  输出切帧（每帧 16 KiB 原始字节，base64 后约 21.8 KiB）、退出码映射（被信号杀 → `128+signo`）、
  收尾（SIGHUP → 800ms → SIGKILL）。
- 控制端 `crates/uulan-client/src/termx.rs`：`TermLink` 只做「字节 ↔ 协议消息」（所以能自动测），
  `run()` 才把本机 stdin/stdout 接上去；unix 上进 raw 模式，客户端退出码 = 远端 shell 的退出码。
- 协议补了一条 `H2C::TermError{id,error}`：开不出 shell / id 不认识 / 输入不是合法 base64
  必须能说清是**哪条会话**出错 —— 静默丢弃会变成「按了键、屏幕没反应、日志也没有」。

实测（本机：真 TCP、真协议、真 id 注册表，PTY 换成内存假 shell）：

```text
cargo test -p uulan-host --lib term              15 passed   （状态机：开/写/改大小/关/退出/上限/切帧）
cargo test -p uulan-client --test term_path       4 passed   （真 TCP 端到端）
  回显逐字节一致；退出码 7 原样上报；
  40000 字节切成 3+ 帧且一个字节不少（单帧 ≤ 16 KiB）；
  resize 到 140x40 后 shell 读到的窗口大小确实是 140x40；
  主动关闭报 129（128+SIGHUP）
```

#### 这一轮踩到的两个坑（都已钉成测试）

- **退出事件被静默丢掉**：后端的「输出通道」先关（读线程收尾）**不等于**进程已退出。
  早期写法在输出通道关掉时直接 `break`，于是 `TermExit` 永远发不出去 —— 控制端的窗口会挂死。
  现在分两步：拿到退出码后先给 200ms 把最后一批输出刷完，再发 `TermExit`
  （顺序反了就是「命令失败但看不到任何报错」）。
- **`Close` 之后退出事件找不到会话**：`Close` 必须把 id 从注册表摘掉（之后的输入不能进死进程），
  但 `TermExit` 是进程真死之后才来的。少了 `closing` 集合，这次退出会被当成「未知会话」忽略。
  现在 `live`（还活着）与 `closing`（已下关闭指令、等退出）分开记，两种来源都要上报。

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机可复现） | 只能在真麒麟上验 |
|---|---|
| 开/写/改大小/关/退出整条状态机；id 复用、重复 id 拒绝、会话数上限 | 真 PTY：`/dev/ptmx`、termios 回显、行缓冲与 raw 模式的真实行为 |
| 输出切帧与字节完整性；退出码（含被信号杀的 128+signo） | SIGHUP→SIGKILL 对**交互式** shell 是否真的收得干净（bash 忽略 SIGTERM 是已知行为） |
| 协议与 base64；控制通道收尾时所有残留会话被收掉 | 麒麟自带 `bash/dash` 的启动文件、中文 locale 下的宽度/换行 |
| 控制端 unix raw 模式（stdin 是管道时自动跳过） | 真终端上的方向键 / `vim` / `top`；**Windows 控制端的 raw 模式尚未接（`SetConsoleMode`）** |

**没有假装验过的**：真 PTY 一行都没在本机跑过。`cargo check -p uulan-host --target x86_64-unknown-linux-gnu`
只证明 `cfg(unix)` 那几行（`libc::kill(SIGKILL)`）类型上成立，不证明它在麒麟上行为正确。

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
- **换分辨率会重开编码源，画面会断约 0.3–1 秒**（探针周期 + 编码器重启 + 控制端重建解码器）。
  期间丢掉的帧不补发；控制端此时若正好在拖动/输入，落点按新尺寸算，不会错位。
- **分辨率跟随只看主屏**（与「不支持多显示器切换」同一条限制）：副屏热插拔不会改变抓取区域。
- **端口映射只支持 IPv4 字面量与主机名目标**（第一版）。IPv6 字面量直接在命令行报错，
  不猜分隔符；需要时再加 `[::1]:port` 语法。
- **转发是独立会话**：`--forward` 和看画面不能共用一个客户端进程（要并成就跑两个客户端）。
  被控端不限制并发会话，所以两个进程能同时连上，但两边的剪贴板会各自同步。
- **默认白名单只有本机回环**。想连被控端内网里的别的机器（数据库、内网 Web）必须自己写
  `portmap_allow` —— 这是安全边界，不是缺省值没做完。
- **bulk 的读任务是单线程分派**：一条流转发卡住（对端不读、本地不消费）会反压整条 bulk，
  文件传输也会跟着慢。流队列有界 8 帧，所以不会撑爆内存。
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