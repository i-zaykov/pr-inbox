# Auto thermo

PR Inbox can start Thermo & approve by itself for the first review of a PR. When someone asks for your review on their PR, its checks pass and your Claude plan has room, the server starts the same run as the **Thermo & approve** button. The app window can close while the background server works.

Auto thermo is on by default. It takes only requests for your review sent after it was turned on. PRs that were already waiting on you when it first ran, or when you last turned it back on, stay as they are until someone asks for your review on them again.

## What starts it

A PR qualifies when all of these hold:

- It is under Waiting on you, open and not a draft, and it asks for your review, by name or through one of your teams. Being assigned to it is not enough.
- That request came after Auto thermo was turned on, or the PR was opened after that.
- Thermo & approve may take it.
- You have not skipped it since the latest request for your review.
- Nobody has reviewed it from your account yet. You have left no review on GitHub, Auto thermo has never started on it, and no Thermo & approve run you clicked reviewed it, on any commit.
- No other Claude run is working on it.
- Every check has passed. Running, failing or missing checks make it wait.

Right before a start, PR Inbox reads the PR from GitHub again, checks all of the above once more, and asks GitHub whether every check passed. The PR list cannot tell which teams are yours, so this fresh read decides for a request sent to a team. When the fresh read says no, PR Inbox does not ask about that PR again until it changes on GitHub.

A PR that will start once something clears shows an **Auto thermo next** tag. Its tooltip says what it waits for: checks, usage, or another automatic run.

Only the first review is automatic. Once a review from you is on the PR, a new commit or a new request for your review does not start it again. Review again with **Thermo & approve** when you choose to.

## The usage gate

Auto thermo starts a review only while your five-hour Claude session is below 70% and every weekly limit is below 90%. The notice under the header offers 50, 60, 70, 80, 85 or 90% for the session and 70, 80, 90 or 95% for the week. The settings call under [State and troubleshooting](#state-and-troubleshooting) takes any whole percentage from 10 to 100.

The numbers come from the endpoint Claude Code's own usage screen reads, `https://api.anthropic.com/api/oauth/usage`. PR Inbox signs that request with the token Claude Code keeps in the keychain entry `Claude Code-credentials`. It hands the token to curl on stdin, so the token never appears in a process list, a log or an error. Session is the five-hour figure. Weekly is the fullest of the weekly figures, per-model limits included.

PR Inbox reads usage only when a PR is ready to start. It reuses a reading for five minutes, because the endpoint rate-limits. Right before each start it reads again if the reading is over a minute old.

If usage cannot be read, nothing starts, and the notice says why. That covers a refused keychain read, an expired token, a refused or rate-limited request and an unreadable reply. The token expires between Claude Code sessions; running Claude Code renews it. If macOS asks whether `security` may use the Claude Code keychain entry, choose Always Allow. A prompt nobody answers within 20 seconds counts as unknown usage.

## What it publishes

The run is the button's own run, and PR Inbox publishes its result from your account the same way. A passing review approves the PR, with any non-blocking notes in its text. Blocking findings request changes and list what to fix. When you start a later Thermo & approve by hand, that review starts from your earlier one and the changes since it. It blocks only on unfixed findings, on code changed since, or on a confirmed bug with real impact, so findings do not arrive one round at a time. An incomplete review publishes nothing. Before publishing, PR Inbox checks that the reviewed commits still match the PR. Before it approves, it also checks that every check passed.

Each automatic start sends a macOS notification naming the PR and your session percentage. The run's card says it started automatically.

## One at a time

At most one automatic run is active at a time. Other ready PRs wait until it finishes. Runs you start by hand are not counted. A running daily review batch still limits writing runs to two at once, and an automatic run queues behind that limit like a manual one.

While Thermo & approve runs on a PR, automatic or clicked, PR Inbox refuses a second Thermo & approve on it, so two reviews of one commit cannot publish opposite verdicts.

## Turning it off and skipping

- Click **Auto thermo: on** in the header to turn it off, and again to turn it back on. Turning it off stops new starts. A run it already started keeps going; use Stop on its card to end it.
- **Skip** on a row keeps that PR out until someone asks for your review again. Each Skip or Unskip sends only that change to the server. A page that loads sends its whole Skip map, the server keeps the later choice for each PR, and the page takes the result. So a Skip or Unskip made in the app window holds, even when a browser tab with an older copy sends its own changes later.
- The server starts nothing until a PR Inbox page has sent its Skip map once, because a Skip may live only in a page's storage. After an install, or when `auto-review.json` is new, open PR Inbox once. Until then the notice says so for each PR that would start.

## State and troubleshooting

The state is stored in `~/Library/Application Support/PR Inbox/auto-review.json`. It holds the settings, when Auto thermo was last turned on, the last automatic start on each PR with its head commit and run, your Skip and Unskip choices, the last usage reading and what each PR waits for. Nothing in it is secret.

PR Inbox saves an attempt before it creates the run. After a restart, a start whose run cannot be found is marked interrupted, and that head does not start again by itself. If the file cannot be read, nothing starts automatically and the file is kept as it is. The server checks once a minute, and at once after a settings or Skip change. The server log is `~/Library/Logs/PR Inbox.log`.

The local API uses the installed server port, normally 4477:

```sh
curl -fsS -H 'x-pr-inbox: 1' http://localhost:4477/api/auto-review
curl -fsS -X POST -H 'x-pr-inbox: 1' -H 'content-type: application/json' -d '{"enabled":false}' http://localhost:4477/api/auto-review/settings
curl -fsS -X POST -H 'x-pr-inbox: 1' -H 'content-type: application/json' -d '{"sessionLimit":60,"weeklyLimit":85}' http://localhost:4477/api/auto-review/settings
curl -fsS -X POST -H 'x-pr-inbox: 1' -H 'content-type: application/json' -d '{"skipped":{"https://github.com/o/r/pull/7":"2026-11-05T10:00:00.000Z"}}' http://localhost:4477/api/auto-review/skipped
curl -fsS -X POST -H 'x-pr-inbox: 1' -H 'content-type: application/json' -d '{"unskipped":{"https://github.com/o/r/pull/7":"2026-11-05T11:00:00.000Z"}}' http://localhost:4477/api/auto-review/skipped
```

The GET only reports status: the settings, the last check's message and error, the usage reading, the PRs that wait and why, and the automatic start in progress. The settings POST changes only the fields it sends, and `"enabled":true` after off counts requests from that moment. The skipped POST adds Skip and Unskip choices with their UTC times. A choice older than the one the server holds for that PR changes nothing. The page adds `"complete":true` when it sends its whole map.

Reviews use the signed-in Claude CLI and count against that account's usage, which is what the gate watches.
