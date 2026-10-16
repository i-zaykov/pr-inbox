// Releases: each repo releases the way it already does. A repo with a `release.yml` workflow (bi-dbt's
// "Manual Release") runs it; the others get a GitHub release tagged from the default branch with
// generated notes, one patch above the latest (v4.4.10 -> v4.4.11, 1.3.5 -> 1.3.6). A repo with neither
// has no release step, so Merge & release only merges there.
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use crate::github::safe_ref;
use crate::util::{first_line, now_ms, sh, Output};

/// How long a repo's plan is reused before GitHub is asked again; a release drops it at once.
const PLAN_TTL_MS: u64 = 5 * 60_000;

#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Method {
    Workflow,
    Tag,
    None,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub repo: String,
    pub branch: String,
    pub method: Method,
    pub latest: Option<String>,
    /// The tag a release would create; None for a workflow, which picks its own version.
    pub next: Option<String>,
    /// Commits on the default branch since the latest release; None when there is no release to count from.
    pub unreleased: Option<u64>,
}

#[derive(Serialize, Default)]
pub struct Plans {
    plans: HashMap<String, Plan>,
    /// Repos GitHub could not answer for, with the reason.
    errors: HashMap<String, String>,
}

#[derive(Serialize)]
pub struct Released {
    message: String,
    link: Option<String>,
}

/// `owner/name`, the only form a release request accepts.
pub fn is_repo(s: &str) -> bool {
    let word = |p: &str| !p.is_empty() && p != "." && p != ".." && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c));
    matches!(s.split('/').collect::<Vec<_>>()[..], [owner, name] if word(owner) && word(name))
}

fn bump_patch(tag: &str) -> Option<String> {
    let (prefix, rest) = tag.strip_prefix('v').map(|r| ("v", r)).unwrap_or(("", tag));
    let nums: Vec<u64> = rest.split('.').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    let [major, minor, patch] = nums[..] else { return None };
    Some(format!("{prefix}{major}.{minor}.{}", patch + 1))
}

fn method_of(has_workflow: bool, latest: Option<&str>) -> Method {
    match (has_workflow, latest) {
        (true, _) => Method::Workflow,
        (false, Some(_)) => Method::Tag,
        (false, None) => Method::None,
    }
}

/// A `gh api` lookup: the value, None when GitHub answered 404, or the error that kept it from answering.
fn lookup(out: &Output) -> Result<Option<String>, String> {
    if out.code == 0 {
        return Ok(Some(out.stdout.trim().to_string()).filter(|s| !s.is_empty()));
    }
    if out.stderr.contains("HTTP 404") {
        return Ok(None);
    }
    Err(first_line(&out.stderr))
}

async fn fetch(repo: &str) -> Result<Plan, String> {
    let t = Duration::from_secs(30);
    let api = |path: String, jq: &'static str| async move { sh("gh", &["api", &path, "--jq", jq], t).await };
    let (latest, info, workflow) = tokio::join!(
        api(format!("repos/{repo}/releases/latest"), ".tag_name"),
        api(format!("repos/{repo}"), ".default_branch"),
        api(format!("repos/{repo}/contents/.github/workflows/release.yml"), ".name"),
    );
    let branch = lookup(&info)?.filter(|b| safe_ref(b)).ok_or_else(|| format!("GitHub did not give {repo}'s default branch"))?;
    let latest = lookup(&latest)?;
    let method = method_of(lookup(&workflow)?.is_some(), latest.as_deref());
    let unreleased = match latest.as_deref().filter(|tag| safe_ref(tag)) {
        Some(tag) => lookup(&api(format!("repos/{repo}/compare/{tag}...{branch}"), ".ahead_by").await)?.and_then(|n| n.parse().ok()),
        None => None,
    };
    Ok(Plan {
        repo: repo.to_string(),
        next: latest.as_deref().and_then(bump_patch).filter(|_| method == Method::Tag),
        branch,
        method,
        latest,
        unreleased,
    })
}

async fn run_release(repo: &str) -> Result<Released, String> {
    // Decided from GitHub as it is now, never from a plan the page saw minutes ago.
    let p = fetch(repo).await?;
    let name = repo.rsplit('/').next().unwrap_or(repo);
    if p.unreleased == Some(0) {
        return Err(format!("Nothing new on {} since {}", p.branch, p.latest.unwrap_or_default()));
    }
    let t = Duration::from_secs(120);
    match p.method {
        Method::Workflow => {
            let out = sh("gh", &["workflow", "run", "release.yml", "-R", repo, "--ref", &p.branch], t).await;
            if out.code != 0 {
                return Err(first_line(&out.stderr));
            }
            Ok(Released { message: format!("Started {name}'s release workflow"), link: Some(format!("https://github.com/{repo}/actions/workflows/release.yml")) })
        }
        Method::Tag => {
            let latest = p.latest.unwrap_or_default();
            let tag = p.next.ok_or_else(|| format!("Could not work out the version after {latest}"))?;
            let out = sh("gh", &["release", "create", &tag, "-R", repo, "--target", &p.branch, "--title", &tag, "--generate-notes"], t).await;
            if out.code != 0 {
                return Err(first_line(&out.stderr));
            }
            let link = Some(out.stdout.trim().to_string()).filter(|u| u.starts_with("https://"));
            Ok(Released { message: format!("Released {name} {tag}"), link })
        }
        Method::None => Err(format!("{name} has no releases yet. Create the first one on GitHub.")),
    }
}

/// Release plans per repo, and the releases in flight, so a double click never cuts two.
#[derive(Default)]
pub struct Releases {
    plans: Mutex<HashMap<String, (u64, Plan)>>,
    busy: Mutex<HashSet<String>>,
}

impl Releases {
    /// How each repo would release now. Plans younger than five minutes are reused; the rest are
    /// fetched at once.
    pub async fn plans(&self, repos: Vec<String>) -> Plans {
        let now = now_ms();
        let mut out = Plans::default();
        let mut stale = Vec::new();
        {
            let cache = self.plans.lock().unwrap();
            for repo in repos {
                match cache.get(&repo) {
                    Some((at, plan)) if now.saturating_sub(*at) < PLAN_TTL_MS => {
                        out.plans.insert(repo, plan.clone());
                    }
                    _ => stale.push(repo),
                }
            }
        }
        let fetches: Vec<_> = stale.into_iter().map(|repo| tokio::spawn(async move { (fetch(&repo).await, repo) })).collect();
        for task in fetches {
            match task.await {
                Ok((Ok(plan), repo)) => {
                    self.plans.lock().unwrap().insert(repo.clone(), (now, plan.clone()));
                    out.plans.insert(repo, plan);
                }
                Ok((Err(e), repo)) => {
                    out.errors.insert(repo, e);
                }
                Err(_) => {}
            }
        }
        out
    }

    /// Cuts one release of `repo` the way it releases. A second request while one runs is refused.
    pub async fn release(&self, repo: &str) -> Result<Released, String> {
        if !self.busy.lock().unwrap().insert(repo.to_string()) {
            return Err("A release of this repo is already running".into());
        }
        let result = run_release(repo).await;
        self.busy.lock().unwrap().remove(repo);
        self.plans.lock().unwrap().remove(repo);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_next_tag_is_one_patch_up_with_the_same_prefix() {
        assert_eq!(bump_patch("v4.4.10").as_deref(), Some("v4.4.11"));
        assert_eq!(bump_patch("1.3.5").as_deref(), Some("1.3.6"));
        assert_eq!(bump_patch("v1.6.0").as_deref(), Some("v1.6.1"));
        for odd in ["v2.0", "v1.2.3-rc1", "release-7", "v1.2.3.4", ""] {
            assert_eq!(bump_patch(odd), None, "{odd}");
        }
    }

    #[test]
    fn a_release_workflow_wins_over_tagging() {
        assert_eq!(method_of(true, Some("v2.66.31")), Method::Workflow);
        assert_eq!(method_of(true, None), Method::Workflow);
        assert_eq!(method_of(false, Some("v4.4.10")), Method::Tag);
        assert_eq!(method_of(false, None), Method::None);
    }

    #[test]
    fn a_404_means_missing_and_any_other_failure_is_an_error() {
        let out = |code, stdout: &str, stderr: &str| Output { code, stdout: stdout.into(), stderr: stderr.into() };
        assert_eq!(lookup(&out(0, "v4.4.10\n", "")), Ok(Some("v4.4.10".into())));
        assert_eq!(lookup(&out(1, "", "gh: Not Found (HTTP 404)")), Ok(None));
        assert_eq!(lookup(&out(1, "", "error connecting to api.github.com")), Err("error connecting to api.github.com".into()));
    }

    #[test]
    fn only_owner_slash_name_is_a_repo() {
        assert!(is_repo("lenus-ehealth/airflow"));
        for bad in ["airflow", "a/b/c", "../etc", "a/..", "a/b?x=1", "a /b", "/b", "a/"] {
            assert!(!is_repo(bad), "{bad}");
        }
    }
}
