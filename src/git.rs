use std::path::Path;

use anyhow::{Result, bail};
use serde::Deserialize;
use tokio::process::Command;

/// Runs a command and returns its trimmed stdout, failing with stderr on error.
async fn output(cmd: &mut Command) -> Result<String> {
    let out = cmd.output().await?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
}

fn git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    cmd
}

pub async fn check_repo(repo: &Path) -> Result<()> {
    output(git(repo).args(["rev-parse", "--show-toplevel"])).await?;
    Ok(())
}

/// Whether `name` is a local branch in `repo`.
pub async fn branch_exists(repo: &Path, name: &str) -> Result<bool> {
    let branch_ref = format!("refs/heads/{name}");
    let out = git(repo).args(["rev-parse", "--verify", "--quiet", &branch_ref]).output().await?;
    Ok(out.status.success())
}

/// Guesses the repository's main branch: the branch `origin/HEAD` points to,
/// else `main` or `master`, else whatever is checked out.
pub async fn default_branch(repo: &Path) -> Result<String> {
    if let Ok(r) = output(git(repo).args(["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"])).await
        && let Some(name) = r.strip_prefix("origin/")
        && branch_exists(repo, name).await?
    {
        return Ok(name.to_string());
    }
    for name in ["main", "master"] {
        if branch_exists(repo, name).await? {
            return Ok(name.to_string());
        }
    }
    output(git(repo).args(["symbolic-ref", "--quiet", "--short", "HEAD"]))
        .await
        .map_err(|_| anyhow::anyhow!("could not determine the main branch; enter it explicitly"))
}

/// Keeps the data directory out of `git status` in the target repo.
pub async fn exclude_path(repo: &Path, data: &Path) -> Result<()> {
    let Ok(rel) = data.strip_prefix(repo) else { return Ok(()) };
    let exclude = output(git(repo).args(["rev-parse", "--git-path", "info/exclude"])).await?;
    let exclude = repo.join(exclude);
    let line = format!("/{}/", rel.display());
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !current.lines().any(|l| l.trim() == line) {
        if let Some(dir) = exclude.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let sep = if current.is_empty() || current.ends_with('\n') { "" } else { "\n" };
        std::fs::write(&exclude, format!("{current}{sep}{line}\n"))?;
    }
    Ok(())
}

/// Creates the item's worktree on `branch` if it doesn't exist yet; a new
/// branch starts from the local branch `main` (not whatever the main checkout
/// has checked out). Returns the worktree's current HEAD (its base commit when
/// just created).
pub async fn ensure_worktree(repo: &Path, path: &Path, branch: &str, main: &str) -> Result<String> {
    if !path.join(".git").exists() {
        let mut cmd = git(repo);
        cmd.args(["worktree", "add"]);
        if branch_exists(repo, branch).await? {
            cmd.arg(path).arg(branch);
        } else {
            if !branch_exists(repo, main).await? {
                bail!("main branch '{main}' does not exist");
            }
            cmd.arg("-b").arg(branch).arg(path).arg(format!("refs/heads/{main}"));
        }
        output(&mut cmd).await?;
    }
    output(git(path).args(["rev-parse", "HEAD"])).await
}

pub async fn remove_worktree(repo: &Path, path: &Path) -> Result<()> {
    if path.exists() {
        output(git(repo).args(["worktree", "remove", "--force"]).arg(path)).await?;
    }
    Ok(())
}

/// Deletes a local branch if it exists (the remote branch is left alone).
pub async fn delete_branch(repo: &Path, branch: &str) -> Result<()> {
    if branch_exists(repo, branch).await? {
        output(git(repo).args(["branch", "-D", branch])).await?;
    }
    Ok(())
}

/// Committed and uncommitted changes in the worktree since `base`, plus untracked files.
pub async fn diff(worktree: &Path, base: Option<&str>) -> Result<String> {
    if !worktree.exists() {
        return Ok("(no worktree yet)".into());
    }
    let base = base.unwrap_or("HEAD");
    let log = output(git(worktree).args(["log", "--oneline", &format!("{base}..HEAD")])).await?;
    let status = output(git(worktree).args(["status", "--short"])).await?;
    let diff = output(git(worktree).args(["diff", "--stat", "--patch", base])).await?;
    Ok(format!(
        "# Commits since base\n{}\n\n# Working tree status\n{}\n\n# Diff against base ({base})\n{}",
        if log.is_empty() { "(none)" } else { &log },
        if status.is_empty() { "(clean)" } else { &status },
        if diff.is_empty() { "(no changes)" } else { &diff },
    ))
}

#[derive(Debug, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub url: String,
    #[serde(default)]
    pub labels: Vec<Label>,
}

#[derive(Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

/// If `input` refers to a GitHub issue (URL, `#123` or `123`), returns the
/// argument for `gh issue view`.
pub fn issue_ref(input: &str) -> Option<String> {
    let s = input.trim();
    if s.contains('\n') {
        return None;
    }
    if s.starts_with("https://github.com/") && s.contains("/issues/") {
        return Some(s.to_string());
    }
    let n = s.strip_prefix('#').unwrap_or(s);
    (!n.is_empty() && n.chars().all(|c| c.is_ascii_digit())).then(|| n.to_string())
}

pub async fn fetch_issue(repo: &Path, issue: &str) -> Result<Issue> {
    let json = output(
        Command::new("gh")
            .current_dir(repo)
            .args(["issue", "view", issue, "--json", "number,title,body,url,labels"]),
    )
    .await?;
    Ok(serde_json::from_str(&json)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(dir: &Path, args: &[&str]) {
        output(git(dir).args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args)).await.unwrap();
    }

    #[tokio::test]
    async fn worktree_starts_from_main_not_head() {
        let root = std::env::temp_dir().join(format!("claudechain-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "trunk"]).await;
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]).await;
        let trunk = output(git(&repo).args(["rev-parse", "HEAD"])).await.unwrap();
        // The main checkout is on another branch, ahead of trunk.
        run(&repo, &["checkout", "-q", "-b", "feature"]).await;
        run(&repo, &["commit", "-q", "--allow-empty", "-m", "feature"]).await;

        assert_eq!(default_branch(&repo).await.unwrap(), "feature");
        assert!(branch_exists(&repo, "trunk").await.unwrap());
        assert!(!branch_exists(&repo, "main").await.unwrap());

        let wt = root.join("wt");
        assert!(ensure_worktree(&repo, &wt, "item", "main").await.is_err());
        assert_eq!(ensure_worktree(&repo, &wt, "item", "trunk").await.unwrap(), trunk);
        let _ = std::fs::remove_dir_all(&root);
    }
}
