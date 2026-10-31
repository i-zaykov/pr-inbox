# PR Inbox

A macOS app that lists your open GitHub pull requests, plus the ones waiting for your review, sorted by what to do next. Every PR is one click away from a Claude Code session that fixes its CI, addresses review comments, rebases it or reviews it.

## What it does

PR Inbox sorts each PR into a lane: Ready to merge, Waiting on you, Fix CI, Changes requested, Update branch, Resolve conflicts, Needs approval, CI running, Drafts, and Close or revive. Each lane comes with the button for its next step.

- Merge, update a branch, mark a draft ready or close a PR without leaving the app. These go through `gh`.
- Click a PR to open it in a panel on the right: its description with screenshots, reviews and comments, labels and checks, and the same buttons as its row. J and K step through the list, Esc closes it.
- Group the list by status or by repository (press G). Each repository shows its next release and how many commits on its default branch are not released yet.
- Merge & release squash-merges the Ready PRs of a repo, then cuts one release of it. A repo with a `release.yml` workflow runs that workflow. Any other repo gets a GitHub release one patch above its latest tag (v1.6.0 becomes v1.6.1), with generated notes. A repo with no releases only merges. Each repo also names the open PRs its next release leaves out, and why.
- Tick the box in front of any of your PRs that can merge, or a heading's box to tick its whole group, then pick Merge or Merge & release from the bar on top, or from the heading of a group with ticks, which takes only that group's. The bar's own box ticks every PR on the board that can merge. Both open the same dialog first, listing what merges and which repos release. A ticked PR held back only by its approval or by being behind takes an admin merge. Esc clears the ticks.
- Admin merge uses your admin rights to merge your own PR past a missing approval or a branch behind its base, once its checks have passed. You tick it in the Merge & release dialog, which says what it skips. A repo that enforces its rules for admins still refuses.
- Press ⌘R to reload the app; the Refresh button (or R) refreshes just the PR list.
- Hand a PR to Claude: fix CI, address review comments, rebase, review, or ask anything about it. A lane with several PRs can start one run per PR at once.
- Reviews, questions and the triage run in Claude Code's plan mode and change nothing. Runs that change code use auto mode in a git worktree of their own (`<clone>/.claude/worktrees/pr-inbox-<number>`), so your checkout stays untouched. PR Inbox removes a clean worktree once its PR is merged or closed.
- Every morning at 08:00, Claude writes a summary of where to start. "Triage with Claude" writes one on demand.

## Requirements

- macOS 13 or later
- Xcode Command Line Tools: `xcode-select --install`
- Rust, from [rustup.rs](https://rustup.rs)
- The GitHub CLI, signed in: `brew install gh`, then `gh auth login`
- [Claude Code](https://code.claude.com/docs), signed in: `claude auth login`

## Install

```bash
git clone https://github.com/i-zaykov/pr-inbox.git
cd pr-inbox
./install.sh
```

`install.sh` builds the app, installs it as `~/Applications/PR Inbox.app` (Spotlight and Raycast find it there), and adds a login agent that keeps its server running. Then it opens the window. The first build compiles every dependency and takes a few minutes.

### Point it at your clones

Runs that change code need a local clone of the repo. PR Inbox looks for each repo in a folder named after it, under `~/code` by default (`~/code/my-repo` for `octo-org/my-repo`). If your clones live elsewhere, say so when you install:

```bash
PR_INBOX_CHECKOUTS=~/work ./install.sh
```

Later reinstalls keep that folder, so a plain `./install.sh` is enough after the first one.

Reviews and questions work without a clone.

### Use another port

The server listens on `localhost:4477`. To change it:

```bash
PR_INBOX_PORT=4480 ./install.sh
```

## First run

The app checks `gh` and `claude` when it opens. If either is signed out, a banner offers a sign-in button that opens Terminal with `gh auth login --web` or `claude auth login`. Finish there, then press "I've signed in".

Claude runs use the `claude` CLI on your Mac with your own login, so they count against your Claude plan. A run card shows a dollar amount only when the run used an API key.

## Update

```bash
git pull
./install.sh
```

## Uninstall

```bash
launchctl bootout gui/$(id -u)/io.github.i-zaykov.pr-inbox
rm ~/Library/LaunchAgents/io.github.i-zaykov.pr-inbox.plist
rm -rf ~/Applications/PR\ Inbox.app
```

Run history lives in `~/Library/Application Support/PR Inbox` and the server log in `~/Library/Logs/PR Inbox.log`. Delete them too if you want nothing left behind.

## Troubleshooting

- The server log is `~/Library/Logs/PR Inbox.log`. `install.sh` prints its last lines when the server does not start.
- The same page works in a browser at http://localhost:4477.
- The "Thermo review" button runs the `/thermo-nuclear-code-quality-review` Claude Code skill. Without that skill installed, the run has nothing to follow.
- PRs from forks get read-only runs, because their branch is not on the repo's origin.
