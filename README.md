# swarm

**Run 5 AI agents at once. Zero conflicts. One terminal.**

[![CI](https://github.com/ApiariTools/swarm/actions/workflows/ci.yml/badge.svg)](https://github.com/ApiariTools/swarm/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Swarm is a TUI agent multiplexer — it manages multiple AI coding agents running in parallel git worktrees. Each agent gets its own isolated branch and worktree, so they never conflict.

```
┌──────────────┬──────────────────────────────────┐
│  swarm (5)   │  Agent 1 — "add auth"            │
│              │  ● running  ⏱ 3m                 │
│  ● add-auth  │                                   │
│  ● fix-bug   │  > Adding OAuth2 middleware...    │
│  ● refactor  ├──────────────────────────────────┤
│  ◆ tests     │  Agent 2 — "fix login bug"       │
│  ✓ docs      │  ● running  ⏱ 1m                 │
│              │                                   │
│              │  > Found root cause in session    │
│              │    handler, patching...           │
│  n new       ├──────────────────────────────────┤
│  t term      │  Agent 3 — "refactor utils"      │
│  ↵ jump      │  ✓ done  — PR #42 opened         │
│  ? help      │                                   │
└──────────────┴──────────────────────────────────┘
```

## The problem

You have 5 coding tasks. You could do them one at a time — or you could run 5 AI agents in parallel, each on its own git branch, and review the PRs as they land.

The hard part isn't spawning agents. It's the isolation. Two agents editing the same working tree will step on each other. Stashing and switching branches doesn't scale. You need real isolation — separate working directories, separate branches, no conflicts.

Swarm gives each agent its own [git worktree](https://git-scm.com/docs/git-worktree), manages the daemon processes, tracks their status, and cleans everything up when they're done. You see all of them in one terminal, jump into any agent's session, and merge results back when they're ready.

**5 tasks. 5 agents. 1 terminal. No conflicts.**

## Features

- Creates isolated git worktrees for parallel coding tasks
- Spawns AI agents (Claude, Codex, Gemini) in each worktree via a background daemon
- Tracks agent status, PR URLs, and waiting/running state in a live TUI
- Merge agent branches back to your base branch with one keystroke
- Auto-pulls local main branch on worktree create and close
- Auto-symlinks `.env` files (and paths listed in `.swarm/worktree-links`) into new worktrees
- Multi-repo support — point swarm at a workspace with multiple repos

## Requirements

- [Rust](https://rustup.rs) (for building from source)
- [Git](https://git-scm.com)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI (`claude`)
- [Codex CLI](https://github.com/openai/codex) (`codex`) — optional, for Codex agent
- [Gemini CLI](https://github.com/google-gemini/gemini-cli) (`gemini`) — optional, for Gemini agent
- [GitHub CLI](https://cli.github.com) (`gh`) — optional, for PR status tracking

## Install

Swarm is not yet published to crates.io. Install from source:

```bash
# Clone the repo
git clone https://github.com/ApiariTools/swarm.git
cd swarm

# Build and install
cargo install --path .
```

## Quick start

```bash
# 1. Launch swarm in your project directory
cd ~/projects/my-app
swarm

# 2. Press `n` to create a new worker
#    → Enter your task: "add user authentication"
#    → An agent spawns in its own worktree and starts coding

# 3. Press `n` again — spin up as many agents as you want
#    Each one gets its own isolated branch and directory

# 4. Press `Enter` to jump into any agent's session
#    Press `Esc` to return to the overview

# 5. When an agent finishes, press `m` to merge its branch back
```

## Usage

### TUI

```bash
swarm                        # Launch in current directory
swarm -d ~/projects/myapp    # Launch with a specific workspace
```

### CLI commands

```bash
# Spawn a new worker
swarm create --repo my-repo --prompt-file /tmp/task.txt

# Inline prompt instead of file
swarm create --repo my-repo "add user authentication"

# List all workers and their state
swarm status
swarm status --json          # Machine-readable output

# Send a follow-up message to a running agent
swarm send <worktree-id> "now add tests"

# Merge a worktree's branch into base
swarm merge <worktree-id>

# Close and clean up a worktree
swarm close <worktree-id>
```

Always pass `--dir <workspace-root>` or run from the workspace root.

### Agents

| Agent | Flag | Behavior |
|-------|------|----------|
| `claude-tui` (default) | _none_ | Persistent — stays alive after task, accepts follow-up messages |
| `claude` | `--agent claude` | Autonomous — exits after completing the task |
| `codex` | `--agent codex` | Uses OpenAI Codex in full-auto mode |
| `gemini` | `--agent gemini` | Uses Google Gemini CLI in autonomous mode |

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `n` | New worktree + agent |
| `t` | Add terminal pane to selected worktree |
| `j` / `k` | Navigate worktrees |
| `Enter` | Jump to agent pane |
| `m` | Merge worktree into base branch |
| `x` | Close worktree |
| `p` | Show PR details |
| `?` | Toggle help |
| `q` | Quit |

## How it works

Swarm orchestrates three things: **worktrees**, a **daemon**, and **agent processes**.

```
                  swarm (TUI)
                      │
          ┌───────────┼───────────┬───────────┐
          ▼           ▼           ▼           ▼
     ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
     │ worktree│ │ worktree│ │ worktree│ │worktree │   ← isolated git worktrees
     │ branch/1│ │ branch/2│ │ branch/3│ │branch/4 │      (separate directories)
     └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘
          │           │           │           │
          ▼           ▼           ▼           ▼
     ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐
     │ claude  │ │ claude  │ │ codex   │ │ gemini  │   ← agent processes
     │ (daemon)│ │ (daemon)│ │ (daemon)│ │ (daemon)│      (one per worktree)
     └─────────┘ └─────────┘ └─────────┘ └─────────┘
```

**Worktrees.** When you create a worker, swarm runs `git worktree add` to create a new working directory branched from your current HEAD. Each agent operates in total isolation — no merge conflicts, no lock contention, no stepping on each other's files.

**Daemon processes.** Each agent runs as a background daemon process managed by swarm. The TUI communicates with agents via IPC (JSON messages over files). Agents can be stopped, restarted, or sent follow-up prompts.

**State persistence.** All state lives in `.swarm/` inside your workspace. Swarm survives restarts — relaunch the TUI and it reconnects to running agents.

## Multi-repo support

Point swarm at a directory containing multiple git repos and it will detect them automatically:

```bash
swarm -d ~/projects/my-monorepo-workspace
```

When creating a new worktree, you'll get a repo picker before entering your task prompt.

## Merging

1. All changes in the worktree are staged and committed
2. The worktree's branch is merged into the base branch with `--no-ff`
3. The worktree and branch are cleaned up

## State & files

Swarm stores its state in a `.swarm/` directory inside your workspace:

```
.swarm/
  state.json        # Session state (survives restarts)
  inbox.jsonl       # Incoming IPC messages
  events.jsonl      # Event log
  wt/               # Git worktrees
  worktree-links    # Extra files to symlink into worktrees (one path per line)
```

Add `.swarm/` to your `.gitignore`.

## Ecosystem

Swarm is part of the [Apiari](https://github.com/ApiariTools/apiari) toolchain. Apiari is the orchestration layer — it can generate tasks, fan them out to swarm workers, and manage the full lifecycle of multi-agent coding workflows.

## License

MIT
