use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

/// 单个 SMTP 账号配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmtpAccount {
    pub id: Uuid,
    /// UI 显示名
    pub name: String,

    /// SMTP 服务器地址
    pub smtp: String,

    /// SMTP 端口
    pub port: u16,

    /// 用户名（通常是邮箱地址）
    pub username: String,

    /// 密码 / 授权码
    pub password: String,

    /// 是否启用 SSL
    pub ssl: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RecipientHistory {
    #[serde(default = "default_history_max")]
    pub max: usize,
    #[serde(default)]
    pub recipients: Vec<String>,
}

impl Default for RecipientHistory {
    fn default() -> Self {
        Self {
            max: default_history_max(),
            recipients: Vec::new(),
        }
    }
}

fn default_history_max() -> usize {
    10
}

fn default_interval_sec() -> u64 {
    20
}

fn default_split_mb() -> u64 {
    10
}

fn default_sevenz_password() -> String {
    "123456".to_string()
}

/// 应用全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// SMTP 账号列表（按顺序切换）
    #[serde(default)]
    pub accounts: Vec<SmtpAccount>,

    /// 旧的单一发送间隔（秒）
    ///
    /// 仅用于向后兼容：老配置文件里只有该字段；当
    /// interval_min_sec / interval_max_sec 未设置时，用它作为默认值。
    /// 保存配置时会同步为 interval_min_sec。
    #[serde(default = "default_interval_sec")]
    pub interval_sec: u64,

    /// 随机发送间隔最小值（秒），None 表示未设置（回落到 interval_sec）
    #[serde(default)]
    pub interval_min_sec: Option<u64>,

    /// 随机发送间隔最大值（秒），None 表示未设置（回落到 interval_min_sec）
    #[serde(default)]
    pub interval_max_sec: Option<u64>,

    /// 7z 分卷大小（MB）
    #[serde(default = "default_split_mb")]
    pub split_mb: u64,

    /// 7z 压缩密码
    #[serde(default = "default_sevenz_password")]
    pub sevenz_password: String,

    /// 是否启用「定时开始发送」
    #[serde(default)]
    pub schedule_enabled: bool,

    /// 定时开始时间（本地时间），支持 "YYYY-MM-DD HH:MM:SS" 或 "YYYY-MM-DD HH:MM"
    #[serde(default)]
    pub schedule_start: String,

    /// 收件人历史
    #[serde(default)]
    pub history: RecipientHistory,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            interval_sec: default_interval_sec(),
            interval_min_sec: None,
            interval_max_sec: None,
            split_mb: default_split_mb(),
            sevenz_password: default_sevenz_password(),
            schedule_enabled: false,
            schedule_start: String::new(),
            history: RecipientHistory::default(),
        }
    }
}

impl AppConfig {
    /// 获取配置文件路径
    pub fn config_path() -> PathBuf {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            // 极端情况下（无法定位 exe）回落到当前目录，避免 unwrap 崩溃
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("."));

        exe.join("config.json")
    }

    /// 从磁盘加载配置
    pub fn load() -> Self {
        let path = Self::config_path();

        if !path.exists() {
            return Self::default();
        }

        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Self>(&text) {
                Ok(cfg) => cfg,
                Err(e) => {
                    eprintln!("配置解析失败，使用默认配置: {e}");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// 保存配置到磁盘
    pub fn save(&self) {
        let path = Self::config_path();

        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        let json = match serde_json::to_string_pretty(self) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("配置序列化失败: {e}");
                return;
            }
        };

        if let Err(e) = fs::write(&path, json) {
            eprintln!("保存配置失败: {e}");
        }
    }

    /// 修正非法/缺失的配置项，返回被修正项的说明（用于写日志）
    ///
    /// - 老配置未填写 min/max 时，回落到 interval_sec
    /// - min > max 时自动交换（并提示）
    /// - split_mb / history.max 至少为 1
    pub fn normalize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();

        if self.split_mb == 0 {
            self.split_mb = default_split_mb();
            notes.push(format!("7z 分卷大小非法（0），已重置为 {} MB", self.split_mb));
        }

        if self.history.max == 0 {
            self.history.max = default_history_max();
        }
        if self.history.recipients.len() > self.history.max {
            self.history.recipients.truncate(self.history.max);
        }

        // min / max 未配置：沿用旧的 interval_sec（老配置文件兼容）
        let legacy = self.interval_sec;
        let min = self.interval_min_sec.unwrap_or(legacy);
        let max = self.interval_max_sec.unwrap_or(min.max(legacy));

        let (min, max) = if min > max {
            notes.push(format!(
                "发送间隔最小值({}s) 大于 最大值({}s)，已自动交换",
                min, max
            ));
            (max, min)
        } else {
            (min, max)
        };

        self.interval_min_sec = Some(min);
        self.interval_max_sec = Some(max);
        // 保证旧字段与新字段语义一致，方便老版本继续读取
        self.interval_sec = min;

        notes
    }

    /// 当前生效的发送间隔区间（秒），保证 min <= max
    pub fn interval_range(&self) -> (u64, u64) {
        let legacy = self.interval_sec;
        let min = self.interval_min_sec.unwrap_or(legacy);
        let max = self.interval_max_sec.unwrap_or(min.max(legacy));
        (min.min(max), max.max(min))
    }

    /// 解析定时开始时间；返回 (是否启用, 开始时间)
    ///
    /// 返回 Err 表示启用了定时但时间格式非法/为空。
    pub fn parse_schedule_start(&self) -> Result<Option<NaiveDateTime>, String> {
        if !self.schedule_enabled {
            return Ok(None);
        }

        let text = self.schedule_start.trim();
        if text.is_empty() {
            return Err("已启用定时开始，但开始时间为空".to_string());
        }

        for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
            if let Ok(dt) = NaiveDateTime::parse_from_str(text, fmt) {
                return Ok(Some(dt));
            }
        }

        Err(format!(
            "无法解析开始时间「{}」，请使用 YYYY-MM-DD HH:MM[:SS] 格式",
            text
        ))
    }
}

impl RecipientHistory {
    pub fn record(&mut self, email: &str) {
        let email = email.trim();
        if email.is_empty() {
            return;
        }

        self.recipients.retain(|e| e != email);
        self.recipients.insert(0, email.to_string());

        if self.recipients.len() > self.max {
            self.recipients.truncate(self.max);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_config_falls_back_to_interval_sec() {
        let mut cfg: AppConfig = serde_json::from_str(r#"{"accounts":[],"interval_sec":30}"#).unwrap();
        cfg.normalize();
        assert_eq!(cfg.interval_range(), (30, 30));
    }

    #[test]
    fn min_greater_than_max_is_swapped() {
        let mut cfg: AppConfig = serde_json::from_str(
            r#"{"accounts":[],"interval_sec":20,"interval_min_sec":9,"interval_max_sec":3}"#,
        )
        .unwrap();
        let notes = cfg.normalize();
        assert_eq!(cfg.interval_range(), (3, 9));
        assert!(!notes.is_empty());
    }

    #[test]
    fn zero_interval_is_explicitly_allowed() {
        // 显式配置 0 秒表示不等待，不能回落到旧的 interval_sec
        let mut cfg: AppConfig = serde_json::from_str(
            r#"{"accounts":[],"interval_sec":20,"interval_min_sec":0,"interval_max_sec":0}"#,
        )
        .unwrap();
        assert!(cfg.normalize().is_empty());
        assert_eq!(cfg.interval_range(), (0, 0));
    }

    #[test]
    fn schedule_parsing() {
        let mut cfg = AppConfig::default();
        assert_eq!(cfg.parse_schedule_start().unwrap(), None);

        cfg.schedule_enabled = true;
        cfg.schedule_start = "2026-10-30 21:30".into();
        assert!(cfg.parse_schedule_start().unwrap().is_some());

        cfg.schedule_start = "not-a-time".into();
        assert!(cfg.parse_schedule_start().is_err());

        cfg.schedule_start = String::new();
        assert!(cfg.parse_schedule_start().is_err());
    }
}
