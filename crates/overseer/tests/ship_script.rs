//! scripts/ship.sh with a fake `gh` that answers from files and logs every
//! call. Nothing here reaches GitHub, and `--install` is never run.

use std::path::Path;
use std::process::{Command, Output};

struct Fake {
    dir: tempfile::TempDir,
}

impl Fake {
    fn new() -> Fake {
        use std::os::unix::fs::PermissionsExt;
        let f = Fake {
            dir: tempfile::tempdir().unwrap(),
        };
        // `pr view N` prints the first line of N.lines, dropping it while
        // more remain; `repo view` names a demo repository.
        let gh = r#"#!/bin/sh
echo "$*" >> "$FAKE/log"
case "$1 $2" in
"repo view") echo demo/overseer ;;
"pr view")
    f="$FAKE/$3.lines"
    if [ "$(wc -l < "$f")" -gt 1 ]; then
        head -n 1 "$f"; tail -n +2 "$f" > "$f.t"; mv "$f.t" "$f"
    else
        cat "$f"
    fi ;;
esac
"#;
        let path = f.dir.path().join("gh");
        std::fs::write(&path, gh).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        f
    }

    fn pr(&self, n: u32, lines: &[&str]) {
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        std::fs::write(self.dir.path().join(format!("{n}.lines")), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/ship.sh");
        Command::new("/bin/sh")
            .arg(script)
            .args(args)
            .env("FAKE", self.dir.path())
            .env("OVERSEER_GH", self.dir.path().join("gh"))
            .env("OVERSEER_SHIP_WAIT", "0")
            .output()
            .unwrap()
    }

    /// The merge and delete calls, in order.
    fn writes(&self) -> Vec<String> {
        std::fs::read_to_string(self.dir.path().join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("pr merge") || l.starts_with("api"))
            .map(str::to_string)
            .collect()
    }
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn green_prs_merge_in_order_at_their_head_and_lose_their_branches() {
    let f = Fake::new();
    f.pr(5, &["OPEN fix-a aaa111 MERGEABLE "]);
    f.pr(6, &["OPEN fix-b bbb222 MERGEABLE "]);
    let out = f.run(&["5", "6"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        f.writes(),
        [
            "pr merge 5 --repo demo/overseer --merge --match-head-commit aaa111",
            "api -X DELETE repos/demo/overseer/git/refs/heads/fix-a",
            "pr merge 6 --repo demo/overseer --merge --match-head-commit bbb222",
            "api -X DELETE repos/demo/overseer/git/refs/heads/fix-b",
        ]
    );
}

#[test]
fn a_conflict_stops_the_run_and_names_what_is_left() {
    let f = Fake::new();
    f.pr(5, &["OPEN fix-a aaa111 MERGEABLE "]);
    f.pr(6, &["OPEN fix-b bbb222 CONFLICTING "]);
    f.pr(7, &["OPEN fix-c ccc333 MERGEABLE "]);
    let out = f.run(&["--repo", "demo/other", "5", "6", "7"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("#6 conflicts with main: merge main into fix-b"),
        "{err}"
    );
    assert!(err.contains("Left to ship: 7."), "{err}");
    assert_eq!(f.writes().len(), 2, "only #5 merges: {:?}", f.writes());
    assert!(f.writes()[0].contains("--repo demo/other"));
}

#[test]
fn running_or_failing_checks_stop_before_merging() {
    let f = Fake::new();
    f.pr(5, &["OPEN fix-a aaa111 MERGEABLE PENDING"]);
    let out = f.run(&["5"]);
    assert!(
        stderr(&out).contains("#5 still has checks running"),
        "{}",
        stderr(&out)
    );
    f.pr(5, &["OPEN fix-a aaa111 MERGEABLE FAILURE"]);
    let out = f.run(&["5"]);
    assert!(
        stderr(&out).contains("did not pass (FAILURE)"),
        "{}",
        stderr(&out)
    );
    assert!(f.writes().is_empty());
}

#[test]
fn an_unknown_mergeable_is_asked_again() {
    let f = Fake::new();
    f.pr(
        5,
        &[
            "OPEN fix-a aaa111 UNKNOWN ",
            "OPEN fix-a aaa111 UNKNOWN ",
            "OPEN fix-a aaa111 MERGEABLE ",
        ],
    );
    let out = f.run(&["5"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(f.writes().len(), 2);
}

#[test]
fn merged_prs_are_skipped_and_closed_ones_stop() {
    let f = Fake::new();
    f.pr(5, &["MERGED fix-a aaa111 UNKNOWN "]);
    f.pr(6, &["CLOSED fix-b bbb222 UNKNOWN "]);
    let out = f.run(&["5", "6"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("#5 is already merged"));
    assert!(stderr(&out).contains("#6 is closed."), "{}", stderr(&out));
    assert!(f.writes().is_empty());
}

#[test]
fn bad_arguments_are_refused() {
    let f = Fake::new();
    assert_eq!(f.run(&[]).status.code(), Some(2));
    assert_eq!(f.run(&["--bogus", "5"]).status.code(), Some(2));
    assert!(stderr(&f.run(&["#5"])).contains("not a PR number"));
}
