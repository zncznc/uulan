//! 设备台账：`device_id -> 显示名`。纯 JSON，落在服务器本机。
//!
//! 只存名字，**不存任何凭据**——服务器本来就不知道设备密码。
//! 台账的作用是让「设备列表」在重启后还在，以及让控制端在设备离线时
//! 也能看到它名字（而不是一片空白）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceBook {
    #[serde(default)]
    pub devices: HashMap<String, String>,
}

impl DeviceBook {
    /// 读台账。**文件不存在是正常情况**（第一次启动），直接返回空台账，
    /// 不打警告 —— 否则每台新部署的服务器第一次启动都先报一条假错误，
    /// 运维会以为装坏了。真错误的判断标准是「文件在、但读不动/解析不了」。
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "台账尚不存在，从空台账开始（首次启动属正常）");
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e).with_context(|| format!("读取台账失败 {}", path.display()));
            }
        };
        // 空文件（刚被截断/首次创建）当成空台账，不要报错。
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&text).context("台账不是合法 JSON")
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir).with_context(|| format!("创建目录 {} 失败", dir.display()))?;
            }
        }
        let text = serde_json::to_string_pretty(self)?;
        // 先写临时文件再改名：避免掉电时留下半个 JSON 把台账读坏。
        let tmp: PathBuf = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("写入 {} 失败", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("改名到 {} 失败", path.display()))?;
        Ok(())
    }

    pub fn remember(&mut self, id: &str, name: &str) {
        self.devices.insert(id.to_string(), name.to_string());
    }

    pub fn get(&self, id: &str) -> Option<&str> {
        self.devices.get(id).map(|s| s.as_str())
    }

    pub fn forget(&mut self, id: &str) -> bool {
        self.devices.remove(id).is_some()
    }

    /// 按 ID 排序，方便人看。
    pub fn list(&self) -> Vec<(String, String)> {
        let mut v: Vec<_> = self.devices.iter().map(|(k, n)| (k.clone(), n.clone())).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时目录。
    ///
    /// 刻意不用 `std::env::temp_dir()`：在部分 Windows 环境下，刚编译出来的测试进程
    /// 对 `%TEMP%` 没有写权限（被组策略/杀软挡住），会得到「拒绝访问 (os error 5)」，
    /// 从而把「逻辑没问题」的测试判成失败。这里一律落在项目自己的 target 目录下。
    fn test_dir() -> PathBuf {
        // 集成测试里 cargo 会设置 CARGO_TARGET_TMPDIR
        if let Some(d) = std::env::var_os("CARGO_TARGET_TMPDIR") {
            let p = PathBuf::from(d);
            let _ = std::fs::create_dir_all(&p);
            return p;
        }
        // 单元测试没有那个变量：用 crate 目录上溯到 workspace 的 target/
        let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("testtmp");
        std::fs::create_dir_all(&p).expect("创建测试临时目录失败");
        p
    }

    fn tmp(name: &str) -> PathBuf {
        let mut p = test_dir();
        p.push(format!("uulan-book-test-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn save_then_load_roundtrip() {
        let p = tmp("rt.json");
        let mut b = DeviceBook::default();
        b.remember("123456789", "麒麟-01");
        b.remember("223456789", "麒麟-02");
        b.save(&p).unwrap();

        let back = DeviceBook::load(&p).unwrap();
        assert_eq!(back.get("123456789"), Some("麒麟-01"));
        assert_eq!(back.list().len(), 2);
        // 按 ID 升序
        assert_eq!(back.list()[0].0, "123456789");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn empty_file_is_an_empty_book_not_an_error() {
        let p = tmp("empty.json");
        std::fs::write(&p, "").unwrap();
        assert_eq!(DeviceBook::load(&p).unwrap().devices.len(), 0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn corrupt_file_is_an_error() {
        let p = tmp("corrupt.json");
        std::fs::write(&p, "{not json").unwrap();
        assert!(DeviceBook::load(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn save_leaves_no_tmp_file_behind() {
        let p = tmp("clean.json");
        let mut b = DeviceBook::default();
        b.remember("123456789", "x");
        b.save(&p).unwrap();
        assert!(p.exists());
        assert!(!p.with_extension("json.tmp").exists());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn remember_overwrites_and_forget_removes() {
        let mut b = DeviceBook::default();
        b.remember("123456789", "a");
        b.remember("123456789", "b");
        assert_eq!(b.get("123456789"), Some("b"));
        assert!(b.forget("123456789"));
        assert!(!b.forget("123456789"));
        assert_eq!(b.get("123456789"), None);
    }
}