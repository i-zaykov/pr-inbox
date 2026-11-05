// Your Claude plan's usage limits, read from the endpoint Claude Code's own usage screen reads. The OAuth token
// comes from the keychain entry Claude Code keeps and reaches curl on stdin only, so it never shows in a process
// list, a log or an error message.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::util::{first_line, local_clock, now_ms, parse_iso, sh, sh_input};

const ENDPOINT: &str = "https://api.anthropic.com/api/oauth/usage";

/// How full each limit is, in percent (100 is the limit), and when it resets.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// The five-hour session.
    pub session: f64,
    pub session_resets_at: Option<u64>,
    /// The fullest weekly limit: the one over all models, or a model's own.
    pub weekly: f64,
    pub weekly_resets_at: Option<u64>,
    pub fetched_at: u64,
}

/// Reads the reply. Session is `five_hour`; weekly is the fullest of every `seven_day*` entry and every weekly
/// entry under `limits`. A reply without both is an error, so unknown usage never reads as room to start.
pub fn parse(text: &str, now: u64) -> Result<Usage, String> {
    let doc: Value = serde_json::from_str(text).map_err(|_| "Claude's usage reply is unreadable")?;
    let reading = |v: &Value, key: &str| {
        v[key].as_f64().filter(|p| p.is_finite() && *p >= 0.0).map(|p| (p, v["resets_at"].as_str().and_then(parse_iso)))
    };
    let fullest = |a: &(f64, Option<u64>), b: &(f64, Option<u64>)| a.0.total_cmp(&b.0);
    let limits = doc["limits"].as_array().map(Vec::as_slice).unwrap_or_default();
    let group = |name: &'static str| limits.iter().filter(move |l| l["group"] == name).filter_map(|l| reading(l, "percent"));
    let session = reading(&doc["five_hour"], "utilization").into_iter().chain(group("session")).max_by(fullest);
    let weekly = doc.as_object().into_iter().flatten()
        .filter(|(key, _)| key.starts_with("seven_day"))
        .filter_map(|(_, v)| reading(v, "utilization"))
        .chain(group("weekly"))
        .max_by(fullest);
    match (session, weekly) {
        (Some((session, session_resets_at)), Some((weekly, weekly_resets_at))) => Ok(Usage { session, session_resets_at, weekly, weekly_resets_at, fetched_at: now }),
        _ => Err("Claude's usage reply has no session or weekly figure".into()),
    }
}

/// Room to start a review: the session below `session_limit` percent and every weekly limit below
/// `weekly_limit`. The refusal names the limit and when it resets, in local time.
pub fn gate(u: &Usage, session_limit: u8, weekly_limit: u8, now: u64) -> Result<(), String> {
    let resets = |at: Option<u64>| at.map(|ms| format!(", resets {}", local_clock(ms, now))).unwrap_or_default();
    if u.session >= f64::from(session_limit) {
        return Err(format!("Session at {:.0}%{}", u.session, resets(u.session_resets_at)));
    }
    if u.weekly >= f64::from(weekly_limit) {
        return Err(format!("Weekly usage at {:.0}%{}", u.weekly, resets(u.weekly_resets_at)));
    }
    Ok(())
}

/// The access token in Claude Code's keychain entry, refused once expired. Errors never quote the entry.
fn read_token(text: &str, now: u64) -> Result<String, String> {
    let doc: Value = serde_json::from_str(text.trim()).map_err(|_| "Claude Code's keychain entry is unreadable")?;
    let oauth = &doc["claudeAiOauth"];
    // A header line ends at a newline, so a token with one could smuggle in another header.
    let token = oauth["accessToken"].as_str().filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_graphic()))
        .ok_or("Claude Code's keychain entry has no usable sign-in token")?;
    if oauth["expiresAt"].as_u64().is_some_and(|at| at <= now) {
        return Err("Claude's sign-in token has expired. Claude Code renews it the next time it runs".into());
    }
    Ok(token.into())
}

/// Reads usage now: the token from the keychain, then the endpoint through curl.
pub async fn fetch() -> Result<Usage, String> {
    let entry = sh("security", &["find-generic-password", "-s", "Claude Code-credentials", "-w"], Duration::from_secs(20)).await;
    if entry.code != 0 {
        return Err("Cannot read Claude Code's sign-in from the keychain. Sign in to Claude Code, and choose Always Allow if macOS asks".into());
    }
    let token = read_token(&entry.stdout, now_ms())?;
    let version = sh("claude", &["--version"], Duration::from_secs(20)).await;
    let version = version.stdout.split_whitespace().next().filter(|v| v.bytes().all(|b| b.is_ascii_alphanumeric() || b".-+".contains(&b))).unwrap_or("0").to_string();
    let headers = format!("Authorization: Bearer {token}\nanthropic-beta: oauth-2025-04-20\nUser-Agent: claude-code/{version}\n");
    let out = sh_input("curl", &["-sS", "--max-time", "20", "-H", "@-", "-w", "\n%{http_code}", ENDPOINT], headers.as_bytes(), Duration::from_secs(30)).await;
    let (body, status) = out.stdout.rsplit_once('\n').unwrap_or(("", out.stdout.as_str()));
    match status.trim() {
        "200" => parse(body, now_ms()),
        "401" | "403" => Err(format!("Claude refused the sign-in token (HTTP {}). Claude Code renews it the next time it runs", status.trim())),
        "429" => Err("Claude's usage endpoint is rate limited. PR Inbox asks again in a few minutes".into()),
        _ if out.code != 0 => Err(format!("Could not reach Claude's usage endpoint: {}", first_line(&out.stderr))),
        code => Err(format!("Claude's usage endpoint answered HTTP {code}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TODAY: &str = r#"{"five_hour":{"utilization":100.0,"resets_at":"2026-11-05T14:00:00.383538+00:00"},
        "seven_day":{"utilization":59.0,"resets_at":"2026-11-08T05:00:00.383564+00:00"},"seven_day_oauth_apps":null,"seven_day_opus":null,
        "extra_usage":{"is_enabled":true,"monthly_limit":null},
        "limits":[{"kind":"session","group":"session","percent":100,"severity":"critical","resets_at":"2026-11-05T14:00:00.383538+00:00","is_active":true},
            {"kind":"weekly_all","group":"weekly","percent":59,"resets_at":"2026-11-08T05:00:00.383564+00:00"},
            {"kind":"weekly_scoped","group":"weekly","percent":4,"scope":{"model":{"display_name":"Fable"}},"resets_at":"2026-11-08T05:00:00Z"}]}"#;

    fn usage(session: f64, weekly: f64) -> Usage {
        Usage { session, session_resets_at: parse_iso("2026-11-05T14:00:00Z"), weekly, weekly_resets_at: parse_iso("2026-11-08T05:00:00Z"), fetched_at: 1 }
    }

    #[test]
    fn todays_reply_reads_as_a_full_session_and_the_fullest_week() {
        let u = parse(TODAY, 7).unwrap();
        assert_eq!((u.session, u.weekly, u.fetched_at), (100.0, 59.0, 7));
        assert_eq!(u.session_resets_at, parse_iso("2026-11-05T14:00:00.383Z"));
        assert_eq!(u.weekly_resets_at, parse_iso("2026-11-08T05:00:00.383Z"));
    }

    #[test]
    fn a_model_limit_or_the_limits_list_alone_still_counts() {
        let u = parse(r#"{"five_hour":{"utilization":12.5},"seven_day":{"utilization":20},"seven_day_opus":{"utilization":95,"resets_at":"2026-11-07T00:00:00Z"}}"#, 0).unwrap();
        assert_eq!((u.session, u.weekly, u.session_resets_at), (12.5, 95.0, None));
        assert_eq!(u.weekly_resets_at, parse_iso("2026-11-07T00:00:00Z"));
        let u = parse(r#"{"limits":[{"group":"session","percent":30},{"group":"weekly","percent":40},{"group":"weekly","percent":91}]}"#, 0).unwrap();
        assert_eq!((u.session, u.weekly), (30.0, 91.0));
    }

    #[test]
    fn missing_null_or_garbage_usage_is_unknown_not_empty() {
        for text in ["", "garbage", "[]", "null", "{}", r#"{"five_hour":null,"seven_day":null}"#, r#"{"five_hour":{"utilization":10}}"#,
            r#"{"seven_day":{"utilization":10}}"#, r#"{"five_hour":{"utilization":"10"},"seven_day":{"utilization":10}}"#,
            r#"{"five_hour":{"utilization":-1},"seven_day":{"utilization":10}}"#, r#"{"five_hour":{},"seven_day":{"utilization":10},"limits":"no"}"#] {
            assert!(parse(text, 0).is_err(), "{text}");
        }
    }

    #[test]
    fn the_gate_holds_at_the_limit_and_names_what_resets() {
        let now = parse_iso("2026-11-05T12:00:00Z").unwrap();
        assert_eq!(gate(&usage(69.9, 89.9), 70, 90, now), Ok(()));
        let session = gate(&usage(100.0, 59.0), 70, 90, now).unwrap_err();
        assert_eq!(session, format!("Session at 100%, resets {}", local_clock(parse_iso("2026-11-05T14:00:00Z").unwrap(), now)));
        assert!(gate(&usage(70.0, 10.0), 70, 90, now).unwrap_err().starts_with("Session at 70%"));
        let weekly = gate(&usage(10.0, 90.0), 70, 90, now).unwrap_err();
        assert!(weekly.starts_with("Weekly usage at 90%, resets "), "{weekly}");
        assert_eq!(gate(&usage(10.0, 90.0), 70, 95, now), Ok(()));
        assert_eq!(gate(&usage(85.0, 10.0), 90, 90, now), Ok(()));
        let unknown_reset = Usage { session_resets_at: None, ..usage(80.0, 10.0) };
        assert_eq!(gate(&unknown_reset, 70, 90, now).unwrap_err(), "Session at 80%");
    }

    #[test]
    fn the_token_is_refused_when_expired_or_unsafe_and_never_quoted() {
        let entry = |token: &str, expires: u64| serde_json::json!({ "claudeAiOauth": { "accessToken": token, "expiresAt": expires } }).to_string();
        assert_eq!(read_token(&entry("sk-ant-oat01-abc", 2_000), 1_000).unwrap(), "sk-ant-oat01-abc");
        for (text, now) in [(entry("sk-ant-oat01-abc", 1_000), 1_000), (entry("sk-ant\nX-Evil: 1", 2_000), 1_000), (entry("", 2_000), 1_000),
            ("{\"claudeAiOauth\":{}}".into(), 0), ("not json sk-ant-oat01-abc".into(), 0)] {
            let e = read_token(&text, now).unwrap_err();
            assert!(!e.contains("sk-ant") && !e.contains("Evil"), "{e}");
        }
    }
}
