# AGENTS.md

Instructions for coding agents (Claude Code, Codex, Antigravity, ...) working on
this repository. `README.md` is the user-facing documentation; read it for what
the product does. This file covers how to work on the code.

## What this is

ClaudeChain is a single Rust binary (axum + tokio) that serves a local web UI at
http://127.0.0.1:7878. It runs work items (GitHub issues or plain-text tasks)
through a configurable pipeline of agent steps. Each step is a separate
`claude -p` child process, running in a git worktree for that item. Agents
communicate only through files; each run ends by writing a message file and a
`transition.json` that tells the orchestrator where the item goes next.

There is no database: the files under the data directory are the state.

## Commands

```bash
cargo build                    # debug build
cargo test                     # unit tests (all inline, see "Testing")
cargo clippy --all-targets     # currently warning-free; keep it that way
cargo run --release            # start the server on 127.0.0.1:7878
cargo run -- --repo <path> --port 7879   # open a specific repo / avoid a running instance
```

- There is no frontend build. `static/` is plain JavaScript, HTML and CSS.
- `static/*` and `defaults/*` are embedded with `include_str!` (see
  `src/web.rs` and `src/config.rs`). After editing them, rebuild and restart
  the server to see the change.
- Logging uses `tracing`. Set `RUST_LOG=claudechain=debug` for more output.

## Layout

```
src/
  main.rs          CLI args (clap), starts Workspace and the web server
  workspace.rs     open repositories and their pipelines (one App per data dir)
  orchestrator.rs  App: owns pipeline, items and running agents; the scheduler
                   loop, transitions, builds the `claude` command line
  runner.rs        runs one agent: spawns the CLI, stream-json in/out, writes
                   events.jsonl, strips environment variables
  reaper.rs        kills every process an agent started (Linux, via /proc)
  store.rs         on-disk types (Item, Run, Transition, ...) and atomic writes
  config.rs        pipeline.toml (Pipeline, Step), prompts, export/import bundle
  git.rs           worktrees, branches, diffs (shells out to `git`)
  web.rs           HTTP API, SSE, static files, cross-site protection
  mcp.rs           MCP endpoint at /mcp (JSON-RPC over HTTP)
  generate.rs      "Generate pipeline using prompt"
  account.rs       per-user state in ~/.config/claudechain/state.json
  sysmon.rs        Resources page: CPU, memory, disk, temperatures (Linux)
static/            index.html, app.js (board, inspector, editor), resources.js, style.css
defaults/          pipeline.toml and prompts written into a new data directory
```

## Conventions

- **Match the surrounding code.** The code is not formatted with `rustfmt`
  (lines go past 100 columns), so do not run `cargo fmt` on files you touch;
  it would rewrite unrelated lines. Format new code by hand in the same style.
- **Comments.** Every module starts with a `//!` doc comment explaining its
  role. Public items get a `///` comment that says what something is for or
  why it works the way it does, not a restatement of the code. Keep this up
  for new modules and items.
- **Errors** use `anyhow` with `.context(...)`. HTTP handlers return
  `ApiError` (`src/web.rs`).
- **Dependencies.** Keep the dependency list short. Prefer shelling out to
  `git` and `gh`, as the existing code does, over adding crates. Do not add a
  JS framework, bundler or npm.
- **Platform.** Linux-only code (anything reading `/proc` or `/sys`) is gated
  with `#[cfg(target_os = "linux")]` and has a no-op fallback.
- **Scope.** Keep changes focused on the task. Don't refactor unrelated code.

## Invariants to preserve

Changes in these areas need extra care. Don't weaken them without being asked to.

- **Security (`src/web.rs`).** There is no authentication. The server binds to
  localhost by default, rejects non-local `Host` headers (DNS rebinding), and
  requires an `X-ClaudeChain` header on every request other than GET/HEAD
  (`guard`). Add new routes inside `router()` so the guard covers them, and
  never let a GET handler change state. `/mcp` goes through the same guard.
- **Agent environment (`src/runner.rs`).** The runner removes `ANTHROPIC_*`,
  `CLAUDECODE`, `CLAUDE_CODE_*` and the variables that `cargo run` and rustup
  set, so agents use the subscription login and the worktree's own toolchain.
  Agents get `--strict-mcp-config` so the user's personal MCP servers are not
  loaded.
- **Process cleanup (`src/reaper.rs`).** Every agent gets a marker environment
  variable, and all processes that carry it are killed when the run ends. New
  ways of spawning agents must keep the marker and the reaper.
- **State on disk (`src/store.rs`).** Write state files with
  `store::write_atomic` (temp file + rename). Only the orchestrator writes
  `item.json`. Each run has its own numbered message file and
  `runs/NNN-<step>/transition.json`, so runs never overwrite each other.
- **Backward compatibility.** Users have existing data directories. New fields
  in `pipeline.toml`, `item.json` and similar files need `#[serde(default)]`.
  A missing prompt file falls back to the built-in default
  (`config::read_prompt`).
- **Git.** Item branches are `claudechain/<id>` (or `claudechain/<folder>-<id>`
  for pipelines other than the first) and start from the local main branch,
  not from HEAD (`git::ensure_worktree`).
- **Transition protocol.** The JSON shape of `transition.json` and the rules in
  `defaults/prompts/general.md` are a contract with every agent prompt. If you
  change one, update the other.

## Defaults and prompts

`defaults/pipeline.toml` and `defaults/prompts/*.md` are only copied into a
data directory when it is created. Editing them does not change existing
repositories, which keep their own copies in `<repo>/.claudechain/`. A new
built-in prompt needs an entry in the `include_str!` table in `src/config.rs`.
`config::tests` round-trips the default pipeline through export/import, so run
`cargo test` after changing either.

## Testing

- Tests are inline `#[cfg(test)] mod tests` blocks in the module they test.
  Add tests next to the code you change.
- Git tests create throwaway repositories in a temp directory. Tests must not
  call the real `claude` or `gh` CLIs or touch the network.
- For UI changes, `cargo test` is not enough. Run the server
  (`cargo run -- --port 7879` if 7878 is in use), open it in a browser, and
  check the change. Look at the browser console for errors.

## Things not to touch

- `.claudechain/` holds live data: ClaudeChain is used to develop itself, so
  this folder contains real work items, run transcripts and other agents'
  worktrees. Don't edit, delete or commit anything in it. It is gitignored.
- `target/`.
- Branches named `claudechain/*` belong to pipeline items. Don't rebase,
  delete or force-push them unless you are the agent working on that item.

## If you are running inside ClaudeChain

When this repository is worked on by its own pipeline, your step prompt and
`general.md` define the file protocol: where to write your message and
transition, and which transitions are allowed. Follow them. This file only
describes the codebase. Work only in your worktree.

## Git and docs

- Commit messages: a short subject line in sentence case and the imperative
  mood, without a trailing period. For example, "Add pipeline export/import as
  a shareable TOML bundle" or "Kill every process an agent started when its
  run ends". Add a body if the reason isn't obvious.
- Don't push, open pull requests or comment on GitHub unless asked to.
- Update `README.md` when you change behaviour users can see: CLI options, the
  data layout, MCP tools, requirements or security behaviour.

## Known gaps

- `README.md` already describes Codex CLI support (a `--codex` option and
  routing OpenAI models to `codex`), but on `main` the runner only drives
  `claude`. The Codex work is in progress on the `claudechain/item-10` branch
  (`src/codex.rs`). Until that is merged, don't assume Codex support exists on
  `main`. Remove this section once it is merged.
