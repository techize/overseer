# Owned item summary and detail view (issue #22)

Date: 2026-11-18
Issue: techize/overseer#22

## Intent

In a slot's session view, the right-hand context panel has an Owns box listing the tickets and beads the slot owns. Today each row shows the key, the status and a title cut off to one line, plus a Release button. When a coordinator has moved several tickets onto one slot (S6 owned 16 on 17 Nov), the keys alone don't say which ticket is which, and reading a ticket means leaving Overseer for Jira.

The change: show each item's title and status clearly under its key, and let a click on the row open the item's details (description, priority, stage, PR, child beads, folder) inside the panel, without leaving the session.

## Goals

1. Each Owns row shows the key and a status label on the first line, and the title below it, wrapped to at most two lines and then cut off with an ellipsis. No extra source calls: the data is already in `WorkItem`.
2. A click on an owned row replaces the panel's contents with that item's detail view. A Back control at the top returns to the normal panel. (The issue's "click again" is covered by Back, since the row is hidden while the detail is open.)
3. Ticket detail: the description as text (it holds the acceptance criteria in this user's Jira, so no separate field is read), status, priority, kanban stage and time in stage, the open PR when one is linked, the child beads (key, title, status), and the 2ndBrain folder path.
4. Bead detail: the description, the latest notes, the dependencies, status and priority.
5. The details are fetched when the item is opened, not in the regular polls. A result is cached in the daemon per key for 5 minutes. The detail view has a Refresh button that bypasses the cache.
6. If the source fails (Jira down, token missing, `bd` error), the view says what failed and still shows what Overseer has: title, status, priority, stage, PR, child beads, and the first paragraph of `00-PROJECT-OVERVIEW.md` from the ticket's folder when there is one.
7. The coordinator gets the same lookup as an MCP tool, `get_item_detail`.

## Non-goals

- Editing tickets or beads from the panel.
- A separate Jira acceptance-criteria field, or a setting to name one.
- Comments, attachments, images or the change history from Jira.
- Adding the description to the bulk polls.
- Showing the detail view anywhere other than the session panel (the sprint board's hover card is unchanged).

## Anti-goals

- Release must never fire from opening the detail. Release stays its own button on the row; only the rest of the row opens the detail.
- The regular Jira and bd polls must not get heavier or slower.
- An empty or spinning panel when the source is down. There is always something to read.
- No real ticket keys, sites, emails or workspace names in code, tests or docs. The repo is public: use DEMO-n, demo.atlassian.net, demo@example.test, demo-ws.

## Constraints

- Tickets come from one of two sources, chosen in `work/store.rs` (`JiraSource`): the REST API (`JiraApi`, when `jira_site` is set) or the configured `jira_command` (acli). The detail fetch must work with both, and say so plainly when Jira is `Off`.
  - API: `GET rest/api/3/issue/{key}?fields=summary,status,priority,description` with the same auth as `tickets()`.
  - Command: `acli jira workitem view <KEY> --fields summary,description,status,priority --json`. Only the program name (the first element of `jira_command`, normally `acli`) is reused; the arguments are fixed. When the command fails, the error line names the command that was run, so a user whose `jira_command` wraps acli in a script can see why. (Builder's call, marked: no check that the program is literally `acli`.)
  - acli is not installed on the build machine, so the exact `view --json` shape is not confirmed. The parser accepts the REST shape (`fields.description` etc. under `fields`) and a flat object (`description` at the top level), with the description as ADF or as a plain string, and fails soft to the fallback on anything else. (Builder's call.)
- Both return the description in Atlassian Document Format (ADF). A small converter turns ADF into plain text with simple structure: headings, paragraphs, bullet and numbered lists, code blocks, and inline text (links keep their text). Unknown node types fall back to their text content. A plain string description (older API versions, or acli printing text) is shown as is.
- Beads: `bd show <id> --json`, run through the configured `bd_command`, in the folder the bead lives in (the same folder the bead poll ran in for that item). `bd show --json` may print one object or an array of one; accept both. The first plan task captures real output from bd 1.0.2 in a throwaway repo (never in the user's notes folder) and saves it as a test fixture; the parser is written against that. "Latest notes" means the last 10 lines of the `notes` field. Dependencies are shown as ids, with the dependency type after the id when bd gives one. (Builder's calls.)
- Source commands run through `run_source` (environment cleared, timeout, process group), like the polls. The timeout is the existing source timeout.
- The fetch happens in the daemon. The GUI and MCP only send a request and render the reply.
- No em dashes in code, comments or docs. Commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. The PR says "Fixes #22".

## Approaches considered

- **A. Fetch on click (chosen).** A new daemon request fetches one item's details from its source when the row is opened, cached per key for 5 minutes, with the folder overview as a fallback. It is the only option that shows the real description without making the polls heavier. Cost: one request type, an ADF converter, and the GUI view.
- **B. Ticket folder only.** Show `00-PROJECT-OVERVIEW.md` and make no Jira call. Cheap and works offline, but many tickets have no folder and folders go stale, so it misses the main goal. Kept only as the fallback inside A.
- **C. Description in the bulk poll.** Instant on click, but every poll carries bodies that are rarely read, and the issue argues against it. Dropped.

The user picked A.

## Design

### Protocol (`overseer-proto`)

- `Request::GetItemDetail { key: String, refresh: bool }`.
- `Response::ItemDetail(ItemDetail)`.
- `ItemDetail`:
  - `key`, `kind` (`ItemKind`), `title`, `status`, `priority: Option<String>`, `stage: Option<Stage>`, `stage_since: Option<i64>`, `folder: Option<PathBuf>`: what the work cache already knows.
  - `body: Option<String>`: the description as plain text, `None` when not fetched.
  - `notes: Option<String>`: beads only, the latest notes.
  - `depends_on: Vec<String>`: beads only, dependency ids, `id (type)` when bd gives a type.
  - `children: Vec<ChildBead>` (`key`, `title`, `status`): beads under this ticket, from the cache.
  - `prs: Vec<PullRequest>`: every open PR linked to the ticket, from the cache, newest first. (Builder's call: a ticket can match more than one.)
  - `overview: Option<String>`: the first paragraph of the folder's `00-PROJECT-OVERVIEW.md`, filled only when `body` is `None` (the fallback).
  - `fetched_at: Option<i64>`: Unix seconds the body was fetched (from cache or now).
  - `error: Option<String>`: why the body could not be fetched, in plain words (for example `Jira did not answer: timed out after 20 s`, `Jira is off: no jira_site or jira_command`).
- A key that is not in the work cache and not owned gets an error response, not an `ItemDetail`.
- Owned items that have left the list (`in_list: false`), and placeholder tickets made up from bead titles, still get a fetch. If Jira says the issue does not exist, the error says `DEMO-1 was not found in Jira`. (Builder's call.)
- No protocol version bump. The GUI and daemon are installed together; if an older daemon rejects the request, the GUI shows its error in the detail view like any other failure. (Builder's call.)
- New fields use `#[serde(default)]` where an older peer could send the struct.

### Daemon

- A handler for `GetItemDetail` in the work store (or next to it), building `ItemDetail` from the cached item and adding the fetched body.
- Cache: `HashMap<String, (Instant, Fetched)>` with a 5-minute TTL. `refresh: true` skips the cache. A failed fetch is not cached.
- Ticket fetch per `JiraSource` as in Constraints. Bead fetch via `bd show <id> --json`. Parsing (`parse_issue_detail`, `parse_bead_detail`, `adf_to_text`) goes in `work/sources.rs` or a new `work/detail.rs`, as pure functions with tests.
- Overview fallback: read the folder's `00-PROJECT-OVERVIEW.md`, skip a front-matter block and the first heading, return the first non-empty paragraph, capped at about 600 characters.

### MCP (`crates/overseer/src/mcp.rs`)

- Tool `get_item_detail` with `key` (required) and `refresh` (optional), sending `GetItemDetail`. Returns the detail as readable text: key and title, status, priority and stage, then the body (or the error and the overview), then PR and child beads.
- Added to `ALLOWED_TOOLS` in `launch.rs` (17 entries) and the MCP tool list tests.
- Available to every slot, since reading a ticket is harmless; the coordinator is the main user.

### GUI (`crates/overseer-gui/src/session.rs`)

- Owns row layout: first line is the key (semibold) and a status label (muted); second line is the title, wrapped, at most two lines, then an ellipsis. The `gone` / `not read yet` notes stay as a small line under the title. Release stays on the right of the row.
- The row, apart from the Release button, is clickable and opens the detail for that key. The Release button stops the click from reaching the row.
- State: `detail: Option<DetailState>` per session view, holding the key, `Loading`, `Loaded(ItemDetail)` or `Failed(String)`. It is cleared when the slot tab changes, or when the item is no longer owned by the slot.
- Detail view, in place of the panel's boxes, scrolling inside the panel:
  - a top bar with Back, the key, and Refresh;
  - the title in full;
  - rows for Status, Priority, Stage (with time in stage), PR (one line per PR), Folder;
  - the body, rendered from the plain text: headings bold, list items indented, code blocks in the monospace font;
  - for a bead: Notes and Depends on;
  - for a ticket: Child beads, one line each;
  - when `error` is set: an orange line saying what failed, then the overview paragraph if there is one, under the cached fields.
- Very long bodies: the first 40 lines are shown with a "Show all" toggle.
- Loading: the cached fields show at once from the `WorkView`, with "Loading description..." where the body will go, so the panel is never empty.

## Acceptance (from the issue)

- Each owned row shows the title and status with no extra source calls.
- Clicking an owned ticket shows its description in the side panel within a couple of seconds, from cache after the first time.
- Clicking an owned bead shows its description and latest notes.
- If the source is down, the detail view says so and shows what it has (title, status, folder overview) instead of an empty panel.
- Release still works from the row and is not triggered by opening the detail.

## Left to the builder

- Exact module placement of the parsers and cache (`sources.rs` versus a new `detail.rs`).
- How the GUI stops the Release click from also opening the row, within gpui's event model.
- The exact cap and wording of the overview paragraph and error lines, within the shapes above.
- Whether `ItemDetail` reuses `HoverModel` pieces for the PR text, or formats it directly.
