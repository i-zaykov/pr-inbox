// The local HTTP API the page talks to. It listens on 127.0.0.1 only and answers only requests addressed
// to localhost (DNS rebinding cannot reach it). Every API call must carry a custom header: other sites
// cannot add one without a CORS preflight this server never answers, so they can neither read nor write.
use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use crate::github::{is_pr_url, Cache, PrAction, Snapshot};
use crate::overview;
use crate::release::{self, Releases};
use crate::reviews;
use crate::runs::{NewRun, RunError, Store};
use crate::util::{first_line, open_terminal, sh, shq, Output};
use crate::workspace;

const INDEX: &str = include_str!("../public/index.html");

struct App {
    store: Arc<Store>,
    prs: Arc<Cache>,
    releases: Arc<Releases>,
    port: u16,
}

type Shared = Arc<App>;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

impl IntoResponse for RunError {
    fn into_response(self) -> Response {
        match self {
            RunError::NotFound => err(StatusCode::NOT_FOUND, "No such run"),
            RunError::Busy(msg) => err(StatusCode::CONFLICT, msg),
            RunError::Invalid(msg) => err(StatusCode::BAD_REQUEST, msg),
            RunError::Failed(msg) => err(StatusCode::BAD_GATEWAY, format!("Could not set up the PR's worktree: {msg}")),
        }
    }
}

/// A host command's result as the page sees it: an error when it failed, not a quiet success.
fn done(out: Output, what: &str) -> Response {
    if out.code == 0 {
        Json(json!({ "ok": true })).into_response()
    } else {
        err(StatusCode::BAD_GATEWAY, format!("{what} failed: {}", first_line(&out.stderr)))
    }
}

fn refused(req: &Request, port: u16) -> Option<&'static str> {
    let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
    let ours = |value: &str, scheme: &str| value == format!("{scheme}localhost:{port}") || value == format!("{scheme}127.0.0.1:{port}");
    if !header("host").is_some_and(|h| ours(h, "")) {
        return Some("Wrong host");
    }
    let api = req.uri().path().starts_with("/api/");
    if api && header("x-pr-inbox") != Some("1") {
        return Some("Missing the PR Inbox header");
    }
    if header("origin").is_some_and(|o| !ours(o, "http://")) {
        return Some("Wrong origin");
    }
    None
}

async fn guard(State(app): State<Shared>, req: Request, next: Next) -> Response {
    if let Some(why) = refused(&req, app.port) {
        return err(StatusCode::FORBIDDEN, why);
    }
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("frame-ancestors 'none'"));
    res
}

async fn index() -> Html<&'static str> {
    Html(INDEX)
}

async fn health() -> Json<Value> {
    let t = Duration::from_secs(20);
    let (gh, auth, ver) = tokio::join!(
        sh("gh", &["api", "user", "--jq", ".login"], t),
        sh("claude", &["auth", "status"], t),
        sh("claude", &["--version"], t),
    );
    let logged_in = serde_json::from_str::<Value>(&auth.stdout).is_ok_and(|v| v["loggedIn"] == true);
    Json(json!({
        "gh": { "ok": gh.code == 0, "error": (gh.code != 0).then(|| first_line(&gh.stderr)) },
        "claude": { "installed": ver.code == 0, "loggedIn": logged_in },
        "morning": overview::schedule_label(),
    }))
}

/// The PR list as the page reads it: the snapshot, plus the thermo-nuclear review standing of each of
/// your PRs that has one, keyed by PR URL.
#[derive(Serialize)]
struct PrList {
    #[serde(flatten)]
    snap: Snapshot,
    thermo: HashMap<String, reviews::Standing>,
}

/// Fetches the PR list for the page, and removes PR Inbox's worktrees of PRs on neither list (merged,
/// closed, or no longer waiting on you) when nothing in them is unsaved. A list GitHub sent only part of
/// skips that, so a PR that failed to load keeps its worktree.
async fn list_prs(State(app): State<Shared>) -> Response {
    let snap = match app.prs.refresh().await {
        Ok(snap) => snap,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    if snap.warning.is_none() {
        let open: HashSet<(String, u64)> = snap.mine.iter().chain(&snap.waiting).map(|p| (p.name.clone(), p.number)).collect();
        let store = app.store.clone();
        tokio::spawn(async move { workspace::sweep(open, store.active_dirs()).await });
    }
    let ledger = reviews::load();
    let thermo = snap.mine.iter().filter_map(|p| reviews::standing(&ledger, p).map(|s| (p.url.clone(), s))).collect();
    Json(PrList { snap, thermo }).into_response()
}

async fn list_runs(State(app): State<Shared>) -> Json<Value> {
    Json(app.store.list())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ActionReq {
    action: PrAction,
    url: String,
    /// The head commit the page showed; a merge is refused by GitHub if the PR moved since.
    #[serde(default)]
    head_sha: Option<String>,
}

async fn pr_action(State(app): State<Shared>, Json(req): Json<ActionReq>) -> Response {
    if !is_pr_url(&req.url) {
        return err(StatusCode::BAD_REQUEST, "Not a pull request URL");
    }
    // An admin merge skips branch protection, so the server checks the PR as GitHub has it, not as the page saw it.
    if req.action == PrAction::AdminMerge {
        match app.prs.open_pr(&req.url).await {
            Ok(Some(pr)) => {
                if let Some(why) = pr.admin_merge_refusal() {
                    return err(StatusCode::CONFLICT, why);
                }
            }
            Ok(None) => return err(StatusCode::NOT_FOUND, "That PR is no longer open"),
            Err(e) => return err(StatusCode::BAD_GATEWAY, e),
        }
    }
    let args = match req.action.gh_args(&req.url, req.head_sha.as_deref()) {
        Ok(args) => args,
        Err(e) => return err(StatusCode::BAD_REQUEST, e),
    };
    let out = sh("gh", &args, Duration::from_secs(120)).await;
    let msg = first_line(if out.stdout.trim().is_empty() { &out.stderr } else { &out.stdout });
    if out.code == 0 { Json(json!({ "ok": true, "output": msg })).into_response() } else { err(StatusCode::UNPROCESSABLE_ENTITY, msg) }
}

/// Starts a run. The PR comes from GitHub as it is now (at most a minute old), never from the page,
/// so the checkout, the branch and the prompt match the real PR.
async fn create_run(State(app): State<Shared>, Json(req): Json<NewRun>) -> Response {
    let NewRun { kind, url, note } = req;
    if kind.is_overview() {
        let snap = match app.prs.recent().await {
            Ok(snap) => snap,
            Err(e) => return err(StatusCode::BAD_GATEWAY, e),
        };
        return app.store.create_overview(kind, overview::prompt(kind, &snap, &app.store)).map(Json).into_response();
    }
    let Some(url) = url.filter(|u| is_pr_url(u)) else { return err(StatusCode::BAD_REQUEST, "A pull request URL is required") };
    let pr = match app.prs.open_pr(&url).await {
        Ok(Some(pr)) => pr,
        Ok(None) => return err(StatusCode::NOT_FOUND, "That PR is no longer open"),
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    // Setting up the worktree runs git in your repo. It runs to the end even when the page goes away, so a
    // dropped request never kills git halfway through and leaves a lock file behind.
    let store = app.store.clone();
    match tokio::spawn(async move { store.create_pr(kind, pr, note).await }).await {
        Ok(result) => result.map(Json).into_response(),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Starting the run crashed"),
    }
}

/// How each repo you have a PR in releases: its method, latest tag, the next one and what is unreleased.
async fn release_plans(State(app): State<Shared>) -> Response {
    let snap = match app.prs.recent().await {
        Ok(snap) => snap,
        Err(e) => return err(StatusCode::BAD_GATEWAY, e),
    };
    let repos: BTreeSet<String> = snap.mine.iter().map(|p| p.repo.clone()).collect();
    Json(app.releases.plans(repos.into_iter().collect()).await).into_response()
}

#[derive(Deserialize)]
struct ReleaseReq {
    repo: String,
}

/// Cuts a release of one repo. It runs to the end even when the page goes away, so a release is never
/// left half made. The repo need not have an open PR: the page releases right after merging the last one.
async fn release_repo(State(app): State<Shared>, Json(req): Json<ReleaseReq>) -> Response {
    if !release::is_repo(&req.repo) {
        return err(StatusCode::BAD_REQUEST, "Not a repository name");
    }
    let releases = app.releases.clone();
    match tokio::spawn(async move { releases.release(&req.repo).await }).await {
        Ok(Ok(done)) => Json(done).into_response(),
        Ok(Err(e)) => err(StatusCode::UNPROCESSABLE_ENTITY, e),
        Err(_) => err(StatusCode::INTERNAL_SERVER_ERROR, "Releasing crashed"),
    }
}

async fn get_run(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    app.store.get(&id).map(Json).into_response()
}

#[derive(Deserialize, Default)]
struct ReplyReq {
    #[serde(default)]
    text: String,
}

async fn run_verb(State(app): State<Shared>, Path((id, verb)): Path<(String, String)>, body: Option<Json<ReplyReq>>) -> Response {
    let store = &app.store;
    match verb.as_str() {
        "stop" => store.stop(&id).map(Json).into_response(),
        "retry" => store.reply(&id, None).map(Json).into_response(),
        "reply" => store.reply(&id, Some(body.unwrap_or_default().0.text)).map(Json).into_response(),
        "dismiss" => store.dismiss(&id).map(|_| Json(json!({ "ok": true }))).into_response(),
        "terminal" => match store.takeover(&id) {
            Ok((cwd, session)) => done(open_terminal(&format!("cd {} && claude --resume {}", shq(&cwd), shq(&session))).await, "Opening Terminal"),
            Err(e) => e.into_response(),
        },
        _ => err(StatusCode::NOT_FOUND, "Not found"),
    }
}

async fn login(Path(tool): Path<String>) -> Response {
    let command = match tool.as_str() {
        "claude" => "claude auth login",
        "gh" => "gh auth login --web",
        _ => return err(StatusCode::NOT_FOUND, "Not found"),
    };
    done(open_terminal(command).await, "Opening Terminal")
}

#[derive(Deserialize)]
struct OpenReq {
    url: String,
}

/// Links inside the app window open in your default browser.
async fn open_url(Json(req): Json<OpenReq>) -> Response {
    if !req.url.starts_with("https://") {
        return err(StatusCode::BAD_REQUEST, "Only https links open from here");
    }
    done(sh("open", &[&req.url], Duration::from_secs(10)).await, "Opening the link")
}

pub fn router(store: Arc<Store>, prs: Arc<Cache>, port: u16) -> Router {
    let app = Arc::new(App { store, prs, releases: Arc::new(Releases::default()), port });
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/api/health", get(health))
        .route("/api/prs", get(list_prs))
        .route("/api/runs", get(list_runs).post(create_run))
        .route("/api/runs/{id}", get(get_run))
        .route("/api/runs/{id}/{verb}", post(run_verb))
        .route("/api/pr-action", post(pr_action))
        .route("/api/releases", get(release_plans))
        .route("/api/release", post(release_repo))
        .route("/api/login/{tool}", post(login))
        .route("/api/open", post(open_url))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A path, its headers, and the refusal expected.
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], Option<&'a str>);

    #[test]
    fn only_requests_addressed_to_this_server_get_through() {
        let ok = ("host", "localhost:4477");
        let cases: [Case; 11] = [
            ("/", &[ok], None),
            ("/", &[("host", "127.0.0.1:4477")], None),
            ("/", &[("host", "evil.com")], Some("Wrong host")),
            ("/", &[("host", "localhost")], Some("Wrong host")),
            ("/", &[("host", "localhost:4478")], Some("Wrong host")),
            ("/", &[], Some("Wrong host")),
            ("/api/prs", &[ok], Some("Missing the PR Inbox header")),
            ("/api/prs", &[ok, ("x-pr-inbox", "1")], None),
            ("/", &[ok, ("origin", "http://evil.com")], Some("Wrong origin")),
            ("/", &[ok, ("origin", "http://localhost:4477")], None),
            ("/api/prs", &[ok, ("x-pr-inbox", "1"), ("origin", "http://localhost:4477")], None),
        ];
        for (path, headers, want) in cases {
            let mut req = axum::http::Request::builder().uri(path);
            for (name, value) in headers {
                req = req.header(*name, *value);
            }
            let req = req.body(axum::body::Body::empty()).unwrap();
            assert_eq!(refused(&req, 4477), want, "{path} {headers:?}");
        }
    }
}
