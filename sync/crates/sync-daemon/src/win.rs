//! Windows 系统调用：单实例互斥体、进程存活、开机自启、打开目录。
//!
//! 这里刻意与 exe 用**不同**的互斥体名和自启值名：两者互不相干，
//! 任一方的启动/退出/卸载都不会影响另一方（用户硬要求，见实施计划 §2.8）。

use std::path::Path;

/// 自己的互斥体。exe 用的是 `Local\WinSync_SingleInstance_v1`，两者可同时持有。
///
/// 名字里带上状态目录的哈希：同一个状态目录只能有一个 daemon（正常情况），
/// 而测试或多实例部署用不同目录时互不阻塞。
fn mutex_name() -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    sync_client::Config::dir()
        .to_string_lossy()
        .to_lowercase()
        .hash(&mut hasher);
    format!(
        "Local\\dsh-winsync-daemon-v1-{:08x}",
        hasher.finish() as u32
    )
}
/// 自启注册表值名，与 exe 的 `WinSync` 区分开。
const AUTOSTART_VALUE: &str = "WinSyncDaemon";

/// 抢单实例锁。拿到（或本来没人占）返回 true；已有实例持有返回 false。
#[cfg(windows)]
pub fn acquire_single_instance() -> bool {
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows_sys::Win32::System::Threading::CreateMutexW;

    unsafe {
        let name: Vec<u16> = mutex_name()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        if (handle as isize) == 0 {
            // 创建失败（极端情况）时宁可放行，也不要让同步服务起不来
            return true;
        }
        // 句柄故意泄漏：进程活着期间必须一直持有它
        GetLastError() != ERROR_ALREADY_EXISTS
    }
}

#[cfg(not(windows))]
pub fn acquire_single_instance() -> bool {
    true
}

/// 进程是否还活着。用于判断 `daemon.json` 是陈旧握手还是真在跑。
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_TIMEOUT};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SYNCHRONIZE 在 windows-sys 里没有导出常量（属通用访问权）
    const SYNCHRONIZE: u32 = 0x0010_0000;

    if pid == 0 {
        return false;
    }
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid);
        if (h as isize) == 0 {
            return false;
        }
        let state = WaitForSingleObject(h, 0);
        CloseHandle(h);
        state == WAIT_TIMEOUT
    }
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// 用资源管理器打开目录（只在 daemon 自己的机器上，路径已由调用方校验）。
pub fn open_dir(path: &Path) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 0x08000000 = CREATE_NO_WINDOW：别在黑框里闪一下
        std::process::Command::new("explorer")
            .arg(path)
            .creation_flags(0x0800_0000)
            .spawn()
            .is_ok()
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(path)
            .spawn()
            .is_ok()
    }
}

#[cfg(windows)]
pub fn autostart_get() -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    };
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let mut key = std::mem::zeroed();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            KEY_QUERY_VALUE,
            &mut key,
        ) != 0
        {
            return false;
        }
        let name: Vec<u16> = AUTOSTART_VALUE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let mut ty = 0u32;
        let mut len = 0u32;
        let rc = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            std::ptr::null_mut(),
            &mut len,
        );
        RegCloseKey(key);
        rc == 0 && len > 0
    }
}

#[cfg(windows)]
pub fn autostart_set(on: bool) -> bool {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };
    unsafe {
        let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Run\0"
            .encode_utf16()
            .collect();
        let mut key = std::mem::zeroed();
        let mut disp = 0u32;
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            0,
            std::ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            std::ptr::null(),
            &mut key,
            &mut disp,
        ) != 0
        {
            return false;
        }
        let name: Vec<u16> = AUTOSTART_VALUE
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let ok = if on {
            let exe = std::env::current_exe().unwrap_or_default();
            let cmd = format!("\"{}\"", exe.display());
            let data: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            ) == 0
        } else {
            RegDeleteValueW(key, name.as_ptr()) == 0
        };
        RegCloseKey(key);
        ok
    }
}

#[cfg(not(windows))]
pub fn autostart_get() -> bool {
    false
}

#[cfg(not(windows))]
pub fn autostart_set(_on: bool) -> bool {
    false
}

/// exe 是否正在运行。用于「读不到 exe 配置时无法确认目录是否重叠」这条兜底。
#[cfg(windows)]
pub fn exe_running() -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };

    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut entry) != 0;
        while ok {
            let name = String::from_utf16_lossy(
                &entry.szExeFile[..entry
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(entry.szExeFile.len())],
            );
            if name.eq_ignore_ascii_case("WinSync.exe") {
                found = true;
                break;
            }
            ok = Process32NextW(snap, &mut entry) != 0;
        }
        CloseHandle(snap);
        found
    }
}

#[cfg(not(windows))]
pub fn exe_running() -> bool {
    false
}
