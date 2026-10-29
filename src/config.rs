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

#[derive(Debug,Serialize, Deserialize, Default, Clone)]
pub struct RecipientHistory {
    pub max: usize,
    pub recipients: Vec<String>,
}

/// 应用全局配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// SMTP 账号列表（按顺序切换）
    pub accounts: Vec<SmtpAccount>,

    /// 邮件发送间隔（秒）
    pub interval_sec: u64,

    /// 7z 分卷大小（MB）
    pub split_mb: u64,

    /// 7z 压缩密码
    pub sevenz_password: String,

    /// 收件人历史
    pub history: RecipientHistory,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            interval_sec: 20,
            split_mb: 10,
            sevenz_password: "123456".to_string(),
            history: RecipientHistory {
                max: 10,
                recipients: Vec::new(),
            },
        }
    }
}

impl AppConfig {
    /// 获取配置文件路径
    pub fn config_path() -> PathBuf {
        let exe = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| std::env::current_dir().unwrap());

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
