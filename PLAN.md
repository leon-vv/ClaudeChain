# ClaudeChain — design sketch & plan

> Implemented. Differences from this plan: the frontend is plain JavaScript
> with server-sent events (no htmx/askama); the data directory defaults to
> `<repo>/.claudechain` and the repo is given with `--repo` instead of in
> `pipeline.toml`; notes are kept in `item.json` and included in the next run's
> prompt and `input.json` (no `notes/` folder). See README.md for usage.

A local Rust web app that pulls GitHub issues for **one target repository**
through a configurable pipeline of agents. Each step has its own model, prompt
and worker limit. Agents talk to each other only through files in the work
item's folder. Agents are run with the `claude` CLI in print mode — no Python
or Node dependency.

## 1. UI sketch

### Pipeline matrix

```
┌ ClaudeChain ── repo: ~/Development/foo (leon-vv/foo) ──────────── [+ Add issue] ┐
│ General prompt  [edit]   (shared preamble: file protocol, repo rules, ...)       │
├────────────┬──────────┬───────────┬────────────┬────────────┬──────────┬──────────┤
│            │ Triage   │ Breakdown │ Implement  │ Build/Test │ Review   │ Pull req │
│            │ haiku  ▾ │ opus    ▾ │ sonnet   ▾ │ sonnet   ▾ │ opus   ▾ │ haiku  ▾ │
│            │ workers 2│ workers 2 │ workers 3  │ workers 1  │ workers 2│ workers 1│
│            │ prompt ✎ │ prompt ✎  │ prompt ✎   │ prompt ✎   │ prompt ✎ │ prompt ✎ │
│            │          │           │            │            │          │☑ approval│
│            │ 1/2 busy │ 0/2       │ 1/3        │ 1/1        │ 0/2      │ 0/1      │
├────────────┼──────────┼───────────┼────────────┼────────────┼──────────┼──────────┤
│ #123       │    ✓     │    ✓      │   ✓ ×2     │ ● running  │          │          │
│ Fix login  │          │           │   ▲────────┤ ✗ failed   │          │          │
│ (iter 3)   │          │           │            │  "tests…"  │          │          │
├────────────┼──────────┼───────────┼────────────┼────────────┼──────────┼──────────┤
│ #127       │    ✓     │ ● running │            │            │          │          │
├────────────┼──────────┼───────────┼────────────┼────────────┼──────────┼──────────┤
│ #130       │ ⏳ queued│           │            │            │          │          │
├────────────┼──────────┼───────────┼────────────┼────────────┼──────────┼──────────┤
│ #121       │    ✓     │    ✓      │    ✓       │    ✓       │    ✓     │⏸ approve?│
│ New CLI    │          │           │            │            │          │[✓] [↩ ▾] │
├────────────┼──────────┼───────────┼────────────┼────────────┼──────────┼──────────┤
│ #118       │    ✓     │    ✓      │    ✓       │    ✓       │    ✓     │ ✓ PR #45 │
└────────────┴──────────┴───────────┴────────────┴────────────┴──────────┴──────────┘
```

Cell states: empty · ⏳ queued · ⏸ awaiting approval · ● running · ✓ done ·
✗ failed/bounced back · ⚠ needs human (agent gave no valid transition, or loop
limit hit).

**Approval toggle:** every column header has an "approval" checkbox (shown
ticked for Pull request above; off by default for the other steps). When it's
on, an item that reaches that step waits as ⏸ instead of being queued. The
cell offers **Approve** (queue the step) or **Send back ▾** (pick an earlier
step, with an optional note for that agent). Toggling it off releases any
waiting items. It's saved as `require_approval` in `pipeline.toml`.
A row can show a step more than once (`×2`) when work bounced back.

"Add issue" accepts a GitHub URL or `#123` (fetched with `gh issue view`), or
raw pasted issue text as a fallback.

### Run inspector (click a cell)

```
┌ #123 · Build/Test · run 2 of 2 ◂ ▸ ── sonnet · 3m12s · $0.41 ── [Stop] [Retry] [Move ▾] ┐
│ [ Input ] [ Transcript ] [ Output ] [ Files ]                                           │
├─────────────────────────────────────────────────────────────────────────────────────────┤
│ Transcript (live)                                                                       │
│  ▸ system   init · model claude-sonnet-5-5 · cwd worktrees/issue-123                    │
│  ▸ user     (initial prompt — expand to view)                                           │
│  ▸ claude   I'll build and run the tests first.                                         │
│  ▸ tool     Bash: cargo test            ▾ output (2 failed: auth::tests::expiry …)      │
│  ▸ claude   Two tests fail; the expiry check uses seconds instead of ms.                │
│  ▸ you      "Ignore the flaky network test, it's known."          ← your message        │
│  ▸ claude   Understood, only the expiry failure counts.                                 │
│  ▸ tool     Write: messages/004-build_test.md                                           │
│  ▸ result   transition → implementation  "expiry test fails (ms vs s)"                  │
├─────────────────────────────────────────────────────────────────────────────────────────┤
│ Message to agent: [ ...............................................  ] [Send]           │
│   running  → delivered into the live session                                            │
│   finished → [Continue this run] (resumes the session) or [Note for next run]           │
└─────────────────────────────────────────────────────────────────────────────────────────┘
```

Tabs:
- **Input** — exactly what was sent: system prompt (general + step prompt),
  the initial user prompt with item context, model, allowed tools, full CLI
  command line.
- **Transcript** — every event from the run, live: assistant text, tool calls
  with collapsible inputs and outputs, your messages, final result, cost.
- **Output** — the message file this run wrote and its `transition.json`.
- **Files** — browse the item folder (issue, all message files, notes) and the
  worktree's `git diff`.

Arrows ◂ ▸ step through earlier runs of the same cell (e.g. the first
Implementation attempt before it bounced).

## 2. Giving an agent direction

Three ways, all of them recorded in the transcript and in the item folder:

1. **While running** — the agent is started with
   `--input-format stream-json`, so the UI's message goes straight into the
   agent's stdin as a new user turn. The agent sees it on its next turn.
2. **After it finished** — "Continue this run" starts
   `claude -p --resume <session_id>` with your message, so the agent keeps its
   full context, can redo work, and writes a new `transition.json`.
3. **Note for next run** — saved to `notes/` in the item folder; the next agent
   run (any step) gets the notes in its prompt. Useful for items that are
   queued or ⚠ waiting for a human.

## 3. File-based protocol

```
.claudechain/
  pipeline.toml                 # steps, models, prompts, workers, allowed moves
  prompts/general.md, prompts/<step>.md
  items/issue-123/
    item.json                   # orchestrator state (current step, history, iteration)
    issue.md                    # the original issue
    messages/
      001-triage.md             # one file per agent run, append-only history
      002-breakdown.md
      003-implementation.md
      004-build_test.md
    notes/                      # human notes for upcoming runs, one file each
    runs/004-build_test/
      input.json                # system prompt, prompt, model, tools, argv
      events.jsonl              # raw stream-json from the CLI (+ your messages)
      meta.json                 # session id, start/end, exit code, cost
      transition.json           # written by the agent at the end of this run
  worktrees/issue-123/          # git worktree on branch `claudechain/issue-123`
```

Every agent run gets: **general prompt + step prompt + item context** (issue id,
paths above, list of earlier message files, notes, and the exact paths of *its
own* message file and transition file). It must finish by writing:

```json
// runs/004-build_test/transition.json
{ "to": "implementation", "summary": "2 tests fail in auth::expiry" }
```

`to` is `"next"`, a step id from that step's `allowed_targets`, or `"human"`.
The orchestrator validates it; if it's missing or invalid, the item is flagged
⚠ needs human. A per-item `max_iterations` stops endless implement ↔ test loops.

**No concurrent writes.** Only one agent works on an item at a time: an item is
always in exactly one step, so agents on different items write to different
folders. Inside an item, every run gets its own numbered message file and its
own `runs/NNN-step/transition.json`, so nothing is overwritten and a leftover
transition from an earlier run can't be mistaken for the current one. Notes
from the UI also go into separate files. The only shared state, `item.json`,
is written only by the orchestrator (write to a temp file, then rename).

```toml
# pipeline.toml (sketch)
repo = "/home/leonvv/Development/foo"
general_prompt = "prompts/general.md"
max_iterations = 6

[[step]]
id = "pull_request"
name = "Pull request"
model = "claude-haiku-5-5"
require_approval = true          # toggled from the column header
# ...

[[step]]
id = "build_test"
name = "Build/Test"
model = "claude-sonnet-5-5"
prompt = "prompts/build_test.md"
max_workers = 1
allowed_targets = ["next", "implementation", "human"]
allowed_tools = ["Read", "Grep", "Glob", "Bash", "Write"]
```

## 4. Architecture

```
Browser (htmx + SSE)
   │  HTTP / server-sent events
Rust (axum)
   ├── web: matrix, run inspector, message box, prompt/model/worker forms
   ├── orchestrator: scheduler loop, one tokio Semaphore per step (resizable),
   │                 reads transition.json, updates item.json, emits events
   ├── store: item/pipeline files on disk (no DB — the files *are* the state)
   ├── git: create/remove a worktree per item
   ├── github: shells out to `gh` (issue fetch, PR creation)
   └── runner: one `claude` child process per run
          claude -p --input-format stream-json --output-format stream-json
                 --verbose --model <m> --append-system-prompt <general+step>
                 --allowedTools <...> --permission-mode <...>
          cwd = worktrees/issue-N, --add-dir items/issue-N
          stdin  ← initial prompt, then any messages from the UI
          stdout → events.jsonl + broadcast to the UI
          stdin is closed after the `result` event (→ process exits)
```

Checked on this laptop: `claude` 2.1.322 with the subscription login works in
stream-json input and output mode (a test call returned a session id and a result).

## 5. Dependencies

Rust:
| crate | why |
|---|---|
| `tokio` | async runtime, process spawning, semaphores |
| `axum` | HTTP server + SSE |
| `tower-http` | static file serving |
| `askama` | compile-time checked HTML templates |
| `serde`, `serde_json`, `toml` | item/pipeline/transition/event files |
| `tokio-stream` | SSE event streams |
| `anyhow`, `thiserror` | errors |
| `tracing`, `tracing-subscriber` | logging |
| `chrono` | timestamps in history |

Frontend: `htmx` (+ SSE extension), vendored as a static JS file — no npm build.

External tools: `claude` (agents), `gh` (GitHub), `git` (worktrees).

## 6. Implementation phases

1. **Skeleton** — cargo project, `pipeline.toml` + default prompts, item model,
   on-disk layout, add an item from a pasted issue.
2. **Runner** — spawn `claude -p` with stream-json in and out, record input,
   events and meta; send extra messages over stdin; resume a finished session.
3. **Orchestrator** — scheduler with per-step worker limits, transitions
   (forward/back), iteration limit, recovery after restart (running → queued).
4. **Web UI** — matrix view with live updates over SSE, run inspector
   (Input / Transcript / Output / Files), message box, prompt/model/worker editing.
5. **Git isolation** — one git worktree per item; the PR step pushes the branch
   and uses `gh pr create`.
6. **Controls & polish** — stop/retry/manual move, cost per item and step,
   per-step tool restrictions.

## 7. Possible later extensions

- Running parts of one item in parallel (e.g. sub-tasks from the breakdown).
  That would need sub-items with their own folders and worktrees; it's out of
  scope for now.
