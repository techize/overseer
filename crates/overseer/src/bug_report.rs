//! `ctl bug-report`: gather what a bug needs into a Markdown file to read,
//! then, with `--post`, file that file as a GitHub issue.
//!
//! The report is never posted straight away: the repository is public, and
//! the board and log name work items, folders and people, so the user reads
//! (and edits) the file first.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context};
use overseer_proto::paths::{log_dir, socket_path};
use overseer_proto::{slot_name, Client, Request, Response, SlotId};

/// Daemon log lines kept, and stderr log lines.
const LOG_LINES: usize = 80;
const ERR_LINES: usize = 30;

/// What the report is made of; any part may be missing.
#[derive(Debug, Default)]
pub struct Parts {
    pub what: String,
    pub builds: Vec<String>,
    pub board: Option<String>,
    /// (slot, screen text or why it could not be read)
    pub screens: Vec<(SlotId, String)>,
    pub log: Option<String>,
    pub err_log: Option<String>,
}

/// The issue title: the first line of `what`, cut to 80 characters.
pub fn title_of(what: &str) -> String {
    let first = what
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("Bug report")
        .trim();
    if first.chars().count() <= 80 {
        first.to_string()
    } else {
        format!(
            "{}...",
            first.chars().take(77).collect::<String>().trim_end()
        )
    }
}

fn fenced(out: &mut String, heading: &str, text: &str) {
    out.push_str(&format!(
        "\n## {heading}\n\n```text\n{}\n```\n",
        text.trim_end()
    ));
}

/// The report: a `# title` line, then the sections.
pub fn compose(p: &Parts) -> String {
    let mut out = format!(
        "# {}\n\n## What happened\n\n{}\n",
        title_of(&p.what),
        p.what.trim()
    );
    fenced(&mut out, "Builds", &p.builds.join("\n"));
    match &p.board {
        Some(b) => fenced(&mut out, "Board", b),
        None => out.push_str("\n## Board\n\nThe daemon was not running.\n"),
    }
    for (slot, screen) in &p.screens {
        fenced(&mut out, &format!("Screen of {}", slot_name(*slot)), screen);
    }
    if let Some(log) = &p.log {
        fenced(
            &mut out,
            &format!("daemon.log (last {LOG_LINES} lines)"),
            log,
        );
    }
    if let Some(log) = p.err_log.as_deref().filter(|l| !l.trim().is_empty()) {
        fenced(
            &mut out,
            &format!("daemon.err.log (last {ERR_LINES} lines)"),
            log,
        );
    }
    out
}

/// `text` with the home folder shown as `~`.
pub fn scrub(text: &str, home: &str) -> String {
    if home.len() < 2 {
        return text.to_string();
    }
    text.replace(home, "~")
}

/// The last `n` lines of `text`.
pub fn tail(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A report file's title (its `# ` line) and the issue body after it.
pub fn split_report(text: &str) -> anyhow::Result<(String, String)> {
    let text = text.trim_start();
    let Some(first) = text.lines().next().and_then(|l| l.strip_prefix("# ")) else {
        bail!("a report starts with a \"# title\" line");
    };
    let title = first.trim().to_string();
    if title.is_empty() {
        bail!("the report's title line is empty");
    }
    let body = text[text.find('\n').unwrap_or(text.len())..]
        .trim()
        .to_string();
    Ok((title, body))
}

/// The file a report written now goes to.
fn report_path(dir: &Path) -> PathBuf {
    // SAFETY: localtime_r only writes the struct it is given.
    let stamp = unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    };
    dir.join(format!("bug-report-{stamp}.md"))
}

/// Gather the parts from a running daemon, if any, and the log files.
async fn gather(what: String, slots: &[SlotId]) -> Parts {
    let mut p = Parts {
        what,
        ..Parts::default()
    };
    let (version, build) = (env!("CARGO_PKG_VERSION"), overseer_proto::current_build());
    match Client::connect(socket_path()).await {
        Ok(client) => {
            p.builds = match client.request(Request::DaemonInfo).await {
                Ok(Response::DaemonInfo(info)) => {
                    crate::ctl::format_version(version, build, Some(&info))
                }
                _ => crate::ctl::format_old_daemon(version, build),
            };
            if let Ok(Response::Board(v)) = client.request(Request::GetBoard).await {
                let board = crate::ctl::format_board(&v, crate::ctl::now_secs(), "the user");
                p.board = Some(board);
            }
            for &slot in slots {
                let screen = match client.request(Request::SlotScreen { id: slot }).await {
                    Ok(Response::Text { text }) => text,
                    Ok(Response::Error { message }) => format!("(not read: {message})"),
                    Ok(_) | Err(_) => "(not read: this daemon cannot show screens)".to_string(),
                };
                p.screens.push((slot, screen));
            }
        }
        Err(_) => p.builds = crate::ctl::format_version(version, build, None),
    }
    let logs = log_dir();
    let read = |name: &str, n| {
        std::fs::read_to_string(logs.join(name))
            .ok()
            .map(|t| tail(&t, n))
    };
    p.log = read("daemon.log", LOG_LINES);
    p.err_log = read("daemon.err.log", ERR_LINES);
    p
}

/// Write the report and say where it is and how to post it.
pub async fn write(what: String, slots: &[SlotId]) -> anyhow::Result<()> {
    if what.trim().is_empty() {
        bail!("say what happened: overseer ctl bug-report \"what happened\"");
    }
    let parts = gather(what, slots).await;
    let home = std::env::var("HOME").unwrap_or_default();
    let text = scrub(&compose(&parts), &home);
    let dir = log_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = report_path(&dir);
    std::fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
    let shown = scrub(&path.display().to_string(), &home);
    println!("Wrote {shown}");
    println!(
        "Read it before posting: the issue is public, and the board and log name your work \
         items and folders. Edit or cut anything private, then:"
    );
    println!("  overseer ctl bug-report --post {shown}");
    Ok(())
}

/// File a report written earlier as an issue on `repo`, labelled bug.
pub fn post(path: &Path, repo: &str) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (title, body) = split_report(&text)?;
    let gh = std::env::var("OVERSEER_GH").unwrap_or_else(|_| "gh".to_string());
    let mut child = Command::new(&gh)
        .args([
            "issue", "create", "--repo", repo, "--label", "bug", "--title", &title,
        ])
        .args(["--body-file", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .with_context(|| format!("run {gh} (the GitHub CLI); is it installed and logged in?"))?;
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(body.as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("{gh} issue create failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_title_is_the_first_line_cut_short() {
        assert_eq!(
            title_of("\n  Relay stuck on S2  \nmore"),
            "Relay stuck on S2"
        );
        let long = "x".repeat(100);
        assert_eq!(title_of(&long).chars().count(), 80);
        assert!(title_of(&long).ends_with("..."));
        assert_eq!(title_of(""), "Bug report");
    }

    #[test]
    fn the_report_has_every_part_and_skips_an_empty_err_log() {
        let p = Parts {
            what: "Relay stuck on S2\nIt stayed pending.".into(),
            builds: vec![
                "app 0.1.0 build abc".into(),
                "daemon 0.1.0 build abc".into(),
            ],
            board: Some("S2 idle  DEMO-4".into()),
            screens: vec![(2, "> hello\n-- input box: typed".into())],
            log: Some("line 1\nline 2".into()),
            err_log: Some("  \n".into()),
        };
        let r = compose(&p);
        assert!(r.starts_with(
            "# Relay stuck on S2\n\n## What happened\n\nRelay stuck on S2\nIt stayed pending.\n"
        ));
        assert!(
            r.contains("## Builds\n\n```text\napp 0.1.0 build abc\ndaemon 0.1.0 build abc\n```")
        );
        assert!(r.contains("## Board\n\n```text\nS2 idle  DEMO-4\n```"));
        assert!(r.contains("## Screen of S2\n\n```text\n> hello\n-- input box: typed\n```"));
        assert!(r.contains("## daemon.log (last 80 lines)"));
        assert!(!r.contains("daemon.err.log"));
        let none = compose(&Parts {
            what: "x".into(),
            ..Parts::default()
        });
        assert!(none.contains("The daemon was not running."));
    }

    #[test]
    fn the_home_folder_is_hidden_and_tails_keep_the_end() {
        assert_eq!(
            scrub("/Users/demo/x and /Users/demo", "/Users/demo"),
            "~/x and ~"
        );
        assert_eq!(scrub("/a", ""), "/a");
        assert_eq!(tail("1\n2\n3\n4", 2), "3\n4");
        assert_eq!(tail("1", 5), "1");
    }

    #[test]
    fn a_report_splits_into_title_and_body() {
        let (t, b) = split_report("# Relay stuck\n\n## What happened\n\nok\n").unwrap();
        assert_eq!(t, "Relay stuck");
        assert_eq!(b, "## What happened\n\nok");
        assert!(split_report("no title\n").is_err());
        assert!(split_report("#  \nbody").is_err());
    }
}
