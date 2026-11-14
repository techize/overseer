# Phase 7: Polish Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Finish the app for daily use on the work laptop. You get a Locks view with every target, holder and waiter and a guarded force release; a Settings view over the settings that exist today in `config.toml` (plus the targets file's status) that saves without clobbering the file; and an install and upgrade path that installs Rust when it is missing, knows which build the daemon runs, and never ends running sessions without warning you. The board header gains the sprint name and day that phase 6 deferred here.

**Architecture:** No ledger change. A build id travels in the bundle's `Info.plist` and is read once at process start (`overseer-proto/src/build.rs`), so the daemon, the app and `install-agent` can tell an upgrade apart; the daemon answers a new `DaemonInfo` request. `agent.rs` gains per-home LaunchAgent labels (an isolated `OVERSEER_HOME` can never name the real agent), restart rules and `ctl restart`. `make install` moves into `scripts/install.sh` (and `make uninstall` into `scripts/uninstall.sh`) so both run against a temp home in tests. A new daemon module `settings.rs` holds the catalogue of editable keys, reads them against the running config and saves edits with `toml_edit`; the GUI's Settings view only talks to it over the socket. The GUI gains `locks_view.rs` (a screen) and `settings_view.rs` (a modal), and `work/sprint.rs` computes the header's sprint text.

**Tech Stack:** Rust, tokio, rusqlite (bundled, unchanged), `serde_json`, `toml` 0.8, `toml_edit` 0.22 (already in `Cargo.lock` through `toml` 0.8), `regex`, GPUI 0.2.2, POSIX `sh`, `launchctl`, `codesign`, rustup.

**Spec:** `docs/superpowers/specs/2026-11-09-overseer-design.md`, section 9 item 7 (the scope), 7.9 (Locks view, configurable font, size and theme, board header), 7.5 (targets, holders, waiters, force release, long-held flag), 7.10 (config and targets files, logs), 7.11 (build, install, upgrade), 7.1 (LaunchAgent), 4 (anti-goals) and 10.

## Global Constraints

- Install is `make install`: installs Rust through `rustup` if it is missing (user-local, no admin), builds release, assembles `~/Applications/Overseer.app` with an ad-hoc signature, symlinks `~/.local/bin/overseer`, installs the LaunchAgent.
- Upgrade is `git pull && make install`, and it warns before restarting the daemon (sessions become resumable).
- The bundle id and the LaunchAgent label are `uk.co.techize.overseer`; the agent lives in `~/Library/LaunchAgents` and needs no admin rights.
- Config is TOML at `~/.config/overseer/config.toml` plus `targets.toml`; the ledger is `~/Library/Application Support/Overseer/overseer.db`; logs are under `~/Library/Logs/Overseer/`.
- The Locks view shows all targets, holders and waiters, with force release (spec 7.9).
- Long-held locks are flagged on the board after a configurable idle period, but never dropped automatically (spec 7.5).
- Quitting the GUI never touches sessions; a daemon restart must leave every session resumable (spec 4, 7.2).
- Nothing acts silently (spec 4): every restart, release and saved setting is reported where you started it.
- Overseer never rewrites `~/.claude/settings.json`, `~/.claude.json` or your global hooks and skills, and the installer never edits your shell rc files.
- No em dashes anywhere: code, comments, UI copy, commits, docs.
- The repo is public: only invented names in tests, fixtures and docs (`DEMO-12`, `ov-3`, `demo-api`, `sprint-26-01`, `argo:uat/root`, the `demo lane` label).
- Tests never touch the real vault `~/Repos/2ndBrain`, `~/Repos/.worktrees`, the real `~/.claude/CURRENT_SESSION.md`, the installed app `~/Applications/Overseer.app`, `~/.local/bin/overseer`, the LaunchAgent `uk.co.techize.overseer` or its plist, its socket, `~/Library/Application Support/Overseer`, or the real `~/.config/overseer/`. Every test that runs `install-agent`, `uninstall-agent`, `ctl restart` or an install script sets `HOME` and `OVERSEER_HOME` to temp dirs and `OVERSEER_LAUNCHCTL` to a stub.
- CI runs `cargo fmt --check`, `cargo clippy --workspace -- -D warnings` and `cargo test --workspace`; all three pass at the end of every task. Imports used only by tests go in the test module.
- The protocol stays backward compatible: new fields on existing structs use `#[serde(default)]`; new `Request`, `Response` and `Event` variants go at the end of their enums; new types are re-exported from `crates/overseer-proto/src/lib.rs`.
- Commits are short human sentences, no emojis, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- UI copy refers to the user as "you", never by name.

## Decisions (gaps this plan fills)

1. **An isolated home gets its own LaunchAgent.** With `OVERSEER_HOME` set, the label is `uk.co.techize.overseer-home-<8 hex>` (FNV-1a 32 of the path's bytes), the plist lives at `$OVERSEER_HOME/agent/<label>.plist` (never in `~/Library/LaunchAgents`, so launchd does not load it at login), and its `EnvironmentVariables` carry `HOME`, `SHELL`, `OVERSEER_HOME`, `OVERSEER_CONFIG`, `OVERSEER_TARGETS`, `CLAUDE_CONFIG_DIR` and `OVERSEER_DEFAULT_COMMAND` as the installing shell has them (set and non-empty only), after `PATH`. With no `OVERSEER_HOME` nothing changes: same label, same path, a plist with only `PATH`. Why: the spec is silent on temp installs; this makes it impossible for a rig or test to name the real agent, and lets the acceptance rig exercise launchd for real. It also ends phase 6's rig hazard (a `ctl` autostart with the caller's environment), because autostart kickstarts the home's own agent when its plist exists.
2. **`OVERSEER_LAUNCHCTL`** names the launchctl program (default `launchctl`). Tests always point it at a stub that logs its arguments, exits 0 for `bootstrap` and `bootout`, and exits 1 for `print` and `kickstart` (so autostart falls back to spawning).
3. **Build id.** `scripts/bundle.sh` writes `<key>OverseerBuild</key>` into `Info.plist`: `$OVERSEER_BUILD` when set, else `<git short sha>[-dirty]-<UTC yyyymmddHHMM>`. Each process reads it once at start from `<exe>/../../Info.plist` (exe canonicalised); an unbundled binary is `dev`. Why: the crate version stays `0.1.0` across phases, so it cannot tell an upgrade; reading once means an old daemon keeps reporting its own build after the bundle is replaced under it.
4. **Restart rules** (the spec's "warns before restarting"), shared by `install-agent` and `ctl restart`: no daemon answering: start it. Same build and an unchanged plist (`install-agent` only): restart nothing. No running session: restart without asking. Running sessions: print the warning; `--yes` (`make install YES=1`) restarts; `--no-restart` (`NO_RESTART=1`) keeps; otherwise ask on a terminal (`[y/N]`, only `y` or `yes` restarts) and keep when stdin is not a terminal. Keeping leaves the old daemon running its old build and launchd untouched. Why: a scripted `make install` must never end sessions silently (spec 4).
5. **Restarting** reuses the crash-like path the ledger already resumes from: under launchd `bootout` then `bootstrap` (`install-agent`) or `kickstart -k` (`ctl restart`); without the agent, `Request::Shutdown`, wait for the socket to go, then the normal autostart. No new signal handling.
6. **Rust install.** When neither `cargo` on `PATH` nor `${CARGO_HOME:-$HOME/.cargo}/bin/cargo` exists, `install.sh` prints `Rust is not installed. Installing it for your user with rustup (no admin rights needed).` and runs `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal --default-toolchain none`, then uses `$CARGO_HOME/bin/cargo`; `rust-toolchain.toml` picks the toolchain. It ends by printing `Add ~/.cargo/bin to your PATH to run cargo yourself.` Why: spec 7.11 step 1; `--no-modify-path` because Overseer does not edit your shell files. `OVERSEER_INSTALL_BINARY=<path>` skips the build (tests and the rig).
7. **Settings scope** (the spec names only paths, JQL, poll intervals, the shared-files list, the lock idle period, font, size and theme): the keys that exist today in `config.toml` (the daemon's top-level keys, `[terminal]`, `[editor]`), the three sprint keys of Decision 11, and a read-only status of `targets.toml`. Read-only in the view: `brain_writer` (it has a setup checklist), `jira_command`, `bd_command`, `sync_command` (a typo turns a source off) and `terminal.ansi` (16 colours are a file job). Each read-only row says to edit `config.toml`, and the view has an Open button for each file that exists.
8. **The daemon is the only writer of `config.toml` from the app.** `SaveSettings` checks each edited key against its catalogue constraint, applies the edits with `toml_edit` (comments, order and unknown keys kept; `Reset` removes the key), checks the whole result still parses as `Config`, refuses when the file's hash differs from the one the view read, and writes atomically through a symlink (temp file in the link target's folder, then rename). Any refusal leaves the file byte for byte as it was. Why: a file that stops parsing makes the daemon start on all defaults, including the default `brain_dir`, the real vault.
9. **Daemon settings apply at the next daemon start**, not live. The view marks a key `restart needed` when the file's value differs from the running config, and offers Restart the daemon (with Decision 4's warning), which runs `<own binary> ctl restart --yes` with the app's environment. Terminal and editor settings apply in the app at once, open terminals included. Why: hot reload would reach the work store, locks, brain writer and launch code, while a restart already brings sessions back resumable.
10. **The app reads the same config file as the daemon.** `Theme::config_path` and `load_editor` use `overseer_proto::paths::config_path()` (which honours `OVERSEER_CONFIG`). Today they ignore it, so a rig's app and its Settings view would read different files.
11. **Sprint name and day** (deferred from phase 6): the name is `sprint_name` from `config.toml`, else the newest folder by name matching `Sprints/sprint-*` under `brain_dir` (spec 7.8 maps a sprint to that folder). The day comes from `sprint_start` (`YYYY-MM-DD`) and `sprint_days` (default 10), counted in weekdays from the start: `sprint-26-01, day 4 of 10`; before the first weekday `sprint-26-01, starts Mon 9 Nov`; after the last `sprint-26-01, ended Fri 20 Nov`; no or invalid `sprint_start` gives the name alone; no name gives no text. Why: the Jira search output carries no sprint, and the vault already names sprints.
12. **Locks view** is a screen (`Screen::Locks`), opened by `l` on the board, Cmd+Shift+L anywhere navigation works, and a click on the board strip's lock count; Esc goes back to the board. It force-releases only the holder it showed (`ForceRelease { holder: Some(..) }`); it does not remove waiters (the spec lists force release only, and a waiter leaves its queue with `release_target`). A confirm open on a target that changes hands is closed with a status line instead of sent.
13. **Settings view** is a modal like the plan view: Cmd+, and the app menu's Settings item open it, its key context `Settings` joins `NAV`'s exclusions, Cmd+S saves, Esc closes (with unsaved edits the first Esc says `Esc again closes without saving`). Lists are edited as comma-separated text.

## Review Focus

1. **A test or the rig reaches the real LaunchAgent** (a temp install boots out `uk.co.techize.overseer`, or `make uninstall` with no binary falls back to booting out the real label). Tests: `custom_home_gets_its_own_label_and_place` and `install_agent_in_a_custom_home_never_names_the_real_agent` in Task 2, `uninstall_in_a_custom_home_never_touches_the_real_agent` in Task 4.
2. **An upgrade ends running sessions without your say-so** (`make install` from a script, stdin not a terminal). The daemon and its sessions are kept and the message says how to finish. Test: `install_agent_keeps_a_daemon_with_running_sessions` in Task 3.
3. **Saving settings clobbers the file**: comments or unknown keys lost, a symlinked `config.toml` replaced by a plain file, or a hand edit made while the view was open overwritten. Tests: `save_keeps_comments_and_unknown_keys`, `save_keeps_a_symlinked_config_a_symlink`, `save_refuses_when_the_file_changed_since_read` in Task 6.
4. **A bad value is saved** and the daemon's next start silently falls back to all defaults (the real vault path among them). Tests: `invalid_value_is_refused_and_the_file_is_untouched` and `unparseable_file_is_never_rewritten` in Task 6.
5. **Force release takes a target from the wrong session** because it changed hands while the confirm was open. Tests: `confirm_sends_the_holder_it_showed` and `confirm_drops_when_the_target_changes_hands` in Task 7.

---

### Task 1: Build id and daemon info

**Files:**
- Create: `crates/overseer-proto/src/build.rs`
- Modify: `crates/overseer-proto/src/lib.rs`, `crates/overseer-proto/src/msg.rs`, `packaging/Info.plist`, `scripts/bundle.sh`, `crates/overseer-daemon/src/server.rs`, `crates/overseer/src/ctl.rs`
- Test: `crates/overseer-daemon/tests/server.rs`, `crates/overseer/tests/cli.rs`, `crates/overseer/tests/install.rs` (new)

**Interfaces:**
- Produces (proto): `pub const DEV_BUILD: &str = "dev"`; `pub fn build_from_plist(text: &str) -> Option<String>` (the `OverseerBuild` string, trimmed; `None` when missing, empty or still `__BUILD__`); `pub fn build_of_exe(exe: &Path) -> String` (`exe.parent()?.parent()?.join("Info.plist")`, else `DEV_BUILD`); `pub fn current_build() -> &'static str` (`OnceLock`, canonicalised `current_exe`); `pub fn build_notice(app: &str, daemon: &str) -> Option<String>` (`None` when equal or either is `dev`, else `The daemon runs build <d>, this app build <a>. Restart the daemon to finish the upgrade (overseer ctl restart, or Settings).`).
- Produces (proto msg): `struct DaemonInfo { version: String, build: String, pid: u32, started_at: i64, exe: PathBuf }` (`Serialize, Deserialize, Clone, Debug, PartialEq`); `Request::DaemonInfo`; `Response::DaemonInfo(DaemonInfo)`.
- Produces (daemon): `run` records `started_at` and `current_build()` on entry; `Ctx` gains `info: &'a DaemonInfo`; `Request::DaemonInfo` answers it.
- Produces (ctl): `CtlCmd::Version`, which connects without autostart; `pub fn format_version(app_version: &str, app_build: &str, daemon: Option<&DaemonInfo>, now: i64) -> Vec<String>`: `app     0.1.0 build <b>`, then `daemon  0.1.0 build <b>  pid <n>  since HH:MM` or `daemon  not running`, then the `build_notice` line when there is one.
- Produces (packaging): `Info.plist` gains `<key>OverseerBuild</key><string>__BUILD__</string>`; `bundle.sh` fills it per Decision 3.

- [ ] **Step 1: Write the failing tests**
  - `build.rs`: `reads_the_build_key` (`build_from_plist("...<key>OverseerBuild</key>\n    <string>abc1234-202611140930</string>...") == Some("abc1234-202611140930")`); `missing_or_placeholder_build_is_none` (no key, `<string></string>`, `<string>__BUILD__</string>` all `None`); `exe_outside_a_bundle_is_dev` (a temp `X.app/Contents/{MacOS/overseer,Info.plist}` gives the key's value; `/tmp/overseer` gives `dev`); `notice_only_when_both_known_and_different` (`("a","a")`, `("dev","b")`, `("a","dev")` give `None`; `("b2","b1")` gives the exact text above).
  - `msg.rs`: `daemon_info_round_trips` for the request and the response.
  - `tests/server.rs`: `daemon_info_reports_pid_and_start`: `pid == std::process::id()`, `build == "dev"`, `started_at` within 5 s of now, `version == env!("CARGO_PKG_VERSION")`.
  - `tests/cli.rs`: `ctl_version_without_a_daemon_does_not_start_one` (prints `daemon  not running`; no `overseer.sock` in the temp home afterwards); `ctl_version_names_the_running_daemon` (after `ctl status`: a line starting `daemon  0.1.0 build dev  pid `).
  - `tests/install.rs` (new, every command with `HOME` and `OVERSEER_HOME` under one `tempfile::tempdir()`): `bundle_writes_the_build_id_and_signs` (`OVERSEER_BUILD=test-7 scripts/bundle.sh <CARGO_BIN_EXE_overseer> <tmp>/Overseer.app`; the plist has `<string>test-7</string>` after `OverseerBuild`; `codesign --verify <app>` exits 0; `<app>/Contents/MacOS/overseer ctl version` prints `app     0.1.0 build test-7`).
- [ ] **Step 2: Run** `cargo test -p overseer-proto && cargo test -p overseer-daemon --test server daemon_info_ && cargo test -p overseer --test cli ctl_version_ && cargo test -p overseer --test install`. Expected: compile errors for the missing items.
- [ ] **Step 3: Implement.** `build_from_plist` finds the key, then the next `<string>...</string>`; no plist crate. `bundle.sh` computes the default id with `git -C "$root" rev-parse --short HEAD` (`unknown` on failure), `-dirty` when `git -C "$root" status --porcelain` prints anything, and `-$(date -u +%Y%m%d%H%M)`, then `sed` replaces `__VERSION__` and `__BUILD__` and prints `bundled <app> (<version>, build <id>)`.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Record a build id in the bundle and report the daemon's`.

### Task 2: A LaunchAgent per home

**Files:**
- Modify: `crates/overseer/src/agent.rs`, `crates/overseer/src/ctl.rs` (`spawn_daemon`)
- Test: `crates/overseer/tests/cli.rs`

**Interfaces:**
- Consumes: `AGENT_LABEL`, `log_dir`, `socket_path`.
- Produces:
  - `pub fn agent_label(overseer_home: Option<&OsStr>) -> String` (Decision 1; an empty value counts as unset)
  - `pub fn plist_path_for(overseer_home: Option<&OsStr>, home: &Path) -> PathBuf`; `plist_path()` uses it with the process environment
  - `pub const AGENT_ENV: [&str; 7] = ["HOME", "SHELL", "OVERSEER_HOME", "OVERSEER_CONFIG", "OVERSEER_TARGETS", "CLAUDE_CONFIG_DIR", "OVERSEER_DEFAULT_COMMAND"]`
  - `pub fn agent_env(custom_home: bool, get: impl Fn(&str) -> Option<OsString>) -> Vec<(String, String)>` (empty unless `custom_home`; set, non-empty vars in `AGENT_ENV` order)
  - `pub fn plist_xml(label: &str, exe: &Path, log_dir: &Path, path_env: &str, extra_env: &[(String, String)]) -> String` (extends today's; `PATH` first, every value XML-escaped)
  - `fn launchctl_program() -> OsString` (`OVERSEER_LAUNCHCTL` or `launchctl`), used by every launchctl call
  - `service()`, `bootout`, `kickstart` and `agent_loaded` use `agent_label` for this process's home
  - `use_launchd(overseer_home, plist_exists)` is replaced by `pub fn use_launchd(plist_exists: bool) -> bool`: the plist found is this home's own, so autostart kickstarts it whenever it exists
- [ ] **Step 1: Write the failing tests**
  - `agent.rs` unit: `default_home_plist_is_unchanged` (`agent_label(None) == "uk.co.techize.overseer"`, `plist_path_for(None, "/Users/me")` is `/Users/me/Library/LaunchAgents/uk.co.techize.overseer.plist`, and `plist_xml(.., &[])` has exactly one `<key>` inside `EnvironmentVariables`, `PATH`); `custom_home_gets_its_own_label_and_place` (`agent_label(Some("/tmp/ov-a"))` matches `^uk\.co\.techize\.overseer-home-[0-9a-f]{8}$`, is the same on a second call, differs for `/tmp/ov-b`, never equals `AGENT_LABEL`; `agent_label(Some(""))` is the real label; the plist path is `/tmp/ov-a/agent/<label>.plist`); `custom_home_plist_carries_the_isolating_env` (the XML has `<key>OVERSEER_HOME</key>`, `<key>HOME</key>`, `<key>SHELL</key>` with their escaped values); `agent_env_skips_unset_and_empty` (with `custom_home` false it is empty; with true, an unset and an empty var are left out and the order follows `AGENT_ENV`). Update the existing plist tests to the new `plist_xml` signature and replace `autostart_uses_launchd_only_for_the_default_home_with_an_agent` with `autostart_uses_this_homes_agent_when_its_plist_exists`.
  - `tests/cli.rs`, with a new `Env::agent(&self, args)` that always sets `HOME` to `<home>/home`, `OVERSEER_HOME`, and `OVERSEER_LAUNCHCTL` to a stub writing `$*` lines to `<home>/launchctl.log`: `install_agent_in_a_custom_home_never_names_the_real_agent` (`install-agent --yes` exits 0; `<home>/agent/<label>.plist` exists and has `OVERSEER_HOME`; nothing under `<home>/home/Library/LaunchAgents`; the log has a `bootstrap gui/<uid> <home>/agent/` line, and every log line naming `uk.co.techize.overseer` names `uk.co.techize.overseer-home-`); `uninstall_agent_in_a_custom_home_removes_its_plist`.
- [ ] **Step 2: Run** `cargo test -p overseer agent:: && cargo test -p overseer --test cli _agent_`. Expected: FAIL.
- [ ] **Step 3: Implement.** FNV-1a 32 inline (offset `0x811c9dc5`, prime `0x01000193`) over `as_bytes()` of the `OsStr`. `install` creates `$OVERSEER_HOME/agent` for a custom home. `install-agent` gains `--yes` and `--no-restart` in `main.rs` now (accepted and passed through; Task 3 gives them meaning).
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Give an isolated home its own LaunchAgent`.

### Task 3: Warn before restarting the daemon

**Files:**
- Modify: `crates/overseer/src/agent.rs`, `crates/overseer/src/main.rs`, `crates/overseer/src/ctl.rs`
- Test: `crates/overseer/tests/cli.rs`

**Interfaces:**
- Consumes: Task 1 `Request::DaemonInfo`, `current_build`; Task 2 labels; `Request::ListSlots`, `Request::Shutdown`; `ctl::spawn_daemon`.
- Produces:
  - `pub struct RunningDaemon { pub build: String, pub running: Vec<(SlotId, String)> }` (slot and label of each `Running` slot)
  - `#[derive(Clone, Copy)] pub struct RestartOpts { pub yes: bool, pub no_restart: bool, pub tty: bool }`
  - `#[derive(Debug, PartialEq)] pub enum RestartChoice { Start, Nothing, Restart, Ask, Keep }`
  - `pub fn restart_choice(daemon: Option<&RunningDaemon>, unchanged: bool, opts: RestartOpts) -> RestartChoice` (Decision 4 in that order; `ctl restart` passes `unchanged = false`)
  - `pub fn restart_warning(d: &RunningDaemon) -> String`: `The daemon (build b1) has 2 running sessions: S1 demo lane, S3 docs. Restarting it ends them; each comes back as resumable, and r on its card resumes it.` (`1 running session`; a slot with no label is just `S4`)
  - `pub const RESTART_PROMPT: &str = "Restart the daemon now? [y/N] "`
  - `pub fn keep_text(build: &str) -> String`: `The daemon keeps running build <b>. Run overseer ctl restart when you are ready to finish the upgrade.`; `pub fn nothing_text(build: &str) -> String`: `The daemon already runs build <b>; nothing to restart.`; restarted: `Restarted the daemon (build <old> to <new>).`; started: `Started the daemon (build <b>).`
  - `fn query_daemon(socket: &Path) -> Option<RunningDaemon>` (`Client::connect` without autostart, `DaemonInfo` then `ListSlots`, 2 s in all)
  - `pub fn install(opts: RestartOpts) -> anyhow::Result<()>`: writes the plist, then acts on the choice (`Keep` and `Nothing` leave launchd alone; `Start` and `Restart` bootout, stop a daemon not run by launchd, then bootstrap)
  - `pub fn restart(opts: RestartOpts) -> anyhow::Result<()>` (Decision 5); `CtlCmd::Restart { #[arg(long)] yes: bool }`; `Cmd::InstallAgent { #[arg(long)] yes: bool, #[arg(long)] no_restart: bool }`
- [ ] **Step 1: Write the failing tests**
  - `agent.rs` unit: `no_daemon_starts`, `same_build_and_plist_restarts_nothing`, `no_running_session_restarts_without_asking` (even with `tty` false), `running_sessions_ask_on_a_terminal`, `running_sessions_without_a_terminal_are_kept`, `yes_restarts_and_no_restart_keeps` (`no_restart` wins over `tty`); `warning_names_each_running_session` (the exact two-session text above, and `has 1 running session: S4.`).
  - `tests/cli.rs` (`Env::agent`, `OVERSEER_DEFAULT_COMMAND` a stub at `<home>/bin/claude` running `exec sleep 3600`, so its sessions are resumable):
    - `install_agent_keeps_a_daemon_with_running_sessions`: `ctl start 1 --cwd <tmp>`; `install-agent` with stdin from `/dev/null`: stdout has the warning and `keep_text("dev")`; exit 0; `ctl status` still lists S1 `running` with the same pid; the launchctl log has no `bootout` and no `bootstrap`.
    - `install_agent_yes_restarts_and_the_session_is_resumable`: same start, `install-agent --yes`: prints `Restarted the daemon (build dev to dev).`; then `ctl status` (autostart; the stub's `kickstart` fails, so it spawns) lists S1 `resumable`.
    - `install_agent_twice_restarts_nothing`: daemon up, no sessions; `install-agent` restarts; `ctl status`; a second `install-agent` prints `nothing_text("dev")`.
    - `ctl_restart_yes_brings_the_daemon_back`: S1 running; `ctl restart --yes` exits 0 and prints the warning then `Restarted the daemon`; the new daemon's `ctl version` pid differs; S1 is `resumable`.
- [ ] **Step 2: Run** `cargo test -p overseer agent:: && cargo test -p overseer --test cli install_agent_ ctl_restart_`. Expected: FAIL.
- [ ] **Step 3: Implement.** `tty` is `std::io::stdin().is_terminal()`. `Ask` prints the warning and the prompt and reads one line. `install-agent`'s `unchanged` is "the plist file's current text equals the new text, and the daemon's build equals `current_build()`". Messages go to stdout; failures to stderr with exit 1.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Warn before an upgrade restarts the daemon`.

### Task 4: Install and uninstall scripts

**Files:**
- Create: `scripts/install.sh`, `scripts/uninstall.sh`
- Modify: `Makefile` (`install` and `uninstall` call the scripts, passing `YES` and `NO_RESTART`), `README.md` (Install, Upgrade, Uninstall, Daemon sections)
- Test: `crates/overseer/tests/install.rs`

**Interfaces:**
- Consumes: `scripts/bundle.sh`, `overseer install-agent [--yes] [--no-restart]`, `overseer uninstall-agent`.
- Produces: `scripts/install.sh` (POSIX `sh`, `set -eu`): Decision 6's Rust step; build unless `OVERSEER_INSTALL_BINARY` is set (binary at `${CARGO_TARGET_DIR:-$root/target}/release/overseer`); `bundle.sh <bin> "$HOME/Applications/Overseer.app"`; `ln -sfn` to `$HOME/.local/bin/overseer`; `install-agent` with `--yes` when `YES=1` and `--no-restart` when `NO_RESTART=1`; the closing summary of today's Makefile. `scripts/uninstall.sh`: `uninstall-agent` through the app binary when it exists; without it, the old `launchctl bootout` fallback runs only when `OVERSEER_HOME` is unset, and otherwise prints `No app binary; remove $OVERSEER_HOME/agent by hand.`; launchctl is `${OVERSEER_LAUNCHCTL:-launchctl}`; removes the link and the app as today.
- [ ] **Step 1: Write the failing tests** (`tests/install.rs`; `HOME=<tmp>/home`, `OVERSEER_HOME=<tmp>/state`, `OVERSEER_LAUNCHCTL` the Task 2 stub, `OVERSEER_BUILD=test-1`, `PATH=<tmp>/stubs:/usr/bin:/bin:/usr/sbin:/sbin`, stdin `/dev/null`):
  - `install_into_a_temp_home`: `OVERSEER_INSTALL_BINARY=<CARGO_BIN_EXE_overseer> scripts/install.sh` exits 0; `<tmp>/home/Applications/Overseer.app/Contents/MacOS/overseer` exists and `codesign --verify` passes; `Info.plist` has `test-1`; `<tmp>/home/.local/bin/overseer` is a symlink to the app's binary; `<tmp>/state/agent/uk.co.techize.overseer-home-*.plist` exists; the launchctl log never names the bare real label; stdout ends with the summary.
  - `install_without_cargo_installs_rust_first`: no `OVERSEER_INSTALL_BINARY`, no `cargo` on `PATH`, `CARGO_HOME=<tmp>/cargo`, `CARGO_TARGET_DIR=<tmp>/target`; a stub `curl` prints a script that writes `$CARGO_HOME/bin/cargo`, a stub that logs `$*` and copies `<CARGO_BIN_EXE_overseer>` to `$CARGO_TARGET_DIR/release/overseer`. Stdout has `Rust is not installed. Installing it for your user with rustup (no admin rights needed).` and `Add ~/.cargo/bin to your PATH to run cargo yourself.`; the cargo log has `build --release -p overseer`; the app is installed.
  - `uninstall_in_a_custom_home_never_touches_the_real_agent`: with no app binary, `scripts/uninstall.sh` prints `No app binary; remove <tmp>/state/agent by hand.` and the launchctl log is empty; after an install, it removes the rig plist, the link and the app.
- [ ] **Step 2: Run** `cargo test -p overseer --test install`. Expected: FAIL (no scripts).
- [ ] **Step 3: Implement** the scripts and the Makefile change. Rewrite the README sections: the rustup step, `make install YES=1` and `NO_RESTART=1`, the warning and resumable sessions (drop the stale "arrives in phase 2" line), `overseer ctl version` and `overseer ctl restart`, and that an `OVERSEER_HOME` install gets its own agent under `$OVERSEER_HOME/agent`.
- [ ] **Step 4: Run** `cargo test --workspace` and `make lint`. Expected: PASS.
- [ ] **Step 5: Commit** `Move install and uninstall into scripts that run against a temp home`.

### Task 5: Sprint name and day in the board header

**Files:**
- Create: `crates/overseer-daemon/src/work/sprint.rs`
- Modify: `crates/overseer-daemon/src/work/mod.rs`, `crates/overseer-daemon/src/work/store.rs`, `crates/overseer-daemon/src/config.rs`, `crates/overseer-proto/src/msg.rs`, `crates/overseer-gui/src/board.rs`, plus `sprint: None` in every `WorkView { .. }` literal

**Interfaces:**
- Consumes: `brain::paths::local_date`, the store's `Sprints/*` folder scan.
- Produces: `Config.sprint_name: String` (default empty), `Config.sprint_start: String` (default empty), `Config.sprint_days: u32` (default 10); `#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)] pub struct Ymd { pub y: i32, pub m: u32, pub d: u32 }`; `pub fn parse_ymd(s: &str) -> Option<Ymd>` (real calendar dates only); `pub fn newest_sprint_folder(brain: &Path) -> Option<String>`; `pub fn sprint_label(name: &str, start: Option<Ymd>, days: u32, today: Ymd) -> Option<String>` (Decision 11; `None` for an empty name); `WorkView.sprint: Option<String>` (`#[serde(default)]`), filled on every `view()` from the folder found at the last scan; GUI `pub fn header_title(work: &WorkView) -> Option<String>` shown muted after `Overseer`.
- [ ] **Step 1: Write the failing tests**
  - `sprint.rs`, with `start = 2026-11-09` (a Monday) and `days = 10`: `today 2026-11-09` gives `sprint-26-01, day 1 of 10`; `11-13` `day 5 of 10`; `11-14` (Saturday) `day 5 of 10`; `11-16` `day 6 of 10`; `11-20` `day 10 of 10`; `11-23` `sprint-26-01, ended Fri 20 Nov`; `11-06` `sprint-26-01, starts Mon 9 Nov`; a Saturday start `2026-11-14` with today `11-14` gives `starts Mon 16 Nov`; no start gives `sprint-26-01`; empty name gives `None`. `parse_ymd_rejects_impossible_dates` (`2026-02-29`, `2026-13-01`, `26-11-09` are `None`; `2028-02-29` is a date). `newest_sprint_folder_picks_the_last_by_name` (`sprint-26-01`, `sprint-26-02`, a folder `notes` and a file `sprint-26-03` gives `sprint-26-02`).
  - `config.rs`: `sprint_keys_default_and_load`.
  - `store.rs`: `view_names_the_sprint_from_the_folder_or_the_config` (a temp vault with `Sprints/sprint-26-01/`; then `sprint_name = "Demo sprint"` wins).
  - `board.rs`: `header_title_is_the_sprint_text`.
- [ ] **Step 2: Run** `cargo test -p overseer-daemon work::sprint config:: work::store && cargo test -p overseer-gui header_title`. Expected: FAIL.
- [ ] **Step 3: Implement.** Weekday from days-since-epoch (civil-from-days); month names `Jan` to `Dec`, weekday names `Mon` to `Sun`. A start that does not parse logs one warning at daemon start and shows the name alone.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Show the sprint name and day in the board header`.

### Task 6: Settings in the daemon

**Files:**
- Create: `crates/overseer-daemon/src/settings.rs`, `crates/overseer-proto/src/settings.rs`
- Modify: `crates/overseer-daemon/Cargo.toml` (`toml_edit = "0.22"`), `crates/overseer-daemon/src/lib.rs`, `crates/overseer-daemon/src/server.rs` (`DaemonConfig.files`, the two requests), `crates/overseer-proto/src/lib.rs`, `crates/overseer-proto/src/msg.rs`, `crates/overseer/src/main.rs` (builds `SettingsFiles`), `crates/overseer/src/ctl.rs`, and every `DaemonConfig { .. }` literal: `crates/overseer-daemon/tests/{server,brain}.rs`, `crates/overseer/tests/{mcp,hook,client}.rs`, `crates/overseer-gui/tests/{hub_reattach,hub_flood,reconnect}.rs` (each gets a temp config path)
- Test: `crates/overseer-daemon/tests/server.rs`, `crates/overseer/tests/cli.rs`

**Interfaces:**
- Consumes: `Config`, `Config::load`, `overseer_term::Palette::default()`, `regex::Regex`, Task 1 `DaemonInfo`, Task 5 `parse_ymd` and sprint keys.
- Produces (proto `settings.rs`, all `Serialize, Deserialize, Clone, Debug, PartialEq`):
  - `enum SettingKind { Text { allow_empty: bool }, Path, Pattern, Date, Bool, Int { min: i64, max: i64 }, Float { min: f64, max: f64 }, Colour, List }`
  - `enum Applies { App, Restart, ReadOnly }`
  - `enum SettingValue { Text(String), Bool(bool), Int(i64), Float(f64), List(Vec<String>) }`
  - `struct Setting { key: String, group: String, kind: SettingKind, applies: Applies, value: SettingValue, default: SettingValue, in_file: bool, restart_needed: bool, note: Option<String> }`
  - `struct SettingsView { config_path: PathBuf, file_hash: String, load_error: Option<String>, settings: Vec<Setting>, targets_path: PathBuf, targets_note: String, targets_error: Option<String>, daemon: DaemonInfo }`
  - `enum SettingEdit { Set { key: String, value: SettingValue }, Reset { key: String } }`
  - `pub const DEFAULT_FONT_FAMILY: &str = "Menlo"`, `DEFAULT_FONT_SIZE: f32 = 13.0`, `DEFAULT_EDITOR: &str = "Visual Studio Code"` (moved here; the GUI re-exports them)
  - `Request::GetSettings`, `Request::SaveSettings { base_hash: String, edits: Vec<SettingEdit> }`, `Response::Settings(SettingsView)`
- Produces (daemon `settings.rs`):
  - `pub struct SettingsFiles { pub config: PathBuf, pub config_error: Option<String>, pub targets: PathBuf, pub targets_note: String, pub targets_error: Option<String> }`; `DaemonConfig.files: SettingsFiles`. `main.rs` fills it from `config_path()`, `targets_path()`, the two load errors and `targets_note`: `<n> patterns in use, read from <path> at daemon start` or `<n> built-in patterns; <path> does not exist`.
  - `pub struct Spec { pub key: &'static str, pub group: &'static str, pub kind: SettingKind, pub applies: Applies, pub note: Option<&'static str>, pub get: Option<fn(&Config) -> SettingValue> }` and `pub const CATALOGUE: &[Spec]`, in this order and grouping: Work items (`jql` Text, `jira_poll_secs` Int 5..=3600, `beads_poll_secs` Int 5..=3600, `ticket_pattern` Pattern); Sprint (`sprint_name` Text empty allowed, `sprint_start` Date, `sprint_days` Int 1..=30); Locks and worktrees (`lock_idle_mins` Int 1..=10080, `worktree_root` Path); 2ndBrain (`brain_dir`, `beads_dir`, `current_session` Path; `journal_path`, `session_note` Text; `shared_files` List; `backup_mins` Int 0..=240; `push` Bool; `brain_writer` Bool ReadOnly); Commands (`jira_command`, `bd_command`, `sync_command` List ReadOnly); Terminal (`terminal.font_family` Text, `terminal.font_size` Float 4..=96, `terminal.background`, `terminal.foreground` Colour, `terminal.ansi` List ReadOnly); Editor (`editor.name` Text). Daemon keys are `Restart` unless read-only; terminal and editor keys are `App`.
  - `pub fn file_hash(text: &str) -> String` (FNV-1a 64 over every byte, 16 hex; `""` for a missing file)
  - `pub fn check(spec: &Spec, value: &SettingValue) -> Result<(), String>`
  - `pub fn read(files: &SettingsFiles, running: &Config, home: &Path, daemon: DaemonInfo) -> SettingsView` (`value` is the file's raw value, unexpanded, else the default; `restart_needed` compares `get` on the file's `Config::load` with `get` on `running`)
  - `pub fn save(files: &SettingsFiles, home: &Path, base_hash: &str, edits: &[SettingEdit]) -> Result<(), String>` (Decision 8)
  - Texts, exact: `<key> must be a whole number from <min> to <max>`; `<key> must be a number from <min> to <max>`; `<key> must be a colour like #1e1e2e`; `sprint_start must be a date like 2026-11-09, or empty`; `ticket_pattern is not a valid pattern: <first line of the regex error>`; `<key> cannot be empty`; `<key> is read-only here; edit config.toml`; `no setting <key>`; `<key> needs <text|a number|true or false|a list>`; `config.toml changed on disk since Settings read it. Close and reopen Settings to see the new text.`; `config.toml does not parse, so Settings will not change it: <error>`.
- Produces (ctl): `settings [--json]`, `settings set <key> <value>` (value read per kind: numbers, `true`/`false`, a list as a JSON array), `settings reset <key>`; both read the view first and send its `file_hash`. `pub fn format_settings(v: &SettingsView) -> Vec<String>`: `config.toml: <path>`, a line per group, then `  <key> = <value>  (<from the file|default>)` with `  restart needed` and `  read-only` when they apply, then `targets.toml: <note>` and any error lines.
- [ ] **Step 1: Write the failing tests**
  - `settings.rs` unit (temp dirs only):
    - `read_marks_file_values_and_defaults`; `restart_needed_when_the_file_differs_from_the_running_config`; `app_keys_never_need_a_restart`.
    - `save_keeps_comments_and_unknown_keys`: file `# my notes\njql = "project = DEMO"\n[custom]\nfoo = 1\n[terminal]\nfont_size = 12.0\n`; set `jql` and `terminal.font_size = 14.0`; the new text still has `# my notes`, `[custom]` and `foo = 1` in their places and the two new values.
    - `save_creates_a_missing_file_and_its_folder` (base hash `""`).
    - `save_keeps_a_symlinked_config_a_symlink`: `config.toml` a link to `dots/config.toml`; after a save, `symlink_metadata` still says symlink and the target holds the new value.
    - `save_refuses_when_the_file_changed_since_read`: read, then rewrite the file by hand, then save with the old hash: the stale text, and the file is the hand-written one.
    - `invalid_value_is_refused_and_the_file_is_untouched`: one case per constraint text above (`jira_poll_secs = 2`, `terminal.font_size = 200.0`, `terminal.background = "blue"`, `sprint_start = "2026-13-01"`, `ticket_pattern = "("`, `jql = ""`, `push = Text("yes")`), each leaving the bytes unchanged.
    - `read_only_keys_are_refused` (`brain_writer`, `jira_command`); `unknown_key_is_refused`; `reset_removes_the_key`.
    - `unparseable_file_is_never_rewritten` (`jql = [not toml`: the parse text, bytes unchanged).
    - `catalogue_daemon_keys_are_config_fields` (for every spec with `get`, a file holding only that key with its default value loads with no error and `get` gives the default back).
  - `tests/server.rs`: `settings_round_trip_over_the_socket` (`GetSettings` shows the temp config path; `SaveSettings` of `jql` answers a view with `jql` `restart_needed` true; a stale hash answers the stale `Error`).
  - `tests/cli.rs`: `ctl_settings_set_and_reset` (`settings set lock_idle_mins 15` prints `  lock_idle_mins = 15  (from the file)  restart needed`; `settings reset lock_idle_mins` shows `(default)`); `ctl_settings_refuses_a_bad_value` (exit 1, `lock_idle_mins must be a whole number from 1 to 10080`).
- [ ] **Step 2: Run** `cargo test -p overseer-daemon settings:: && cargo test -p overseer-daemon --test server settings_ && cargo test -p overseer --test cli ctl_settings_`. Expected: FAIL.
- [ ] **Step 3: Implement.** `save`: read the file (missing is `""`), compare `file_hash`, parse with `toml_edit::DocumentMut`, apply each edit (creating the `terminal` or `editor` table when needed), run `check`, then `toml::from_str::<Config>` on the result, then write: resolve the path with `canonicalize` when it is a symlink, create the parent folder, write `.<name>.tmp-<pid>` beside the target and `rename` it. A `Float` setting accepts an `Int` value. Paths and dates travel as `Text`.
- [ ] **Step 4: Run** the Step 2 command, then `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Read and save settings through the daemon without clobbering config.toml`.

### Task 7: Locks view (GUI)

**Files:**
- Create: `crates/overseer-gui/src/locks_view.rs`
- Modify: `crates/overseer-gui/src/app.rs` (`Screen::Locks`, action `GoLocks`, bindings, render, status), `crates/overseer-gui/src/board.rs` (the strip's lock count), `crates/overseer-gui/src/lib.rs`

**Interfaces:**
- Consumes: `LockView` (claims, waits, `flagged`), `SlotInfo`, `Request::ForceRelease`, `Event::LocksChanged`, the app's `status`.
- Produces:
  - `pub struct LockRow { pub target: String, pub holder: Option<SlotId>, pub holder_text: String, pub kind: &'static str, pub since: String, pub why: Option<String>, pub pin: Option<String>, pub long_held: bool, pub waiters: Vec<String> }`
  - `pub fn lock_rows(view: &LockView, slots: &[SlotInfo], now: i64, clock: impl Fn(i64) -> String) -> Vec<LockRow>`: claims oldest first, then targets with only waiters by their first wait; `holder_text` `S1 demo lane` (or `S1`, or `nobody` for a waiter-only target); `kind` `claim`, `command`, `grant` or `pin`; `since` `since 08:40 (2h 5m)` from `pub fn age_text(secs: i64) -> String` (`12m`, `2h 5m`, `3d 4h`); `why` `for DEMO-33` (item, else reason); `pin` `at <revision>`; `waiters` `1. S4 since 09:10, for sync`
  - `pub fn locks_summary(view: &LockView) -> String`: `3 held, 1 waiting`, `1 held`, `Nothing is held and nobody is waiting.`; `pub fn locks_strip(view: &LockView) -> String`: `Locks: 3 held, 1 waiting (l)` or `Locks: none held (l)`
  - `#[derive(Default)] pub struct LocksState` with `pub fn key(&mut self, key: &str, rows: &[LockRow]) -> LocksAction`, `pub fn prompt(&self) -> Option<String>`, `pub fn on_change(&mut self, rows: &[LockRow]) -> Option<String>`, `pub fn selected(&self, rows: &[LockRow]) -> Option<usize>`; `#[derive(Debug, PartialEq)] pub enum LocksAction { Redraw, Send(Request), Back, Status(String), Ignored }`
  - Keys: `up` and `down` move (clamped, selection kept by target name); `enter` or `f` opens the confirm on a held row, `Release S1's argo:uat/root? S4 is next in the queue. Enter / Esc` or `... Nobody is waiting. Enter / Esc`, and on a waiter-only row gives `Status("Nobody holds <target>; nothing to release")`; in the confirm `enter` sends `ForceRelease { target, holder: Some(<the holder shown>) }` and `escape` cancels; `escape` otherwise is `Back`. `on_change` closes an open confirm whose target now has another holder or none and returns `<target> changed hands; look again`.
  - Bindings: `KeyBinding::new("l", GoLocks, Some("Board"))`, `KeyBinding::new("cmd-shift-l", GoLocks, nav)`; the view's key context is `Locks`; footer `Up and Down pick a target · Enter or f releases it · Esc goes back`. The daemon's reply text goes to `status` as it is (`Released argo:uat/root. S4 holds it now.`, or the error).
- [ ] **Step 1: Write the failing tests** (`locks_view.rs`, pure; routing in `app.rs` tests):
  - `rows_list_holders_kinds_reasons_and_waiters_in_order`; `waiter_only_target_comes_last_held_by_nobody`; `long_held_rows_are_flagged` (with `flag_after_secs` 3600 and `last_used` two hours ago); `pin_shows_its_revision`; `age_text_steps` (`59` `0m`, `3600` `1h 0m`, `90000` `1d 1h`); `summary_and_strip_texts` (each text above).
  - `confirm_sends_the_holder_it_showed`: select S1's `argo:uat/root` with S4 waiting; the prompt is the exact text; `enter` gives `Send(ForceRelease { target: "argo:uat/root", holder: Some(1) })`.
  - `confirm_drops_when_the_target_changes_hands`: open the confirm, then `on_change` with rows where S4 holds it: returns `argo:uat/root changed hands; look again`, `prompt()` is `None`, and a following `enter` opens a new confirm for S4 rather than sending.
  - `selection_follows_the_target_when_rows_reorder`; `escape_cancels_the_confirm_then_goes_back`; `waiter_only_row_has_nothing_to_release`.
  - `app.rs`: `l_opens_locks_from_the_board_only` (`l` on `BOARD` routes to `GoLocks`, on `TERMINAL` to nothing); `cmd_shift_l_opens_locks_everywhere_but_modals` (`TERMINAL`, `CHROME`, `BOARD` route; `DIALOG`, `PICKER`, `PLAN` do not); `locks_keys_reach_the_view` (`escape`, `enter`, `f`, `up`, `down` route to nothing in `["Overseer", "Locks"]`; `cmd-1` still routes to `GoS1`).
- [ ] **Step 2: Run** `cargo test -p overseer-gui locks_view && cargo test -p overseer-gui app::tests`. Expected: FAIL.
- [ ] **Step 3: Implement.** `render_locks` lists the rows (selected row with the accent border, `held long` in orange, waiters indented), the summary as the title and the confirm prompt above the footer. On `LocksChanged` the app calls `on_change` and puts any text in `status`. The board strip's `locks_strip` text is clickable and opens the view.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the locks view with a guarded force release`.

### Task 8: Settings view (GUI)

**Files:**
- Create: `crates/overseer-gui/src/settings_view.rs`
- Modify: `crates/overseer-gui/src/app.rs` (action `OpenSettings`, `cmd-,`, menu item, `modal_open`, `NAV` gains `&& !Settings`, `DaemonInfo` at connect, restart), `crates/overseer-gui/src/theme.rs`, `crates/overseer-gui/src/terminal_view.rs` (`set_theme`, `load_editor`, `DEFAULT_EDITOR` re-export), `crates/overseer-gui/src/lib.rs`, `README.md` (a Settings section)

**Interfaces:**
- Consumes: Task 6 types and requests, `CATALOGUE` (dev-dependency, tests only), Task 1 `build_notice` and `current_build`, Task 3 `ctl restart --yes`, `TextField` from `start_dialog.rs`, the modal pattern of `open_plan`.
- Produces:
  - `pub struct SettingsDraft` with `pub fn new(view: SettingsView) -> Self`, `pub fn set_text(&mut self, key: &str, text: &str)`, `pub fn toggle(&mut self, key: &str)`, `pub fn reset(&mut self, key: &str)`, `pub fn dirty(&self) -> bool`, `pub fn edits(&self) -> Result<Vec<SettingEdit>, String>` (text read per kind with the daemon's texts, lists split on commas and trimmed), `pub fn rows(&self) -> Vec<SettingRow>`
  - `pub fn saved_text(after: &SettingsView, edited: &[String]) -> String`: `Saved.`, or `Saved. The daemon uses the new jql and lock_idle_mins after it restarts.` (edited keys that are `restart_needed`, joined with `, ` and a final `and`)
  - `pub fn restart_confirm(slots: &[SlotInfo]) -> String`: `Restart the daemon? 2 running sessions end and come back as resumable: S1 demo lane, S3 docs.` or `Restart the daemon? No session is running.`
  - `pub fn app_keys_changed(edits: &[SettingEdit]) -> bool` (any `terminal.` or `editor.` key)
  - entity `SettingsPanel` emitting `SettingsEvent::{Saved { text: String, app_changed: bool }, Closed, Restart}`; layout: one block per group, each row `key`, the field (or `true`/`false` toggle, or the read-only value with its note), `default` or `from the file`, `restart needed` in orange, a `Reset` link when `in_file`; a Files block with both paths, `targets_note`, any error in red and Open buttons (`open -t <path>`, only for files that exist); an About block with `App build <a>` and `Daemon build <d>, pid <p>, running since HH:MM`; buttons `Save` (Cmd+S), `Restart the daemon` (opens `restart_confirm`, then emits `Restart`) and `Close` (Esc, per Decision 13). A daemon error after Save shows in red in the view and the view stays open.
  - `Theme::config_path()` returns `Some(overseer_proto::paths::config_path())`; `load_editor` uses it.
  - `pub fn set_theme(&mut self, theme: Theme, cx: &mut Context<Self>)` on `TerminalView`: replaces the theme, clears `metrics` so the next paint measures and resizes, notifies.
  - App: on `Saved` with `app_changed`, `Theme::load()` and `set_theme` on every terminal view; on `Restart`, spawn `std::env::current_exe()` with `ctl restart --yes` (stdout and stderr to `gui.log`), status `Restarting the daemon`; the existing reconnect does the rest. At connect, request `DaemonInfo` and put `build_notice(current_build(), &info.build)` in `status` when it is `Some`.
- [ ] **Step 1: Write the failing tests**
  - `settings_view.rs`: `draft_starts_clean`; `editing_marks_dirty_and_produces_typed_edits` (`jira_poll_secs` `"60"` gives `Set { Int(60) }`, `terminal.font_size` `"14"` gives `Float(14.0)`, `shared_files` `"a.md, b/*.md"` gives `List(["a.md", "b/*.md"])`); `bad_number_is_caught_before_sending` (`"sixty"` gives `jira_poll_secs must be a whole number from 5 to 3600`); `reset_produces_a_reset_edit`; `read_only_rows_are_not_editable` (`set_text("brain_writer", ..)` leaves the draft clean); `saved_text_names_restart_keys`; `restart_confirm_lists_running_sessions` (both texts).
  - `app.rs`: `cmd_comma_opens_settings_from_everywhere_but_modals`; `keys_reach_the_settings_view` (`escape`, `cmd-s`, `tab`, `cmd-a`, `enter` route to nothing in `["Overseer", "Settings"]`); `cmd_digits_do_not_route_while_settings_open`.
  - `theme.rs`: `theme_and_daemon_read_the_same_config_file` (`Theme::config_path() == Some(overseer_proto::paths::config_path())`); `catalogue_terminal_keys_parse_in_theme` (for each `terminal.*` spec that is not read-only, a `[terminal]` table holding its default parses with `Theme::from_toml`, so a catalogue key and the theme's `deny_unknown_fields` cannot drift apart).
- [ ] **Step 2: Run** `cargo test -p overseer-gui settings_view theme:: app::tests`. Expected: FAIL.
- [ ] **Step 3: Implement.** Open sends `GetSettings` with `request_marked` and builds the panel from the reply; Save sends `SaveSettings { base_hash: view.file_hash, edits }` and, on `Response::Settings`, rebuilds the draft from the new view and emits `Saved`. Add the README section: where the file is, which keys apply at once and which after a restart, the read-only rows, and that hand edits stay safe because the view refuses to save over a file that changed.
- [ ] **Step 4: Run** `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** `Add the settings view`.

### Task 9: Acceptance list

**Files:**
- Create: `docs/acceptance/phase-7.md`

- [ ] **Step 1: Write the list** in the style of `docs/acceptance/phase-6.md` (intro, Risk paragraph, Steps 0 to 6, tables with ID, Check, Steps, Pass, Result, Known limitations, Sign-off). The rig is `/tmp/ov-hand7` with `export OVERSEER_HOME=/tmp/ov-hand7`, `OVERSEER_CONFIG` and `OVERSEER_TARGETS` under it, `RH=$OVERSEER_HOME/home` (the rig's `HOME` for installs and the app), stub `bd`, Jira and `claude` as in phase 6, a test vault with `Sprints/sprint-26-01/`, and the isolated prefix `HOME=$RH SHELL=/bin/sh CLAUDE_CONFIG_DIR=$OVERSEER_HOME/claude-config`. Before any `HOME=$RH` command export the real `CARGO_HOME` and `RUSTUP_HOME`. Build once with the real environment (`cargo build --release -p overseer`) and install with `OVERSEER_INSTALL_BINARY=$PWD/target/release/overseer`. Never `open` the rig bundle or start it from Finder (LaunchServices would start it with your real environment, against the real socket); always run `$RH/Applications/Overseer.app/Contents/MacOS/overseer` with the prefix. Step 0 records, beside phase 6's list: the installed app's binary and `Info.plist` checksums, `readlink ~/.local/bin/overseer`, the real plist's checksum, `launchctl list | awk '$3 == "uk.co.techize.overseer"'` (exact match: the rig's label shares the prefix), and checksums of `~/.config/overseer/config.toml` and `targets.toml` when they exist. Rows:
  - **Install and upgrade:** I1 fresh install into the rig (`OVERSEER_BUILD=rig-a`, prefix, `make install`): app, link, signature, build `rig-a` in `ctl version`, the plist at `$OVERSEER_HOME/agent/uk.co.techize.overseer-home-*.plist` with the isolating env, `launchctl print gui/$(id -u)/<rig label>` running, the real label's line unchanged. I2 Rust missing: a `PATH` with no cargo, `CARGO_HOME` under the rig and a stub `curl`, so nothing is downloaded; the two Decision 6 lines and the stub cargo's `build --release -p overseer`. I3 autostart through the rig agent: `ctl shutdown`, then a bare `ctl status` with only `OVERSEER_HOME` set starts the rig agent's daemon, and a stub session's environment shows `HOME=$RH` (phase 6's hazard gone). U1 upgrade with no session running (`rig-b`): restarts without asking. U2 upgrade with S1 and S3 running and stdin from `/dev/null` (`rig-c`): the exact warning and keep texts, both sessions still running, `ctl version` shows daemon `rig-b` and app `rig-c` with the notice, and the app's status line shows the notice at connect. U3 the prompt: answer `n` (kept), then `make install YES=1`: `Restarted the daemon (build rig-b to rig-c).`, S1 and S3 resumable on the board, `r` resumes S1. U4 `ctl restart` with a running session: warning and prompt on a terminal, `--yes` restarts. U5 uninstall: `make uninstall` with the prefix removes the rig app, link and plist, the rig label is gone from `launchctl list`, the state folder stays.
  - **Locks view (GUI):** L1 `l` on the board, Cmd+Shift+L from a session terminal (the stub's screen gets no `l`), Esc back to the board, Cmd+1 from the view. L2 rows: S1 claim on `argo:uat/root` for `DEMO-12` with S4 and S5 queued, S3 on `repo:demo-api:main`, a waiter-only target, `lock_idle_mins = 1` (then a daemon restart) for `held long` after two minutes; every text as Task 7 quotes it. L3 force release: the confirm text, Enter, status `Released argo:uat/root. S4 holds it now.`, the relays to S1 and S4 in `ctl locks`. L4 changed hands: open the confirm, then `ctl force-release argo:uat/root` in the terminal: the confirm closes with `argo:uat/root changed hands; look again` and nothing more is released. L5 the board strip count and its click.
  - **Settings (GUI and ctl):** S1 open with Cmd+, from the board and a session and from the app menu; values match the rig `config.toml`, `from the file` and `default` marks, the read-only rows with their notes. S2 save `jql` and `lock_idle_mins` into a `config.toml` that has a comment line and an unknown table: `diff` shows only those two lines changed; `Saved. The daemon uses the new jql and lock_idle_mins after it restarts.`; `ctl settings` shows `restart needed`. S3 `terminal.font_size` and `terminal.background` apply at once to an open terminal; `editor.name` is used by the next Cmd+click. S4 bad values refused with the exact Task 6 texts and the file's checksum unchanged. S5 a hand edit while the view is open: Save refuses with the stale text. S6 a symlinked `config.toml`: after Save it is still a link and the target has the value. S7 Reset removes the key. S8 `targets.toml`: missing gives the built-in note; a broken one plus a restart shows its error; Open opens the rig file. S9 Restart the daemon from Settings: the confirm text, the app reconnects, `restart needed` clears, sessions resumable.
  - **Header:** H1 `sprint-26-01` from the folder; with `sprint_start` set to last Monday the day text; `sprint_name` overriding; a future start (`starts`); a past sprint (`ended`).
  - **X1** the full suite three times, exactly as phase 6's X1 (clean `PATH`, the rig variables unset, a fresh `OVERSEER_HOME` per run), adding `-u RH` and `-u OVERSEER_LAUNCHCTL`.
  - **Z1** nothing leaked: phase 6's checks plus the Step 0 checksums of the installed app and its `Info.plist`, the `~/.local/bin/overseer` link, the real plist, the exact real-label line, the real `~/.config/overseer` files, `launchctl list | grep -c overseer-home-` printing 0 after U5, and nothing under the real `~/Library/LaunchAgents` named `overseer-home`.
  - Known limitations: daemon settings apply only after a restart; `brain_writer` and the command lists are read-only in the view; the Locks view cannot remove a waiter; the sprint day comes from `sprint_start`, not Jira; an isolated home's agent is not loaded at login.
- [ ] **Step 2: Check** `grep -c $'\u2014' docs/acceptance/phase-7.md` prints 0 and every row ends `| pending |`.
- [ ] **Step 3: Commit** `Add the phase 7 acceptance list`.
