//! Which board stage a Jira status belongs to, from the `stage_*` settings.

use overseer_proto::Stage;

use crate::config::Config;

pub struct Stages {
    /// Lowercased, trimmed status names per stage.
    lists: Vec<(Stage, Vec<String>)>,
}

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

impl Stages {
    pub fn from_config(cfg: &Config) -> Stages {
        let of = |v: &[String]| v.iter().map(|s| norm(s)).collect::<Vec<_>>();
        Stages {
            lists: vec![
                (Stage::ToDo, of(&cfg.stage_todo)),
                (Stage::Blocked, of(&cfg.stage_blocked)),
                (Stage::Next, of(&cfg.stage_next)),
                (Stage::InProgress, of(&cfg.stage_in_progress)),
                (Stage::ToBeTested, of(&cfg.stage_to_be_tested)),
                (Stage::Done, of(&cfg.stage_done)),
            ],
        }
    }

    /// The stage whose list names `status`; an unmapped status is In progress.
    pub fn stage_of(&self, status: &str) -> Stage {
        let status = norm(status);
        self.lists
            .iter()
            .find(|(_, names)| names.contains(&status))
            .map_or(Stage::InProgress, |(stage, _)| *stage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_stages() {
        let s = Stages::from_config(&Config::default());
        for (status, want) in [
            ("Prioritised Backlog", Stage::ToDo),
            ("Next", Stage::Next),
            ("Selected for Development", Stage::Next),
            ("in progress", Stage::InProgress),
            (" Code Review ", Stage::InProgress),
            ("To Be Tested", Stage::ToBeTested),
            ("Blocked", Stage::Blocked),
            ("RESOLVED", Stage::Done),
            ("Ready for Release", Stage::InProgress),
            ("", Stage::InProgress),
        ] {
            assert_eq!(s.stage_of(status), want, "{status}");
        }
    }
}
