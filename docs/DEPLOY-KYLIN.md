# 在银河麒麟 V10 SP1 真机上部署与实测

本文记录 uulan 第一次在**真实麒麟机器**上跑通的全过程：机器实况、编译方法、
实测数字、以及踩到的每一个坑和它的根因。以后往第二台、第三台麒麟上部署时
照着走即可。

---

## 1. 实测机器实况

| 项目 | 实测值 | 备注 |
|---|---|---|
| 系统 | 银河麒麟桌面操作系统 V10 (SP1) | `Build 20250117` / `buildid 64983` |
| 内核 | `5.4.18-116-generic #105-KYLINOS` | |
| CPU | **海光 Hygon C86 3250**，8 核 16 线程 | x86_64，正是设计文档里的海光场景 |
| 显卡 | **Glenfly Arise1020**（格兰菲，国产 GPU） | 硬件编码不可指望，走软件 libx264 |
| 内存 | 31 GiB | |
| 磁盘 | `/` 39 G（剩 21 G）、`/home` 916 G（剩 848 G） | |
| 桌面 | **X11**，X.Org 1.20.8 | `Xorg -core :0 -seat seat0 -auth /var/run/lightdm/root/:0` |
| 桌面环境 | **UKUI**（`ukui-session` + `ukui-kwin_x11`） | `ukui-kwin_x11` 而非 `_wayland`，确认是 X11 |
| 登录器 | lightdm | |
| 分辨率 | 1920x1080 @ 60Hz（VGA-0） | |
| glibc | `2.31-0kylin9.1k21.6` | Ubuntu 20.04 世代 |
| ffmpeg | **4.2.2-1kylin1k23.8** | 自带 `libx264`/`libx265`，见下 |
| 虚拟化 | 跑在 **VMware** 里 | 902 端口是 VMware authd，**5900 的 VNC 是 VMware MKS 控制台，不是桌面共享** |

### ffmpeg 能力实测

```
libx264    ✅  （关键：麒麟这个构建没砍掉）
libx265    ✅
h264_vaapi ⚠️  有封装，但 Glenfly 显卡基本跑不起来
h264_nvenc ⚠️  有封装，无 N 卡
x11grab    ✅
kmsgrab    ✅  （需 root）
```

### 一条容易误解的现象

机器上 5900 端口开放，但它**不是**麒麟的远程桌面，而是 VMware 宿主给这台虚拟机
的控制台通道（`220 VMware Authentication Daemon ... MKSDisplayProtocol:VNC`，
RFB 003.007 只提供 TLS/VNC 两种认证，且握手是 VMware 私有变形）。
所以「内网里已经有现成的远程桌面可用」这条退路并不存在。

---

## 2. 编译：直接在麒麟本机编，不要交叉编译

**结论：在目标机上原生编译。** 理由：

- 本机 Windows 上做 Linux 交叉编译需要一套 Linux 链接器。仓库里曾经留过一份
  zig cc 的 `.cargo/config.toml`，但 zig 根本没装、`cargo-zigbuild` 也没装，
  而且那份配置指向 Windows 路径，**在 Linux 上做原生构建时会直接报错**。
  留着一个跑不通的方案去破坏能走通的路是负资产，已删除。
- 目标机能出外网（实测 `curl https://mirrors.aliyun.com` 通，
  apt 源为 `archive.kylinos.cn`），装工具链的成本很低。
- 16 线程编译整个 workspace 只要 **23 秒**（增量）/ 33 秒（首次）。

### 步骤

```bash
# 1. 装 Rust（国内镜像；--no-modify-path 不碰 ~/.profile）
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --no-modify-path

# 2. 配 crates 镜像（写 ~/.cargo/config.toml，属用户目录）
cat > ~/.cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = 'rsproxy-sparse'
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
[net]
git-fetch-with-cli = true
EOF

# 3. 编译
export PATH=$HOME/.cargo/bin:$PATH
cd ~/.local/uulan/src && cargo build --release
```

产物放在 `~/.local/uulan/src/target/release/`，三个静态链接度很高的 ELF：

```
uulan-server  1.6 MB
uulan-host    2.0 MB
uulan-client  1.8 MB
```

`ldd` 只需要 `libc/libm/libdl/libpthread/libgcc_s`，没有任何第三方运行时依赖。

### 非交互 SSH 的陷阱

`ssh mis@host 'command -v cargo'` 会返回「没装」，因为**非交互 SSH 不读
`~/.profile`**，PATH 里没有 `~/.cargo/bin`。必须显式：

```bash
export PATH=$HOME/.cargo/bin:$PATH
```

排查「Rust 到底装没装」时别被这个误导 —— 我第一次就误判成「机器上早就有 Rust」。

---

## 3. 采集：SSH 进来能不能抓 :0

能，而且**不用手动指定 XAUTHORITY**：

```bash
export DISPLAY=:0
xdpyinfo | head -3      # 直接就能通
```

原因：X 的 cookie 在 `~/.Xauthority`（`mis` 自己的会话），而 X 客户端在
`XAUTHORITY` 未设置时会自动读 `$HOME/.Xauthority`。

但注意：**`import -window root /tmp/a.png` 必须在 `DISPLAY=:0` 的环境里执行**。
我一开始把 `DISPLAY=:0` 只加在前一条命令上，截图那条没带，报
`unable to open X server ''` —— 那是命令写错，不是权限问题。

实测采集参数（与 `crates/uulan-media/src/ffmpeg.rs` 的 `encode_args` 逐字一致）：

```bash
ffmpeg -hide_banner -loglevel error -nostdin \
  -f x11grab -draw_mouse 0 -framerate 25 -video_size 1920x1080 -i :0.0+0,0 \
  -c:v libx264 -preset ultrafast -tune zerolatency -aud 1 -g 12 \
  -pix_fmt yuv420p -crf 23 -f h264 out.h264
```

在麒麟的 ffmpeg 4.2.2 上**完全可用**，包括 `-aud 1`（这点专门验过）。

### 一个必须知道的事实：一帧有 16 个 slice

拆开码流数 NAL：

```
2 秒 / 25fps 素材 50 帧：
  AUD      50   ← 一帧一个，与帧数严格相等
  SPS/PPS   5   ← 对应 5 个关键帧，-g 12 生效
  IDR 片   80   ← 80 / 5 keyframe = 16 片/帧
  非IDR片 720
```

`-tune zerolatency` 会打开 x264 的**分片线程**，一帧被切成 16 个 slice（= 线程数）。
这直接决定了 AU 切分的做法：

- **必须靠 AUD 切帧**（`-aud 1` 保证每帧一个 AUD）。
- 兜底方案里的 `first_mb_in_slice == 0` 在这种流上会把**一帧劈成 16 个假 AU**，
  只能作为 AUD 缺失时的最后手段，绝不能当主判据。

---

## 4. 实测数字

链路：麒麟真机（X11 采集 → libx264 编码）→ 中继盲转发 → Windows 控制端（ffmpeg 解码）

| 指标 | 数值 | 说明 |
|---|---|---|
| 分辨率 | 1920x1080 @ 30fps | |
| 首帧延迟 | **216 ~ 240 ms** | 优化前是 **1769 ms**，见 §6 |
| 解出帧数 | 60 / 60 | 零丢帧 |
| 码率 | 约 10 ~ 11 Mbps | CRF 23 + zerolatency，桌面场景 |
| 关键帧间隔 | 15 帧（fps/2） | |
| 画面正确性 | MAE **1.23/255（0.48%）**，相关系数 **0.9995** | 与本机 `import` 截图逐像素比对 |
| 被控端收到 Incoming → 开始推流 | **3.5 ms** | 服务端到被控端的调度开销可以忽略 |

「画面正确性」那一行是这样测的：把控制端 `--save-frame` 存出的帧和麒麟本机上
`import -window root` 直接截的图，都缩到 480x270 后逐通道比。0.48% 的差异就是
H.264 有损压缩本身的代价，说明**送过去的就是真实桌面，不是花屏**。

---

## 5. 部署与运行

```bash
# 源码传过去（排除 target，保留远端已有的 target 做增量编译）
tar czf uulan-src.tgz --exclude=target --exclude=.git --exclude='*.log' .
scp uulan-src.tgz mis@192.168.223.15:~/
ssh mis@192.168.223.15 'mkdir -p ~/.local/uulan/src && tar xzf ~/uulan-src.tgz -C ~/.local/uulan/src'

# 编译
ssh mis@192.168.223.15 'export PATH=$HOME/.cargo/bin:$PATH; cd ~/.local/uulan/src && cargo build --release'

# 中继（0.0.0.0 才能让别的机器连）
~/.local/uulan/src/target/release/uulan-server --bind 0.0.0.0:21116 --state ~/.local/uulan/devices.json

# 被控端（必须带上 DISPLAY）
export DISPLAY=:0
~/.local/uulan/src/target/release/uulan-host --server 192.168.223.15:21116 --name 麒麟真机
```

仓库里的 `tools/kylin/run.sh` 就是这套的封装（`server` / `host` / `stop` / `status`）。

### 进程管理的坑：别用 `pkill -f`

```bash
pkill -f uulan-server      # ❌ 会把自己也杀掉
pkill -x uulan-server      # ✅ 按进程名精确匹配
```

`pkill -f` 匹配**整条命令行**，而你执行这条 pkill 的 shell，其命令行里正好就含
`uulan-server` 这个字符串 —— 于是它把自己也杀了。表现出来是「命令毫无输出地消失」，
非常难查。

---

## 6. 踩到的坑与根因（按严重程度排序）

### P0：控制端一连接，被控端自己退出

真机上暴露的第一个致命 bug：

```
WARN uulan_host: 心跳失败 error=期望 Pong，收到 Incoming { session: "...", client_name: "Windows控制端" }
Error: 期望 Pong，收到 Incoming {...}
```

被控端上线后，同一条会合连接上同时跑着两类消息：服务端主动推的 `Incoming`，
和心跳的 `Pong`。而旧实现是「发 Ping 就死等 Pong，来了别的就报协议违规」——
**报错是必然的，只是早晚**。

更隐蔽的是第二层根因：主循环用 `tokio::select!` 一边等 `recv()` 一边等心跳定时器。
`select!` 会**丢弃没被选中的 future**，于是读到一半的帧被砍断、已进缓冲的字节
永久丢失（`read_exact` 不是取消安全的）。这一层没法靠"加容错"绕过去。

**修法**：`RvConn::into_split()` 把连接拆成读写两半。读端由一个循环独占、永不取消；
写端独立成任务、只写不读；`Pong` 由读端记录，没有人再死等它。
回归测试见 `crates/uulan-server/tests/heartbeat_interleave.rs`（3 条）。

### P1：首帧要等 1.7 秒

`decode_args` 里没有延迟相关参数，ffmpeg 从管道读裸流时默认先做输入分析
（`probesize` 5 MB、`analyzeduration` 5 秒），一直攒数据才吐第一帧。

**修法**：加 `-probesize 32 -analyzeduration 0 -flags low_delay`，实测零代价。
首帧 1769 ms → 216 ms，快 8 倍。

**但 `-fflags nobuffer` 不能加**（看起来很像该加，实测有代价）：

| 解码参数 | 10 帧素材解出 |
|---|---|
| 基线（原参数） | 10 帧 |
| 只加 `probesize32/analyzeduration0` | **10 帧** ✅ 零代价 |
| 只加 `low_delay` | **10 帧** ✅ 零代价 |
| 只加 `nobuffer` | **0 帧** ❌ 从文件读直接不出帧 |
| `probesize32 + nobuffer` | **9 帧** ❌ 末尾稳定丢一帧 |

### P2：首次启动打出一条假错误

中继第一次运行时台账文件还不存在，`DeviceBook::load` 把「文件不存在」当成
「读取台账失败」打 WARN，运维会以为装坏了。修法：`NotFound` 单独处理，走 debug 日志。

### 其它（不致命但要记住）

- **XML/路径**：git-bash 里的 `/d/...` 和 `/tmp` 是 MSYS 路径，**原生 Windows 程序
  不认**（报「拒绝访问」）。凡是传给原生 exe 的路径一律先 `cygpath -m` 转成 `D:/...`。
- **`pkill -f` 自杀**：见 §5。
- **`%TEMP%` 写不进去**：本机新编译的进程往 `%TEMP%` 写文件报「拒绝访问 (os error 5)」，
  疑似组策略/杀软。测试与落盘目录改到工作区内（`target/` 下）。
- **非交互 SSH 无 PATH**：见 §2。

---

## 7. 还没做的（下一步）

| 功能 | 状态 |
|---|---|
| 窗口界面（winit + softbuffer） | ✅ 已完成（默认开窗口，`--headless` 保留） |
| 键鼠注入（XTest） | ✅ 两端已接线，本机全链路测试通过；**真机未验** |
| 键鼠注入（uinput 兜底） | 未做（X11 会话走 XTest 已够，uinput 只有 Wayland/无 X 时才需要） |
| 剪贴板双向 | ✅ 文本已实现（Win32 + X11 后端，本机全链路 + 真 Win32 剪贴板实测）；**X11 一侧真机未验** |
| 文件传输 / 终端 / 端口映射 | 未做 |
| 分辨率变化后重建解码器 | 未做（`xdpyinfo` 只在启动时探测一次） |
| `actual_fps` 按秒滑动平均 | 未做，目前恒为 0 |
| systemd 开机自启 | 未做 |

## 8. 输入注入（M3）在麒麟上的编译与验证

注入后端用 XTest，直接 `#[link(name = "X11")]` / `#[link(name = "Xtst")]` 声明符号，
**不引第三方 crate**（这样整个 crate 仍能在 Windows 上编译和跑测试）。

编译机要先装开发包（运行库有 ≠ 头/符号链接有）：

```
sudo apt install libx11-dev libxtst-dev      # 麒麟 V10 源里就有
cargo build --release
```

验证（在麒麟本机盯着屏幕做，**这台 Windows 开发机上无法验证**）：

```
# 1) 后端是否就绪：日志里必须有这一行，sink 名是 xtest 而不是 none
RUST_LOG=info ./uulan-host --config host.json
#   → INFO uulan_host::session: 键鼠注入后端 sink="xtest"
#   → INFO uulan_host::input::x11: XTest 注入后端已就绪 ... autorepeat_turned_off=true

# 2) 控制端连过去，依次验：鼠标移动（点不准 = 坐标问题）、左键单击、
#    滚轮上下、单个字母、Ctrl+C、按着字母不放看连发速率、中文输入
# 3) 断开后必须满足：
#    - 被控端本机的键盘连发恢复正常（说明 XAutoRepeatOn 还原了）
#    - 没有任何键处于按下状态
#    - 被控端日志有 `键鼠事件统计 events=N inject_errors=0`
#    - 控制端退出时有 `输入上报：移动 … 按键 …（被控端拒绝 0）`
```

`sink="none"` 说明 X 显示打不开或没有 XTest 扩展 —— 这时键鼠一条都注入不了，
但画面照常，容易误判成「能用但点不动」。控制端会持续收到 `InputError`（2 秒节流一条）。

## 9. 剪贴板（M2.5）在麒麟上的验证

剪贴板后端与采集用**同一个显示号**（`$DISPLAY`），因为 selection 是每个 X 显示独立的：
抓的是哪个桌面，剪贴板就必须是哪个桌面的。它只依赖 libX11（已在 §8 装过）。

```
# 1) 后端就绪：日志必须有这一行，backend 是 x11 而不是 none
RUST_LOG=info ./uulan-host --config host.json
#   → INFO uulan_host::session: 剪贴板后端 backend="x11" available=true
#   → INFO uulan_clip::x11: X11 剪贴板后端已就绪

# 2) 四个方向都要过一遍（每条都要真的粘贴出来看）：
#    a. 控制端 Ctrl+C 一段中文 → 麒麟上 Ctrl+V
#    b. 麒麟上 Ctrl+C 一段中文 → 控制端 Ctrl+V
#    c. 麒麟上用鼠标中键粘贴（走 PRIMARY）—— 应当也是那段文字
#    d. 在麒麟上用浏览器复制一段带换行和 emoji 的文本 → 控制端粘贴
# 3) 断开后再看：
#    - 被控端日志 `剪贴板同步统计 sent=… recv=… errors=0 skipped_same=…`
#      （skipped_same 会一直涨，这是正常的：两边的轮询大多数时候看到的都是同一份内容）
#    - 控制端退出时有 `剪贴板同步：发送 … / 接收 …（本机写入失败 0）`
```

几个已知的现场现象，别当成 bug：

- **被控端进程退出后，它写进 X11 剪贴板的那段文字会失效**（selection 归所有者进程）。
- 超过 192 KiB 的内容不同步（日志里 `skipped_oversize` 会涨），不截断。
- 「清空剪贴板」不会传到对端。
- 有些程序（少数 Electron 应用）只提供 `text/html` 不接受 `UTF8_STRING`，
  读出来是 `None`，表现是「复制了但对面没反应」——日志里是「剪贴板所有者拒绝提供 UTF8_STRING」。
