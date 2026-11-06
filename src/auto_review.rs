// Starts Thermo & approve by itself when someone asks for your review on their PR after Auto thermo was turned on,
// its checks pass and your Claude plan has room: the session below one limit and every weekly limit below another.
// It is the button's own run, so it publishes the same way: an approval on PASS, a request for changes on FAIL. One
// automatic run at a time. It takes only the first review of a PR: at most one start per PR, and none once you
// reviewed it, so a new commit or a new request after a review waits for you.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::github::{self, is_full_sha, is_pr_url, Cache, Ci, Lane, Pr};
use crate::review_flow;
use crate::runs::{self, RunError, Store};
use crate::usage::{self, Usage};
use crate::util::{data_dir, notify, now_ms, parse_iso, utc_iso, write_atomic};

/// The usage endpoint rate-limits, so a reading is reused this long. It is read only when a PR is ready to start.
const USAGE_MAX_AGE_MS: u64 = 5 * 60_000;
/// Right before a start the reading may be at most this old.
const USAGE_FRESH_MS: u64 = 60_000;
/// The PR list may be this old. The page's own refreshes keep it fresher while it is open; when it is closed,
/// this keeps the list query, the costliest GitHub call, from running every minute.
const LIST_MAX_AGE_MS: u64 = 2 * 60_000;
/// Ready PRs checked against GitHub per tick, so one whose checks never verify cannot hold up the rest.
const TRIES: usize = 3;
pub const MAX_SKIPPED: usize = 500;
const UNCONFIRMED: &str = "Waiting for GitHub to confirm every check passed";
const UNREADABLE: &str = "Could not read the PR from GitHub";
const UNSYNCED: &str = "Open PR Inbox once so Auto thermo knows which PRs you skipped";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    enabled: bool,
    /// Starts only while the five-hour session is below this percentage.
    session_limit: u8,
    /// Starts only while every weekly limit is below this percentage.
    weekly_limit: u8,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true, session_limit: 70, weekly_limit: 90 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Attempt {
    sha: String,
    run_id: Option<String>,
    started_at: u64,
    status: String,
    summary: String,
}

impl Attempt {
    fn active(&self) -> bool {
        matches!(self.status.as_str(), "starting" | "queued" | "running")
    }
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct State {
    settings: Settings,
    /// When Auto thermo was last turned on, or this file was made. A request for your review sent before it is
    /// left alone, so turning it on never works through PRs that were already waiting.
    on_since: u64,
    /// The last automatic start on each PR, by PR URL. A start on a newer head replaces it.
    attempts: BTreeMap<String, Attempt>,
    /// Skip choices from every page: PR URL to when you skipped it.
    skipped: BTreeMap<String, String>,
    /// PR URL to when you unskipped it, so a page holding an older copy cannot skip it again.
    unskipped: BTreeMap<String, String>,
    /// A page sent its whole Skip map since this file was made. Until then nothing starts: a Skip may live only in
    /// a page's storage.
    skips_synced: bool,
    /// What the last check found, in plain words.
    message: String,
    error: Option<String>,
    usage: Option<Usage>,
    usage_error: Option<String>,
    /// When usage was last read, whether or not that worked.
    usage_at: u64,
    /// The PRs it will start once something clears, by PR URL, with what each waits for.
    waiting: BTreeMap<String, String>,
}

impl State {
    fn running(&self) -> Option<&String> {
        self.attempts.iter().find(|(_, a)| a.active()).map(|(url, _)| url)
    }

    fn view(&self) -> Value {
        let mut view = serde_json::to_value(self).unwrap_or_default();
        view["running"] = self.running().cloned().into();
        view
    }
}

#[derive(Debug, PartialEq)]
enum Choice {
    /// Not one for the automatic run.
    Skip,
    /// It will start once this clears.
    Wait(&'static str),
    Ready,
}

/// Whether the automatic run takes this PR. Only someone else's open, non-draft PR that asks for your review, asked
/// since Auto thermo was turned on (`new`), and that Thermo & approve may take; not one you skipped since the last
/// request (the page's rule). Only the first review is automatic: not a PR you already reviewed on GitHub, one it
/// started on before (`attempted`), or one a Thermo & approve run reviewed (`reviewed`), whatever its head now.
/// `busy`: another run works on the PR.
fn choice(pr: &Pr, new: bool, skipped_at: Option<&str>, attempted: bool, busy: bool, reviewed: bool) -> Choice {
    let skipped = skipped_at.is_some_and(|at| pr.asked_at.as_str() <= at);
    if pr.lane != Lane::Waiting || pr.state != "OPEN" || pr.is_draft || !pr.review_requested || pr.approve_refusal().is_some() || skipped || !new {
        return Choice::Skip;
    }
    if !is_full_sha(&pr.head_sha) || pr.your_review.is_some() || attempted || reviewed {
        return Choice::Skip;
    }
    if busy {
        return Choice::Wait("Another Claude run is working on this PR");
    }
    match pr.ci {
        Ci::Pass => Choice::Ready,
        Ci::Running => Choice::Wait("Checks are still running"),
        Ci::Fail => Choice::Wait("A check is failing. It starts once every check passes"),
        Ci::None => Choice::Wait("No checks have reported yet"),
    }
}

/// Whether the request for your review came at or after `since`, or the PR was opened since then. In the PR list a
/// team request counts whichever team it went to; the fresh read before a start counts only your teams.
fn requested_since(pr: &Pr, since: &str) -> bool {
    [&pr.asked_at, &pr.team_asked_at, &pr.created_at].iter().any(|at| at.as_str() >= since)
}

/// What the PR list says about a PR that the fresh read can disagree with.
fn listed_as(pr: &Pr) -> String {
    format!("{} {} {}", pr.head_sha, pr.updated_at, pr.asked_at)
}

/// The order ready PRs are tried in: the request that has waited longest first, behind any GitHub would not confirm
/// last time (`before`), so a PR whose checks never verify cannot hold up the rest. A PR the fresh read turned down
/// is left out until the list shows it changed, so a few the list and the fresh read disagree on cannot either.
fn queue<'a>(ready: Vec<&'a Pr>, before: &BTreeMap<String, String>, turned_down: &HashMap<String, String>) -> Vec<&'a Pr> {
    let stuck = |pr: &Pr| before.get(&pr.url).is_some_and(|why| why == UNCONFIRMED || why.starts_with(UNREADABLE));
    let asked = |pr: &Pr| [&pr.asked_at, &pr.team_asked_at].into_iter().max().filter(|at| !at.is_empty()).unwrap_or(&pr.created_at).clone();
    let mut ready: Vec<&Pr> = ready.into_iter().filter(|pr| turned_down.get(&pr.url) != Some(&listed_as(pr))).collect();
    ready.sort_by_key(|pr| (stuck(pr), asked(pr)));
    ready
}

/// Takes Skip changes from a page. A skip counts unless the server holds a later skip or unskip of that PR, and an
/// unskip removes a skip no later than itself, so no page can drop or revive another page's choice with an older
/// copy. Times are the page's UTC ISO strings, so they compare as strings.
fn merge_skips(s: &mut State, skipped: BTreeMap<String, String>, unskipped: BTreeMap<String, String>) {
    for (url, at) in unskipped {
        if s.skipped.get(&url).is_some_and(|held| *held <= at) {
            s.skipped.remove(&url);
        }
        let latest = s.unskipped.entry(url).or_default();
        if *latest < at {
            *latest = at;
        }
    }
    for (url, at) in skipped {
        if [s.skipped.get(&url), s.unskipped.get(&url)].iter().all(|held| held.is_none_or(|h| *h < at)) {
            s.skipped.insert(url, at);
        }
    }
}

/// What one start attempt came to.
enum Step {
    Started(String),
    Failed(String),
    Wait(String),
    /// The fresh read says it is not a PR for the automatic run.
    Gone,
    /// Auto thermo was turned off meanwhile.
    Off,
}

/// "repo#7" for a PR URL.
fn short(url: &str) -> String {
    match url.trim_start_matches("https://github.com/").split('/').collect::<Vec<_>>()[..] {
        [_, repo, "pull", number] => format!("{repo}#{number}"),
        _ => url.into(),
    }
}

/// How many PRs wait and for what, in one line: "3 PRs wait: Checks are still running (2). Session at 100%."
fn waits(waiting: &BTreeMap<String, String>) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for why in waiting.values() {
        match counts.iter_mut().find(|(w, _)| w == why) {
            Some((_, n)) => *n += 1,
            None => counts.push((why, 1)),
        }
    }
    let reasons: Vec<String> = counts.iter().map(|(why, n)| if *n > 1 { format!("{why} ({n})") } else { why.to_string() }).collect();
    match waiting.len() {
        0 => String::new(),
        1 => format!("1 PR waits: {}.", reasons.join(". ")),
        n => format!("{n} PRs wait: {}.", reasons.join(". ")),
    }
}

fn unknown(e: String) -> String {
    format!("Usage unknown, so nothing starts: {e}")
}

/// A limit from the page: a whole percentage from 10 to 100, or none to leave it.
pub fn limit(value: Option<i64>) -> Result<Option<u8>, String> {
    match value {
        None => Ok(None),
        Some(v @ 10..=100) => Ok(Some(v as u8)),
        Some(_) => Err("Limits are whole percentages from 10 to 100".into()),
    }
}

/// Why a page's Skip changes are refused: too many, or an entry that is not a PR URL and a UTC time, the form
/// `choice` compares with GitHub's times.
pub fn skips_refusal(skipped: &BTreeMap<String, String>, unskipped: &BTreeMap<String, String>) -> Option<String> {
    if skipped.len() + unskipped.len() > MAX_SKIPPED {
        return Some(format!("At most {MAX_SKIPPED} skipped PRs"));
    }
    skipped.iter().chain(unskipped).any(|(url, at)| !is_pr_url(url) || !at.ends_with('Z') || parse_iso(at).is_none())
        .then(|| "Each skipped PR needs its pull request URL and the UTC time you skipped it".into())
}

fn read(path: &Path) -> Result<State, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| format!("Cannot read {}, so nothing starts automatically: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(format!("Cannot read automatic review history, so nothing starts automatically: {e}")),
    }
}

pub struct AutoReview {
    state: Mutex<State>,
    path: PathBuf,
    load_error: Option<String>,
    wake: tokio::sync::Notify,
    /// PRs the fresh read turned down, by URL, with what the list said then (`listed_as`). Kept in memory: after a
    /// restart each is read once more.
    turned_down: Mutex<HashMap<String, String>>,
}

impl AutoReview {
    pub fn load() -> Self {
        Self::at(data_dir().join("auto-review.json"))
    }

    fn at(path: PathBuf) -> Self {
        let (mut state, load_error) = match read(&path) {
            Ok(s) => (s, None),
            Err(e) => (State { error: Some(e.clone()), ..State::default() }, Some(e)),
        };
        // A new file counts requests from now, never from the epoch.
        if state.on_since == 0 {
            state.on_since = now_ms();
        }
        Self { state: Mutex::new(state), path, load_error, wake: tokio::sync::Notify::new(), turned_down: Mutex::default() }
    }

    /// Saves the change before memory sees it, so an attempt is on disk before its run exists. A change that
    /// changes nothing writes nothing. A file this binary could not read is kept, never overwritten.
    fn change(&self, f: impl FnOnce(&mut State)) -> Result<(), String> {
        if let Some(e) = &self.load_error {
            return Err(e.clone());
        }
        let mut state = self.state.lock().unwrap();
        let mut next = state.clone();
        f(&mut next);
        if next == *state {
            return Ok(());
        }
        let saved = serde_json::to_string(&next).map_err(|e| e.to_string()).and_then(|s| write_atomic(&self.path, &s).map_err(|e| e.to_string()));
        if let Err(e) = saved {
            let e = format!("Cannot save automatic review history, so nothing starts: {e}");
            state.error = Some(e.clone());
            return Err(e);
        }
        *state = next;
        Ok(())
    }

    pub fn status(&self) -> Value {
        self.state.lock().unwrap().view()
    }

    /// Changes the settings, then checks again at once. Limits come validated by `limit`. Turning it on counts
    /// requests from now.
    pub fn configure(&self, enabled: Option<bool>, session_limit: Option<u8>, weekly_limit: Option<u8>) -> Result<Value, String> {
        self.change(|s| {
            if enabled == Some(true) && !s.settings.enabled {
                s.on_since = now_ms();
            }
            s.settings.enabled = enabled.unwrap_or(s.settings.enabled);
            s.settings.session_limit = session_limit.unwrap_or(s.settings.session_limit);
            s.settings.weekly_limit = weekly_limit.unwrap_or(s.settings.weekly_limit);
            if !s.settings.enabled {
                s.waiting.clear();
                s.message.clear();
            }
        })?;
        self.wake.notify_one();
        Ok(self.status())
    }

    /// Takes Skip changes from a page, validated by `skips_refusal`, then checks again at once: an unskipped PR
    /// may start. `complete`: the page sent its whole map, as it does when it loads.
    pub fn take_skips(&self, skipped: BTreeMap<String, String>, unskipped: BTreeMap<String, String>, complete: bool) -> Result<Value, String> {
        self.change(|s| {
            merge_skips(s, skipped, unskipped);
            s.skips_synced |= complete;
        })?;
        self.wake.notify_one();
        Ok(self.status())
    }

    fn gate(&self, u: &Usage) -> Result<(), String> {
        let s = self.state.lock().unwrap().settings.clone();
        usage::gate(u, s.session_limit, s.weekly_limit, now_ms())
    }

    /// The plan's usage, reused while it is under `max_age` old. A failed read is reused as long, so a refusal
    /// does not turn into a request every minute.
    async fn usage(&self, max_age: u64) -> Result<Usage, String> {
        let (at, reading, error) = {
            let s = self.state.lock().unwrap();
            (s.usage_at, s.usage.clone(), s.usage_error.clone())
        };
        if now_ms().saturating_sub(at) < max_age {
            if let Some(e) = error {
                return Err(e);
            }
            if let Some(u) = reading {
                return Ok(u);
            }
        }
        let result = usage::fetch().await;
        self.change(|s| {
            s.usage_at = now_ms();
            s.usage = result.as_ref().ok().cloned();
            s.usage_error = result.as_ref().err().cloned();
        })?;
        result
    }

    fn judge(&self, pr: &Pr, store: &Store) -> Choice {
        let busy = store.busy_on(&pr.url);
        let reviewed = store.approved(&pr.url);
        let s = self.state.lock().unwrap();
        let new = requested_since(pr, &utc_iso(s.on_since));
        choice(pr, new, s.skipped.get(&pr.url).map(String::as_str), s.attempts.contains_key(&pr.url), busy, reviewed)
    }

    /// Brings each attempt up to date with its run. A start whose run cannot be found after a restart is marked
    /// interrupted, and the PR does not start again by itself.
    fn reconcile(&self, store: &Store) -> Result<(), String> {
        let attempts = self.state.lock().unwrap().attempts.clone();
        let mut updates = Vec::new();
        for (url, a) in &attempts {
            let run = match &a.run_id {
                Some(id) => store.get(id).ok(),
                None if a.status == "starting" => store.approve_run_since(url, a.started_at),
                None => None,
            };
            let mut next = a.clone();
            if let Some(run) = run {
                let said;
                (next.run_id, next.status, said) = runs::outcome(&run);
                next.summary = said.unwrap_or_else(|| a.summary.clone());
            } else if a.active() {
                next.status = "interrupted".into();
                next.summary = "The automatic start could not be confirmed after a restart. This PR will not start again by itself.".into();
            }
            if (&next.status, &next.run_id, &next.summary) != (&a.status, &a.run_id, &a.summary) {
                updates.push((url.clone(), next));
            }
        }
        if updates.is_empty() {
            return Ok(());
        }
        self.change(|s| {
            for (url, next) in updates {
                if s.attempts.get(&url).is_some_and(|a| a.started_at == next.started_at) {
                    s.attempts.insert(url, next);
                }
            }
        })
    }

    async fn tick(&self, store: &Store, prs: &Cache) -> Result<(), String> {
        if let Some(e) = &self.load_error {
            return Err(e.clone());
        }
        self.reconcile(store)?;
        if !self.state.lock().unwrap().settings.enabled {
            return self.change(|s| {
                s.waiting.clear();
                s.message.clear();
                s.error = None;
            });
        }
        let snap = prs.within(LIST_MAX_AGE_MS).await?;
        if let Some(warning) = snap.warning {
            return Err(format!("GitHub sent an incomplete PR list, so nothing starts until it loads in full: {warning}"));
        }
        let listed: HashSet<&str> = snap.mine.iter().chain(&snap.others).map(|p| p.url.as_str()).collect();
        self.turned_down.lock().unwrap().retain(|url, _| listed.contains(url.as_str()));
        let mut waiting = BTreeMap::new();
        let mut ready: Vec<&Pr> = Vec::new();
        for pr in &snap.others {
            match self.judge(pr, store) {
                Choice::Skip => {}
                Choice::Wait(why) => { waiting.insert(pr.url.clone(), why.to_string()); }
                Choice::Ready => ready.push(pr),
            }
        }
        let (before, synced, running) = {
            let s = self.state.lock().unwrap();
            (s.waiting.clone(), s.skips_synced, s.running().cloned())
        };
        let turned_down = self.turned_down.lock().unwrap().clone();
        let ready = queue(ready, &before, &turned_down);
        let mut lines = Vec::new();
        if !synced {
            for pr in &ready {
                waiting.insert(pr.url.clone(), UNSYNCED.into());
            }
        } else if let Some(url) = running {
            lines.push(format!("Thermo & approve is running on {}.", short(&url)));
            for pr in &ready {
                waiting.insert(pr.url.clone(), format!("The automatic review of {} runs first", short(&url)));
            }
        } else if !ready.is_empty() {
            // Usage is read only now, when a PR is ready to start.
            let room = self.usage(USAGE_MAX_AGE_MS).await.map_err(unknown).and_then(|u| self.gate(&u));
            let mut started = None;
            // Fresh reads that may still start a PR this tick. One the fresh read turns down does not count.
            let mut tries = 0;
            for pr in &ready {
                let why = match (&room, &started) {
                    (Err(why), _) => why.clone(),
                    (_, Some(name)) => format!("The automatic review of {name} runs first"),
                    _ if tries >= TRIES => "Next in line".into(),
                    _ => match self.start(&pr.url, &snap.login, store).await? {
                        Step::Started(name) => {
                            lines.push(format!("Started Thermo & approve on {name}."));
                            started = Some(name);
                            continue;
                        }
                        Step::Failed(line) => {
                            tries += 1;
                            lines.push(line);
                            continue;
                        }
                        Step::Wait(why) => {
                            tries += 1;
                            why
                        }
                        Step::Gone => {
                            self.turned_down.lock().unwrap().insert(pr.url.clone(), listed_as(pr));
                            continue;
                        }
                        Step::Off => return Ok(()),
                    },
                };
                waiting.insert(pr.url.clone(), why);
            }
        }
        lines.push(waits(&waiting));
        let mut message = lines.into_iter().filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
        if message.is_empty() {
            message = "No new PR waits on your review.".into();
        }
        self.change(|s| {
            s.waiting = waiting;
            s.message = message;
            s.error = None;
            // A PR that left the list is merged or closed. A run of it, if any, still covers its head.
            s.attempts.retain(|url, a| a.active() || listed.contains(url.as_str()));
            s.skipped.retain(|url, _| listed.contains(url.as_str()));
            s.unskipped.retain(|url, _| listed.contains(url.as_str()));
        })
    }

    /// Starts the run on one ready PR after checking it again: the PR as GitHub has it now, every check passed,
    /// and usage read within the last minute still under the limits.
    async fn start(&self, url: &str, login: &str, store: &Store) -> Result<Step, String> {
        let pr = match github::fetch_linked_pr(url).await {
            Ok(linked) if linked.login == login => linked.pr,
            Ok(_) => return Ok(Step::Gone),
            Err(e) => return Ok(Step::Wait(format!("{UNREADABLE}: {e}"))),
        };
        match self.judge(&pr, store) {
            Choice::Ready => {}
            Choice::Wait(why) => return Ok(Step::Wait(why.into())),
            Choice::Skip => return Ok(Step::Gone),
        }
        // The list's CI rollup can lag; publishing an approval needs every check passed.
        if review_flow::checks_pass(url).await.is_err() {
            return Ok(Step::Wait(UNCONFIRMED.into()));
        }
        let reading = match self.usage(USAGE_FRESH_MS).await.map_err(unknown).and_then(|u| self.gate(&u).map(|_| u)) {
            Ok(u) => u,
            Err(why) => return Ok(Step::Wait(why)),
        };
        if !self.state.lock().unwrap().settings.enabled {
            return Ok(Step::Off);
        }
        let name = format!("{}#{}", pr.name, pr.number);
        let attempt = Attempt { sha: pr.head_sha.clone(), run_id: None, started_at: now_ms(), status: "starting".into(), summary: String::new() };
        // On disk before the run exists: after a restart in between, this head is not started again.
        self.change(|s| { s.attempts.insert(url.into(), attempt); })?;
        let result = store.create_auto(pr).await;
        let mut step = Step::Gone;
        self.change(|s| match result {
            Ok(run) => {
                if let Some(a) = s.attempts.get_mut(url) {
                    (a.run_id, a.status, _) = runs::outcome(&run);
                }
                step = Step::Started(name.clone());
            }
            // Another run took the PR first. Nothing started, so a later tick tries again.
            Err(RunError::Busy(why)) => {
                s.attempts.remove(url);
                step = Step::Wait(why.into());
            }
            Err(e) => {
                let why = match e {
                    RunError::Invalid(m) | RunError::Failed(m) => m,
                    RunError::Busy(m) => m.into(),
                    RunError::NotFound => "The new run disappeared".into(),
                };
                if let Some(a) = s.attempts.get_mut(url) {
                    a.status = "failed".into();
                    a.summary = format!("Could not start: {why}");
                }
                step = Step::Failed(format!("Could not start Thermo & approve on {name}: {why}."));
            }
        })?;
        if matches!(step, Step::Started(_)) {
            notify("PR Inbox", &format!("Started Thermo & approve on {name}. Session at {:.0}%.", reading.session));
        }
        Ok(step)
    }
}

/// Checks every minute, and at once when the settings or the Skip map change.
pub async fn worker(auto: Arc<AutoReview>, store: Arc<Store>, prs: Arc<Cache>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _ = every.tick() => {}, _ = auto.wake.notified() => {} }
        if let Err(e) = auto.tick(&store, &prs).await {
            let _ = auto.change(|s| s.error = Some(e));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::YourReview;
    use crate::runs::Status;

    const HEAD: &str = "cccccccccccccccccccccccccccccccccccccccc";

    fn pr() -> Pr {
        Pr { url: "https://github.com/o/r/pull/7".into(), name: "r".into(), number: 7, state: "OPEN".into(), lane: Lane::Waiting, review_requested: true,
            head_sha: HEAD.into(), asked_at: "2026-11-05T10:00:00Z".into(), created_at: "2026-11-01T10:00:00Z".into(), ci: Ci::Pass, ..Pr::default() }
    }

    fn temp() -> PathBuf {
        std::env::temp_dir().join(format!("pr-inbox-auto-{}", uuid::Uuid::new_v4())).join("auto-review.json")
    }

    fn map(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries.iter().map(|(u, a)| (u.to_string(), a.to_string())).collect()
    }

    /// A store whose state counts requests from 2026-11-05T09:00:00Z.
    fn since_nine(path: PathBuf) -> AutoReview {
        let auto = AutoReview::at(path);
        auto.state.lock().unwrap().on_since = parse_iso("2026-11-05T09:00:00Z").unwrap();
        auto
    }

    #[test]
    fn only_the_first_review_of_a_pr_starts_and_only_with_passing_checks() {
        assert_eq!(choice(&pr(), true, None, false, false, false), Choice::Ready);
        let not_ours = [
            Pr { lane: Lane::Reviewed, ..pr() },
            Pr { state: "MERGED".into(), ..pr() },
            Pr { is_draft: true, ..pr() },
            Pr { review_requested: false, ..pr() },
            Pr { head_sha: "abc".into(), ..pr() },
            Pr { your_review_sha: HEAD.into(), your_review: Some(YourReview::Commented), ..pr() },
        ];
        for p in &not_ours {
            assert_eq!(choice(p, true, None, false, false, false), Choice::Skip, "{:?}", p);
        }
        assert_eq!(choice(&pr(), true, None, true, false, false), Choice::Skip, "it already started on this PR, on any head");
        assert_eq!(choice(&pr(), true, None, false, false, true), Choice::Skip, "a Thermo & approve run already reviewed this PR");
        // Asked again on a new commit after any review of yours: the next review is yours to start.
        for review in [YourReview::ChangesRequested, YourReview::Approved, YourReview::Commented, YourReview::Dismissed] {
            let reviewed_older_head = Pr { your_review: Some(review), your_review_sha: "b".repeat(40), ..pr() };
            assert_eq!(choice(&reviewed_older_head, true, None, false, false, false), Choice::Skip, "{review:?}");
        }
    }

    #[test]
    fn a_request_from_before_it_was_turned_on_is_left_alone() {
        assert_eq!(choice(&pr(), false, None, false, false, false), Choice::Skip);
        let since = "2026-11-05T09:00:00Z";
        assert!(requested_since(&pr(), since), "asked by name after it was turned on");
        let old = Pr { asked_at: "2026-11-04T10:00:00Z".into(), ..pr() };
        assert!(!requested_since(&old, since), "the backlog stays as it is");
        assert!(requested_since(&Pr { team_asked_at: "2026-11-05T09:30:00Z".into(), ..old.clone() }, since), "a team asked since");
        assert!(requested_since(&Pr { created_at: "2026-11-05T09:30:00Z".into(), ..old.clone() }, since), "opened since");
        assert!(!requested_since(&Pr { asked_at: String::new(), created_at: String::new(), ..old }, since), "no dates read as old");
    }

    #[test]
    fn skip_holds_until_your_review_is_asked_for_again() {
        assert_eq!(choice(&pr(), true, Some("2026-11-05T11:00:00.000Z"), false, false, false), Choice::Skip);
        assert_eq!(choice(&pr(), true, Some("2026-11-05T10:00:00Z"), false, false, false), Choice::Skip);
        assert_eq!(choice(&pr(), true, Some("2026-11-05T09:00:00.000Z"), false, false, false), Choice::Ready);
        assert_eq!(choice(&Pr { asked_at: String::new(), ..pr() }, true, Some("2026-11-05T09:00:00.000Z"), false, false, false), Choice::Skip);
    }

    #[test]
    fn running_failing_or_missing_checks_and_other_runs_wait_with_a_reason() {
        assert_eq!(choice(&pr(), true, None, false, true, false), Choice::Wait("Another Claude run is working on this PR"));
        for ci in [Ci::Running, Ci::Fail, Ci::None] {
            assert!(matches!(choice(&Pr { ci, ..pr() }, true, None, false, false, false), Choice::Wait(_)), "{ci:?}");
        }
    }

    #[test]
    fn prs_the_fresh_read_turned_down_cannot_starve_a_new_request() {
        let at = |n: u64, asked: &str| Pr { url: format!("https://github.com/o/r/pull/{n}"), asked_at: asked.into(), updated_at: "2026-11-05T09:00:00Z".into(), ..pr() };
        let prs = [at(1, "2026-11-05T09:01:00Z"), at(2, "2026-11-05T09:02:00Z"), at(3, "2026-11-05T09:03:00Z"), at(4, "2026-11-05T09:04:00Z")];
        let urls = |q: Vec<&Pr>| q.iter().map(|p| p.url.rsplit('/').next().unwrap().parse::<u64>().unwrap()).collect::<Vec<_>>();
        let none = HashMap::new();
        assert_eq!(urls(queue(prs.iter().collect(), &BTreeMap::new(), &none)), [1, 2, 3, 4], "the longest waiting request first");
        let turned_down: HashMap<String, String> = prs[..3].iter().map(|p| (p.url.clone(), listed_as(p))).collect();
        assert_eq!(urls(queue(prs.iter().collect(), &BTreeMap::new(), &turned_down)), [4], "older PRs the fresh read refused stay out");
        let pushed = [Pr { head_sha: "d".repeat(40), ..prs[0].clone() }, prs[3].clone()];
        assert_eq!(urls(queue(pushed.iter().collect(), &BTreeMap::new(), &turned_down)), [1, 4], "a PR that changed is read again");
        let stuck = map(&[(prs[0].url.as_str(), UNCONFIRMED)]);
        assert_eq!(urls(queue(prs.iter().collect(), &stuck, &none)), [2, 3, 4, 1], "unconfirmed checks go to the back");
    }

    #[test]
    fn settings_default_to_on_at_seventy_and_ninety_and_old_files_keep_them() {
        let state: State = serde_json::from_str(r#"{"settings":{"enabled":false},"attempts":{}}"#).unwrap();
        assert_eq!(state.settings, Settings { enabled: false, session_limit: 70, weekly_limit: 90 });
        let auto = AutoReview::at(temp());
        assert_eq!(auto.status()["settings"], serde_json::json!({ "enabled": true, "sessionLimit": 70, "weeklyLimit": 90 }));
        assert!(auto.status()["running"].is_null());
        assert_eq!(auto.status()["skipsSynced"], false, "nothing starts before a page sends its Skip map");
    }

    #[test]
    fn requests_count_from_when_it_was_first_run_or_turned_back_on() {
        let path = temp();
        let before = now_ms();
        let auto = AutoReview::at(path.clone());
        let first = auto.state.lock().unwrap().on_since;
        assert!(first >= before, "a new file counts from now");
        auto.configure(Some(true), Some(60), None).unwrap();
        assert_eq!(auto.state.lock().unwrap().on_since, first, "on to on keeps the time");
        auto.state.lock().unwrap().on_since = 5;
        auto.configure(Some(false), None, None).unwrap();
        assert_eq!(auto.state.lock().unwrap().on_since, 5);
        auto.configure(Some(true), None, None).unwrap();
        assert!(auto.state.lock().unwrap().on_since >= before, "off to on counts from now");
        let old: State = serde_json::from_str(r#"{"settings":{"enabled":true}}"#).unwrap();
        std::fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
        assert!(AutoReview::at(path.clone()).state.lock().unwrap().on_since >= before, "a file without the time counts from now");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn limits_and_skip_changes_are_validated() {
        assert_eq!(limit(None), Ok(None));
        assert_eq!(limit(Some(10)), Ok(Some(10)));
        assert_eq!(limit(Some(100)), Ok(Some(100)));
        for bad in [9, 101, -1, 1000] { assert!(limit(Some(bad)).is_err(), "{bad}"); }
        let none = BTreeMap::new();
        assert_eq!(skips_refusal(&map(&[("https://github.com/o/r/pull/7", "2026-11-05T10:00:00.000Z")]), &none), None);
        assert_eq!(skips_refusal(&none, &map(&[("https://github.com/o/r/pull/7", "2026-11-05T10:00:00Z")])), None);
        assert!(skips_refusal(&map(&[("https://evil.test/o/r/pull/7", "2026-11-05T10:00:00.000Z")]), &none).is_some());
        assert!(skips_refusal(&map(&[("https://github.com/o/r/pull/7", "yesterday")]), &none).is_some());
        assert!(skips_refusal(&none, &map(&[("https://github.com/o/r/pull/7", "2026-11-05T11:00:00+01:00")])).is_some(), "only UTC compares with GitHub's times");
        let many: BTreeMap<_, _> = (0..=MAX_SKIPPED).map(|n| (format!("https://github.com/o/r/pull/{}", n + 1), "2026-11-05T10:00:00Z".to_string())).collect();
        assert!(skips_refusal(&many, &none).is_some());
    }

    #[test]
    fn no_page_can_drop_or_revive_another_pages_skip_with_an_older_copy() {
        let (x, y) = ("https://github.com/o/r/pull/1", "https://github.com/o/r/pull/2");
        let mut s = State::default();
        // The app window skips X; later a browser loaded before that skips Y and sends only its own change.
        merge_skips(&mut s, map(&[(x, "2026-11-05T10:00:00.000Z")]), BTreeMap::new());
        merge_skips(&mut s, map(&[(y, "2026-11-05T10:30:00.000Z")]), BTreeMap::new());
        assert_eq!(s.skipped, map(&[(x, "2026-11-05T10:00:00.000Z"), (y, "2026-11-05T10:30:00.000Z")]));
        // A whole map with an older time for X keeps the later one.
        merge_skips(&mut s, map(&[(x, "2026-11-05T09:00:00.000Z")]), BTreeMap::new());
        assert_eq!(s.skipped[x], "2026-11-05T10:00:00.000Z");
        // Unskip X in the app; a browser still holding the old skip cannot bring it back.
        merge_skips(&mut s, BTreeMap::new(), map(&[(x, "2026-11-05T11:00:00.000Z")]));
        assert!(!s.skipped.contains_key(x));
        merge_skips(&mut s, map(&[(x, "2026-11-05T10:00:00.000Z"), (y, "2026-11-05T10:30:00.000Z")]), BTreeMap::new());
        assert!(!s.skipped.contains_key(x));
        // Skipping it again afterwards counts, and an older unskip does not undo that.
        merge_skips(&mut s, map(&[(x, "2026-11-05T12:00:00.000Z")]), BTreeMap::new());
        merge_skips(&mut s, BTreeMap::new(), map(&[(x, "2026-11-05T11:00:00.000Z")]));
        assert_eq!(s.skipped[x], "2026-11-05T12:00:00.000Z");
    }

    #[test]
    fn only_a_whole_map_from_a_page_lets_automatic_starts_begin() {
        let path = temp();
        let auto = AutoReview::at(path.clone());
        auto.take_skips(map(&[(pr().url.as_str(), "2026-11-05T10:00:00Z")]), BTreeMap::new(), false).unwrap();
        assert_eq!(auto.status()["skipsSynced"], false, "one click is not the page's whole map");
        auto.take_skips(BTreeMap::new(), BTreeMap::new(), true).unwrap();
        let again = AutoReview::at(path.clone());
        let s = again.state.lock().unwrap().clone();
        assert!(s.skips_synced);
        assert_eq!(s.skipped.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_unconfirmed_start_is_interrupted_after_restart_and_not_repeated() {
        let path = temp();
        let auto = since_nine(path.clone());
        let attempt = Attempt { sha: HEAD.into(), run_id: None, started_at: 5, status: "starting".into(), summary: String::new() };
        auto.change(|s| { s.attempts.insert(pr().url, attempt); }).unwrap();
        let restarted = AutoReview::at(path.clone());
        assert_eq!(restarted.status()["running"], pr().url);
        restarted.reconcile(&Store::empty()).unwrap();
        let a = restarted.state.lock().unwrap().attempts[&pr().url].clone();
        assert_eq!(a.status, "interrupted");
        assert!(restarted.status()["running"].is_null());
        assert_eq!(restarted.judge(&pr(), &Store::empty()), Choice::Skip, "the interrupted PR is not started again");
        let pushed = Pr { head_sha: "d".repeat(40), ..pr() };
        assert_eq!(restarted.judge(&pushed, &Store::empty()), Choice::Skip, "not even on a new head");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_start_saved_before_its_run_adopts_the_run_after_a_restart() {
        let starting = Attempt { sha: HEAD.into(), run_id: None, started_at: 500, status: "starting".into(), summary: String::new() };
        let restarted = |store: &Store| {
            let auto = AutoReview::at(temp());
            auto.change(|s| { s.attempts.insert(pr().url, starting.clone()); }).unwrap();
            auto.reconcile(store).unwrap();
            std::fs::remove_dir_all(auto.path.parent().unwrap()).unwrap();
            auto
        };
        let auto = restarted(&Store::with_approve("r1", &pr().url, 501, Status::Running, ""));
        let a = auto.state.lock().unwrap().attempts[&pr().url].clone();
        assert_eq!((a.run_id.as_deref(), a.status.as_str()), (Some("r1"), "running"));
        assert_eq!(auto.status()["running"], pr().url, "it still counts as the one automatic run");
        let auto = restarted(&Store::with_approve("r0", &pr().url, 499, Status::Running, ""));
        assert_eq!(auto.state.lock().unwrap().attempts[&pr().url].status, "interrupted", "a run from before the attempt is not its run");
    }

    #[test]
    fn an_attempt_follows_its_run_to_the_end_and_keeps_its_last_line() {
        let auto = AutoReview::at(temp());
        let attempt = Attempt { sha: HEAD.into(), run_id: Some("r1".into()), started_at: 500, status: "running".into(), summary: String::new() };
        auto.change(|s| { s.attempts.insert(pr().url, attempt); }).unwrap();
        auto.reconcile(&Store::with_approve("r1", &pr().url, 500, Status::Done, "FAIL: 2 blocking findings\nDetails")).unwrap();
        let a = auto.state.lock().unwrap().attempts[&pr().url].clone();
        assert_eq!((a.status.as_str(), a.summary.as_str()), ("done", "FAIL: 2 blocking findings"));
        assert!(auto.status()["running"].is_null(), "the next PR may start");
        std::fs::remove_dir_all(auto.path.parent().unwrap()).unwrap();
    }

    #[test]
    fn an_unreadable_history_starts_nothing_and_is_kept() {
        let path = temp();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ torn").unwrap();
        let auto = AutoReview::at(path.clone());
        assert!(auto.status()["error"].as_str().unwrap().contains("nothing starts"));
        assert!(auto.configure(Some(false), None, None).is_err());
        assert!(auto.take_skips(BTreeMap::new(), BTreeMap::new(), true).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ torn");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn settings_and_skips_persist_and_an_empty_change_writes_nothing() {
        let path = temp();
        let auto = AutoReview::at(path.clone());
        auto.configure(Some(false), Some(50), None).unwrap();
        auto.take_skips(map(&[(pr().url.as_str(), "2026-11-05T10:00:00Z")]), BTreeMap::new(), true).unwrap();
        let again = AutoReview::at(path.clone());
        let s = again.state.lock().unwrap().clone();
        assert_eq!(s.settings, Settings { enabled: false, session_limit: 50, weekly_limit: 90 });
        assert_eq!(s.skipped.len(), 1);
        std::fs::remove_file(&path).unwrap();
        auto.change(|s| s.error = None).unwrap();
        assert!(!path.exists(), "nothing changed, so nothing was written");
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_notice_counts_what_each_pr_waits_for() {
        let mut waiting = BTreeMap::new();
        assert_eq!(waits(&waiting), "");
        waiting.insert("a".to_string(), "Checks are still running".to_string());
        assert_eq!(waits(&waiting), "1 PR waits: Checks are still running.");
        waiting.insert("b".to_string(), "Checks are still running".to_string());
        waiting.insert("c".to_string(), "Session at 100%, resets 15:00".to_string());
        assert_eq!(waits(&waiting), "3 PRs wait: Checks are still running (2). Session at 100%, resets 15:00.");
        assert_eq!(short("https://github.com/o/pr-inbox/pull/781"), "pr-inbox#781");
    }
}
