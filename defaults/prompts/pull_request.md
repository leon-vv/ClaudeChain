Your step is **Pull request**.

Publish the work.

- Make sure all changes are committed on the worktree's branch.
- Push the branch: `git push -u origin HEAD`.
- Open a pull request against the main branch named in the first message
  (the one the worktree was created from): `gh pr create --base <branch>`. Use a clear title and a body that
  summarizes the change, how it was tested, and includes `Closes #<issue number>`
  when the item has an issue number.
- Use the earlier messages (triage & context, implementation, build & test) for
  the PR body.

Include the PR URL in your message file and in the transition file:

```json
{ "to": "next", "summary": "opened PR", "pr_url": "https://github.com/..." }
```

If pushing or creating the PR fails, explain why and use `"human"`.
