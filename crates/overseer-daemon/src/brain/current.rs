//! Per-slot sections at the top of CURRENT_SESSION.md (Ruling 14).

use std::collections::BTreeMap;

use overseer_proto::msg::SlotId;

const OPEN: &str = "<!-- overseer:sections -->\n";
const CLOSE: &str = "<!-- /overseer:sections -->\n";

/// Splits the file into the slot blocks at its top and everything after.
fn parse(file: &str) -> (BTreeMap<SlotId, String>, &str) {
    let mut blocks = BTreeMap::new();
    let Some(inner) = file.strip_prefix(OPEN) else {
        return (blocks, file);
    };
    let Some(end) = inner.find(CLOSE) else {
        return (blocks, file);
    };
    let (region, rest) = (&inner[..end], &inner[end + CLOSE.len()..]);
    let mut cur: Option<(SlotId, String)> = None;
    for line in region.split_inclusive('\n') {
        let t = line.trim_end();
        if let Some((slot, buf)) = cur.as_mut() {
            buf.push_str(line);
            if t == format!("<!-- /overseer:S{slot} -->") {
                blocks.insert(*slot, std::mem::take(buf));
                cur = None;
            }
        } else if let Some(n) = t
            .strip_prefix("<!-- overseer:S")
            .and_then(|s| s.strip_suffix(" -->"))
            .and_then(|s| s.parse::<SlotId>().ok())
        {
            cur = Some((n, line.to_string()));
        }
    }
    (blocks, rest)
}

fn build(blocks: &BTreeMap<SlotId, String>, rest: &str) -> String {
    if blocks.is_empty() {
        return rest.to_string();
    }
    let mut out = String::from(OPEN);
    for b in blocks.values() {
        out.push_str(b);
    }
    out.push_str(CLOSE);
    out.push_str(rest);
    out
}

/// Writes or replaces slot `slot`'s block; the rest of the file is untouched.
pub fn set_section(file: &str, slot: SlotId, label: &str, text: &str) -> String {
    let (mut blocks, rest) = parse(file);
    let label = label.split_whitespace().collect::<Vec<_>>().join(" ");
    let heading = if label.is_empty() {
        format!("## S{slot}")
    } else {
        format!("## S{slot} {label}")
    };
    let text = text.trim_matches('\n');
    blocks.insert(
        slot,
        format!("<!-- overseer:S{slot} -->\n{heading}\n{text}\n<!-- /overseer:S{slot} -->\n"),
    );
    build(&blocks, rest)
}

/// Removes slot `slot`'s block; the markers go when no block is left.
pub fn clear_section(file: &str, slot: SlotId) -> String {
    let (mut blocks, rest) = parse(file);
    if blocks.remove(&slot).is_none() {
        return file.to_string();
    }
    build(&blocks, rest)
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
}
