# uulan 现场部署（麒麟 / systemd + X11）

内网物理隔离，没有 apt 源、没有公网，所以**没有 deb/rpm 仓库**：
安装包就是「二进制 + systemd 单元 + 安装脚本」的 tar.gz。

## 一、三步装完

在**一台与被控端同款的麒麟机器上**打包（原生编译，不交叉编译）：

```bash
bash deploy/make-package.sh --version 0.3.0
# → /tmp/uulan-0.3.0-x86_64-linux.tar.gz（含 sha256）

# 现场还需要一台麒麟当控制端时，把控制端一起打进包：
bash deploy/make-package.sh --version 0.3.0 --with-client
# → 同上，但 bin/ 里多一个 uulan-client（麒麟上原生编出来的那份）
```

把包拷到目标机（U 盘 / scp），然后二选一：

```bash
# A. 系统装（root）：/opt/uulan + 中继系统单元 + 被控端用户单元
tar xzf uulan-0.3.0-x86_64-linux.tar.gz && cd uulan-0.3.0-x86_64-linux
sudo bash install.sh --user mis --server 192.168.223.15:21116

# B. 用户态装（普通用户，无 root 口令）：全部在 ~/.local/uulan，两个都是用户单元
bash install.sh --userland --server 192.168.223.15:21116
```

`install.sh` 做的事（幂等，可重复跑）：

1. 二进制进 `<prefix>/bin/`（`uulan-server`、`uulan-host`、`uulan-wait-x`；
   包里有 `uulan-client` 时也一起装，它不是服务、不建单元）；
2. 中继单元：系统装 → `/etc/systemd/system/uulan-server.service`；
   用户态装 → `~/.config/systemd/user/uulan-server.service`；
3. 被控端 → `~/.config/systemd/user/uulan-host.service`（**两种装法都是用户单元**）；
4. `loginctl enable-linger <user>` —— 开机不等登录就起用户管理器；
5. 首次安装生成 `~/.config/uulan/host.json`（随机 12 位密码，0600）。

装完看状态：

```bash
systemctl status uulan-server            # 用户态装改成 systemctl --user
systemctl --user status uulan-host
journalctl --user -u uulan-host -f       # 被控端日志
uulan-host --show                         # 设备 ID / 访问密码（自己在终端里看）
```

## 二、为什么被控端必须用用户单元

| | 中继 | 被控端 |
|---|---|---|
| 要不要 X | 不要 | **要**（`x11grab` 抓 `:0`、XTest 注入、读 `~/.Xauthority`） |
| 能不能 root 跑 | 能（只搬字节） | 不能（root 拿不到用户会话的 X 授权，抓出来是黑屏） |
| 单元位置 | 系统装的 `/etc/systemd/system/`；用户态装的家目录 | `~/.config/systemd/user/` |

被控端用户单元里写死 `DISPLAY=:0`、`XAUTHORITY=%h/.Xauthority`。
`%h` 是 systemd 的用户主目录占位符，装到别的用户名下不用改。

**开机顺序**：用户单元挂在 `default.target`，但 Xorg 可能还没起。
所以 `ExecStartPre=<prefix>/bin/uulan-wait-x 300` 先等 `xdpyinfo` 能打开 `:0`；
超时就退出 1，systemd 按 `Restart=always` 再等一轮 —— **先开机、后登录也能自己追上**，
不需要 autologin。

用户态安装时中继也不能开 `ProtectHome`/`ProtectSystem`：台账 `state/devices.json`
就在家目录里，一 hide 就写不了。系统装的单元模板才带那套沙箱（`uulan-server.service`
vs `uulan-server.user.service`）。

## 三、口令不进日志（这条是硬要求）

`uulan-host` 的启动横幅里，**访问密码只在 stdout 是终端时才打印**；
在 systemd（journal）或重定向到文件时只打印占位提示。
所以 `journalctl --user -u uulan-host` 里搜不到密码。要看密码：

```bash
uulan-host --show
```

（`--show` 是唯一会打印口令的入口，而且必须你自己去敲。）

## 四、卸载

```bash
sudo bash uninstall.sh             # 系统装：停服务、删单元，保留二进制与配置
sudo bash uninstall.sh --purge     # 连二进制、状态、配置一起删
bash uninstall.sh --userland       # 用户态装
```

## 五、现场验收（装完必做）

```bash
# 1) 两端都在跑
systemctl is-active uulan-server                  # 用户态：systemctl --user
systemctl --user is-active uulan-host

# 2) 被控端真的抓到了 :0（不是黑屏）
DISPLAY=:0 xdpyinfo | grep dimensions

# 3) 从控制端取一帧并与真机截图比对（M2 的方法，见 docs/DEPLOY-KYLIN.md）
uulan-client --server 192.168.223.15:21116 --device <ID> --password-file pw.txt \
  --headless --seconds 12 --save-frame D:/uulan-bin/x.ppm
```

## 六、已知限制

- **greeter / 锁屏解锁不支持**：X11 授权拦住，KMS 抓出来是黑屏（设计文档第 2 节第 7 条）。
  被控端必须等用户**已登录**才有画面。
- **Wayland 会话不支持**：麒麟 V10 SP1 默认 X11；若切到 Wayland，`x11grab` 抓不到。
  `DISPLAY=:0` 连不上时 `uulan-wait-x` 会一直重试（`journalctl --user` 能看到）。
- 打包是**原生编译**，产物只保证在构建它的那类机器上跑（glibc 世代要一致）。