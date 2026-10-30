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

/// 单条日志
#[derive(Clone)]
pub struct LogItem {
    /// 记录时间（HH:MM:SS，发送页日志区显示用）
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

    fn push(&self, level: LogLevel, msg: impl Into<String>) {
        let mut logs = self.inner.lock();

        logs.push(LogItem {
            time: Local::now().format("%H:%M:%S").to_string(),
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

    /// 当前日志快照（UI 渲染用）
    pub fn entries(&self) -> Vec<LogItem> {
        self.inner.lock().clone()
    }
}
