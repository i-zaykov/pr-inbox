// Overviews cover every PR at once: the triage you ask for, and the morning summary that runs by itself.
// Both are read-only Claude runs over a list this module builds from GitHub (and, for the morning, recent runs).
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

use crate::github::{Cache, Lane, Pr, Snapshot};
use crate::runs::{Kind, Store};
use crate::util::{data_dir, local_date, local_hour, now_ms, read_json, serde_name, write_atomic};

const DAY_MS: u64 = 86_400_000;

const TRIAGE: &str = "Here are my open pull requests. Check each one with gh where the line below is not enough (latest CI, review comments, whether it can merge), then put them in priority order (merge, fix, rebase, review, close), one line each with the reason, and say which one to start with. Do not change anything.";

const DIGEST: &str = "Write my morning summary of pull requests. Go through each PR below with gh: read failing CI logs, new review comments, and whether it can merge. Also say what the Claude runs listed below achieved. Do not change, comment on, approve or merge anything.

Format:
- First line: one sentence on what to do first today.
- Then short sections, only those that have items: Merge now, Needs a fix, Waiting on me, Waiting on others, Close. One line per PR: the PR, what is going on, the next action.
- Last line: what the Claude runs did.
Keep it under 250 words, and make it your last message: leave no background task running.";

/// Small state that outlives the run list, so dismissing a card cannot undo it.
#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct State {
    last_digest: String,
}

fn read_state() -> State {
    read_json(&data_dir().join("state.json"))
}

/// The hour the morning summary starts, 08:00 unless PR_INBOX_MORNING_HOUR says otherwise.
fn morning_hour() -> u32 {
    std::env::var("PR_INBOX_MORNING_HOUR").ok().and_then(|h| h.parse().ok()).filter(|h| *h < 24).unwrap_or(8)
}

pub fn schedule_label() -> String {
    format!("{:02}:00", morning_hour())
}

fn line(pr: &Pr) -> String {
    let mut state = vec![format!("CI {}", serde_name(&pr.ci)), serde_name(&pr.merge_state).to_lowercase()];
    state.extend(pr.review.as_ref().map(|r| serde_name(r).to_lowercase()));
    if pr.is_draft {
        state.push("draft".into());
    }
    if pr.open_threads > 0 {
        state.push(format!("{} open review threads", pr.open_threads));
    }
    format!("- {}#{} \"{}\" by {} ({}) {}", pr.repo, pr.number, pr.title, pr.author, state.join(", "), pr.url)
}

/// The list an overview run reads: every PR, and for the morning summary the last day's runs.
fn context(snap: &Snapshot, store: &Store, kind: Kind) -> String {
    let mut lines = vec!["My open pull requests:".to_string()];
    lines.extend(snap.mine.iter().map(line));
    for (lane, heading) in [(Lane::Waiting, "Waiting on me (review requests and assignments)"), (Lane::Reviewed, "Reviewed by me, waiting on the author")] {
        let prs: Vec<String> = snap.others.iter().filter(|p| p.lane == lane).map(line).collect();
        if !prs.is_empty() {
            lines.push(format!("\n{heading}:"));
            lines.extend(prs);
        }
    }
    if kind == Kind::Digest {
        let activity = store.activity_since(now_ms().saturating_sub(DAY_MS));
        lines.push("\nClaude runs from the last day:".into());
        lines.extend(if activity.is_empty() { vec!["- none".to_string()] } else { activity });
    }
    lines.join("\n")
}

/// The whole prompt of a triage or morning summary run.
pub fn prompt(kind: Kind, snap: &Snapshot, store: &Store) -> String {
    format!("{}\n\n{}", if kind == Kind::Digest { DIGEST } else { TRIAGE }, context(snap, store, kind).trim())
}

/// Starts the morning summary once a day: at the set hour, or as soon as the server is up after it.
/// The day is recorded when it starts, so dismissing its card does not start another.
pub async fn morning_loop(store: Arc<Store>, prs: Arc<Cache>) {
    let mut every = tokio::time::interval(Duration::from_secs(60));
    let mut last_digest = read_state().last_digest;
    loop {
        every.tick().await;
        if local_hour() < morning_hour() || last_digest == local_date(now_ms()) {
            continue;
        }
        // If GitHub is unreachable, the next minute tries again.
        let Ok(snap) = prs.recent().await else { continue };
        if store.create_overview(Kind::Digest, prompt(Kind::Digest, &snap, &store)).is_ok() {
            last_digest = local_date(now_ms());
            if let Ok(text) = serde_json::to_string(&State { last_digest: last_digest.clone() }) {
                let _ = write_atomic(&data_dir().join("state.json"), &text);
            }
        }
    }
}
