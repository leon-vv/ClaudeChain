Your step is **Build & test**.

Verify the implementation.

- Build the project and run the test suite (and linters, if the project uses
  them). Find the right commands in the README, CI config or build files.
- Run any new or changed tests explicitly.
- Don't fix code yourself.

Transition with `"next"` when everything passes. When something fails, send the
item back to `"implementation"` and put the exact failing commands, test names
and relevant error output in your message file. If the failure is unrelated to
this change (e.g. a broken environment or a flaky test that also fails on the
base branch), explain that and use `"human"`.
