# Sprint kanban and open PRs on the board: design

Monday 16 November 2026. Mockup: the "Overseer board with sprint kanban" canvas (private to Jonathan).

## Intent and why

Jonathan's aim every sprint is to finish it with every story done. Today the board shows sessions, not the sprint, so seeing where the sprint stands means opening Jira or the 2ndBrain dashboard. He wants a quick look on the board itself:

- how far through the sprint the work is
- which tickets are not started, in progress, in PR review, in testing, or waiting on something
- the open PRs and how old they are

Colours and small icons carry most of the meaning, so it reads at a glance.

## Goals

- A sprint kanban under the six session cards, shaped like Jira's: one column per stage, one small card per ticket.
- A sprint progress line above the columns: a bar coloured by stage, done count, a pace marker, open sprint breakers.
- An "Open PRs" panel in the top right of the board, beside the coordinator panel, with each PR's age.
- PRs linked to the tickets they name, so a ticket card shows its PR.
- All of it live from the sources the daemon already polls, plus one new PR source.

## Non-goals

- Changing tickets or PRs from the board (no drag between columns, no approve or merge).
- Reading the hand-written `SPRINT-DASHBOARD.md` (its "Next" and "blocked on" notes). Maybe later as a hover.
- Team-wide sprint views. The JQL stays Jonathan's own sprint tickets.
- GitHub PRs. The PR source is Bitbucket through `bb`; the command is a setting, so another tool can be plugged in later only if its JSON matches.

## Anti-goals

- A board that grows so tall the session cards scroll off a laptop screen. The kanban must stay compact (bounded card count per column).
- Colour alone carrying meaning: every colour also has an icon or a word.
- Stale data shown as current: when a source fails, the panel says so, as the header's source line does now.

## Constraints

- Rust workspace; GPUI front end (`crates/overseer-gui/src/board.rs`), daemon work store (`crates/overseer-daemon/src/work/`), wire types in `crates/overseer-proto`.
- Jira through `acli jira workitem search --jql {jql} --json` (already the default `jira_command`). Ticket fields today: key, summary, status name, priority name.
- Bitbucket through `bb pullrequest list --state open --output json`, which only works run inside a repo folder.
- Colours from `app.rs` `colors`; no new colour system.
- Tests never call real `acli` or `bb`; parsers are tested on sample JSON.
- Public repo: invented names in tests and docs.

## Approaches

1. **Daemon reads Jira and Bitbucket (picked).** Stage comes from the Jira status already polled; a new PR source runs `bb` per configured repo. Live, and stays in step with Jira.
2. **Read `SPRINT-DASHBOARD.md`.** Richer notes but hand-written markdown that changes shape and is only as fresh as its last update. Rejected.
3. **Option 1 plus dashboard notes on hover.** More moving parts; deferred until Jonathan misses the notes.

Jonathan asked for Jira-style cards after seeing the first sketch; approach 1 supplies all the data the mockup shows except the "waiting on X" reason (see below).

## Decided design

### Stages

Each ticket falls into one stage: To do, In progress, In review, Testing, Waiting, Done. The mapping from Jira status name to stage is six list settings, one per stage (`stage_todo`, `stage_in_progress`, `stage_in_review`, `stage_testing`, `stage_waiting`, `stage_done`), each a list of status names matched case-insensitively (my call: this reuses the existing list setting kind instead of adding a table kind). Defaults (my call, from statuses seen in Jonathan's sprints; adjust in settings):

- To do: Backlog, Prioritised Backlog, To Do, Selected for Development
- In progress: In Progress
- In review: In Review, Code Review, Review
- Testing: To Be Tested, Testing, In Testing, QA
- Waiting: Blocked, Waiting, On Hold
- Done: Done, Closed, Resolved

The tickets counted are the ones the header's `done` count already counts (tickets in the list, no beads, no placeholders), and that count moves onto the `stage_done` list so the header and the progress line always agree.

An unmapped status goes to In progress and the card shows the raw status name, so nothing disappears.

A ticket in To do or In progress that has an open PR linked to it shows in In review instead (my call: a PR in review is what Jonathan wants to see, even if Jira was not moved). This move is display only: the time-in-stage record keeps Jira's stage.

### Ticket cards

Each card shows:

- a priority chip: BREAKER (red, for the Jira priority "Sprint Breaker"), HIGH (orange, Highest and High), MED (grey, Medium), LOW (dim, Low and Lowest)
- the key, and the title clamped to two lines
- time in this stage: the daemon records when it first saw the ticket in its current stage and keeps that in the ledger, so it survives restarts (Jira's own change date is not in the search output today). A ticket the daemon has never seen move shows no age, since its first sighting is not when it entered the stage (my call)
- chips along the bottom: the owning slot (`S2`), the linked PR with its age coloured as below, the bead count

Waiting cards show "waiting" and their time waiting. The reason ("on DEMO-361") is not in the Jira search output, so it is left out.

Cards sort by priority, then by time in stage, oldest first. Each column shows at most 4 cards, then "+N more". Done shows only its count and the most recently finished key. Clicking a card opens the ticket's 2ndBrain folder, as the picker does; a ticket with no folder does nothing on click.

### Progress line

- A bar split by stage, Done first, in the stage colours.
- "6 of 41 done".
- Pace: expected done today = total x (sprint day / sprint days), from the existing `sprint_start` and `sprint_days`. When done is below expected, an orange "Behind pace: N expected done by today". A white tick on the bar marks today's expected point.
- With no `sprint_start` set, the tick and the pace warning are hidden.
- "N sprint breakers open" in red when any are not Done.

### Open PRs panel

- Top right of the board, the coordinator panel and S0 card to its left. Narrow windows wrap it below.
- One row per open PR, oldest first: an age dot, the repo and number, the linked ticket key, the title truncated, and the age (`5h`, `3d`).
- Ages are worked out by the GUI from the timestamps the daemon sends, so they tick on between polls.
- Age colour: green under 2 days, amber from 2 days, red from 5 days. The dot also has a word in its tooltip so colour is not the only signal.
- Shows up to 8 rows, then "+N more".
- Clicking a row opens the PR in the browser.

### PR source

- New settings: `pr_repos` (list of repo folders, default empty, which turns the source off), `pr_command` (default `bb pullrequest list --state open --output json`), `pr_poll_secs` (default 300), `pr_author` (empty for all open PRs in those repos, else only PRs by that author; my call: default empty).
- The daemon runs `pr_command` in each folder of `pr_repos`, through `run_source` with the same timeout and environment the Jira call uses, on its own timer set by `pr_poll_secs`. One repo failing does not drop the others' PRs.
- Parsing is lenient: an array, or an object with `values`. From each PR: `id`, `title`, `created_on`, `source.branch.name`, `links.html.href`, `author.display_name`. The repo name is the folder name. Missing fields are tolerated; a PR with no id is skipped.
- PRs that match no ticket still show in the panel; they just appear on no card.
- A PR links to a ticket when its title or branch matches `ticket_pattern`.
- The source shows in the header's source line (`prs 5 · 09:28`) and its errors like the others, per repo.

### Wire and storage

- `WorkView` gains `prs: Vec<PullRequest>` and a `sprint: Option<SprintBoard>` replacing today's `sprint: Option<String>` label (keeping the label inside it), with serde defaults so an older GUI and daemon still talk.
- The ledger gains a table for each ticket's current stage and when it entered it.

## Left to the builder

- Exact GPUI component split and sizes; keep the board fitting a 1440x900 window with the kanban visible.
- Whether stage colours are new constants in `colors` (testing purple `#d2a8ff` is the only new hue).
- Ledger migration details.
- Exact `bb` JSON shape beyond the fields above; build the parser so a sample from the work laptop can be dropped into a test.
