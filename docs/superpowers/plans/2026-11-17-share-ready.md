# Overseer for a Second User Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Anyone on the team can install Overseer, give it a Jira token and use it with no vault, while Jonathan's setup keeps working with no config change.

**Architecture:**
- The daemon gains a blocking Jira REST client, run on a blocking thread like the command sources. It is used when `jira_site` is set.
- Config resolution decides the notes folder: Jonathan's 2ndBrain when present, else an Overseer-owned `~/Overseer`, which is created, marked and git-initialised.
- After each Jira poll, the owned folder gets sprint and ticket folders. Personal values become settings.

**Tech Stack:** Rust 2021 workspace, tokio, GPUI 0.2.2, toml/toml_edit, `ureq` 2 (rustls), the macOS `security` CLI.

**Spec:** `docs/superpowers/specs/2026-11-17-share-ready-design.md`

## Global Constraints

- **Wording:** no em dashes anywhere (code, comments, docs, commits). No emojis in commits.
- **Invented names only:** in tests, fixtures and docs, use `DEMO-n`, `demo.atlassian.net`, `demo@example.test`. This is a public repo.
- **Token:** it never goes to config.toml, logs, the ledger, session prompts or a process's argv.
- **Keychain item:** a generic password, service `overseer-jira`, account = `jira_email`. The env fallback is `ATLASSIAN_API_TOKEN`.
- **API base:** every Jira request goes to `https://api.atlassian.com/ex/jira/<cloudId>/...`, never to the site host. Only the cloud id lookup uses `https://<jira_site>/_edge/tenant_info`.
- **Rejected token:** the source message is `Jira token rejected (<status>): check the token and its scopes` for 401 and 403.
- **Empty guard:** a 200 with zero tickets after a poll that had some keeps the previous tickets, with the message `Jira returned no tickets; keeping the last list`. The second empty poll in a row is accepted. This applies to both the acli and API paths.
- **Owned-folder marker:** the file is `.overseer-notes`. A folder without it is never synced into.
- **Generated ticket file:** `ticket.md` is the only file Overseer writes in a ticket folder.
- **Sprint folder name:** `sprint-<slug>`. The slug is lowercase, with every run of characters other than ASCII letters and digits turned into a single `-`, and no `-` at either end. If the slug already starts with `sprint`, the prefix is not added again.
- **Ticket pattern:** an empty `ticket_pattern` means derive it.
  - Built from the project keys of the last Jira list, `(K1|K2)-\d+`, with the keys sorted.
  - Before any list, or with an empty list: `[A-Z][A-Z0-9]+-\d+`.
- **`user_name` default:** the first word of the macOS full name (`id -F`), else `$USER`, else `you`.
- **Jonathan's install:** with no config file and `~/Repos/2ndBrain` present, every existing config value resolves as today, except `ticket_pattern`, which becomes derived.
- **Final checks:** `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check` must all pass after each task.

## Review Focus

1. **Keychain read hangs.** If `security` blocks on an unlock or access prompt, the Jira poll must time out with a clear message ("Keychain did not answer"), not stall the source. Task 3 tests a stub `security` that sleeps.
2. **Odd Jira text in folder names.** A sprint name or ticket title with `/`, `..`, emoji or 200 characters must never escape `Sprints/` or make an unusable path. Slugs are capped at 50 characters. Task 5 tests `../evil/Sprint 1 🚀`.
3. **User files in ticket folders.** A `notes.md` that the user or a session put in a ticket folder must survive every sync. Task 5 tests it.
4. **The network drops mid-sprint.** A connect error keeps the last tickets and sprint header. It is not treated as an empty list. Task 4 tests it.
5. **No config at all, with 2ndBrain present.** The resolved config must equal today's defaults, apart from `ticket_pattern`. Task 1 tests it.

---

### Task 1: Config keys and default resolution

**Files:**
- Modify: `crates/overseer-daemon/src/config.rs`
- Modify: `crates/overseer/src/main.rs:98-101` (pass the full name)

**Interfaces:**
- Produces, as new `Config` fields:
  - `user_name: String`
  - `jira_site: String`
  - `jira_email: String`
  - `jira_board: String` (empty means unset)
  - `jira_cloud_id: String`
  - `#[serde(skip)] notes_owned: bool`
- `brain_dir` also accepts `notes_dir` (`#[serde(alias = "notes_dir")]`). Code keeps the name `brain_dir`.
- `pub fn Config::load_with(path: &Path, home: &Path, full_name: Option<&str>) -> (Config, Option<String>)`
- `Config::load(path, home)` calls `load_with` with `macos_full_name()`.
- `pub fn macos_full_name() -> Option<String>` runs `id -F` and trims the result. It returns `None` on any failure.
- `pub const OWNED_MARKER: &str = ".overseer-notes";`

**Resolution rules.** These apply only to keys absent from the file. Detect absence with a `toml::Table`, as `fold_legacy_stage_keys` does.
- `brain_dir`:
  - `~/Repos/2ndBrain` if that directory exists.
  - Else `~/Overseer`.
- `notes_owned`:
  - true if `brain_dir/.overseer-notes` exists, or if `brain_dir` was defaulted to `~/Overseer`.
  - A set `brain_dir` holding the marker is owned too.
- `beads_dir` defaults to the resolved `brain_dir`.
- When `notes_owned`:
  - `brain_writer` defaults to true.
  - `push` defaults to false.
  - `current_session` defaults to `<brain_dir>/CURRENT_SESSION.md`.
- `ticket_pattern` defaults to `""`.
- `user_name` follows the Global Constraints rule. It uses `$USER` from the process env.

- [ ] **Step 1: Write the failing tests** in `config.rs` `mod tests`:
  - `no_file_with_2ndbrain_is_todays_setup`. A temp home containing `Repos/2ndBrain/` and `load_with(missing, home, Some("Demo Person"))` give:
    - `brain_dir == home/Repos/2ndBrain`
    - `notes_owned == false`
    - `brain_writer == false`
    - `push == true`
    - `current_session == home/.claude/CURRENT_SESSION.md`
    - `jira_command[0] == "acli"`
    - `ticket_pattern == ""`
    - `user_name == "Demo"`
  - `no_file_without_vault_owns_overseer_folder`. A temp home with nothing gives:
    - `brain_dir == home/Overseer`
    - `beads_dir == home/Overseer`
    - `notes_owned`
    - `brain_writer`
    - `!push`
    - `current_session == home/Overseer/CURRENT_SESSION.md`
  - `notes_dir_alias_and_marker`. `notes_dir = "~/n"`, with `home/n/.overseer-notes` present, gives `brain_dir == home/n` and `notes_owned`. With no marker, `!notes_owned` and `brain_writer == false`.
  - `explicit_keys_win`. A file setting `brain_writer = false`, `push = true` and `ticket_pattern = "DEMO-\\d+"` in an owned folder keeps all three.
  - `user_name_fallbacks`. `full_name` `None` gives `$USER`. A set `user_name` key wins over both.
  - Update `missing_file_is_defaults` and the other existing tests that assert `PLAT-\d+` or `~/Repos/2ndBrain`, so they create the folders they rely on.
- [ ] **Step 2:** Run `cargo test -p overseer-daemon config::` and expect the new tests to fail.
- [ ] **Step 3:** Implement the fields, `load_with`, the resolution and `macos_full_name`. `main.rs` keeps calling `Config::load`.
- [ ] **Step 4:** Run `cargo test -p overseer-daemon config::` and expect a pass. Then run the gates from Global Constraints. Fix other crates' tests that relied on `PLAT-\d+` by setting `ticket_pattern` explicitly in their config.
- [ ] **Step 5:** Commit: `Resolve the notes folder and personal settings from what is on the machine`.

### Task 2: The owned notes folder

**Files:**
- Create: `crates/overseer-daemon/src/notes.rs` (and add `pub mod notes;` to `lib.rs`)
- Modify: `crates/overseer/src/main.rs` (call it before `run`)

**Interfaces:**
- Consumes: `Config.notes_owned`, `Config.brain_dir`, `OWNED_MARKER` (Task 1).
- Produces: `pub fn ensure_owned(dir: &Path) -> std::io::Result<()>`. It:
  - creates `dir` and `dir/Sprints`
  - writes the marker, with the one line `This folder is kept by Overseer. Delete this file to stop Overseer writing sprint and ticket folders here.`, if it is missing
  - if `dir/.git` is missing, runs `git init -q -b main` and makes a first commit of the marker. The commit uses `-c user.name=Overseer -c user.email=overseer@localhost` only when git has no identity configured.
  - It never touches a folder that exists without the marker and has other entries in it. In that case it returns `Err(io::ErrorKind::AlreadyExists)` with the message `<dir> exists and is not an Overseer notes folder`.
- In `main.rs`:
  - When `settings.notes_owned`, call `ensure_owned(&settings.brain_dir)`.
  - On error, log a warning and set `brain_writer = false` and `notes_owned = false` for this run.

- [ ] **Step 1: Write the failing tests** in `notes.rs`:
  - `creates_marks_and_inits`. A missing dir results in the marker, `Sprints/`, `.git/`, and `git log --oneline` showing 1 commit.
  - `second_call_changes_nothing`. A second call leaves `git rev-parse HEAD` the same.
  - `refuses_a_foreign_folder`. A dir holding `a.md` and no marker gives an `AlreadyExists` error and no marker is written.
  - `adopts_an_empty_folder`. An existing empty dir becomes owned.
- [ ] **Step 2:** Run `cargo test -p overseer-daemon notes::` and expect a fail.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run the tests and the gates.
- [ ] **Step 5:** Commit: `Create and mark Overseer's own notes folder`.

### Task 3: Jira REST client and token lookup

**Files:**
- Create: `crates/overseer-daemon/src/work/jira_api.rs`
- Create: `crates/overseer-daemon/src/work/token.rs`
- Modify: `crates/overseer-daemon/src/work/mod.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (add `ureq = { version = "2", features = ["json"] }`)

**Interfaces:**
- Consumes: `Ticket` and `parse_jira` (`work/sources.rs`), and `Ymd` (`work/sprint.rs`).
- Produces in `jira_api.rs`:
  - `pub struct ActiveSprint { pub name: String, pub start: Ymd, pub end: Ymd }`
  - `pub struct JiraApi { /* site, email, board, api_base, cloud_id: Mutex<Option<String>> */ }`
  - `pub fn JiraApi::new(site: &str, email: &str, board: &str, cloud_id: &str) -> JiraApi`. `api_base` is `https://api.atlassian.com`, and `site_base` is `https://<site>`.
  - `#[cfg(test)] pub fn with_bases(self, api_base: String, site_base: String) -> JiraApi`
  - `pub fn tickets(&self, token: &str, jql: &str) -> Result<Vec<Ticket>, String>`, which is blocking:
    - It sends `POST {api}/ex/jira/{cloud}/rest/api/3/search/jql` with the body `{"jql":…,"fields":["summary","status","priority"],"maxResults":100}`.
    - It follows `nextPageToken` until `isLast` is true or the token is absent, with at most 20 pages.
    - It parses each page with `parse_jira`.
  - `pub fn active_sprint(&self, token: &str) -> Result<Option<ActiveSprint>, String>` sends `GET .../rest/agile/1.0/board/{board}/sprint?state=active`. It returns `Ok(None)` when the board is empty.
  - `pub fn cloud_id(&self) -> Result<String, String>` uses the configured value, else `GET {site}/_edge/tenant_info` → `cloudId`. It caches the answer and logs `jira cloud id <id>` once.
  - `pub fn classify(status: u16, body: &str) -> String`. 401 and 403 give the Global Constraints text. Anything else gives `Jira answered <status>: <first line of body, at most 200 chars>`.
  - `pub fn parse_active_sprint(json: &str) -> Result<Option<ActiveSprint>, String>` reads `values[]` with `startDate` and `endDate` as ISO times, using the date part. It picks the latest start and skips entries without both dates.
- Produces in `token.rs`:
  - `pub async fn jira_token(email: &str, vars: &[(OsString, OsString)], security: &str) -> Result<String, String>`
  - It runs `<security> find-generic-password -s overseer-jira -a <email> -w` through `run_source` with `SOURCE_TIMEOUT`. On a timeout the message is `Keychain did not answer`.
  - If it finds nothing, it falls back to `ATLASSIAN_API_TOKEN` in `vars`.
  - If both are absent: `no Jira token: set it in Settings or ATLASSIAN_API_TOKEN`.
  - `security` is `/usr/bin/security` in production and a stub path in tests.
  - `pub async fn save_jira_token(email: &str, token: &str, security: &str) -> Result<(), String>`
    - It runs `<security> -i` and writes `add-generic-password -U -s overseer-jira -a "<email>" -w "<token>"\n` to its stdin, escaping `"` and `\`.
    - It checks the exit status. The token never goes into argv.
- Auth is `Authorization: Basic base64(email:token)`. Use `ureq`'s request headers and add no extra base64 crate if ureq exposes one. If it does not, add `base64 = "0.22"`.

- [ ] **Step 1: Write the failing tests:**
  - `classify_rejected`. `classify(401, "x")` and `classify(403, "")` give the exact Global Constraints text. `classify(500, "boom\nmore")` gives `Jira answered 500: boom`.
  - `active_sprint_picks_latest_start`. A fixture with two active sprints, plus one without dates, returns the later one with `Ymd` start and end.
  - `paging_follows_next_token`. A `std::net::TcpListener` fake server answers page 1 with `nextPageToken:"p2"` and page 2 with `isLast:true`. The result is 3 tickets, the second request carries `"nextPageToken":"p2"` in its body, and every path starts `/ex/jira/demo-cloud/`.
  - `unauthorised_is_an_error_not_empty`. The fake returns 401, and `tickets()` gives `Err` with the rejected text.
  - `token_from_keychain_then_env`. A stub `security` script printing `tok1` gives `tok1`. A stub exiting 44 with `ATLASSIAN_API_TOKEN=tok2` in vars gives `tok2`. Neither gives the "no Jira token" error.
  - `keychain_timeout_is_reported`. A stub that sleeps past the timeout gives an error containing `Keychain did not answer`. Make the timeout a parameter in a `pub(crate)` inner function, so the test uses 200 ms.
  - `save_token_never_in_argv`. A stub `security` writes its argv and stdin to files. argv is exactly `-i`, and stdin holds the escaped token.
- [ ] **Step 2:** Run `cargo test -p overseer-daemon jira_api:: token::` and expect a fail.
- [ ] **Step 3:** Implement it. Call the blocking functions from async code through `tokio::task::spawn_blocking`, with a 60 s overall timeout.
- [ ] **Step 4:** Run the tests and the gates.
- [ ] **Step 5:** Commit: `Read Jira tickets and the active sprint through the REST API`.

### Task 4: Store: pick the source, guard empty lists, sprint from Jira, derived pattern

**Files:**
- Modify: `crates/overseer-daemon/src/work/store.rs`
- Modify: `crates/overseer-daemon/src/work/sprint.rs`
- Modify: `crates/overseer-daemon/src/work/mod.rs`

**Interfaces:**
- Consumes: `JiraApi`, `ActiveSprint`, `jira_token` (Task 3), and `Config.jira_*` (Task 1).
- Produces in `sprint.rs`:
  - `pub fn weekdays_between(start: Ymd, end: Ymd) -> u32` counts inclusive Monday-to-Friday days, at least 1.
  - `pub fn sprint_folder_name(jira_name: &str) -> String` follows the Global Constraints rule and caps at 50 chars.
- Produces in `store.rs` (pure, `pub(crate)`):
  - `fn derive_pattern(keys: &[String]) -> String` follows the Global Constraints rule.
  - `enum EmptyGuard { Accept, Keep }`
  - `fn empty_guard(prev_len: usize, new_len: usize, streak: u32) -> EmptyGuard`. It returns Keep only when `new_len == 0 && prev_len > 0 && streak == 0`.
  - `WorkStore` keeps `empty_streak: u32` in the cache.
- `WorkStore` field `jira: JiraSource`, where `enum JiraSource { Api(JiraApi), Command(Vec<String>), Off }`:
  - `Api` when `jira_site` is non-empty.
  - Else `Command` when `jira_command` is non-empty.
  - Else `Off`, with the existing `JIRA_OFF`.
  - A missing `jira_email` with `jira_site` set gives `Off` and the status `jira_email is empty; set it in Settings`.
- `pattern` becomes `RwLock<Regex>`. When `ticket_pattern` is empty, it is recompiled from `derive_pattern` after each good Jira list. A set pattern behaves as today.
- Sprint header:
  - When `Api` with a board, `refresh(Jira)` also reads `active_sprint`. Cache it as `Option<ActiveSprint>`. An error leaves the last value and is appended to the Jira status message.
  - `sprint_text` and `sprint_day` order: config `sprint_name`/`sprint_start`/`sprint_days` when set, then the active sprint (`sprint_folder_name(name)`, `start`, `weekdays_between(start, end)`), then today's folder scan.
  - With a board and no active sprint, `WorkView.sprint` is `Some("No active sprint")` and `sprint_day` is `None`.
- `pub fn active_sprint(&self) -> Option<ActiveSprint>` and `pub fn tickets(&self) -> Vec<Ticket>` exist for Task 5.
- **Beads in an owned folder.** When `notes_owned` and the beads read fails because `bd` is not found or the folder has no `.beads`, the beads status is `ok` with the message `beads not set up here` rather than an error. This needs a `notes_owned: bool` on `WorkStore`.
- **`sync_command`.** Keep the default. It already skips when the script file is missing, which is the spec's "runs only when set" for any folder without the script. This is a ruling: no behaviour change is needed.

- [ ] **Step 1: Write the failing tests:**
  - `weekdays_between` covers Mon 9 Nov 2026 to Fri 20 Nov 2026 = 10, and Wed to Tue = 6.
  - `sprint_folder_name`:
    - `"DEMO Sprint 26-01"` → `sprint-demo-sprint-26-01`
    - `"Sprint 4"` → `sprint-4`
    - `"../evil/Sprint 1 🚀"` → `sprint-evil-sprint-1`
    - a 200-char name gives at most 50 chars
    - `""` → `sprint`
  - `derive_pattern`:
    - `["DEMO-2","OPS-1","DEMO-9"]` → `(DEMO|OPS)-\d+`
    - `[]` → `[A-Z][A-Z0-9]+-\d+`
    - the result matches `OPS-1` and does not match `UTF-8`
  - `empty_guard`:
    - `(3,0,0)` gives Keep
    - `(3,0,1)` gives Accept
    - `(0,0,0)` gives Accept
    - `(3,2,0)` gives Accept
  - Store-level, in `crates/overseer-daemon/tests/server.rs` style, with a `jira_command` stub that prints a list first and then `[]`:
    - The first empty poll keeps the tickets, and the status message is `Jira returned no tickets; keeping the last list`.
    - The second empty poll empties them.
    - A stub that exits non-zero keeps the tickets with `ok == false` (Review Focus 4).
    - With `notes_owned` and a `bd_command` naming a missing program, the beads status is `ok` with `beads not set up here`.
- [ ] **Step 2:** Run the tests and expect a fail.
- [ ] **Step 3:** Implement it. Keep the acli path's existing behaviour apart from the guard.
- [ ] **Step 4:** Run the tests and the gates.
- [ ] **Step 5:** Commit: `Use the Jira API when a site is set and keep the last list on a sudden empty result`.

### Task 5: Sprint and ticket folders in the owned notes folder

**Files:**
- Modify: `crates/overseer-daemon/src/notes.rs`
- Modify: `crates/overseer-daemon/src/server.rs` (call it after Jira refreshes)

**Interfaces:**
- Consumes:
  - `sprint_folder_name` (Task 4)
  - `WorkStore::active_sprint` and `WorkStore::tickets` (Task 4)
  - `Config.notes_owned` and `jira_site` (Task 1)
  - the existing `folder_for` logic in `store.rs` (make it `pub(crate)`)
- Produces:
  - `pub fn ticket_folder_name(key: &str, title: &str) -> String` gives `KEY-<slug of title>`, with the same slug rules and a 50-char cap on the slug. A title with an empty slug gives `KEY`.
  - `pub fn sync_sprint(dir: &Path, sprint: &str, tickets: &[Ticket], site: &str, now: i64) -> std::io::Result<usize>`. It returns the number of `ticket.md` files written. For each ticket it:
    - uses an existing folder for the key anywhere under `Sprints/` (via `folder_for`), else creates `Sprints/<sprint>/<ticket_folder_name>`
    - writes `ticket.md` only when its content, excluding the `Updated:` line, differs
  - `ticket.md` content, exact:
    ```
    # {KEY}: {title}

    - Status: {status}
    - Priority: {priority or "none"}
    - Jira: https://{site}/browse/{KEY}
    - Updated: {YYYY-MM-DD HH:MM} by Overseer

    Overseer rewrites this file from Jira. Put your own notes in other files in this folder.
    ```
  - When `jira_site` is empty (the acli path), the Jira line is left out.
- Wiring:
  - Run only when `notes_owned` and the store has an active sprint.
  - The daemon calls it after each Jira refresh, off the async thread.
  - No commit is made here. The writer's timed backup commits the files. This is a ruling against the spec's "same commit queue": the backup is that queue's committer for free files.

- [ ] **Step 1: Write the failing tests** in `notes.rs`:
  - `creates_sprint_and_ticket_folders`. Two tickets give two folders under `Sprints/sprint-demo-1/` with the exact `ticket.md`, and the call returns 2.
  - `second_sync_writes_nothing`. The same tickets one minute later return 0 and leave the files byte-for-byte unchanged.
  - `status_change_rewrites_only_ticket_md`. A `notes.md` beside it is unchanged after a status change, and `ticket.md` shows the new status (Review Focus 3).
  - `existing_folder_for_key_is_reused`. An existing `Sprints/sprint-demo-0/DEMO-1-old/` is used instead of a new one.
  - `ticket_leaving_sprint_keeps_folder`. Syncing again without `DEMO-2` leaves its folder and files in place.
  - `hostile_title_stays_inside`. A title of `../../x` gives a folder directly under the sprint folder (Review Focus 2).
- [ ] **Step 2:** Run `cargo test -p overseer-daemon notes::` and expect a fail.
- [ ] **Step 3:** Implement it and wire it into `server.rs`.
- [ ] **Step 4:** Run the tests and the gates.
- [ ] **Step 5:** Commit: `Keep sprint and ticket folders in Overseer's own notes folder in step with Jira`.

### Task 6: The user's name in prompts and messages

**Files:**
- Modify the roles: `crates/overseer-daemon/src/roles/coordinator.md`, `worker.md`, `brain_worker.md` and `brain_coordinator.md`.
- Modify: `launch.rs` (`{user}` replacement)
- Modify the messages: `coord/plan.rs:325,344,373,427`, `locks/table.rs:314,342`, `relay/courier.rs:40,66,94`, `brain/mod.rs:63` and `overseer/src/ctl.rs:660`.

**Interfaces:**
- Consumes: `Config.user_name` (Task 1).
- Every user-facing "Jonathan" in those files becomes the configured name. Plumb it as a `user: String` (or `Arc<str>`) field on the owning service, set from config at start. The builder picks the struct.
- `courier.rs:66` matches messages by prefix. Change it to match `"{user} released"`, or better, a non-name marker. It must still route.
- In the role prompts:
  - "2ndBrain rules" becomes "Notes rules".
  - "the vault at {brain}" becomes "the notes folder at {brain}".
  - MCP tool names such as `commit_2ndbrain` stay unchanged.
- Tests: existing tests that assert "Jonathan …" pass a user of `Demo` and assert "Demo …". Add `launch` test `role_names_the_user`: the rendered coordinator prompt contains `Demo runs up to six` and no `Jonathan`.

- [ ] **Step 1:** Change the test expectations to `Demo` and add `role_names_the_user`.
- [ ] **Step 2:** Run `cargo test --workspace` and expect those to fail.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run `grep -rn "Jonathan" crates --include='*.rs' --include='*.md'`. Only comments and test fixtures that are not user-facing may remain. Then run the gates.
- [ ] **Step 5:** Commit: `Name the configured user in prompts and messages`.

### Task 7: Settings rows, the Jira token, and GUI wording

**Files:**
- Modify: `crates/overseer-daemon/src/settings.rs` (catalogue and checks)
- Modify: `crates/overseer-proto/src/msg.rs` (`Request::SetJiraToken`, plus `jira_token_set: bool` in `SettingsView` or alongside it)
- Modify: `crates/overseer-daemon/src/server.rs` (handle the request)
- Modify the GUI: `crates/overseer-gui/src/settings_view.rs`, `board.rs:424-456`, `notify.rs:118` and `start_dialog.rs:20`.

**Interfaces:**
- Consumes: `save_jira_token` and `jira_token` (Task 3), and the Task 1 fields.
- Catalogue changes:
  - A new group const `JIRA: &str = "Jira"` with rows `jira_site`, `jira_email` and `jira_board`, plus `jira_cloud_id` (Text, allow empty).
  - `user_name` goes under the renamed `BRAIN`, now `"Notes"`.
  - The `brain_dir` row's key becomes `notes_dir`. When saving, if the file has `brain_dir` and not `notes_dir`, edit `brain_dir`.
  - An empty `ticket_pattern` is accepted, with the note `Empty: built from your Jira project keys`.
- `Request::SetJiraToken { token: String }`. The daemon calls `save_jira_token(&cfg.jira_email, …)` and answers `Response::Ok` or `Response::Error`. An empty `jira_email` gives the error `set jira_email first`. The token is never logged; the request's `Debug` impl prints `SetJiraToken { token: "<hidden>" }`.
- The Settings view gets a "Jira token" row in the Jira group:
  - It shows `set` or `not set`, from `jira_token(...).is_ok()` at `GetSettings` time.
  - It has a masked text field and a Save button that sends the request.
  - The field clears after a send and is never filled back in.
- GUI wording:
  - `board.rs` strip texts swap `2ndBrain:` for `Notes:`.
  - The notification title becomes `Notes`.
  - `start_dialog` `DEFAULT_CWD` becomes the `BrainView.brain_dir` when known, else `~`. Change `initial_fields` to take it.
- Tests:
  - Update the existing `2ndBrain:` assertions to `Notes:`.
  - `settings.rs` test `notes_dir_saves_into_old_key`.
  - A proto test that `SetJiraToken`'s Debug output hides the token.
  - A `start_dialog` test that the cwd defaults to the given notes folder.

- [ ] **Step 1:** Write and update the tests.
- [ ] **Step 2:** Run `cargo test --workspace` and expect a fail.
- [ ] **Step 3:** Implement it.
- [ ] **Step 4:** Run the gates.
- [ ] **Step 5:** Commit: `Add Jira and notes settings, a Keychain token field, and neutral wording`.

### Task 8: README, LICENSE, docs tidy

**Files:**
- Modify: `README.md` (a new top section, "Getting started")
- Create: `LICENSE` (MIT, `Copyright (c) 2026 Jonathan Gill`)
- Move: `docs/acceptance/` and `docs/setup/` go to `docs/internal/`, via `git mv`. Add `docs/internal/README.md` with one line: `The maintainer's hand checks and machine notes; not needed to use Overseer.`
- Modify: `.gitignore` (add `.claude/`)

**Getting started must cover, in this order:**
1. What Overseer is, and what S0 and S1 to S6 are.
2. Requirements:
   - macOS 13 or later
   - Xcode command line tools
   - Rust via rustup
   - Claude Code
   - a scoped Atlassian API token with `read:jira-work`, `read:sprint:jira-software` and `read:board-scope:jira-software`
   - optionally `bb` (PRs) and `bd` (beads)
3. Install: `git clone https://github.com/techize/overseer.git`, `cd overseer`, `make install`.
4. First run: a starter `config.toml` with `jira_site = "demo.atlassian.net"`, `jira_email = "demo@example.test"` and `jira_board = "1"`. Say where the file lives. Then set the token in Settings.
5. The notes folder: `~/Overseer` is created and filled, or point `notes_dir` at an existing folder.
6. What each optional tool adds.
7. Troubleshooting:
   - token rejected
   - no tickets (check the JQL and the board)
   - Keychain prompt
   - notifications not showing

Update the reference sections below for the new keys. Fix any links into moved docs (`grep -rn "docs/acceptance\|docs/setup"`).

- [ ] **Step 1:** Write the docs, LICENSE and moves.
- [ ] **Step 2:** Run `grep -rn $'\u2014' README.md docs/internal/README.md LICENSE`, expecting no output. Run the link grep, expecting only `docs/internal` paths. Run the gates.
- [ ] **Step 3:** Commit: `Add a getting started guide and licence, and move maintainer docs aside`.
