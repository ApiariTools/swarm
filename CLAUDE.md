# Swarm

Rust TUI that orchestrates multiple AI coding agents in parallel using git worktrees and custom daemon processes.

## Quick Reference

```bash
cargo build --release          # Build
cargo run                      # Run (launches TUI)
cargo run -- --help            # CLI help
cargo test                     # Run tests
```

## Git Workflow

- You are working in a swarm worktree on a `swarm/*` branch. Stay on this branch.
- NEVER push to or merge into `main` directly.
- When done, create a PR from your branch. Swarm will handle merging.
- NEVER run `git push origin main` or `git checkout main`.

## Architecture

```
src/
  main.rs              # CLI (clap) + daemon bootstrap + IPC handlers
  core/
    agent.rs           # AgentKind enum (Claude, Codex) + launch commands
    git.rs             # Git worktree/branch operations, multi-repo detection
    ipc.rs             # JSONL-based inbox/events message passing
    merge.rs           # Commit-all + merge-into-base flows
    shell.rs           # shell_quote() and sanitize() helpers (with unit tests)
    state.rs           # JSON persistence (.swarm/state.json)
  tui/
    mod.rs             # Event loop (100ms tick, crossterm key handling)
    app.rs             # App state machine + all business logic (largest file, ~1500 lines)
    picker.rs          # Repo/agent picker popup
    render.rs          # Ratatui rendering (sidebar, overlays, welcome screen)
    theme.rs           # Color palette (honey/amber bee theme)
```

## Key Concepts

- **Worktree** (`tui/app.rs:Worktree`): The primary unit of work. Each has an isolated git worktree, a branch (`swarm/<sanitized-prompt>-N`), an agent pane, optional terminal panes, PR tracking info, and an LLM-generated summary.
- **AgentKind** (`core/agent.rs`): Currently `ClaudeTui` (default), `Claude`, and `Codex`. Agents launch as daemon subprocesses with `--dangerously-skip-permissions` for Claude.
- **TUI**: Full-screen ratatui interface with worker list, conversation view, and input box. Each worker's agent runs as a background daemon process.
- **IPC**: External commands write JSONL to `.swarm/inbox.jsonl`, sidebar reads on 500ms tick. Events emitted to `.swarm/events.jsonl`. CLI commands auto-start the sidebar if it's not already running.
- **State**: Persisted to `.swarm/state.json` on every mutation. Restored on restart with orphan worktree discovery.
- **Shell utilities** (`core/shell.rs`): `shell_quote(s)` wraps in single quotes using the `'\''` escape idiom. `sanitize(s)` produces safe branch/directory names (lowercase, non-alphanumeric replaced with hyphens, truncated to 40 chars). Both have unit tests.

## How It Works

1. `swarm` launches a full-screen TUI
2. Press `n` to create a new worktree: prompt -> pick agent -> git worktree created -> agent daemon spawned
3. Each worker gets a unique color (8-color cycle) for visual identification
4. Agent process liveness checked every 3s, PR status via `gh` every 30s
6. `m` merges worktree branch into base (commit all -> merge --no-ff -> cleanup)
7. `x` closes worktree (kill panes -> remove git worktree -> delete branch)

## Modes (State Machine)

`Normal` -> `RepoSelect` (multi-repo) -> `Input` -> `AgentSelect` -> creates worktree
`Normal` -> `Confirm` (for merge/close actions)
`Normal` -> `Help` (toggle with `?`)
`Normal` -> `PrDetail` (PR overlay with open/copy actions, toggle with `p`)

## Keyboard Shortcuts

### Sidebar (Normal mode)

| Key | Action |
|-----|--------|
| `n` | New worktree + agent |
| `t` | Add terminal pane to selected worktree |
| `j` / `k` / arrows | Navigate worktrees |
| `Enter` | Jump to agent pane |
| `m` | Merge worktree into base branch |
| `x` | Close worktree |
| `p` | Show PR detail overlay |
| `?` | Toggle help |
| `q` | Quit |

### PR Detail overlay

| Key | Action |
|-----|--------|
| `o` / `Enter` | Open PR in browser |
| `c` | Copy PR URL to clipboard |
| `Esc` / `p` / `q` | Dismiss |

### Input mode

| Key | Action |
|-----|--------|
| `Enter` | Submit |
| `Alt+Enter` | Add newline |
| `Esc` | Cancel |

### Confirm mode

| Key | Action |
|-----|--------|
| `y` / `Enter` | Confirm |
| `n` / `Esc` | Cancel |

## CLI Commands

```bash
swarm                              # Launch sidebar TUI
swarm status [--json]              # Print state
swarm create "task prompt"         # Queue new worktree (auto-starts sidebar if needed)
swarm send <worktree-id> "msg"     # Send message to agent
swarm close <worktree-id>          # Close a worktree
swarm merge <worktree-id>          # Merge and close
swarm pick                         # Run interactive repo/agent picker
```

## File Layout on Disk

```
project/
  .swarm/
    state.json         # Persistent app state
    inbox.jsonl        # IPC: incoming commands
    events.jsonl       # IPC: emitted events
    wt/                # Git worktrees live here
      task-name-1/
      task-name-2/
```

## Dependencies

Workspace crates:
- **apiari-common**: Shared types and utilities

External:
- **ratatui** + **crossterm**: TUI rendering and input
- **tokio**: Async runtime (event loop)
- **clap**: CLI parsing
- **serde** + **serde_json**: State/IPC serialization
- **toml**: Config file parsing
- **chrono**: Timestamps
- **uuid**: Message IDs
- **color-eyre**: Error handling
- **dirs**: Home directory lookup

## Integration with Apiari Toolchain

Swarm is part of the [Apiari](https://github.com/ApiariTools) toolchain. It can be used standalone or with the other tools:

```
hive start <quest-id>
  |
  v (subprocess)
swarm create "task prompt"     # Hive dispatches tasks to swarm
  |
  v
.swarm/state.json              # Keeper reads this for dashboard
                               # Keeper reads this for dashboard display
```

| Tool | Relationship |
|------|-------------|
| **hive** | Calls swarm CLI as subprocess (`create`, `status`, `send`, `close`, `merge`) to execute tasks |
| **keeper** | Reads `.swarm/state.json` for dashboard display |
| **buzz** | No direct interaction (buzz signals go to hive/keeper, not swarm) |
| **apiari-common** | Shared types and utilities (workspace crate) |

### IPC Protocol

Other tools interact with swarm via CLI commands that write to `.swarm/inbox.jsonl`:

```bash
swarm create "task prompt"     # Queues a CreateWorktree message
swarm send <id> "message"      # Queues a SendMessage
swarm close <id>               # Queues a CloseWorktree
swarm merge <id>               # Queues a MergeWorktree
swarm status --json            # Reads state directly (no IPC)
```

The TUI polls `inbox.jsonl` every 500ms and processes queued messages. IPC commands auto-start the swarm sidebar if it's not already running.

## Conventions

- All git branches created by swarm are prefixed `swarm/`
- Shell commands use the `'\''` idiom for safe single-quote escaping (see `core/shell.rs`)
- Branch/directory names are sanitized via `sanitize()`: lowercase, hyphens, truncated to 40 chars
- Worker IDs are `<repo>-N` format (e.g., `hive-1`, `swarm-2`)
- Multi-repo: if the working dir contains multiple child git repos, swarm detects and offers a picker
- Auto-trusts `mise` if repo has `.mise.toml` or `mise.toml`
- Release binary uses LTO + strip for small size

## Merge Policy
- NEVER run `gh pr merge` — open PRs, let CI run, then stop. Merging is the human's job.
