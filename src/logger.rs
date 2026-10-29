use chrono::Local;
use parking_lot::Mutex;
use std::sync::Arc;

/// 日志级别
#[derive(Clone, Copy)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Info => "INFO",
            LogLevel::Warn => "WARN",
            LogLevel::Error => "ERROR",
        }
    }
}

/// 单条日志
#[derive(Clone)]
pub struct LogItem {
    pub time: String,
    pub level: LogLevel,
    pub message: String,
}

/// Logger 本体
#[derive(Clone)]
pub struct Logger {
    inner: Arc<Mutex<Vec<LogItem>>>,
    max: usize,
}

impl Logger {
    /// 创建 logger，max 为最大日志条数（超过自动丢弃最早的）
    pub fn new(max: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
            max,
        }
    }

    /// 清空日志（开始一次新发送任务时调用）
    pub fn clear(&self) {
        self.inner.lock().clear();
    }

    /// 获取当前日志快照（给 egui 用）
    pub fn snapshot(&self) -> Vec<LogItem> {
        self.inner.lock().clone()
    }

    fn push(&self, level: LogLevel, msg: impl Into<String>) {
        let mut logs = self.inner.lock();

        logs.push(LogItem {
            time: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            level,
            message: msg.into(),
        });

        if logs.len() > self.max {
            let overflow = logs.len() - self.max;
            logs.drain(0..overflow);
        }
    }

    /// INFO
    pub fn info(&self, msg: impl Into<String>) {
        self.push(LogLevel::Info, msg);
    }

    /// WARN
    pub fn warn(&self, msg: impl Into<String>) {
        self.push(LogLevel::Warn, msg);
    }

    /// ERROR
    pub fn error(&self, msg: impl Into<String>) {
        self.push(LogLevel::Error, msg);
    }
    pub fn entries(&self) -> Vec<LogItem> {
        self.inner.lock().clone()
    }
}
