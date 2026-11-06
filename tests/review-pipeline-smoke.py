#!/usr/bin/env python3
"""Run the real local server against fake GitHub/Claude and a temporary Git remote.

No account, external API, model call, installed app, or scheduled batch is used.
Build the debug binary first: cargo build && python3 tests/review-pipeline-smoke.py
"""
import datetime
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
BINARY = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target")) / "debug/pr-inbox"
PYTHON = shutil.which("python3")

FAKE = r'''
import json, os, pathlib, sys
root = pathlib.Path(os.environ["REVIEW_SMOKE"])
state = json.loads((root / "fixture.json").read_text())
args = sys.argv[1:]
def emit(value): print(json.dumps(value))
if pathlib.Path(sys.argv[0]).name == "claude":
    if "--json-schema" in args:
        assert "--restricted" in args and "--safe-mode" in args and "--strict-mcp-config" in args
        assert args[args.index("--tools") + 1] == "Read,Glob,Grep"
        assert args[args.index("--permission-mode") + 1] == "dontAsk"
        scope = json.loads(pathlib.Path("scope.json").read_text())
        assert pathlib.Path("head/hidden.txt").read_text() == "must be reviewed\n"
        assert pathlib.Path("head/subst.txt").read_text() == "$Format:%H$\n"
        assert pathlib.Path("target/code.txt").is_file()
        assert not pathlib.Path("objects.git").exists()
        result = dict(scope, maintainabilityComplete=True, correctnessComplete=True,
                      verdict="pass", summary="Both passes found no blockers", findings=[])
        if state["scenario"] == "stale":
            state["head"] = "c" * 40
            (root / "fixture.json").write_text(json.dumps(state))
        if state["scenario"] == "incomplete": result["correctnessComplete"] = False
        if state["scenario"] == "rereview":
            previous, since = pathlib.Path("previous-review.md"), pathlib.Path("since-previous-review.patch")
            seen = json.loads((root / "seen.json").read_text()) if (root / "seen.json").exists() else []
            seen.append({"previous":previous.read_text() if previous.exists() else None, "since":since.read_text() if since.exists() else None})
            (root / "seen.json").write_text(json.dumps(seen))
            result["notes"] = ["code.txt:1 could name the change"]
        emit({"type":"system","subtype":"init","cwd":os.getcwd(),"apiKeySource":"none"})
        emit({"type":"result","is_error":False,"result":"Review complete","structured_output":result})
    else:
        emit({"type":"system","subtype":"init","cwd":os.getcwd(),"apiKeySource":"none"})
        emit({"type":"result","is_error":False,"result":"Repair checked; branch already clean"})
    sys.exit(0)
if args[:2] == ["api", "graphql"]:
    mine = state["scenario"] == "repair"
    node = {"number":7,"title":"Test review","url":"https://github.com/o/r/pull/7","state":"OPEN",
        "isDraft":False,"isCrossRepository":False,"headRefName":"feat","headRefOid":state["head"],
        "baseRefName":"main","baseRefOid":state["base"],"author":{"login":"me" if mine else "ana"},
        "repository":{"nameWithOwner":"o/r","name":"r","isArchived":False},
        "reviewRequests":{"nodes":[{"requestedReviewer":{"login":"me"}}]},
        "reviewThreads":{"nodes":[]},"labels":{"nodes":[]},"timelineItems":{"nodes":[]},
        "commits":{"nodes":[{"commit":{"statusCheckRollup":{"state":"SUCCESS","contexts":{"nodes":[]}}}}]}}
    emit({"data":{"viewer":{"login":"me","pullRequests":{"nodes":[node] if mine else []}},
        "repository":{"pullRequest":node},"reviews":{"nodes":[] if mine else [node]},
        "assigned":{"nodes":[]},"reviewed":{"nodes":[]}}})
elif args[:2] == ["pr", "view"]:
    reviews = [{"author":{"login":r["user"]["login"]},"body":r["body"],"state":r["state"]} for r in json.loads((root / "posted.json").read_text())]
    emit({"title":"Test review","body":"Check the implementation","reviews":reviews,"comments":[]})
elif args[:2] == ["pr", "checks"]:
    pending = state.get("pending", False)
    emit([{"name":"test","state":"IN_PROGRESS" if pending else "SUCCESS"}])
    sys.exit(8 if pending else 0)
elif args and args[0] == "api" and "--method" in args:
    payload = json.load(sys.stdin)
    if state.get("reject", False):
        print('HTTP/2.0 403 Forbidden\n\n{"message":"permission denied"}')
        sys.exit(1)
    assert payload["commit_id"] == state["head"]
    records = json.loads((root / "posted.json").read_text())
    records.append({"id":len(records)+1,"html_url":"https://github.com/o/r/pull/7#pullrequestreview-1",
        "commit_id":payload["commit_id"],"state":"APPROVED" if payload["event"] == "APPROVE" else "CHANGES_REQUESTED",
        "body":payload["body"],"user":{"login":"me"}})
    (root / "posted.json").write_text(json.dumps(records))
    if state["scenario"] == "ambiguous":
        print("connection lost after submission", file=sys.stderr)
        sys.exit(1)
    print("HTTP/2.0 200 OK\n\n", end="")
    emit(records[-1])
elif args and args[0] == "api" and any("/comments" in arg for arg in args): emit([[]])
elif args and args[0] == "api" and any("/reviews" in arg for arg in args):
    emit([json.loads((root / "posted.json").read_text())])
else:
    print("Unexpected fake gh request: " + repr(args), file=sys.stderr)
    sys.exit(2)
'''


def git(directory, *args, env=None):
    return subprocess.check_output(["git", "-c", "user.name=test", "-c", "user.email=test@example.invalid",
        "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null", "-C", str(directory), *args],
        env=env, stderr=subprocess.DEVNULL, text=True).strip()


def case(scenario):
    with tempfile.TemporaryDirectory(prefix="pr-inbox-pipeline-") as directory:
        root = Path(directory)
        (root / "bin").mkdir()
        (root / "data").mkdir()
        (root / "checkouts").mkdir()
        origin = root / "o/r.git"
        origin.mkdir(parents=True)
        git(origin, "init", "--bare", "--quiet")
        git(origin, "symbolic-ref", "HEAD", "refs/heads/main")
        repo = root / "seed"
        repo.mkdir()
        git(repo, "init", "-b", "main", "--quiet")
        (repo / "code.txt").write_text("original\n")
        (repo / ".gitattributes").write_text("hidden.txt export-ignore\nsubst.txt export-subst\n")
        (repo / "hidden.txt").write_text("must be reviewed\n")
        (repo / "subst.txt").write_text("$Format:%H$\n")
        git(repo, "add", ".")
        git(repo, "commit", "-qm", "base")
        base = git(repo, "rev-parse", "HEAD")
        git(repo, "checkout", "-qb", "feat")
        (repo / "code.txt").write_text("reviewed change\n")
        git(repo, "commit", "-qam", "feature")
        head = git(repo, "rev-parse", "HEAD")
        git(repo, "push", "--quiet", str(origin), "main", "feat")
        config = root / "gitconfig"
        config.write_text(f'[url "{origin}"]\n\tinsteadOf = https://github.com/o/r.git\n')
        env = dict(os.environ, REVIEW_SMOKE=str(root), PR_INBOX_DATA=str(root / "data"),
            PR_INBOX_CHECKOUTS=str(root / "checkouts"), PATH=str(root / "bin") + ":" + os.environ["PATH"],
            GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM="1")
        git(root, "clone", "--quiet", "https://github.com/o/r.git", str(root / "checkouts/r"), env=env)
        for name in ["gh", "claude"]:
            command = root / "bin" / name
            command.write_text(f"#!{PYTHON}\n" + FAKE)
            command.chmod(0o755)
        # Suppress notifications and prevent the daily summary from creating an unrelated run.
        (root / "bin/osascript").write_text("#!/bin/sh\nexit 0\n")
        (root / "bin/osascript").chmod(0o755)
        (root / "data/state.json").write_text(json.dumps({"lastDigest":datetime.date.today().isoformat()}))
        fixture = dict(scenario=scenario, head=head, base=base, pending=scenario == "pending", reject=scenario == "rejected")
        (root / "fixture.json").write_text(json.dumps(fixture))
        (root / "posted.json").write_text("[]")
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        env["PR_INBOX_PORT"] = str(port)
        log = (root / "server.log").open("w")
        process = subprocess.Popen([str(BINARY), "--server"], env=env, stdout=log, stderr=log)

        def request(path, body=None):
            data = None if body is None else json.dumps(body).encode()
            req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data,
                headers={"x-pr-inbox":"1", "Content-Type":"application/json"})
            with urllib.request.urlopen(req, timeout=5) as response:
                return json.load(response)

        def settled(run_id):
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                run = request("/api/runs/" + run_id)
                if run["status"] not in ["queued", "running"]: return run
                time.sleep(0.1)
            raise AssertionError("Review did not settle: " + json.dumps(run))

        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    request("/api/runs")
                    break
                except (urllib.error.URLError, TimeoutError):
                    if time.monotonic() > deadline: raise
                    time.sleep(0.1)
            body = {"kind":"thermo" if scenario == "repair" else "approve", "url":"https://github.com/o/r/pull/7"}
            run = settled(request("/api/runs", body)["id"])
            expected = "failed" if scenario in ["stale", "incomplete", "pending", "ambiguous", "rejected"] else "done"
            assert run["status"] == expected, (scenario, run)
            posted = json.loads((root / "posted.json").read_text())
            assert len(posted) == (1 if scenario in ["success", "ambiguous", "superseded", "rereview"] else 0), (scenario, posted)
            if scenario in ["pending", "ambiguous", "rejected"]:
                fixture["pending"] = False
                fixture["reject"] = False
                (root / "fixture.json").write_text(json.dumps(fixture))
                request("/api/runs/" + run["id"] + "/retry", {})
                run = settled(run["id"])
                assert run["status"] == "done", run
                assert len(json.loads((root / "posted.json").read_text())) == 1
            if scenario == "superseded":
                records = json.loads((root / "posted.json").read_text())
                records.append(dict(records[-1], id=2, state="CHANGES_REQUESTED", body="New finding"))
                (root / "posted.json").write_text(json.dumps(records))
                again = settled(request("/api/runs", body)["id"])
                assert again["status"] == "done", again
                records = json.loads((root / "posted.json").read_text())
                assert len(records) == 3 and records[-1]["state"] == "APPROVED", records
            if scenario == "rereview":
                (repo / "code.txt").write_text("fixed after review\n")
                git(repo, "commit", "-qam", "address review")
                pushed = git(repo, "rev-parse", "HEAD")
                git(repo, "push", "--quiet", str(origin), "feat")
                fixture["head"] = pushed
                (root / "fixture.json").write_text(json.dumps(fixture))
                again = settled(request("/api/runs", body)["id"])
                assert again["status"] == "done", again
                first, second = json.loads((root / "seen.json").read_text())
                assert first == {"previous":None, "since":None}, first
                assert second["previous"].startswith(f"Reviewed head: {head}") and "Both passes found no blockers" in second["previous"], second
                assert "-reviewed change" in second["since"] and "+fixed after review" in second["since"], second
                records = json.loads((root / "posted.json").read_text())
                assert records[-1]["commit_id"] == pushed and "Non-blocking:\n- code.txt:1 could name the change" in records[-1]["body"], records
                head = pushed
            if scenario == "success":
                again = settled(request("/api/runs", body)["id"])
                assert again["status"] == "done", again
                assert len(json.loads((root / "posted.json").read_text())) == 1
            if run["status"] == "done":
                ledger = json.loads((root / "data/reviews.json").read_text())
                assert any(r.get("scope", {}).get("headSha") == head for r in ledger.values()), ledger
            else:
                assert not (root / "data/reviews.json").exists()
            print(f"PASS {scenario}", flush=True)
        except Exception:
            print((root / "server.log").read_text())
            raise
        finally:
            process.terminate()
            process.wait(timeout=10)
            log.close()


if __name__ == "__main__":
    for scenario in ["success", "repair", "stale", "incomplete", "pending", "ambiguous", "rejected", "superseded", "rereview"]:
        case(scenario)
