//! 配置与路径。ADR-12：directories 定位标准目录 + TOML 配置。

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::net::IpAddr;

pub const UI_SCALE_OPTIONS: [u16; 4] = [90, 100, 110, 125];
pub const CURRENT_SETTINGS_VERSION: u32 = 1;

/// 网络请求如何处理本机代理/TUN 提供的合成 DNS 地址。
///
/// Strict 保持默认的公网地址校验；TunCompatible 只额外信任代理
/// TUN 常用的 198.18.0.0/15 合成地址，明确写出的私有 IP 仍会被拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NetworkMode {
    #[default]
    Strict,
    TunCompatible,
}

impl NetworkMode {
    pub fn allows_synthetic_ip(self, address: IpAddr) -> bool {
        matches!(self, Self::TunCompatible)
            && matches!(address, IpAddr::V4(address) if {
                let [a, b, _, _] = address.octets();
                a == 198 && (b == 18 || b == 19)
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Missing in legacy TOML; Desktop Runtime upgrades zero to the current version in memory.
    #[serde(default)]
    pub settings_version: u32,
    /// 全局默认抓取间隔（秒）。
    pub default_interval_secs: i64,
    /// 退避基数（秒）：失败后 next = base * 2^fail_count，封顶 cap。
    pub backoff_base_secs: i64,
    pub backoff_cap_secs: i64,
    /// 连续失败达到此次数则自动禁用该源（ADR-11）。
    pub disable_after_failures: i64,
    /// 是否弹桌面通知（ADR-7）。
    pub notifications: bool,
    /// GUI logical-point zoom. Kept in the shared config so it survives restarts.
    pub ui_scale_percent: u16,
    /// 外部网络请求的地址校验模式。
    pub network_mode: NetworkMode,
    pub resource_enrichment: ResourceEnrichmentConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceEnrichmentConfig {
    pub enabled: bool,
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub max_input_chars: usize,
    pub prompt_version: String,
    pub schema_version: String,
}

impl Default for ResourceEnrichmentConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "openai-compatible".into(),
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            max_input_chars: 60_000,
            prompt_version: "resource-v1".into(),
            schema_version: "1".into(),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            settings_version: CURRENT_SETTINGS_VERSION,
            default_interval_secs: 900, // 15 分钟
            backoff_base_secs: 60,
            backoff_cap_secs: 3600,
            disable_after_failures: 10,
            notifications: true,
            ui_scale_percent: 100,
            network_mode: NetworkMode::default(),
            resource_enrichment: ResourceEnrichmentConfig::default(),
        }
    }
}

impl Config {
    pub fn ui_scale_factor(&self) -> f32 {
        let percent = if UI_SCALE_OPTIONS.contains(&self.ui_scale_percent) {
            self.ui_scale_percent
        } else {
            100
        };
        f32::from(percent) / 100.0
    }

    pub fn validate(&self) -> Result<()> {
        if self.settings_version > CURRENT_SETTINGS_VERSION {
            anyhow::bail!(
                "配置版本 {} 高于当前支持的版本 {}",
                self.settings_version,
                CURRENT_SETTINGS_VERSION
            );
        }
        if !UI_SCALE_OPTIONS.contains(&self.ui_scale_percent) {
            anyhow::bail!("不支持的界面缩放比例：{}%", self.ui_scale_percent);
        }
        if self.default_interval_secs <= 0
            || self.backoff_base_secs <= 0
            || self.backoff_cap_secs < self.backoff_base_secs
            || self.disable_after_failures <= 0
        {
            anyhow::bail!("订阅刷新设置无效");
        }
        Ok(())
    }
}

/// 把 "30s" / "5m" / "6h" / "2d" 解析成秒；纯数字按秒。
pub fn parse_duration(s: &str) -> Result<i64> {
    const MAX_DURATION_SECONDS: i64 = 365 * 24 * 60 * 60;
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
    anyhow::ensure!(v >= 0, "时长必须为非负数: {s}");
    let seconds = v
        .checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("时长超出范围: {s}"))?;
    anyhow::ensure!(seconds <= MAX_DURATION_SECONDS, "时长不能超过 365 天: {s}");
    Ok(seconds)
}

#[cfg(test)]
mod tests {
    use super::{Config, NetworkMode, parse_duration};
    #[test]
    fn durations() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("6h").unwrap(), 21600);
        assert_eq!(parse_duration("2d").unwrap(), 172800);
        assert_eq!(parse_duration("45").unwrap(), 45);
        assert!(parse_duration("abc").is_err());
        assert!(parse_duration("-1s").is_err());
        assert!(parse_duration("999999999999999999999d").is_err());
        assert!(parse_duration("366d").is_err());
    }

    #[test]
    fn legacy_config_gets_readable_default_scale_and_invalid_scale_is_safe() {
        let legacy: Config = toml::from_str("notifications = false").unwrap();
        assert_eq!(legacy.ui_scale_percent, 100);
        assert_eq!(legacy.ui_scale_factor(), 1.0);

        let invalid: Config = toml::from_str("ui_scale_percent = 777").unwrap();
        assert_eq!(invalid.ui_scale_factor(), 1.0);
    }

    #[test]
    fn network_mode_defaults_to_strict_and_round_trips() {
        let legacy: Config = toml::from_str("notifications = false").unwrap();
        assert_eq!(legacy.network_mode, NetworkMode::Strict);
        let config = Config {
            network_mode: NetworkMode::TunCompatible,
            ..Config::default()
        };
        let encoded = toml::to_string(&config).unwrap();
        let decoded: Config = toml::from_str(&encoded).unwrap();
        assert_eq!(decoded.network_mode, NetworkMode::TunCompatible);
    }
}
