# ClaudeChain

ClaudeChain is a local web application that runs software tasks through a
configurable pipeline of coding agents. Each step of the pipeline is a separate
agent run with its own prompt, model, thinking level, tools and worker limit.
Agents communicate only through files, and each agent decides where the work
goes next: forward, back to an earlier step, or to a human for review.

Agents run on [Claude Code](https://claude.com/claude-code) or the
[Codex CLI](https://github.com/openai/codex), chosen per step by model, so a
single pipeline can mix Claude and OpenAI models.

![ClaudeChain board](docs/screenshot.png)

## Why a pipeline?

A single long agent session that plans, implements, tests and reviews a change
carries its entire history into every request. ClaudeChain splits that work
into short, focused runs, which lowers cost and keeps quality predictable:

- **Right-sized models.** Each step picks its own model and thinking level.
  Routine work such as triage, running tests or opening a pull request can use
  a small, fast model, while the expensive model is reserved for the steps that
  need it.
- **Short contexts.** Every step starts a fresh session that sees only the task
  and the messages earlier steps wrote for it, not their full transcripts.
  Requests stay small, and the model is not distracted by stale context.
- **Better prompt caching.** All runs of a step share the same general prompt,
  step prompt and tool set, so the start of each request is identical across
  tasks and is served from the prompt cache.
- **Minimal tool surface.** Steps receive only the tools they need, and your
  personal MCP servers are not loaded, which keeps tool definitions out of the
  context and limits what each agent can do.
- **Bounded loops.** Steps may send work back for another iteration, but a
  per-pipeline `max_iterations` limit hands the task to a human instead of
  letting it loop indefinitely. An auto-pause threshold stops new runs before
  your plan limits are reached.

## Features

- **Parallel by design.** Many tasks run concurrently, each in its own git
  worktree. Several repositories can be open at once, and each repository can
  have several pipelines, all running at the same time and sharing one account
  and its usage limits.
- **Any task source.** Add a GitHub issue (URL or `#123`) or describe a task in
  plain text.
- **Pipeline editor.** Add, reorder and configure steps from presets or from
  scratch, or generate a pipeline from a description. Pipelines can be exported
  to and imported from a single TOML file.
- **Full visibility.** Inspect any run: live transcript, exact input, output
  files and branch diff. Chat with a running agent or resume a finished
  session.
- **Human in the loop.** Steps can require approval, and any agent can hand a
  task to you when it is stuck.
- **Usage tracking.** Plan usage and limits, token usage per run, API-equivalent
  cost estimates and system load.
- **MCP server.** Other agents can watch and drive the board (see below).

## Requirements

- Rust (2024 edition), `git`, and the GitHub CLI `gh`, logged in.
- At least one agent CLI, logged in:
  - The [Claude Code](https://claude.com/claude-code) `claude` CLI. Agents
    always use this login (`ANTHROPIC_*` environment variables are removed),
    so with a subscription, runs count against your plan.
  - The [Codex CLI](https://github.com/openai/codex) `codex`, for steps that
    use OpenAI models (`gpt-*`, `o3`, `o4-mini`, or any model containing
    `codex`).

Costs shown in the UI are API-equivalent estimates, not actual charges.

## Getting started

```bash
cargo run --release
```

Open http://127.0.0.1:7878 and select a repository. Its pipelines, prompts and
tasks are stored in `<repo>/.claudechain`, which is added to
`.git/info/exclude`. Recently opened repositories are remembered in
`~/.config/claudechain/state.json`.

Each browser tab shows one repository and pipeline, identified in the URL
(`?repo=<id>`), so tabs survive a reload and can be bookmarked. Repositories
that no tab shows keep running their agents until the server stops.

| Option             | Description                                    |
| ------------------ | ---------------------------------------------- |
| `--repo <path>`    | Repository to open (default: the most recent)  |
| `--data <dir>`     | Data directory (default: `<repo>/.claudechain`)|
| `--port <port>`    | Port to listen on (default `7878`)             |
| `--host <host>`    | Address to bind to (default `127.0.0.1`)       |
| `--claude <path>`  | Path to the `claude` CLI                       |
| `--codex <path>`   | Path to the `codex` CLI                        |

### OpenAI models (Codex)

Steps whose model is an OpenAI model (`gpt-*`, `o3`, `o4-mini`, anything with
`codex`) run through the [Codex CLI](https://github.com/openai/codex)
(`codex exec --json`, logged in; `--codex <path>` to point at it) instead of
Claude Code. Add the model ids to the pipeline's model list to pick them.
Differences: Codex has no tool allow list, so the permission mode picks its
sandbox (`plan` → read-only, `bypassPermissions` → full access, otherwise
workspace-write with network, plus the item folder, the repository's `.git`
and `~/.cargo`). The user's `~/.codex/config.toml` is ignored (no plugins or
MCP servers). Messages sent while a Codex agent runs are kept for its next
run. Codex's 5-hour and weekly limits are read from its session logs
(`~/.codex/sessions`); the switch next to "Plan usage" on the Resources page
picks whether that page and the toolbar show Claude's or Codex's limits
(auto-pause uses Claude's). Codex reports no cost: the API-equivalent cost of Codex
runs is estimated from a built-in table of OpenAI API prices (`codex::price`;
cache writes at the 30-minute price; unknown models show no cost). Token counts are split into uncached input, cached input and
output like Claude's. Pipeline generation still uses
Claude.

## How it works

### Runs and transitions

Each run starts the agent CLI non-interactively in the task's worktree, with
the step's prompt, permission mode and tools. When it finishes, the agent
writes a message for the next step and a `transition.json`
(`{"to": "next" | "<step id>" | "human", ...}`), and ClaudeChain moves the task
accordingly. A missing transition, a manual stop, or exceeding
`max_iterations` hands the task to you. Running agents stop with the server and
are requeued on the next start.

### Worktrees and branches

Every task gets its own git worktree on a dedicated branch,
`claudechain/<id>`, created from the repository's main branch. All steps of a
task work in that same worktree, so changes accumulate on one branch that can
be reviewed, pushed or turned into a pull request. Tasks never touch your main
checkout or each other, which is what makes running them in parallel safe.
Pipelines other than the first include their folder name in the branch
(`claudechain/<folder>-<id>`) so that branch names don't collide. If a task's
branch falls behind and conflicts with main, a dedicated merge-conflict agent
can resolve it.

### Data layout

```
<repo>/.claudechain/
  pipeline.toml        steps and their settings
  prompts/             general.md plus one prompt per step
  items/<id>/          task description, state, messages/ and runs/
  worktrees/<id>/      git worktree on branch claudechain/<id>
  pipelines.json       this pipeline's name and the list of all pipelines
```

Each additional pipeline is its own data directory with the same layout.
Switching between pipelines in the UI never interrupts running agents.

## MCP server

ClaudeChain serves a basic [MCP](https://modelcontextprotocol.io) endpoint at
`/mcp` (Streamable HTTP, JSON responses), so an agent can watch and drive the
board. Add it to Claude Code with:

```bash
claude mcp add --transport http claudechain http://127.0.0.1:7878/mcp --header "X-ClaudeChain: 1"
```

The `X-ClaudeChain` header is required for every POST (see the cross-site
protection in `src/web.rs`). Available tools: `list_repos`, `board` (text
overview of all items per step: status, iteration, last run, PR),
`list_items`, `list_steps`, `get_item`, `get_run_message`, `add_item`,
`approve`, `move_item`, `retry`, `stop` and `send_message`.

Every tool takes an optional `repo` argument (a repository id from
`list_repos`, its path or its directory name). Without it, `board` and
`list_items` cover all open repositories, item tools find the repository that
has the item, and `add_item`/`list_steps` work when exactly one repository is
open.

## Security

ClaudeChain has no authentication: anyone who can reach the server can run
agents on your machine. By default it listens on localhost only, rejects
non-local `Host` headers and requires an `X-ClaudeChain` header on every
change, so other websites cannot drive it from your browser. To use it from
another machine, prefer an SSH tunnel (`ssh -L 7878:127.0.0.1:7878 host`) over
`--host 0.0.0.0`.

Agents run with the permission mode configured per step (default `dontAsk`)
and only the tools granted to that step. Only import pipeline files you trust:
their prompts and tools determine what the agents do.

## License

MIT. See [LICENSE](LICENSE).
