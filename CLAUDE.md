# Swarm

Rust TUI that orchestrates multiple AI coding agents in parallel using git worktrees and tmux.

## Quick Reference

```bash
cargo build --release          # Build
cargo run                      # Run (launches tmux session with TUI sidebar)
cargo run -- --help            # CLI help
cargo test                     # Run tests (none yet)
```

## Architecture

```
src/
  main.rs              # CLI (clap) + tmux session bootstrap
  core/
    agent.rs           # AgentKind enum (Claude, Codex) + launch commands
    git.rs             # Git worktree/branch operations, multi-repo detection
    ipc.rs             # JSONL-based inbox/events message passing
    merge.rs           # Commit-all + merge-into-base flows
    state.rs           # JSON persistence (.swarm/state.json)
    tmux.rs            # Tmux wrapper: sessions, panes, splits, styling
  tui/
    mod.rs             # Event loop (100ms tick, crossterm key handling)
    app.rs             # App state machine + all business logic (largest file)
    render.rs          # Ratatui rendering (sidebar, overlays, welcome screen)
    theme.rs           # Color palette (honey/amber bee theme)
```

## Key Concepts

- **Worktree** (`app.rs:Worktree`): The primary unit of work. Each has an isolated git worktree, a branch (`swarm/<sanitized-prompt>-N`), an agent pane, and optional terminal panes.
- **AgentKind** (`agent.rs`): Currently `Claude` and `Codex`. Agents launch via their CLI in a tmux pane with `--dangerously-skip-permissions` for Claude.
- **Sidebar**: The TUI runs in a narrow left pane (38 chars) using `main-vertical` tmux layout. Agent panes stack vertically to its right.
- **IPC**: External commands write JSONL to `.swarm/inbox.jsonl`, sidebar reads on 500ms tick. Events emitted to `.swarm/events.jsonl`.
- **State**: Persisted to `.swarm/state.json` on every mutation. Restored on restart with orphan worktree discovery.

## How It Works

1. `swarm` creates/joins a tmux session named `swarm-<dirname>`
2. The sidebar TUI runs in the leftmost pane
3. Press `n` to create a new worktree: prompt -> pick agent -> git worktree created -> agent launched in split pane
4. Each worktree gets a unique color (8-color cycle) for pane borders and sidebar indicator
5. Pane liveness checked every 3s, PR status via `gh` every 30s
6. `m` merges worktree branch into base (commit all -> merge --no-ff -> cleanup)
7. `x` closes worktree (kill panes -> remove git worktree -> delete branch)

## Modes (State Machine)

`Normal` -> `RepoSelect` (multi-repo) -> `Input` -> `AgentSelect` -> creates worktree
`Normal` -> `Confirm` (for merge/close actions)
`Normal` -> `Help` (toggle with `?`)

## CLI Commands (IPC)

```bash
swarm                              # Launch sidebar TUI
swarm status [--json]              # Print state
swarm create "task prompt"         # Queue new worktree
swarm send <worktree-id> "msg"     # Send message to agent
swarm close <worktree-id>          # Close a worktree
swarm merge <worktree-id>          # Merge and close
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

- **ratatui** + **crossterm**: TUI rendering and input
- **tokio**: Async runtime (event loop)
- **clap**: CLI parsing
- **serde** + **serde_json**: State/IPC serialization
- **chrono**: Timestamps
- **uuid**: Message IDs
- **color-eyre**: Error handling
- **dirs**: Home directory lookup

## Conventions

- All git branches created by swarm are prefixed `swarm/`
- Shell commands use `'\''` idiom for safe single-quote escaping
- Tmux pane IDs are `%N` format (e.g., `%0`, `%3`)
- Multi-repo: if the working dir contains multiple child git repos, swarm detects and offers a picker
- Auto-trusts `mise` if repo has `.mise.toml` or `mise.toml`
- Release binary uses LTO + strip for small size
