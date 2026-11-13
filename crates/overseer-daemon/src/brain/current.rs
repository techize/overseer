//! Per-slot sections at the top of CURRENT_SESSION.md (Ruling 14).

use overseer_proto::msg::SlotId;

const OPEN: &str = "<!-- overseer:sections -->\n";
const CLOSE: &str = "<!-- /overseer:sections -->\n";

enum Part {
    Block(SlotId, String),
    Raw(String),
}

/// Splits the file into the parts of the region at its top and everything
/// after. A region that is not well formed (no close marker, an unterminated
/// or duplicate block) is not touched: it counts as part of the rest.
fn parse(file: &str) -> (Vec<Part>, &str) {
    let Some(inner) = file.strip_prefix(OPEN) else {
        return (Vec::new(), file);
    };
    // The close marker counts only as a whole line.
    let end = if inner.starts_with(CLOSE) {
        Some(0)
    } else {
        inner.find(&format!("\n{CLOSE}")).map(|i| i + 1)
    };
    let Some(end) = end else {
        return (Vec::new(), file);
    };
    let (region, rest) = (&inner[..end], &inner[end + CLOSE.len()..]);
    let mut parts: Vec<Part> = Vec::new();
    let mut cur: Option<(SlotId, String)> = None;
    for line in region.split_inclusive('\n') {
        let t = line.trim_end();
        if let Some((slot, buf)) = cur.as_mut() {
            buf.push_str(line);
            if t == format!("<!-- /overseer:S{slot} -->") {
                parts.push(Part::Block(*slot, std::mem::take(buf)));
                cur = None;
            }
            continue;
        }
        let start = t
            .strip_prefix("<!-- overseer:S")
            .and_then(|s| s.strip_suffix(" -->"))
            .and_then(|s| s.parse::<SlotId>().ok());
        if let Some(n) = start {
            if parts
                .iter()
                .any(|p| matches!(p, Part::Block(s, _) if *s == n))
            {
                return (Vec::new(), file);
            }
            cur = Some((n, line.to_string()));
        } else if let Some(Part::Raw(r)) = parts.last_mut() {
            r.push_str(line);
        } else {
            parts.push(Part::Raw(line.to_string()));
        }
    }
    if cur.is_some() {
        return (Vec::new(), file);
    }
    (parts, rest)
}

fn build(parts: &[Part], rest: &str) -> String {
    if parts.is_empty() {
        return rest.to_string();
    }
    let mut out = String::from(OPEN);
    for p in parts {
        match p {
            Part::Block(_, t) | Part::Raw(t) => out.push_str(t),
        }
    }
    out.push_str(CLOSE);
    out.push_str(rest);
    out
}

/// Keeps a slot's text from forging a marker: a line that starts with
/// `<!-- overseer:` or `<!-- /overseer:` gets a backslash in front.
fn neutralise(text: &str) -> String {
    text.split('\n')
        .map(|l| {
            if l.starts_with("<!-- overseer:") || l.starts_with("<!-- /overseer:") {
                format!("\\{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Writes or replaces slot `slot`'s block; the rest of the file is untouched.
pub fn set_section(file: &str, slot: SlotId, label: &str, text: &str) -> String {
    let (mut parts, rest) = parse(file);
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let heading = if label.is_empty() {
        format!("## S{slot}")
    } else {
        format!("## S{slot} {label}")
    };
    let text = neutralise(text.trim_matches('\n'));
    let block = Part::Block(
        slot,
        format!("<!-- overseer:S{slot} -->\n{heading}\n{text}\n<!-- /overseer:S{slot} -->\n"),
    );
    if let Some(i) = parts
        .iter()
        .position(|p| matches!(p, Part::Block(s, _) if *s == slot))
    {
        parts[i] = block;
    } else {
        let at = parts
            .iter()
            .position(|p| matches!(p, Part::Block(s, _) if *s > slot))
            .unwrap_or(parts.len());
        parts.insert(at, block);
    }
    build(&parts, rest)
}

/// Removes slot `slot`'s block; the markers go when nothing is left in them.
pub fn clear_section(file: &str, slot: SlotId) -> String {
    let (mut parts, rest) = parse(file);
    let Some(i) = parts
        .iter()
        .position(|p| matches!(p, Part::Block(s, _) if *s == slot))
    else {
        return file.to_string();
    };
    parts.remove(i);
    build(&parts, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history() -> String {
        (0..2500).map(|i| format!("old line {i}\n")).collect()
    }

    #[test]
    fn sections_sit_above_the_history() {
        let h = history();
        let a = set_section(&h, 3, "demo-api", "working on DEMO-12");
        let b = set_section(&a, 1, "", "idle");
        assert!(b.starts_with("<!-- overseer:sections -->\n<!-- overseer:S1 -->\n## S1\nidle\n"));
        assert!(b.find("overseer:S1 -->").unwrap() < b.find("overseer:S3 -->").unwrap());
        assert!(b.contains("## S3 demo-api\nworking on DEMO-12\n<!-- /overseer:S3 -->\n"));
        assert!(b.ends_with(&h));
        assert_eq!(b.lines().count(), 2500 + 1 + 4 + 4 + 1);
    }

    #[test]
    fn second_write_replaces_only_that_section() {
        let h = history();
        let a = set_section(&set_section(&h, 3, "a", "one"), 1, "b", "keep");
        let c = set_section(&a, 3, "a2", "two");
        assert!(c.contains("## S3 a2\ntwo\n"));
        assert!(!c.contains("one"));
        assert!(c.contains("## S1 b\nkeep\n"));
        assert!(c.ends_with(&h));
        assert_eq!(c.matches("<!-- overseer:S3 -->").count(), 1);
    }

    #[test]
    fn clear_section_removes_the_block() {
        let h = history();
        let a = set_section(&set_section(&h, 3, "a", "one"), 1, "b", "keep");
        let b = clear_section(&a, 3);
        assert!(!b.contains("S3"));
        assert!(b.contains("## S1 b\nkeep\n"));
        let c = clear_section(&b, 1);
        assert_eq!(c, h);
        assert_eq!(clear_section(&h, 4), h);
    }

    #[test]
    fn stray_text_in_the_region_survives() {
        let a = set_section("body\n", 3, "a", "one");
        let a = a.replace(
            "<!-- /overseer:sections -->",
            "Jonathan's note\n<!-- /overseer:sections -->",
        );
        let b = set_section(&a, 1, "b", "two");
        assert!(b.contains("Jonathan's note\n<!-- /overseer:sections -->"));
        assert!(b.find("S1 -->").unwrap() < b.find("S3 -->").unwrap());
        let c = clear_section(&b, 3);
        assert!(c.contains("Jonathan's note"));
        assert!(c.ends_with("body\n"));
    }

    #[test]
    fn a_missing_close_marker_loses_nothing() {
        let a = set_section(&set_section("body\n", 3, "a", "one"), 1, "b", "two");
        let broken = a.replace("<!-- /overseer:S1 -->\n", "");
        let b = set_section(&broken, 5, "c", "three");
        assert!(b.ends_with(&broken));
        assert!(b.starts_with("<!-- overseer:sections -->\n<!-- overseer:S5 -->\n## S5 c\nthree\n"));
        assert_eq!(clear_section(&broken, 3), broken);
        let dup = format!("{OPEN}<!-- overseer:S2 -->\nx\n<!-- /overseer:S2 -->\n<!-- overseer:S2 -->\ny\n<!-- /overseer:S2 -->\n{CLOSE}z\n");
        assert!(set_section(&dup, 4, "", "t").ends_with(&dup));
        let open_only = format!("{OPEN}text\n");
        assert!(set_section(&open_only, 4, "", "t").ends_with(&open_only));
    }

    #[test]
    fn forged_markers_in_text_are_neutralised() {
        let t = "ok\n<!-- /overseer:S3 -->\n<!-- /overseer:sections -->\n<!-- overseer:S9 -->";
        let a = set_section("body\n", 3, "a", t);
        let whole = |m: &str| a.lines().filter(|l| *l == m).count();
        assert_eq!(whole("<!-- /overseer:S3 -->"), 1);
        assert_eq!(whole("<!-- /overseer:sections -->"), 1);
        assert!(a.contains("\\<!-- /overseer:S3 -->"));
        let b = set_section(&a, 1, "", "x");
        assert!(b.ends_with("body\n"));
        assert_eq!(clear_section(&clear_section(&b, 3), 1), "body\n");
    }
}
