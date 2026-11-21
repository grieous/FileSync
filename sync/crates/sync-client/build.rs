// 把版本信息嵌入 exe。
//
// 不用 winresource：它靠 reg.exe 找 Windows SDK，而本机 reg.exe 被安全策略拦了。
// 这里自己走两步：rc.exe 编译 .rc → cvtres 转成 COFF 目标文件 → 交给链接器。
// 直接把 .res 当链接参数传进去，link.exe 会静默忽略，所以必须先转 .obj。
//
// 版本号来自 Cargo.toml 的 [workspace.package] version，.rc 内容在这里生成，
// 避免版本号在两处各写一遍、改一处忘另一处。

use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    let base = Path::new(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(base)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.reverse();
    dirs.into_iter()
        .map(|d| d.join("x64").join("rc.exe"))
        .find(|p| p.exists())
}

fn find_cvtres() -> Option<PathBuf> {
    let base = Path::new(r"C:\Program Files\Microsoft Visual Studio");
    let mut found: Vec<PathBuf> = Vec::new();
    for edition in std::fs::read_dir(base).ok()?.flatten() {
        let products = std::fs::read_dir(edition.path()).ok();
        for product in products.into_iter().flatten().flatten() {
            let msvc = product.path().join("VC").join("Tools").join("MSVC");
            if let Ok(vers) = std::fs::read_dir(&msvc) {
                for v in vers.flatten() {
                    let p = v
                        .path()
                        .join("bin")
                        .join("Hostx64")
                        .join("x64")
                        .join("cvtres.exe");
                    if p.exists() {
                        found.push(p);
                    }
                }
            }
        }
    }
    found.sort();
    found.pop()
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let mut nums = version
        .split('.')
        .map(|s| s.parse::<u32>().unwrap_or(0))
        .collect::<Vec<_>>();
    while nums.len() < 4 {
        nums.push(0);
    }
    let quad = format!("{},{},{},{}", nums[0], nums[1], nums[2], 0);

    let rc_text = format!(
        r#"#pragma code_page(65001)

#define VS_VERSION_INFO   1
#define VOS_NT_WINDOWS32  0x00040004L
#define VFT_APP           0x00000001L
#define FILEFLAGSMASK_    0x3fL

VS_VERSION_INFO VERSIONINFO
 FILEVERSION {quad}
 PRODUCTVERSION {quad}
 FILEFLAGSMASK FILEFLAGSMASK_
 FILEFLAGS 0x0L
 FILEOS VOS_NT_WINDOWS32
 FILETYPE VFT_APP
 FILESUBTYPE 0x0L
BEGIN
    BLOCK "StringFileInfo"
    BEGIN
        BLOCK "080404B0"
        BEGIN
            VALUE "CompanyName", "WinSync"
            VALUE "FileDescription", "WinSync 文件夹同步"
            VALUE "FileVersion", "{version}"
            VALUE "InternalName", "WinSync"
            VALUE "OriginalFilename", "WinSync.exe"
            VALUE "ProductName", "WinSync"
            VALUE "ProductVersion", "{version}"
        END
    END
    BLOCK "VarFileInfo"
    BEGIN
        VALUE "Translation", 0x804, 1200
    END
END
"#
    );

    let Some(rc) = find_rc() else {
        println!("cargo:warning=找不到 rc.exe，跳过版本资源");
        return;
    };
    let Some(cvtres) = find_cvtres() else {
        println!("cargo:warning=找不到 cvtres.exe，跳过版本资源");
        return;
    };

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let rc_file = out_dir.join("version.rc");
    let res = out_dir.join("winres.res");
    let obj = out_dir.join("winres.obj");

    if std::fs::write(&rc_file, rc_text).is_err() {
        println!("cargo:warning=写入 version.rc 失败");
        return;
    }

    let a = Command::new(&rc)
        .args([
            "/nologo",
            "/fo",
            res.to_str().unwrap(),
            rc_file.to_str().unwrap(),
        ])
        .output();

    match a {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            println!(
                "cargo:warning=rc.exe 失败: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            println!("cargo:warning=调用 rc.exe 失败: {e}");
            return;
        }
    }

    let b = Command::new(&cvtres)
        .args([
            "/NOLOGO",
            "/MACHINE:X64",
            &format!("/OUT:{}", obj.display()),
            &res.display().to_string(),
        ])
        .output();

    match b {
        Ok(o) if o.status.success() => {
            println!("cargo:rustc-link-arg-bins={}", obj.display());
        }
        Ok(o) => println!(
            "cargo:warning=cvtres 失败: {}",
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(e) => println!("cargo:warning=调用 cvtres 失败: {e}"),
    }
}
