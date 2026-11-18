# Item detail follow-ups (issue #48)

Date: 2026-11-18
Issue: techize/overseer#48 (follow-ups from the #22 review, PR #47)

## Intent

The owned-item detail view (#22) shipped with a list of review leftovers. None blocks the feature, but together they make the view truer to the source, harder to break, and better tested. The user also wants the coordinator (S0) to plan properly: read each candidate ticket in full, acceptance criteria included, before it assigns work.

Since #22, acli is installed and signed in on the user's machine, so real acli output could be checked. The user's config leaves `jira_site` and `jira_command` unset, so acli is the Jira source Overseer actually uses. bd 1.0.2 output was captured in a scratch repo.

## Goals

### Daemon

1. **Refresh failure keeps the good copy.** When `refresh: true` fails and the cache still holds an entry for the key, reply with that cached body/notes/dependencies, its original `fetched_at`, and `error` set to the failure. With no cache entry, behave as today: error plus overview.
2. **Overview whenever the body is empty.** Fill `overview` whenever the final `body` is `None` or blank, not only when the fetch failed.
3. **Key validation.** Before any fetch, reject a key that does not match `^[A-Za-z0-9][A-Za-z0-9._-]*$` with the error `DEMO-1 is not a valid item key` (with the key in place of DEMO-1). Percent-encode the key in the REST path. Keys are already checked against known items; this is extra protection.
4. **Cache housekeeping.** Remove expired entries when inserting. Two requests for the same key at the same time share one fetch (single-flight): the second waits for the first and gets its result. A `refresh` request does not join a fetch that started without refresh. (Builder's call.)
5. **acli not-found wording.** acli 1.3.41 answers an unknown key with exit 1 and stderr `✗ Error: Issue does not exist or you do not have permission to see it.` When stderr contains `Issue does not exist`, the error is `DEMO-1 was not found in Jira`, the same as the REST path.
6. **bd dependency types.** Real `bd show --json` (bd 1.0.2) prints an array of one object, and `dependencies` is an array of objects with `id`, `title`, `status`, `priority`, `issue_type`, timestamps and `dependency_type` (for example `"blocks"`). Show each as `id (dependency_type)`. Keep accepting plain strings and objects without a type.
7. **Undecodable requests.** When the daemon reads a frame it cannot decode as a request but can read a `seq` from, it answers that `seq` with `Response::Error("the daemon does not understand this request; restart the daemon to finish an upgrade")` (wording builder's call within that sense) and keeps the connection open. A frame with no readable `seq` still closes the connection as today.

### GUI

8. **Refresh keeps the text.** While a refresh is in flight, keep showing the loaded detail with "Refreshing..." in place of the Fetched age. The request counter from #22 still drops stale replies.
9. **Child beads while loading or failed.** The Loading and Failed states show the child beads from the `WorkView` (items whose parent is the key), and PRs newest first, like the Loaded state.
10. **Live fields.** While a detail is open and the item is listed (`in_list`), status, priority, stage, time in stage and PRs come from the live `WorkView`, so they follow polls. Body, notes and dependencies stay from the reply.
11. **Show less only when it does something.** Offer Show all / Show less only when the untruncated body is longer than 40 lines.
12. **Empty title.** When the title is empty, show the key as the title. The same goes for the Owns row.
13. **Bullets.** No empty bullets: a list item whose first child is not a paragraph writes no lone marker (fix in `adf_to_text`, and drop empty bullets in `body_lines` too). Bullet indent becomes `8 + 12 * depth` px (builder may tune by eye).
14. **Build mismatch wording and timing.** Compare builds by their timestamp suffix (`<commit>-<yyyymmddHHMM>`): when the daemon's is older, say to restart the daemon; when it is newer, say to restart Overseer (the app). When either is unparseable, use a neutral "the app and the daemon are different builds; restart both". While the daemon's build is not yet known, a click waits for the build probe (or its timeout) before deciding, rather than sending. `DEV_BUILD` on either side still counts as a match, as today.

### Coordinator

15. **S0 reads tickets.** `coordinator.md` tells S0 to read every candidate ticket in full with `get_item_detail` while planning (description and acceptance criteria), and to assign on what the work actually is, not on titles. The line "Do not read Jira yourself" becomes: read tickets through `get_item_detail`, never by calling Jira or acli directly. The current text goes in a new fixture `tests/fixtures/roles-before-detail/coordinator.md`, and its hash joins `PREVIOUS_COORDINATOR` so unedited installs upgrade.

### Tests

16. Ordered lists, deep headings (h4 to h6), string dependencies, an anonymised fixture of the real acli view shape (REST layout: `key`, `fields.summary`, `fields.description` as ADF, `fields.status.name`, `fields.priority.name`, plus null top-level keys such as `changelog` and `renderedFields`), an anonymised fixture of the real bd show shape, the acli not-found mapping, and an MCP integration test with a successful `get_item_detail` round trip against a stub daemon or stub source.

## Non-goals

- New fields in the detail view (comments, attachments, history).
- Changing the polls.
- Changing which Jira source the user's config uses.

## Anti-goals

- Real ticket keys, sites, emails or names anywhere in the repo. Fixtures built from captured output are rewritten with DEMO-n, demo.atlassian.net, demo@example.test.
- A refresh that leaves the panel emptier than before it was pressed.
- An unknown request that drops the connection when it could have been answered.
- S0 bypassing Overseer to talk to Jira directly.

## Constraints

As #22: source commands through `run_source`; no em dashes in code, comments or docs; commits end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`; the PR says "Fixes #48". Never run the real `security` binary against the user's Keychain in tests.

## Approaches

- **One branch, one PR (chosen).** Every item touches the same few files (`store.rs`, `detail.rs`, `session.rs`, `app.rs`, `coordinator.md`), so splitting would mean rebasing the same hunks repeatedly.
- **One PR per group** (daemon, GUI, coordinator). Smaller reviews, but conflicts between them and four rounds of install for the user. Not worth it at this size.

## Left to the builder

- The single-flight mechanism (a map of in-flight keys to a shared future or watch channel).
- Exact wording of the build-mismatch messages within goal 14.
- How the undecodable-request path reads `seq` (decode the frame as a loose JSON value first).
