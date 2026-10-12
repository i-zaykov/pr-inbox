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
    match tokio::time::timeout(timeout, c.output()).await {
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
}
