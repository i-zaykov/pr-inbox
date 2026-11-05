// The thermo-nuclear review ledger. A verdict follows an unchanged PR patch across a rebase once the
// new head's checks pass. Changed patches and missing Git evidence keep the old verdict outdated.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::github::{fetch_linked_pr, is_full_sha, is_pr_url, safe_ref, Ci, Pr};
use crate::util::{checkouts_dir, data_dir, now_ms, read_json, set_aside, write_atomic, GIT_ENV};
use crate::workspace;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
}

/// The GitHub PR and exact comparison that a review actually inspected.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReviewScope {
    pub repo: String,
    pub number: u64,
    pub branch: String,
    pub base: String,
    pub base_sha: String,
    pub head_sha: String,
}

impl ReviewScope {
    pub fn from_pr(pr: &Pr) -> Self {
        Self {
            repo: pr.repo.clone(), number: pr.number, branch: pr.head.clone(), base: pr.base.clone(),
            base_sha: pr.base_sha.clone(), head_sha: pr.head_sha.clone(),
        }
    }

    pub fn matches(&self, pr: &Pr) -> bool {
        self == &Self::from_pr(pr)
    }

    fn validate(&self) -> Result<(), String> {
        if self.number == 0 || !is_pr_url(&format!("https://github.com/{}/pull/{}", self.repo, self.number)) {
            return Err("review scope needs a valid owner/repo and positive PR number".into());
        }
        if !safe_ref(&self.branch) || !safe_ref(&self.base) {
            return Err("review scope needs valid head and base branch names".into());
        }
        if !is_full_sha(&self.base_sha) || !is_full_sha(&self.head_sha) {
            return Err("review scope needs full base and head commit ids".into());
        }
        Ok(())
    }

    fn same_target(&self, pr: &Pr) -> bool {
        self.repo == pr.repo && self.number == pr.number && self.branch == pr.head && self.base == pr.base
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    repo: String,
    branch: String,
    sha: String,
    verdict: Verdict,
    summary: String,
    at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scope: Option<ReviewScope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    patch: Option<ReviewedPatch>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct ReviewedPatch {
    base: String,
    merge_base: String,
    id: String,
}

/// Latest review per PR, plus legacy records keyed by `owner/repo#branch`.
pub type Ledger = BTreeMap<String, Review>;

fn key(repo: &str, branch: &str) -> String {
    format!("{repo}#{branch}")
}

#[derive(Serialize, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum ThermoState {
    Pass,
    Fail,
    Outdated,
}

/// A PR's thermo-nuclear review as the page shows it.
#[derive(Serialize, Debug)]
pub struct Standing {
    state: ThermoState,
    sha: String,
    summary: String,
    at: u64,
}

impl Standing {
    pub fn passed(&self) -> bool {
        self.state == ThermoState::Pass
    }
}

/// A shared commit or branch never transfers a scoped review to another PR. Always use the newest
/// evidence, even when an older record happens to match the current head. Ties fail closed.
fn review_for<'a>(ledger: &'a Ledger, pr: &Pr) -> Option<&'a Review> {
    ledger
        .values()
        .filter(|r| match &r.scope {
            Some(scope) => scope.repo == pr.repo && scope.number == pr.number,
            None => r.repo == pr.repo && (r.branch == pr.head || r.branch == workspace::own_branch(pr.number)),
        })
        .max_by_key(|r| (r.at, valid_scope(r).is_none(), r.verdict == Verdict::Fail))
}

fn valid_scope(review: &Review) -> Option<&ReviewScope> {
    review.scope.as_ref().filter(|scope| {
        scope.validate().is_ok() && scope.repo == review.repo && scope.branch == review.branch && scope.head_sha == review.sha
    })
}

fn standing(r: &Review, pr: &Pr, same_patch: bool) -> Standing {
    let current = valid_scope(r).is_some_and(|scope| {
        scope.matches(pr) || (scope.same_target(pr) && same_patch && pr.ci == Ci::Pass)
    });
    let state = match r.verdict {
        _ if !current => ThermoState::Outdated,
        Verdict::Pass => ThermoState::Pass,
        Verdict::Fail => ThermoState::Fail,
    };
    Standing { state, sha: r.sha.clone(), summary: r.summary.clone(), at: r.at }
}

const PROOF_TIMEOUT: Duration = Duration::from_secs(8);

/// Check different PRs concurrently, with at most four Git comparisons in flight. Every proof,
/// including waiting for a slot, has a deadline so an unavailable remote cannot hang the inbox.
pub async fn standings(ledger: &Ledger, prs: &[Pr]) -> HashMap<String, Standing> {
    let mut pending = tokio::task::JoinSet::new();
    for pr in prs {
        let Some(review) = review_for(ledger, pr).cloned() else { continue };
        let pr = pr.clone();
        pending.spawn(async move {
            let needs_proof = valid_scope(&review).is_some_and(|scope| scope.same_target(&pr) && !scope.matches(&pr));
            let same_patch = needs_proof && pr.ci == Ci::Pass && cached_equivalent(&review, &pr).await;
            (pr.url.clone(), standing(&review, &pr, same_patch))
        });
    }
    let mut result = HashMap::new();
    while let Some(next) = pending.join_next().await {
        if let Ok((url, standing)) = next {
            result.insert(url, standing);
        }
    }
    result
}

async fn cached_equivalent(review: &Review, pr: &Pr) -> bool {
    static CACHE: OnceLock<Mutex<HashMap<String, (Instant, bool)>>> = OnceLock::new();
    static WORKERS: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let dir = checkouts_dir().join(&pr.name);
    let cache_key = serde_json::to_string(&(review, &pr.head_sha, &pr.base, &pr.base_sha, &dir)).unwrap_or_default();
    if let Some((at, same)) = cache.lock().unwrap().get(&cache_key) {
        if at.elapsed() < Duration::from_secs(if *same { 3600 } else { 60 }) {
            return *same;
        }
    }
    let same = tokio::time::timeout(PROOF_TIMEOUT, async {
        let Ok(_permit) = WORKERS.get_or_init(|| tokio::sync::Semaphore::new(4)).acquire().await else { return false };
        equivalent(&dir, review, pr).await.unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let mut cache = cache.lock().unwrap();
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(cache_key, (Instant::now(), same));
    same
}

fn git_command(dir: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(["--no-replace-objects", "-C"]).arg(dir).args(args).envs(GIT_ENV);
    // Read object data only. Missing objects are fetched explicitly, with a deadline, below.
    cmd.env("GIT_NO_LAZY_FETCH", "1").env("GIT_OPTIONAL_LOCKS", "0").kill_on_drop(true);
    cmd
}

async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = git_command(dir, args).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

async fn matches_repo(dir: &Path, repo: &str) -> bool {
    let Some(origin) = git(dir, &["remote", "get-url", "origin"]).await else { return false };
    let origin = origin.trim_end_matches(".git").to_lowercase();
    let repo = repo.to_lowercase();
    origin.ends_with(&format!("/{repo}")) || origin.ends_with(&format!(":{repo}"))
}

/// Fetch only missing objects, without changing branches, FETCH_HEAD or the working tree.
async fn ensure_commits(dir: &Path, shas: &[&str]) -> Option<()> {
    let mut missing = Vec::new();
    for sha in shas {
        if !is_full_sha(sha) {
            return None;
        }
        if git(dir, &["cat-file", "-e", &format!("{sha}^{{commit}}")]).await.is_none() && !missing.contains(sha) {
            missing.push(*sha);
        }
    }
    if !missing.is_empty() {
        let mut args = vec!["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--no-recurse-submodules", "origin"];
        args.extend(missing);
        git(dir, &args).await?;
    }
    Some(())
}

async fn merge_base(dir: &Path, base: &str, head: &str) -> Option<String> {
    let sha = git(dir, &["merge-base", "--all", base, head]).await?;
    // Multiple merge bases need a virtual merge and cannot identify one reviewed patch safely.
    is_full_sha(&sha).then_some(sha)
}

/// Keep whitespace, context, filenames, modes and binary contents. Only patch metadata such as
/// commit ids and line offsets can change without changing this fingerprint. Pipe raw bytes so a
/// non-UTF-8 source file cannot collapse two different patches into the same replacement characters.
async fn patch_id(dir: &Path, base: &str, head: &str) -> Option<String> {
    let mut diff = git_command(
        dir,
        &[
            "diff", "--no-ext-diff", "--no-textconv", "--binary", "--full-index", "--no-renames", "--no-color", "--no-relative",
            "--src-prefix=a/", "--dst-prefix=b/", "--unified=3", "--inter-hunk-context=0", "--diff-algorithm=myers",
            "--no-indent-heuristic", "--ignore-submodules=none", "-O/dev/null", base, head, "--",
        ],
    );
    let mut diff = diff.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().ok()?;
    let input: Stdio = diff.stdout.take()?.try_into().ok()?;
    let hash = git_command(dir, &["patch-id", "--verbatim"]).stdin(input).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
    let (diff, hash) = tokio::join!(diff.wait_with_output(), hash.wait_with_output());
    if !diff.ok()?.status.success() {
        return None;
    }
    let hash = hash.ok()?;
    if !hash.status.success() {
        return None;
    }
    let text = std::str::from_utf8(&hash.stdout).ok()?.trim();
    if text.is_empty() {
        return Some("empty".into());
    }
    let id = text.split_whitespace().next()?;
    is_full_sha(id).then(|| id.to_string())
}

async fn equivalent(dir: &Path, review: &Review, pr: &Pr) -> Option<bool> {
    let scope = valid_scope(review)?;
    if !scope.same_target(pr) || !is_full_sha(&pr.head_sha) || !is_full_sha(&pr.base_sha) || !matches_repo(dir, &pr.repo).await {
        return None;
    }
    if review.patch.as_ref().is_some_and(|patch| patch.base != pr.base || !is_full_sha(&patch.merge_base)) {
        return None;
    }
    ensure_commits(dir, &[&pr.head_sha, &pr.base_sha]).await?;
    let old = match &review.patch {
        Some(patch) => patch.id.clone(),
        None => {
            ensure_commits(dir, &[&scope.head_sha, &scope.base_sha]).await?;
            let old_base = merge_base(dir, &scope.base_sha, &scope.head_sha).await?;
            patch_id(dir, &old_base, &review.sha).await?
        }
    };
    let base = merge_base(dir, &pr.base_sha, &pr.head_sha).await?;
    Some(old == patch_id(dir, &base, &pr.head_sha).await?)
}

async fn reviewed_patch(dir: &Path, scope: &ReviewScope) -> Option<ReviewedPatch> {
    if scope.validate().is_err() || !matches_repo(dir, &scope.repo).await {
        return None;
    }
    let merge_base = merge_base(dir, &scope.base_sha, &scope.head_sha).await?;
    let id = patch_id(dir, &merge_base, &scope.head_sha).await?;
    Some(ReviewedPatch { base: scope.base.clone(), merge_base, id })
}

fn ledger_file() -> std::path::PathBuf {
    data_dir().join("reviews.json")
}

pub fn load() -> Ledger {
    read_json(&ledger_file())
}

/// Several reviews can finish at once, each running `record-review` as its own process. An exclusive
/// lock makes their read-modify-write one at a time, and a rename makes each write all or nothing.
fn with_lock<T>(work: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    fs::create_dir_all(data_dir()).map_err(|e| e.to_string())?;
    let lock = File::create(data_dir().join("reviews.lock")).map_err(|e| e.to_string())?;
    // SAFETY: flock on a descriptor we own; the lock is released when `lock` is dropped.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err("could not lock the review ledger".into());
    }
    work()
}

fn record(review: Review) -> Result<String, String> {
    let key = match &review.scope {
        Some(scope) => format!("{}#pr:{}", scope.repo, scope.number),
        None => key(&review.repo, &review.branch),
    };
    let verdict = if review.verdict == Verdict::Pass { "PASS" } else { "FAIL" };
    let status = if review.scope.is_some() { verdict } else { "OUTDATED legacy review" };
    let line = format!("Recorded {status} for {key} at {}", &review.sha[..review.sha.len().min(7)]);
    with_lock(|| {
        let file = ledger_file();
        let mut ledger: Ledger = match fs::read_to_string(&file) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(ledger) => ledger,
                // Never overwrite a ledger this binary cannot read: keep it beside the new one.
                Err(_) => {
                    set_aside(&file);
                    Ledger::new()
                }
            },
            Err(_) => Ledger::new(),
        };
        let replace = ledger.get(&key).is_none_or(|existing| {
            review.at > existing.at || (review.at == existing.at && review.verdict == Verdict::Fail)
        });
        if !replace {
            return Err("a newer or conflicting review is already recorded for this PR".into());
        }
        ledger.insert(key, review);
        write_atomic(&file, &serde_json::to_string_pretty(&ledger).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })?;
    Ok(line)
}

/// App callers validate the current GitHub snapshot before recording. Persist patch evidence against
/// that exact base/head pair; a moving local origin ref must never change what the verdict covers.
pub async fn record_verified(scope: &ReviewScope, verdict: Verdict, summary: &str, dir: &Path) -> Result<String, String> {
    scope.validate()?;
    let patch = tokio::time::timeout(PROOF_TIMEOUT, reviewed_patch(dir, scope)).await.ok().flatten();
    record(Review {
        repo: scope.repo.clone(), branch: scope.branch.clone(), sha: scope.head_sha.clone(), verdict,
        summary: summary.into(), at: now_ms(), scope: Some(scope.clone()), patch,
    })
}

fn parse_review_args(args: &[String]) -> Result<Review, String> {
    let mut flags = HashMap::new();
    let (pairs, remainder) = args.as_chunks::<2>();
    for pair in pairs {
        let name = pair[0].as_str();
        if !matches!(name, "--repo" | "--pr" | "--branch" | "--sha" | "--base" | "--base-sha" | "--verdict" | "--summary") {
            return Err(format!("unknown review argument: {name}"));
        }
        if flags.insert(name, pair[1].clone()).is_some() {
            return Err(format!("duplicate review argument: {name}"));
        }
    }
    if let Some(name) = remainder.first() {
        return Err(format!("{name} requires a value"));
    }
    let flag = |name: &str| flags.get(name).cloned();
    let need = |name: &str| flag(name).filter(|v| !v.trim().is_empty()).ok_or(format!("{name} is required"));
    let verdict = match need("--verdict")?.to_lowercase().as_str() {
        "pass" => Verdict::Pass,
        "fail" => Verdict::Fail,
        other => return Err(format!("--verdict must be pass or fail, not {other}")),
    };
    let sha = need("--sha")?;
    if !is_full_sha(&sha) {
        return Err("--sha must be a full 40-character commit id".into());
    }
    let repo = need("--repo")?;
    let branch = need("--branch")?;
    let scope = if flag("--pr").is_some() || flag("--base-sha").is_some() {
        let scope = ReviewScope {
            repo: repo.clone(), number: need("--pr")?.parse().map_err(|_| "--pr must be a positive PR number")?,
            branch: branch.clone(), base: need("--base")?, base_sha: need("--base-sha")?, head_sha: sha.clone(),
        };
        scope.validate()?;
        Some(scope)
    } else {
        // Keep old callers' history, but never infer a PR identity or base from a branch or reflog.
        None
    };
    Ok(Review { repo, branch, sha, verdict, summary: flag("--summary").unwrap_or_default(), at: now_ms(), scope, patch: None })
}

async fn verify_local_head(scope: &ReviewScope, dir: &Path) -> Result<(), String> {
    if !matches_repo(dir, &scope.repo).await {
        return Err("checkout origin does not match the reviewed repository".into());
    }
    if git(dir, &["rev-parse", "HEAD"]).await.as_deref() != Some(scope.head_sha.as_str()) {
        return Err("checkout HEAD differs from the reviewed commit".into());
    }
    match git(dir, &["status", "--porcelain", "--untracked-files=all", "--ignore-submodules=none"]).await {
        Some(status) if status.is_empty() => Ok(()),
        _ => Err("review requires a clean working tree, including untracked files".into()),
    }
}

fn verify_pr_scope(scope: &ReviewScope, pr: &Pr) -> Result<(), String> {
    if pr.state != "OPEN" {
        return Err("reviewed PR is no longer open".into());
    }
    if !scope.matches(pr) {
        return Err("PR head or base changed since review; review the current diff before recording".into());
    }
    Ok(())
}

/// `pr-inbox record-review --repo owner/name --pr 7 --branch b --sha s --base main
/// --base-sha s --verdict pass|fail [--summary text]`. Calls without a PR scope remain outdated.
pub fn record_from_args(args: &[String]) -> Result<String, String> {
    let review = parse_review_args(args)?;
    let Some(scope) = &review.scope else { return record(review) };
    let dir = std::env::current_dir().map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        // Fail before contacting GitHub if the command cannot describe the local reviewed code.
        tokio::time::timeout(PROOF_TIMEOUT, verify_local_head(scope, &dir)).await
            .map_err(|_| "timed out verifying the reviewed checkout")??;
        let url = format!("https://github.com/{}/pull/{}", scope.repo, scope.number);
        let linked = fetch_linked_pr(&url).await?;
        verify_pr_scope(scope, &linked.pr)?;
        if review.verdict == Verdict::Pass {
            crate::review_flow::checks_pass(&url).await?;
            verify_pr_scope(scope, &fetch_linked_pr(&url).await?.pr)?;
        }
        record_verified(scope, review.verdict, &review.summary, &dir).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    fn pr() -> Pr {
        Pr {
            repo: "o/r".into(), number: 7, head: "feat".into(), head_sha: "b".repeat(40),
            base: "main".into(), base_sha: "c".repeat(40), state: "OPEN".into(), ..Default::default()
        }
    }

    fn review(branch: &str, sha: char, verdict: Verdict) -> (String, Review) {
        let pr = Pr { head: branch.into(), head_sha: sha.to_string().repeat(40), ..pr() };
        let r = Review {
            repo: "o/r".into(), branch: branch.into(), sha: pr.head_sha.clone(), verdict,
            summary: format!("on {branch}"), at: 1, scope: Some(ReviewScope::from_pr(&pr)), patch: None,
        };
        (key(&r.repo, &r.branch), r)
    }

    #[test]
    fn newest_evidence_wins_over_an_older_pass_for_the_exact_head() {
        let pr = pr();
        let (_, old) = review("feat", 'b', Verdict::Pass);
        let newer = Review { verdict: Verdict::Fail, at: 2, summary: "new blocker".into(), ..old.clone() };
        let ledger: Ledger = [("a-old-pass".into(), old), ("z-new-fail".into(), newer)].into_iter().collect();
        let s = standing(review_for(&ledger, &pr).unwrap(), &pr, false);
        assert_eq!((s.state, s.summary.as_str()), (ThermoState::Fail, "new blocker"));
        let moved = Pr { head_sha: "d".repeat(40), ..pr.clone() };
        let s = standing(review_for(&ledger, &moved).unwrap(), &moved, false);
        assert_eq!((s.state, s.summary.as_str()), (ThermoState::Outdated, "new blocker"));
        let page = serde_json::to_value(standing(review_for(&ledger, &pr).unwrap(), &pr, false)).unwrap();
        assert_eq!(page, serde_json::json!({ "state": "fail", "sha": "b".repeat(40), "summary": "new blocker", "at": 2 }));
        let mut newer_other_head = ledger["z-new-fail"].clone();
        newer_other_head.sha = "a".repeat(40);
        newer_other_head.scope.as_mut().unwrap().head_sha = newer_other_head.sha.clone();
        let mut ledger = ledger;
        ledger.insert("z-new-fail".into(), newer_other_head);
        assert_eq!(standing(review_for(&ledger, &pr).unwrap(), &pr, false).state, ThermoState::Outdated);
    }

    #[test]
    fn equal_timestamps_prefer_fail_and_other_prs_cannot_borrow_a_review() {
        let (_, pass) = review("feat", 'b', Verdict::Pass);
        let fail = Review { verdict: Verdict::Fail, ..pass.clone() };
        let ledger: Ledger = [("a-fail".into(), fail), ("z-pass".into(), pass)].into_iter().collect();
        assert_eq!(standing(review_for(&ledger, &pr()).unwrap(), &pr(), false).state, ThermoState::Fail);
        assert!(review_for(&ledger, &Pr { number: 8, ..pr() }).is_none());
        assert!(review_for(&ledger, &Pr { repo: "another/repo".into(), ..pr() }).is_none());
    }

    #[test]
    fn retargets_and_changed_base_commits_are_outdated_even_at_the_reviewed_head() {
        let (_, review) = review("feat", 'b', Verdict::Pass);
        assert_eq!(standing(&review, &pr(), false).state, ThermoState::Pass);
        let retargeted = Pr { base: "release".into(), ..pr() };
        assert_eq!(standing(&review, &retargeted, true).state, ThermoState::Outdated);
        let changed_base = Pr { base_sha: "d".repeat(40), ..pr() };
        assert_eq!(standing(&review, &changed_base, false).state, ThermoState::Outdated);
        let renamed = Pr { head: "another-branch".into(), ..pr() };
        assert_eq!(standing(&review, &renamed, true).state, ThermoState::Outdated);
    }

    #[test]
    fn legacy_records_keep_branch_history_but_never_establish_a_pass() {
        let legacy_json = serde_json::json!({
            "repo": "o/r", "branch": "feat", "sha": "b".repeat(40), "verdict": "pass", "summary": "old pass", "at": 3,
        });
        let legacy: Review = serde_json::from_value(legacy_json).unwrap();
        let ledger: Ledger = [("o/r#feat".into(), legacy.clone())].into_iter().collect();
        assert_eq!(standing(review_for(&ledger, &pr()).unwrap(), &pr(), true).state, ThermoState::Outdated);
        assert!(review_for(&ledger, &Pr { head: "unrelated".into(), ..pr() }).is_none());
        let own = Review { branch: "pr-inbox/7".into(), ..legacy };
        let ledger: Ledger = [("o/r#pr-inbox/7".into(), own)].into_iter().collect();
        assert!(review_for(&ledger, &pr()).is_some());
        assert!(review_for(&ledger, &Pr { number: 8, ..pr() }).is_none());
    }

    #[test]
    fn scope_roundtrips_and_current_pr_verification_refuses_changed_or_closed_prs() {
        let scope = ReviewScope::from_pr(&pr());
        assert!(scope.matches(&pr()));
        assert!(verify_pr_scope(&scope, &pr()).is_ok());
        for changed in [
            Pr { head_sha: "a".repeat(40), ..pr() },
            Pr { base_sha: "a".repeat(40), ..pr() },
            Pr { base: "release".into(), ..pr() },
            Pr { number: 8, ..pr() },
            Pr { state: "CLOSED".into(), ..pr() },
        ] {
            assert!(verify_pr_scope(&scope, &changed).is_err());
        }
        let json = serde_json::to_value(&scope).unwrap();
        assert_eq!(json["baseSha"], "c".repeat(40));
        assert_eq!(json["headSha"], "b".repeat(40));
        assert_eq!(serde_json::from_value::<ReviewScope>(json).unwrap(), scope);
    }

    #[test]
    fn cli_scope_requires_pr_base_and_full_base_sha_without_promoting_legacy_calls() {
        let strings = |items: &[&str]| items.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut args = strings(&["--repo", "o/r", "--branch", "feat", "--sha", &"b".repeat(40), "--verdict", "pass"]);
        assert!(parse_review_args(&args).unwrap().scope.is_none());
        let mut missing_value = args.clone();
        missing_value.push("--pr".into());
        assert!(parse_review_args(&missing_value).is_err());
        args.extend(strings(&["--base", "main"]));
        assert!(parse_review_args(&args).unwrap().scope.is_none());
        args.extend(strings(&["--pr", "7"]));
        assert!(parse_review_args(&args).is_err());
        args.extend(strings(&["--base-sha", &"c".repeat(40)]));
        assert_eq!(parse_review_args(&args).unwrap().scope, Some(ReviewScope::from_pr(&pr())));
        let mut duplicate = args.clone();
        duplicate.extend(strings(&["--pr", "8"]));
        assert!(parse_review_args(&duplicate).is_err());
        *args.last_mut().unwrap() = "short".into();
        assert!(parse_review_args(&args).is_err());
    }

    struct Repo {
        tmp: PathBuf,
        dir: PathBuf,
        base: String,
        reviewed: String,
    }

    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.tmp);
        }
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn base_ref(dir: &Path, sha: &str, date: &str) {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-C"])
            .arg(dir)
            .args(["update-ref", "--create-reflog", "refs/remotes/origin/main", sha])
            .env("GIT_COMMITTER_DATE", date)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    impl Repo {
        fn new() -> Self {
            let tmp = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
            let dir = tmp.join("r");
            let origin = tmp.join("o/r.git");
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(&origin).unwrap();
            run_git(&origin, &["init", "--quiet", "--bare"]);
            run_git(&dir, &["init", "--quiet", "-b", "main"]);
            run_git(&dir, &["remote", "add", "origin", &origin.to_string_lossy()]);
            let content: String = (0..20).map(|i| format!("line {i}\n")).collect();
            fs::write(dir.join("code"), content).unwrap();
            fs::write(dir.join("binary"), [0, 1, 2, 3]).unwrap();
            run_git(&dir, &["add", "."]);
            run_git(&dir, &["commit", "--quiet", "-m", "base"]);
            let base = run_git(&dir, &["rev-parse", "HEAD"]);
            base_ref(&dir, &base, "2025-01-01T00:00:00Z");
            run_git(&dir, &["checkout", "--quiet", "-b", "feat"]);
            let code = fs::read_to_string(dir.join("code")).unwrap().replace("line 10\n", "reviewed code\n");
            fs::write(dir.join("code"), code).unwrap();
            fs::write(dir.join("binary"), [0, 4, 5, 6]).unwrap();
            run_git(&dir, &["add", "."]);
            run_git(&dir, &["commit", "--quiet", "-m", "feature"]);
            let reviewed = run_git(&dir, &["rev-parse", "HEAD"]);
            Self { tmp, dir, base, reviewed }
        }

        async fn review(&self) -> Review {
            let scope = ReviewScope {
                repo: "o/r".into(), number: 7, branch: "feat".into(), base: "main".into(),
                base_sha: self.base.clone(), head_sha: self.reviewed.clone(),
            };
            Review {
                repo: "o/r".into(), branch: "feat".into(), sha: self.reviewed.clone(), verdict: Verdict::Pass,
                summary: "no blockers".into(), at: 1_735_693_200_000,
                patch: Some(reviewed_patch(&self.dir, &scope).await.unwrap()), scope: Some(scope),
            }
        }

        fn rebase(&self) -> Pr {
            run_git(&self.dir, &["checkout", "--quiet", "main"]);
            let code = fs::read_to_string(self.dir.join("code")).unwrap();
            fs::write(self.dir.join("code"), format!("upstream header\n{code}")).unwrap();
            fs::write(self.dir.join("other"), "upstream change\n").unwrap();
            run_git(&self.dir, &["add", "."]);
            run_git(&self.dir, &["commit", "--quiet", "-m", "upstream"]);
            let base_sha = run_git(&self.dir, &["rev-parse", "HEAD"]);
            base_ref(&self.dir, &base_sha, "2025-01-02T00:00:00Z");
            run_git(&self.dir, &["checkout", "--quiet", "feat"]);
            run_git(&self.dir, &["rebase", "--quiet", "main"]);
            let head_sha = run_git(&self.dir, &["rev-parse", "HEAD"]);
            assert_ne!(head_sha, self.reviewed);
            Pr { repo: "o/r".into(), number: 7, name: "r".into(), head: "feat".into(), head_sha, base: "main".into(), base_sha, ci: Ci::Pass, ..Default::default() }
        }

        fn commit(&self, pr: &mut Pr) {
            run_git(&self.dir, &["add", "."]);
            run_git(&self.dir, &["commit", "--quiet", "-m", "changed patch"]);
            pr.head_sha = run_git(&self.dir, &["rev-parse", "HEAD"]);
        }
    }

    #[tokio::test]
    async fn an_unchanged_patch_survives_a_rebase_and_line_offsets_once_ci_passes() {
        let repo = Repo::new();
        let review = repo.review().await;
        assert_eq!(review.patch.as_ref().unwrap().merge_base, repo.base);
        let mut pr = repo.rebase();
        let same = equivalent(&repo.dir, &review, &pr).await.unwrap();
        assert!(same);
        assert_eq!(standing(&review, &pr, same).state, ThermoState::Pass);
        for ci in [Ci::None, Ci::Running, Ci::Fail] {
            pr.ci = ci;
            assert_eq!(standing(&review, &pr, same).state, ThermoState::Outdated, "{ci:?}");
        }
        pr.ci = Ci::Pass;
        let failed_review = Review { verdict: Verdict::Fail, ..review.clone() };
        assert_eq!(standing(&failed_review, &pr, same).state, ThermoState::Fail);
        // The persisted fingerprint still works when the old commit has been pruned locally/remotely.
        let mut pruned = review;
        pruned.sha = "f".repeat(40);
        pruned.scope.as_mut().unwrap().head_sha = pruned.sha.clone();
        assert_eq!(equivalent(&repo.dir, &pruned, &pr).await, Some(true));
    }

    #[tokio::test]
    async fn passing_ci_does_not_cover_a_changed_patch_or_changed_whitespace() {
        let repo = Repo::new();
        let review = repo.review().await;
        let mut pr = repo.rebase();
        let code = fs::read_to_string(repo.dir.join("code")).unwrap().replace("reviewed code", "reviewed  code");
        fs::write(repo.dir.join("code"), code).unwrap();
        repo.commit(&mut pr);
        let same = equivalent(&repo.dir, &review, &pr).await.unwrap();
        assert!(!same);
        assert_eq!(standing(&review, &pr, same).state, ThermoState::Outdated);
    }

    #[tokio::test]
    async fn a_binary_change_or_mode_change_invalidates_the_review() {
        let repo = Repo::new();
        let review = repo.review().await;
        let mut pr = repo.rebase();
        fs::write(repo.dir.join("binary"), [0, 4, 5, 7]).unwrap();
        repo.commit(&mut pr);
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, Some(false));
        run_git(&repo.dir, &["reset", "--hard", "HEAD~1"]);
        run_git(&repo.dir, &["update-index", "--chmod=+x", "code"]);
        run_git(&repo.dir, &["commit", "--quiet", "-m", "mode"]);
        pr.head_sha = run_git(&repo.dir, &["rev-parse", "HEAD"]);
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, Some(false));
    }

    #[tokio::test]
    async fn a_scoped_review_without_a_fingerprint_needs_its_exact_commit_histories() {
        let repo = Repo::new();
        let mut review = repo.review().await;
        review.patch = None;
        let json = serde_json::to_string(&review).unwrap();
        assert!(!json.contains("patch"));
        let mut review: Review = serde_json::from_str(&json).unwrap();
        let pr = repo.rebase();
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, Some(true));
        review.sha = "f".repeat(40);
        review.scope.as_mut().unwrap().head_sha = review.sha.clone();
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, None);
        review.sha = repo.reviewed.clone();
        review.scope.as_mut().unwrap().head_sha = review.sha.clone();
        review.scope.as_mut().unwrap().base_sha = "f".repeat(40);
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, None);
        review.scope.as_mut().unwrap().base_sha = repo.base.clone();
        // Exact scope no longer relies on a reflog that may expire or refer to a previous target.
        run_git(&repo.dir, &["update-ref", "-d", "refs/remotes/origin/main"]);
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, Some(true));
        review.scope = None;
        assert_eq!(equivalent(&repo.dir, &review, &pr).await, None);
        assert_eq!(standing(&review, &pr, true).state, ThermoState::Outdated);
    }

    #[tokio::test]
    async fn fetching_missing_commit_objects_keeps_refs_fetch_head_and_working_tree_untouched() {
        let repo = Repo::new();
        let review = repo.review().await;
        run_git(&repo.dir, &["push", "--quiet", "origin", "main", "feat"]);
        let clone = repo.tmp.join("reader");
        run_git(&repo.tmp, &["clone", "--quiet", "--branch", "main", "o/r.git", "reader"]);
        fs::write(clone.join("unsaved"), "keep my work\n").unwrap();
        fs::write(clone.join(".git/FETCH_HEAD"), "keep fetch head\n").unwrap();
        let refs = run_git(&clone, &["show-ref"]);
        let head = run_git(&clone, &["rev-parse", "HEAD"]);
        let status = run_git(&clone, &["status", "--porcelain"]);
        let pr = repo.rebase();
        run_git(&repo.dir, &["push", "--quiet", "--force", "origin", "main", "feat"]);
        assert_eq!(equivalent(&clone, &review, &pr).await, Some(true));
        assert_eq!(run_git(&clone, &["show-ref"]), refs);
        assert_eq!(run_git(&clone, &["rev-parse", "HEAD"]), head);
        assert_eq!(run_git(&clone, &["status", "--porcelain"]), status);
        assert_eq!(fs::read_to_string(clone.join("unsaved")).unwrap(), "keep my work\n");
        assert_eq!(fs::read_to_string(clone.join(".git/FETCH_HEAD")).unwrap(), "keep fetch head\n");
    }

    #[tokio::test]
    async fn missing_repository_or_base_evidence_never_makes_a_review_current() {
        let repo = Repo::new();
        let review = repo.review().await;
        let pr = repo.rebase();
        assert_eq!(equivalent(&repo.tmp.join("missing"), &review, &pr).await, None);
        assert_eq!(equivalent(&repo.dir, &review, &Pr { base: "other".into(), ..pr.clone() }).await, None);
        assert_eq!(equivalent(&repo.dir, &review, &Pr { base_sha: String::new(), ..pr.clone() }).await, None);
        assert_eq!(equivalent(&repo.dir, &review, &Pr { repo: "somebody/else".into(), ..pr.clone() }).await, None);
        // A review of the exact current SHA still works without any Git history or CI information.
        assert_eq!(standing(&review, &Pr { head_sha: review.sha.clone(), base_sha: repo.base.clone(), ci: Ci::None, ..pr }, false).state, ThermoState::Pass);
    }

    #[tokio::test]
    async fn patch_proof_cannot_transfer_between_prs_or_retargeted_branches() {
        let repo = Repo::new();
        let review = repo.review().await;
        let pr = repo.rebase();
        assert_eq!(equivalent(&repo.dir, &review, &Pr { number: 8, ..pr.clone() }).await, None);
        assert_eq!(equivalent(&repo.dir, &review, &Pr { base: "release".into(), ..pr.clone() }).await, None);
        let legacy = Review { scope: None, ..review };
        assert_eq!(equivalent(&repo.dir, &legacy, &pr).await, None);
    }

    #[tokio::test]
    async fn recording_patch_evidence_uses_the_scoped_base_even_after_origin_moves() {
        let repo = Repo::new();
        let review = repo.review().await;
        let pr = repo.rebase();
        let patch = reviewed_patch(&repo.dir, review.scope.as_ref().unwrap()).await.unwrap();
        assert_eq!(patch.merge_base, repo.base);
        assert_eq!(patch.id, review.patch.as_ref().unwrap().id);
        let moved_base = Pr { head_sha: review.sha.clone(), ..pr };
        assert_eq!(standing(&review, &moved_base, false).state, ThermoState::Outdated);
        let same = equivalent(&repo.dir, &review, &moved_base).await.unwrap();
        assert!(same);
        assert_eq!(standing(&review, &moved_base, same).state, ThermoState::Pass);
    }

    #[tokio::test]
    async fn local_review_validation_rejects_moved_head_and_uncommitted_or_untracked_code() {
        let repo = Repo::new();
        let review = repo.review().await;
        let scope = review.scope.as_ref().unwrap();
        assert!(verify_local_head(scope, &repo.dir).await.is_ok());
        fs::write(repo.dir.join("untracked"), "unreviewed code\n").unwrap();
        assert!(verify_local_head(scope, &repo.dir).await.is_err());
        fs::remove_file(repo.dir.join("untracked")).unwrap();
        fs::write(repo.dir.join("code"), "uncommitted code\n").unwrap();
        assert!(verify_local_head(scope, &repo.dir).await.is_err());
        run_git(&repo.dir, &["checkout", "--", "code"]);
        assert!(verify_local_head(scope, &repo.dir).await.is_ok());
        repo.rebase();
        assert!(verify_local_head(scope, &repo.dir).await.is_err());
    }
}
