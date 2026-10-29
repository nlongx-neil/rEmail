use std::env;
use std::process::Command;
use std::path::Path;

fn main() {
    // 仅在发布构建时运行
    if env::var("PROFILE").unwrap() == "release" {
        // 获取目标平台
        let target = env::var("TARGET").unwrap();
        
        // 确定可执行文件名
        let exe_name = if target.contains("windows") {
            "mail_sender.exe"
        } else {
            "mail_sender"
        };
        
        // 构建输出路径
        let out_dir = env::var("OUT_DIR").unwrap();
        let target_dir = Path::new(&out_dir)
            .parent().unwrap()  // ../../../target
            .parent().unwrap()
            .parent().unwrap();
        
        let exe_path = target_dir.join(exe_name);

        println!("cargo:warning=正在检查可执行文件: {}", exe_path.display());
        
        if exe_path.exists() {
            println!("cargo:warning=正在使用UPX压缩可执行文件...");
            
            // 执行UPX压缩
            let status = Command::new("upx")
                .arg("--best")
                .arg("--lzma")
                .arg(exe_path.to_str().unwrap())
                .status();
            
            match status {
                Ok(s) if s.success() => {
                    println!("cargo:warning=✅ UPX压缩成功");
                    
                    // 显示压缩信息
                    let _ = Command::new("upx")
                        .arg("-l")
                        .arg(exe_path.to_str().unwrap())
                        .status();
                }
                Ok(s) => {
                    println!("cargo:warning=❌ UPX压缩失败，退出码: {}", s);
                }
                Err(e) => {
                    println!("cargo:warning=⚠️ UPX未安装或无法运行: {}", e);
                    println!("cargo:warning=请安装UPX: https://upx.github.io/");
                }
            }
        }else{
            println!("cargo:warning=未找到可执行文件: {}", exe_path.display());
        }
    }
}
