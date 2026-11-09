//! Detect clickable URLs and file paths in a snapshot row.

use std::path::PathBuf;

use crate::bridge::Snapshot;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Url(String),
    /// Path (`~` left unexpanded) with an optional 1-based line number.
    Path(PathBuf, Option<u32>),
}

/// Find a link under the cell at (`row`, `col`). Only the single row is
/// examined, so links wrapped across rows are not joined.
pub fn find_link_at(snap: &Snapshot, row: usize, col: usize) -> Option<Link> {
    if row >= snap.rows || col >= snap.cols {
        return None;
    }
    // Characters with the column each starts at. The spacer after a wide
    // character is skipped.
    let mut chars: Vec<(char, usize)> = Vec::with_capacity(snap.cols);
    let mut skip = false;
    for c in 0..snap.cols {
        if skip {
            skip = false;
            continue;
        }
        let cell = snap.cell(row, c)?;
        chars.push((cell.ch, c));
        skip = cell.wide;
    }
    let idx = chars.iter().rposition(|&(_, c)| c <= col)?;
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
    let token: String = chars[start..end].iter().map(|&(c, _)| c).collect();
    classify(&token)
}

fn classify(token: &str) -> Option<Link> {
    if (token.starts_with("http://") && token.len() > 7)
        || (token.starts_with("https://") && token.len() > 8)
    {
        return Some(Link::Url(token.to_string()));
    }
    if !(token.starts_with('/') || token.starts_with("~/") || token.starts_with("./")) {
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
    if path == "/" || path == "~/" || path == "./" {
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
    fn no_link_on_plain_word() {
        let s = snap("just some words / here");
        assert_eq!(find_link_at(&s, 0, 6), None);
        assert_eq!(find_link_at(&s, 0, 15), None, "lone slash is not a path");
        assert_eq!(find_link_at(&s, 0, 79), None);
        assert_eq!(find_link_at(&s, 9, 0), None);
    }
}
