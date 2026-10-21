//! 「此电脑」要列的东西：Windows 的盘符、Linux 的挂载点。
//!
//! 协议里 `:/` 表示「此电脑」。麒麟上能翻到哪些地方完全由挂载决定，
//! 所以这里读 `/proc/self/mounts`；解析单独抽成 [`parse_proc_mounts`]，
//! 这样本机（Windows）也能用一段真实的 mounts 文本把它测住。

/// 一条挂载记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPoint {
    pub path: String,
    pub fstype: String,
}

/// 伪文件系统：列出来对用户毫无意义（`/proc/sys/fs/binfmt_misc` 之类）。
const PSEUDO_FS: &[&str] = &[
    "proc",
    "sysfs",
    "devtmpfs",
    "devpts",
    "tmpfs",
    "cgroup",
    "cgroup2",
    "securityfs",
    "debugfs",
    "tracefs",
    "configfs",
    "fusectl",
    "binfmt_misc",
    "pstore",
    "efivarfs",
    "autofs",
    "hugetlbfs",
    "mqueue",
    "rpc_pipefs",
    "nsfs",
    "bpf",
    "selinuxfs",
    "fuse.gvfsd-fuse",
    "overlay",
];

/// 这些顶层目录下面的挂载点不往「此电脑」里放（系统内部噪音）。
const NOISE_PREFIXES: &[&str] = &["/proc", "/sys", "/dev", "/run"];

/// 解析 `/proc/self/mounts` 的文本（也接受 `/proc/mounts`、`/etc/mtab` 的格式）。
///
/// 格式：`device mountpoint fstype options dump pass`，字段间空格，路径里的空格写成 `\040`。
pub fn parse_proc_mounts(text: &str) -> Vec<MountPoint> {
    let mut out: Vec<MountPoint> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut f = line.split_whitespace();
        let (_dev, Some(mp), Some(fstype)) = (f.next(), f.next(), f.next()) else {
            continue;
        };
        if PSEUDO_FS.contains(&fstype) {
            continue;
        }
        let path = unescape_octal(mp);
        if NOISE_PREFIXES.iter().any(|p| path == *p || path.starts_with(&format!("{p}/"))) {
            continue;
        }
        if out.iter().any(|m| m.path == path) {
            continue;
        }
        out.push(MountPoint { path, fstype: fstype.to_string() });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// `/proc/mounts` 把空格等字符转义成三位八进制（`\040`）。
fn unescape_octal(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        // 只有「反斜杠 + 三个八进制数字」才是转义。先确认这三字节都是 ASCII 数字，
        // 否则 `&s[i+1..i+4]` 可能切在多字节字符中间而 panic。
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0');
            out.push(v as char);
            i += 4;
            continue;
        }
        // 非 ASCII 的 UTF-8 字节按原样拼回去
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// 「此电脑」下要显示的条目名（已排序）。
pub fn list() -> Vec<String> {
    #[cfg(windows)]
    {
        let mut v = Vec::new();
        for letter in b'A'..=b'Z' {
            let root = format!("{}:\\", letter as char);
            if std::fs::metadata(&root).is_ok() {
                v.push(root);
            }
        }
        v
    }
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        let mut v: Vec<String> = parse_proc_mounts(&text).into_iter().map(|m| m.path).collect();
        if !v.iter().any(|p| p == "/") {
            v.insert(0, "/".to_string());
        }
        v
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        vec!["/".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
sysfs /sys sysfs rw,nosuid,nodev,noexec,relatime 0 0
proc /proc proc rw,nosuid,nodev,noexec,relatime 0 0
udev /dev devtmpfs rw,nosuid,relatime,size=8123456k 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw,relatime,fmask=0077 0 0
tmpfs /run tmpfs rw,nosuid,nodev 0 0
/dev/sda1 /data ext4 rw,relatime 0 0
/dev/sdb1 /media/u/My\040Disk ntfs rw,relatime 0 0
/dev/sda1 /data ext4 rw,relatime 0 0
"#;

    #[test]
    fn keeps_real_filesystems_and_drops_pseudo_ones() {
        let m = parse_proc_mounts(SAMPLE);
        let paths: Vec<&str> = m.iter().map(|x| x.path.as_str()).collect();
        assert_eq!(paths, vec!["/", "/boot/efi", "/data", "/media/u/My Disk"]);
    }

    #[test]
    fn duplicate_mounts_are_merged() {
        // 同一个设备挂到同一个点两次（bind mount 常见）只留一条。
        let m = parse_proc_mounts("/dev/sda1 /data ext4 rw 0 0\n/dev/sda1 /data ext4 rw 0 0\n");
        assert_eq!(m.len(), 1);
    }

    #[test]
    fn spaces_in_mount_points_are_unescaped() {
        let m = parse_proc_mounts("/dev/sdb1 /media/u/My\\040Disk ntfs rw 0 0\n");
        assert_eq!(m[0].path, "/media/u/My Disk");
    }

    #[test]
    fn malformed_lines_are_skipped_not_panicked() {
        let m = parse_proc_mounts("garbage\n\n/dev/x\n# comment\n/dev/y /mnt ext4 rw 0 0\n");
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "/mnt");
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert!(parse_proc_mounts("").is_empty());
    }

    #[test]
    fn list_is_non_empty_on_this_machine() {
        // Windows 上至少 C:；Linux 上至少 /。
        assert!(!list().is_empty(), "「此电脑」一个条目都没有，客户端会看到空列表");
    }
}