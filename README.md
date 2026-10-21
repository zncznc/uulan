# uulan —— 内网环境下的国产化远程控制

自建的远程控制工具，目标是**在完全物理隔离的内网里**，把「远程桌面 + 键鼠 +
剪贴板 + 文件传输 + 终端 + 端口映射」这套能力跑起来，被控端面向**银河麒麟
V10 SP1**，控制端支持麒麟与 Windows。

不依赖任何公网服务：登录、信令、中继全部自己实现，一台内网机器当服务器即可。

```
被控端（麒麟 V10 SP1）          中继（任一内网机器）        控制端（麒麟 / Windows）
  X11 采集 ──► libx264 ──┐                              ┌── ffmpeg 解码 ──► 窗口
  XTest 注入 ◄───────────┤      三条 TCP 通道，盲转发     ├── 键鼠事件
  剪贴板读写 ◄───────────┘   control / video / bulk       └── 剪贴板读写
```

## 功能状态

| 能力 | 状态 | 验证程度 |
|---|---|---|
| 远程桌面（采集→编码→解码→显示） | ✅ | **真麒麟 V10 SP1 实测**：Windows 端看到 1920x1080 桌面，零丢帧 |
| 控制端窗口（winit + softbuffer） | ✅ | 本机全链路实测 |
| 键鼠注入（XTest） | ✅ | 本机全链路实测；**真机行为待验** |
| 剪贴板文本双向 | ✅ | 本机全链路 + 真 Win32 剪贴板实测；X11 后端待真机验 |
| 分辨率变化自动重建解码器 | ✅ | 本机全链路实测 |
| 实测帧率上报 | ✅ | 本机实测 |
| 文件传输（列/取/送/建/删/改名/哈希） | ✅ | 本机全链路 + 真二进制实测（6 MiB 往返 sha256 一致）；麒麟待验 |
| 端口映射（`--forward`，ssh -L 语义） | ✅ | 本机全链路 + 真二进制实测（256 KiB 单流 + 两条并发流逐字节一致）；麒麟待验 |
| 终端（PTY，`--term`） | ✅ | 本机全链路实测（状态机 15 项 + 真 TCP 端到端 4 项）；**真 PTY 行为待麒麟验证** |
| 端到端加密 | ⬜ | 未做（局域网内目前是明文，见「已知限制」） |

`cargo test --workspace`：**301 个测试通过，0 失败，0 警告**。

## 核心设计决策

这些决策都是踩过坑之后的结论，理由记在 [`docs/DESIGN.md`](docs/DESIGN.md)。

- **不用 WebRTC。** 内网延迟 <1ms、没有 NAT，ICE/SDP/DTLS/SRTP 全是纯负担。
  改成三条独立 TCP 通道：`control`（键鼠/剪贴板）、`video`、`bulk`（文件 + 端口映射，按 id 多路复用），
  互不阻塞 —— 文件传输的大包不会堵住输入事件，视频丢帧不会卡住键鼠。
- **不链接 libavcodec。** 走 FFmpeg CLI 子进程。代价是管道拷贝，收益是被控端
  不需要 pkg-config 和一堆开发头文件 —— 麒麟源里本来就有 ffmpeg。
- **中继是**盲转发**。** 服务器不认识控制消息、不看视频帧、不碰文件内容，
  只做 `copy_bidirectional`。设备密码由被控端校验，服务器被拿下也拿不到屏幕或凭据。
- **AU 切分靠 AUD。** 编码端显式 `-aud 1`；实测一帧会被切成 16 个 slice
  （zerolatency 的分片线程），所以兜底的 `first_mb_in_slice == 0` 绝不能当主判据，
  否则一帧会被劈成 16 个假 AU。
- **会合连接的读写必须分开。** `select!` 会丢弃未选中的 future，而读帧不是
  取消安全的；心跳若「发 Ping 就死等 Pong」，必然撞上插进来的服务端推送。
  详见 [`docs/DEPLOY-KYLIN.md`](docs/DEPLOY-KYLIN.md) 里那个 P0 缺陷。

## 在麒麟上构建与运行

目标机需要能出网一次（装工具链），之后可完全离线运行。

```bash
# 1. 装 Rust（国内镜像；--no-modify-path 不动你的 shell 配置）
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path

# 2. crates 镜像
mkdir -p ~/.cargo && cat > ~/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
EOF

# 3. 编译（16 线程机器上约 25 秒）
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release
```

```bash
# 中继（0.0.0.0 才能让别的机器连上）
./target/release/uulan-server --bind 0.0.0.0:21116 --state ./devices.json

# 被控端（必须有 DISPLAY）
export DISPLAY=:0
./target/release/uulan-host --server <中继IP>:21116 --name 我的电脑

# 控制端（图形窗口；--headless 走纯命令行，适合脚本）
./target/release/uulan-client --server <中继IP>:21116 \
  --device <9位设备ID> --password <访问密码>
```

首次运行被控端会自动生成 12 位访问密码并落盘（权限 0600），设备 ID 由中继分配。
把「设备 ID + 密码」给控制端的人即可连上。

## 实测数据（真麒麟 V10 SP1）

环境：海光 C86 3250（8核16线程）+ Glenfly Arise1020，X11 + UKUI，
ffmpeg 4.2.2-1kylin1k23.8，glibc 2.31。

| 指标 | 数值 |
|---|---|
| 首帧延迟 | **216 ms**（优化前 1769 ms） |
| 画面 | 1920x1080 @ 30fps，零丢帧，约 10 Mbps |
| 画面正确性 | 与实机截图逐像素比对：平均误差 0.48%，相关系数 0.9995 |

## 已知限制

- **局域网内流量目前是明文**（H.264 + JSON）。设计上视频本身有编码保护，
  但键鼠、剪贴板、密码是明文传输 —— **不要跨不可信网段使用**，端到端加密在 M6。
- 桌面若是 **Wayland** 会话，`x11grab` 抓不到；当前只支持 X11。
- 锁屏 / 登录界面（greeter）不支持。
- 键鼠注入、剪贴板、分辨率重建的真机（麒麟）行为尚未验证，只有本机全链路实测。
- 无安装包、无 systemd 开机自启。

## 文档

| 文件 | 内容 |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | 总体设计、协议、为什么不用 WebRTC/GTK、里程碑 |
| [`docs/DEPLOY-KYLIN.md`](docs/DEPLOY-KYLIN.md) | 真机部署全过程、实测数字、每一个坑的根因 |
| [`docs/milestones/`](docs/milestones/) | 各里程碑的实测记录 |

## 与参考项目的关系

设计阶段参考了 [`VallierDev/uu-remote-linux`](https://github.com/VallierDev/uu-remote-linux)
（非官方 UU 远程 Linux 被控端，GPL-3.0）的**技术思路与踩坑记录**。

那个项目的登录、信令、ICE/TURN 全部依赖网易云服务，在物理隔离网里连扫码登录都
出不来，所以本项目**没有移植它的代码**，而是自建了信令与中继，架构也完全不同
（无 WebRTC、无 GTK、无 C 引擎）。仅复用了「什么问题会咬人」这一层经验。

## 许可

**MIT**，全文见 [`LICENSE`](LICENSE)。`Cargo.toml` 的
`[workspace.package] license = "MIT"` 与之一致，六个 crate 全部继承该声明。

### 为什么可以选 MIT（而不是被迫用 GPL）

本项目是**独立实现**，与参考项目之间不存在代码复用。这不是自我声明，是比对过的：

| 比对项 | 结果 |
|---|---|
| 双方 Rust 源文件 | 本项目 42 个 / 参考项目 264 个 |
| 「有实质内容」的代码行（≥45 字符、非注释/导入/括号） | 本项目 1914 种 / 参考项目 12924 种 |
| **完全相同的行** | **3 条**，且全是一行通用写法（`tokio::time::sleep(Duration::from_millis(N)).await;`），不构成受保护的表达 |
| 共享的字符串字面量与函数名 | 46 个，**逐条核对全部是第三方接口名**：ffmpeg 自己的命令行参数（`-draw_mouse` `-probesize` `-pix_fmt` `-x264-params` …）、VAAPI 标准设备节点 `/dev/dri/renderD128`、编码器名 `h264_nvenc`、通用常量 `127.0.0.1`、环境变量 `XDG_CONFIG_HOME`、PPM 文件格式头 |

也就是说：任何人用 FFmpeg 做抓屏编码，都必然写出同一批参数名 —— 这些字符串
既不属于本项目也不属于参考项目，而是 FFmpeg 的公开接口。

设计阶段确实读过参考项目的 README、AGENTS 与踩坑记录，但学到的都是**思想与方法**
（例如「GTK 窗口在锁屏时会停止重绘」「X11 按键连发期间要关掉 autorepeat」）。
思想、方法与事实不受版权保护，这与抄代码是两件事。

> 说明：以上是工程级的代码比对，不是法律意见。如需对外商业化分发，建议自行咨询律师。

### 依赖许可证

190 个依赖全部为宽松许可（MIT / Apache-2.0 / BSD / Zlib / ISC / Unlicense），
**没有 AGPL、没有未知许可、没有传染性条款**，与 MIT 完全兼容。