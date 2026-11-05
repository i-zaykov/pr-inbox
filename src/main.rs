// PR Inbox: a native window over a local Rust server that lists your open GitHub pull requests,
// runs gh actions on them, and hands any of them to a background Claude Code session.
//
//   PR Inbox.app          opens the window (starts the server inside the app)
//   pr-inbox --server     runs only the server, for a browser at http://localhost:4477
//   pr-inbox record-review --repo o/r --branch b --sha s --verdict pass|fail [--summary text]
mod auto_review;
mod detail;
mod deep_link;
mod github;
mod overview;
mod overnight;
mod release;
mod reviews;
mod review_flow;
mod runs;
mod server;
mod usage;
mod util;
mod workspace;

use std::sync::{Arc, Mutex};
use tauri::Manager;

const PORT: u16 = 4477;

fn main() {
    util::fix_path();
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("record-review") {
        match reviews::record_from_args(&args[2..]) {
            Ok(line) => println!("{line}"),
            Err(e) => {
                eprintln!("record-review: {e}");
                std::process::exit(2);
            }
        }
        return;
    }
    let headless = std::env::args().any(|a| a == "--server");
    let port: u16 = std::env::var("PR_INBOX_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(PORT);

    let runtime = tokio::runtime::Runtime::new().expect("start the async runtime");
    let listener = runtime.block_on(tokio::net::TcpListener::bind(("127.0.0.1", port)));
    match listener {
        // Only the process that owns the port reads, schedules and writes the runs: a second copy that
        // also loaded them could launch a queued run twice and overwrite runs.json from a stale view.
        Ok(listener) => {
            let store = Arc::new(runs::Store::load());
            let prs = Arc::new(github::Cache::default());
            let overnight = Arc::new(overnight::Batch::load());
            store.set_overnight_limit(overnight.active());
            let auto = Arc::new(auto_review::AutoReview::load());
            let app = server::router(store.clone(), prs.clone(), overnight.clone(), auto.clone(), port);
            runtime.spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            runtime.spawn(runs::ticker(store.clone()));
            runtime.spawn(overnight::worker(overnight, store.clone(), prs.clone()));
            runtime.spawn(auto_review::worker(auto, store.clone(), prs.clone()));
            runtime.spawn(overview::morning_loop(store, prs));
            eprintln!("PR Inbox is serving http://localhost:{port}");
        }
        // Another copy already serves the port: the window shows its page. A background server exits
        // instead, so launchd starts it again once the port is free.
        Err(e) if headless => {
            eprintln!("Port {port} is taken ({e}); exiting so launchd retries.");
            std::process::exit(1);
        }
        Err(e) => eprintln!("Port {port} is taken ({e}); opening the copy that is already running."),
    }

    if headless {
        runtime.block_on(std::future::pending::<()>());
        return;
    }

    let url = format!("http://localhost:{port}/?app=1");
    let links = Arc::new(Mutex::new(deep_link::Delivery::default()));
    let page_links = links.clone();
    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url.parse().expect("valid url")))
                .on_page_load(move |window, payload| {
                    let mut links = page_links.lock().unwrap();
                    links.ready = payload.event() == tauri::webview::PageLoadEvent::Finished
                        && payload.url().scheme() == "http" && payload.url().host_str() == Some("localhost")
                        && payload.url().port() == Some(port);
                    if links.ready {
                        for request in links.pending.drain(..) {
                            deep_link::deliver(&window, request);
                        }
                    }
                })
                .title("PR Inbox")
                .inner_size(1480.0, 960.0)
                .min_inner_size(820.0, 560.0)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build the PR Inbox window")
        .run(move |app, event| {
            if let tauri::RunEvent::Opened { urls } = event {
                let window = app.get_webview_window("main");
                if let Some(window) = &window {
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                }
                let mut links = links.lock().unwrap();
                for url in urls {
                    let request = deep_link::Request::from(deep_link::parse(&url));
                    match &window {
                        Some(window) if links.ready => deep_link::deliver(window, request),
                        _ => links.pending.push(request),
                    }
                }
            }
        });
}
