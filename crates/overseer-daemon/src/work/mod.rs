//! Work items from beads and Jira, grouped into one list.

pub mod group;
pub mod sources;

pub use group::{group, ticket_of};
pub use sources::{parse_beads, parse_jira, run_source, Bead, Ticket};

/// Compile a ticket pattern. A bad pattern is an error the caller can show.
pub fn compile_pattern(pattern: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(&format!(r"\b(?:{pattern})\b"))
        .map_err(|e| format!("bad ticket_pattern: {e}"))
}
