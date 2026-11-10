# ClaudeChain

A local web app that runs GitHub issues through a pipeline of Claude Code
agents: Triage → Work breakdown → Implementation → Build & test → Review →
Pull request. Each step has its own prompt, model and worker limit. Agents talk
to each other only through files, and each one decides where the work goes next
(forward, back to an earlier step, or to you).

## Requirements

- Rust (to build)
- [Claude Code](https://claude.com/claude-code) `claude` CLI, logged in. With a
  subscription login, runs count against your plan; the costs shown in the UI
  are API-equivalent estimates. ClaudeChain removes `ANTHROPIC_*` environment
  variables when starting agents so they always use the CLI login.
- `git`, and `gh` (logged in) for fetching issues and opening pull requests.

## Running

```bash
cargo run --release -- --repo /path/to/your/repo
```

Then open http://127.0.0.1:7878. Options: `--port`, `--host`, `--data <dir>`
(default `<repo>/.claudechain`, which is added to the repo's `.git/info/exclude`),
and `--claude <path>`.

## Using it

- **+ Add issue**: paste a GitHub issue URL, `#123`, or plain issue text.
- **Column headers**: pick the model, the maximum number of agents running that
  step at once (0 pauses the step), whether items need your approval before the
  step runs, and edit the step prompt. **General prompt** is shared by all steps.
- **Cells** show every run of that step as a chip (✓ forward, ↩ sent back,
  ✗ failed or stopped). The current step has a blue edge and shows the status.
- **Click a cell or chip** to open the run inspector:
  - *Transcript*: everything the agent did, live, including tool calls and output.
  - *Input*: the exact system prompt, prompt, tools and command line.
  - *Output*: the message file and `transition.json` the agent wrote.
  - *Files*: everything in the item folder. *Diff*: the item branch's changes.
- **Giving direction**:
  - *Send to agent* (while running): delivered into the live session.
  - *Continue this run* (after it finished): resumes the session with your message.
  - *Note for next run*: added to the prompt of the item's next run.
- **Stop**, **Retry step**, **Move…** (to any step or done), **Remove**
  (archives the item folder and removes the worktree; the branch is kept).

## How it works

```
<repo>/.claudechain/
  pipeline.toml              steps, models, workers, tools, allowed transitions
  prompts/general.md         shared prompt; prompts/<step>.md per step
  items/<id>/
    item.json                state (written only by ClaudeChain)
    issue.md
    messages/NNN-<step>.md   one message per run, written by the agent
    runs/NNN-<step>/
      input.json             what the agent was given
      events.jsonl           raw stream-json transcript + your messages
      transition.json        {"to": "next" | "<step id>" | "human", "summary": "..."}
  worktrees/<id>/            git worktree on branch claudechain/<id>
  archive/                   removed items
```

Each run starts `claude -p` with stream-json input and output, in the item's
worktree, with the item folder added via `--add-dir`. The prompt is the general
prompt plus the step prompt (as an appended system prompt) and an item context
message with the paths, history, notes and allowed transitions. When the run
ends, ClaudeChain reads its `transition.json` and moves the item. A missing
or invalid transition, a stop, or too many backward moves (`max_iterations`)
hands the item to you (⚠ needs human).

Steps, tools, permission mode and allowed transitions are edited in
`pipeline.toml` (restart to apply). Agents run with `--permission-mode dontAsk`
and only the tools listed for their step, and don't load your own MCP servers.
If ClaudeChain stops, running agents are stopped too, and their items are
requeued on the next start.
