# Overseer for a second user: design

Tuesday 17 November 2026.

## Intent and why

Kasia wants to use Overseer. She uses the same Jira as Jonathan but has no Obsidian vault. Today Overseer assumes his machine:
- his name is in the session prompts
- the defaults point at his 2ndBrain vault and its layout
- Jira comes through `acli`
- sprint dates come from config keys and vault folders

The goal is for anyone on the team to install it, give it a Jira token, and have everything work, with Jonathan's own setup unchanged.

Jonathan's calls:
- **Same Jira, no vault.** That is the case to serve.
- **Settings instead of hard-coded assumptions.**
- **Jira through the API.** Use the scoped Atlassian API token approach from the 4 Nov work thread, not acli.
- **Overseer keeps its own notes folder, filled from Jira** ("own folder"), so the journal, session notes and ticket folders work without a vault. In his words, the notes folder "would be 2nd brain for us, but useable for anyone else not using obsidian and a 2nd brain": for Jonathan the notes folder is 2ndBrain, and for everyone else it is a plain folder Overseer creates and fills.

## Goals

1. **Jira by API.**
   - The daemon reads sprint tickets and the active sprint (name, start, end) straight from Jira's REST API, using a scoped API token, through `https://api.atlassian.com/ex/jira/<cloudId>`.
   - A rejected token shows as an error, never as "no tickets".
2. **Settings for everything personal.** These become config keys with neutral defaults, shown in Settings:
   - user name
   - Jira site
   - board
   - ticket pattern
   - token source
   - notes folder
   - worktree root
3. **Own notes folder.**
   - When there is no vault, Overseer creates and owns a notes folder with the same layout the features expect: `Sprints/<sprint>/<KEY>-<slug>/`, the daily journal and session notes.
   - It keeps the sprint and ticket folders in step with Jira.
4. **Jonathan's install keeps working with no config change.** That means his vault, acli if he keeps it, and his paths.
5. **A README a new user can follow**, plus a LICENSE and tidy docs.

## Non-goals

- Bitbucket through the API was a non-goal at first; it was added later, see "Addendum: Bitbucket by API".
- A prebuilt, notarised download. Kasia builds with `make install` like Jonathan.
- Writing to Jira, such as status changes or comments.
- Syncing into Jonathan's existing vault folders. His sprint-sync skill owns those.

## Anti-goals

- **A dead or wrong token that looks like an empty sprint.** This happened on 3 Nov, when a 401 was read as "no results" and nearly overwrote 34 cells. A 401 or 403 must be an error on the board. A 200 with zero tickets when the last poll had some must show a warning, not quietly empty the kanban.
- **Overseer writing into a vault it does not own.**
- **The token stored in plain text** in config.toml, logs, the ledger, or anything a session can read.
- **Breaking Jonathan's setup on upgrade.**

## Approaches

1. **A native HTTP client in the daemon (picked).** The daemon calls the Jira REST API itself, with a small blocking client run on a worker thread, as the command sources run today. One code path for everyone, with errors it can classify (401 against an empty result).
2. **Keep shelling out, but to curl.** Rejected: it gives brittle error classification, and the token ends up on a command line.
3. **Keep acli and only document it.** Rejected: Jonathan asked for the API, and acli's error handling is what caused the 3 Nov scare.

`jira_command` (acli) stays as a fallback source for anyone who sets it. The API is used when `jira_site` is set.

## Decided design (my calls unless marked)

### Jira API

**Config:**
- `jira_site` (for example `demo.atlassian.net`)
- `jira_email`
- `jira_board` (the board id for sprints)
- optional `jira_cloud_id`

**Cloud id.** When `jira_cloud_id` is unset, the daemon reads it once from `https://<jira_site>/_edge/tenant_info` and logs it.

**Requests.** All go to `https://api.atlassian.com/ex/jira/<cloudId>/`, never to the site host, which gives 200 with 0 issues for scoped tokens:
- **Tickets.** `POST rest/api/3/search/jql` with the existing `jql`. It requests the fields the work store needs: key, summary, status, priority, parent, issuetype, plus the story points field if configured. It pages through every result.
- **Active sprint.** `GET rest/agile/1.0/board/<board>/sprint?state=active` gives the name, startDate and endDate. These replace `sprint_name`, `sprint_start` and `sprint_days`, and the sprint name from vault folders. The old keys still win when set, as overrides.

**Auth.** Basic auth with email and token. The token is looked up in this order:
1. The macOS Keychain, as a generic password with service `overseer-jira` and the email as account.
2. The `ATLASSIAN_API_TOKEN` environment variable, from the captured login-shell environment.

Settings gets a "Jira token" field that writes the Keychain item through `security add-generic-password -U` and never displays the token afterwards. The token never goes to config.toml, logs, the ledger or session prompts.

**Errors** are shown as the Jira source's message, as source errors are today:
- 401 or 403: "Jira token rejected (401): check the token and its scopes".
- Other non-2xx responses: the status and the first line of the body.
- Network failure: the error.
- 200 with zero tickets after a poll that had some: keep the previous tickets and show "Jira returned no tickets; keeping the last list". After two such polls in a row, accept the empty result.

**Scopes.** The README lists the scopes the token needs: read:jira-work, read:sprint:jira-software and read:board-scope:jira-software.

### Settings for personal values

- **`user_name`.** Defaults to the first word of the macOS full name (`id -F`), else `$USER`.
  - It replaces "Jonathan" in the role prompts (`roles/*.md` become templates with `{user}`).
  - It also replaces it in daemon messages to sessions (`coord/plan.rs`, `locks/table.rs`, `relay/courier.rs`).
  - Code comments may keep his name.
- **`ticket_pattern`.** The default becomes a generic `[A-Z][A-Z0-9]+-\d+`. Jonathan's PLAT tickets still match.
- **`worktree_root`.** The default stays `~/Repos/.worktrees`.
- **Start dialog cwd.** The default becomes the notes folder when it exists, else home.
- **`sync_command`.** The default becomes empty, and it runs only when set.
  - Compatibility: for the vault case below, the old default applies when the script exists.
- **Settings view.** Gains rows for the new keys. The text "2ndBrain" in the board strip, notifications and Settings becomes "Notes", or the notes folder's name.

### Notes folder

**`notes_dir`** replaces `brain_dir` (`brain_dir` stays as an alias). Default:
- `~/Repos/2ndBrain` if that folder exists (Jonathan's case, unchanged).
- Otherwise `~/OverseerNotes`, created on first use. (Not `~/Overseer`: on a case-insensitive volume that is the source clone at `~/overseer`.)

`beads_dir` defaults to the notes folder as today. A missing `bd` or no beads data is not an error for a folder Overseer owns.

**Ownership.**
- Overseer owns a folder only if it holds a `.overseer-notes` marker file, which it writes when it creates the folder.
- A folder without the marker, such as Jonathan's vault, is never synced into. Today's behaviour stays: Overseer reads it, and the journal and session notes are written only if `brain_writer` is on.

**Sprint sync, owned folder only.**
- After each Jira poll, Overseer makes sure `Sprints/<sprint name>/` exists, plus a `<KEY>-<slug>/` folder for each sprint ticket.
- Each ticket folder holds a generated `ticket.md` with the key, title, status, priority, Jira link and last sync time. Overseer rewrites only that file. Anything else the user or a session adds there is never touched.
- Tickets that leave the sprint keep their folders.
- The kanban's folder links and the "Ticket folders" lines in kickoffs then work as they do for Jonathan.

**Journal and session notes, owned folder.**
- `brain_writer` defaults to on, using the existing `journal_path` and `session_note` templates.
- There is no push and no sync_command unless configured.
- The `current_session` file defaults to `<notes_dir>/CURRENT_SESSION.md` when the notes folder is owned, rather than `~/.claude/CURRENT_SESSION.md`.

### Docs and repo

- **README.** A new top section, "Getting started", covering:
  - what Overseer is, and what S0 and S1 to S6 are
  - requirements: macOS 13 or later, Xcode command line tools, Claude Code, a scoped Atlassian API token with the listed scopes, and optionally `bb` and `bd`
  - `git clone https://github.com/techize/overseer.git` and `make install`
  - setting the token in Settings
  - a starter `config.toml` for "same Jira, no vault"
  - what each optional tool adds
  - troubleshooting: token rejected, no tickets, notifications

  The existing sections stay below as reference, updated for the new keys.
- **LICENSE.** An MIT LICENSE file with copyright Jonathan Gill.
- **Docs tidy.** `docs/acceptance/` and `docs/setup/` move to `docs/internal/` with a one-line README saying they are the maintainer's hand checks.
- **.gitignore** gains `.claude/`.

## Settled after the builder check (my calls)

- **Git in the owned folder.** When Overseer creates its notes folder it runs `git init` there and commits locally, so the existing journal and session-note writer works unchanged and the user gets history. There is no remote and no push. The sprint sync writes (`ticket.md`, sprint folders) go through the same commit queue as the other notes writes.
- **Safeguards on both Jira paths.** The error classification and the "keep the last list" rule apply to acli as well as the API. Jonathan stays on acli until he sets `jira_site`, and the 3 Nov failure was on acli. The source is picked like this: the API if `jira_site` is set, else `jira_command`, whose default stays acli. A new user without acli sees a clear "acli not found: set jira_site" error, and the README tells them to set `jira_site`.
- **Fields.** Only the fields `WorkItem` already has are filled. Jira's parent (the epic) is not put into `WorkItem.parent`, because that field groups beads. Issue type and story points are not fetched. Showing them on the board is follow-up.
- **Which sprint.**
  - `jira_board` is optional. When it is unset, sprint name and dates come from the old config keys as today, and the sprint sync is skipped.
  - When several sprints are active, the one with the latest start date is used.
  - When no sprint is active, there is no sprint header and no sync, and the board shows "No active sprint".
  - `sprint_days` is worked out from the start and end dates.
- **Sprint folder name.** `sprint-<slug>`, where the slug is the Jira name in lower case with anything other than letters and digits turned into `-`. A name that already starts with "sprint" is not prefixed twice. This keeps `newest_sprint_folder` and the `Sprints/*/sprint-*.md` locking default working.
- **Ticket pattern.** No fixed default. The pattern is built from the project keys of the tickets Jira returns (for example `(PLAT)-\d+`), so Jonathan gets `PLAT-\d+` as today, Kasia gets her project's keys, and strings like `UTF-8` don't match. A `ticket_pattern` in config still overrides it. Before the first poll, and with no tickets, the generic `[A-Z][A-Z0-9]+-\d+` is used.

## Left to the builder

- The HTTP crate. It should be small and use rustls, for example `ureq`.
- The JSON field mapping from Jira's response into `WorkItem`.
- Paging details.
- How the Keychain is called. The `security` CLI or a crate are both fine.
- Where the sprint-sync code lives.
- Exact Settings layout.
- Whether `brain_*` names are renamed in code or only in config.

## Addendum: Bitbucket by API (added 17 Nov 2026)

Jonathan's call, after looking at Teamwork Graph: make the app self-sufficient, so PRs come from Bitbucket's REST API instead of the `bb` tool. Teamwork Graph's `twg` CLI was considered and rejected for the daemon. It is another installed tool, it still needs a separate Bitbucket token, and its browser sign-in suits a person at a terminal, not a daemon running for days. It may still be useful inside sessions, as follow-up.

### Approaches

1. **Direct REST calls from the daemon (picked).** This uses the same `ureq` client and Keychain pattern as Jira, and one code path for everyone.
2. **`twg bb` as the PR command.** Rejected for the reasons above.
3. **Keep `bb` and document it.** Rejected, because it leaves the app depending on an installed tool.

### Design

- **Token.** A scoped Atlassian API token for the Bitbucket app, with the scopes `read:repository:bitbucket` and `read:pullrequest:bitbucket`. It is a separate token from Jira's, because a scoped token covers one app.
  - It is stored in the Keychain with service `overseer-bitbucket`, and the account is `jira_email`, the Atlassian account email.
  - The env fallback is `BITBUCKET_API_TOKEN`.
  - Auth is basic, with email and token.
- **When the API is used.** The API is used when a Bitbucket token is found. Otherwise `pr_command` (`bb`) is used, as today, so Jonathan's setup is unchanged until he saves a token.
  - **Error handling:**
    - A 401 or 403 is an error on the board: "Bitbucket token rejected (401): check the token and its scopes".
    - Any other non-2xx gives the status and the first line of the body.
    - Errors never carry the token.
- **Requests,** to `https://api.bitbucket.org/2.0/`:
  - **Repo list for `pr_workspace`:** `GET repositories/{workspace}?pagelen=100&sort=-updated_on`, plus `q=project.key="{pr_project}"` when `pr_project` is set. It follows `next`, and the result feeds the existing `parse_repos`.
  - **Open PRs per repo:** `GET repositories/{workspace}/{slug}/pullrequests?state=OPEN&pagelen=50`. It follows `next`, and the result feeds the existing `parse_prs`. The response shape is the one `bb --output json` already passes through.
  - **`pr_repos` folders.** The workspace and slug come from the folder's git remote (`origin` first). Both the `https://` and `git@bitbucket.org:` forms are read. A folder with no Bitbucket remote gives a per-repo error.
- **Parallelism, timeouts and keep-last-good** stay as they are for the PR source: `PR_PARALLEL`, `SOURCE_TIMEOUT` and the repo list cache.
- **Settings.** A "Bitbucket token" row works like the Jira one: write-only, showing set or not set. `Request::SetJiraToken` becomes a general token request that names which token it sets.
- **README.** It covers making the Bitbucket token, its scopes and the Settings step, and says `bb` is now optional.

### Not in scope

- Creating or merging PRs.
- Bitbucket Data Center.
- OAuth.
- Using Teamwork Graph in sessions.
