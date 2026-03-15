# swarm

Run agents in parallel. Git worktrees + daemon processes + vibes.

Swarm is a TUI agent multiplexer — it manages multiple AI coding agents running in parallel git worktrees. Each agent gets its own isolated branch and worktree, so they never conflict.

```
┌──────────────┬──────────────────────────────┐
│  swarm (3)   │  Agent 1 - "add auth"        │
│              │  ● running                    │
│  ● add-auth  │                               │
│  ● fix-bug   ├──────────────────────────────┤
│  ◆ refactor  │  Agent 2 - "fix login bug"   │
│              │  ◆ waiting                    │
│              │                               │
│              ├──────────────────────────────┤
│              │  Agent 3 - "refactor utils"   │
│  n new       │  ✓ done                       │
│  t term      │                               │
│  ↵ jump      │                               │
│  ? help      │                               │
└──────────────┴──────────────────────────────┘
```

### What it does

- Creates isolated git worktrees for parallel coding tasks
- Spawns Claude agents in each worktree via a background daemon
- Tracks agent status, PR URLs, and waiting/running state
- Auto-pulls local main branch on worktree create and close
- Auto-symlinks `.env` files (and paths listed in `.swarm/worktree-links`) into new worktrees

## Requirements

- [Rust](https://rustup.rs) (for building)
- [Git](https://git-scm.com)
- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI (`claude`)
- [GitHub CLI](https://cli.github.com) (`gh`) — optional, for PR status tracking

## Install

```bash
cargo install --path .
```

On macOS, always codesign after install:

```bash
codesign -f -s - ~/.cargo/bin/swarm
```

## Usage

### TUI

```bash
swarm                        # Launch in current directory
swarm -d ~/projects/myapp    # Launch with a specific workspace
```

### CLI Commands

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

## License

MIT
