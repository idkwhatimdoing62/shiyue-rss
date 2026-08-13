//! 配置与路径。ADR-12：directories 定位标准目录 + TOML 配置。

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 全局默认抓取间隔（秒）。
    pub default_interval_secs: i64,
    /// 退避基数（秒）：失败后 next = base * 2^fail_count，封顶 cap。
    pub backoff_base_secs: i64,
    pub backoff_cap_secs: i64,
    /// 连续失败达到此次数则自动禁用该源（ADR-11）。
    pub disable_after_failures: i64,
    /// 是否弹桌面通知（ADR-7）。
    pub notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_interval_secs: 900, // 15 分钟
            backoff_base_secs: 60,
            backoff_cap_secs: 3600,
            disable_after_failures: 10,
            notifications: true,
        }
    }
}

pub struct Paths {
    pub config_file: PathBuf,
    pub db_file: PathBuf,
    pub log_file: PathBuf,
}

impl Paths {
    pub fn resolve() -> Result<Self> {
        // LEGACY COMPATIBILITY: 拾阅的早期开发版本使用 rrss 作为应用
        // 标识。继续读取这个目录，升级后用户的订阅、归档和摘录不会丢失。
        let pd = ProjectDirs::from("", "", "rrss").context("无法确定用户目录")?;
        let config_dir = pd.config_dir().to_path_buf();
        let data_dir = pd.data_local_dir().to_path_buf();
        std::fs::create_dir_all(&config_dir)?;
        std::fs::create_dir_all(&data_dir)?;
        Ok(Self {
            config_file: config_dir.join("config.toml"),
            db_file: data_dir.join("rrss.db"),
            log_file: data_dir.join("rrss.log"),
        })
    }
}

/// 读配置；首次运行写出一份默认 config.toml。
pub fn load(paths: &Paths) -> Result<Config> {
    if paths.config_file.exists() {
        let text = std::fs::read_to_string(&paths.config_file)?;
        Ok(toml::from_str(&text).context("解析 config.toml 失败")?)
    } else {
        let cfg = Config::default();
        std::fs::write(&paths.config_file, toml::to_string_pretty(&cfg)?)?;
        Ok(cfg)
    }
}

/// 把 "30s" / "5m" / "6h" / "2d" 解析成秒；纯数字按秒。
pub fn parse_duration(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, mult) = if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 86400)
    } else {
        (s, 1)
    };
    let v: i64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("无法解析时长: {s}"))?;
    Ok(v * mult)
}

#[cfg(test)]
mod tests {
    use super::parse_duration;
    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("6h").unwrap(), 21600);
        assert_eq!(parse_duration("2d").unwrap(), 172800);
        assert_eq!(parse_duration("45").unwrap(), 45);
        assert!(parse_duration("abc").is_err());
    }
}
