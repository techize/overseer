# Overseer: design

**Date**: Monday, 9 November 2026
**Owner**: Jonathan (techize)
**Repo**: `github.com/techize/overseer` (public)
**Status**: draft for approval

---

## 1. Intent and why

Jonathan runs 4 to 6 Claude Code CLI sessions in parallel on his work laptop. Today they live in Warp split panes (sometimes iTerm2), and each one picks up a Jira ticket when he says "work on PLAT-12345". The sessions then use Jira and Bitbucket CLIs or MCPs, his skills, and his 2ndBrain Obsidian vault to work out the context.

The cost lands on Jonathan's memory:

- He tells the wrong session what to do next.
- He gives a second session a ticket that another session has already part-worked.
- Two sessions write to the same shared target: a repo, an ArgoCD app or pin, a trigger.dev deploy.
- He relays between sessions by hand ("S1 tells Jonathan when the pin is back on main, Jonathan tells S3").
- The 2ndBrain vault breaks **every day**. All four symptoms show up: git failures (rejected pushes, rebase mess, `index.lock`), lost or overwritten content in shared files, one session's commit sweeping up another's half-done edits, and sessions confusing each other through the shared `CURRENT_SESSION.md`.

On 9 Nov 2026 he already ran this by hand. A coordinator session split the In Progress tickets into S1 to S4 so that each shared write target had a single owner, and wrote a kickoff prompt for each. See `2ndBrain/Sessions/2026-11-09-in-progress-split-4-sessions.md`. Overseer turns that manual process into an app.

**Success** means Jonathan glances at one screen and knows, for every session, what it owns, what it holds, what it is doing and whether it needs him. The app refuses double assignments, stops cross-session write collisions and carries the relays. 2ndBrain stops breaking.

## 2. Goals

1. Run and host up to **6 worker sessions** (S1 to S6) plus **1 coordinator** (S0), each a real interactive `claude` CLI session.
2. Provide a **graphical tile board** as the main screen, with drill-in to a live terminal plus context panel by click or key.
3. Track **work items** (Jira tickets and beads) and enforce **single ownership**.
4. Track **shared write targets** with claim, release and wait, and enforce them with a hook that blocks colliding write commands.
5. Deliver **relays** session to session automatically, and show Jonathan every relay so he doesn't duplicate it.
6. Make the app the **single writer and committer for 2ndBrain shared state**, fixing all four symptoms.
7. Give the **coordinator session** tools to see the whole board, propose splits and kickoffs for approval, message sessions and summarise.
8. Ship as a **single Rust binary** that Jonathan builds from the repo on the work laptop, with no admin rights needed.

## 3. Non-goals

- Writing to Jira. Overseer reads Jira only. Comments and transitions stay with the sessions behind Jonathan's normal approval.
- Managing Claude sessions started outside Overseer, such as a quick `claude` in Warp.
- Multi-machine operation, Linux or Windows.
- Replacing Jonathan's skills, beads or the 2ndBrain structure. Overseer feeds and protects them.
- Running sessions headless or unattended. These are interactive sessions Jonathan supervises.

## 4. Anti-goals (a failure even if it technically works)

- **Blocking read-only work.** Investigation (`kubectl get`, `git log`, reading files, Jira reads) is never blocked or slowed.
- **Losing a session.** A crash or quit of the window must never kill a running Claude session. A daemon restart must make every session resumable.
- **Acting silently.** Every automatic message, relay, block and commit shows on the board. Nothing reaches a session or 2ndBrain without a visible trace.
- **Worse terminal than Warp.** If Claude Code's TUI renders badly, lags or loses keystrokes, Jonathan will go back to Warp. Terminal fidelity is a hard requirement.
- **Fighting Jonathan's existing setup.** Overseer must not rewrite `~/.claude/settings.json` or his global hooks. Its hooks and tools are added per session.

## 5. Constraints

- **Machines.**
  - The work laptop is the target. It's macOS, not MDM-managed, with **no admin rights**. Homebrew works, apps can be installed, and local servers are fine.
  - Development and testing happen on the home Mac (`home.local`), where `acli`, `bb` and Warp are absent. They can be installed if needed.
- **Claude Code.** CLI version 2.1.x. Overseer relies on these flags, all present in 2.1.259: `--settings <file>`, `--mcp-config <file>`, `--append-system-prompt-file`, `--session-id <uuid>`, `--resume <id>`, `--name`.
- **Existing tools on the work laptop.** `acli` (Jira), `bb` (Bitbucket), `bd` (beads 1.0.x, DB in `~/Repos/2ndBrain/.beads/`), `git`, the 2ndBrain vault at `~/Repos/2ndBrain` (Obsidian, with the Obsidian Git plugin making timed `vault backup` commits), and the `post-commit` hook that runs `05-Meta/Sync-System/sync-run.sh --auto` in the background.
- **Worktrees.** These live at `~/Repos/.worktrees/<repo>-<TICKET>`, the existing convention.
- **House rules.** No em dashes in any human-readable output the app writes (journal lines, commit messages, notes). Commit messages are human-style, with no AI language.

## 6. Approaches considered

| Approach | Trade-off | Outcome |
|---|---|---|
| TypeScript daemon + Electron window (xterm.js, node-pty, tmux) | Fastest to build, most proven terminal-in-UI. App is around 200MB with two runtimes. | Rejected. Jonathan wants a small, tight single binary. |
| TypeScript daemon + browser tab | Least to build. Lives among browser tabs, so `1`-`6`/`Esc` shortcuts fight the browser and notifications are weak. | Rejected. |
| Rust + Tauri (Rust core, web UI in WKWebView) | Small (10 to 15MB). UI still in TypeScript. | Rejected in favour of all-native. |
| Rust terminal UI (ratatui) inside Warp | Quickest. Not graphical, which fails the board requirement. | Rejected. |
| **Rust native GUI: GPUI + `alacritty_terminal`, one binary with a daemon mode, no tmux** | Smallest and fastest, the same terminal stack Zed ships. GPUI is pre-1.0 (pin it). We build our own session holder in place of tmux. | **Chosen** by Jonathan. |

Terminal engine options:
- **`alacritty_terminal`** (chosen): battle-tested, and Zed's terminal panel uses it.
- **`libghostty-vt`**: arguably better, but pre-1.0, its Rust bindings are at 0.2 and it isn't thread-safe. A candidate to swap in later, so keep the terminal engine behind an internal trait.
- **`wezterm-term`**: solid, but has little embedding prior art.

**tmux is dropped.** Jonathan chose the full single binary. The daemon holds the PTYs itself, and `overseer attach <slot>` replaces `tmux attach` as the emergency fallback from any terminal.

## 7. Decided design

### 7.1 One binary, several modes

`overseer` is a single Rust executable:

| Command | Role |
|---|---|
| `overseer` | Opens the GUI. Starts the daemon first if it isn't running. |
| `overseer daemon` | Long-lived background service. Owns PTYs, the ledger, the 2ndBrain writer and the Jira and beads cache. Installed as a **user LaunchAgent** (`~/Library/LaunchAgents`, no admin), so it survives closing the GUI. |
| `overseer attach <slot>` | Raw passthrough of a slot's PTY into the current terminal, such as Warp, with a detach key. |
| `overseer hook <event>` | Called by Claude Code hooks. Reads the hook JSON on stdin, asks the daemon, replies with hook JSON. Must be fast (target under 50 ms). |
| `overseer mcp` | stdio MCP server that Claude Code spawns per session. It proxies tool calls to the daemon and is identified by the `OVERSEER_SLOT` env var. |
| `overseer ctl …` | Scriptable CLI over the same API (`status`, `assign`, `locks`, `send`), useful for tests and skills. |

The GUI, hook, mcp and ctl modes all talk to the daemon over a Unix socket in `~/Library/Application Support/Overseer/`. The daemon is the single source of truth.

### 7.2 Sessions and slots

- **Slots.** S0 is the coordinator and S1 to S6 are workers. Each slot has a user-editable label, such as "magento".
- **Launching.** Starting a slot spawns `claude` in a PTY owned by the daemon:
  - **Working directory:** chosen by Jonathan, defaulting to `~/Repos`, or the worktree for its first work item.
  - **Environment:** `OVERSEER_SLOT=S3`.
  - **`--session-id <uuid>`:** recorded in the ledger.
  - **`--name "S3 karpenter"`.**
  - **`--settings <generated file>`:** adds Overseer's hooks on top of Jonathan's own settings, without editing `~/.claude/settings.json`.
  - **`--mcp-config <generated file>`:** adds the Overseer MCP server.
  - **`--append-system-prompt-file <role file>`:** tells the session it is SN of an Overseer team, lists the tools and the rules (claim before writing, report progress, use the 2ndBrain tools rather than committing).
- **Scrollback.** The daemon keeps each slot's terminal state and a scrollback buffer, so the GUI can attach, detach and reattach without losing the screen.
- **Daemon restarts.** If the daemon restarts (an upgrade, a crash or a reboot), the PTYs die but the ledger survives. On start, the board shows each slot as **resumable**, and one key relaunches it with `claude --resume <session-id>` in the same directory with the same flags. Ownership and locks are kept.
- **Quitting the GUI** never touches sessions.

### 7.3 Session state

States: **working**, **needs you**, **idle**, **blocked by lock**, **resumable**, **empty**.

| Event | Source | Effect |
|---|---|---|
| Work starts | `UserPromptSubmit` hook | **working** |
| Session waits on Jonathan | `Notification` hook (permission prompt or idle-input wait) | **needs you**, with a macOS notification. Clicking it opens that session. |
| Turn ends | `Stop` hook | **idle** |
| Write blocked | `PreToolUse` deny | **blocked by lock**, until the wait resolves |
| "Now doing" line | `PostToolUse` (last tool and short args), overridden by the latest `report_progress` text | Card's "now doing" line |
| Session registered | `SessionStart` hook | Records or confirms the session ID |

If hooks fail, a light fallback reads the terminal screen for a visible permission prompt so that **needs you** is never missed. This fallback is the builder's call.

### 7.4 Work items

- **Sources.**
  - **Jira:** through `acli` with `--json` output, reusing Jonathan's existing Jira auth, so Overseer stores no Jira credentials. The default query is the active sprint and current user, configurable as JQL.
  - **Beads:** through `bd list --json` in the 2ndBrain directory.
- **Caching.** Both are polled on a timer (Jira every few minutes, beads more often) and on demand.
- **Grouping.** A bead whose title or `external_ref` contains `PLAT-\d+` groups under that ticket. Other beads stand alone. When a standalone bead later gets a ticket, the item keeps its owner and history.
- **Ownership rules (decided):**
  - A **ticket**, or a standalone bead, has at most one owner session. Assigning it to a second session is **refused**, and the board says who owns it. Jonathan can explicitly move it, which releases it from the old owner and tells that session.
  - A **bead under a ticket owned by another session** can be assigned with a **warning** ("This bead is under PLAT-12179, owned by S3. Assign to S4 anyway?"). If confirmed, S3 is told that S4 has that slice.
- **Writing back to beads.** On assignment, the app records ownership on the bead (`bd update` assignee, or a label such as `overseer:S3`), so the history is visible in beads itself.
- **Promotion conflict.** If a standalone bead owned by one session gets linked to a ticket owned by another, the app shows the same warning as a split-off bead and tells both sessions.
- **Picker.** Assigning work uses a picker that lists Jira tickets with their beads nested underneath, plus standalone beads, showing owner, status, priority and the link to the 2ndBrain ticket folder.

### 7.5 Shared write targets and locks

**Targets** are typed strings:
- `repo:<name>:<branch>`, for the default branch or a shared checkout
- `argo:<context>/<app>`, including pins of root apps such as UAT root
- `deploy:trigger.dev/<env>`
- `tf:<workspace>`
- `custom:<anything>`

**Claims:**
- A session claims a target through the MCP tool `claim_target(target, reason)` before writing, and releases it with `release_target`.
- If the target is held, the caller joins a **wait queue**, and the tool returns "held by S1 since 08:40 for PLAT-12334".
- Jonathan can claim, release or force-release any target from the GUI.
- Kickoffs (7.7) can pre-assign targets.

**The safety net is a `PreToolUse` hook on Bash.**
- It matches write-shaped commands against a **pattern file**, such as `~/.config/overseer/targets.toml`. Each pattern is a regex mapped to a target template, for example:
  - `git push` in the cwd repo → `repo:<repo>:<branch>`
  - `argocd app set|sync <app>` → `argo:<ctx>/<app>`
  - `npx trigger.dev deploy` → `deploy:trigger.dev/<env>`
  - `terraform|tofu apply`
  - `kubectl apply|delete|patch`
- If the resolved target is held by **another** session, the hook **denies** the command and gives Claude a reason ("held by S1, use claim_target to queue"). The board flags it.
- If the target is **unheld**, the hook auto-claims it for the caller and lets the command run, so nothing goes untracked.
- If no pattern matches, the command is allowed. **Read-only commands are never matched.**
- The pattern file ships with sensible defaults drawn from Jonathan's stack and is extended over time.

**Lock lifetime (decided):**
- **Auto-claims by one-shot commands** (push, apply, deploy) are released when that command finishes (`PostToolUse`).
- **Pins** (`argocd app set --revision`, root-app pins) stay held until the pin is set back to its recorded previous target, or the lock is explicitly released.
- **Releasing a work item** releases every target the session claimed for it.
- **Long-held locks** are flagged on the board after a configurable idle period, but never dropped automatically.
- **Blocked callers queue automatically.** A command the hook denies puts the caller in that target's wait queue. The card shows "waiting on <target>" whenever the session isn't working, and the grant arrives as a relay.

**Worktrees.** When an assignment involves a repo, the app offers to create `~/Repos/.worktrees/<repo>-<TICKET>` and start or point the session there. Sessions in separate worktrees can edit freely. The lock guards the shared things: the default branch, the shared checkout, pins and deploys.

### 7.6 Relays

- When a target is released and someone is waiting, the daemon gives the target to the first waiter and **sends that session a message**, such as "S1 has released argo:uat/root (back on main). You now hold it."
- Sessions can also send explicit relays through `notify_session(slot, text)`. This is allowed for the coordinator, and for workers in reply to a relay.
- Every relay appears on the board strip ("14:05 told S3: UAT root back on main") and in both sessions' context panels, so Jonathan never relays a second time.
- **Delivery:** a message is typed into the target PTY, using bracketed paste for multi-line text, followed by Enter.
  - If the session is **idle**, it's delivered at once.
  - If it's **working**, it waits for the next idle.
  - If it's **needs you** because a permission or question prompt is showing, it **never** injects, because a keystroke could answer the prompt. It waits.
  - An **idle-input wait** (the `Notification` hook firing because the session has sat at its prompt) **is deliverable**.
  - If **Jonathan has typed into the input box** but not sent it, which shows up as a non-empty input line on screen, the message waits until the box is empty.
  - Queued messages show as pending on the card.

### 7.7 Coordinator (S0)

- S0 is a normal Claude session with a coordinator role prompt and extra MCP tools:
  - `get_board`: all slots, work items, owners, locks, waits, recent reports and relays.
  - `get_work_items`: the Jira and beads cache.
  - `propose_plan`: see below.
  - `notify_session`
  - `request_status(slot)`: asks a session for a short status, and its answer comes back through `report_progress`.
  - `set_board_summary(text)`
- **Plans.** `propose_plan` takes a structured plan: for each slot, the work items to assign, the targets to pre-assign, the worktree to use, the kickoff prompt, and collision rules.
  - The plan appears in the GUI for **Jonathan to approve, edit or reject**.
  - Once approved, the daemon assigns the items (applying the ownership rules, and refusing or warning as in 7.4), pre-claims targets, creates worktrees, launches any empty slots, and sends each kickoff into its terminal.
- **Board header.** The coordinator's latest `set_board_summary` is the board's headline. It is also refreshed when Jonathan asks S0 "what's going on?"
- **Manual assignment** without the coordinator uses a built-in kickoff template: owned items, held targets, worktree, rules, and pointers to the ticket folder and CURRENT_SESSION section.

### 7.8 2ndBrain writer (fixes all four daily symptoms)

The daemon becomes the **only committer** to `~/Repos/2ndBrain` and the **only writer of shared files**.

1. **Session-owned files** (its own `Sessions/...` note, its ticket folders under `Sprints/<sprint>/<TICKET>-*`) can be written directly by the session. Ownership follows the work-item ledger. **Committing** goes through the tool `commit_2ndbrain(paths, message)`.
   - The daemon serialises all git operations behind one mutex.
   - It stages **only the given paths** (pathspec commits, never `-A`).
   - It handles `index.lock`, pulls with rebase on push rejection, and pushes.
   - This fixes the git failures and the half-done edits being swept into someone else's commit.
2. **Shared files** (the daily journal, `SPRINT-DASHBOARD.md` and the sprint hub) are edited only through daemon tools, never directly:
   - `append_journal(section, text)`
   - `edit_shared_file(path, old_text, new_text)`: an exact-replacement edit, applied under a per-file lock against the current content. It fails with the current text if `old_text` is stale, never clobbers.
   - The `PreToolUse` hook (matcher `Edit|Write|MultiEdit` plus Bash redirects) **denies direct writes** to paths on the shared-files list and tells Claude which tool to use.
   - This fixes lost and overwritten content.
3. **`~/.claude/CURRENT_SESSION.md`** gets **one section per slot**, between markers (`<!-- overseer:S3 -->`). Sessions update only their own section through `update_current_session(text)`, and direct edits are denied. Each session's role prompt tells it to read only its own section unless asked otherwise.
   - This fixes cross-session confusion over state.
   - Migration: the existing 2,500-line file's history is kept below the slot sections, untouched.
4. **`git commit` and `git push` in 2ndBrain from a session are blocked** by the hook, with a message to use `commit_2ndbrain`.
5. **The post-commit auto-sync.** The daemon commits with the `post-commit` hook's background `sync-run.sh --auto` **suppressed**, through an env var the hook honours (`OVERSEER_COMMIT=1`). It then runs the sync itself, **once**, after its queue drains. This removes the auto-sync commit racing the next queued commit.
6. **Session-owned paths are enforced.** The hook denies writes, and `commit_2ndbrain` refuses paths, that belong to another slot's session note or ticket folder. Ownership comes from the work-item ledger. The Jira sprint maps to `Sprints/sprint-YY-NN/`. Paths nobody owns are allowed.

#### 7.8.1 Overseer takes over every other 2ndBrain writer (decided: full takeover)

Recon found three writers outside Claude that sweep up everything, plus hooks and skills that write outside the Bash tool. Jonathan approved Overseer taking all of them over on the work laptop.

| Writer today | What it does | Under Overseer |
|---|---|---|
| **Obsidian Git** | Commits all files (`autoCommitOnlyStaged: false`), pulls and pushes every 5 min | Auto-commit and auto-push **turned off** on the work laptop. Auto-pull can stay, but it runs through the daemon's lock, so the daemon does the pulling (setup checklist). |
| **`sync-run.sh --auto`** (`05-Meta/Sync-System/`) | `git add -A`, commit, push, called from `post-commit` | Honours `OVERSEER_COMMIT=1`, so it is a no-op inside daemon commits. The daemon runs it under its own lock after queue drains, with its `add -A` narrowed by the daemon running it only once there are no uncommitted session-owned paths outside the queue. |
| **Timed backup** | (Obsidian Git did this) | **The daemon** commits a "vault backup" of changes on a timer (default 5 min). These are only paths not owned by any active slot, or owned paths whose session is idle, so half-done edits are never swept. |
| **beads (`bd`) Dolt sync** | Syncs `.beads/` to `main` on origin | `bd` write and sync calls from sessions go through the daemon's git lock: the hook wraps them, and the daemon runs `bd sync` itself on its queue. |
| **Global hooks and skills** (`~/.claude/hooks/pai-autocompact-save.sh`, `hooks/lib/git-cmd.js`, session-compact, journal, land-the-plane and similar) | Commit 2ndBrain and write `CURRENT_SESSION.md` and the journal directly | Made **Overseer-aware**: when `OVERSEER_SLOT` is set, they route commits through `overseer ctl commit`, journal lines through `overseer ctl journal` and CURRENT_SESSION writes through `overseer ctl current-session`. With no `OVERSEER_SLOT` they behave exactly as now. Part of the Overseer work, and changed in their own repos with Jonathan's approval. |

`~/.claude/CURRENT_SESSION.md` lives outside the vault and isn't committed today. That stays the same, and the daemon is its only writer while sessions run under Overseer.

The daemon's single git mutex covers every git operation in 2ndBrain: session commits, backups, sync, pulls and bead syncs.

### 7.9 GUI

**Board** (main screen, layout "A tiles", picked from mockups):
- **Header:** sprint name and day, done count, and a "needs you" count.
- **Coordinator summary panel.**
- **A 3×2 grid of slot cards.** Each card shows:
  - slot and label
  - state, with time in that state
  - work-item pills (Jira) and bead pills
  - lock pills
  - the "now doing" line
  - pending relay count

  Card colours by state:
  - **needs you:** orange, the loudest
  - **blocked:** red
  - **working:** green
  - **idle:** grey
- **A bottom strip:** recent relays and the 2ndBrain queue status.
- **Assign:** empty cards offer "+ assign".

**Session view** (layout "A: terminal + context panel"):
- **Left:** the live terminal at full fidelity, where Jonathan types directly.
- **Right:** the context panel:
  - **Owns:** items and beads
  - **Holds:** locks and the worktree
  - **Progress:** a timeline of `report_progress` and state changes
  - **Relays**
  - **Key actions:** `m` message, `s` ask status, `a` assign more, `r` release lock, `n` open the 2ndBrain session note in Obsidian

**Navigation and other screens:**
- **Tab strip:** `0` to `6`, `Esc` back to the board. Keyboard shortcuts must not reach the terminal while it has focus, except through a deliberate modifier. The builder chooses the modifier; `Cmd+0..6` is suggested.
- **Plan approval view:** for coordinator plans.
- **Work-item picker:** see 7.4.
- **Locks view:** all targets, holders and waiters, with force release.

**macOS notifications** fire for **needs you** and for a denied write.

**Terminal must-haves (decided: all of them, so that Warp isn't missed):**
- full xterm-256 and truecolor
- Claude Code's TUI drawn correctly, including resize
- **Shift+Enter** sends a newline to Claude
- **image paste**: pasting a screenshot or image into Claude works as it does in iTerm2 and Warp
- **Option as Meta** for word jumps and edits
- **Cmd+click** opens URLs and file paths (file paths open in the editor)
- **mouse selection and copy**, including when the TUI has mouse reporting on (Shift-drag overrides)
- **scrollback search** (Cmd+F)
- configurable **font, size and theme**
- IME input
- bracketed paste

Phase 1 isn't done until each of these is checked against a real Claude session.

### 7.10 Storage

- **Ledger.** SQLite (`rusqlite`, bundled) at `~/Library/Application Support/Overseer/overseer.db`. It holds slots, sessions, work items cache, assignments and history, targets, claims, waits, relays, progress reports, events and the 2ndBrain queue.
- **Config.** TOML at `~/.config/overseer/config.toml` (paths, JQL, poll intervals, shared-files list) plus `targets.toml`.
- **Logs.** Under `~/Library/Logs/Overseer/`.

### 7.11 Build and install

- **Install.** `make install` on the work laptop:
  1. Installs Rust through `rustup` if it's missing (user-local, no admin).
  2. Builds release.
  3. Assembles `~/Applications/Overseer.app` with an ad-hoc signature (locally built, so no quarantine flag and no Gatekeeper prompt).
  4. Symlinks `~/.local/bin/overseer`.
  5. Installs the LaunchAgent.
- **Upgrades.** `git pull && make install`. It warns before restarting the daemon (sessions become resumable).
- **CI.** GitHub Actions on a macOS runner runs build, tests and clippy.

## 8. Testing approach

- **Unit tests:**
  - ownership rules (refuse, warn, move)
  - lock claim, wait and handoff
  - target pattern resolution against a corpus of real commands from Jonathan's session logs. The corpus stays **local, outside the public repo** (loaded from `~/.config/overseer/corpus/` and skipped in CI). CI runs against sanitised fixtures.
  - the `edit_shared_file` stale-detection logic
  - CURRENT_SESSION section handling
  - Jira and bead grouping
- **Integration tests** with a **fake `claude`** script that emits hook calls and TUI output, covering spawn, attach and reattach, state transitions, message delivery rules (never inject at a prompt), and daemon restart then resume.
- **2ndBrain writer tests** against a throwaway git repo with simulated concurrent sessions, Obsidian-style timed commits and a remote that rejects pushes.
- **End-to-end on the home Mac** with real `claude` sessions (Jira and Bitbucket stubbed if `acli` and `bb` aren't installed), then a live trial on the work laptop.
- **Terminal fidelity check:** run Claude Code through long sessions, plan mode and permission dialogs, with resize, scrollback and copy/paste.

## 9. Delivery order

1. **Core shell.** Daemon with PTY slots, GUI board and session view with an embedded terminal, attach and reattach, `overseer attach`. Usable as "Warp replacement with a board".
2. **Hooks and state.** Generated settings, live states, notifications, resume after restart.
3. **Ledger.** Work items from Jira and beads, picker, ownership rules, manual kickoff template.
4. **Locks.** Targets, `claim_target`/`release_target`, `PreToolUse` safety net, relays and delivery rules.
5. **2ndBrain writer.** Commit queue, shared-file tools and blocks, CURRENT_SESSION sections, sync suppression, timed backups, `bd` sync through the lock, Overseer-aware global hooks and skills, and a work-laptop setup checklist (Obsidian Git settings).
6. **Coordinator.** S0 role, board tools, plan approval flow.
7. **Polish.** Locks view, settings UI, install and upgrade.

## 10. Left to the builder

- The crate layout and internal API shape. Keep the terminal engine behind a trait so that `libghostty-vt` can be swapped in later.
- The daemon to GUI wire protocol over the Unix socket.
- How terminal state is mirrored to the GUI (daemon-side `alacritty_terminal` grid snapshots versus byte-stream replay into a GUI-side grid).
- The exact GPUI version to pin, and the component approach. Existing GPUI terminal components such as `gpui-terminal` can be used or borrowed from.
- The screen-scrape fallback for **needs you**.
- The exact default regexes in `targets.toml`, which are tuned from the corpus.
- Visual polish within the chosen layouts.
