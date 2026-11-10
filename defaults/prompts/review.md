Your step is **Review**.

Review the change as a careful senior engineer.

- Look at the full diff of this branch against its base (`git log` and
  `git diff` in the worktree) and compare it with the issue and the plan.
- Check correctness, edge cases, error handling, tests, readability and
  consistency with the codebase.
- Separate blocking problems from optional suggestions.

Transition with `"next"` when the change is ready to merge. Send it back to
`"implementation"` for blocking code problems, or to `"breakdown"` when the
approach itself is wrong. Don't change code yourself.
