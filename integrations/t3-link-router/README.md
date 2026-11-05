# T3 Code PR links

This macOS helper sends GitHub pull request links from T3 Code to PR Inbox. It uses the Apple event's sender process to identify T3 Code. Unknown senders and every other URL go to your previous browser.

The helper becomes the system HTTP and HTTPS handler. It saves each protocol's previous browser before enabling, then forwards other links directly to that app. It runs without a window or Dock icon. T3 Code updates do not replace it.

Do not enable this helper when a managed browser policy requires Chrome or another browser to be the default. Chrome's `DefaultBrowserSettingEnabled=true` policy repeatedly tries to replace the router and produces macOS confirmation dialogs. The helper now refuses to enable under that policy. Keep the managed browser as the system default; ordinary T3 PR links then open there. PR Inbox's own UI and `pr-inbox://open?url=...` links still work.

Install the current PR Inbox first, then run:

```sh
zsh integrations/t3-link-router/install.sh --enable
```

In T3 Code settings under Diffs & pull requests, turn off **Open pull requests in T3 Code** and set browser links to the system browser. Clicking a PR URL then opens that PR in PR Inbox. The PR Inbox window supports both an already running app and a cold launch.

Restore your previous browser routing:

```sh
"$HOME/Applications/PR Inbox Link Router.app/Contents/MacOS/pr-inbox-link-router" --restore
```

Then turn T3 Code's PR link setting back on if you want its built-in viewer. The restore command does not overwrite browser defaults you changed after installation.

If macOS refuses the restore command, choose your browser in **System Settings > Desktop & Dock > Default web browser**. Confirm both handlers with the helper's `--status` command.

Configuration lives in `~/Library/Application Support/PR Inbox Link Router/browsers.json`. `last-route.json` records only the latest source app and destination app, without the URL. `--status` prints the current HTTP and HTTPS handlers; `--self-test` checks routing without opening apps.
