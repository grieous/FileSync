//! 小工具：时间格式、过滤摘要、CSV 转义。
//!
//! 时间与 CSV 的格式必须和 exe 导出的逐字符一致，否则「两边导出对不上」
//! 会被当成 bug 排查半天。所以这里是照抄 winsync-gui.rs 的实现，而不是另写一套。

use sync_core::Filter;

pub fn now_secs() -> i64 {    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 28–34 秒之间的抖动，避免多任务同时打服务器。
pub fn jitter() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        % 7
}

/// `MM-DD HH:MM`，日志行里用的短时间（界面展示用，daemon 目前只发原始秒）。
#[allow(dead_code)]
pub fn fmt_ts(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (_, m, d) = civil(days);
    format!("{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

/// `YYYY-MM-DD HH:MM:SS`，CSV 用的完整时间。
pub fn fmt_ts_full(secs: i64) -> String {
    let local = secs + 8 * 3600;
    let days = local.div_euclid(86400);
    let rem = local.rem_euclid(86400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 落盘时间戳，带 `Z`（沿用 exe 的写法，不要改，否则日志比对会错位）。
pub fn stamp_full() -> String {
    let secs = now_secs();
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// 公历换算（Howard Hinnant 的 civil_from_days）。
pub fn civil(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    ((y + if m <= 2 { 1 } else { 0 }) as i32, m, d)
}

pub fn filter_summary(f: &Filter) -> String {
    let mut parts: Vec<String> = Vec::new();
    if f.ignore_dotfiles {
        parts.push("忽略点文件".into());
    }
    if !f.ignore_extensions.is_empty() {
        parts.push(format!("忽略 {}", f.ignore_extensions.join("/")));
    }
    if f.size_min.is_some() || f.size_max.is_some() {
        parts.push("按大小过滤".into());
    }
    if parts.is_empty() {
        "无过滤".into()
    } else {
        parts.join(" · ")
    }
}

pub fn short_server(s: &str) -> String {
    s.trim()
        .trim_start_matches("ws://")
        .trim_start_matches("wss://")
        .trim_end_matches("/ws")
        .to_string()
}

/// CSV 字段转义，规则与 exe 的 `csv_escape` 相同。
pub fn csv_escape(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// 把 anyhow 的错误链拼成一行人话，且去掉被上层描述重复包含的段落。
///
/// `{e:#}` 会老老实实把整条链都拼上，而 tungstenite 这一层自己的 `Display`
/// 已经把底层 io 错误带进去了，于是界面上会出现
/// `连接服务器失败: IO error: … (os error 10061): … (os error 10061)`。
pub fn describe_error(err: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for item in err.chain() {
        let text = item.to_string();
        let text = text.trim();
        if text.is_empty() || parts.iter().any(|seen| seen.contains(text)) {
            continue;
        }
        parts.push(text.to_string());
    }
    if parts.is_empty() {
        return "未知错误".to_string();
    }
    parts.join(": ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_matches_exe_rules() {
        assert_eq!(csv_escape("a.txt"), "a.txt");
        assert_eq!(csv_escape("a,b.txt"), "\"a,b.txt\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn timestamps_are_fixed_offset() {
        // 2026-11-21 14:00:00 UTC → 22:00 北京时间
        assert_eq!(fmt_ts_full(1_795_269_600), "2026-11-21 22:00:00");
    }

    #[test]
    fn filter_summary_reads_like_the_gui() {
        let mut f = Filter::default();
        assert_eq!(filter_summary(&f), "无过滤");
        f.ignore_dotfiles = true;
        f.ignore_extensions = vec!["lnk".into(), "tmp".into()];
        assert_eq!(filter_summary(&f), "忽略点文件 · 忽略 lnk/tmp");
    }

    #[test]
    fn error_chain_is_joined_without_repeating_itself() {
        // 复刻真实那一条：tungstenite 的 Display 已经带上了底层 io 错误
        let io = anyhow::anyhow!("由于目标计算机积极拒绝，无法连接。 (os error 10061)");
        let mid = anyhow::anyhow!("IO error: {io}");
        let top = mid.context("连接服务器失败");
        assert_eq!(
            describe_error(&top),
            "连接服务器失败: IO error: 由于目标计算机积极拒绝，无法连接。 (os error 10061)"
        );
    }
}
