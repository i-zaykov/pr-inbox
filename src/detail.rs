// One PR in full, for the side panel: its description and every review and comment as GitHub renders them,
// its labels and its checks. GitHub's HTML is already sanitised, so the page shows it as it comes.
// Images a private repo serves (a screenshot committed to a branch) need your token, so `image` fetches them.
use serde::Serialize;
use serde_json::Value;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

use crate::util::{first_line, sh};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Detail {
    body_html: String,
    labels: Vec<String>,
    /// Reviews and comments, oldest first. A review with no text and no verdict is left out.
    timeline: Vec<Entry>,
    checks: Vec<CheckRun>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    author: String,
    bot: bool,
    /// The review's verdict (APPROVED, CHANGES_REQUESTED, COMMENTED), or empty for a comment.
    state: String,
    body_html: String,
    at: String,
    url: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CheckRun {
    name: String,
    /// success, failure, cancelled, skipped, neutral, timed_out, action_required, or running.
    state: String,
    url: String,
}

const HTML: &str = "Accept: application/vnd.github.full+json";

async fn api(path: &str) -> Result<Value, String> {
    let out = sh("gh", &["api", "-H", HTML, path], Duration::from_secs(30)).await;
    if out.code != 0 {
        return Err(first_line(&out.stderr));
    }
    serde_json::from_str(&out.stdout).map_err(|e| format!("GitHub sent something unreadable: {e}"))
}

fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

fn entry(v: &Value, state: String, at: &str) -> Entry {
    Entry {
        author: text(&v["user"]["login"]),
        bot: v["user"]["type"] == "Bot",
        state,
        body_html: text(&v["body_html"]),
        at: text(&v[at]),
        url: text(&v["html_url"]),
    }
}

/// `repo` is owner/name, checked by the caller.
pub async fn fetch(repo: &str, number: u64) -> Result<Detail, String> {
    let pr = api(&format!("repos/{repo}/pulls/{number}")).await?;
    let sha = text(&pr["head"]["sha"]);
    let paths = [
        format!("repos/{repo}/pulls/{number}/reviews?per_page=100"),
        format!("repos/{repo}/issues/{number}/comments?per_page=100"),
        format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
    ];
    let (reviews, comments, checks) = tokio::join!(api(&paths[0]), api(&paths[1]), api(&paths[2]));
    let mut timeline: Vec<Entry> = Vec::new();
    for r in reviews?.as_array().into_iter().flatten() {
        let state = text(&r["state"]);
        if state == "PENDING" || (state == "COMMENTED" && text(&r["body"]).trim().is_empty()) {
            continue;
        }
        timeline.push(entry(r, state, "submitted_at"));
    }
    for c in comments?.as_array().into_iter().flatten() {
        timeline.push(entry(c, String::new(), "created_at"));
    }
    timeline.sort_by(|a, b| a.at.cmp(&b.at));
    // Checks are a nice-to-have: a repo without Actions access still shows the rest.
    let checks = checks.ok().and_then(|c| c["check_runs"].as_array().cloned()).unwrap_or_default();
    let checks = checks
        .iter()
        .map(|c| CheckRun {
            name: text(&c["name"]),
            state: if c["status"] == "completed" { text(&c["conclusion"]) } else { "running".into() },
            url: text(&c["html_url"]),
        })
        .collect();
    Ok(Detail {
        body_html: text(&pr["body_html"]),
        labels: pr["labels"].as_array().into_iter().flatten().map(|l| text(&l["name"])).collect(),
        timeline,
        checks,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct File {
    name: String,
    /// The name before a rename, when GitHub reports one.
    previous: String,
    /// added, removed, modified, renamed, copied, changed or unchanged.
    status: String,
    additions: u64,
    deletions: u64,
    /// The unified diff of the file; empty for a binary file or one too large for GitHub to show.
    patch: String,
}

/// The PR's changed files with their diffs, up to GitHub's limit of 3000.
pub async fn files(repo: &str, number: u64) -> Result<Vec<File>, String> {
    let path = format!("repos/{repo}/pulls/{number}/files?per_page=100");
    let out = sh("gh", &["api", "--paginate", "--slurp", &path], Duration::from_secs(60)).await;
    if out.code != 0 {
        return Err(first_line(&out.stderr));
    }
    let pages: Value = serde_json::from_str(&out.stdout).map_err(|e| format!("GitHub sent something unreadable: {e}"))?;
    Ok(pages
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_array)
        .flatten()
        .map(|f| File {
            name: text(&f["filename"]),
            previous: text(&f["previous_filename"]),
            status: text(&f["status"]),
            additions: f["additions"].as_u64().unwrap_or(0),
            deletions: f["deletions"].as_u64().unwrap_or(0),
            patch: text(&f["patch"]),
        })
        .collect())
}

/// People who can be asked to review in `repo`: its assignable users, which read access can list.
pub async fn people(repo: &str) -> Result<Vec<String>, String> {
    let path = format!("repos/{repo}/assignees?per_page=100");
    let out = sh("gh", &["api", "--paginate", "--jq", ".[].login", &path], Duration::from_secs(30)).await;
    if out.code != 0 {
        return Err(first_line(&out.stderr));
    }
    let mut people: Vec<String> = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    people.sort_by_key(|p| p.to_lowercase());
    Ok(people)
}

/// A GitHub user name: letters, digits and single hyphens, at most 39 characters.
pub fn is_login(s: &str) -> bool {
    !s.is_empty() && s.len() <= 39 && !s.starts_with('-') && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A file path inside a repo as raw.githubusercontent.com takes it: owner/repo/ref/path, ref may hold slashes.
pub fn is_raw_path(p: &str) -> bool {
    p.split('/').count() >= 4
        && !p.split('/').any(|s| s.is_empty() || s == "." || s == "..")
        && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-/@%+".contains(c))
}

/// Fetches one repo file with your gh token. The token goes to curl on stdin, so `ps` never shows it.
pub async fn image(path: &str) -> Result<Vec<u8>, String> {
    let token = sh("gh", &["auth", "token"], Duration::from_secs(10)).await;
    if token.code != 0 {
        return Err(first_line(&token.stderr));
    }
    let url = format!("https://raw.githubusercontent.com/{path}");
    let mut child = tokio::process::Command::new("curl")
        .args(["-sfL", "--max-time", "30", "--max-filesize", "26214400", "-H", "@-", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("curl: {e}"))?;
    let mut stdin = child.stdin.take().ok_or("curl: no stdin")?;
    stdin.write_all(format!("Authorization: token {}\n", token.stdout.trim()).as_bytes()).await.map_err(|e| e.to_string())?;
    drop(stdin);
    let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("GitHub refused {path}"));
    }
    Ok(out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_paths_stay_inside_github() {
        assert!(is_raw_path("lenus-ehealth/airflow/pr-assets/dbt-durable-execution/reattach.png"));
        assert!(!is_raw_path("lenus-ehealth/airflow/main"));
        assert!(!is_raw_path("a/b/../../evil.png"));
        assert!(!is_raw_path("a/b/c//d.png"));
        assert!(!is_raw_path("a/b/c/d.png?x=1"));
    }

    #[test]
    fn logins_are_plain_names() {
        assert!(is_login("i-zaykov"));
        assert!(!is_login("--add-reviewer"));
        assert!(!is_login("a b"));
        assert!(!is_login(""));
    }
}
