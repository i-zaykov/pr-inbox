// An explicitly started batch, not another daily timer. T3 starts it; the server owns its queue.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::github::{self, Cache, Ci, Lane, Mergeable, Pr};
use crate::reviews;
use crate::runs::{self, Kind, RunError, Store};
use crate::util::{data_dir, local_date, now_ms, sh, write_atomic};

pub(crate) const INSTRUCTIONS: &str = "This is an authorized daily review and repair pass on my own PR. Fix failing CI and confirmed review findings, commit and push the fixes on this PR's branch, and finish the thermo review. Read unresolved review threads as well as the diff. Make only changes needed by this PR. If intent, access, credentials or a requested decision is unclear, stop and report the blocker. Do not merge the pull request, release, deploy, close, approve or post comments. Do not create or push tags, change branch protection, bypass checks, force-push, or modify any other PR. Bringing the PR branch up to date with its base is allowed. Wait for checks on the final pushed head to finish. Report the final head, CI result and unresolved blockers. Do not record a review verdict. PR Inbox performs an independent final review, verifies checks and records the verdict. Never report the PR ready while its latest checks are pending.";

const BATCH_WINDOW_MS: u64 = 9 * 60 * 60 * 1000;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attempt {
    sha: String,
    kind: Kind,
    started_at: u64,
    run_id: Option<String>,
    status: String,
    summary: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct State {
    night: String,
    active: bool,
    started_at: u64,
    ends_at: u64,
    finished_at: Option<u64>,
    checked_at: Option<u64>,
    reason: String,
    error: Option<String>,
    attempts: BTreeMap<String, Attempt>,
    skipped: BTreeMap<String, String>,
    deferred: BTreeMap<String, String>,
    /// Rotate candidates so a PR with unreadable or pending checks cannot block the rest.
    cursor: String,
}

impl State {
    fn finish(&mut self, reason: &str) {
        self.active = false;
        self.finished_at = Some(now_ms());
        self.reason = reason.into();
    }

    fn view(&self) -> Value {
        let running = self.attempts.values().filter(|a| matches!(a.status.as_str(), "starting" | "queued" | "running")).count();
        let completed = self.attempts.values().filter(|a| a.status == "done").count();
        let failed = self.attempts.len() - running - completed;
        let message = if self.night.is_empty() {
            "Starts at 08:00 when T3 and this Mac are awake. Run reviews starts today's batch now.".into()
        } else {
            format!("{}. {} completed, {} active, {} need attention, {} waiting, {} skipped.", if self.active { "Review batch running" } else { &self.reason }, completed, running, failed, self.deferred.len(), self.skipped.len())
        };
        let mut view = serde_json::to_value(self).unwrap_or_default();
        view["message"] = message.into();
        view["counts"] = json!({ "attempted": self.attempts.len(), "running": running, "completed": completed, "failed": failed, "skipped": self.skipped.len(), "deferred": self.deferred.len() });
        view
    }
}

fn read(path: &Path) -> Result<State, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("Cannot read review batch history; no reviews will start: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(format!("Cannot read review batch history: {e}")),
    }
}

// One batch per local calendar day, with nine hours to dispatch work after the actual start.
// Retain the persisted `night` field and API paths so existing history stays readable.
fn window(ms: u64) -> (String, u64) {
    (local_date(ms), ms.saturating_add(BATCH_WINDOW_MS))
}

struct Awake(Child);
impl Awake {
    fn start() -> Result<Self, String> {
        Command::new("caffeinate").args(["-i", "-w", &std::process::id().to_string()]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().map(Self).map_err(|e| format!("Could not keep this Mac awake: {e}"))
    }
}
impl Drop for Awake {
    fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
}

pub struct Batch {
    state: Mutex<State>,
    path: PathBuf,
    load_error: Option<String>,
    awake: Mutex<Option<Awake>>,
    wake: tokio::sync::Notify,
}

impl Batch {
    pub fn load() -> Self { Self::at(data_dir().join("overnight.json")) }

    fn at(path: PathBuf) -> Self {
        let (state, load_error) = match read(&path) { Ok(s) => (s, None), Err(e) => (State { error: Some(e.clone()), ..State::default() }, Some(e)) };
        Self { state: Mutex::new(state), path, load_error, awake: Mutex::new(None), wake: tokio::sync::Notify::new() }
    }

    // Persist the intent before changing memory or starting a process. Failure stops dispatch.
    fn change(&self, f: impl FnOnce(&mut State)) -> Result<(), String> {
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        f(&mut next);
        let saved = serde_json::to_string(&next).map_err(|e| e.to_string()).and_then(|s| write_atomic(&self.path, &s).map_err(|e| e.to_string()));
        if let Err(e) = saved {
            state.finish("Stopped because review batch history could not be saved");
            state.error = Some(e.clone());
            return Err(e);
        }
        *state = next;
        Ok(())
    }

    pub fn status(&self) -> Value { self.state.lock().unwrap().view() }

    pub fn active(&self) -> bool {
        let state = self.state.lock().unwrap();
        state.active && now_ms() < state.ends_at
    }

    pub async fn start(&self, store: &Store, _prs: &Cache) -> Result<Value, String> {
        if let Some(e) = &self.load_error { return Err(e.clone()) }
        let now = now_ms();
        let (night, ends_at) = window(now);
        let current = self.state.lock().unwrap().clone();
        if current.night == night { return Ok(current.view()) }
        let awake = Awake::start()?;
        let mut started = false;
        self.change(|s| {
            if s.night != night {
                *s = State { night, active: true, started_at: now, ends_at, ..State::default() };
                started = true;
            }
        })?;
        if started {
            *self.awake.lock().unwrap() = Some(awake);
            store.set_overnight_limit(true);
            self.wake.notify_one();
        }
        Ok(self.status())
    }

    /// Stop dispatch only. Runs that already started finish their fixes and CI checks.
    pub fn stop(&self) -> Value {
        let _ = self.change(|s| { if s.active { s.finish("Review queue stopped; active reviews will finish"); } });
        self.awake.lock().unwrap().take();
        self.wake.notify_one();
        self.status()
    }

    fn allowed(&self, night: &str) -> bool {
        let s = self.state.lock().unwrap();
        s.active && s.night == night && now_ms() < s.ends_at
    }

    fn reconcile(&self, store: &Store) -> Result<(), String> {
        let snapshot = self.state.lock().unwrap().clone();
        let mut updates = Vec::new();
        for (url, a) in &snapshot.attempts {
            let run = a.run_id.as_ref().and_then(|id| store.get(id).ok()).or_else(|| store.overnight_run(&snapshot.night, url));
            if let Some(run) = run {
                let mut next = a.clone();
                let said;
                (next.run_id, next.status, said) = runs::outcome(&run);
                next.summary = said.unwrap_or_else(|| a.summary.clone());
                if next.status != a.status || next.run_id != a.run_id || next.summary != a.summary { updates.push((url.clone(), next)); }
            } else if matches!(a.status.as_str(), "starting" | "queued" | "running") {
                let mut next = a.clone();
                next.status = "interrupted".into();
                next.summary = "A previous start could not be confirmed after restart. This PR will not be repeated today.".into();
                updates.push((url.clone(), next));
            }
        }
        if !updates.is_empty() { self.change(|s| { if s.night == snapshot.night { s.attempts.extend(updates); } })?; }
        Ok(())
    }

    async fn tick(&self, store: &Store, prs: &Cache) -> Result<(), String> {
        self.reconcile(store)?;
        let before = self.state.lock().unwrap().clone();
        if before.active && now_ms() >= before.ends_at { self.change(|s| s.finish("Nine-hour dispatch window ended; active reviews will finish"))?; }
        if !self.state.lock().unwrap().active {
            store.set_overnight_limit(false);
            self.awake.lock().unwrap().take();
            return Ok(());
        }
        store.set_overnight_limit(true);
        if self.awake.lock().unwrap().is_none() { *self.awake.lock().unwrap() = Some(Awake::start()?); }
        if before.checked_at.is_some_and(|at| now_ms().saturating_sub(at) < 60_000) { return Ok(()) }
        self.change(|s| s.checked_at = Some(now_ms()))?;
        let snap = prs.refresh().await?;
        if let Some(warning) = snap.warning { return Err(format!("GitHub returned an incomplete PR list. Waiting before starting reviews: {warning}")) }
        let standings = reviews::standings(&reviews::load(), &snap.mine).await;
        let mut skipped = BTreeMap::new();
        let mut deferred = BTreeMap::new();
        let mut candidate = None;
        for pr in rotated(&snap.mine, &before.cursor) {
            if before.attempts.contains_key(&pr.url) { continue }
            match choice(pr, standings.get(&pr.url).is_some_and(|s| s.passed()), store.writer_on(&pr.url)) {
                Choice::Skip(reason) => { skipped.insert(pr.url.clone(), reason.into()); }
                Choice::Wait(reason) => { deferred.insert(pr.url.clone(), reason.into()); }
                Choice::Run(_) => {
                    if candidate.is_none() && store.active_writers() < 2 { candidate = Some(pr.url.clone()); }
                    else { deferred.insert(pr.url.clone(), "Waiting for a writing slot".into()); }
                }
            }
        }
        self.change(|s| { s.skipped = skipped; s.deferred = deferred; s.error = None; })?;
        let Some(url) = candidate else {
            let state = self.state.lock().unwrap().clone();
            let running = state.attempts.values().any(|a| matches!(a.status.as_str(), "starting" | "queued" | "running"));
            if !running && state.deferred.is_empty() { self.change(|s| s.finish("Review batch complete"))?; }
            return Ok(());
        };
        // Resolve the candidate again after selection; the snapshot or the user's branch may have moved.
        self.change(|s| s.cursor = url.clone())?;
        let fresh = github::fetch_linked_pr(&url).await?;
        let pr = fresh.pr;
        if fresh.login != snap.login || pr.author != fresh.login { return self.change(|s| { s.skipped.insert(url, "No longer one of your PRs under the signed-in account".into()); }) }
        let current = reviews::standings(&reviews::load(), std::slice::from_ref(&pr)).await;
        let kind = match choice(&pr, current.get(&url).is_some_and(|s| s.passed()), store.writer_on(&url)) {
            Choice::Run(kind) => kind,
            Choice::Skip(reason) => return self.change(|s| { s.skipped.insert(url, reason.into()); }),
            Choice::Wait(reason) => return self.change(|s| { s.deferred.insert(url, reason.into()); }),
        };
        // A failing rollup can hide other checks that are still pending.
        let checks = sh("gh", &["pr", "checks", &url, "--json", "bucket"], Duration::from_secs(20)).await;
        if !checks_finished(&checks.stdout) {
            return self.change(|s| { s.deferred.insert(url, "Waiting for all checks to finish or become readable".into()); });
        }
        if !self.allowed(&before.night) { return Ok(()) }
        let attempt = Attempt { sha: pr.head_sha.clone(), kind, started_at: now_ms(), run_id: None, status: "starting".into(), summary: String::new() };
        // The PR URL, not its head SHA, prevents this batch from reviewing its own pushed fixes again.
        self.change(|s| { s.attempts.insert(url.clone(), attempt); })?;
        let result = store.create_overnight(kind, pr, INSTRUCTIONS.into(), &before.night, &|| self.allowed(&before.night)).await;
        self.change(|s| match result {
            Ok(run) => {
                if let Some(a) = s.attempts.get_mut(&url) { (a.run_id, a.status, _) = runs::outcome(&run); }
            }
            Err(RunError::Busy(reason)) => { s.attempts.remove(&url); s.deferred.insert(url, reason.into()); }
            Err(e) => { if let Some(a) = s.attempts.get_mut(&url) { a.status = "failed".into(); a.summary = format!("Could not start review: {e:?}"); } }
        })
    }
}

#[derive(Debug, PartialEq)]
enum Choice { Skip(&'static str), Wait(&'static str), Run(Kind) }

fn rotated<'a>(prs: &'a [Pr], cursor: &str) -> Vec<&'a Pr> {
    let mut ordered: Vec<_> = prs.iter().collect();
    ordered.sort_by(|a, b| (a.url.as_str() <= cursor, &a.url).cmp(&(b.url.as_str() <= cursor, &b.url)));
    ordered
}

fn choice(pr: &Pr, passed: bool, busy: bool) -> Choice {
    if pr.state != "OPEN" { return Choice::Skip("PR is no longer open") }
    if pr.is_draft { return Choice::Skip("Draft PR") }
    if !pr.writable || pr.write_refusal().is_some() { return Choice::Skip("PR cannot be safely written by this app") }
    if busy { return Choice::Wait("A writing run is already working on this PR") }
    if pr.ci == Ci::Running || pr.lane == Lane::Checking || pr.mergeable == Mergeable::Unknown { return Choice::Wait("Waiting for checks and mergeability") }
    if pr.mergeable == Mergeable::Conflicting { return Choice::Run(Kind::Resolve) }
    if passed && pr.ci == Ci::Pass { return Choice::Skip("Current review and CI already passed") }
    Choice::Run(Kind::Thermo)
}

fn checks_finished(text: &str) -> bool {
    serde_json::from_str::<Value>(text).ok().and_then(|v| v.as_array().cloned()).is_some_and(|checks| checks.iter().all(|c| matches!(c["bucket"].as_str(), Some("pass" | "fail" | "skipping" | "cancel"))))
}

pub async fn worker(batch: Arc<Batch>, store: Arc<Store>, prs: Arc<Cache>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _ = every.tick() => {}, _ = batch.wake.notified() => {} }
        if let Err(e) = batch.tick(&store, &prs).await { let _ = batch.change(|s| s.error = Some(e)); }
        if !batch.state.lock().unwrap().active { store.set_overnight_limit(false); batch.awake.lock().unwrap().take(); }
    }
}

/// Read-only context for the existing morning digest, including errors when a pass never started.
pub fn summary() -> String {
    match read(&data_dir().join("overnight.json")) {
        Ok(s) if !s.night.is_empty() => {
            let mut lines = vec![format!("Review batch {}: {}", s.night, s.view()["message"].as_str().unwrap_or_default())];
            if let Some(error) = s.error { lines.push(format!("Error: {error}")); }
            lines.extend(s.attempts.into_iter().map(|(url, a)| format!("- {url}: {}. {}", a.status, a.summary)));
            lines.extend(s.deferred.into_iter().map(|(url, why)| format!("- {url}: not started. {why}")));
            lines.join("\n")
        }
        Ok(_) => "No daily review batch has been started.".into(),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr() -> Pr { Pr { state: "OPEN".into(), writable: true, head: "feat".into(), base: "main".into(), ci: Ci::Pass, mergeable: Mergeable::Mergeable, ..Pr::default() } }

    #[test]
    fn only_owned_open_writable_work_needing_review_or_repairs_is_selected() {
        assert_eq!(choice(&pr(), true, false), Choice::Skip("Current review and CI already passed"));
        assert_eq!(choice(&pr(), false, false), Choice::Run(Kind::Thermo));
        assert_eq!(choice(&Pr { ci: Ci::Fail, ..pr() }, true, false), Choice::Run(Kind::Thermo));
        assert_eq!(choice(&Pr { mergeable: Mergeable::Conflicting, ..pr() }, true, false), Choice::Run(Kind::Resolve));
        for p in [Pr { state: "MERGED".into(), ..pr() }, Pr { is_draft: true, ..pr() }, Pr { is_cross_repo: true, ..pr() }, Pr { writable: false, ..pr() }] { assert!(matches!(choice(&p, false, false), Choice::Skip(_))); }
        assert!(matches!(choice(&pr(), false, true), Choice::Wait(_)));
        assert!(matches!(choice(&Pr { ci: Ci::Running, ..pr() }, false, false), Choice::Wait(_)));
        assert!(matches!(choice(&Pr { mergeable: Mergeable::Unknown, ..pr() }, false, false), Choice::Wait(_)));
    }

    #[test]
    fn a_failed_check_does_not_hide_a_pending_check_or_unreadable_status() {
        assert!(!checks_finished(r#"[{"bucket":"fail"},{"bucket":"pending"}]"#));
        assert!(checks_finished(r#"[{"bucket":"fail"},{"bucket":"pass"}]"#));
        assert!(checks_finished("[]"));
        for input in ["", "gh error", "{}", r#"[{"bucket":"unknown"}]"#] { assert!(!checks_finished(input)); }
    }

    #[test]
    fn a_candidate_with_unreadable_checks_cannot_starve_other_prs() {
        let prs: Vec<_> = ["pr/c", "pr/a", "pr/b"].into_iter().map(|url| Pr { url: url.into(), ..pr() }).collect();
        let mut cursor = String::new();
        let mut visited = Vec::new();
        for _ in 0..4 {
            // Every candidate fails the separate check lookup, leaving it eligible next tick.
            let chosen = rotated(&prs, &cursor)[0];
            assert!(!checks_finished("unavailable"));
            cursor = chosen.url.clone();
            visited.push(cursor.clone());
        }
        assert_eq!(visited, ["pr/a", "pr/b", "pr/c", "pr/a"]);
    }

    #[test]
    fn a_persisted_intent_survives_restart_even_without_a_confirmed_run() {
        let dir = std::env::temp_dir().join(format!("pr-inbox-overnight-{}", uuid::Uuid::new_v4()));
        let path = dir.join("overnight.json");
        let batch = Batch::at(path.clone());
        batch.change(|s| { s.night = "2026-11-03".into(); s.attempts.insert("pr/1".into(), Attempt { sha: "a".repeat(40), kind: Kind::Thermo, started_at: 1, run_id: None, status: "starting".into(), summary: String::new() }); }).unwrap();
        let loaded = Batch::at(path);
        assert!(loaded.state.lock().unwrap().attempts.contains_key("pr/1"));
        assert_eq!(loaded.status()["counts"]["attempted"], 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_morning_start_has_nine_hours_and_uses_the_local_calendar_day() {
        fn local(hour: i32, day: i32) -> u64 {
            // SAFETY: mktime receives an initialized local calendar value.
            unsafe { let mut t: libc::tm = std::mem::zeroed(); t.tm_year = 126; t.tm_mon = 10; t.tm_mday = day; t.tm_hour = hour; t.tm_isdst = -1; (libc::mktime(&mut t) as u64) * 1000 }
        }
        let morning = window(local(8, 4));
        assert_eq!(morning.0, "2026-11-04");
        assert_eq!(morning.1, local(17, 4));
        assert_eq!(morning.0, window(local(2, 4)).0);
        assert_eq!(morning.0, window(local(16, 4)).0);
        assert_ne!(morning.0, window(local(8, 5)).0);
        assert_eq!(window(local(10, 4)).1, local(19, 4));
    }
}
