// PR Inbox: a native window over a local Rust server that lists your open GitHub pull requests,
// runs gh actions on them, and hands any of them to a background Claude Code session.
//
//   PR Inbox.app          opens the window (starts the server inside the app)
//   pr-inbox --server     runs only the server, for a browser at http://localhost:4477
//   pr-inbox record-review --repo o/r --branch b --sha s --verdict pass|fail [--summary text]
mod detail;
mod github;
mod overview;
mod release;
mod reviews;
mod runs;
mod server;
mod util;
mod workspace;

use std::sync::Arc;

const PORT: u16 = 4477;

fn main() {
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
    util::fix_path();
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
            let app = server::router(store.clone(), prs.clone(), port);
            runtime.spawn(async move {
                let _ = axum::serve(listener, app).await;
            });
            runtime.spawn(runs::ticker(store.clone()));
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
    tauri::Builder::default()
        .setup(move |app| {
            tauri::WebviewWindowBuilder::new(app, "main", tauri::WebviewUrl::External(url.parse().expect("valid url")))
                .title("PR Inbox")
                .inner_size(1480.0, 960.0)
                .min_inner_size(820.0, 560.0)
                .build()?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run the PR Inbox window");
}
