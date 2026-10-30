use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::Duration;

/// 发送进度（线程安全）
///
/// UI 线程只读，发送线程写
#[derive(Clone)]
pub struct SendProgress {
    /// 是否暂停
    pub paused: Arc<AtomicBool>,

    /// 是否停止
    pub stopped: Arc<AtomicBool>,

    /// 总文件数（只读）
    pub total_files: u64,

    /// 当前文件索引（1-based，0 表示未开始）
    pub current_file_index: Arc<AtomicU64>,

    /// 当前文件的分卷总数
    pub current_file_total_parts: Arc<AtomicU64>,

    /// 当前文件已发送的分卷数
    pub current_file_sent_parts: Arc<AtomicU64>,

    /// 后台任务是否已经结束（正常结束 / 出错 / 被停止）
    pub finished: Arc<AtomicBool>,

    /// 是否处于「等待定时开始」状态
    pub waiting: Arc<AtomicBool>,

    /// 距离定时开始还剩多少秒（0 表示已到点）
    pub waiting_seconds_left: Arc<AtomicU64>,
}

impl SendProgress {
    /// 创建新的发送进度
    pub fn new(total_files: u64) -> Self {
        Self {
            paused: Arc::new(AtomicBool::new(false)),
            stopped: Arc::new(AtomicBool::new(false)),
            total_files,
            current_file_index: Arc::new(AtomicU64::new(0)),
            current_file_total_parts: Arc::new(AtomicU64::new(0)),
            current_file_sent_parts: Arc::new(AtomicU64::new(0)),
            finished: Arc::new(AtomicBool::new(false)),
            waiting: Arc::new(AtomicBool::new(false)),
            waiting_seconds_left: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 后台任务结束时调用（由发送线程设置）
    pub fn mark_finished(&self) {
        self.waiting.store(false, Ordering::Relaxed);
        self.finished.store(true, Ordering::Relaxed);
    }

    /// 任务是否已经结束
    #[inline]
    pub fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Relaxed)
    }

    /// 进入等待定时开始状态
    pub fn set_waiting(&self, seconds_left: u64) {
        self.waiting_seconds_left
            .store(seconds_left, Ordering::Relaxed);
        self.waiting.store(true, Ordering::Relaxed);
    }

    /// 退出等待状态（到点 / 被停止）
    pub fn clear_waiting(&self) {
        self.waiting.store(false, Ordering::Relaxed);
        self.waiting_seconds_left.store(0, Ordering::Relaxed);
    }

    #[inline]
    pub fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn waiting_left(&self) -> u64 {
        self.waiting_seconds_left.load(Ordering::Relaxed)
    }

    /// 是否被停止
    #[inline]
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// 是否暂停
    #[inline]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// 等待暂停结束（发送线程调用）
    pub fn wait_if_paused(&self) {
        while self.is_paused() && !self.is_stopped() {
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// 重置当前文件的分卷进度
    pub fn reset_file_parts(&self, total_parts: u64) {
        self.current_file_total_parts
            .store(total_parts, Ordering::Relaxed);
        self.current_file_sent_parts
            .store(0, Ordering::Relaxed);
    }

    /// 发送一个分卷后调用
    pub fn inc_sent_part(&self) {
        self.current_file_sent_parts
            .fetch_add(1, Ordering::Relaxed);
    }

    /// 进入下一个文件（1-based）
    pub fn set_current_file(&self, index: u64) {
        self.current_file_index
            .store(index, Ordering::Relaxed);
        self.current_file_total_parts
            .store(0, Ordering::Relaxed);
        self.current_file_sent_parts
            .store(0, Ordering::Relaxed);
    }
}
