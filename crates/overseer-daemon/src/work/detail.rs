//! Pure parsers for the owned item detail view: Jira issues (REST or acli),
//! Atlassian document format text, `bd show` output and the project overview.

use std::time::Duration;

use serde_json::Value;

/// How long a fetched detail stays fresh before a new open refetches it.
pub const DETAIL_TTL: Duration = Duration::from_secs(300);
/// How many trailing lines of a bead's notes the detail keeps.
pub const NOTES_LINES: usize = 10;
/// The longest overview paragraph, in characters.
pub const OVERVIEW_CAP: usize = 600;

/// The text of an inline node: its own text, or its children's.
fn inline(node: &Value) -> String {
    let attr = |k: &str| {
        node.get("attrs")
            .and_then(|a| a.get(k))
            .and_then(Value::as_str)
    };
    match node.get("type").and_then(Value::as_str) {
        Some("text") => node
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        Some("hardBreak") => "\n".to_owned(),
        Some("mention" | "emoji" | "inlineCard") => {
            attr("text").or(attr("url")).unwrap_or_default().to_owned()
        }
        _ => children(node).iter().map(inline).collect(),
    }
}

fn children(node: &Value) -> &[Value] {
    node.get("content")
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

fn block(node: &Value, depth: usize, out: &mut String) {
    match node.get("type").and_then(Value::as_str) {
        Some("heading") => {
            let level = node
                .get("attrs")
                .and_then(|a| a.get("level"))
                .and_then(Value::as_u64)
                .unwrap_or(1)
                .clamp(1, 6) as usize;
            out.push_str(&format!("{} {}\n", "#".repeat(level), inline(node)));
        }
        Some("paragraph") => {
            out.push_str(&inline(node));
            out.push_str("\n\n");
        }
        Some(kind @ ("bulletList" | "orderedList")) => {
            for (i, item) in children(node).iter().enumerate() {
                for (j, child) in children(item).iter().enumerate() {
                    if j == 0 && child.get("type").and_then(Value::as_str) == Some("paragraph") {
                        out.push_str(&"  ".repeat(depth));
                        if kind == "bulletList" {
                            out.push_str("- ");
                        } else {
                            out.push_str(&format!("{}. ", i + 1));
                        }
                        out.push_str(&inline(child));
                        out.push('\n');
                    } else {
                        block(child, depth + 1, out);
                    }
                }
            }
        }
        Some("codeBlock") => out.push_str(&format!("```\n{}\n```\n", inline(node))),
        Some("text" | "hardBreak" | "mention" | "emoji" | "inlineCard") => {
            out.push_str(&inline(node));
        }
        _ => {
            for child in children(node) {
                block(child, depth, out);
            }
        }
    }
}

/// Plain text for an Atlassian document (or a bare string).
pub fn adf_to_text(v: &Value) -> String {
    if let Some(s) = v.as_str() {
        return s.to_owned();
    }
    let mut out = String::new();
    block(v, 0, &mut out);
    out.trim_end_matches(['\n', ' ']).to_owned()
}

/// What the detail view shows of a Jira issue.
#[derive(Debug, Clone, PartialEq)]
pub struct IssueDetail {
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub body: Option<String>,
}

/// Whether `key` is safe to pass to `acli`, `bd` or a URL path.
pub fn valid_item_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The REST-style message for an acli failure that means the issue is gone.
pub fn acli_not_found(key: &str, err: &str) -> Option<String> {
    err.contains("Issue does not exist")
        .then(|| format!("{key} was not found in Jira"))
}

/// The first JSON value in `out`, starting at the first of `starts`.
fn json_from(out: &str, what: &str, starts: &[char]) -> Result<Value, String> {
    let start = out
        .find(starts)
        .ok_or_else(|| format!("{what} output: no JSON found"))?;
    serde_json::Deserializer::from_str(&out[start..])
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| format!("{what} output: no JSON found"))?
        .map_err(|e| format!("{what} output: {e}"))
}

/// A name held as `{name}` or as a plain string.
fn named(v: Option<&Value>) -> Option<String> {
    let v = v?;
    v.get("name")
        .and_then(Value::as_str)
        .or_else(|| v.as_str())
        .map(str::to_owned)
}

/// Reads a Jira issue from the REST shape (fields under `fields`) or the flat
/// shape acli prints, skipping any text before the first JSON.
pub fn parse_issue_detail(out: &str) -> Result<IssueDetail, String> {
    let v = json_from(out, "jira", &['{'])?;
    let f = v.get("fields").unwrap_or(&v);
    let title = f
        .get("summary")
        .and_then(Value::as_str)
        .ok_or("jira output: no summary")?
        .to_owned();
    let body = f
        .get("description")
        .map(adf_to_text)
        .filter(|s| !s.trim().is_empty());
    Ok(IssueDetail {
        title,
        status: named(f.get("status")).unwrap_or_default(),
        priority: named(f.get("priority")),
        body,
    })
}

/// What the detail view shows of a bead.
#[derive(Debug, Clone, PartialEq)]
pub struct BeadDetail {
    pub body: Option<String>,
    pub notes: Option<String>,
    pub depends_on: Vec<String>,
}

fn non_empty(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// Reads `bd show --json` output: an object, or an array holding it first.
pub fn parse_bead_detail(out: &str) -> Result<BeadDetail, String> {
    let v = json_from(out, "bd", &['[', '{'])?;
    let obj = match &v {
        Value::Array(a) => a.first().ok_or("bd output: empty list")?,
        other => other,
    };
    if !obj.is_object() {
        return Err("bd output: unexpected JSON".into());
    }
    let notes = non_empty(obj.get("notes")).map(|n| {
        let lines: Vec<&str> = n.lines().collect();
        lines[lines.len().saturating_sub(NOTES_LINES)..].join("\n")
    });
    let depends_on = obj
        .get("dependencies")
        .and_then(Value::as_array)
        .map(|deps| {
            deps.iter()
                .filter_map(|d| {
                    if let Some(id) = d.as_str() {
                        return Some(id.to_owned());
                    }
                    let id = non_empty(d.get("depends_on_id")).or(non_empty(d.get("id")))?;
                    Some(
                        match non_empty(d.get("dependency_type")).or(non_empty(d.get("type"))) {
                            Some(t) => format!("{id} ({t})"),
                            None => id.to_owned(),
                        },
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(BeadDetail {
        body: non_empty(obj.get("description")).map(str::to_owned),
        notes,
        depends_on,
    })
}

/// The first paragraph of a project overview, after any front matter and the
/// first heading, cut to `OVERVIEW_CAP` characters.
pub fn overview_paragraph(md: &str) -> Option<String> {
    let mut lines = md.lines().peekable();
    if lines.peek().is_some_and(|l| l.trim() == "---") {
        lines.next();
        for l in lines.by_ref() {
            if l.trim() == "---" {
                break;
            }
        }
    }
    let mut lines = lines.skip_while(|l| l.trim().is_empty()).peekable();
    if lines.peek().is_some_and(|l| l.starts_with('#')) {
        lines.next();
    }
    let para: Vec<&str> = lines
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.trim().is_empty())
        .collect();
    if para.is_empty() {
        return None;
    }
    let text = para.join("\n");
    if text.chars().count() <= OVERVIEW_CAP {
        return Some(text);
    }
    let cut: String = text.chars().take(OVERVIEW_CAP - 3).collect();
    Some(format!("{}...", cut.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const JIRA: &str = include_str!("../../tests/fixtures/jira-issue.json");
    const BD: &str = include_str!("../../tests/fixtures/bd-show.json");
    const ACLI_VIEW: &str = include_str!("../../tests/fixtures/acli-view.json");

    #[test]
    fn adf_renders_headings_lists_and_code() {
        let v: Value = serde_json::from_str(JIRA).unwrap();
        let text = adf_to_text(&v["fields"]["description"]);
        assert_eq!(
            text,
            "## Goal\nShow the detail view.\n\n- First\n  - Nested\n- Second\n```\ncargo test\n```"
        );
    }

    #[test]
    fn adf_odd_nodes_fall_back_to_text() {
        let doc = json!({"type":"doc","content":[
            {"type":"panel","content":[{"type":"paragraph","content":[
                {"type":"text","text":"Heads up"},
                {"type":"hardBreak"},
                {"type":"mention","attrs":{"text":"@Demo"}}]}]}]});
        let text = adf_to_text(&doc);
        assert_eq!(text, "Heads up\n@Demo");
        let d = parse_issue_detail(r#"{"fields":{"summary":"S","description":null}}"#).unwrap();
        assert_eq!(d.body, None);
        assert_eq!(d.title, "S");
    }

    #[test]
    fn issue_detail_reads_rest_and_flat_shapes() {
        let d = parse_issue_detail(JIRA).unwrap();
        assert_eq!(d.title, "Wire the detail view");
        assert_eq!(d.status, "In Progress");
        assert_eq!(d.priority.as_deref(), Some("High"));
        assert!(d.body.unwrap().starts_with("## Goal"));

        let flat = r#"{"key":"DEMO-2","summary":"T","status":"To Do","description":"plain"}"#;
        let d = parse_issue_detail(flat).unwrap();
        assert_eq!(d.status, "To Do");
        assert_eq!(d.body.as_deref(), Some("plain"));

        let d = parse_issue_detail(&format!("Loading...\n{flat}")).unwrap();
        assert_eq!(d.title, "T");

        // A bracketed log prefix is not the start of the JSON.
        let d = parse_issue_detail(&format!("[INFO] fetching DEMO-2\n{flat}")).unwrap();
        assert_eq!(d.title, "T");

        assert!(parse_issue_detail("not json").is_err());
    }

    #[test]
    fn bead_detail_takes_notes_tail_and_deps() {
        let d = parse_bead_detail(BD).unwrap();
        let notes = d.notes.unwrap();
        assert_eq!(notes.lines().count(), NOTES_LINES);
        assert_eq!(notes.lines().next(), Some("note line 3"));
        assert_eq!(notes.lines().last(), Some("note line 12"));
        assert_eq!(d.depends_on, vec!["demo-2 (blocks)".to_string()]);
        assert!(d.body.unwrap().starts_with("Show the item body."));

        let obj: Value = serde_json::from_str(BD).unwrap();
        let wrapped = Value::Array(vec![obj[0].clone()]).to_string();
        assert_eq!(
            parse_bead_detail(&wrapped).unwrap(),
            parse_bead_detail(&obj[0].to_string()).unwrap()
        );
    }

    fn doc(content: Value) -> Value {
        json!({"type":"doc","version":1,"content":content})
    }
    fn para(t: &str) -> Value {
        json!({"type":"paragraph","content":[{"type":"text","text":t}]})
    }
    fn item(content: Value) -> Value {
        json!({"type":"listItem","content":content})
    }

    #[test]
    fn acli_view_fixture_reads_like_jira() {
        let d = parse_issue_detail(ACLI_VIEW).unwrap();
        assert_eq!(d.title, "Rotate the demo widget keys");
        assert_eq!(d.status, "In Progress");
        assert_eq!(d.priority.as_deref(), Some("Medium"));
        assert_eq!(
            d.body.as_deref(),
            Some("Rotate the keys for demo-ws before Friday. See https://demo.atlassian.net/browse/DEMO-8\n\n### Acceptance criteria\n- Old keys stop working\n- New keys are in the vault\n1. Make the keys\n2. Ship them\nthen check\n```\ndemo rotate --all\n```")
        );
    }

    #[test]
    fn ordered_lists_number_and_nest() {
        let d = doc(json!([{"type":"orderedList","content":[
            item(json!([para("a")])), item(json!([para("b")])), item(json!([para("c")]))]}]));
        assert_eq!(adf_to_text(&d), "1. a\n2. b\n3. c");
        let d = doc(json!([{"type":"bulletList","content":[item(json!([
            para("x"),
            {"type":"orderedList","content":[item(json!([para("y")]))]}]))]}]));
        assert_eq!(adf_to_text(&d), "- x\n  1. y");
    }

    #[test]
    fn deep_headings_keep_their_level() {
        let h = |l: Value, t: &str| {
            let attrs = if l.is_null() {
                json!({})
            } else {
                json!({"level": l})
            };
            json!({"type":"heading","attrs":attrs,"content":[{"type":"text","text":t}]})
        };
        let d = doc(json!([
            h(json!(4), "Four"),
            h(json!(5), "Five"),
            h(json!(6), "Six"),
            h(json!(9), "Nine"),
            h(Value::Null, "None")
        ]));
        assert_eq!(
            adf_to_text(&d),
            "#### Four\n##### Five\n###### Six\n###### Nine\n# None"
        );
    }

    #[test]
    fn a_list_item_without_a_paragraph_has_no_lone_marker() {
        let d = doc(json!([{"type":"bulletList","content":[item(json!([
            {"type":"bulletList","content":[item(json!([para("inner")]))]}]))]}]));
        assert_eq!(adf_to_text(&d), "  - inner");
        let d = doc(json!([{"type":"bulletList","content":[
            item(json!([])), item(json!([para("kept")]))]}]));
        assert_eq!(adf_to_text(&d), "- kept");
    }

    #[test]
    fn bead_dependencies_in_every_shape() {
        assert_eq!(
            parse_bead_detail(BD).unwrap().depends_on,
            vec!["demo-2 (blocks)".to_string()]
        );
        let out = r#"{"dependencies": ["demo-3", {"id": "demo-4"}, {"depends_on_id": "demo-5", "type": "related"}]}"#;
        assert_eq!(
            parse_bead_detail(out).unwrap().depends_on,
            vec!["demo-3", "demo-4", "demo-5 (related)"]
        );
    }

    #[test]
    fn acli_not_found_maps_to_the_rest_wording() {
        let err = "acli exited with 1: \u{2717} Error: Issue does not exist or you do not have permission to see it.";
        assert_eq!(
            acli_not_found("DEMO-1", err).as_deref(),
            Some("DEMO-1 was not found in Jira")
        );
        assert_eq!(
            acli_not_found("DEMO-1", "acli exited with 1: timeout"),
            None
        );
    }

    #[test]
    fn item_keys_are_checked() {
        for ok in ["DEMO-1", "demo-12", "ov-3.1", "a_b"] {
            assert!(valid_item_key(ok), "{ok}");
        }
        for bad in [
            "",
            "-DEMO",
            ".x",
            "DEMO 1",
            "DEMO-1;rm",
            "DEMO/1",
            "\u{c9}DEMO",
            "DEMO-1\n",
        ] {
            assert!(!valid_item_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn overview_skips_front_matter_and_heading() {
        let md = "---\ntitle: x\n---\n# DEMO-1\n\nFirst para\nline two.\n\nSecond.";
        assert_eq!(
            overview_paragraph(md).as_deref(),
            Some("First para\nline two.")
        );

        let long = format!("# T\n\n{}", "é".repeat(2000));
        let cut = overview_paragraph(&long).unwrap();
        assert!(cut.chars().count() <= OVERVIEW_CAP);
        assert!(cut.ends_with("..."));

        assert_eq!(overview_paragraph(""), None);
    }
}
