//! Detect clickable URLs and file paths in a snapshot row.

use std::path::PathBuf;

use crate::bridge::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Url(String),
    /// Path (`~` left unexpanded) with an optional 1-based line number.
    Path(PathBuf, Option<u32>),
}

/// A link and the viewport columns it covers, `start_col..end_col`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSpan {
    pub link: Link,
    pub start_col: usize,
    pub end_col: usize,
}

/// Find a link under the cell at (`row`, `col`). Only the single row is
/// examined, so links wrapped across rows are not joined.
pub fn find_link_at(snap: &Snapshot, row: usize, col: usize) -> Option<Link> {
    find_link_span_at(snap, row, col).map(|s| s.link)
}

/// Like `find_link_at`, also giving the columns the link text covers.
pub fn find_link_span_at(snap: &Snapshot, row: usize, col: usize) -> Option<LinkSpan> {
    if row >= snap.rows || col >= snap.cols {
        return None;
    }
    // Characters with the column each starts at and its width. The spacer
    // after a wide character is skipped.
    let mut chars: Vec<(char, usize, usize)> = Vec::with_capacity(snap.cols);
    let mut skip = false;
    for c in 0..snap.cols {
        if skip {
            skip = false;
            continue;
        }
        let cell = snap.cell(row, c)?;
        chars.push((cell.ch, c, if cell.wide { 2 } else { 1 }));
        skip = cell.wide;
    }
    let idx = chars.iter().rposition(|&(_, c, _)| c <= col)?;
    if chars[idx].0.is_whitespace() || chars[idx].0 == '\0' {
        return None;
    }
    let is_sep = |ch: char| ch.is_whitespace() || ch == '\0';
    let mut start = idx;
    while start > 0 && !is_sep(chars[start - 1].0) {
        start -= 1;
    }
    let mut end = idx + 1;
    while end < chars.len() && !is_sep(chars[end].0) {
        end += 1;
    }
    // Trim wrapping punctuation, but never past the clicked character.
    let lead = |ch: char| matches!(ch, '(' | '[' | '{' | '<' | '\'' | '"' | '`');
    let trail = |ch: char| {
        matches!(
            ch,
            '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '>' | '\'' | '"' | '`'
        )
    };
    while start < idx && lead(chars[start].0) {
        start += 1;
    }
    while end > idx + 1 && trail(chars[end - 1].0) {
        end -= 1;
    }
    if lead(chars[idx].0) && start == idx || trail(chars[idx].0) && end == idx + 1 {
        // Clicked on the punctuation itself.
        return None;
    }
    let token: String = chars[start..end].iter().map(|&(c, _, _)| c).collect();
    let (_, last_col, last_width) = chars[end - 1];
    classify(&token).map(|link| LinkSpan {
        link,
        start_col: chars[start].1,
        end_col: last_col + last_width,
    })
}

fn classify(token: &str) -> Option<Link> {
    if (token.starts_with("http://") && token.len() > 7)
        || (token.starts_with("https://") && token.len() > 8)
        || (token.starts_with("file://") && token.len() > 7)
    {
        return Some(Link::Url(token.to_string()));
    }
    if token.contains("://") {
        return None;
    }
    let anchored = token.starts_with('/')
        || token.starts_with("~/")
        || token.starts_with("./")
        || token.starts_with("../");
    // A bare relative path such as `src/main.rs:1` needs a slash and must
    // start like a file name. Callers check that it exists before using it.
    let relative = token.contains('/')
        && token
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
    if !(anchored || relative) {
        return None;
    }
    // Peel up to two numeric suffixes: path:line or path:line:col.
    let mut path = token;
    let mut nums: Vec<u32> = Vec::new();
    while nums.len() < 2 {
        match path.rsplit_once(':') {
            Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => {
                nums.push(tail.parse().ok()?);
                path = head;
            }
            _ => break,
        }
    }
    if path.len() < 2 && path != "/" || path.is_empty() {
        return None;
    }
    if path == "/" || path == "~/" || path == "./" || path == "../" {
        return None;
    }
    // With line:col the line is the earlier number.
    let line = nums.last().copied();
    Some(Link::Path(PathBuf::from(path), line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::{TermBridge, DEFAULT_SCROLLBACK};

    fn snap(text: &str) -> Snapshot {
        let mut b = TermBridge::new(80, 5, DEFAULT_SCROLLBACK);
        b.feed(text.as_bytes());
        b.snapshot()
    }

    #[test]
    fn finds_https_url() {
        let s = snap("see https://example.com/a?b=1 now");
        let want = Some(Link::Url("https://example.com/a?b=1".into()));
        assert_eq!(find_link_at(&s, 0, 4), want);
        assert_eq!(find_link_at(&s, 0, 20), want);
        assert_eq!(find_link_at(&s, 0, 0), None);
    }

    #[test]
    fn url_trims_trailing_punctuation() {
        let s = snap("(https://example.com/x).");
        assert_eq!(
            find_link_at(&s, 0, 5),
            Some(Link::Url("https://example.com/x".into()))
        );
    }

    #[test]
    fn finds_path_with_line() {
        let s = snap("error in ~/Repos/eks/main.tf:42 here");
        assert_eq!(
            find_link_at(&s, 0, 14),
            Some(Link::Path(PathBuf::from("~/Repos/eks/main.tf"), Some(42)))
        );
    }

    #[test]
    fn finds_path_with_line_and_col_and_plain_path() {
        let s = snap("/usr/lib/x.rs:10:5: oops");
        assert_eq!(
            find_link_at(&s, 0, 3),
            Some(Link::Path(PathBuf::from("/usr/lib/x.rs"), Some(10)))
        );
        let s = snap("open ./src/main.rs");
        assert_eq!(
            find_link_at(&s, 0, 8),
            Some(Link::Path(PathBuf::from("./src/main.rs"), None))
        );
    }

    #[test]
    fn span_covers_link_text_only() {
        let s = snap("see (https://example.com/x). ok");
        assert_eq!(
            find_link_span_at(&s, 0, 10),
            Some(LinkSpan {
                link: Link::Url("https://example.com/x".into()),
                start_col: 5,
                end_col: 26,
            })
        );
        let s = snap("at crates/overseer/src/main.rs:1 now");
        let span = find_link_span_at(&s, 0, 5).unwrap();
        assert_eq!(
            span.link,
            Link::Path(PathBuf::from("crates/overseer/src/main.rs"), Some(1))
        );
        assert_eq!((span.start_col, span.end_col), (3, 32));
    }

    #[test]
    fn relative_and_file_url_links() {
        let s = snap("../up/a.rs:3 file:///tmp/x.txt");
        assert_eq!(
            find_link_at(&s, 0, 1),
            Some(Link::Path(PathBuf::from("../up/a.rs"), Some(3)))
        );
        assert_eq!(
            find_link_at(&s, 0, 15),
            Some(Link::Url("file:///tmp/x.txt".into()))
        );
        let s = snap("ftp://host/x -flag/y");
        assert_eq!(find_link_at(&s, 0, 2), None, "other schemes ignored");
        assert_eq!(find_link_at(&s, 0, 15), None, "must start like a name");
    }

    #[test]
    fn no_link_on_plain_word() {
        let s = snap("just some words / here");
        assert_eq!(find_link_at(&s, 0, 6), None);
        assert_eq!(find_link_at(&s, 0, 15), None, "lone slash is not a path");
        assert_eq!(find_link_at(&s, 0, 79), None);
        assert_eq!(find_link_at(&s, 9, 0), None);
    }
}
