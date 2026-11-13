//! The 2ndBrain writer service against a throwaway vault with a bare remote,
//! a stub `bd` and a stub sync script that log their arguments and env.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use overseer_daemon::brain::paths::{fill_template, Owners};
use overseer_daemon::brain::{BrainContext, BrainOptions, BrainService, WRITER_OFF};
use overseer_daemon::ledger::Ledger;
use overseer_daemon::Config;
use overseer_proto::{
    read_frame, write_frame, Event, Frame, QueueState, Request, Response, Sender, WriteCheck,
};
use tokio::sync::broadcast;
use tokio::time::{sleep, Instant};

const WAIT: Duration = Duration::from_secs(10);
const DRAIN: Duration = Duration::from_millis(300);

fn sh(dir: &Path, args: &[&str]) -> String {
    let o = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn script(p: &Path, body: &str) {
    write(p, body);
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
}

const SYNC_REL: &str = "05-Meta/Sync-System/sync-run.sh";

struct Vault {
    base: PathBuf,
    vault: PathBuf,
    remote: PathBuf,
}

impl Vault {
    fn new(tag: &str) -> Vault {
        let base = std::env::temp_dir().join(format!("ov-brain-{tag}-{}", uuid_like()));
        let remote = base.join("remote.git");
        let vault = base.join("vault");
        std::fs::create_dir_all(&remote).unwrap();
        std::fs::create_dir_all(&vault).unwrap();
        sh(&remote, &["init", "--bare", "-b", "main"]);
        sh(&vault, &["init", "-b", "main"]);
        for (k, v) in [
            ("user.name", "Test"),
            ("user.email", "test@example.com"),
            ("commit.gpgsign", "false"),
            ("pull.rebase", "false"),
        ] {
            sh(&vault, &["config", k, v]);
        }
        sh(
            &vault,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        write(&vault.join("README.md"), "vault\n");
        let log = base.join("sync.log");
        script(
            &vault.join(SYNC_REL),
            &format!(
                "#!/bin/sh\necho \"OVERSEER_COMMIT=$OVERSEER_COMMIT $*\" >> '{}'\necho sync done\n",
                log.display()
            ),
        );
        let bd_log = base.join("bd.log");
        script(
            &base.join("bd-stub.sh"),
            &format!(
                "#!/bin/sh\necho \"$(pwd -P)|$OVERSEER_COMMIT|$*\" >> '{}'\necho bd said $1\n",
                bd_log.display()
            ),
        );
        sh(&vault, &["add", "-A"]);
        sh(&vault, &["commit", "-m", "initial"]);
        sh(&vault, &["push", "-u", "origin", "main"]);
        Vault {
            base,
            vault,
            remote,
        }
    }

    fn config(&self) -> Config {
        Config {
            brain_writer: true,
            brain_dir: self.vault.clone(),
            beads_dir: self.vault.clone(),
            current_session: self.base.join("home/.claude/CURRENT_SESSION.md"),
            bd_command: vec![self.base.join("bd-stub.sh").to_string_lossy().into_owned()],
            jira_command: vec![],
            ..Config::default()
        }
    }

    fn options(&self) -> BrainOptions {
        BrainOptions {
            drain: DRAIN,
            backup: None,
            retry: Duration::from_millis(100),
            lock_wait: Duration::from_millis(200),
            home: self.base.join("home"),
        }
    }

    fn service(&self, ledger: Arc<Ledger>, opts: BrainOptions) -> Arc<BrainService> {
        let (events, _) = broadcast::channel(256);
        BrainService::with_options(&self.config(), ledger, events, opts).expect("writer is on")
    }

    fn commits(&self) -> usize {
        sh(&self.vault, &["rev-list", "--count", "HEAD"])
            .trim()
            .parse()
            .unwrap()
    }

    fn log(&self, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.base.join(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn status(&self) -> String {
        sh(
            &self.vault,
            &["status", "--porcelain", "--untracked-files=all"],
        )
    }
}

impl Drop for Vault {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "{}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

async fn until(what: &str, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(25)).await;
    }
}

fn text(r: &Response) -> String {
    match r {
        Response::Text { text } => text.clone(),
        other => panic!("expected text, got {other:?}"),
    }
}

fn error(r: &Response) -> String {
    match r {
        Response::Error { message } => message.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

const S1_NOTE: &str = "Sessions/2026/11-November/2026-11-13-s1-demo.md";
const S2_NOTE: &str = "Sessions/2026/11-November/2026-11-13-s2-demo.md";
const S2_FOLDER: &str = "Sprints/sprint-26-01/DEMO-12-demo-lane";

fn owners() -> Owners {
    Owners {
        notes: vec![(1, S1_NOTE.into()), (2, S2_NOTE.into())],
        folders: vec![(2, S2_FOLDER.into(), "DEMO-12".into())],
    }
}

/// A context the test can change while the service runs.
fn shared_context(svc: &BrainService, ctx: BrainContext) -> Arc<Mutex<BrainContext>> {
    let cell = Arc::new(Mutex::new(ctx));
    let read = cell.clone();
    svc.set_context(move || read.lock().unwrap().clone());
    cell
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_commit_is_queued_committed_and_pushed() {
    let v = Vault::new("commit");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    write(&v.vault.join(S1_NOTE), "# S1\n");
    write(&v.vault.join("unrelated.md"), "Jonathan's\n");
    let before = v.commits();
    let r = svc
        .commit(
            Sender::Slot { slot: 1 },
            vec![S1_NOTE.into()],
            "S1: session note \u{2014} first".into(),
            &owners(),
        )
        .await;
    assert!(text(&r).contains("committed"), "{r:?}");
    assert_eq!(v.commits(), before + 1);
    let body = sh(&v.vault, &["log", "-1", "--format=%B"]);
    let id = svc.view().entries[0].id;
    assert!(body.starts_with("S1: session note - first\n"), "{body}");
    assert!(
        body.trim_end().ends_with(&format!("Overseer-Queue: {id}")),
        "{body}"
    );
    let files = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), S1_NOTE);
    assert!(v.status().contains("unrelated.md"), "other edits untouched");
    until("the push", || {
        let e = &svc.view().entries[0];
        e.state == QueueState::Committed && e.pushed == Some(true)
    })
    .await;
    let remote = sh(&v.remote, &["log", "--format=%s", "main"]);
    assert!(remote.contains("S1: session note - first"), "{remote}");
    let e = &svc.view().entries[0];
    assert_eq!(e.from, Sender::Slot { slot: 1 });
    assert_eq!(e.paths, vec![S1_NOTE.to_string()]);
    let head = sh(&v.vault, &["rev-parse", "HEAD"]);
    assert_eq!(e.commit.as_deref(), Some(head.trim()));
    assert!(svc.view().on);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_refuses_another_slots_folder_and_shared_files() {
    let v = Vault::new("refuse");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let journal = "06-Daily/2026/11-November/2026-11-13-journal.md";
    let folder_file = format!("{S2_FOLDER}/notes.md");
    for rel in [S2_NOTE, journal, folder_file.as_str()] {
        write(&v.vault.join(rel), "x\n");
    }
    let before = v.commits();
    let s1 = Sender::Slot { slot: 1 };
    let r = svc
        .commit(s1, vec![S2_NOTE.into()], "S1: x".into(), &owners())
        .await;
    assert_eq!(
        error(&r),
        format!("{S2_NOTE} belongs to S2 (session note). Ask Jonathan before writing it.")
    );
    let r = svc
        .commit(s1, vec![folder_file.clone()], "S1: x".into(), &owners())
        .await;
    assert_eq!(
        error(&r),
        format!("{folder_file} belongs to S2 (DEMO-12). Ask Jonathan before writing it.")
    );
    let r = svc
        .commit(s1, vec![journal.into()], "S1: x".into(), &owners())
        .await;
    assert_eq!(
        error(&r),
        format!(
            "{journal} is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement."
        )
    );
    let r = svc
        .commit(s1, vec!["../outside.md".into()], "S1: x".into(), &owners())
        .await;
    assert!(error(&r).contains("outside 2ndBrain"), "{r:?}");
    assert_eq!(v.commits(), before, "nothing committed");

    // The hook's check gives the same texts, and lets a slot write its own.
    let check = |paths: Vec<PathBuf>| WriteCheck {
        slot: 1,
        tool: "Write".into(),
        paths,
        cwd: Some(v.vault.clone()),
    };
    assert_eq!(
        svc.check_write(&check(vec![v.vault.join(S2_NOTE)]), &owners()),
        Some(format!(
            "{S2_NOTE} belongs to S2 (session note). Ask Jonathan before writing it."
        ))
    );
    assert_eq!(
        svc.check_write(&check(vec![PathBuf::from(journal)]), &owners()),
        Some(format!(
            "{journal} is a shared 2ndBrain file. Use append_journal for the journal or edit_shared_file for an exact replacement."
        ))
    );
    assert_eq!(
        svc.check_write(&check(vec![v.config().current_session]), &owners()),
        Some(
            "CURRENT_SESSION.md is written by Overseer. Use update_current_session for your section."
                .to_string()
        )
    );
    assert_eq!(
        svc.check_write(&check(vec![v.vault.join(S1_NOTE)]), &owners()),
        None
    );
    assert_eq!(
        svc.check_write(&check(vec![PathBuf::from("/tmp/elsewhere.md")]), &owners()),
        None
    );
    // A folder holding another slot's files is not writable as a whole.
    assert!(svc
        .check_write(&check(vec![v.vault.join("Sprints")]), &owners())
        .is_some_and(|d| d.contains("belongs to S2")));

    // Jonathan has no ownership check.
    let r = svc
        .commit(Sender::You, vec![S2_NOTE.into()], "note".into(), &owners())
        .await;
    assert!(text(&r).contains("committed"), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_of_an_unknown_path_is_an_error() {
    let v = Vault::new("unknown");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let before = v.commits();
    let r = svc
        .commit(
            Sender::Slot { slot: 1 },
            vec!["never-existed.md".into()],
            "S1: x".into(),
            &owners(),
        )
        .await;
    assert!(!error(&r).is_empty());
    assert_eq!(v.commits(), before);
    assert_eq!(svc.view().entries[0].state, QueueState::Failed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_append_commits_the_journal() {
    let v = Vault::new("journal");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let r = svc
        .append_journal(
            Sender::Slot { slot: 2 },
            "Work Log",
            "- shipped \u{2014} done",
        )
        .await;
    assert!(text(&r).contains("committed"), "{r:?}");
    let rel = fill_template(&v.config().journal_path, now(), 0, "");
    let file = std::fs::read_to_string(v.vault.join(&rel)).unwrap();
    assert!(file.starts_with("# Journal - "), "{file}");
    assert!(file.contains("## Work Log\n\n- shipped - done\n"), "{file}");
    let subject = sh(&v.vault, &["log", "-1", "--format=%s"]);
    assert_eq!(subject.trim(), "S2: journal, Work Log");
    let files = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), rel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edit_shared_replaces_once_and_commits() {
    let v = Vault::new("edit");
    let rel = "Sprints/sprint-26-01/SPRINT-DASHBOARD.md";
    write(&v.vault.join(rel), "- DEMO-12 todo\n");
    sh(&v.vault, &["add", "-A"]);
    sh(&v.vault, &["commit", "-m", "dashboard"]);
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let r = svc
        .edit_shared(Sender::Slot { slot: 3 }, rel, "todo", "done")
        .await;
    assert!(text(&r).contains("committed"), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(v.vault.join(rel)).unwrap(),
        "- DEMO-12 done\n"
    );
    let subject = sh(&v.vault, &["log", "-1", "--format=%s"]);
    assert_eq!(subject.trim(), "S3: edit SPRINT-DASHBOARD.md");
    let r = svc
        .edit_shared(Sender::Slot { slot: 3 }, rel, "todo", "done")
        .await;
    assert!(error(&r).starts_with("old_text was not found"), "{r:?}");
    let r = svc
        .edit_shared(Sender::Slot { slot: 3 }, "README.md", "vault", "x")
        .await;
    assert!(error(&r).contains("not a shared 2ndBrain file"), "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_session_sections_are_written_and_cleared() {
    let v = Vault::new("current");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let cs = v.config().current_session;
    write(&cs, "Jonathan's notes\n");
    assert_eq!(
        svc.update_current_session(3, "demo lane", "working on DEMO-12")
            .await,
        Response::Ok
    );
    let file = std::fs::read_to_string(&cs).unwrap();
    assert!(
        file.contains("## S3 demo lane\nworking on DEMO-12\n"),
        "{file}"
    );
    assert!(file.ends_with("Jonathan's notes\n"), "{file}");
    svc.clear_current_session(3).await;
    assert_eq!(std::fs::read_to_string(&cs).unwrap(), "Jonathan's notes\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_session_symlinked_into_the_vault_is_still_current_session() {
    let v = Vault::new("cslink");
    let target_rel = "Claude/MEMORY/State/CURRENT_SESSION.md";
    let target = v.vault.join(target_rel);
    write(&target, "state\n");
    sh(&v.vault, &["add", "-A"]);
    sh(&v.vault, &["commit", "-m", "state"]);
    let cs = v.config().current_session;
    std::fs::create_dir_all(cs.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &cs).unwrap();
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let deny =
        "CURRENT_SESSION.md is written by Overseer. Use update_current_session for your section.";
    let check = WriteCheck {
        slot: 1,
        tool: "Edit".into(),
        paths: vec![target.clone()],
        cwd: None,
    };
    assert_eq!(svc.check_write(&check, &owners()).as_deref(), Some(deny));
    let r = svc
        .commit(
            Sender::Slot { slot: 1 },
            vec![target_rel.into()],
            "S1: x".into(),
            &owners(),
        )
        .await;
    assert_eq!(error(&r), deny);

    // Writes go through the link, and the backup commits the target.
    svc.update_current_session(1, "demo", "on it").await;
    assert!(std::fs::symlink_metadata(&cs)
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::read_to_string(&target).unwrap().contains("on it"));
    svc.backup_now().await;
    let files = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), target_rel);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_runs_once_after_the_queue_drains() {
    let v = Vault::new("sync");
    // Long enough that each commit and its push finish inside it under a
    // loaded full-suite run.
    let drain = Duration::from_millis(1500);
    let svc = v.service(
        Arc::new(Ledger::in_memory()),
        BrainOptions {
            drain,
            ..v.options()
        },
    );
    let ctx = shared_context(&svc, BrainContext::default());
    for i in 0..3 {
        let rel = format!("free-{i}.md");
        write(&v.vault.join(&rel), "x\n");
        let r = svc
            .commit(
                Sender::You,
                vec![rel],
                format!("free {i}"),
                &Owners::default(),
            )
            .await;
        assert!(text(&r).contains("committed"), "{r:?}");
    }
    until("the sync", || v.log("sync.log").len() == 1).await;
    sleep(drain * 3).await;
    assert_eq!(v.log("sync.log"), vec!["OVERSEER_COMMIT=1 --auto"]);
    until("the sync record", || svc.view().last_sync.is_some()).await;
    assert!(svc.view().last_sync.unwrap().1);

    // A working slot with an uncommitted path of its own holds the sync back.
    let s3_note = "Sessions/s3.md";
    write(&v.vault.join(s3_note), "half done\n");
    *ctx.lock().unwrap() = BrainContext {
        owners: Owners {
            notes: vec![(3, s3_note.into())],
            folders: vec![],
        },
        running: vec![3],
        busy: vec![3],
    };
    write(&v.vault.join("free-3.md"), "x\n");
    svc.commit(
        Sender::You,
        vec!["free-3.md".into()],
        "free 3".into(),
        &Owners::default(),
    )
    .await;
    sleep(drain * 4).await;
    assert_eq!(v.log("sync.log").len(), 1, "held back by S3");

    // Once that path is committed the next drain runs it.
    let owners = ctx.lock().unwrap().owners.clone();
    svc.commit(
        Sender::Slot { slot: 3 },
        vec![s3_note.into()],
        "S3: note".into(),
        &owners,
    )
    .await;
    until("the second sync", || v.log("sync.log").len() == 2).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_takes_free_and_idle_paths_only() {
    let v = Vault::new("backup");
    let opts = BrainOptions {
        backup: Some(Duration::from_millis(400)),
        ..v.options()
    };
    let svc = v.service(Arc::new(Ledger::in_memory()), opts);
    shared_context(
        &svc,
        BrainContext {
            owners: owners(),
            // S1 is idle, S2 is working.
            running: vec![1, 2],
            busy: vec![2],
        },
    );
    write(&v.vault.join("free.md"), "free\n");
    write(&v.vault.join(S1_NOTE), "idle\n");
    write(&v.vault.join(S2_NOTE), "working\n");
    write(&v.vault.join("locked.md"), "half\n");
    let guard = svc.lock_file(&v.vault.join("locked.md")).await;
    until("the backup commit", || {
        sh(&v.vault, &["log", "-1", "--format=%s"]).starts_with("vault backup: ")
    })
    .await;
    let subject = sh(&v.vault, &["log", "-1", "--format=%s"]);
    let re = regex::Regex::new(r"^vault backup: \d{4}-\d{2}-\d{2} \d{2}:\d{2}$").unwrap();
    assert!(re.is_match(subject.trim()), "{subject}");
    let mut files: Vec<String> = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    files.sort();
    assert_eq!(files, vec![S1_NOTE.to_string(), "free.md".to_string()]);
    let status = v.status();
    assert!(status.contains("s2-demo.md"), "{status}");
    assert!(status.contains("locked.md"), "{status}");
    until("the backup record", || svc.view().last_backup.is_some()).await;
    drop(guard);
    until("the push", || {
        sh(&v.remote, &["log", "-1", "--format=%s", "main"]).starts_with("vault backup: ")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_survives_restart_without_double_commits() {
    let v = Vault::new("restart");
    let db = v.base.join("state/overseer.db");
    let (first, second);
    {
        // The old daemon queued two entries and committed the first, then
        // went down before recording it.
        let ledger = Ledger::open(&db).unwrap();
        first = ledger
            .queue_push(now(), &Sender::Slot { slot: 1 }, &["a.md".into()], "S1: a")
            .unwrap();
        second = ledger
            .queue_push(now(), &Sender::Slot { slot: 1 }, &["b.md".into()], "S1: b")
            .unwrap();
        ledger.queue_begin(first).unwrap();
        write(&v.vault.join("a.md"), "a\n");
        write(&v.vault.join("b.md"), "b\n");
        sh(&v.vault, &["add", "--", "a.md"]);
        sh(
            &v.vault,
            &[
                "commit",
                "-m",
                &format!("S1: a\n\nOverseer-Queue: {first}"),
                "--",
                "a.md",
            ],
        );
    }
    let before = v.commits();
    let ledger = Arc::new(Ledger::open(&db).unwrap());
    let svc = v.service(ledger.clone(), v.options());
    until("both entries settled", || {
        svc.view()
            .entries
            .iter()
            .all(|e| e.state == QueueState::Committed && e.pushed == Some(true))
    })
    .await;
    assert_eq!(v.commits(), before + 1, "only the second is committed now");
    let log = sh(&v.vault, &["log", "--format=%B"]);
    assert_eq!(
        log.matches(&format!("Overseer-Queue: {first}\n")).count(),
        1,
        "{log}"
    );
    assert_eq!(
        log.matches(&format!("Overseer-Queue: {second}\n")).count(),
        1,
        "{log}"
    );
    assert!(v.status().is_empty(), "{}", v.status());
    let view = svc.view();
    let a = view.entries.iter().find(|e| e.id == first).unwrap();
    assert!(a.commit.is_some());
    // A second restart finds nothing left to do.
    drop(svc);
    let svc = v.service(ledger, v.options());
    sleep(DRAIN * 2).await;
    assert_eq!(v.commits(), before + 1);
    assert_eq!(svc.view().entries.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bd_runs_under_the_lock_then_syncs() {
    let v = Vault::new("bd");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let guard = svc.git().lock().await;
    let task = {
        let svc = svc.clone();
        tokio::spawn(async move {
            svc.run_bd(
                ["update", "ov-3", "--status", "in_progress"]
                    .map(String::from)
                    .to_vec(),
            )
            .await
        })
    };
    sleep(Duration::from_millis(300)).await;
    assert!(v.log("bd.log").is_empty(), "bd waits for the vault lock");
    drop(guard);
    let r = task.await.unwrap();
    match r {
        Response::BdOutput {
            code,
            stdout,
            stderr,
        } => {
            assert_eq!(code, 0, "{stderr}");
            assert_eq!(stdout, "bd said update\n");
        }
        other => panic!("{other:?}"),
    }
    let pwd = std::fs::canonicalize(&v.vault).unwrap();
    assert_eq!(
        v.log("bd.log")[0],
        format!("{}|1|update ov-3 --status in_progress", pwd.display())
    );
    until("bd sync", || v.log("bd.log").len() == 2).await;
    assert_eq!(v.log("bd.log")[1], format!("{}|1|sync", pwd.display()));
    sleep(DRAIN * 2).await;
    assert_eq!(v.log("bd.log").len(), 2, "one bd sync per drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn changes_are_announced() {
    let v = Vault::new("events");
    let (events, mut rx) = broadcast::channel(256);
    let svc = BrainService::with_options(
        &v.config(),
        Arc::new(Ledger::in_memory()),
        events,
        v.options(),
    )
    .unwrap();
    write(&v.vault.join("free.md"), "x\n");
    svc.commit(
        Sender::You,
        vec!["free.md".into()],
        "free".into(),
        &Owners::default(),
    )
    .await;
    let mut seen = false;
    while let Ok(ev) = rx.try_recv() {
        if let Event::BrainChanged(view) = ev {
            seen |= view
                .entries
                .iter()
                .any(|e| e.state == QueueState::Committed);
        }
    }
    assert!(seen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_index_lock_is_retried() {
    let v = Vault::new("ilock");
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    let lock = v.vault.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    write(&v.vault.join("free.md"), "x\n");
    let r = svc
        .commit(
            Sender::You,
            vec!["free.md".into()],
            "free".into(),
            &Owners::default(),
        )
        .await;
    assert_eq!(
        r,
        Response::Queued {
            message: "index.lock held, will retry".into()
        }
    );
    assert_eq!(svc.view().entries[0].state, QueueState::Queued);
    std::fs::remove_file(&lock).unwrap();
    until("the retried commit", || {
        svc.view().entries[0].state == QueueState::Committed
    })
    .await;
    assert_eq!(sh(&v.vault, &["log", "-1", "--format=%s"]).trim(), "free");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_conflict_keeps_the_commit_and_tells_jonathan() {
    let v = Vault::new("conflict");
    sh(&v.base, &["clone", v.remote.to_str().unwrap(), "other"]);
    let other = v.base.join("other");
    sh(&other, &["config", "user.name", "Other"]);
    sh(&other, &["config", "user.email", "other@example.com"]);
    write(&other.join("README.md"), "theirs\n");
    sh(&other, &["commit", "-am", "theirs"]);
    sh(&other, &["push"]);
    let svc = v.service(Arc::new(Ledger::in_memory()), v.options());
    write(&v.vault.join("README.md"), "ours\n");
    let r = svc
        .commit(
            Sender::You,
            vec!["README.md".into()],
            "ours".into(),
            &Owners::default(),
        )
        .await;
    assert!(text(&r).contains("committed"), "{r:?}");
    until("the push result", || svc.view().entries[0].pushed.is_some()).await;
    let view = svc.view();
    assert_eq!(view.entries[0].pushed, Some(false));
    assert!(
        view.push_problem
            .as_deref()
            .is_some_and(|p| p.starts_with("2ndBrain push needs you: ")),
        "{view:?}"
    );
    assert_eq!(sh(&v.vault, &["log", "-1", "--format=%s"]).trim(), "ours");
    assert_eq!(
        std::fs::read_to_string(v.vault.join("README.md")).unwrap(),
        "ours\n"
    );
}

/// A trigger on the file ledger that makes one kind of queue write fail,
/// as a full disk or a locked database would.
fn break_ledger(db: &Path, name: &str, when: &str) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(db).unwrap();
    c.execute_batch(&format!(
        "CREATE TRIGGER {name} BEFORE UPDATE OF {when} BEGIN SELECT RAISE(ABORT, 'injected'); END;"
    ))
    .unwrap();
    c
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_state_write_after_a_commit_is_not_committed_twice() {
    let v = Vault::new("ledgerfail");
    let db = v.base.join("state/overseer.db");
    let ledger = Arc::new(Ledger::open(&db).unwrap());
    let opts = BrainOptions {
        retry: Duration::from_millis(150),
        ..v.options()
    };
    let svc = v.service(ledger, opts);
    let c = break_ledger(
        &db,
        "no_commit",
        "state ON brain_queue WHEN NEW.state = 'Committed'",
    );
    write(&v.vault.join("free.md"), "x\n");
    let before = v.commits();
    let caller = {
        let svc = svc.clone();
        tokio::spawn(async move {
            svc.commit(
                Sender::You,
                vec!["free.md".into()],
                "free".into(),
                &Owners::default(),
            )
            .await
        })
    };
    until("the commit", || v.commits() == before + 1).await;
    // Several retries pass while the ledger cannot record it.
    sleep(Duration::from_millis(800)).await;
    assert_eq!(v.commits(), before + 1, "never committed twice");
    assert_eq!(svc.view().entries[0].state, QueueState::Queued);
    assert!(!caller.is_finished(), "the caller still waits");
    c.execute_batch("DROP TRIGGER no_commit;").unwrap();
    let r = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
    assert!(text(&r).contains("committed"), "{r:?}");
    until("the push", || {
        let e = &svc.view().entries[0];
        e.state == QueueState::Committed && e.pushed == Some(true)
    })
    .await;
    assert_eq!(v.commits(), before + 1);
    let body = sh(&v.vault, &["log", "-1", "--format=%B"]);
    assert!(body.contains("Overseer-Queue: 1"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_attempt_count_holds_the_commit_back() {
    let v = Vault::new("beginfail");
    let db = v.base.join("state/overseer.db");
    let ledger = Arc::new(Ledger::open(&db).unwrap());
    let opts = BrainOptions {
        retry: Duration::from_millis(150),
        ..v.options()
    };
    let svc = v.service(ledger, opts);
    let c = break_ledger(&db, "no_attempts", "attempts ON brain_queue");
    write(&v.vault.join("free.md"), "x\n");
    let before = v.commits();
    let caller = {
        let svc = svc.clone();
        tokio::spawn(async move {
            svc.commit(
                Sender::You,
                vec!["free.md".into()],
                "free".into(),
                &Owners::default(),
            )
            .await
        })
    };
    sleep(Duration::from_millis(800)).await;
    assert_eq!(v.commits(), before, "no attempt without a count");
    assert_eq!(svc.view().entries[0].state, QueueState::Queued);
    c.execute_batch("DROP TRIGGER no_attempts;").unwrap();
    let r = tokio::time::timeout(WAIT, caller).await.unwrap().unwrap();
    assert!(text(&r).contains("committed"), "{r:?}");
    assert_eq!(v.commits(), before + 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn index_lock_gives_up_after_four_attempts() {
    let v = Vault::new("ilockfail");
    let opts = BrainOptions {
        retry: Duration::from_millis(50),
        ..v.options()
    };
    let svc = v.service(Arc::new(Ledger::in_memory()), opts);
    std::fs::write(v.vault.join(".git/index.lock"), "").unwrap();
    write(&v.vault.join("free.md"), "x\n");
    svc.commit(
        Sender::You,
        vec!["free.md".into()],
        "free".into(),
        &Owners::default(),
    )
    .await;
    until("the last attempt", || {
        svc.view().entries[0].state == QueueState::Failed
    })
    .await;
    assert_eq!(
        svc.view().entries[0].error.as_deref(),
        Some("index.lock still held after 4 attempts")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_tick_waits_the_drain_before_syncing() {
    let v = Vault::new("backupsync");
    let drain = Duration::from_millis(1500);
    let opts = BrainOptions {
        drain,
        backup: Some(Duration::from_millis(300)),
        ..v.options()
    };
    let svc = v.service(Arc::new(Ledger::in_memory()), opts);
    write(&v.vault.join("free.md"), "x\n");
    until("the backup commit", || {
        sh(&v.vault, &["log", "-1", "--format=%s"]).starts_with("vault backup: ")
    })
    .await;
    let committed = Instant::now();
    sleep(Duration::from_millis(500)).await;
    assert!(
        v.log("sync.log").is_empty(),
        "no sync before the quiet period"
    );
    until("the sync", || v.log("sync.log").len() == 1).await;
    assert!(committed.elapsed() >= drain - Duration::from_millis(400));
    drop(svc);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backup_leaves_a_queued_entrys_paths_alone() {
    let v = Vault::new("backupqueued");
    let opts = BrainOptions {
        retry: Duration::from_millis(1500),
        ..v.options()
    };
    let svc = v.service(Arc::new(Ledger::in_memory()), opts);
    let lock = v.vault.join(".git/index.lock");
    std::fs::write(&lock, "").unwrap();
    let r = svc
        .append_journal(Sender::Slot { slot: 2 }, "Work Log", "- one")
        .await;
    assert_eq!(
        r,
        Response::Queued {
            message: "index.lock held, will retry".into()
        }
    );
    std::fs::remove_file(&lock).unwrap();
    write(&v.vault.join("free.md"), "x\n");
    svc.backup_now().await;
    assert!(sh(&v.vault, &["log", "-1", "--format=%s"]).starts_with("vault backup: "));
    let files = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), "free.md");
    until("the journal commit", || {
        sh(&v.vault, &["log", "-1", "--format=%s"]).trim() == "S2: journal, Work Log"
    })
    .await;
    let rel = fill_template(&v.config().journal_path, now(), 0, "");
    let files = sh(&v.vault, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(files.trim(), rel);
}

// The daemon over its socket, with the writer off and on.

async fn ask(socket: &Path, req: Request) -> Response {
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let (mut rd, mut wr) = stream.into_split();
    write_frame(&mut wr, &Frame::Request { seq: 1, req })
        .await
        .unwrap();
    loop {
        let f = tokio::time::timeout(WAIT, read_frame(&mut rd))
            .await
            .expect("reply in time")
            .unwrap()
            .expect("frame");
        if let Frame::Response { seq: 1, resp } = f {
            return resp;
        }
    }
}

async fn daemon(v: &Vault, settings: Config) -> (PathBuf, tokio::task::JoinHandle<()>) {
    let socket = v.base.join("d.sock");
    let cfg = overseer_daemon::DaemonConfig {
        socket: socket.clone(),
        default_command: vec!["/bin/sh".into()],
        env: overseer_daemon::EnvSource::Inherit,
        state_dir: v.base.join("state"),
        hook_program: PathBuf::from("/bin/true"),
        settings,
        patterns: overseer_daemon::locks::targets::default_patterns(),
    };
    let task = tokio::spawn(async move {
        let _ = overseer_daemon::run(cfg).await;
    });
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(&socket).await.is_ok() {
            return (socket, task);
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("daemon did not come up");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writer_off_answers_with_the_config_hint() {
    let v = Vault::new("off");
    let off = Config {
        brain_writer: false,
        ..v.config()
    };
    let (events, _) = broadcast::channel(16);
    assert!(BrainService::new(&off, Arc::new(Ledger::in_memory()), events).is_none());
    assert_eq!(
        WRITER_OFF,
        "the 2ndBrain writer is off (brain_writer in config.toml)"
    );
    let (socket, task) = daemon(&v, off).await;
    write(&v.vault.join("free.md"), "x\n");
    let before = v.commits();
    for req in [
        Request::Commit {
            from: None,
            paths: vec!["free.md".into()],
            message: "x".into(),
        },
        Request::AppendJournal {
            from: Some(1),
            section: "Work Log".into(),
            text: "x".into(),
        },
        Request::EditShared {
            from: Some(1),
            path: "x.md".into(),
            old_text: "a".into(),
            new_text: "b".into(),
        },
        Request::UpdateCurrentSession {
            slot: 1,
            text: "x".into(),
        },
        Request::RunBd {
            from: None,
            args: vec!["update".into()],
        },
    ] {
        assert_eq!(
            ask(&socket, req).await,
            Response::Error {
                message: WRITER_OFF.into()
            }
        );
    }
    match ask(&socket, Request::BrainStatus).await {
        Response::Brain(view) => assert!(!view.on),
        other => panic!("{other:?}"),
    }
    let check = WriteCheck {
        slot: 1,
        tool: "Write".into(),
        paths: vec![v
            .vault
            .join("06-Daily/2026/11-November/2026-11-13-journal.md")],
        cwd: None,
    };
    assert_eq!(
        ask(&socket, Request::CheckWrite(check)).await,
        Response::Verdict {
            deny: None,
            rewrite: None
        }
    );
    assert_eq!(v.commits(), before);
    assert!(v.log("bd.log").is_empty());
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_serves_the_writer_requests() {
    let v = Vault::new("daemon");
    let (socket, task) = daemon(&v, v.config()).await;
    write(&v.vault.join("free.md"), "x\n");
    let r = ask(
        &socket,
        Request::Commit {
            from: None,
            paths: vec![v.vault.join("free.md").to_string_lossy().into_owned()],
            message: "free note".into(),
        },
    )
    .await;
    assert!(text(&r).contains("committed"), "{r:?}");
    assert_eq!(
        sh(&v.vault, &["log", "-1", "--format=%s"]).trim(),
        "free note"
    );
    match ask(&socket, Request::BrainStatus).await {
        Response::Brain(view) => {
            assert!(view.on);
            assert_eq!(view.entries[0].from, Sender::You);
        }
        other => panic!("{other:?}"),
    }
    let check = WriteCheck {
        slot: 1,
        tool: "Write".into(),
        paths: vec![v
            .vault
            .join("06-Daily/2026/11-November/2026-11-13-journal.md")],
        cwd: None,
    };
    match ask(&socket, Request::CheckWrite(check)).await {
        Response::Verdict { deny: Some(d), .. } => assert!(d.contains("shared 2ndBrain file")),
        other => panic!("{other:?}"),
    }
    match ask(
        &socket,
        Request::RunBd {
            from: Some(1),
            args: vec!["close".into(), "ov-3".into()],
        },
    )
    .await
    {
        Response::BdOutput { code, .. } => assert_eq!(code, 0),
        other => panic!("{other:?}"),
    }
    let _ = ask(&socket, Request::Shutdown).await;
    let _ = tokio::time::timeout(WAIT, task).await;
}
