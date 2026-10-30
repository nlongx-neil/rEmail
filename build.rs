use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");

    let target = env::var("TARGET").unwrap_or_default();
    let is_windows = target.contains("windows");

    // Windows：把 assets/icon.ico 作为资源嵌入 exe（任务栏 / 资源管理器显示自定义图标）
    if is_windows {
        embed_windows_icon();
    }

    // 仅发布构建 + Windows 时用 UPX 压缩（UPX 只处理 PE，其他平台跳过）
    if env::var("PROFILE").unwrap_or_default() == "release" {
        if !is_windows {
            println!("cargo:warning=非 Windows 目标，跳过 UPX 压缩");
            return;
        }
        compress_with_upx(&target);
    }
}

fn embed_windows_icon() {
    // 构建主机是 Windows：走 winres（内部会调用 rc.exe 或 windres）
    #[cfg(windows)]
    {
        let mut res = winres::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("FileDescription", "MailSender");
        res.set("ProductName", "MailSender");

        if let Err(e) = res.compile() {
            println!("cargo:warning=嵌入 exe 图标失败（不影响编译）: {e}");
        }
    }

    // 构建主机非 Windows（如 macOS/Linux 交叉编译到 Windows）：显式调用 mingw 的
    // windres 生成 COFF 资源对象，再通过 rustc-link-arg 交给链接器，确保图标/版本信息真的嵌进去。
    #[cfg(not(windows))]
    cross_compile_windows_icon();
}

#[cfg(not(windows))]
fn cross_compile_windows_icon() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let icon = manifest.join("assets").join("icon.ico");
    if !icon.exists() {
        panic!("交叉编译嵌入图标失败：{} 不存在", icon.display());
    }

    let rc_path = out_dir.join("mail_sender.rc");
    let rc = format!(
        "1 ICON \"{icon}\"\n\
         1 VERSIONINFO\n\
         FILEVERSION 0,1,0,0\n\
         PRODUCTVERSION 0,1,0,0\n\
         FILEFLAGSMASK 0x3fL\n\
         FILEFLAGS 0x0L\n\
         FILEOS 0x40004L\n\
         FILETYPE 0x1L\n\
         FILESUBTYPE 0x0L\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   BLOCK \"040904b0\"\n\
         \x20   BEGIN\n\
         \x20     VALUE \"FileDescription\", \"MailSender\"\n\
         \x20     VALUE \"FileVersion\", \"0.1.0.0\"\n\
         \x20     VALUE \"ProductName\", \"MailSender\"\n\
         \x20     VALUE \"ProductVersion\", \"0.1.0.0\"\n\
         \x20     VALUE \"OriginalFilename\", \"mail_sender.exe\"\n\
         \x20   END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20   VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
        icon = icon.display().to_string().replace('\\', "/")
    );
    fs::write(&rc_path, rc).expect("写入 .rc 失败");

    let windres = env::var("WINDRES")
        .or_else(|_| env::var("WINDRES_x86_64_pc_windows_gnu"))
        .unwrap_or_else(|_| "x86_64-w64-mingw32-windres".to_string());
    let obj = out_dir.join("mail_sender_res.o");

    let status = Command::new(&windres)
        .arg("-O")
        .arg("coff")
        .arg(format!("--include-dir={}", manifest.display()))
        .arg(&rc_path)
        .arg("-o")
        .arg(&obj)
        .status();

    match status {
        Ok(s) if s.success() && obj.exists() => {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
            println!("cargo:warning=已用 {} 嵌入 Windows 图标与版本信息", windres);
        }
        Ok(s) => panic!("交叉编译嵌入图标失败：windres 退出码 {}", s.code().unwrap_or(-1)),
        Err(e) => panic!(
            "交叉编译嵌入图标失败：无法执行 {windres}（{e}）。请安装 mingw-w64（或设置 WINDRES 指向 x86_64-w64-mingw32-windres）"
        ),
    }
}

fn compress_with_upx(target: &str) {
    let exe_name = if target.contains("windows") {
        "mail_sender.exe"
    } else {
        "mail_sender"
    };

    // OUT_DIR = <target-dir>/<profile>/build/<pkg>-<hash>/out
    let out_dir = match env::var("OUT_DIR") {
        Ok(v) => v,
        Err(_) => return,
    };
    let target_dir = Path::new(&out_dir)
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf());

    let exe_path = match target_dir {
        Some(dir) => dir.join(exe_name),
        None => return,
    };

    println!("cargo:warning=正在检查可执行文件: {}", exe_path.display());

    if !exe_path.exists() {
        println!("cargo:warning=未找到可执行文件: {}", exe_path.display());
        return;
    }

    println!("cargo:warning=正在使用UPX压缩可执行文件...");
    let status = Command::new("upx")
        .arg("--best")
        .arg("--lzma")
        .arg(&exe_path)
        .status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:warning=UPX压缩成功");
            let _ = Command::new("upx").arg("-l").arg(&exe_path).status();
        }
        Ok(s) => println!("cargo:warning=UPX压缩失败，退出码: {}", s),
        Err(e) => {
            println!("cargo:warning=UPX未安装或无法运行: {}", e);
            println!("cargo:warning=请安装UPX: https://upx.github.io/");
        }
    }
}
