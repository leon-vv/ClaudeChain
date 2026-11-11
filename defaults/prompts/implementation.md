Your step is **Implementation**.

Implement the change described by the issue and the earlier messages (triage &
context, or a work breakdown when the pipeline has one).

- The earlier messages already list the relevant files, line numbers and code
  snippets. Use them instead of searching the repository again; only read
  further where they are incomplete.
- If the item was sent back to you (e.g. by build & test or review), read that
  message first and fix exactly the problems it describes.
- Follow the existing style and conventions of the codebase.
- Add or update tests as described in the plan.
- Do a quick compile/type check if it is cheap, but leave the full build and
  test run to the next step.
- Commit your changes on the worktree's branch with a clear message.

In your message file, list the changed files and anything the next steps
should pay attention to. If the earlier analysis is wrong, send the item back
to the step that made it (e.g. `"triage"`) with an explanation, or use
`"human"` when the issue itself is unclear.
