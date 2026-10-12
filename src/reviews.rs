// The thermo-nuclear review ledger: the review skill records PASS or FAIL against the exact commit it
// reviewed, and the page shows that verdict on the PR while the PR's head is still that commit.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::AsRawFd;

use crate::github::{is_full_sha, Pr};
use crate::util::{data_dir, now_ms, read_json, set_aside, write_atomic};
use crate::workspace;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Review {
    repo: String,
    branch: String,
    sha: String,
    verdict: Verdict,
    summary: String,
    at: u64,
}

/// Latest review per `owner/repo#branch`.
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

/// The review that speaks for a PR: one of the commit the PR is on now, else the latest on its branch or on
/// PR Inbox's own branch for it (a run there records that name). It is outdated once the PR moved past it.
pub fn standing(ledger: &Ledger, pr: &Pr) -> Option<Standing> {
    let r = ledger
        .values()
        .find(|r| r.repo == pr.repo && r.sha == pr.head_sha)
        .or_else(|| ledger.get(&key(&pr.repo, &pr.head)))
        .or_else(|| ledger.get(&key(&pr.repo, &workspace::own_branch(pr.number))))?;
    let state = match r.verdict {
        _ if r.sha != pr.head_sha => ThermoState::Outdated,
        Verdict::Pass => ThermoState::Pass,
        Verdict::Fail => ThermoState::Fail,
    };
    Some(Standing { state, sha: r.sha.clone(), summary: r.summary.clone(), at: r.at })
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
    let key = key(&review.repo, &review.branch);
    let verdict = if review.verdict == Verdict::Pass { "PASS" } else { "FAIL" };
    let line = format!("Recorded {verdict} for {key} at {}", &review.sha[..review.sha.len().min(7)]);
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
        ledger.insert(key, review);
        write_atomic(&file, &serde_json::to_string_pretty(&ledger).map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })?;
    Ok(line)
}

/// `pr-inbox record-review --repo owner/name --branch b --sha s --verdict pass|fail [--summary text]`
pub fn record_from_args(args: &[String]) -> Result<String, String> {
    let flag = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
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
    record(Review { repo: need("--repo")?, branch: need("--branch")?, sha, verdict, summary: flag("--summary").unwrap_or_default(), at: now_ms() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn review(branch: &str, sha: char, verdict: Verdict) -> (String, Review) {
        let r = Review { repo: "o/r".into(), branch: branch.into(), sha: sha.to_string().repeat(40), verdict, summary: format!("on {branch}"), at: 1 };
        (key(&r.repo, &r.branch), r)
    }

    #[test]
    fn the_head_commit_decides_then_the_branch_then_pr_inboxs_own_branch() {
        let pr = Pr { repo: "o/r".into(), number: 7, head: "feat".into(), head_sha: "b".repeat(40), ..Default::default() };
        let ledger: Ledger = [review("feat", 'a', Verdict::Pass), review("pr-inbox/7", 'b', Verdict::Fail)].into_iter().collect();
        let s = standing(&ledger, &pr).unwrap();
        assert_eq!((s.state, s.summary.as_str()), (ThermoState::Fail, "on pr-inbox/7"));
        let moved = Pr { head_sha: "c".repeat(40), ..pr.clone() };
        let s = standing(&ledger, &moved).unwrap();
        assert_eq!((s.state, s.summary.as_str()), (ThermoState::Outdated, "on feat"));
        let own: Ledger = [review("pr-inbox/7", 'c', Verdict::Pass)].into_iter().collect();
        assert_eq!(standing(&own, &moved).unwrap().state, ThermoState::Pass);
        assert!(standing(&own, &Pr { number: 8, head_sha: "d".repeat(40), ..moved }).is_none());
        let page = serde_json::to_value(standing(&ledger, &pr).unwrap()).unwrap();
        assert_eq!(page, serde_json::json!({ "state": "fail", "sha": "b".repeat(40), "summary": "on pr-inbox/7", "at": 1 }));
    }
}
