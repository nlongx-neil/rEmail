use std::path::{Path, PathBuf};
use std::process::Command;
use std::fs;
use std::io;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::config::AppConfig;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
/// 7z 分卷结果
#[derive(Debug)]
pub struct SevenZResult {
    /// 分卷文件列表（按顺序）
    pub parts: Vec<PathBuf>,
}

/// 使用 7z 进行分卷压缩
///
/// - 输入：原始文件
/// - 输出：
///   xxx.7z.001
///   xxx.7z.002
///   ...
///
/// 规则：
/// - 如果文件 <= split_mb，返回单文件（原文件）
/// - 如果 > split_mb，生成分卷并返回分卷路径
pub fn split_if_needed(
    input: &Path,
    work_dir: &Path,
    config: &AppConfig,
) -> io::Result<SevenZResult> {
    let meta = fs::metadata(input)?;
    let size_mb = meta.len() / 1024 / 1024;
    let split_mb = config.split_mb.max(1);

    // 不需要分卷
    if size_mb <= split_mb {
        return Ok(SevenZResult {
            parts: vec![input.to_path_buf()],
        });
    }

    // 输出文件名
    let base_name = input
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "非法文件名"))?;

    let output_7z = work_dir.join(format!("{}.7z", base_name));

    // 构造 7z 命令
    let mut cmd = Command::new(sevenz_path()?);
    cmd.arg("a")
        .arg(&output_7z)
        .arg(input)
        .arg(format!("-v{}m", split_mb))
        .arg("-y");
    // 密码为空时不能传 "-p"（7z 会进入交互式输入），此时按不加密处理
    if !config.sevenz_password.is_empty() {
        cmd.arg(format!("-p{}", config.sevenz_password))
            .arg("-mhe=on"); // 文件名加密
    }
    hide_console(&mut cmd);

    let output = cmd.output()?;

    if !output.status.success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "7z 压缩失败: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }

    // 查找生成的分卷文件
    let mut parts = Vec::new();
    let prefix = format!("{}.7z.", base_name);

    for entry in fs::read_dir(work_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();

        if name.starts_with(&prefix) {
            parts.push(entry.path());
        }
    }

    // 按 001 / 002 排序
    parts.sort();

    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "未找到 7z 分卷输出",
        ));
    }

    Ok(SevenZResult { parts })
}

/// 隐藏子进程控制台窗口（仅 Windows 需要）
fn hide_console(cmd: &mut Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = cmd;
    }
}

/// 获取 7z 可执行文件路径
///
/// 策略（跨平台）：
/// 1️⃣ 优先使用 exe 同目录下的 7z（Windows: 7z.exe；macOS/Linux: 7z / 7zz / 7za）
/// 2️⃣ 其次在 PATH 中查找
/// 3️⃣ 都没有时回落到默认命令名，交由 Command 报错并提示安装
fn sevenz_path() -> io::Result<PathBuf> {
    // Homebrew 的 sevenzip 提供 7zz，p7zip 提供 7z / 7za
    let names: &[&str] = if cfg!(windows) {
        &["7z.exe", "7z"]
    } else {
        &["7z", "7zz", "7za"]
    };

    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
    {
        for name in names {
            let p = dir.join(name);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    if let Some(path) = find_in_path(names) {
        return Ok(path);
    }

    Ok(PathBuf::from(names[0]))
}

/// 在 PATH 中按顺序查找可执行文件
fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        for name in names {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 清理分卷文件（发送完成后）
pub fn cleanup_parts(parts: &[PathBuf], original: &Path) {
    for p in parts {
        if p != original {
            let _ = fs::remove_file(p);
        }
    }
}
