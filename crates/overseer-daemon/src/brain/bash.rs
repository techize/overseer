//! Pure parsing of Bash command lines for the 2ndBrain write checks
//! (Rulings 10 to 12). Best effort over shell syntax: quotes, `;`, `&&`,
//! `||`, `|`, redirects and the writers the rulings name. Nothing here
//! looks at the vault or runs a command; it only finds what a command line
//! would write, so a missed exotic form is allowed rather than guessed.

use std::path::{Path, PathBuf};

use super::paths::locate;
use crate::locks::targets::{literal_heredoc_bodies, write_segments, Segment};

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

/// File arguments of `sed -i` or `perl -i`; empty without `-i`. Only each
/// tool's own forms count: perl's `-M`, `-m`, `-I` and `-x` take the rest
/// of the word as a value, `-i` takes it as the backup suffix.
fn in_place_files(words: &[String], perl: bool) -> Vec<String> {
    let mut in_place = false;
    let mut script_given = false;
    let mut files: Vec<&str> = Vec::new();
    let mut i = 1;
    let mut options = true;
    while i < words.len() {
        let w = words[i].as_str();
        if options && w == "--" {
            options = false;
        } else if options && w.starts_with("--") {
            if !perl && w.starts_with("--in-place") {
                in_place = true;
            } else if !perl && (w.starts_with("--expression=") || w.starts_with("--file=")) {
                script_given = true;
            } else if !perl && matches!(w, "--expression" | "--file") {
                script_given = true;
                i += 1;
            }
        } else if options && is_option(w) {
            let rest = &w[1..];
            for (k, c) in rest.char_indices() {
                let last = k + c.len_utf8() == rest.len();
                if perl && matches!(c, 'M' | 'm' | 'I' | 'x') {
                    break;
                }
                match c {
                    'i' => {
                        in_place = true;
                        break;
                    }
                    'e' | 'E' if perl => {
                        script_given = true;
                        i += last as usize;
                        break;
                    }
                    'e' | 'f' if !perl => {
                        script_given = true;
                        i += last as usize;
                        break;
                    }
                    'l' if !perl => {
                        i += last as usize;
                        break;
                    }
                    c if c.is_ascii_alphabetic() => {}
                    _ => break,
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

/// One write a command line makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Content written to, or created at, a file.
    File(String),
    /// A file removed or moved away.
    Delete(String),
    /// A recursive delete (`rm -r`, `rmdir`): everything under it goes.
    DeleteTree(String),
    /// `cp` or `mv` of `sources` to `target`, which is the destination file
    /// or an existing folder the sources go into.
    Copy {
        target: String,
        sources: Vec<String>,
    },
}

fn recursive(args: &[String]) -> bool {
    args.iter().take_while(|w| w.as_str() != "--").any(|w| {
        w == "--recursive" || (is_option(w) && !w.starts_with("--") && w.contains(['r', 'R']))
    })
}

fn copy_args(args: &[String]) -> Option<(String, Vec<String>)> {
    let mut target: Option<String> = None;
    let mut pos: Vec<String> = Vec::new();
    let mut options = true;
    let mut i = 0;
    while i < args.len() {
        let w = args[i].as_str();
        if options && w == "--" {
            options = false;
        } else if options && matches!(w, "-t" | "--target-directory") {
            target = args.get(i + 1).cloned();
            i += 1;
        } else if let Some(v) = w.strip_prefix("--target-directory=").filter(|_| options) {
            target = Some(v.to_string());
        } else if options && matches!(w, "-S" | "--suffix") {
            i += 1;
        } else if !(options && is_option(w)) {
            pos.push(w.to_string());
        }
        i += 1;
    }
    let target = match target {
        Some(t) => t,
        None => pos.pop()?,
    };
    Some((target, pos))
}

fn segment_writes(seg: &Segment) -> Vec<Write> {
    let mut out: Vec<Write> = seg.redirects.iter().cloned().map(Write::File).collect();
    let Some(cmd) = seg.words.first() else {
        return out;
    };
    let args = &seg.words[1..];
    fn files(v: Vec<&str>) -> Vec<Write> {
        v.into_iter().map(|p| Write::File(p.to_string())).collect()
    }
    match base(cmd) {
        "tee" => out.extend(files(positionals(args, &[]))),
        "sed" => out.extend(
            in_place_files(&seg.words, false)
                .into_iter()
                .map(Write::File),
        ),
        "perl" => out.extend(
            in_place_files(&seg.words, true)
                .into_iter()
                .map(Write::File),
        ),
        "cp" | "mv" => {
            if let Some((target, sources)) = copy_args(args) {
                if base(cmd) == "mv" {
                    out.extend(sources.iter().cloned().map(Write::Delete));
                }
                out.push(Write::Copy { target, sources });
            }
        }
        "rm" => {
            let tree = recursive(args);
            out.extend(positionals(args, &[]).into_iter().map(|p| {
                if tree {
                    Write::DeleteTree(p.to_string())
                } else {
                    Write::Delete(p.to_string())
                }
            }));
        }
        "rmdir" => out.extend(
            positionals(args, &[])
                .into_iter()
                .map(|p| Write::DeleteTree(p.to_string())),
        ),
        "touch" => out.extend(files(positionals(
            args,
            &["-t", "-d", "-r", "--date", "--reference"],
        ))),
        "truncate" => out.extend(files(positionals(
            args,
            &["-s", "-r", "--size", "--reference"],
        ))),
        _ => {}
    }
    out
}

/// A written path as the caller should resolve it: `$HOME` as `~`, joined
/// to the folder of an earlier `cd`. `None` for a path that cannot be
/// judged (other variables, substitutions, `/dev/*`, empty).
fn fix(p: &str, cwd: &Path) -> Option<String> {
    let p = expand_home(p);
    if p.is_empty() || p.contains('$') || p.contains('`') {
        return None;
    }
    let p = join(cwd, &p);
    (!is_dev(&p)).then_some(p)
}

/// Whether `p` is a device file: absolute and, with `.` and `..` folded
/// and symlinks resolved where it exists, under `/dev/` (or `/proc/`, where
/// Linux's `/dev/stdout` points). `/dev/../x` is not.
fn is_dev(p: &str) -> bool {
    if !p.starts_with('/') {
        return false;
    }
    let folded = crate::locks::context::normalize(Path::new(p));
    let real = std::fs::canonicalize(&folded).unwrap_or_else(|_| folded.clone());
    let under =
        |x: &Path| x.starts_with("/dev") && x != Path::new("/dev") || x.starts_with("/proc");
    under(&folded) && under(&real)
}

/// The writes a command line makes (Ruling 11), paths as in `write_paths`.
pub fn writes(command: &str) -> Vec<Write> {
    let mut out: Vec<Write> = Vec::new();
    for seg in write_segments(command, Path::new("")) {
        for w in segment_writes(&seg) {
            let c = &seg.cwd;
            let w = match w {
                Write::File(p) => fix(&p, c).map(Write::File),
                Write::Delete(p) => fix(&p, c).map(Write::Delete),
                Write::DeleteTree(p) => fix(&p, c).map(Write::DeleteTree),
                Write::Copy { target, sources } => fix(&target, c).map(|target| Write::Copy {
                    target,
                    sources: sources.iter().filter_map(|s| fix(s, c)).collect(),
                }),
            };
            if let Some(w) = w {
                if !out.contains(&w) {
                    out.push(w);
                }
            }
        }
    }
    out
}

/// The paths a command line writes (Ruling 11): `>` and `>>` targets,
/// `tee [-a]` files, `sed -i`/`perl -i` files, the last argument of `cp`
/// and `mv` (and the sources of `mv`, which are removed), every argument of
/// `rm`, `rmdir`, `touch` and `truncate`. Paths are as written, with
/// `$HOME` as `~`, joined to the folder of an earlier `cd`; a relative one
/// is left for the caller to resolve against the hook's cwd. Words with
/// other variables or substitutions are dropped. Globs stay as written;
/// the daemon expands them.
pub fn write_paths(command: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for w in writes(command) {
        let p = match w {
            Write::File(p) | Write::Delete(p) | Write::DeleteTree(p) => p,
            Write::Copy { target, .. } => target,
        };
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Whether `w` has a shell wildcard.
pub fn has_glob(w: &str) -> bool {
    w.contains(['*', '?', '['])
}

fn wild(pat: &[char], s: &[char]) -> bool {
    match pat.first() {
        None => s.is_empty(),
        Some('*') => (0..=s.len()).any(|k| wild(&pat[1..], &s[k..])),
        Some('?') => !s.is_empty() && wild(&pat[1..], &s[1..]),
        Some('[') => {
            let Some(end) = pat.iter().skip(2).position(|c| *c == ']').map(|e| e + 2) else {
                return s.first() == Some(&'[') && wild(&pat[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let set = &pat[1..end];
            let (neg, set) = match set.first() {
                Some('!' | '^') => (true, &set[1..]),
                _ => (false, set),
            };
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == '-' {
                    hit |= set[i] <= c && c <= set[i + 2];
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != neg && wild(&pat[end + 1..], &s[1..])
        }
        Some(p) => s.first() == Some(p) && wild(&pat[1..], &s[1..]),
    }
}

/// The paths a glob matches, one directory level per path component,
/// resolved against `cwd` (`~` against `home`). Hidden names match only a
/// pattern that starts with a dot. Empty when nothing matches.
pub fn glob(pattern: &str, cwd: &Path, home: &Path) -> Vec<PathBuf> {
    let full = match pattern.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => cwd.join(pattern),
    };
    let mut found = vec![PathBuf::new()];
    for comp in full.components() {
        let name = comp.as_os_str().to_string_lossy().into_owned();
        let mut next = Vec::new();
        if matches!(comp, std::path::Component::Normal(_)) && has_glob(&name) {
            let pat: Vec<char> = name.chars().collect();
            for dir in &found {
                let Ok(rd) = std::fs::read_dir(dir) else {
                    continue;
                };
                let mut names: Vec<String> = rd
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| !n.starts_with('.') || name.starts_with('.'))
                    .filter(|n| wild(&pat, &n.chars().collect::<Vec<_>>()))
                    .collect();
                names.sort();
                next.extend(names.into_iter().map(|n| dir.join(n)));
            }
        } else {
            next.extend(found.iter().map(|d| d.join(comp.as_os_str())));
        }
        found = next;
        found.truncate(1000);
    }
    found.retain(|p| p.exists());
    found
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
        Some(args) if segs.len() == 1 && !bd_line_unsafe(command, &segs[0]) => BdCall::Write(args),
        Some(_) => BdCall::Compound,
        None if found => BdCall::Read,
        None => BdCall::None,
    }
}

/// A single `bd` write line the rewrite cannot keep inside the vault
/// checks: command substitution (anywhere, quoted or not, but not in the
/// body of a quoted heredoc), a file redirect, or anything before the `bd`
/// word (an env prefix, `command`, a path).
fn bd_line_unsafe(command: &str, seg: &Segment) -> bool {
    let t = command.trim_start();
    let plain_start = t
        .strip_prefix("bd")
        .is_some_and(|r| r.starts_with([' ', '\t']));
    if !plain_start {
        return true;
    }
    if seg.redirects.iter().any(|r| !is_dev(r)) {
        return true;
    }
    // Everything is scanned but the bodies of heredocs with a quoted
    // delimiter, which the shell reads literally (`<<A <<'B'` scans A's).
    let mut scanned = String::new();
    let mut at = 0;
    for r in literal_heredoc_bodies(command) {
        scanned.push_str(&command[at..r.start]);
        at = r.end;
    }
    scanned.push_str(&command[at..]);
    scanned.contains("$(") || scanned.contains("${") || scanned.contains('`')
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
        assert_eq!(w("mv -f x.md y.md"), ["x.md", "y.md"]);
        assert_eq!(w("cp --target-directory=Sessions x.md"), ["Sessions"]);
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

    #[test]
    fn only_recursive_deletes_are_trees() {
        assert_eq!(writes("rm -rf d"), [Write::DeleteTree("d".into())]);
        assert_eq!(writes("rm -R d"), [Write::DeleteTree("d".into())]);
        assert_eq!(writes("rm --recursive d"), [Write::DeleteTree("d".into())]);
        assert_eq!(writes("rmdir d"), [Write::DeleteTree("d".into())]);
        assert_eq!(writes("rm -f a.md"), [Write::Delete("a.md".into())]);
        assert_eq!(
            writes("mv a.md dir/"),
            [
                Write::Delete("a.md".into()),
                Write::Copy {
                    target: "dir/".into(),
                    sources: vec!["a.md".into()]
                }
            ]
        );
        assert_eq!(
            writes("cp -r a.md b.md dir"),
            [Write::Copy {
                target: "dir".into(),
                sources: vec!["a.md".into(), "b.md".into()]
            }]
        );
    }

    #[test]
    fn option_clusters_only_count_for_their_own_tool() {
        // perl -M and -I take the rest of the word; they are not -i.
        assert!(w("perl -Mstrict -ne print 06-Daily/j.md").is_empty());
        assert!(w("perl -MList::Util=sum -ne 'print' j.md").is_empty());
        assert!(w("perl -I lib -mstrict -ne print j.md").is_empty());
        assert_eq!(w("perl -Mstrict -pi -e 's/a/b/' j.md"), ["j.md"]);
        assert_eq!(w("perl -pi.bak -e 's/a/b/' j.md"), ["j.md"]);
        assert_eq!(w("perl -i s.pl j.md"), ["j.md"]);
        // sed: -n and -E are not in-place; --in-place and -i are.
        assert!(w("sed -nE s/a/b/p j.md").is_empty());
        assert_eq!(w("sed --in-place s/a/b/ j.md"), ["j.md"]);
        assert_eq!(w("sed -ni s/a/b/p j.md"), ["j.md"]);
        assert_eq!(w("sed -E -i '' -e s/a/b/ j.md"), ["j.md"]);
    }

    #[test]
    fn bd_writes_with_substitution_redirects_or_prefixes_are_refused() {
        let denied = |c: &str| assert_eq!(bd_call(c), BdCall::Compound, "{c}");
        denied("bd create \"x $(git -C ~/Repos/2ndBrain push)\"");
        denied("bd create 'x $(echo)'");
        denied("bd create \"x `date`\"");
        denied("bd create x > 06-Daily/2026/11-November/2026-11-13-journal.md");
        denied("bd create x >> j.md");
        denied("FOO=1 bd create x");
        denied("env X=y bd create x");
        denied("command bd create x");
        denied("/usr/local/bin/bd create x");
        // /dev redirects, plain lines and heredocs with a quoted delimiter
        // are rewritten.
        for ok in [
            "bd create x 2>/dev/null",
            "bd create x --json",
            "bd create x --body-file - <<'EOF'\nuses $(x) and `y` literally\nEOF",
            "bd create x --body-file - <<\"EOF\"\n$(x)\nEOF",
        ] {
            assert!(matches!(bd_call(ok), BdCall::Write(_)), "{ok}");
        }
        // An unquoted delimiter expands its body.
        denied("bd create x --body-file - <<EOF\n$(git push)\nEOF");
        // Substitution on the operator's own line is still caught.
        denied("bd create \"$(git push)\" --body-file - <<'EOF'\nbody\nEOF");
        // Reads are not touched.
        assert_eq!(bd_call("bd show \"$(echo ov-3)\""), BdCall::Read);
    }

    #[test]
    fn vault_git_follows_cd_with_a_redirect_or_home_variable() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let brain = home.join("Repos/2ndBrain");
        let other = home.join("Repos/other");
        let v = |c: &str| vault_git_in(c, &other, &brain, &home);
        assert_eq!(
            v("cd ~/Repos/2ndBrain 2>/dev/null && git push").as_deref(),
            Some("push")
        );
        assert_eq!(
            v("cd $HOME/Repos/2ndBrain && git commit -m x").as_deref(),
            Some("commit")
        );
        assert_eq!(v("cd ~/Repos/2ndBrain 2>/dev/null && git log"), None);
    }

    #[test]
    fn globs_expand_one_level_per_component() {
        let dir = std::env::temp_dir().join(format!("ov-glob-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        for f in ["a/x1.md", "a/x2.md", "a/y.txt", "a/.hid.md", "a/b/z.md"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let g = |p: &str| -> Vec<String> {
            glob(p, &dir, Path::new("/nohome"))
                .iter()
                .map(|p| p.strip_prefix(&dir).unwrap().display().to_string())
                .collect()
        };
        assert_eq!(g("a/*.md"), ["a/x1.md", "a/x2.md"]);
        assert_eq!(g("a/x?.md"), ["a/x1.md", "a/x2.md"]);
        assert_eq!(g("a/x[1].md"), ["a/x1.md"]);
        assert_eq!(g("a/x[!1].md"), ["a/x2.md"]);
        assert_eq!(g("*/b/*.md"), ["a/b/z.md"]);
        assert_eq!(g("a/.h*"), ["a/.hid.md"]);
        assert!(g("a/*.rs").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn heredoc_bodies_are_exempt_only_when_really_quoted() {
        let denied = |c: &str| assert_eq!(bd_call(c), BdCall::Compound, "{c}");
        // A `<<'E'` inside a quoted string is not a heredoc.
        denied("bd create \"x <<'E'\n$(evil)\"");
        denied("bd create x <<< \"$(evil)\"");
        denied("bd create 'x <<\"E\"' \"$(evil)\"");
        // Two heredocs: the unquoted one's body expands.
        denied("bd create x --body-file - <<'A' <<B\nliteral $(x)\nA\n$(x)\nB");
        denied("bd create x --body-file - <<'A' <<B\nliteral\nA\n${HOME}\nB");
        // Only quoted bodies are exempt, whatever else is on the line.
        for ok in [
            "bd create x --body-file - <<'EOF'\n$(x) `y` ${z}\nEOF",
            "bd create x --body-file - <<'A' <<\"B\"\n$(x)\nA\n`y`\nB",
            "bd create x --body-file - <<\\EOF\n$(x)\nEOF",
        ] {
            assert!(matches!(bd_call(ok), BdCall::Write(_)), "{ok}");
        }
        // ${...} outside a literal body is refused too.
        denied("bd create \"x ${HOME}\"");
    }

    #[test]
    fn dev_paths_are_exempt_only_when_they_stay_under_dev() {
        assert!(is_dev("/dev/null"));
        assert!(!is_dev("/dev/../Users/j/journal.md"));
        assert!(!is_dev("/dev/./../etc/x"));
        assert!(!is_dev("/dev"));
        assert!(!is_dev("dev/null"));
        let denied = |c: &str| assert_eq!(bd_call(c), BdCall::Compound, "{c}");
        denied("bd create x > /dev/../vault/06-Daily/j.md");
        denied("bd create x 2> /dev/../vault/j.md");
        assert!(matches!(
            bd_call("bd create x > /dev/null"),
            BdCall::Write(_)
        ));
        // A non-bd write through `/dev/..` is judged, not skipped.
        assert_eq!(w("echo x > /dev/../vault/j.md"), ["/dev/../vault/j.md"]);
        assert_eq!(w("tee /dev/../vault/j.md"), ["/dev/../vault/j.md"]);
        assert_eq!(
            w("cd /dev && echo x > ../vault/j.md"),
            ["/dev/../vault/j.md"]
        );
        assert!(w("echo x > /dev/null").is_empty());
        assert!(w("cd /dev && echo x > null").is_empty());
    }
}
