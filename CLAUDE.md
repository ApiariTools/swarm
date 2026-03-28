# Swarm

## Rules
1. You are working in a git worktree on a `swarm/*` branch. Never commit to main.
2. Only modify files within this repository.
3. Do not run `cargo install` or modify system state.

## Architecture
- `src/main.rs` — CLI entry point (clap)
- `src/daemon/` — daemon event loop, worker lifecycle, protocol
- `src/daemon_tui/` — TUI (worker list, conversation panel)
- `src/core/` — git operations, state management, profiles, shell utils
- `src/core/state.rs` — WorktreeState, WorkerPhase, ReviewVerdict
- `src/core/profile.rs` — worker profiles (default, reviewer)

## Pre-Commit Checks
**Run `cargo fmt -p apiari-swarm` before every commit.** CI runs `cargo fmt -p apiari-swarm --check` and will reject unformatted code. Do not skip this step.
