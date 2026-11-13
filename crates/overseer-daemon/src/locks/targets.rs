//! Shared write targets from a Bash command: which targets it writes to,
//! by the compiled-in patterns plus an optional `targets.toml`.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::Deserialize;

use super::context::{normalize, Context};

const DEFAULTS: &str = include_str!("default-targets.toml");

/// Target prefixes; anything else is not a target.
const PREFIXES: [&str; 6] = ["repo:", "argo:", "deploy:", "tf:", "kube:", "custom:"];

/// Placeholders filled from the machine or the folder, not from the command.
const SPECIAL: [&str; 6] = [
    "argo_ctx",
    "kube_ctx",
    "tf_workspace",
    "repo",
    "branch",
    "dir",
];

/// Revisions that end a pin when a pin pattern gives no `unpin` list.
const DEFAULT_UNPIN: [&str; 3] = ["main", "master", "HEAD"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternKind {
    Command,
    Pin,
}

/// One parsed and compiled pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub name: String,
    pub command: Regex,
    pub skip: Option<Regex>,
    pub target: String,
    pub kind: PatternKind,
    pub unpin: Vec<String>,
    /// Extra values: the first group of each regex, searched in the command.
    pub fields: Vec<(String, Regex)>,
    /// Used when a field or capture is missing.
    pub defaults: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    replace_defaults: bool,
    #[serde(default)]
    pattern: Vec<RawPattern>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPattern {
    name: String,
    command: String,
    #[serde(default)]
    skip: Option<String>,
    target: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    unpin: Option<Vec<String>>,
    #[serde(default)]
    fields: BTreeMap<String, String>,
    #[serde(default)]
    defaults: BTreeMap<String, String>,
}

fn flat(e: impl std::fmt::Display) -> String {
    e.to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn placeholder_re() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\{([A-Za-z_][A-Za-z0-9_]*)\}").expect("placeholder regex"))
}

impl Pattern {
    fn compile(raw: RawPattern) -> Result<Pattern, String> {
        let name = raw.name;
        let re = |what: &str, src: &str| {
            Regex::new(src)
                .map_err(|e| format!("pattern \"{name}\": bad {what} regex: {}", flat(e)))
        };
        let command = re("command", &raw.command)?;
        let skip = raw.skip.as_deref().map(|s| re("skip", s)).transpose()?;
        let mut fields = Vec::new();
        for (field, src) in &raw.fields {
            let r = re(&format!("field {field}"), src)?;
            if r.captures_len() < 2 {
                return Err(format!(
                    "pattern \"{name}\": field {field} has no capture group"
                ));
            }
            fields.push((field.clone(), r));
        }
        let kind = match raw.kind.as_deref() {
            None | Some("command") => PatternKind::Command,
            Some("pin") => PatternKind::Pin,
            Some(k) => {
                return Err(format!(
                    "pattern \"{name}\": kind \"{k}\" is not command or pin"
                ))
            }
        };
        let captures: Vec<&str> = command.capture_names().flatten().collect();
        if kind == PatternKind::Pin && !captures.contains(&"revision") {
            return Err(format!(
                "pattern \"{name}\": a pin needs a (?P<revision>...) capture"
            ));
        }
        let ph = placeholder_re();
        for c in ph.captures_iter(&raw.target) {
            let p = &c[1];
            let known = SPECIAL.contains(&p)
                || captures.contains(&p)
                || raw.fields.contains_key(p)
                || raw.defaults.contains_key(p);
            if !known {
                return Err(format!(
                    "pattern \"{name}\": target placeholder {{{p}}} is not a capture, field or default"
                ));
            }
        }
        if !valid_target(&ph.replace_all(&raw.target, "x")) {
            return Err(format!(
                "pattern \"{name}\": target \"{}\" must start with one of {} and have no spaces or stray braces",
                raw.target,
                PREFIXES.join(" ")
            ));
        }
        let unpin = match (kind, raw.unpin) {
            (PatternKind::Pin, None) => DEFAULT_UNPIN.iter().map(|s| s.to_string()).collect(),
            (_, u) => u.unwrap_or_default(),
        };
        Ok(Pattern {
            name,
            command,
            skip,
            target: raw.target,
            kind,
            unpin,
            fields,
            defaults: raw.defaults,
        })
    }
}

fn parse(text: &str) -> Result<(bool, Vec<Pattern>), String> {
    let file: File = toml::from_str(text).map_err(flat)?;
    let patterns = file
        .pattern
        .into_iter()
        .map(Pattern::compile)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((file.replace_defaults, patterns))
}

/// The compiled-in patterns.
pub fn default_patterns() -> Vec<Pattern> {
    parse(DEFAULTS).expect("default-targets.toml").1
}

/// A missing file gives the defaults and `None`; a bad file or a bad regex
/// gives the defaults and the error text. The file's patterns are checked
/// before the defaults, which `replace_defaults = true` drops.
pub fn load_patterns(path: &Path) -> (Vec<Pattern>, Option<String>) {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (default_patterns(), None),
        Err(e) => return (default_patterns(), Some(format!("{}: {e}", path.display()))),
    };
    match parse(&text) {
        Ok((true, mine)) => (mine, None),
        Ok((false, mut mine)) => {
            mine.extend(default_patterns());
            (mine, None)
        }
        Err(e) => (default_patterns(), Some(format!("{}: {e}", path.display()))),
    }
}

/// One command of a Bash line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// Unquoted words from the command word on, redirections left out.
    pub words: Vec<String>,
    /// The source text from the command word to the last word, comments
    /// and heredoc bodies left out.
    pub text: String,
    /// The folder it runs in, after any earlier `cd`.
    pub cwd: PathBuf,
    /// Targets of its output redirections (`>`, `>>`, `2>`, `&>`), quotes
    /// removed. Only `write_segments` fills it.
    pub redirects: Vec<String>,
}

/// A command as the lexer saw it: words with their source offsets.
struct RawSegment {
    words: Vec<(String, usize)>,
    redirects: Vec<String>,
    end: usize,
}

struct Lexer<'a> {
    src: &'a str,
    chars: Vec<(usize, char)>,
    i: usize,
}

impl Lexer<'_> {
    fn peek(&self, k: usize) -> Option<char> {
        self.chars.get(self.i + k).map(|&(_, c)| c)
    }

    fn offset(&self) -> usize {
        self.chars.get(self.i).map_or(self.src.len(), |&(o, _)| o)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(' ' | '\t')) {
            self.i += 1;
        }
    }

    /// One word, quotes removed, up to unquoted whitespace or an operator.
    fn word(&mut self) -> String {
        let mut w = String::new();
        while let Some(c) = self.peek(0) {
            match c {
                '\'' => {
                    self.i += 1;
                    while let Some(c) = self.peek(0) {
                        self.i += 1;
                        if c == '\'' {
                            break;
                        }
                        w.push(c);
                    }
                }
                '"' => {
                    self.i += 1;
                    while let Some(c) = self.peek(0) {
                        self.i += 1;
                        match c {
                            '"' => break,
                            '\\' => match self.peek(0) {
                                Some('\n') => self.i += 1,
                                Some(n @ ('"' | '\\' | '$' | '`')) => {
                                    w.push(n);
                                    self.i += 1;
                                }
                                _ => w.push('\\'),
                            },
                            c => w.push(c),
                        }
                    }
                }
                '\\' => {
                    self.i += 1;
                    match self.peek(0) {
                        Some('\n') => self.i += 1,
                        Some(n) => {
                            w.push(n);
                            self.i += 1;
                        }
                        None => {}
                    }
                }
                ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')' | '`' => break,
                '$' if self.peek(1) == Some('(') => break,
                c => {
                    w.push(c);
                    self.i += 1;
                }
            }
        }
        w
    }

    /// The rest of the line, newline consumed.
    fn line(&mut self) -> String {
        let mut l = String::new();
        while let Some(c) = self.peek(0) {
            self.i += 1;
            if c == '\n' {
                break;
            }
            l.push(c);
        }
        l
    }

    fn split(mut self) -> Vec<RawSegment> {
        let mut out = Vec::new();
        let mut words: Vec<(String, usize)> = Vec::new();
        let mut redirects: Vec<String> = Vec::new();
        // End of the last word or redirection, so comments stay out of the text.
        let mut last_end = 0;
        let mut heredocs: Vec<(String, bool)> = Vec::new();
        let mut end =
            |words: &mut Vec<(String, usize)>, redirects: &mut Vec<String>, last_end: usize| {
                if !words.is_empty() || !redirects.is_empty() {
                    out.push(RawSegment {
                        words: std::mem::take(words),
                        redirects: std::mem::take(redirects),
                        end: last_end,
                    });
                }
            };
        while let Some(c) = self.peek(0) {
            match c {
                ' ' | '\t' => self.i += 1,
                '\\' if self.peek(1) == Some('\n') => self.i += 2,
                '\n' => {
                    end(&mut words, &mut redirects, last_end);
                    self.i += 1;
                    for (delim, strip_tabs) in heredocs.drain(..) {
                        while self.i < self.chars.len() {
                            let l = self.line();
                            let l = if strip_tabs {
                                l.trim_start_matches('\t')
                            } else {
                                &l
                            };
                            if l == delim {
                                break;
                            }
                        }
                    }
                }
                ';' | '(' | ')' | '`' => {
                    end(&mut words, &mut redirects, last_end);
                    self.i += 1;
                }
                '$' if self.peek(1) == Some('(') => {
                    end(&mut words, &mut redirects, last_end);
                    self.i += 2;
                }
                '|' => {
                    end(&mut words, &mut redirects, last_end);
                    self.i += if matches!(self.peek(1), Some('|' | '&')) {
                        2
                    } else {
                        1
                    };
                }
                '&' if self.peek(1) == Some('>') => {
                    self.i += if self.peek(2) == Some('>') { 3 } else { 2 };
                    self.skip_blanks();
                    let target = self.word();
                    if !target.is_empty() {
                        redirects.push(target);
                    }
                    last_end = self.offset();
                }
                '&' => {
                    end(&mut words, &mut redirects, last_end);
                    self.i += if self.peek(1) == Some('&') { 2 } else { 1 };
                }
                '<' | '>' => {
                    if let Some(target) = self.redirect(&mut heredocs) {
                        redirects.push(target);
                    }
                    last_end = self.offset();
                }
                '#' => {
                    while !matches!(self.peek(0), None | Some('\n')) {
                        self.i += 1;
                    }
                }
                _ => {
                    let start = self.offset();
                    let w = self.word();
                    // `2>file`: the digits are the redirection's fd, not a word.
                    let fd = matches!(self.peek(0), Some('<' | '>'))
                        && !w.is_empty()
                        && w.bytes().all(|b| b.is_ascii_digit());
                    if !fd {
                        words.push((w, start));
                        last_end = self.offset();
                    }
                }
            }
        }
        end(&mut words, &mut redirects, last_end);
        out
    }

    /// A redirection at `<` or `>`. A heredoc is queued so its body is
    /// skipped at the next newline. Gives the target of an output
    /// redirection to a file (not `>&2`, not an input).
    fn redirect(&mut self, heredocs: &mut Vec<(String, bool)>) -> Option<String> {
        let c = self.peek(0);
        self.i += 1;
        if c == Some('<') && self.peek(0) == Some('<') {
            self.i += 1;
            if self.peek(0) == Some('<') {
                self.i += 1;
            } else {
                let strip_tabs = self.peek(0) == Some('-');
                if strip_tabs {
                    self.i += 1;
                }
                self.skip_blanks();
                let delim = self.word();
                if !delim.is_empty() {
                    heredocs.push((delim, strip_tabs));
                }
                return None;
            }
        } else if matches!(self.peek(0), Some('>' | '&' | '|')) {
            self.i += 1;
        }
        self.skip_blanks();
        let target = self.word();
        let dup =
            target == "-" || (!target.is_empty() && target.bytes().all(|b| b.is_ascii_digit()));
        (c == Some('>') && !target.is_empty() && !dup).then_some(target)
    }
}

fn is_assignment(w: &str) -> bool {
    match w.split_once('=') {
        Some((name, _)) => {
            let mut b = name.bytes();
            b.next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
        }
        None => false,
    }
}

/// Shell words that run the command after them.
const PREFIX_WORDS: [&str; 14] = [
    "!", "{", "}", "if", "then", "elif", "else", "while", "until", "do", "time", "command", "exec",
    "nohup",
];

/// Index of the command word: leading assignments, `env` and its options,
/// and shell keywords are passed over.
fn command_word(words: &[(String, usize)]) -> Option<usize> {
    let mut i = 0;
    while let Some((w, _)) = words.get(i) {
        if is_assignment(w) || PREFIX_WORDS.contains(&w.as_str()) {
            i += 1;
        } else if w == "env" {
            i += 1;
            while let Some((w, _)) = words.get(i) {
                if matches!(w.as_str(), "-u" | "--unset" | "-C" | "--chdir" | "-S") {
                    i += 2;
                } else if w.starts_with('-') || is_assignment(w) {
                    i += 1;
                } else {
                    break;
                }
            }
        } else {
            return Some(i);
        }
    }
    None
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// `dir` against `cwd`, with `~` expanded.
fn resolve(cwd: &Path, dir: &str) -> PathBuf {
    // `$HOME` and `${HOME}` prefixes mean the same as `~`.
    let tilde;
    let dir = match ["$HOME", "${HOME}"].iter().find_map(|v| {
        dir.strip_prefix(v)
            .filter(|r| r.is_empty() || r.starts_with('/'))
    }) {
        Some(rest) => {
            tilde = format!("~{rest}");
            tilde.as_str()
        }
        None => dir,
    };
    let p = match dir.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => match home() {
            Some(h) => h.join(rest.trim_start_matches('/')),
            None => PathBuf::from(dir),
        },
        _ => PathBuf::from(dir),
    };
    normalize(&cwd.join(p))
}

/// Quote-aware split on `;`, `&&`, `||`, `|`, `&` and newlines. Heredoc
/// bodies and comments are dropped, and so are leading `VAR=value` words
/// and an `env` prefix. A `cd <dir>` changes `cwd` for what follows.
pub fn segments(command: &str, cwd: &Path) -> Vec<Segment> {
    write_segments(command, cwd)
        .into_iter()
        .filter(|s| !s.words.is_empty())
        .collect()
}

/// `segments` that also keeps a command with no command word but a
/// redirection (`> file`, `: > file`) and fills `Segment::redirects`.
pub fn write_segments(command: &str, cwd: &Path) -> Vec<Segment> {
    let lexer = Lexer {
        src: command,
        chars: command.char_indices().collect(),
        i: 0,
    };
    let mut cwd = cwd.to_path_buf();
    let mut out = Vec::new();
    for raw in lexer.split() {
        let Some(at) = command_word(&raw.words) else {
            if !raw.redirects.is_empty() {
                out.push(Segment {
                    words: Vec::new(),
                    text: String::new(),
                    cwd: cwd.clone(),
                    redirects: raw.redirects,
                });
            }
            continue;
        };
        let start = raw.words[at].1;
        let text = command[start..raw.end.max(start)]
            .replace("\\\n", " ")
            .trim()
            .to_owned();
        let words: Vec<String> = raw.words[at..].iter().map(|(w, _)| w.clone()).collect();
        let here = cwd.clone();
        if words[0] == "cd" {
            let dir = words[1..]
                .iter()
                .find(|w| !w.starts_with('-') || w.as_str() == "-");
            match dir.map(String::as_str) {
                // `cd -` goes to a folder the line cannot know.
                Some("-") => {}
                Some(d) => cwd = resolve(&cwd, d),
                None => {
                    if let Some(h) = home() {
                        cwd = h;
                    }
                }
            }
        }
        out.push(Segment {
            words,
            text,
            cwd: here,
            redirects: raw.redirects,
        });
    }
    out
}

/// The branches a `git push` writes: positional words after `push` (options
/// skipped, `-o`/`--push-option`/`--repo` take a value), the first being the
/// remote. Each refspec gives its destination; none, or `HEAD`, gives the
/// current branch; `--all` or `--mirror` gives `*`.
pub fn push_branches(words: &[String], current: Option<&str>) -> Vec<String> {
    let Some(at) = words.iter().position(|w| w == "push") else {
        return Vec::new();
    };
    let mut positional = Vec::new();
    let mut rest = words[at + 1..].iter();
    let mut options = true;
    while let Some(w) = rest.next() {
        if options && w == "--" {
            options = false;
        } else if options && w.starts_with('-') && w.len() > 1 {
            match w.as_str() {
                "--all" | "--mirror" => return vec!["*".into()],
                "-o" | "--push-option" | "--repo" => {
                    rest.next();
                }
                _ => {}
            }
        } else {
            positional.push(w.as_str());
        }
    }
    let mut out: Vec<String> = Vec::new();
    let mut add = |b: Option<&str>| {
        if let Some(b) = b.filter(|b| !b.is_empty()) {
            if !out.iter().any(|o| o == b) {
                out.push(b.to_owned());
            }
        }
    };
    let refspecs = positional.get(1..).unwrap_or_default();
    if refspecs.is_empty() {
        add(current);
    }
    for spec in refspecs {
        let spec = spec.trim_start_matches('+');
        let dst = match spec.rsplit_once(':') {
            Some((src, "")) => src,
            Some((_, dst)) => dst,
            None => spec,
        };
        let dst = dst.strip_prefix("refs/heads/").unwrap_or(dst);
        add(if dst == "HEAD" { current } else { Some(dst) });
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitKind {
    Command,
    Pin { revision: String },
    Unpin { revision: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub target: String,
    pub kind: HitKind,
    pub pattern: String,
}

fn unquote(s: &str) -> &str {
    for q in ['"', '\''] {
        if let Some(inner) = s.strip_prefix(q).and_then(|s| s.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// The value after `flag` (`--flag v` or `--flag=v`).
fn flag_value(words: &[String], flag: &str) -> Option<String> {
    let mut it = words.iter();
    while let Some(w) = it.next() {
        if w == flag {
            return it.next().cloned();
        }
        if let Some(v) = w.strip_prefix(flag).and_then(|v| v.strip_prefix('=')) {
            return Some(v.to_owned());
        }
    }
    None
}

/// A capture of the command regex, else the first group of a field regex.
fn captured(p: &Pattern, caps: &regex::Captures, text: &str, name: &str) -> Option<String> {
    caps.name(name)
        .map(|m| unquote(m.as_str()).to_owned())
        .or_else(|| {
            let (_, re) = p.fields.iter().find(|(f, _)| f == name)?;
            Some(unquote(re.captures(text)?.get(1)?.as_str()).to_owned())
        })
        .filter(|v| !v.is_empty())
}

/// Every target a matched pattern gives for a segment; empty when a
/// placeholder cannot be filled.
fn render(p: &Pattern, caps: &regex::Captures, seg: &Segment, ctx: &dyn Context) -> Vec<String> {
    let dir = match captured(p, caps, &seg.text, "dir") {
        Some(d) => resolve(&seg.cwd, &d),
        None => seg.cwd.clone(),
    };
    let value = |name: &str| -> Option<Vec<String>> {
        let got = match name {
            "dir" => Some(vec![dir.display().to_string()]),
            _ => match captured(p, caps, &seg.text, name) {
                Some(v) => Some(vec![v]),
                None => match name {
                    "repo" => ctx.repo(&dir).map(|v| vec![v]),
                    "branch" => {
                        let current = ctx.branch(&dir);
                        let pushing = seg.words.first().is_some_and(|w| w == "git")
                            && seg.words.iter().any(|w| w == "push");
                        if pushing {
                            Some(push_branches(&seg.words, current.as_deref()))
                        } else {
                            current.map(|v| vec![v])
                        }
                    }
                    "argo_ctx" => flag_value(&seg.words, "--argocd-context")
                        .or_else(|| ctx.argo_ctx())
                        .map(|v| vec![v]),
                    "kube_ctx" => ctx
                        .kube_ctx(flag_value(&seg.words, "--context").as_deref())
                        .map(|v| vec![v]),
                    "tf_workspace" => ctx.tf_workspace(&dir).map(|v| vec![v]),
                    _ => None,
                },
            },
        };
        got.filter(|v| !v.is_empty())
            .or_else(|| p.defaults.get(name).map(|d| vec![d.clone()]))
    };
    let mut out = vec![String::new()];
    let mut last = 0;
    for m in placeholder_re().captures_iter(&p.target) {
        let whole = m.get(0).expect("whole match");
        let lit = &p.target[last..whole.start()];
        last = whole.end();
        let Some(values) = value(&m[1]) else {
            return Vec::new();
        };
        out = out
            .iter()
            .flat_map(|o| values.iter().map(move |v| format!("{o}{lit}{v}")))
            .collect();
    }
    for o in &mut out {
        o.push_str(&p.target[last..]);
    }
    out
}

/// The targets a Bash command writes to: for each segment the first pattern
/// that matches and is not skipped. One hit per target, the first one,
/// except that a pin or unpin replaces a plain command hit (so a sync then
/// a pin of one app keeps the pin); a placeholder that cannot be filled
/// drops that hit.
pub fn match_command(
    patterns: &[Pattern],
    command: &str,
    cwd: &Path,
    ctx: &dyn Context,
) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for seg in segments(command, cwd) {
        for p in patterns {
            let Some(caps) = p.command.captures(&seg.text) else {
                continue;
            };
            if p.skip.as_ref().is_some_and(|s| s.is_match(&seg.text)) {
                continue;
            }
            let kind = match p.kind {
                PatternKind::Command => Some(HitKind::Command),
                PatternKind::Pin => captured(p, &caps, &seg.text, "revision").map(|revision| {
                    if p.unpin.contains(&revision) {
                        HitKind::Unpin { revision }
                    } else {
                        HitKind::Pin { revision }
                    }
                }),
            };
            if let Some(kind) = kind {
                for target in render(p, &caps, &seg, ctx) {
                    if !valid_target(&target) {
                        continue;
                    }
                    let hit = Hit {
                        target: target.clone(),
                        kind: kind.clone(),
                        pattern: p.name.clone(),
                    };
                    match seen.get(&target) {
                        None => {
                            seen.insert(target, hits.len());
                            hits.push(hit);
                        }
                        Some(&i) if hits[i].kind == HitKind::Command => hits[i] = hit,
                        Some(_) => {}
                    }
                }
            }
            break;
        }
    }
    hits
}

/// A known prefix, a non-empty rest, no whitespace or braces, at most 200
/// characters.
pub fn valid_target(t: &str) -> bool {
    t.chars().count() <= 200
        && !t.chars().any(|c| c.is_whitespace() || c == '{' || c == '}')
        && PREFIXES
            .iter()
            .any(|p| t.strip_prefix(p).is_some_and(|rest| !rest.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// repo is the folder name, so a `cd` or `-C` shows in the target.
    struct Fake {
        branch: Option<&'static str>,
        argo: Option<&'static str>,
        kube: Option<&'static str>,
    }

    const FAKE: Fake = Fake {
        branch: Some("feature-x"),
        argo: Some("uat"),
        kube: Some("staging-eu"),
    };

    impl Context for Fake {
        fn repo(&self, dir: &Path) -> Option<String> {
            dir.file_name().map(|n| n.to_string_lossy().into_owned())
        }
        fn branch(&self, _dir: &Path) -> Option<String> {
            self.branch.map(str::to_owned)
        }
        fn argo_ctx(&self) -> Option<String> {
            self.argo.map(str::to_owned)
        }
        fn kube_ctx(&self, flag: Option<&str>) -> Option<String> {
            flag.or(self.kube).map(str::to_owned)
        }
        fn tf_workspace(&self, _dir: &Path) -> Option<String> {
            Some("default".into())
        }
    }

    fn cwd() -> PathBuf {
        PathBuf::from("/work/demo-api")
    }

    fn defaults() -> Vec<Pattern> {
        let (p, err) = load_patterns(Path::new("/nonexistent/targets.toml"));
        assert!(err.is_none(), "{err:?}");
        p
    }

    fn targets_with(patterns: &[Pattern], ctx: &dyn Context, command: &str) -> Vec<String> {
        match_command(patterns, command, &cwd(), ctx)
            .into_iter()
            .map(|h| h.target)
            .collect()
    }

    fn targets(command: &str) -> Vec<String> {
        targets_with(&defaults(), &FAKE, command)
    }

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Dir {
            let p = std::env::temp_dir().join(format!("ovs-tgt-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn file(&self, body: &str) -> PathBuf {
            let p = self.0.join("targets.toml");
            std::fs::write(&p, body).unwrap();
            p
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn defaults_parse_in_order() {
        let names: Vec<String> = defaults().into_iter().map(|p| p.name).collect();
        assert_eq!(
            names,
            [
                "git push",
                "argocd pin",
                "argocd set or sync",
                "trigger.dev deploy",
                "terraform apply or destroy",
                "kubectl apply, delete or patch",
            ]
        );
    }

    #[test]
    fn git_push_pattern() {
        let hits = match_command(&defaults(), "git push", &cwd(), &FAKE);
        assert_eq!(
            hits,
            [Hit {
                target: "repo:demo-api:feature-x".into(),
                kind: HitKind::Command,
                pattern: "git push".into(),
            }]
        );
    }

    #[test]
    fn argocd_pin_pattern() {
        let hits = match_command(
            &defaults(),
            "argocd app set demo-api --revision abc123",
            &cwd(),
            &FAKE,
        );
        assert_eq!(hits[0].target, "argo:uat/demo-api");
        assert_eq!(hits[0].pattern, "argocd pin");
    }

    #[test]
    fn a_pin_replaces_a_command_hit_on_the_same_target() {
        let hits = match_command(
            &defaults(),
            "argocd app sync demo-api && argocd app set demo-api --revision abc",
            &cwd(),
            &FAKE,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].target, "argo:uat/demo-api");
        assert_eq!(
            hits[0].kind,
            HitKind::Pin {
                revision: "abc".into()
            }
        );
        // The first pin is kept over a later command or pin.
        let hits = match_command(
            &defaults(),
            "argocd app set demo-api --revision abc; argocd app sync demo-api; argocd app set demo-api --revision main",
            &cwd(),
            &FAKE,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].kind,
            HitKind::Pin {
                revision: "abc".into()
            }
        );
    }

    #[test]
    fn argocd_set_or_sync_pattern() {
        let hits = match_command(&defaults(), "argocd app sync demo-api", &cwd(), &FAKE);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].target, "argo:uat/demo-api");
        assert_eq!(hits[0].kind, HitKind::Command);
        assert_eq!(hits[0].pattern, "argocd set or sync");
        assert_eq!(
            targets("argocd app set demo-api -p image.tag=v2"),
            ["argo:uat/demo-api"]
        );
        assert!(targets("argocd app sync demo-api --dry-run").is_empty());
    }

    #[test]
    fn trigger_dev_deploy_pattern() {
        assert_eq!(
            targets("npx trigger.dev@latest deploy"),
            ["deploy:trigger.dev/prod"]
        );
        assert_eq!(
            targets("bunx trigger.dev deploy --env=staging"),
            ["deploy:trigger.dev/staging"]
        );
        assert_eq!(
            targets("pnpm dlx trigger.dev@4 deploy -e uat"),
            ["deploy:trigger.dev/uat"]
        );
    }

    #[test]
    fn terraform_pattern() {
        assert_eq!(targets("terraform apply"), ["tf:default"]);
        assert_eq!(targets("tofu -chdir=../infra destroy"), ["tf:default"]);
    }

    #[test]
    fn kubectl_pattern() {
        assert_eq!(
            targets("kubectl apply -f x.yaml"),
            ["kube:staging-eu/default"]
        );
        assert_eq!(
            targets("kubectl --context uat-eu -n demo patch deploy demo-api -p {}"),
            ["kube:uat-eu/demo"]
        );
        assert_eq!(
            targets("kubectl delete pod demo-api-0 --namespace demo"),
            ["kube:staging-eu/demo"]
        );
    }

    #[test]
    fn push_refspecs() {
        let cur = Some("feature-x");
        assert_eq!(push_branches(&words("git push"), cur), ["feature-x"]);
        assert_eq!(
            push_branches(&words("git push -u origin HEAD"), cur),
            ["feature-x"]
        );
        assert_eq!(
            push_branches(&words("git push origin HEAD:main"), cur),
            ["main"]
        );
        assert_eq!(
            push_branches(&words("git push origin +main feature-y"), cur),
            ["main", "feature-y"]
        );
        assert_eq!(
            push_branches(&words("git push --force-with-lease origin feature-x"), cur),
            ["feature-x"]
        );
        assert_eq!(
            push_branches(&words("git push origin refs/heads/x"), cur),
            ["x"]
        );
        assert_eq!(
            push_branches(
                &words("git push -o ci.skip --repo origin upstream main"),
                cur
            ),
            ["main"]
        );
        assert_eq!(push_branches(&words("git push --all origin"), cur), ["*"]);
        assert_eq!(push_branches(&words("git push --mirror"), cur), ["*"]);
        assert!(push_branches(&words("git push"), None).is_empty());
        assert_eq!(
            push_branches(&words("git push origin HEAD:main"), None),
            ["main"]
        );

        assert_eq!(
            targets("git push origin +main feature-y"),
            ["repo:demo-api:main", "repo:demo-api:feature-y"]
        );
    }

    #[test]
    fn pin_and_unpin() {
        let hits = match_command(
            &defaults(),
            "argocd app set demo-api --revision abc123",
            &cwd(),
            &FAKE,
        );
        assert_eq!(
            hits[0].kind,
            HitKind::Pin {
                revision: "abc123".into()
            }
        );
        for rev in ["main", "master", "HEAD"] {
            let hits = match_command(
                &defaults(),
                &format!("argocd app set demo-api --revision {rev}"),
                &cwd(),
                &FAKE,
            );
            assert_eq!(
                hits[0].kind,
                HitKind::Unpin {
                    revision: rev.into()
                }
            );
            assert_eq!(hits[0].target, "argo:uat/demo-api");
        }
    }

    #[test]
    fn quoted_and_heredoc_writes_do_not_match() {
        for c in [
            r#"git commit -m "then git push""#,
            "echo 'argocd app sync x'",
            "cat <<EOF\nkubectl apply -f x\nEOF",
            "cat <<'EOF'\nkubectl apply -f x\nEOF\n",
            "cat <<-EOF\n\tgit push\n\tEOF",
            r#"grep "terraform apply" notes.md"#,
            "echo hi # && git push",
        ] {
            assert!(targets(c).is_empty(), "{c:?} gave {:?}", targets(c));
        }
        // After the heredoc ends, commands count again.
        assert_eq!(
            targets("cat <<EOF > x\nkubectl apply\nEOF\ngit push"),
            ["repo:demo-api:feature-x"]
        );
    }

    #[test]
    fn dry_runs_and_reads_do_not_match() {
        for c in [
            "git push --dry-run",
            "git push -n",
            "kubectl apply --dry-run=client -f x",
            "kubectl get pods",
            "argocd app get x",
            "terraform plan",
        ] {
            assert!(targets(c).is_empty(), "{c:?} gave {:?}", targets(c));
        }
        assert_eq!(
            targets("kubectl apply --dry-run=none -f x"),
            ["kube:staging-eu/default"]
        );
    }

    #[test]
    fn compound_writes_match() {
        let other = PathBuf::from("/work/other");
        let hits = match_command(&defaults(), "cd ../demo-api && git push", &other, &FAKE);
        assert_eq!(hits[0].target, "repo:demo-api:feature-x");
        let segs = segments("cd ../demo-api && git push", &other);
        assert_eq!(segs[1].cwd, PathBuf::from("/work/demo-api"));
        assert_eq!(segs[1].text, "git push");

        let hits = match_command(
            &defaults(),
            "git -C ../demo-api push origin HEAD:main",
            &other,
            &FAKE,
        );
        assert_eq!(hits[0].target, "repo:demo-api:main");
        assert_eq!(targets("FOO=1 terraform apply"), ["tf:default"]);
        assert_eq!(
            targets("make build && git push"),
            ["repo:demo-api:feature-x"]
        );
        assert_eq!(
            targets("git push && git push origin feature-x"),
            ["repo:demo-api:feature-x"],
            "one hit per target"
        );
    }

    #[test]
    fn segments_split_and_strip_prefixes() {
        let segs = segments(
            "A=1 B='x y' env -u C D=2 git push 2>&1 | tee log; ls\ngit status || echo \"a;b\" &",
            &cwd(),
        );
        let texts: Vec<&str> = segs.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "git push 2>&1",
                "tee log",
                "ls",
                "git status",
                "echo \"a;b\""
            ]
        );
        assert_eq!(segs[0].words, ["git", "push"]);
        assert_eq!(segs[4].words, ["echo", "a;b"]);
    }

    #[test]
    fn write_segments_keep_redirect_targets_only_for_files() {
        let segs = write_segments(
            "> a.md; echo x 2>> b.md >&2 | tee c.md <in.md >&d.md",
            &cwd(),
        );
        let r: Vec<&[String]> = segs.iter().map(|s| s.redirects.as_slice()).collect();
        assert_eq!(r[0], ["a.md"]);
        assert_eq!(r[1], ["b.md"]);
        // `<in.md` is an input; `>&d.md` is a file, `>&2` is not.
        assert_eq!(r[2], ["d.md"]);
        // `segments` still drops a redirect-only line.
        assert_eq!(segments("> a.md", &cwd()).len(), 0);
    }

    #[test]
    fn cd_with_a_redirect_still_locks_the_repo_it_moved_to() {
        assert_eq!(
            targets("cd /w/other-repo 2>/dev/null && git push"),
            ["repo:other-repo:feature-x"]
        );
        assert_eq!(
            targets("cd /w/other-repo > /dev/null; git push"),
            ["repo:other-repo:feature-x"]
        );
    }

    #[test]
    fn cd_with_a_redirect_or_home_variable_still_moves_the_folder() {
        let home = home().unwrap();
        let segs = segments("cd ~/Repos/x 2>/dev/null && git push", &cwd());
        assert_eq!(segs[1].cwd, home.join("Repos/x"));
        let segs = segments("cd $HOME/Repos/x && git push", &cwd());
        assert_eq!(segs[1].cwd, home.join("Repos/x"));
        let segs = segments("cd ${HOME}/Repos/x && git push", &cwd());
        assert_eq!(segs[1].cwd, home.join("Repos/x"));
        let segs = segments("cd \"$HOME\" && git push", &cwd());
        assert_eq!(segs[1].cwd, home);
    }

    #[test]
    fn unfillable_placeholder_drops_the_hit() {
        let none = Fake {
            branch: None,
            argo: None,
            kube: None,
        };
        let p = defaults();
        assert!(targets_with(&p, &none, "git push").is_empty());
        assert_eq!(
            targets_with(&p, &none, "git push origin HEAD:main"),
            ["repo:demo-api:main"]
        );
        assert!(targets_with(&p, &none, "argocd app sync demo-api").is_empty());
        assert!(targets_with(&p, &none, "kubectl apply -f x").is_empty());
        assert_eq!(
            targets_with(&p, &none, "kubectl --context uat-eu apply -f x"),
            ["kube:uat-eu/default"]
        );
        // A placeholder value with whitespace is not a valid target.
        assert!(targets("git push origin 'a b'").is_empty());
    }

    #[test]
    fn user_file_comes_first_and_can_replace_defaults() {
        let d = Dir::new();
        let p = d.file(
            r#"
[[pattern]]
name = "pushes"
command = '^git\s+push\b'
target = "custom:pushes/{repo}"

[[pattern]]
name = "release"
command = '^make\s+release\s+(?P<svc>\S+)'
fields = { stage = '\sSTAGE=(\S+)' }
defaults = { stage = "beta" }
target = "custom:release/{svc}/{stage}"
"#,
        );
        let (pats, err) = load_patterns(&p);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(pats.len(), 8);
        assert_eq!(
            targets_with(&pats, &FAKE, "git push"),
            ["custom:pushes/demo-api"]
        );
        assert_eq!(
            targets_with(&pats, &FAKE, "make release demo-web"),
            ["custom:release/demo-web/beta"]
        );
        assert_eq!(
            targets_with(&pats, &FAKE, "make release demo-web STAGE=ga"),
            ["custom:release/demo-web/ga"]
        );
        assert_eq!(
            targets_with(&pats, &FAKE, "terraform apply"),
            ["tf:default"]
        );

        let p = d.file(
            r#"
replace_defaults = true
[[pattern]]
name = "pin"
command = '^deployctl\s+pin\s+(?P<svc>\S+)\s+(?P<revision>\S+)'
kind = "pin"
target = "custom:{svc}"
"#,
        );
        let (pats, err) = load_patterns(&p);
        assert!(err.is_none(), "{err:?}");
        assert_eq!(pats.len(), 1);
        assert!(targets_with(&pats, &FAKE, "git push").is_empty());
        let hits = match_command(&pats, "deployctl pin demo-api master", &cwd(), &FAKE);
        assert_eq!(
            hits[0].kind,
            HitKind::Unpin {
                revision: "master".into()
            },
            "a pin with no unpin list uses main, master, HEAD"
        );
    }

    #[test]
    fn bad_regex_falls_back_with_an_error() {
        let d = Dir::new();
        let bad = [
            (
                "[[pattern]]\nname = \"broken\"\ncommand = '(git'\ntarget = \"custom:x\"\n",
                "broken",
            ),
            (
                "[[pattern]]\nname = \"skip\"\ncommand = '^x'\nskip = '['\ntarget = \"custom:x\"\n",
                "skip",
            ),
            (
                "[[pattern]]\nname = \"nogroup\"\ncommand = '^x'\nfields = { a = 'b' }\ntarget = \"custom:{a}\"\n",
                "nogroup",
            ),
            (
                "[[pattern]]\nname = \"prefix\"\ncommand = '^x'\ntarget = \"other:x\"\n",
                "prefix",
            ),
            (
                "[[pattern]]\nname = \"unknown\"\ncommand = '^x'\ntarget = \"custom:{nope}\"\n",
                "nope",
            ),
            (
                "[[pattern]]\nname = \"pinless\"\ncommand = '^x'\nkind = \"pin\"\ntarget = \"custom:x\"\n",
                "revision",
            ),
            (
                "[[pattern]]\nname = \"kind\"\ncommand = '^x'\nkind = \"lock\"\ntarget = \"custom:x\"\n",
                "lock",
            ),
            ("[[pattern]]\nname = 3\n", "targets.toml"),
            (
                "[[pattern]]\nname = \"typo\"\ncomand = '^x'\ntarget = \"custom:x\"\n",
                "comand",
            ),
        ];
        for (body, needle) in bad {
            let (pats, err) = load_patterns(&d.file(body));
            let err = err.unwrap_or_else(|| panic!("no error for {body}"));
            assert!(err.contains(needle), "{err}");
            assert!(err.contains("targets.toml"), "{err}");
            assert_eq!(pats.len(), 6, "defaults for {body}");
        }
    }

    #[test]
    fn valid_targets() {
        for t in [
            "repo:demo-api:main",
            "argo:uat/demo-api",
            "deploy:trigger.dev/prod",
            "tf:default",
            "kube:staging-eu/default",
            "custom:x",
        ] {
            assert!(valid_target(t), "{t}");
        }
        for t in ["repo:", "other:x", "custom:a b", "custom:a\tb", "nothing"] {
            assert!(!valid_target(t), "{t}");
        }
        assert!(valid_target(&format!("custom:{}", "x".repeat(193))));
        assert!(!valid_target(&format!("custom:{}", "x".repeat(194))));
    }

    /// Each line is `<expected targets, comma separated, or ->` TAB `<command>`.
    fn check_corpus(patterns: &[Pattern], name: &str, body: &str) -> Vec<String> {
        let mut failures = Vec::new();
        for (n, line) in body.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((want, command)) = line.split_once('\t') else {
                failures.push(format!("{name}:{}: no tab", n + 1));
                continue;
            };
            let command = command.replace("\\n", "\n");
            let want: Vec<&str> = match want.trim() {
                "-" => vec![],
                w => w.split(',').map(str::trim).collect(),
            };
            let got = targets_with(patterns, &FAKE, &command);
            if got != want {
                failures.push(format!(
                    "{name}:{}: {command:?} gave {got:?}, want {want:?}",
                    n + 1
                ));
            }
        }
        failures
    }

    #[test]
    fn fixture_corpus() {
        let body = include_str!("../../tests/fixtures/commands.tsv");
        let failures = check_corpus(&defaults(), "commands.tsv", body);
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert!(body.lines().filter(|l| !l.starts_with('#')).count() > 30);
    }

    /// The same check over a local corpus of real commands, with the
    /// patterns the daemon would load. Only reads; skipped when the folder
    /// is missing (as on CI).
    #[test]
    fn local_corpus() {
        let dir = match std::env::var_os("OVERSEER_CORPUS").filter(|v| !v.is_empty()) {
            Some(d) => PathBuf::from(d),
            None => match std::env::var_os("HOME") {
                Some(h) => PathBuf::from(h).join(".config/overseer/corpus"),
                None => return,
            },
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let (patterns, err) = load_patterns(&overseer_proto::paths::targets_path());
        assert!(err.is_none(), "{err:?}");
        let mut files: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "tsv"))
            .collect();
        files.sort();
        let mut failures = Vec::new();
        for f in files {
            let body = std::fs::read_to_string(&f).unwrap();
            failures.extend(check_corpus(&patterns, &f.display().to_string(), &body));
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }
}
