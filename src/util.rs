use std::process::{Command, ExitStatus};

pub fn command(program: &str) -> Command {
    #[cfg(not(windows))]
    return Command::new(program);

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut cmd = Command::new(program);
        cmd.creation_flags(0x0800_0000);
        cmd
    }
}

/// A `yt-dlp` command with the Python environment variables that a host
/// Python install could use to hijack the bundled interpreter removed.
pub fn ytdlp() -> Command {
    let mut cmd = command("yt-dlp");
    cmd.env_remove("PYTHONHOME").env_remove("PYTHONPATH");
    cmd
}

pub fn seconds_to_hms(sec: f64) -> String {
    let s = sec as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Parses a hand-typed clock value — `SS`, `MM:SS` or `HH:MM:SS` — into whole
/// seconds. Every segment is one or two ASCII digits; hours must be below 24
/// and minutes / seconds below 60. Anything else (empty segments, signs,
/// floats, `NaN`) is rejected rather than coerced.
pub fn parse_clock(s: &str) -> Option<u32> {
    let segments: Vec<&str> = s.split(':').collect();
    if segments.len() > 3 {
        return None;
    }
    let mut nums = [0u32; 3];
    let offset = 3 - segments.len();
    for (slot, seg) in nums[offset..].iter_mut().zip(&segments) {
        if seg.is_empty() || seg.len() > 2 || !seg.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = seg.parse().ok()?;
    }
    let [h, m, sec] = nums;
    let in_range = h < 24 && m < 60 && sec < 60;
    in_range.then_some(h * 3600 + m * 60 + sec)
}

/// Returns `true` when `name`'s stem (the portion before the first `.`)
/// is a Windows reserved device name, case-insensitively.
///
/// Windows routes opens of `CON`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` (and
/// variants with an extension, e.g. `NUL.mp4`) to kernel device objects
/// rather than the filesystem, corrupting the download silently.  The check
/// is compiled in only on Windows so it carries zero cost on other platforms.
#[cfg(windows)]
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_uppercase();
    matches!(
        stem.as_str(),
        "CON"  | "PRN"  | "AUX"  | "NUL"  |
        "COM0" | "COM1" | "COM2" | "COM3" | "COM4" |
        "COM5" | "COM6" | "COM7" | "COM8" | "COM9" |
        "LPT0" | "LPT1" | "LPT2" | "LPT3" | "LPT4" |
        "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
    )
}

/// Sanitizes `name` for use as a literal filename component embedded
/// directly into a yt-dlp output template (e.g. `"{sanitized}.%(ext)s"`).
pub fn sanitize_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => out.push('_'),
            '%' => out.push_str("%%"),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_end_matches('.').trim();
    if trimmed.is_empty() {
        return "chapter".to_string();
    }
    // Prefix reserved device names with an underscore so the yt-dlp output
    // template never resolves to a kernel device handle on Windows.
    #[cfg(windows)]
    if is_windows_reserved(trimmed) {
        return format!("_{trimmed}");
    }
    trimmed.to_string()
}

/// Number of base-10 digits needed to print `n` (minimum 1).
pub fn digit_width(mut n: usize) -> usize {
    if n == 0 {
        return 1;
    }
    let mut w = 0;
    while n > 0 {
        w += 1;
        n /= 10;
    }
    w
}

/// Extracts a user-presentable message from a failed yt-dlp invocation by
/// scanning stderr for the last `ERROR:` line.  Falls back to the exit code
/// when no error line is present.
pub(crate) fn extract_ytdlp_error(stderr: &str, status: &ExitStatus) -> String {
    stderr
        .lines()
        .rfind(|l| l.trim_start().starts_with("ERROR:"))
        .map(|l| l.trim_start().trim_start_matches("ERROR:").trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| match status.code() {
            Some(code) => format!("yt-dlp failed (exit code {code})"),
            None => "yt-dlp failed (terminated by signal)".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clock_accepts_the_three_forms() {
        assert_eq!(parse_clock("5"), Some(5));
        assert_eq!(parse_clock("01:30"), Some(90));
        assert_eq!(parse_clock("1:02:03"), Some(3723));
        assert_eq!(parse_clock("00:00:00"), Some(0));
    }

    #[test]
    fn parse_clock_rejects_everything_else() {
        for bad in ["", ":", "1::2", "60", "1:60", "24:00:00", "1:2:3:4", "-1", "1.5", "NaN", "inf", "123", "a:b"] {
            assert_eq!(parse_clock(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn sanitize_escapes_template_and_path_characters() {
        assert_eq!(sanitize_filename("50% off: a/b"), "50%% off_ a_b");
        assert_eq!(sanitize_filename("  ..  "), "chapter");
    }

    #[test]
    fn digit_width_counts_digits() {
        assert_eq!([0, 9, 10, 999, 1000].map(digit_width), [1, 1, 2, 3, 4]);
    }
}
