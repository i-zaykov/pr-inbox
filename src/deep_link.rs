// The only native link action is selecting a PR. Opening a link never starts a run or writes to GitHub.
use serde::Serialize;
use tauri::Url;

/// Accept links to a PR's discussion, files, commits or checks and keep its canonical GitHub URL.
pub fn normalize_pr_url(value: &str) -> Result<String, String> {
    let url = Url::parse(value.trim()).map_err(|_| "Not a GitHub pull request URL")?;
    if url.scheme() != "https" || url.host_str() != Some("github.com") || !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
        return Err("Expected an https://github.com pull request URL".into());
    }
    let parts: Vec<_> = url.path().trim_end_matches('/').split('/').collect();
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    if parts.len() < 5 || !parts[0].is_empty() || !word(parts[1]) || !word(parts[2]) || parts[3] != "pull" {
        return Err("Not a GitHub pull request URL".into());
    }
    let number = parts[4].parse::<u64>().ok().filter(|n| *n > 0 && parts[4].bytes().all(|c| c.is_ascii_digit()))
        .ok_or("The pull request number must be a positive integer")?;
    if parts.len() > 5 && !matches!(parts[5], "files" | "commits" | "checks") {
        return Err("Not a supported pull request page".into());
    }
    Ok(format!("https://github.com/{}/{}/pull/{number}", parts[1], parts[2]))
}

pub fn parse(value: &Url) -> Result<String, String> {
    if value.scheme() != "pr-inbox" || value.host_str() != Some("open") || !matches!(value.path(), "" | "/")
        || !value.username().is_empty() || value.password().is_some() || value.port().is_some() || value.fragment().is_some() {
        return Err("Expected pr-inbox://open?url=<GitHub pull request URL>".into());
    }
    let params: Vec<_> = value.query_pairs().collect();
    if params.len() != 1 || params[0].0 != "url" {
        return Err("A PR Inbox link must contain exactly one url parameter".into());
    }
    normalize_pr_url(&params[0].1)
}

#[derive(Serialize)]
pub struct Request {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl From<Result<String, String>> for Request {
    fn from(result: Result<String, String>) -> Self {
        match result {
            Ok(url) => Self { url: Some(url), error: None },
            Err(error) => Self { url: None, error: Some(error) },
        }
    }
}

/// Native URL events can arrive before the page registers its listener. Keep them until loading ends.
#[derive(Default)]
pub struct Delivery {
    pub ready: bool,
    pub pending: Vec<Request>,
}

pub fn deliver(window: &tauri::WebviewWindow, request: Request) {
    let detail = serde_json::to_string(&request).expect("serialize a link request");
    if let Err(error) = window.eval(format!("window.dispatchEvent(new CustomEvent('pr-inbox:open', {{detail:{detail}}}));")) {
        eprintln!("Could not deliver the PR link to the window: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_pr_subpages_and_discussion_links() {
        for url in [
            "https://github.com/owner/repo/pull/42", "https://github.com/owner/repo/pull/042/",
            "https://github.com/owner/repo/pull/42/files#diff-abc", "https://github.com/owner/repo/pull/42/commits/abc?x=1",
            "https://github.com/owner/repo/pull/42/checks", "https://github.com/owner/repo/pull/42#issuecomment-1",
        ] {
            assert_eq!(normalize_pr_url(url).unwrap(), "https://github.com/owner/repo/pull/42");
        }
    }

    #[test]
    fn refuses_other_hosts_credentials_ports_and_invalid_numbers() {
        for url in [
            "http://github.com/o/r/pull/1", "https://github.com.evil.test/o/r/pull/1", "https://github.com@evil.test/o/r/pull/1",
            "https://someone@github.com/o/r/pull/1", "https://github.com:4477/o/r/pull/1", "https://github.com/o/r/issues/1",
            "https://github.com/o/r/pull/0", "https://github.com/o/r/pull/-1", "https://github.com/o/r/pull/1e3",
            "https://github.com/o/r/pull/18446744073709551616", "https://github.com/o/r/pull/1/merge", "https://github.com/o%2Fr/r/pull/1",
        ] {
            assert!(normalize_pr_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn accepts_only_one_open_action_with_one_url() {
        let mut link = Url::parse("pr-inbox://open").unwrap();
        link.query_pairs_mut().append_pair("url", "https://github.com/o/r/pull/7/files#diff");
        assert_eq!(parse(&link).unwrap(), "https://github.com/o/r/pull/7");
        for link in [
            "pr-inbox://merge?url=https://github.com/o/r/pull/7", "pr-inbox://open",
            "pr-inbox://open?url=https://github.com/o/r/pull/7&action=merge",
            "pr-inbox://open?url=https://github.com/o/r/pull/7&url=https://github.com/o/r/pull/8",
            "pr-inbox://open?url=javascript:alert(1)", "pr-inbox://open/path?url=https://github.com/o/r/pull/7",
        ] {
            assert!(parse(&Url::parse(link).unwrap()).is_err(), "{link}");
        }
    }
}
