Your step is **Fix merge conflicts**.

The human pressed "Fix conflicts" because this work item's pull request can no
longer be merged cleanly into the main branch. Bring the branch up to date and
push it again. This is not a pipeline step: you don't move the item anywhere.

1. Make sure the worktree is clean. If there are uncommitted changes, commit
   them first with a clear message.
2. Get the latest main branch (the one named in the first message, usually
   `main` or `master`):
   - `git fetch origin` (when a remote exists), then
   - update the local main branch to match the remote, e.g.
     `git fetch origin <main>:<main>` (this works without checking it out; the
     main branch is checked out in another worktree, so don't check it out here).
3. Merge it into this branch: `git merge <main>` (use `origin/<main>` if it is
   newer than the local one). Don't rebase and don't force-push.
4. Resolve every conflict. Read both sides and the earlier messages in the work
   item folder to understand what this branch changes, keep the intent of both
   sides, and remove all conflict markers. Run a quick build or check when the
   project has one (e.g. `cargo check`) to make sure the result compiles.
5. Commit the merge (`git commit --no-edit` when the default message is fine).
6. Push to the pull request's branch: `git push origin HEAD` (the branch is
   already tracked by the PR given in the first message).

In your message file, list the conflicting files and how you resolved each.

Transition file:

- `{ "to": "done", "summary": "merged main, resolved N conflicts, pushed" }`
  when the branch is merged, committed and pushed (also when there were no
  conflicts at all).
- `{ "to": "human", "summary": "..." }` when you can't resolve a conflict
  safely, the push fails, or the merge would need a decision from the human.
  Leave the worktree in a clear state (abort the merge with `git merge --abort`
  if you didn't finish it) and explain the options in the message file.
