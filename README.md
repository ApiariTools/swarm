# swarm

Run agents in parallel. Git worktrees + custom daemons + vibes.

Swarm is a terminal UI that lets you run multiple AI coding agents simultaneously, each in its own isolated git worktree. Think of it as a multiplayer mode for AI-assisted development.

## How it works

```
┌──────────────┬──────────────────────────────┐
│  swarm (3)   │  Agent 1 - "add auth"        │
│              │  $ claude --dangerously-...   │
│  ● add-auth  │                               │
│  ● fix-bug   ├──────────────────────────────┤
│  ◆ refactor  │  Agent 2 - "fix login bug"   │
│              │  $ claude --dangerously-...   │
│              │                               │
│              ├──────────────────────────────┤
│              │  Agent 3 - "refactor utils"   │
│  n new       │  (done)                       │
│  t term      │                               │
│  ↵ jump      │                               │
│  ? help      │                               │
└──────────────┴──────────────────────────────┘
```

Each agent gets its own git branch and worktree, so they never conflict. When an agent finishes, merge its work back with a single keystroke.

## Requirements

- [Rust](https://rustup.rs) (for building)
- At least one supported agent CLI:
  - [Claude Code](https://docs.anthropic.com/en/docs/claude-code) (`claude`)
  - [Codex](https://github.com/openai/codex) (`codex`)
- [GitHub CLI](https://cli.github.com) (`gh`) - optional, for PR status tracking

## Install

```bash
cargo install --path .
```

Or build manually:

```bash
cargo build --release
# Binary at target/release/swarm
```

## Usage

```bash
# Launch in the current directory
swarm

# Launch with a specific directory
swarm -d ~/projects/my-app

# Use claude (one-shot) instead of default claude-tui
swarm -a claude
```

### Keyboard Shortcuts

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

### CLI Commands

Swarm also supports commands for scripting and inter-process communication:

```bash
swarm status              # Show current state
swarm status --json       # Machine-readable output
swarm create --repo my-repo "add auth"   # Create a new worktree with an agent
swarm send my-task "msg"  # Send a message to a running agent
swarm merge my-task       # Merge a worktree's branch into base
swarm close my-task       # Close and clean up a worktree

# For long/multiline prompts, use --prompt-file instead of inline args:
swarm create --repo my-repo --prompt-file /tmp/task.txt
```

## Multi-Repo Support

Point swarm at a directory containing multiple git repos and it will detect them automatically:

```bash
swarm -d ~/projects/my-monorepo-workspace
```

When creating a new worktree, you'll get a repo picker before entering your task prompt.

## How Merging Works

1. All changes in the worktree are staged and committed
2. The worktree's branch is merged into the base branch with `--no-ff`
3. The worktree and branch are cleaned up

## State & Files

Swarm stores its state in a `.swarm/` directory inside your project:

```
.swarm/
  state.json      # Session state (survives restarts)
  inbox.jsonl     # Incoming IPC messages
  events.jsonl    # Event log
  wt/             # Git worktrees
```

You may want to add `.swarm/` to your `.gitignore`.

## License

MIT
