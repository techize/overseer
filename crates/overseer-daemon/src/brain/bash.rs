//! Pure parsing of Bash command lines for the 2ndBrain write checks
//! (Rulings 10 to 12). Best effort over shell syntax: quotes, `;`, `&&`,
//! `||`, `|`, redirects and the writers the rulings name. Nothing here
//! looks at the vault or runs a command; it only finds what a command line
//! would write, so a missed exotic form is allowed rather than guessed.

use std::path::{Path, PathBuf};

use super::paths::locate;
use crate::locks::targets::{write_segments, Segment};

/// What a command line does with `bd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BdCall {
    /// No `bd` in the line.
    None,
    /// Only `bd` reads.
    Read,
    /// A `bd` write that is the whole line, with its arguments.
    Write(Vec<String>),
    /// A `bd` write joined with other commands.
    Compound,
}

/// `bd` subcommands that write (Ruling 10); `comments` writes only as
/// `comments add`.
const BD_WRITES: [&str; 11] = [
    "create", "update", "close", "reopen", "dep", "label", "comment", "delete", "sync", "edit",
    "assign",
];

/// `bd` global flags that take a value.
const BD_VALUE_FLAGS: [&str; 2] = ["--db", "--actor"];

fn base(word: &str) -> &str {
    Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
}

fn is_option(w: &str) -> bool {
    w.len() > 1 && w.starts_with('-')
}

/// `$HOME` and `${HOME}` prefixes become `~`; other variables stay as written.
fn expand_home(w: &str) -> String {
    for var in ["$HOME", "${HOME}"] {
        if let Some(rest) = w.strip_prefix(var) {
            if rest.is_empty() || rest.starts_with('/') {
                return format!("~{rest}");
            }
        }
    }
    w.to_string()
}

/// Positional arguments of `words` (after the command word): options are
/// skipped, `value_opts` take the next word, `--` ends the options.
fn positionals<'a>(words: &'a [String], value_opts: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut options = true;
    while i < words.len() {
        let w = words[i].as_str();
        if options && w == "--" {
            options = false;
        } else if options && is_option(w) {
            if value_opts.contains(&w) {
                i += 1;
            }
        } else {
            out.push(w);
        }
        i += 1;
    }
    out
}

/// A short-option cluster's letters up to the first non-letter, e.g. `ni`
/// for `-ni` and `i` for `-i.bak`. `None` for a long option or a word that
/// is not an option.
fn cluster(w: &str) -> Option<&str> {
    let rest = w.strip_prefix('-')?;
    if rest.starts_with('-') || rest.is_empty() {
        return None;
    }
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// File arguments of `sed -i` or `perl -i`; empty without `-i`.
fn in_place_files(words: &[String]) -> Vec<String> {
    let mut in_place = false;
    let mut script_given = false;
    let mut files: Vec<&str> = Vec::new();
    let mut i = 1;
    let mut options = true;
    while i < words.len() {
        let w = words[i].as_str();
        if options && w == "--" {
            options = false;
        } else if options && (w.starts_with("--in-place") || w == "--in-place") {
            in_place = true;
        } else if options && (w.starts_with("--expression=") || w.starts_with("--file=")) {
            script_given = true;
        } else if options && matches!(w, "--expression" | "--file") {
            script_given = true;
            i += 1;
        } else if options && is_option(w) {
            if let Some(c) = cluster(w) {
                if c.contains('i') {
                    in_place = true;
                }
                // `-e script` and `-f file` (and perl's `-pie script`).
                if c.ends_with('e') || c.ends_with('f') {
                    script_given = true;
                    if w.len() == c.len() + 1 {
                        i += 1;
                    }
                }
            }
        } else if w.is_empty() {
            // BSD sed's empty backup suffix.
        } else if !script_given {
            script_given = true;
        } else {
            files.push(w);
        }
        i += 1;
    }
    if in_place {
        files.into_iter().map(String::from).collect()
    } else {
        Vec::new()
    }
}

fn join(prefix: &Path, p: &str) -> String {
    if p.starts_with('/') || p.starts_with('~') || prefix.as_os_str().is_empty() {
        p.to_string()
    } else {
        prefix.join(p).to_string_lossy().into_owned()
    }
}

fn segment_writes(seg: &Segment) -> Vec<String> {
    let mut out: Vec<String> = seg.redirects.clone();
    if let Some(cmd) = seg.words.first() {
        let args = &seg.words[1..];
        match base(cmd) {
            "tee" => out.extend(positionals(args, &[]).into_iter().map(String::from)),
            "sed" => out.extend(in_place_files(&seg.words)),
            "perl" => out.extend(in_place_files(&seg.words)),
            "cp" | "mv" => {
                let target = args
                    .iter()
                    .position(|w| matches!(w.as_str(), "-t" | "--target-directory"))
                    .and_then(|i| args.get(i + 1))
                    .map(String::as_str)
                    .or_else(|| {
                        positionals(args, &["-t", "--target-directory", "-S", "--suffix"])
                            .last()
                            .copied()
                    });
                out.extend(target.map(String::from));
            }
            "rm" => out.extend(positionals(args, &[]).into_iter().map(String::from)),
            "touch" => out.extend(
                positionals(args, &["-t", "-d", "-r", "--date", "--reference"])
                    .into_iter()
                    .map(String::from),
            ),
            "truncate" => out.extend(
                positionals(args, &["-s", "-r", "--size", "--reference"])
                    .into_iter()
                    .map(String::from),
            ),
            _ => {}
        }
    }
    out
}

/// The paths a command line writes (Ruling 11): `>` and `>>` targets,
/// `tee [-a]` files, `sed -i`/`perl -i` files, the last argument of `cp`
/// and `mv`, every argument of `rm`, `touch` and `truncate`. Paths are as
/// written, with `$HOME` as `~`, joined to the folder of an earlier `cd`;
/// a relative one is left for the caller to resolve against the hook's
/// cwd. Words with other variables or substitutions are dropped.
pub fn write_paths(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in write_segments(command, Path::new("")) {
        for w in segment_writes(&seg) {
            let w = expand_home(&w);
            if w.is_empty() || w.contains('$') || w.contains('`') || w.starts_with("/dev/") {
                continue;
            }
            let p = join(&seg.cwd, &w);
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// Subcommands of `git` that only read (Ruling 12).
const GIT_READS: [&str; 8] = [
    "status",
    "log",
    "diff",
    "show",
    "blame",
    "rev-parse",
    "ls-files",
    "describe",
];

/// `git` global options that take a value.
const GIT_VALUE_OPTS: [&str; 6] = [
    "-c",
    "--namespace",
    "--exec-path",
    "--super-prefix",
    "--attr-source",
    "--config-env",
];

fn inside(brain: &Path, home: &Path, dir: &Path) -> bool {
    locate(brain, None, dir, home).is_ok()
}

/// The denied `git` subcommand when a command in `command` runs git in the
/// vault with anything but a read (Ruling 12). The repo is the cwd, moved
/// by `cd <dir> &&` and `-C <dir>`, or named by `--git-dir`/`--work-tree`.
pub fn vault_git(command: &str, cwd: &Path, brain: &Path) -> Option<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from)?;
    vault_git_in(command, cwd, brain, &home)
}

pub(crate) fn vault_git_in(command: &str, cwd: &Path, brain: &Path, home: &Path) -> Option<String> {
    for seg in write_segments(command, cwd) {
        if seg.words.first().map(|w| base(w)) != Some("git") {
            continue;
        }
        let words = &seg.words;
        let mut dir = seg.cwd.clone();
        let mut git_dir: Option<PathBuf> = None;
        let mut work_tree: Option<PathBuf> = None;
        let abs = |from: &Path, p: &str| -> PathBuf {
            let p = expand_home(p);
            let p = match p.strip_prefix('~') {
                Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                    home.join(rest.trim_start_matches('/'))
                }
                _ => PathBuf::from(p),
            };
            from.join(p)
        };
        let mut i = 1;
        let mut sub: Option<usize> = None;
        while i < words.len() {
            let w = words[i].as_str();
            if w == "-C" {
                if let Some(d) = words.get(i + 1) {
                    dir = abs(&dir, d);
                }
                i += 2;
                continue;
            }
            if let Some(v) = w.strip_prefix("--git-dir=") {
                git_dir = Some(abs(&dir, v));
            } else if w == "--git-dir" {
                git_dir = words.get(i + 1).map(|v| abs(&dir, v));
                i += 1;
            } else if let Some(v) = w.strip_prefix("--work-tree=") {
                work_tree = Some(abs(&dir, v));
            } else if w == "--work-tree" {
                work_tree = words.get(i + 1).map(|v| abs(&dir, v));
                i += 1;
            } else if GIT_VALUE_OPTS.contains(&w) {
                i += 1;
            } else if !is_option(w) {
                sub = Some(i);
                break;
            }
            i += 1;
        }
        let Some(at) = sub else { continue };
        let name = words[at].as_str();
        let args = &words[at + 1..];
        let mut repos: Vec<PathBuf> = vec![dir];
        if let Some(g) = &git_dir {
            repos.push(if g.file_name().is_some_and(|n| n == ".git") {
                g.parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| g.clone())
            } else {
                g.clone()
            });
        }
        repos.extend(work_tree);
        // `--git-dir` outside the vault with a work tree in it still writes
        // the vault's files, so any named place counts.
        let in_vault = if git_dir.is_some() || repos.len() > 1 {
            repos[1..].iter().any(|r| inside(brain, home, r))
        } else {
            inside(brain, home, &repos[0])
        };
        if !in_vault {
            continue;
        }
        let read = GIT_READS.contains(&name)
            || (name == "branch"
                && args.iter().all(|a| {
                    matches!(
                        a.as_str(),
                        "-a" | "-r"
                            | "-v"
                            | "-vv"
                            | "--list"
                            | "--show-current"
                            | "--all"
                            | "--remotes"
                    )
                }))
            || (name == "remote"
                && args
                    .iter()
                    .all(|a| matches!(a.as_str(), "-v" | "--verbose")));
        if !read {
            return Some(name.to_string());
        }
    }
    None
}

/// What the line does with `bd` (Ruling 10).
pub fn bd_call(command: &str) -> BdCall {
    let segs = write_segments(command, Path::new(""));
    let mut found = false;
    let mut write: Option<Vec<String>> = None;
    for seg in &segs {
        if seg.words.first().map(|w| base(w)) != Some("bd") {
            continue;
        }
        found = true;
        let args = &seg.words[1..];
        let mut i = 0;
        while i < args.len() && is_option(&args[i]) {
            i += BD_VALUE_FLAGS.contains(&args[i].as_str()) as usize + 1;
        }
        let Some(sub) = args.get(i) else { continue };
        let is_write = BD_WRITES.contains(&sub.as_str())
            || (sub == "comments" && args.get(i + 1).is_some_and(|a| a == "add"));
        if is_write && write.is_none() {
            write = Some(args.to_vec());
        }
    }
    match write {
        Some(args) if segs.len() == 1 => BdCall::Write(args),
        Some(_) => BdCall::Compound,
        None if found => BdCall::Read,
        None => BdCall::None,
    }
}

/// Single-quotes `w` for the shell unless it is plain.
fn quote(w: &str) -> String {
    if !w.is_empty()
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+".contains(&b))
    {
        w.to_string()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

/// The command that runs a `bd` write through the daemon: the original
/// line with its `bd` word replaced when it starts the line (so quoting,
/// substitutions and redirects behave as written), else the parsed
/// arguments shell-quoted.
pub fn bd_rewrite(command: &str, args: &[String]) -> String {
    let t = command.trim_start();
    if let Some(rest) = t.strip_prefix("bd") {
        if rest.starts_with([' ', '\t']) {
            return format!("overseer ctl bd --{rest}");
        }
    }
    let quoted: Vec<String> = args.iter().map(|a| quote(a)).collect();
    format!("overseer ctl bd -- {}", quoted.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(c: &str) -> Vec<String> {
        write_paths(c)
    }

    #[test]
    fn redirects_and_writers_are_found() {
        assert_eq!(
            w("echo x >> 06-Daily/2026/11-November/j.md"),
            ["06-Daily/2026/11-November/j.md"]
        );
        assert_eq!(w("echo x > a.md 2> err.log"), ["a.md", "err.log"]);
        assert_eq!(w("cmd &> all.log"), ["all.log"]);
        assert_eq!(w("echo hi | tee -a a.md b.md"), ["a.md", "b.md"]);
        assert_eq!(w("tee -a a.md b.md"), ["a.md", "b.md"]);
        assert_eq!(w("sed -i '' s/a/b/ f.md"), ["f.md"]);
        assert_eq!(w("sed -i.bak -e s/a/b/ f.md g.md"), ["f.md", "g.md"]);
        assert_eq!(w("sed -n s/a/b/p f.md"), Vec::<String>::new());
        assert_eq!(w("perl -pi -e 's/a/b/' f.md"), ["f.md"]);
        assert_eq!(w("cp x.md Sessions/y.md"), ["Sessions/y.md"]);
        assert_eq!(w("mv -f x.md y.md"), ["y.md"]);
        assert_eq!(w("cp -t Sessions x.md"), ["Sessions"]);
        assert_eq!(w("rm -f a.md"), ["a.md"]);
        assert_eq!(w("rm -rf dir other.md"), ["dir", "other.md"]);
        assert_eq!(w("touch a.md"), ["a.md"]);
        assert_eq!(w("truncate -s 0 a.md"), ["a.md"]);
        assert_eq!(
            w("echo x > a.md && rm b.md; touch c.md"),
            ["a.md", "b.md", "c.md"]
        );
    }

    #[test]
    fn home_prefixes_resolve_and_other_variables_are_left() {
        assert_eq!(w("echo x > ~/n.md"), ["~/n.md"]);
        assert_eq!(w("echo x > $HOME/n.md"), ["~/n.md"]);
        assert_eq!(w("echo x > ${HOME}/n.md"), ["~/n.md"]);
        assert!(w("echo x > $DIR/n.md").is_empty());
        assert!(w("echo x > /dev/null 2>&1").is_empty());
        assert_eq!(w("cd Sprints && echo x > a.md"), ["Sprints/a.md"]);
        assert_eq!(w("cd /v && rm a.md"), ["/v/a.md"]);
    }

    #[test]
    fn quoted_text_is_not_a_write() {
        assert!(w("echo \"a > b\"").is_empty());
        assert!(w("git commit -m \"x >> y\"").is_empty());
        assert!(w("echo 'rm a.md'").is_empty());
        assert!(w("cat <<EOF\necho > not.md\nEOF").is_empty());
        assert!(w("ls -l a.md | grep x").is_empty());
        assert!(w("cat a.md 2>&1").is_empty());
        assert!(w("sort < in.md").is_empty());
    }

    #[test]
    fn vault_git_rules() {
        let home = Path::new("/h");
        let brain = Path::new("/h/Repos/2ndBrain");
        let other = Path::new("/h/Repos/other");
        let v = |c: &str, cwd: &Path| vault_git_in(c, cwd, brain, home);
        assert_eq!(
            v("git -C ~/Repos/2ndBrain push", other).as_deref(),
            Some("push")
        );
        assert_eq!(
            v("git -C /h/Repos/2ndBrain commit -m x", other).as_deref(),
            Some("commit")
        );
        assert_eq!(v("git add -A", brain).as_deref(), Some("add"));
        assert_eq!(v("git log", brain), None);
        assert_eq!(v("git status", brain), None);
        assert_eq!(v("git -C ~/Repos/2ndBrain diff --stat", other), None);
        assert_eq!(v("git branch", brain), None);
        assert_eq!(v("git branch -d x", brain).as_deref(), Some("branch"));
        assert_eq!(v("git remote -v", brain), None);
        assert_eq!(v("git remote add x y", brain).as_deref(), Some("remote"));
        // A repo outside the vault is not ours.
        assert_eq!(v("git push", other), None);
        assert_eq!(v("git -C /h/Repos/other commit -m x", brain), None);
        // `cd` and `--git-dir` move the repo.
        assert_eq!(
            v("cd /h/Repos/2ndBrain && git commit -m x", other).as_deref(),
            Some("commit")
        );
        assert_eq!(
            v("git --git-dir=/h/Repos/2ndBrain/.git push", other).as_deref(),
            Some("push")
        );
        assert_eq!(v("git -C Sprints pull", brain).as_deref(), Some("pull"));
        assert_eq!(v("ls && echo git push", brain), None);
    }

    #[test]
    fn bd_rules() {
        assert_eq!(bd_call("bd ready --json"), BdCall::Read);
        assert_eq!(bd_call("bd ready --json | jq ."), BdCall::Read);
        assert_eq!(bd_call("bd comments ov-3"), BdCall::Read);
        assert_eq!(bd_call("ls"), BdCall::None);
        assert_eq!(
            bd_call("bd update ov-3 --status done"),
            BdCall::Write(vec![
                "update".into(),
                "ov-3".into(),
                "--status".into(),
                "done".into()
            ])
        );
        assert_eq!(
            bd_call("bd comments add ov-3 hello"),
            BdCall::Write(vec![
                "comments".into(),
                "add".into(),
                "ov-3".into(),
                "hello".into()
            ])
        );
        assert_eq!(
            bd_call("bd --db x.db create 'a b'"),
            BdCall::Write(vec![
                "--db".into(),
                "x.db".into(),
                "create".into(),
                "a b".into()
            ])
        );
        assert_eq!(bd_call("bd close ov-3 && git push"), BdCall::Compound);
        assert_eq!(bd_call("cd x; bd sync"), BdCall::Compound);
        assert_eq!(bd_call("echo bd update"), BdCall::None);
    }

    #[test]
    fn bd_rewrite_keeps_the_line_as_written() {
        assert_eq!(
            bd_rewrite("bd create \"a $(echo b)\" -p 1", &[]),
            "overseer ctl bd -- create \"a $(echo b)\" -p 1"
        );
        assert_eq!(
            bd_rewrite("FOO=1 bd close ov-3", &["close".into(), "ov 3".into()]),
            "overseer ctl bd -- close 'ov 3'"
        );
    }
}
