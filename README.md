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
| 远程桌面（采集→编码→解码→显示） | ✅ | **真麒麟 V10 SP1 双向实测**：Windows 控制端与麒麟控制端各自验过（麒麟侧 29.7 fps / 首帧 256 ms / 丢帧 0） |
| 控制端窗口（winit + softbuffer） | ✅ | 本机 + 真机（开窗 15 s `painted=870`） |
| 键鼠注入（XTest） | ✅ | **真机实测**：`xev` 独立判据，打字 / 拖拽 / 滚轮 / autorepeat 还原全对；物理 `:0` 上也验过 |
| 剪贴板文本双向 | ✅ | 真机 X11 与独立 X 客户端**双向验过**；Win32 侧本机验过（跨机 Windows↔麒麟见「已知限制」） |
| 分辨率变化自动重建解码器 | ✅ | **真机**：会话中途改分辨率不断线，相关系数 0.9999 / 0.9997 |
| 画面参数热更新（`--quality/--fps/--scale`） | ✅ | **真机**：`--quality fast` 码率 9743→1002 kbps、`--fps 15` 实测 15.00 fps、`--scale 0.5` 出 960x540 |
| 文件传输（列/取/送/建/删/改名/哈希 + 目录树） | ✅ | **真机**：6 MiB 往返 sha256 一致；`--put-tree`/`--get-tree` 6/6 逐文件一致，软链接与越界路径被拒 |
| 端口映射（`--forward`，ssh -L 语义） | ✅ | 真二进制实测：256 KiB 单流 + 两条并发流逐字节一致 |
| 终端（PTY，`--term`） | ✅ | **真机 Linux 侧**：真 `/dev/pts`、退出码 7 原样返回；Windows 侧真 ConPTY；尺寸跟随真机验过 |
| 音频（Opus/Ogg，第 4 条加密通道） | ✅ | **真机** PulseAudio monitor → Windows：10.6 s PCM、440 Hz、解码错误 0 |
| 端到端加密（E2E + PAKE） | ✅ | **中继抓包实测**：clipboard 双向 10978 字节里找不到任何明文；PAKE 真机实测 |
| 断线重连 / 半开连接判死 | ✅ | **真机**：中继连 KILL 5 次，host 进程不退、设备 ID 不变，2–3 s 重新上线；半开 49 s 判定离线 |
| 开机自启（systemd） | ✅ | **真机**：`reboot` 后 43 s SSH 恢复、48 s 时中继与被控端双双 active，控制端随即连上 |
| 离线安装包 / 就地升级 | ✅ | **真机**：`install.sh --userland` 一次装通，重装后设备 ID 与配置一字未变 |

`cargo test --workspace`：**454 个测试通过，0 失败，0 警告**（2026-11-11 连跑两轮一致）。

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
# 口令别放命令行（会进 ps / shell 历史）：用 --password-file <文件> 或 UULAN_PASSWORD
./target/release/uulan-client --server <中继IP>:21116 \
  --device <9位设备ID> --password-file ~/.config/uulan/client-pw.txt
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

- 桌面若是 **Wayland** 会话，`x11grab` 抓不到；当前只支持 **X11**。
- **锁屏 / 登录界面（greeter）不出画面**。锁屏时被控端会在进 Mapper 之前丢弃输入并
  松开已按下的键 —— 这是刻意的设计行为，不是缺陷（已有真机端到端验证）。
- **跨机剪贴板（Windows ↔ 麒麟）尚未端到端验过。** 无人值守会话里拿不到 Windows
  剪贴板（`OpenClipboard` 报 Win32 错误 5「拒绝访问」，与 uulan 无关），需在交互式
  桌面会话里跑 `scripts/verify-clipboard-cross.sh`。控制端会**显式打印不可用原因**，
  不会假装发送成功。
- **系统装（root，`/opt/uulan`）已验**（2026-11-12）；用户态装（`install.sh --userland`）更早验过。
  注意两者不能同时跑中继：都会抢 21116，切换前要先停掉用户单元的 `uulan-server.service`。
- **PAKE 是本仓库自研实现（CPace / Ristretto255），没有第三方审计。**
- 端口映射目前是**真二进制在本机全链路**验的，没有单独的麒麟真机记录。

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