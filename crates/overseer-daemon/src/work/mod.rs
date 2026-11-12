//! Work items from beads and Jira, grouped into one list.

pub mod group;
pub mod ownership;
pub mod sources;
pub mod store;

pub use group::{group, ticket_of};
pub use sources::{parse_beads, parse_jira, run_source, Bead, Ticket};
pub use store::{Source, WorkStore};

/// Compile a ticket pattern. A bad pattern is an error the caller can show.
pub fn compile_pattern(pattern: &str) -> Result<regex::Regex, String> {
    if pattern.trim().is_empty() {
        return Err("bad ticket_pattern: empty".into());
    }
    regex::Regex::new(&format!(r"\b(?:{pattern})\b")).map_err(|e| {
        let flat = e
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        format!("bad ticket_pattern: {flat}")
    })
}

#[cfg(test)]
mod tests {
    use super::compile_pattern;

    #[test]
    fn empty_pattern_is_rejected() {
        assert!(compile_pattern("").is_err());
        assert!(compile_pattern("  ").is_err());
    }

    #[test]
    fn bad_pattern_error_is_one_line() {
        let e = compile_pattern("[A-Z+\n(").unwrap_err();
        assert!(!e.contains('\n'), "{e}");
        assert!(e.starts_with("bad ticket_pattern"));
    }
}
