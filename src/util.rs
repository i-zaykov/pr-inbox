// Shared helpers: paths, JSON files, the PATH a GUI app lacks, running host commands, Terminal.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// Git inside a run authenticates through gh, never the macOS keychain helper: launched from the background
// server, osxkeychain raises a password dialog on every push and the run blocks until someone answers it.
// The empty value clears the helpers git config files set (Homebrew's gitconfig sets osxkeychain), and
// GIT_TERMINAL_PROMPT=0 makes any other credential ask fail at once instead of waiting.
pub const GIT_ENV: [(&str, &str); 6] = [
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "credential.helper"),
    ("GIT_CONFIG_VALUE_0", ""),
    ("GIT_CONFIG_KEY_1", "credential.helper"),
    ("GIT_CONFIG_VALUE_1", "!gh auth git-credential"),
];

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn local_tm(secs: i64) -> libc::tm {
    let t = secs as libc::time_t;
    // SAFETY: localtime_r writes into the zeroed struct we own and reads only `t`.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    }
}

/// The local calendar date of a millisecond timestamp, as YYYY-MM-DD.
pub fn local_date(ms: u64) -> String {
    let tm = local_tm((ms / 1000) as i64);
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

pub fn local_hour() -> u32 {
    local_tm((now_ms() / 1000) as i64).tm_hour as u32
}

/// A millisecond timestamp in GitHub's format, YYYY-MM-DDTHH:MM:SSZ, so the two compare as strings.
pub fn utc_iso(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    // SAFETY: gmtime_r writes into the zeroed struct we own and reads only `t`.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::gmtime_r(&t, &mut tm);
        tm
    };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Milliseconds since the epoch of an RFC 3339 time such as GitHub's `2026-11-05T14:00:00Z` or Anthropic's
/// `2026-11-05T14:00:00.383538+00:00`. None for anything else.
pub fn parse_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |at: usize, len: usize| -> Option<i32> { s.get(at..at + len).filter(|p| p.bytes().all(|c| c.is_ascii_digit()))?.parse().ok() };
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    // SAFETY: an all-zero tm is a valid value; every field timegm reads is set below.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    (tm.tm_year, tm.tm_mon, tm.tm_mday) = (num(0, 4)? - 1900, num(5, 2)? - 1, num(8, 2)?);
    (tm.tm_hour, tm.tm_min, tm.tm_sec) = (num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(0..12).contains(&tm.tm_mon) || !(1..=31).contains(&tm.tm_mday) || tm.tm_hour > 23 || tm.tm_min > 59 || tm.tm_sec > 60 {
        return None;
    }
    let mut rest = &s[19..];
    let mut ms = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        ms = frac[..digits].chars().chain("000".chars()).take(3).collect::<String>().parse().ok()?;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && matches!(rest.as_bytes()[0], b'+' | b'-') && rest.as_bytes()[3] == b':' => {
            let at = s.len() - 6;
            let minutes = i64::from(num(at + 1, 2)? * 60 + num(at + 4, 2)?);
            if rest.starts_with('+') { minutes } else { -minutes }
        }
        _ => return None,
    };
    // SAFETY: timegm reads the calendar value we own and keeps no reference to it.
    let secs: i64 = unsafe { libc::timegm(&mut tm) };
    u64::try_from((secs - offset * 60) * 1000 + ms).ok()
}

/// A local time of day, "15:00", with the weekday in front when it falls on another day than `now`: "Sun 06:00".
pub fn local_clock(ms: u64, now: u64) -> String {
    let tm = local_tm((ms / 1000) as i64);
    let time = format!("{:02}:{:02}", tm.tm_hour, tm.tm_min);
    if local_date(ms) == local_date(now) {
        return time;
    }
    let day = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"].get(tm.tm_wday as usize).copied().unwrap_or("");
    format!("{day} {time}")
}

/// The string serde gives a unit enum variant, such as "CHANGES_REQUESTED" for a review decision.
pub fn serde_name<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default()
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()))
}

pub fn data_dir() -> PathBuf {
    std::env::var("PR_INBOX_DATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join("Library/Application Support/PR Inbox"))
}

pub fn checkouts_dir() -> PathBuf {
    std::env::var("PR_INBOX_CHECKOUTS").map(PathBuf::from).unwrap_or_else(|_| home().join("code"))
}

/// Writes `text` to a temporary file beside `file`, then renames it over `file`, so a crash mid-write
/// never leaves a torn file.
pub fn write_atomic(file: &Path, text: &str) -> std::io::Result<()> {
    fs::create_dir_all(file.parent().unwrap_or(Path::new(".")))?;
    let tmp = file.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&tmp, text)?;
    fs::rename(tmp, file)
}

/// Moves a file this binary cannot read out of the way, so it is kept rather than overwritten.
pub fn set_aside(file: &Path) -> PathBuf {
    let kept = file.with_extension(format!("json.broken-{}", now_ms()));
    let _ = fs::rename(file, &kept);
    kept
}

/// A JSON file's contents, or the default when it is missing or unreadable.
pub fn read_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> T {
    fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// Opened from Finder or Raycast, the app gets launchd's bare PATH; gh and claude live in Homebrew.
pub fn fix_path() {
    let current = std::env::var("PATH").unwrap_or_default();
    let mut parts: Vec<String> = current.split(':').filter(|p| !p.is_empty()).map(String::from).collect();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        if !parts.iter().any(|p| p == extra) {
            parts.push(extra.to_string());
        }
    }
    let local = home().join(".local/bin").to_string_lossy().to_string();
    if !parts.contains(&local) {
        parts.push(local);
    }
    std::env::set_var("PATH", parts.join(":"));
}

pub fn first_line(s: &str) -> String {
    s.trim().lines().next().unwrap_or("").chars().take(300).collect()
}

pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a prepared command with a deadline, reading both pipes while it runs. A command that cannot start
/// gives code 127, and one past its deadline is killed and gives 124, as in a shell.
pub async fn output(mut c: tokio::process::Command, timeout: Duration) -> Output {
    let program = c.as_std().get_program().to_string_lossy().to_string();
    c.kill_on_drop(true);
    ended(&program, tokio::time::timeout(timeout, c.output()).await)
}

fn ended(program: &str, result: Result<std::io::Result<std::process::Output>, tokio::time::error::Elapsed>) -> Output {
    match result {
        Ok(Ok(o)) => Output {
            code: o.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        },
        Ok(Err(e)) => Output { code: 127, stdout: String::new(), stderr: format!("{program}: {e}") },
        Err(_) => Output { code: 124, stdout: String::new(), stderr: format!("{program} timed out") },
    }
}

pub async fn sh(cmd: &str, args: &[&str], timeout: Duration) -> Output {
    let mut c = tokio::process::Command::new(cmd);
    c.args(args);
    output(c, timeout).await
}

/// `sh` with `input` written to the command's stdin, for a secret that must stay out of its argv, where any
/// local process can read it.
pub async fn sh_input(cmd: &str, args: &[&str], input: &[u8], timeout: Duration) -> Output {
    use tokio::io::AsyncWriteExt;
    let mut c = tokio::process::Command::new(cmd);
    c.args(args).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    let run = async {
        let mut child = c.spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).await?;
        }
        child.wait_with_output().await
    };
    ended(cmd, tokio::time::timeout(timeout, run).await)
}

pub fn shq(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn asq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub async fn open_terminal(command: &str) -> Output {
    let script = format!("tell application \"Terminal\" to do script {}", asq(command));
    sh("osascript", &["-e", &script, "-e", "tell application \"Terminal\" to activate"], Duration::from_secs(20)).await
}

/// True when `pid` is a live process running the claude CLI. kill(pid, 0) alone also matches an unrelated
/// process that reused the number after a reboot, which would keep a finished run Running forever and let
/// Stop signal a stranger. A zombie has no path, so it reads as gone.
pub fn is_claude(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only checks existence; proc_pidpath writes at most `buf.len()` bytes.
    if unsafe { libc::kill(pid as i32, 0) } != 0 {
        return false;
    }
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid as i32, buf.as_mut_ptr().cast(), buf.len() as u32) };
    n > 0 && String::from_utf8_lossy(&buf[..n as usize]).rsplit('/').next() == Some("claude")
}

/// Waits for a child on a throwaway thread, so a process nobody else waits for never lingers as a zombie.
pub fn reap(mut child: std::process::Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// A macOS notification. Best effort: it never blocks and a failure is ignored.
pub fn notify(title: &str, body: &str) {
    let script = format!("display notification {} with title {}", asq(body), asq(title));
    let spawned = std::process::Command::new("osascript")
        .args(["-e", &script])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(child) = spawned {
        reap(child);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_iso_matches_githubs_timestamps() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_iso(1_791_372_605_999), "2026-10-07T11:30:05Z");
    }

    #[test]
    fn parse_iso_reads_github_and_anthropic_times() {
        assert_eq!(parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso("2026-10-07T11:30:05Z"), Some(1_791_372_605_000));
        assert_eq!(parse_iso("2026-10-07T11:30:05.999Z"), Some(1_791_372_605_999));
        assert_eq!(parse_iso("2026-10-07T11:30:05.9Z"), Some(1_791_372_605_900));
        assert_eq!(parse_iso("2026-11-05T14:00:00.383538+00:00"), parse_iso("2026-11-05T14:00:00.383Z"));
        assert_eq!(parse_iso("2026-11-05T15:00:00+01:00"), parse_iso("2026-11-05T14:00:00Z"));
        assert_eq!(parse_iso("2026-11-05T09:30:00-04:30"), parse_iso("2026-11-05T14:00:00Z"));
        assert_eq!(parse_iso("2024-02-29T00:00:00Z").map(utc_iso), Some("2024-02-29T00:00:00Z".into()));
        for bad in ["", "2026-11-05", "2026-11-05T14:00:00", "2026-13-05T14:00:00Z", "2026-11-05T14:00:00+0100", "2026-11-05T14:00:00.Z", "garbage-in-garbage-out", "2026-11-05T14:00:00Zjunk", "1969-12-31T23:59:59Z"] {
            assert_eq!(parse_iso(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_reset_on_another_day_names_the_weekday() {
        let now = parse_iso("2026-11-05T12:00:00Z").unwrap();
        assert_eq!(local_clock(now, now).len(), 5);
        let later = local_clock(now + 3 * 86_400_000, now);
        assert!(["Sun ", "Mon "].iter().any(|d| later.starts_with(d)), "{later}");
    }

    #[tokio::test]
    async fn input_reaches_the_command_on_stdin_only() {
        let out = sh_input("cat", &[], b"Authorization: Bearer secret\n", Duration::from_secs(5)).await;
        assert_eq!((out.code, out.stdout.as_str()), (0, "Authorization: Bearer secret\n"));
    }
}
