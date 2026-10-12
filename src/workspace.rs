// Where a run that changes code works: PR Inbox's own git worktree for that PR, never your checkout and
// never another tool's worktree. It lives at <repo>/.claude/worktrees/pr-inbox-<number>, so every run on
// one PR finds the same directory and runs on different PRs never share files.
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::github::Pr;
use crate::util::{checkouts_dir, first_line, output, GIT_ENV};

pub struct Workspace {
    pub dir: PathBuf,
    /// The PR's branch on GitHub.
    pub head: String,
    /// The local branch the worktree has checked out. It differs from `head` when that branch is
    /// already checked out somewhere else; pushes then go to `HEAD:<head>`.
    pub branch: String,
    /// The worktree already existed with commits or changes of its own (an earlier run on this PR).
    pub leftover: bool,
}

const GIT_TIMEOUT: Duration = Duration::from_secs(90);

/// Runs git with the run's credential setup and a deadline, so a stalled network fails instead of hanging.
async fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let verb = args.first().copied().unwrap_or("");
    let mut c = tokio::process::Command::new("git");
    c.arg("-C").arg(dir).args(args).envs(GIT_ENV);
    let out = output(c, GIT_TIMEOUT).await;
    match out.code {
        0 => Ok(out.stdout),
        124 => Err(format!("git {verb} took longer than {}s", GIT_TIMEOUT.as_secs())),
        127 => Err(out.stderr),
        _ => Err(format!("git {verb}: {}", first_line(&out.stderr))),
    }
}

async fn clean(dir: &Path) -> bool {
    git(dir, &["status", "--porcelain"]).await.is_ok_and(|s| s.trim().is_empty())
}

/// Branches checked out in any worktree of the repo, the main checkout included.
async fn checked_out(repo: &Path) -> Vec<String> {
    git(repo, &["worktree", "list", "--porcelain"])
        .await
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("branch refs/heads/").map(String::from))
        .collect()
}

/// The local branch a PR's worktree uses when the PR's own branch is checked out somewhere else.
pub fn own_branch(number: u64) -> String {
    format!("pr-inbox/{number}")
}

/// The directory a PR's runs use; it exists once a writing run has started on that PR.
fn dir_for(repo: &Path, number: u64) -> PathBuf {
    repo.join(".claude/worktrees").join(format!("pr-inbox-{number}"))
}

/// The PR number in a worktree directory's name, only when the name is exactly the one `dir_for` gives.
fn pr_number(name: &str) -> Option<u64> {
    name.strip_prefix("pr-inbox-").and_then(|n| n.parse().ok()).filter(|n| format!("pr-inbox-{n}") == name)
}

/// An origin URL as it is compared: trimmed, without `.git`, lowercase.
fn normalized(origin: &str) -> String {
    origin.trim().trim_end_matches(".git").to_lowercase()
}

/// The origin URL points at `repo` (owner/name), over https or ssh.
fn origin_matches(origin: &str, repo: &str) -> bool {
    let (origin, repo) = (normalized(origin), repo.to_lowercase());
    origin.ends_with(&format!("/{repo}")) || origin.ends_with(&format!(":{repo}"))
}

/// Refuses a local clone whose origin is another repository than the PR's.
async fn check_origin(repo: &Path, pr: &Pr) -> Result<(), String> {
    let origin = git(repo, &["remote", "get-url", "origin"]).await?;
    if origin_matches(&origin, &pr.repo) {
        Ok(())
    } else {
        Err(format!("{} has origin {}, not {}.", repo.display(), normalized(&origin), pr.repo))
    }
}

/// The worktree as git has it now. It counts as leftover until `for_pr` has caught it up with the PR.
async fn describe(dir: &Path, head: &str) -> Result<Workspace, String> {
    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"]).await?.trim().to_string();
    Ok(Workspace { dir: dir.to_path_buf(), head: head.into(), branch, leftover: true })
}

/// Creates the PR's worktree on the PR's branch, or on `own_branch` when that branch is checked out in
/// your checkout or another worktree. A local branch that already exists is reused as it is.
async fn add(repo: &Path, pr: &Pr, dir: &Path, remote: &str) -> Result<(), String> {
    let _ = git(repo, &["worktree", "prune"]).await;
    let head = pr.head.as_str();
    let elsewhere = checked_out(repo).await.iter().any(|b| b == head);
    let branch = if elsewhere { own_branch(pr.number) } else { head.to_string() };
    let path = dir.to_string_lossy();
    if git(repo, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).await.is_ok() {
        git(repo, &["worktree", "add", "--quiet", "--", &path, &branch]).await?;
    } else {
        git(repo, &["worktree", "add", "--quiet", "--track", "-b", &branch, "--", &path, remote]).await?;
    }
    Ok(())
}

/// The PR's worktree, created when missing and caught up with the PR branch when it holds nothing of its own.
pub async fn for_pr(repo: &Path, pr: &Pr) -> Result<Workspace, String> {
    check_origin(repo, pr).await?;
    let head = pr.head.as_str();
    let remote = format!("origin/{head}");
    git(repo, &["fetch", "--quiet", "origin", "--", &format!("+refs/heads/{head}:refs/remotes/origin/{head}")]).await?;
    let dir = dir_for(repo, pr.number);
    if !dir.join(".git").exists() {
        add(repo, pr, &dir, &remote).await?;
    }
    let mut ws = describe(&dir, head).await?;
    // Clean and nothing of its own: catch up with the PR branch, by fast-forward only. A branch that was
    // force-pushed since stays leftover, and the prompt tells the run to compare it with origin.
    if clean(&dir).await && git(&dir, &["merge-base", "--is-ancestor", "HEAD", &remote]).await.is_ok() {
        git(&dir, &["reset", "--quiet", "--hard", &remote]).await?;
        ws.leftover = false;
    }
    Ok(ws)
}

/// The PR's worktree as it is, without fetching or resetting it: for a run that queues behind another
/// writing run on this PR, which may be working in it.
pub async fn current(repo: &Path, pr: &Pr) -> Result<Workspace, String> {
    check_origin(repo, pr).await?;
    let dir = dir_for(repo, pr.number);
    if dir.join(".git").exists() {
        return describe(&dir, &pr.head).await;
    }
    // No worktree to describe: the queued run fails at launch, saying its directory is gone.
    Ok(Workspace { dir, head: pr.head.clone(), branch: pr.head.clone(), leftover: true })
}

/// Removes PR Inbox's worktree for a PR, if it holds nothing unsaved.
async fn remove_if_clean(repo: &Path, number: u64) -> bool {
    let dir = dir_for(repo, number);
    dir.join(".git").exists() && clean(&dir).await && git(repo, &["worktree", "remove", "--", &dir.to_string_lossy()]).await.is_ok()
}

/// Removes the clean pr-inbox-<number> worktrees of PRs on neither list (merged, closed, or no longer
/// waiting on you), skipping any a queued or running run uses. `open` holds (repo directory name, PR
/// number); no other worktree name is ever touched.
pub async fn sweep(open: HashSet<(String, u64)>, busy: HashSet<PathBuf>) {
    let Ok(repos) = std::fs::read_dir(checkouts_dir()) else { return };
    for repo in repos.flatten() {
        let repo_dir = repo.path();
        let name = repo.file_name().to_string_lossy().to_string();
        let Ok(trees) = std::fs::read_dir(repo_dir.join(".claude/worktrees")) else { continue };
        for tree in trees.flatten() {
            let Some(n) = tree.file_name().to_str().and_then(pr_number) else { continue };
            if open.contains(&(name.clone(), n)) || busy.contains(&tree.path()) {
                continue;
            }
            remove_if_clean(&repo_dir, n).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn an_origin_matches_only_its_own_repo() {
        let cases = [
            ("https://github.com/o/r", "o/r", true),
            ("https://github.com/o/r.git\n", "o/r", true),
            ("git@github.com:o/r.git", "o/r", true),
            ("https://github.com/Octo-Org/PR-Inbox.git", "octo-org/pr-inbox", true),
            ("https://github.com/xorg/repo.git", "org/repo", false),
            ("git@github.com:o/other.git", "o/r", false),
        ];
        for (origin, repo, want) in cases {
            assert_eq!(origin_matches(origin, repo), want, "{origin} {repo}");
        }
    }

    #[test]
    fn only_exact_pr_inbox_worktree_names_are_swept() {
        assert_eq!(pr_number("pr-inbox-12"), Some(12));
        for other in ["pr-inbox-012", "pr-inbox-+12", "pr-inbox-", "pr-inbox-12a", "pr-inbox-12-old", "feature-12", "my-open-prs"] {
            assert_eq!(pr_number(other), None, "{other}");
        }
    }

    /// A bare origin at <tmp>/o/r.git (so it matches repo "o/r"), your clone at <tmp>/r, and a second clone
    /// that pushes the way GitHub would receive them. The directory goes when the test ends, failed or not.
    struct Repos {
        tmp: PathBuf,
        clone: PathBuf,
        pusher: PathBuf,
    }

    impl Drop for Repos {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.tmp);
        }
    }

    fn run_git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "-C"])
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str) -> String {
        std::fs::write(dir.join(file), file).unwrap();
        run_git(dir, &["add", "--", file]);
        run_git(dir, &["commit", "--quiet", "-m", file]);
        head_of(dir)
    }

    fn head_of(dir: &Path) -> String {
        run_git(dir, &["rev-parse", "HEAD"])
    }

    impl Repos {
        fn new() -> Repos {
            let tmp = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
            let origin = tmp.join("o/r.git");
            std::fs::create_dir_all(&origin).unwrap();
            let repos = Repos { tmp: tmp.clone(), clone: tmp.join("r"), pusher: tmp.join("pusher") };
            run_git(&origin, &["init", "--quiet", "--bare"]);
            run_git(&origin, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            run_git(&tmp, &["init", "--quiet", "pusher"]);
            run_git(&repos.pusher, &["symbolic-ref", "HEAD", "refs/heads/main"]);
            run_git(&repos.pusher, &["remote", "add", "origin", &origin.to_string_lossy()]);
            commit(&repos.pusher, "readme");
            run_git(&repos.pusher, &["push", "--quiet", "origin", "main"]);
            run_git(&tmp, &["clone", "--quiet", "--", &origin.to_string_lossy(), "r"]);
            repos
        }

        /// Pushes a new commit to `branch` on origin. A new branch, or a `force` push, starts from main.
        fn push(&self, branch: &str, file: &str, force: bool) -> String {
            run_git(&self.pusher, &["fetch", "--quiet", "origin"]);
            let exists = !run_git(&self.pusher, &["branch", "--remotes", "--list", &format!("origin/{branch}")]).is_empty();
            let from = if exists && !force { format!("origin/{branch}") } else { "origin/main".into() };
            run_git(&self.pusher, &["checkout", "--quiet", "-B", branch, &from]);
            let sha = commit(&self.pusher, file);
            run_git(&self.pusher, &["push", "--quiet", "--force", "origin", branch]);
            sha
        }

        fn pr(&self, number: u64, head: &str) -> Pr {
            Pr { repo: "o/r".into(), name: "r".into(), number, head: head.into(), base: "main".into(), ..Default::default() }
        }
    }

    #[tokio::test]
    async fn a_fresh_worktree_tracks_the_pr_branch() {
        let r = Repos::new();
        let sha = r.push("feat", "a", false);
        let ws = for_pr(&r.clone, &r.pr(1, "feat")).await.unwrap();
        assert_eq!((ws.branch.as_str(), ws.leftover), ("feat", false));
        assert_eq!(ws.dir, r.clone.join(".claude/worktrees/pr-inbox-1"));
        assert_eq!(head_of(&ws.dir), sha);
        assert_eq!(run_git(&ws.dir, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feat");
    }

    #[tokio::test]
    async fn a_branch_checked_out_in_your_clone_gets_its_own_branch_and_current_reports_it() {
        let r = Repos::new();
        r.push("busy", "a", false);
        run_git(&r.clone, &["fetch", "--quiet", "origin"]);
        run_git(&r.clone, &["checkout", "--quiet", "busy"]);
        let pr = r.pr(2, "busy");
        let ws = for_pr(&r.clone, &pr).await.unwrap();
        assert_eq!((ws.branch.as_str(), ws.leftover), ("pr-inbox/2", false));
        let now = current(&r.clone, &pr).await.unwrap();
        assert_eq!((now.branch.as_str(), now.head.as_str(), now.leftover), ("pr-inbox/2", "busy", true));
        assert_eq!(now.dir, ws.dir);
    }

    #[tokio::test]
    async fn a_clean_worktree_behind_origin_catches_up() {
        let r = Repos::new();
        r.push("feat", "a", false);
        let pr = r.pr(3, "feat");
        for_pr(&r.clone, &pr).await.unwrap();
        let newer = r.push("feat", "b", false);
        let ws = for_pr(&r.clone, &pr).await.unwrap();
        assert!(!ws.leftover);
        assert_eq!(head_of(&ws.dir), newer);
    }

    #[tokio::test]
    async fn an_unpushed_commit_in_the_worktree_is_kept() {
        let r = Repos::new();
        r.push("feat", "a", false);
        let pr = r.pr(4, "feat");
        let ws = for_pr(&r.clone, &pr).await.unwrap();
        let mine = commit(&ws.dir, "local");
        let ws = for_pr(&r.clone, &pr).await.unwrap();
        assert!(ws.leftover);
        assert_eq!(head_of(&ws.dir), mine);
    }

    #[tokio::test]
    async fn a_reused_local_branch_with_an_unpushed_commit_is_leftover() {
        let r = Repos::new();
        r.push("feat", "a", false);
        run_git(&r.clone, &["fetch", "--quiet", "origin"]);
        run_git(&r.clone, &["checkout", "--quiet", "feat"]);
        let mine = commit(&r.clone, "local");
        run_git(&r.clone, &["checkout", "--quiet", "main"]);
        let ws = for_pr(&r.clone, &r.pr(5, "feat")).await.unwrap();
        assert_eq!((ws.branch.as_str(), ws.leftover), ("feat", true));
        assert_eq!(head_of(&ws.dir), mine);
    }

    #[tokio::test]
    async fn a_force_pushed_branch_leaves_the_worktree_leftover() {
        let r = Repos::new();
        let old = r.push("feat", "a", false);
        let pr = r.pr(6, "feat");
        assert!(!for_pr(&r.clone, &pr).await.unwrap().leftover);
        r.push("feat", "b", true);
        let ws = for_pr(&r.clone, &pr).await.unwrap();
        assert!(ws.leftover);
        assert_eq!(head_of(&ws.dir), old);
    }
}
