# Daily reviews

The daily T3 Code task starts a PR Inbox review batch at 08:00 local time. PR Inbox runs the thermo workflow on your own open, non-draft PRs that it can write to. The app window can close while the background server works.

Each run reviews the code, fixes findings, commits and pushes changes, and checks CI. A PR with conflicts uses the resolve-and-review workflow. The batch does not merge, release, deploy, close PRs, post reviews or change repository protections.

The batch shares two worker slots with other writing runs. It skips a current passing review with green CI and waits for CI already in progress. It attempts each PR at most once per calendar day. Failed reviews can get another attempt the following day. A persisted attempt prevents an app restart or a repeated scheduled trigger from starting the same work again.

The batch ends when its queue is complete. Otherwise, it stops starting new reviews nine hours after its actual start. A batch started at 08:00 reaches that deadline at 17:00. A later manual start gets its own nine-hour window. Pending CI can defer a PR until the deadline. Reviews already running then finish.

The separate 08:00 morning summary is a triage snapshot of current GitHub heads and CI. It does not report completed results from the batch that starts at the same time. Those results appear later under each PR's run cards. Finishing a process does not by itself mean its review passed.

## Starting and stopping

- The daily task in T3 Code starts the batch automatically. Keep T3 Code running and signed in. The scheduled trigger accepts a delayed start from 08:00 through 16:59 local time and skips starts outside that window.
- Click **Run reviews** in PR Inbox to start manually. This uses the same daily batch, so the scheduled trigger does not duplicate it.
- Click **Stop queue** to stop new dispatch. Active reviews finish. Use an individual run's Stop button to interrupt it.
- Pause the daily task in T3 Code to disable future automatic batches. Stopping today's queue does not disable tomorrow's schedule.

## Keeping the Mac available

Open the Mac in the morning and keep it connected to the internet. For a long batch, leave it plugged in with its lid open. PR Inbox prevents idle sleep while the batch is working. The display may sleep.

A wake schedule is optional. If you want the Mac to wake automatically before the daily task, run this in Terminal:

```sh
sudo pmset repeat wake MTWRFSU 07:55:00
```

This requires an administrator password and replaces any existing repeating power schedule. Check it first with `pmset -g sched`. It leaves one-off system wake events alone. To remove the repeating wake later, run `sudo pmset repeat cancel`.

No wake setting is required if you normally open the Mac in the morning. A sleeping Mac cannot start the task until it wakes. You can also use **Run reviews** while it is awake. A shutdown, logout, closed lid or lost network can interrupt the work.

## State and troubleshooting

The batch state is stored in `~/Library/Application Support/PR Inbox/overnight.json`. Each review appears under its PR alongside normal runs. The server log is `~/Library/Logs/PR Inbox.log`.

The local API uses the installed server port, normally 4477:

```sh
curl -fsS -H 'x-pr-inbox: 1' http://localhost:4477/api/overnight
curl -fsS -X POST -H 'x-pr-inbox: 1' http://localhost:4477/api/overnight/start
curl -fsS -X POST -H 'x-pr-inbox: 1' http://localhost:4477/api/overnight/stop
```

The GET only reports status. The two POST requests start or stop dispatch. A repeated start returns the existing batch for that calendar day.

Reviews use the signed-in Claude CLI and count against that account's usage. Repository hooks and CI still apply. If a fix cannot be completed, its run card reports the blocker. Check the latest review verdict and CI before treating a PR as ready.
