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

第二阶段（**M6 已实现**，见 `crates/uulan-protocol/src/crypto.rs`）：
- **只在 control 通道**上、`Hello` 之前做一次握手：
  `CryptoHello{eph_c, nonce_c}` → `CryptoHello{eph_h, nonce_h}` → 双方各自算
  `K = HKDF-SHA256(X25519(eph_priv, peer_pub), salt=nonce_c||nonce_h, info="uulan-session-v1")`，
  并用 SHA-256 把两个公钥 + 两个随机数绑成「转录哈希」。
- **video / bulk 不重复握手**：用 `HKDF(K, "uulan-ch-v1"||通道||方向)` 派生子密钥。
  省一次往返（视频要尽快出首帧），而且三条连接本来就都过同一个中继，各握一次不会更安全。
  方向分开派生（c→h / h→c 不同密钥），所以两个方向的记录计数器可以各自从 0 起而不会撞 nonce。
- 之后每条连接的**每个帧**用 `ChaCha20-Poly1305` 加密：nonce = 连接内单调计数器（64 位），
  AAD 绑了 `"uulan-rec-v1"||通道号`（把视频帧塞进控制通道会直接解密失败）。
- 口令证明改为 `HMAC-SHA256(K, "uulan-auth-v1"||标签||转录哈希||口令)`，**双向**：
  控制端发 `uulan-auth-c1` 标签证明自己知道口令；被控端在 `AuthOk` 里回 `uulan-auth-h1` 标签的证明，
  控制端**必须校验**（不校验的话，中继可以冒充被控端收走所有键盘与文件）。
- 结果：`server` 即使被拿下，抓到的也只是密文 —— 见 `crates/uulan-client/tests/crypto_mitm.rs`
  的实测（代理记录 10978 字节，两个方向都找不到 canary 明文）。

> 关键是这套东西是**分层叠加**的：加密贴着 `frame` 层做，
> `SecReader`/`SecWriter` 实现了 `AsyncRead`/`AsyncWrite`，所以
> `uulan-bulk`、`uulan-fs`、终端、剪贴板、视频泵的业务代码一行都没改。

**这一层没有解决的（照实写）**：
- **主动 MITM 的中继**可以各连各的握手，于是它同时知道两份 K；这时它拿到的 `Auth` 是一个
  「口令对不对」的离线校验值，可以拿字典撞。彻底解决要 PAKE（SPAKE2/OPAQUE），属于后续工作。
  当前口径：**被动抓包看不到任何明文；主动 MITM 需要先拿下中继 + 具备离线爆破口令的算力**。
- 记录长度前缀仍是明文（帧长在旧协议里本来就明文，TCP 也能看出分段），所以「屏幕上有没有大动作」
  这类流量特征仍可见。
- 一条会话一把密钥，重连即换（临时密钥对每次新生成）；没有会话内的密钥轮换。

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
│  ├─ uulan-protocol/          # 帧编解码、消息类型、端到端加密层（crypto.rs：握手 + AEAD 记录层）
│  ├─ uulan-server/            # 中继/信令，bin: uulan-server
│  ├─ uulan-host/              # 被控端，bin: uulan-host（Linux only）
│  ├─ uulan-client/            # 控制端，bin: uulan-client（Windows + Linux）
│  ├─ uulan-clip/              # 剪贴板：平台无关的去重状态机 + Win32/X11 后端
│  ├─ uulan-bulk/              # bulk 连接的多路复用（读分派 / 写串行 / 每流一个任务）
│  └─ uulan-x11/               # X11 采集参数、输入注入、剪贴板（host 专用，隔离平台代码）
├─ deploy/                     # M5：现场部署（离线包 + systemd 单元 + 安装/卸载脚本）
   ├─ install.sh               # 系统装（root，/opt/uulan）或 --userland（无 root，~/.local/uulan）
   ├─ uninstall.sh             # 对称；--purge 连配置一起删
   ├─ make-package.sh          # 原生编译 → uulan-<ver>-x86_64-linux.tar.gz
   ├─ uulan-wait-x.sh          # 等 :0 就绪（ExecStartPre），超时交给 Restart 再等
   ├─ README.md                # 现场三步装 + 验收 + 已知限制
   └─ systemd/                 # uulan-server（系统）/ uulan-server.user / uulan-host（用户）
└─ tools/
   ├─ xvfb-test.sh             # 在无头 Linux 上用 Xvfb 起一个假的麒麟桌面来测
   └─ smoke-local.sh           # 本机冒烟（含 CLI、配置落盘）
└─ scripts/
   ├─ verify-window-e2e.sh     # 拷出工作区跑全链路 + 断言窗口真的呈现了帧
   ├─ verify-log-hygiene.sh    # RUST_LOG=trace 全量跑一遍，搜口令/票据/终端原文（M6）
   ├─ compare-ppm.py           # 两帧 PPM 的相关系数 + 平均绝对误差（真机逐像素比对用）
   └─ verify-clipboard-e2e.sh  # 真实系统剪贴板改一次，断言它经协议到了对端
```

## 8. 里程碑

状态：✅ 已完成并自测通过 · 🔄 进行中 · ⬜ 未开始

| 阶段 | 状态 | 交付 | 验收标准（实际达成的） |
|---|---|---|---|
| **M1 骨架** | ✅ | workspace 能编译；protocol 单测过；server 能注册设备并配对转发 | `cargo test` 全绿；本机起 server，两端能配对三条通道并盲转发 |
| **M2 能看见** | ✅ | 被控端采集编码、控制端解码；端到端视频通路 | 进程内端到端测试全绿；**并在真麒麟 V10 SP1 上跑通**：Windows 控制端看到 1920x1080 真实桌面，首帧 216ms、零丢帧、画面与实机截图相关系数 0.9995。**2026-10-24 补：麒麟控制端也过了同一套验收** —— `uulan-client` 在真机上编得出来（见 M5 小节），headless 29.7 fps / 首帧 256ms / 丢帧 0 / 相关系数 0.9998、MAE 0.50%，开窗 15 s `painted=870` |
| **M2.5 画好** | ✅ | 窗口 ✅；**键鼠注入 ✅（两端已接，本机全链路 + 真机 XTest 实测）**；**实测帧率上报 ✅**；**剪贴板文本双向 ✅（本机全链路 + 真 Win32；X11 后端已在真机与外部 X 客户端双向验过）**；**分辨率变化重建解码器 ✅（两端都做了，本机全链路实测）** | 能远程操作并双向粘文字、被控端换分辨率后画面自动跟上。真机实测见 `docs/milestones/M3-M4-真机-2026-10-23.txt`（XTest 注入：指针落点精确、keysym 正确、滚轮=按钮5、autorepeat 已还原；X11 剪贴板：与独立 X 客户端双向互通） |
| **M3 能操作** | ✅ | XTest 注入 + 按键连发处理；`--inject` 脚本化注入（可无人值守验）；uinput 兜底仍未做 | **真机已验**（2026-10-23）：`--inject` 走真链路 → 被控端 XTest → 真 X 服务器；用 `xev`/`xdotool` 当独立判据，坐标/键位/按钮号/配对全对（当次验在另起的 `Xvfb :99` 上）。**同一轮夜间批补验**：`--inject move:640,400` 直接打到真 `:0`，`xdotool getmouselocation` 读到 `x:640 y:400`（注入前 1901,0，已还原）。限制：`:0` 被 UKUI 锁屏挡着，**往应用里打字/拖拽仍未验** |
| **M4 工具齐全** | 🔄 | **文件传输 ✅**（列/取/送/建/删/改名/哈希 + 路径沙箱 + sha256，真二进制实测 6 MiB 往返一致）；**端口映射 ✅**（bulk 多路复用 + `--forward`，真二进制实测 256 KiB 单流 + 两条并发流逐字节一致）；**终端（PTY）✅ 本机全链路 + 真机 Linux 侧**（真 `/dev/pts`、termios 默认、winsize 下发、退出码 7 原样回来、SIGHUP→SIGKILL 升级日志）；**终端尺寸跟随 ✅（2026-10-25，真机验：本地窗口变了远端 PTY 跟着变）** | 三项都用真 server + 真被控端 + 真客户端链路验过。终端已上真麒麟验 Linux 独有部分；尺寸跟随真机实测：本地 pty `100x30`→`132x43` 后远端 `stty size` 由 `30 100` 变 `43 132`，`--term-size` 钉死时不跟随（见 `docs/milestones/终端尺寸跟随-2026-10-25.txt`）。剪贴板整链（Windows↔麒麟）本轮**未验**（cron 上下文没有交互式剪贴板） |
| **M5 麒麟落地** | 🔄 | 离线安装包（tar.gz + install.sh + 3 个 systemd 单元）✅、systemd 开机自启 ✅（用户态装实测）、**就地重装（升级）✅**、**安装预检前移 ✅（缺输入不再留半装前缀）**、**控制端可编可装 ✅（`--with-client` 打包 + install.sh 装进前缀，2026-10-24）**、部署文档 ✅（`deploy/README.md` + `docs/DEPLOY-KYLIN.md` 第 13/14/15 节） | 真机上 `bash install.sh --userland` 一次装通：中继+被控端都是 `enabled`+`active`、`Linger=yes`、`ExecStartPre` 的 uulan-wait-x 真的等到 `:0`；`systemctl --user kill -s KILL` 后 `NRestarts=1` 自动拉起并重新上线；包内二进制取帧 29.6fps、相关系数 0.9969。见 `docs/milestones/M5-麒麟落地-2026-10-23.txt`。**就地升级已验**（2026-10-24）：在服务正在跑的真机上从离线包装重装 → 设备 ID 与 host.json 一字未变、二进制被替换、KILL 后 systemd 拉起的是前缀里的新二进制、再连 29.2fps/丢帧 0；见 `docs/milestones/M5-就地升级与安装预检-2026-10-24.txt`。**控制端在麒麟上编+装已验**（2026-10-24，见 `docs/milestones/麒麟控制端-2026-10-24.txt`）：`--with-client` 的包 3.8M、重装后前缀里多出 `uulan-client`（sha 与包内/`target` 三者一致）。**未验**：整机重启后的自启、系统装（root，/opt/uulan）路径（本机 sudo 要口令）。2026-10-25 复核：`systemctl reboot` 对远程会话被 polkit 要求认证（`pkcheck --action-id org.freedesktop.login1.reboot` → *Authorization requires authentication*），mis 又没有免密 sudo，所以**无人值守时重启不了真机**——这条只能等人到机器跟前再验，别写成已验 |
| **M6 加固** | ✅ | **E2E 加密 ✅（本机全链路 + 中继抓包实测）**；**PAKE（CPace/Ristretto255）✅**：口令只用来生成生成元，会话密钥 = `HKDF(X25519 共享点 ‖ PAKE 共享点)` —— 主动 MITM 的中继既解不开任何通道、也没法拿证明离线撞字典；临时验证码「用一次即失效」✅ + **每小时自动轮换 ✅（只轮换已存在的码）**+ **运行中的服务能看见 `--new-temp-code` 新写的码 ✅**；**日志脱敏 ✅ 全量**（口令只在终端/`--show` 打；会话票据改成不可逆标签 `tXXXXXXXX`；终端内容/剪贴板只记长度；两个二进制的日志一律走 stderr）；控制端口令可走 `--password-file` / `UULAN_PASSWORD`；**系统指标 ✅**（`H2C::Metrics`：CPU/内存/网速，被控端读 `/proc` 差分）；**锁屏状态 ✅**（`H2C::SystemState`：锁屏时被控端**在进 Mapper 之前**丢输入并松开已按下的键；控制端也停发） | 中继抓包看不到明文内容 —— **已用「记录字节的代理」实测**：clipboard 两个方向共 10978 字节，找不到任何明文；日志脱敏见 `docs/milestones/M6-日志脱敏-2026-10-23.txt`；**PAKE 真机实测见 `docs/milestones/M6-PAKE-2026-10-24.txt`**：永久口令 29.6fps/首帧 227ms/丢帧 0/相关系数 0.9982（与改前 0.9980 同水平）、错口令被拒、临时码走候选序号 1 首次可用第二次被拒且用完不消耗永久口令、journal 122 行口令 0 命中。**指标与锁屏的真机数字见 `docs/milestones/M6-指标与锁屏状态-2026-10-25.txt`**：`--status` 报 locked=true、CPU 0.8–0.9% 与真机 `/proc/stat` 独立差分（0.72–2.00%）同量级，内存 3792–3800MB 对得上 `free` 的「总-可用」；用**旧客户端**（不认状态）往锁屏里 `--inject`，被控端 `dropped_locked=3`、`events=0`、指针一个像素没动。**唯一遗留**：PAKE 是本仓库自研实现，没有第三方审计。**断线重连 ✅（2026-10-25）**：被控端↔中继断线自动重连（退避 1/2/4/8/15/30 s 封顶，**进程不退、设备 ID 不变**），并加注册令牌让「同一台机器重连」能拿回自己的 ID、而「后来者」仍顶不掉在线设备。真机：中继连续 KILL 5 次，host 的 MainPID 与 NRestarts 一次都没变，每次约 2–3 秒重新上线且设备 ID 仍是 805747189；重连后取帧 29.6 fps / 丢帧 0 / 首帧 229 ms / 相关系数 0.9981（基线 0.9981）。见 `docs/milestones/断线重连-2026-10-25.txt` |

### M5 落地：部署形态（systemd + 离线安装包）

内网没有公网、没有 apt 源，所以**没有 deb/rpm 仓库**：安装包就是
`uulan-<ver>-x86_64-linux.tar.gz`（三个二进制 + 三个 systemd 单元 + `install.sh` + 文档），
两条安装路径：`sudo bash install.sh`（root，`/opt/uulan`）或 `bash install.sh --userland`
（普通用户，`~/.local/uulan`，现场拿不到 sudo 口令时用）。

几个决定不是风格问题，是「起不起来」的问题：

- **被控端永远是 systemd 用户单元。** 它要 x11grab 抓 `:0`、要 XTest 注入、要读
  `~/.Xauthority`；系统单元跑在 root 下没有会话，X11 授权直接拦住 → 黑屏。
- **开机顺序不靠 autologin**：用户单元挂 `default.target`，`ExecStartPre=uulan-wait-x 300`
  先等 `xdpyinfo` 能打开 `:0`，超时以 1 退出 → `Restart=always` 再等一轮。
  于是「先开机、后登录」也能在登录后自己追上。
- **口令不进 journal**：`uulan-host` 的访问密码只在 stdout 是终端、或显式 `--show` 时才打印
  （`journalctl --user -u uulan-host` 现场最容易被贴进工单）。
- **中继要两份模板**：系统装带 `ProtectHome/ProtectSystem` 沙箱；用户态装**不能** hide 家目录
  （台账 `state/devices.json` 就写在那里），所以单独一份 `uulan-server.user.service`。
- **打包与安装的路径约定，必须由「从包里装一遍」来证**：第一次从离线包装就断在
  `uulan-wait-x` 的位置差异上（源码树在 `deploy/`，包里在 `bin/`），`set -e` 把安装打断，
  留下一个只有 `uulan-host` 的半装前缀。

实测（2026-10-23 真麒麟，用户态装）：`enabled`+`active`、`Linger=yes`、
`systemctl --user kill -s KILL` 后 `NRestarts=1` 自动拉起并重新上线（同一设备 ID）、
包内二进制取帧 29.6 fps、与真机截图相关系数 0.9969。
**未验**：整机重启后的自启、root 系统装路径。详见 `docs/milestones/M5-麒麟落地-2026-10-23.txt`。

#### 就地升级与安装预检（2026-10-24）

内网现场的升级路径只有一条：把新的 `uulan-<ver>-x86_64-linux.tar.gz` 拷进去、解包、
在**服务正在跑**的机器上重跑一遍 `install.sh`。这条路径的真机实测结论：

- **配置与身份不会被覆盖**：重装前后 `~/.config/uulan/host.json` 的 sha256 一字未变
  （设备 ID 805747189、访问口令、临时码全在），安装摘要会自己把设备 ID 读出来打印。
- **跑起来的是前缀里的新二进制**：`systemctl --user kill -s KILL` 让 systemd 重新拉起后，
  `readlink /proc/<pid>/exe` 指向 `~/.local/uulan/bin/uulan-host`，其 sha256 与 `target/release` 一致。
- **在 ext4 上替换正在运行的可执行文件不需要先停服务**：`install` 走的是 unlink+create，
  同物异位对照（运行中的 `sleep` 被 install 覆盖）退出码 0、进程照旧跑；
  所以「升级 = 重装 + 重启服务」，没有 ETXTBSY 这一关。

**安装预检必须挡在动磁盘之前。** 原来的预检漏了 `uulan-wait-x`（它在 `install -d $PREFIX`
和两个二进制落盘**之后**才去找），于是缺它就在装到一半时 die —— 真机上真的留下过
一个只有 host+server 的半装前缀，`systemctl` 显示 failed，而安装输出看着只是「报了个错」。
现在三种布局的查找都在预检段（前缀一个字节都不写），并由 `scripts/verify-install-preflight.sh`
钉住：假 getent/systemctl/loginctl + 沙箱家目录 + 假二进制，四条失败路径都断言
「退出码非 0 **且 prefix 未被创建**」，外加一条正常路径。本机 15/0（正常路径因 MSYS 的
目录 chmod 限制标 SKIP），真麒麟 15/0（含正常路径）。

另附 20 分钟长连接稳定性实测（同一轮）：持续 1200 s、收到 AU 35998、解出 35990 帧、
**解码丢帧 0**、稳定 30.0 fps、平均码率 9058 kbps、首帧 205 ms；被控端进程内存
8004 kB → 9448 → 9468 → 收尾 7720 kB（无单调上涨），线程 17–19。

详见 `docs/milestones/M5-就地升级与安装预检-2026-10-24.txt`，升级步骤见
`docs/DEPLOY-KYLIN.md` 第 14 节。

#### 控制端（麒麟）编得出来、装得进去（2026-10-24）

设计目标一直是「控制端同时支持麒麟和 Windows」，但到这一轮之前，**麒麟侧的二进制从来没存在过**：
`libx11-dev` 在麒麟源里装不上，`x11.pc` / `X11/*.h` / `libX11.so` 全缺，`cargo build -p uulan-client`
直接断在 `tiny-xlib` 的 `pkg-config x11` 上。两处修法（都是构建期的事，不动业务代码）：

- **softbuffer 走 dlopen**：unix 段加 `x11-dlopen` 特性（`tiny-xlib/dlopen` + `x11rb/dl-libxcb`），
  libX11/libxcb 变成运行期 `dlopen`，构建期不需要任何 X11 开发文件。
- **`#[link(name = "X11")]` 的链接期软链自己造**：`uulan-clip` / `uulan-host` 各加一个 `build.rs`，
  系统里没有 `libX11.so` 时在 `OUT_DIR/x11-link/` 里软链出这个名字再 `cargo:rustc-link-search`。
  以前这条路要走 root 改 `/usr/lib`，现场没有 sudo 口令就断在这里。

由此 `make-package.sh --with-client` 能打出带控制端的离线包，`install.sh` 见到包里有
`uulan-client` 就装进前缀（它不是服务，不建单元）。真机实测：麒麟控制端 headless 29.7 fps /
首帧 256ms / 丢帧 0 / 与真机截图相关系数 0.9998、MAE 0.50%；开窗 15 s `painted=870`；
`--term` 里 shell 的 `exit 3` 原样回来。详见 `docs/milestones/麒麟控制端-2026-10-24.txt`
与 `docs/DEPLOY-KYLIN.md` 第 15 节。

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

#### 真机验收（2026-10-23）：XTest 真的把键鼠打进了 X 服务器

两种验法以前都没有：窗口模式要人手点，命令行没有注入开关。这一轮补的是
**`uulan-client --inject`**（`crates/uulan-client/src/inject.rs`）—— 把一串动作写成脚本，
沿真实链路发出去（控制通道 → 中继 → 被控端 `Mapper` → XTest），再由**第三方观察者**
给结论：

```
--inject "move:300,200; click:left; type:hello; tap:0x41; wheel:down; move:400,300"
```

判据用的是真机上的 `xev`（一个独立的 X 客户端，逐条打印它收到的事件）与 `xdotool`：

```
xdotool getmouselocation  → x:400 y:300              ← 最后一条 move 精确落点
xev: MotionNotify 2 / ButtonPress 2 / ButtonRelease 2 / KeyPress 6 / KeyRelease 6
     keysym: h,e,l,l,o,a（tap:0x41 = VK 65 → 0x61）
     ButtonPress: button 1 root:(300,200) / button 5 root:(300,200)   ← 左键 + 滚轮一格
xset q → auto repeat: on                              ← 会话结束还原了全局 autorepeat
```

注入点用的是真机上另起的 `Xvfb :99`（同一台机器、真 X 服务器 + 真 XTest），
因为当时 `:0` 被 `ukui-screensaver-dialog` 全屏锁着 —— 往锁屏里打字验不出东西，
也不该去动用户的锁屏状态。**仍未验**：`:0` 真实桌面/WM 上的打字与拖拽（要解锁）。

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

#### 真机验收（2026-10-23）：X11 后端在真机上跟**外部程序**互通，并修掉一个 ABI 错位 bug

之前这里是「`x11.rs` 里每一个 Xlib 调用在本机一行都没执行过」—— 现在已经上过真机，
而且第一次上就抓到一个只有对面是别的程序才会暴露的 bug：

- **一个结构体干了两件事**：`XSelectionRequestEvent`（有 `owner`）和 `XSelectionEvent`
  `SelectionNotify`（**没有** `owner`）被合并成同一个 `#[repr(C)]` 结构，靠
  `owner_or_requestor` 兼容。于是处理 notify 时字段整体错位一格：
  `selection` 读到的是 `target`、`property` 读到的是 `time`。
  表现是两头都像卡死 —— **读别人的剪贴板永远超时，别人读我们的永远等不到回音**；
  自己读自己反而永远成功（走 `own_text` 缓存，根本不经过 X 协议），所以本机测不出来。
- 修法：拆成 `SelectionRequestEvent` / `SelectionNotifyEvent` / `SelectionClearEvent`
  三个结构体，并加**布局断言**（64 位下 80/72/56 字节 + 关键 offset + 两条与指针宽度
  无关的结构关系）。以后谁再"合并一下省点代码"，`cargo test` 立刻红。

真机实测（判据是一个独立写的 Xlib 客户端 `tools/xclip-or.c`，不是我们自己的代码）：

```
GET-BEFORE len=17 <<<ORACLE-OWNED-7f3a>>>      我们的 requestor 侧读到外部 owner 的内容
SET-OK <<<PROBE-WRITE-42>>>
外部客户端：OR-GET len=14 <<<PROBE-WRITE-42>>>  我们的 owner 侧能应答外部 requestor
```

`crates/uulan-clip/examples/x11_clip_probe.rs` 就是这条验收的探针（写完保持持有 N 秒，
好让外部客户端在这期间读回）。**仍未验**：Windows ↔ 麒麟的整链（需要一边有交互式剪贴板；
cron 上下文里 `Set-Clipboard` 之后 `Get-Clipboard` 读回空串，驱动不了）。

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

实测分两层，**两层都在本机跑过**：

```text
# 第一层：协议与状态机（PTY 换成内存假 shell，可以断言每个细节）
cargo test -p uulan-host --lib term              16 passed
cargo test -p uulan-client --test term_path       4 passed
  回显逐字节一致；退出码 7 原样上报；
  10009 字节切成 3+ 帧且一个字节不少（单帧 ≤ 16 KiB）；
  resize 到 140x40 后 shell 读到的窗口大小确实是 140x40；
  主动关闭报 129（128+SIGHUP）；控制端不收时反压生效（消息一条不丢、顺序不乱）

# 第二层：真 PTY + 真进程（真二进制、真 server、真 client）
bash scripts/verify-term-e2e.sh
  → 被控端日志 backend="portable-pty"、shell=cmd.exe
  → 远端 cmd.exe 真的执行了 echo uulan-term-ok
  → 客户端退出码 = 远端 shell 的退出码（7）
```

#### 真机验收（2026-10-23）：麒麟上的 /dev/ptmx + termios + SIGHUP→SIGKILL

```text
printf 'tty; echo "SHELL=$0"; stty -a | head -3; echo UULAN-TERM-$((6*7)); exit 7\n' \
  | uulan-client ... --term --term-shell /bin/bash

tty   → /dev/pts/1                                 真 PTY（/dev/ptmx），不是管道
$0   → /bin/bash                                   麒麟 bash 启动文件生效（提示符 mis@mis-pc:~$）
stty  → rows 30; columns 120; intr=^C; erase=^? …   termios 默认齐全、winsize 按 --term-size 下发
输出 → UULAN-TERM-42；客户端退出码 → 7             退出码原样回来
```

收尾（这轮真正想验的一步）—— 交互式 bash 里 `bash -c 'trap "" HUP; sleep 300' &` 之后关会话：

```text
host: 终端会话结束 id=1 code=129
      WARN 终端会话不响应 SIGHUP，补 SIGKILL         ← 升级路径真的走到了
结果: 忽略 SIGHUP 的那个进程活下来了（pid 522630 / sleep 522631）
对照: 普通后台作业 `sleep 300 &` 关会话后被清掉（bash 退出时 HUP 自己的作业）→ 残留 0
```

口径：**普通作业会被收干净；显式 `trap '' HUP` 的进程会活下来** —— 这正是 nohup 语义，
不是缺陷。但文档必须写明，否则用户会以为「关掉会话就等于杀掉远端进程」。

第二层在 Windows 上走的是 **ConPTY**（`portable-pty` 的 native 后端），所以
「起 shell → 读输出 → 写输入 → 传退出码 → 会话收尾」这套管线对**真进程**是成立的，
不只是对假后端成立。麒麟上要额外验的是 Linux 独有的部分（`/dev/ptmx`、termios、
SIGHUP→SIGKILL 的收尾），见 docs/DEPLOY-KYLIN.md §10。

#### 这一轮踩到的两个坑（都已钉成测试）

- **退出事件被静默丢掉**：后端的「输出通道」先关（读线程收尾）**不等于**进程已退出。
  早期写法在输出通道关掉时直接 `break`，于是 `TermExit` 永远发不出去 —— 控制端的窗口会挂死。
  现在分两步：拿到退出码后先给 200ms 把最后一批输出刷完，再发 `TermExit`
  （顺序反了就是「命令失败但看不到任何报错」）。
- **ConPTY 会先问光标位置，不答就不往下画**：Windows 被控端一开会话，ConPTY 就给「终端」
  发一条 DSR（`ESC[6n`），等一句 `ESC[row;colR`。控制端只是个字节管道，不回答的话
  shell 就卡在那里（表现是「连上了、什么都没有、3 秒后会话被杀」）。
  现在控制端在输出里见到 DSR 就回 `ESC[1;1R`（跨帧也要认，所以留了 3 字节尾巴）。
  这不是做终端仿真，只是把「必须回的那一条」回了；unix 的 PTY 不发这个，麒麟路径上是空转。
- **本机 stdin 结束 ≠ 用户要断开**：`printf 'ls\nexit 7\n' | uulan-client --term` 里
  stdin 一关就发 `Close` 会把 shell 杀掉，退出码变成 129。现在按终端惯例先发 EOT（Ctrl-D），
  再挂一个 3 秒看门狗：对方自己走就拿它的退出码，赖着不走才强关。
- **`Close` 之后退出事件找不到会话**：`Close` 必须把 id 从注册表摘掉（之后的输入不能进死进程），
  但 `TermExit` 是进程真死之后才来的。少了 `closing` 集合，这次退出会被当成「未知会话」忽略。
  现在 `live`（还活着）与 `closing`（已下关闭指令、等退出）分开记，两种来源都要上报。

#### 已经验过 / 只能在真麒麟上验

| 已经验（本机可复现） | 只能在真麒麟上验 |
|---|---|
| 开/写/改大小/关/退出整条状态机；id 复用、重复 id 拒绝、会话数上限 | `/dev/ptmx` 起的真 PTY：termios 回显、行缓冲与 raw 模式的真实行为 |
| **真 PTY 起真进程**（ConPTY + cmd.exe）：执行命令、拿回输出、退出码 7 原样传递 | SIGHUP→SIGKILL 对**交互式** shell 是否真的收得干净（bash 忽略 SIGTERM 是已知行为） |
| 输出切帧与字节完整性；退出码（含被信号杀的 128+signo）；反压不丢字节 | 麒麟自带 `bash/dash` 的启动文件、中文 locale 下的宽度/换行 |
| 控制端 unix raw 模式（stdin 是管道时自动跳过）、ConPTY 的 DSR 应答 | 真终端上的方向键 / `vim` / `top`；**Windows 控制端的 raw 模式尚未接（`SetConsoleMode`）** |

**没有假装验过的**：Linux 的 PTY 路径一行都没跑过。本机跑通的是 Windows ConPTY；
`cargo check -p uulan-host --target x86_64-unknown-linux-gnu` 只证明 `cfg(unix)` 那几行
（`libc::kill(SIGKILL)`）类型上成立，不证明它在麒麟上行为正确。

#### 终端尺寸跟随（2026-10-25 · 真机验）

开会话时下发一次行列是不够用的：控制端窗口一拉大，远端 `vim`/`top` 还按旧行列排版。这一轮接上，
三个决定值得写清：

- **轮询而不是 `SIGWINCH`**：Windows 没有这个信号（那边只能问 `GetConsoleScreenBufferInfo`），
  而信号处理器里不能碰 tokio（async-signal-safe），真要接得再造一条自我唤醒的管道。
  现在 300 ms 问一次，**只在尺寸变化时发消息**，空闲时零字节开销。
- **纯逻辑单独一层**（`termx::SizeWatch`）：盯三条规则 —— 「0 不是尺寸（是问不到）」、
  「问不到 ≠ 变了」、「同一个尺寸不重复发」。这三条错任何一条，远端要么排版崩（0 行列），
  要么每 300 ms 收一条无意义的 resize。规则都在状态机里，所以本机单测就能钉住。
- **基线是「已经发出去的那个」**：`SizeWatch::new(initial)` 拿的是开会话时真正下发的行列，
  否则第一次观察就是假变化、远端白重排一次。

真机实测（2026-10-25，麒麟 V10 SP1 / 海光 C86；python pty 起**真**客户端 → 真中继 → 真被控端 → 真 PTY）：

```text
本地 pty 100x30  → 远端 stty size = 30 100
本地 pty 132x43  → 1.2s 后远端 stty size = 43 132；客户端日志「本机终端尺寸变了」×1
对照 --term-size 80x24（钉死）→ 本地改成 132x43，远端仍是 24 80，日志 0 条通知
两次会话客户端退出码都是 0
```

`--term-size` 那条对照是必须的：它证明「跟随」是**新加的行为**，不是本来就在乱发改尺寸。

### M6 加固：端到端加密（已完成 · 本机全链路 + 中继抓包实测）

中继一直是「看不懂内容的盲转发」，但过去这只是**约定**：它是明文转发，谁在 server 上抓一次包，
就拿到了屏幕、键盘、剪贴板、文件。M6 把这条约定变成事实。

**分层**（`crates/uulan-protocol/src/crypto.rs`，约 700 行含单测）：

| 层 | 内容 | 为什么这么放 |
|---|---|---|
| 握手 | X25519 临时密钥对 + 双方随机数 → HKDF-SHA256 → 会话主密钥 K；SHA-256 转录哈希把两个公钥、两个随机数绑在一起 | 只在 control 通道做一次；视频/端口映射不额外往返 |
| 子密钥 | `HKDF(K, "uulan-ch-v1" ‖ 通道号 ‖ 方向)` | 换通道/换方向就换密钥，两个方向的计数器各自从 0 起也不会撞 nonce |
| 记录层 | 每个帧一个记录：`u32 长度 ‖ ChaCha20-Poly1305(帧字节)`，nonce = 连接内 64 位计数器，AAD = `"uulan-rec-v1" ‖ 通道号` | 贴在 `frame` 层做，业务代码零改动 |
| 口令证明 | `HMAC-SHA256(K, "uulan-auth-v1" ‖ 标签 ‖ 转录哈希 ‖ 口令)`，控制端 `-c1` / 被控端 `-h1` | `Auth` 不再发明文口令；双向都要验（否则中继能冒充被控端） |

**为什么用 `SecReader`/`SecWriter` 而不是「改所有消息类型」**：两者实现了 `AsyncRead`/`AsyncWrite`，
于是 `read_frame`/`write_frame` 的调用点一个字都没变 —— `uulan-bulk` 的多路复用、`uulan-fs` 的
`Bulk<R,W>`、终端的切帧、视频泵的交接全部照旧。整个改动只落在「连接怎么造」那几行：
被控端 `session.rs` 开头、控制端 `connect.rs`。

**一个必须这样的细节**：控制通道在 Hello/Auth 阶段就已经在用加密两半了，所以它**不能**再被包成
一条整连接交出去 —— 重新构造会让计数器归零＝nonce 重用＝把明文交出去。因此 `connect()` 交出的
`SecControl` 只是「已经走了一段的两半」。这条写成了类型（`SecControl` vs `SecConn`），不靠注释提醒。

**实测（2026-10-22，本机）**：

- `cargo test -p uulan-protocol` 58 passed（其中 crypto 16 项：两端密钥一致、通道/方向密钥互不相同、
  跨会话密钥不同、证明方向不可冒用、版本/低阶点拒绝、记录往返、**密文里搜不到明文**、
  篡改一个 bit 必须失败、**重放上一条记录必须失败**、换方向密钥读不了、截断报 UnexpectedEof、
  2 MiB 大帧往返、200 条小记录按序、帧辅助函数拒绝坏数据）
- `crates/uulan-client/tests/crypto_mitm.rs`：控制端 → **记录字节的代理（扮演中继）** → 真 server
  → 真被控端。代理记录 **10978 字节**，两个方向都**找不到** canary 明文；与此同时被控端的
  假剪贴板确实收到了那段文字（写入 1 次）——「看不见明文」不是因为没发。
  同一份记录里仍能看到会合阶段的明文 JSON（`relay_open`/`client_connect`），
  说明记录本身有效，不是空跑。
- 全链路回归：`verify-window-e2e.sh` / `verify-files-e2e.sh` / `verify-portmap-e2e.sh` /
  `verify-term-e2e.sh` / `verify-clipboard-e2e.sh` 全部照旧通过（它们现在跑的就是加密链路）。

**踩到的坑（值得记住）**：加密层把**整条记录一次性**解出来，收帧任务一个 poll 里能连读几十帧，
于是视频泵瞬间把 8 格队列冲满 —— `end_to_end` 里「环回不丢帧」这条断言当场变红（20 帧只解出 10 帧）。
丢帧策略本身没问题（远程桌面宁可跳帧也不排队），错的是「一个调度窗口里连读到队列满」。
修法是在队列满时给解码任务 `DROP_GRACE = 20ms` 的追赶时间（约 30fps 一帧间隔的三分之二），
真跟不上才丢 —— 修完 20/20 帧全解出，丢帧数归零。
**这条比加密本身更容易忽略**：任何「批量到达」的改动都要回头看下游有界队列的行为。

**本机验不了 / 待真机**：

- X25519/AEAD 本身与平台无关，单测已覆盖；麒麟上只需确认 `getrandom` 能正常取熵（麒麟 V10 SP1 正常）。
- 加密后 CPU 占用已在真机上量过（2026-10-24，Hygon C86 3250 8 核 16 线程，1080p@30 推流 20 分钟）：
  `uulan-host`（含加解密、协议、输入）**0.9%** 单核、`uulan-server` **0.4%**，
  大头是软件编码 `ffmpeg(x11grab+libx264)` **130%**。所以加密本身的开销可以忽略，
  1080p 的 CPU 预算全在 x264 上（要省就得换硬件编码或降帧/降分辨率）。
  采样时屏幕基本静止，是 x264 最省的一档；动态桌面上编码那部分会更高。
- 与旧版本**不互通**：协议已到 v3（v1 明文 → v2 强制加密 → v3 加 PAKE），服务端会用
  `proto_mismatch` 拒绝旧端；被控端等不到 `CryptoHello` 会在 10 秒后收尾。这是刻意的：
  宁可连不上，也不要悄悄退回明文/弱钥。**升级必须三件一起换**（server + host + client）。

### M6 加固：一次性临时码的热加载（已完成 · 真机实测）

装好的机器上，被控端是常驻的 systemd 用户单元；而 `uulan-host --new-temp-code` 是**另一个进程**
（现场就是「敲一条命令，把码发给同事」）。原来 `HostState` 只在启动时读一次 `host.json`
并把凭据留在内存里，于是：

- CLI 写进磁盘的新码**常驻服务根本看不见** —— 真机实测：新码连上去直接被「密码不正确」拒掉；
- 更糟的是服务下一次 `persist()`（吃掉临时码、或按小时轮换）会拿内存里的旧值把文件盖回去，
  于是 CLI 生成过什么都不留痕迹；
- 日志上什么都看不出来，现场表现就是「生成的验证码不能用」。

修法：`HostState::adopt_external_config()` 在**鉴权前**和**轮换临时码前**读一次磁盘，
只采用凭据相关的三项（口令 / 临时码 / 显示名）；设备 ID 与采集参数属于运行期状态，
仍由进程自己维护，不让一份可能过期的文件盖回去。口令与临时码的变化只记「变了」，值不进日志。

真机验收（2026-10-24，麒麟真机 + Windows 控制端）：

```
# 改前（旧二进制）
uulan-host --new-temp-code → 生成的码连上去：「被控端拒绝：密码不正确」（exit 1）

# 改后（2f44513）
uulan-host --new-temp-code → 新码第一次连接：已连上（exit 0），journal:
    INFO uulan_host::session: 配置文件里的一次性临时验证码变了（新码不写日志）
    INFO uulan_host::session: 鉴权通过 peer=… used_temp_code=true
  同一个码再连一次：「被控端拒绝：密码不正确」（exit 1）
  host.json 里 temp_code 已清成 null（用一次即失效，落盘生效）
  永久密码照旧可用（exit 0）
```

单测 `temp_code_written_by_another_process_is_honoured` / `password_written_by_another_process_is_honoured`
把这两条钉住（都是「先红后绿」：没有 adopt 时第一个断言就失败）。

### M6 加固：日志脱敏（已完成 · 本机 + 真机）

远控系统最容易被忽视的一条攻击面不是网络，而是**日志**：`journalctl --user -u uulan-host`
一行就能导出全部历史，而工单、聊天群、邮件里贴的日志从来不会先做脱敏。

这一轮把「哪些东西可以进日志」定成规矩，并做成可重复的验收：

| 东西 | 以前 | 现在 |
|---|---|---|
| 访问密码 / 临时码 | 只在终端或 `--show` 时打印（已经是这样） | 不变；新增 `scripts/verify-log-hygiene.sh` 把它变成**每轮可执行的断言** |
| **会话票据**（32 hex，60 秒有效，能拨 relay 三条通道） | 原样打进出 5 处日志（服务端 3、被控端 1、控制端 1） | `uulan_protocol::redact::tag()`：`t` + SHA-256 前 4 字节，稳定但不可逆 |
| 终端里敲的命令 / 剪贴板内容 | 只记字节数与 id（本来就没打过内容） | 不变；金丝雀测试把它钉住 |
| 控制端口令的传递方式 | 只有 `--password`（会进 `ps`、shell 历史、工单截图） | 新增 `--password-file`（取首行去空白）与 `UULAN_PASSWORD`，优先级命令行 > 文件 > 环境 |
| 日志的输出通道 | `tracing_subscriber::fmt()` 默认写 **stdout** | 两个二进制都显式 `.with_writer(std::io::stderr)` |

**为什么票据必须脱敏而不是「反正 60 秒就过期」**：60 秒足够一条脚本把三条通道拨起来并挂着；
而 journal 默认永久保留。真机实测里，改前那 15 行旧票据至今仍躺在 journal 里 ——
日志一旦写出就收不回来，所以只能在写的时候就不写。

**为什么日志必须走 stderr**：`RUST_LOG=info uulan-client --term > out.bin` 里，stdout 是
**要被程序消费的 PTY 字节流**。把 `INFO 鉴权通过` 混进终端字节，轻则终端花屏，
重则「脚本按 \r 切命令」的逻辑错位。日志归 stderr，stdout 只留终端字节、目录列表、统计 JSON。

#### 实测（本机 `scripts/verify-log-hygiene.sh`，`RUST_LOG=trace` 全量）

```text
日志行数合计：134（server 38 / host 86 / client 10）
✓ 访问密码原值 0 次命中      ✓ 终端命令金丝雀 0 次命中
✓ 会话票据原值（session=<32 hex>）0 次命中     ✓ 会话是标签形态 tXXXXXXXX
✓ 终端 stdout 干净（没有日志行混进 PTY 字节流）
✓ 确有会话 / 确有终端会话 / 确有文件操作（证明不是空跑）
```

反向断言是刻意的：「搜不到密码」只有在「确实发生过一次会话」时才有意义。

#### 实测（真麒麟 journal）

```text
从重新部署时刻起：journal 162 行
  原始票据（32 hex）：0       标签形态（tXXXXXXXX）：30       口令：0
⚠ 更早的 15 行仍是旧格式票据（2026-10-22 21:27–21:29，改前的二进制写的）
```

### M6 加固：PAKE（已完成 · 真机实测 2026-10-24）

加密接通之后还剩一个洞：**握手是不认证的 X25519**。主动 MITM 的中继可以各连各的握手，
同时拿到两份主密钥 —— 于是它既能解密全部流量，又拿着 `HMAC(会话密钥, …‖口令)` 这份证明
去**离线**校验每一个口令猜测（每猜一次不需要再连一次）。

改成 CPace（balanced PAKE，Ristretto255，`crates/uulan-protocol/src/pake.rs`）：

```text
控制端                                            被控端
  G = H2G(DSI‖sid‖口令)                             G_i = H2G(DSI‖sid‖候选_i)
  Ya = G^x        --PakeInit{ya}-->
                  <--PakeResp{yb_0..yb_n}--         Yb_i = G_i^y   （每个候选凭据一个点）
  K_i = Yb_i^x                                      K = Ya^y
会话密钥 = HKDF(X25519 共享点 ‖ K)                  只有命中的那个 i 两端相等
```

几个不是「照论文抄一遍」的地方：

- **零新依赖**：只用 `curve25519-dalek` 的 Ristretto255 与 `from_uniform_bytes`
  （官方哈希到群）。它本来就在依赖树里（x25519-dalek 的底层），内网离线打包不多带东西 ——
  这一条直接决定了能不能在现场装。
- **sid 必须是握手的转录哈希**：不绑的话，两次会话的 PAKE 点可以互相剪接。
- **多候选凭据不改协议形态**：被控端有「永久口令 + 临时码」，而 PAKE 之后证明里没有口令、
  也没法拿口令逐个试算。所以控制端**每个候选次序各算一把密钥、各给一份证明**，
  被控端拿自己那一个共享点核对，命中序号由 `AuthOk.idx` 回传。
  `Auth` 因此变成 `proofs: Vec<String>`，**条数必须等于候选数**（多给少给都直接拒）。
- **鉴权通过后原地换密钥**：控制通道用 `rekey()`（换密钥 + 计数器归零；新密钥下同一个
  计数器值不会撞 nonce）。**绝不能重建 `SecConn`** —— 同一把密钥重建就是 nonce 重用。
  视频/bulk 此时还没开始用，直接用最终密钥建即可。
- **版本硬性递增**：`PROTO_VERSION 2→3`、`CRYPTO_VERSION 1→2`。旧端在 `CryptoHello`
  就被 `BadVersion` 挡掉（实测旧客户端连新服务端：`ProtoMismatch 服务端协议 v3，控制端 v2`），
  不会出现「一半新一半旧」的半加密状态。

真机实测（2026-10-24，海光 C86 / 1920x1080 / :0 锁屏态）：永久口令 12 s headless
解出 355 帧、丢帧 0、首帧 227ms、29.6 fps、与 x11grab 截图相关系数 **0.9982** / MAE 0.71%
（改前同口径 0.9980 / 0.71% ⇒ 密钥派生改动没碰坏画面通路）；麒麟本机控制端 29.6 fps / 丢帧 0；
错口令被拒；外部进程写的临时码首次可用（`cred_slot=1`）、第二次被拒、永久口令不受影响。
详见 `docs/milestones/M6-PAKE-2026-10-24.txt`。

### M6 加固：系统指标与锁屏状态（已完成 · 真机实测 2026-10-25）

协议里 `H2C::Metrics` / `H2C::SystemState` 从 M1 就定义好了，但被控端主循环里
**一条都没发过** —— 控制端看不到对面的负载，也不知道对面已经锁屏，还会把键鼠
继续往锁屏里送。这不是缺功能，是隐患：XTest 是 X 服务器级注入，**绕过锁屏的抓取**。
本轮第一次上机就复现了 —— 门禁没生效时 `--inject move:700,400` 真的把指针从
`x:1599 y:0` 挪到了 `x:700 y:400`（当时 `:0` 正被 `ukui-screensaver-dialog` 锁着）。

#### 被控端：`crates/uulan-host/src/probe/`

- **纯逻辑与平台代码分开**（同键鼠那套）：`/proc` 解析、差分、锁屏窗口名判定全是纯函数，
  Windows 上 `cargo test` 就能钉住；只有碰 X 的那一步在 `probe/x11.rs`（`cfg(linux)`）。
- **第一拍不编数**：没有差分基线时 CPU/网速报 0（语义同 `VideoCfg.actual_fps` 的 0）。
  计数器回绕、同一毫秒连拍、`idle` 比上一拍还少 —— 一律「不知道」，不是「0%」。
- **`/proc` 的三个口径**：CPU 只累加前 8 列（`guest`/`guest_nice` 已经含在
  user/nice 里，重复累加会让总时间虚高、百分比偏低）；内存用 `MemTotal - MemAvailable`
  （**不是** `free` 的「已用」列，后者不含 buff/cache）；网卡跳过 `lo`
  （中继与被控同机时，回环几百 MB/s 会把「网络负载」算成假的大数）。
- **锁屏判定只认焦点窗口名**，而且是**整串白名单**（`ukui-screensaver-dialog` 等）
  或 `-screensaver-dialog` 结尾。刻意不做「标题含 screensaver 就算锁屏」：
  `Screensaver Preferences` 会被误判，用户的键鼠就被无声掐掉了。漏判的方向是
  「照旧注入」而不是「误掐」——这个方向是选的。
- **不依赖 `xdotool`**：最小安装的麒麟不带它，一条命令失败就等于「永远探不到锁屏」。
  用 `XGetInputFocus` + `XFetchName`（空则 `_NET_WM_NAME`/UTF8_STRING）直连，
  和 XTest 后端共用同一个 `#[link(name="X11")]`。
  （注意：同一个 `XOpenDisplay` 在一个 crate 里声明两次、各带一个不透明 `Display`
  类型，rustc 会报 *signature doesn't match the previous declaration* ——
  `probe/x11.rs` 复用 `input::x11::Display`。）
- **锁屏时输入在进 `Mapper` 之前丢**。丢在 Mapper 之后会留下「按下被丢掉、松开却
  注入了」的半边状态，解锁后就是一个卡住的修饰键。变锁那一刻还要把已按下的键松开。
  只丢输入类：剪贴板/文件/终端/Ping/Bye 锁屏时照样要走（否则解锁前连文件都传不了）。
- **首探必须在任何输入之前**（本轮真机抓到的第二个 bug）：探测只挂在 1 秒定时器上时，
  控制端 `--inject` 的指令比它先到，门禁形同不存在。现在会话开局先同步探一次、
  `input.set_locked()` 之后再进 `control_loop`，并把 `SystemState` 作为控制通道的
  **第一帧**发出去（控制端因此能在发第一条键鼠之前就知道对面锁着）。

#### 控制端：`HostStatus` + `--status`

- `HostStatus`（原子量）存 locked / CPU×100 / 内存 MB / 指标条数 / 状态变化次数 /
  锁屏丢弃数；`cpu` 存整数百分比×100（`f32` 进 `AtomicU64` 要 `to_bits`，读的时候
  还得自己解释）。`NaN` 与越界都夹住 —— 标题里出现 `NaN%` 比不显示更糟。
- **锁屏时 `InputSender::send` 直接短路并返回 `true`**：返回 `false` 是「通道坏了」，
  两者混成一个数会让窗口误以为连接断了。
- 窗口标题：`被控端 CPU 12% · 内存 3.4/8.0 GB`，锁屏时追加 `已锁屏（键鼠已暂停）`
  （放最后 —— 它是「为什么点不动」的直接答案）。
- **`--status <秒>`**：只连控制通道，每秒一行，跑完打汇总。存在的理由同 `--inject`：
  这两条消息必须能机器验证，不能靠人眼看标题。`--inject` 也会在发键鼠前先等这条
  状态（最多 500ms），锁屏时明说「按策略不发」。

#### 实测（`docs/milestones/M6-指标与锁屏状态-2026-10-25.txt`）

```
本机：373 passed / 0 failed / 1 ignored（基线 353，+20）、0 警告；release 0 警告
真机：locked=true has_state=true cpu=0.8–0.9% mem=3792–3800/32025MB metrics=5 changes=1
      独立参照（同窗口直接差分 /proc/stat）：1.00% / 0.85% / 2.00% / 0.72%
      free -m 总计 32025 可用 28215 ⇒ 总-可用 = 3810MB（上报 3792–3800）
旧客户端不认状态直接 --inject：dropped_locked=3 / events=0 / 指针 x:700 y:400 未动
画面（顺带核对 M2）：29.6 fps、丢帧 0、首帧 241ms、相关系数 0.9991 / MAE 0.54%
```

**未验**：解锁后的 `locked=true → false` 切换（这台机器的 `:0` 一直锁着，无人值守
的夜里既不能也不该去解锁）。这条由本机端到端测试覆盖，真机只验了 locked=true 一侧。

### M6 加固：断线重连与注册身份（已完成 · 真机实测 2026-10-25）

真机的升级路径是「重装 + 重启服务」。中继一重启，被控端那条会合连接就断，而旧实现
`run_forever` 直接 `return Err`：进程退出，全靠 systemd `Restart=always` 拉起。两个现场后果：

- **手工跑起来的机器没有 systemd**（`run.sh host`）：中继重启一次，这台机器就再也不上线；
- **「重新拉起」=「重新注册」**，而服务端那时旧注册可能还没被读端回收，
  `handle_host_register` 里「这个 ID 已有人注册就换新 ID」会把**自己的 ID** 当成别人的 ——
  被控端把新 ID 写回 `host.json`，现场手里那个「设备 ID + 口令」当场失效。

但那条「不许顶掉在线设备」的规则本身是对的（否则局域网里谁都能把真机踢下线，
`tests/rvclient.rs::live_duplicate_registration_does_not_steal_the_id` 钉着）。
两者要同时成立，只能让被控端**证明自己是这台机器**：

- **注册令牌**：`host.json` 里的 `reg_token`（与口令无关的随机值；老配置在 `load_or_create`
  时补一个并落盘）。注册时明文带上，服务端只存 `SHA-256("uulan-reg-token-v1"‖token)`。
  判定三条：ID 空着 → 照收；已注册且令牌哈希一致 → 同一台机器重连，照收并**替换**旧注册；
  已注册但令牌不符/没给 → 不许顶，分配新 ID 并写清原因。
- **令牌不是凭据**：泄漏只意味着别人能占用某个设备 ID（连不进来、解不开任何通道），
  而且它与口令无关，所以不构成口令的离线校验器 —— 与 PAKE 的设计目的一致。
- **注册表卫生**（顺手修的两个真 bug）：注册成功后必须 `drop` 掉局部的 tx，
  否则替换旧注册时它的推送通道永远不关，旧连接变成一条谁也连不到的僵尸注册；
  `Device` 加注册流水号，收尾时只摘「还是我这条」，否则旧连接的收尾会把刚上线的新注册删掉
  （现象：重连成功、随即显示离线）。
- **被控端重连循环**：原 `run_forever` 的函数体搬进 `serve_relay`，外层按
  **1/2/4/8/15/30 秒封顶**退避重试，注册成功即清零。Ctrl-C 处理器提到 `main` 只装一次
  （以前它挂在「一条连接的寿命」里，重连就会重复装一堆）。「已上线」凭据横幅只打首条连接。
- **控制端只补「连接阶段」重试**（`--connect-retry`，默认 3，间隔 1/2/4/8 s）：
  **只对连不上重试**（`RvError::Io` / 超时）。口令错、被锁、被控端忙不重试（那是往对方失败
  计数器里加数）；设备不在线、ID 不合法、协议不匹配也不重试（再试一百次也是同一句话，
  只会让人多等 7 秒）。实测：设备 ID 打错 199 ms 就报错；中继端口不通时按 1 s、2 s 重试后报错。

真机（2026-10-25，麒麟 V10 SP1，`ssh kylin`）：中继被 KILL 一次 → host **MainPID 不变、
NRestarts 不变**，1 s + 2 s 退避后「已用原有设备 ID 上线」，约 3 秒恢复；
连续 KILL 4 次（间隔 3 s）同样保持同一 PID / 同一设备 ID。重连后取帧 29.6 fps / 丢帧 0 /
首帧 229 ms / 与真机截图相关系数 0.9981、MAE 0.71%。详见
`docs/milestones/断线重连-2026-10-25.txt`。

**还没做**：控制端**会话中途**的自动接回。被控端/中继在会话中掉线时，客户端把「画面流结束」
当正常收尾（实测 8.8 秒退出、退出码 0）。要做得多花一步：把 winit 事件循环改成
「只建一次 + 会话槽可替换」，否则没法在一个进程里重建窗口。

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
- **终端尺寸跟随是轮询的（300 ms）**：控制端每 300 ms 问一次本机窗口大小，变了就发一条
  `term_resize`；显式给了 `--term-size` 就不跟随。所以改窗口后远端 `vim`/`top` 最多晚
  300 ms 重排。**无控制台的会话里问不到大小**（stdout 是管道 / 服务里跑），这时自然不跟随，
  不是故障。行列只跟窗口走，不跟字号/缩放走。
- **Windows 控制端的终端是行模式**（没接 `SetConsoleMode`）：方向键、Ctrl-C 要等回车才发出去。
  快捷键（Ctrl-C 等）在这条路径上不可靠，排障时优先用 `--term` 打命令而不是开 `top`。
- **PAKE 是本仓库自己写的实现**（`crates/uulan-protocol/src/pake.rs`），没有第三方审计。
  用到的都是成熟原语（Ristretto255 哈希到群、常数标量乘、SHA-512/256 域分隔），
  但「自研密码学」这件事要写在明处：现场安全评估时按未审计实现对待。
  PAKE 之后主动 MITM 的中继既解不开流量，也**不能**离线撞口令（每猜一次必须在线连一次），
  实际强度取决于口令本身与 `AUTH_MAX_FAILURES` 的锁定策略。
- **`--password` 仍会留在 `ps` / shell 历史里**：控制端优先用 `--password-file`（首行）
  或环境变量 `UULAN_PASSWORD`。被控端的口令在 `~/.config/uulan/host.json`，不在命令行上。
- **改前的旧日志里仍可能有明文票据**：票据脱敏从 0.3.x 起生效，之前写进 journal 的
  32 字符票据**改不回来**（只活 60 秒，但会一直留在日志里）。现场可以用
  `journalctl --user --vacuum-time=1d` 清一遍，或者接受它。
- **加密是强制的**（`PROTO_VERSION = 3` / `CRYPTO_VERSION = 2`），不与旧端互通。三件同批部署即可，不需要灰度开关。
- **锁屏判定只认「输入焦点窗口名」**（`probe::LOCK_WINDOW_NAMES` 白名单 +
  `-screensaver-dialog` 后缀）。UKUI 实测有效；换一款锁屏程序、或它改成自绘全屏
  而不改窗口名就会漏判。漏判的方向是「照旧注入」而不是「误掐键鼠」——刻意的。
- **`Metrics.rtt_ms` 恒为 0**：RTT 是控制端侧的观测量（它自己算 Ping→Pong 往返），
  被控端没有可比的时钟，所以如实填 0 而不是编一个数；控制端标题目前也不显示它。
  网速（`net_rx/net_tx`）被控端已经在发，控制端还没显示（避免标题过长）。
- **被控端进程退出后，它写进 X11 剪贴板的内容随之失效** —— 这是 X11 的固有行为
  （selection 归所有者进程所有），不是缺陷；控制端退出同理，Windows 一侧会保留最后一次写的内容。

- **控制端会话中途断线不会自动接回**：只有「连接阶段」有重试（`--connect-retry`）。
  会话里被控端/中继掉线时客户端按「画面流结束」收尾退出（退出码 0），窗口模式随之关闭。
  重连 = 重新握手 + 重新建解码器 + 重新读剪贴板基线，需要把窗口事件循环改成可重复使用，
  是独立的一项工作。
- **注册令牌（`host.json` 的 `reg_token`）是身份标识，不是凭据**：它明文走注册
  （发生在加密握手之前），服务端只存哈希。拿到它只能占用某个设备 ID。
  老版本被控端没有它，重连时仍拿不到自己在线中的 ID（行为与改动前一致），两端同批升级即可。
- **中继没做「半开连接」的超时踢人**：在线判定只看推送通道是否关闭 + 有没有 Ping，
  没有基于 `last_seen` 的超时清理。网络被切成半开时服务端可能长时间认为那台设备还在线。
  被控端现在会自己退避重连并拿回 ID，所以现场是可恢复的；但「多久判死」还没定策略。
- **整机重启后的开机自启仍未验**：没人值守时重启不了真机（polkit 对远程会话要求认证、
  mis 无免密 sudo）。这条必须有人到机器跟前做，别在报告里写成已验。

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