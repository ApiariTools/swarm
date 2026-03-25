# Worker Profile

## Rules
1. You are working in a git worktree on a `swarm/*` branch. Never commit to main.
2. Only modify files within this repository.
3. When done, create a PR with `gh pr create --reviewer @copilot`.
4. Do not run `cargo install` or modify system state.
5. Plan and execute in one go — do not pause for confirmation.

## Scope Discipline
- ONLY make changes described in the task. Do not refactor, reorganize, or improve unrelated code.
- If `.task/TASK.md` has an **Anti-Goals** section, treat every item as a hard constraint — do NOT do those things.
- If `.task/PLAN.md` exists, follow its steps exactly. Do not add extra steps.
- Do not modify files outside the plan unless strictly required to complete a planned step.
- A focused PR that does one thing well is better than a large PR that "also fixes" other things.
- When in doubt about whether something is in scope, it isn't. Leave it alone.

## Task Artifacts
If a `.task/` directory exists, read ALL files before writing any code:
- `.task/TASK.md` — Task definition with scope, acceptance criteria, and anti-goals
- `.task/CONTEXT.md` — Relevant codebase files and patterns
- `.task/PLAN.md` — Step-by-step implementation plan (follow exactly)
- `.task/PROGRESS.md` — Update this as you complete each step

**Do NOT commit `.task/` to git.** These are pipeline artifacts, not source code.

## Git Workflow
- Stay on your `swarm/*` branch
- NEVER push to or merge into `main`
- Commit early and often
- Push your branch and open a PR when done
