//! what `by run` says once the program it ran has ended
//!
//! an editor that runs a command for a user sums up what came of it with the last line the
//! command wrote: `OK`, `No changes detected`. `by run` writes lines of its own after the
//! program's, saying what it wrote back to the project and what it did not, and on stderr those
//! would be the last. `by run --notes PATH` writes them to `PATH` instead, one json object a
//! line, so that the output ends where the program's did and the notes are still there to show

use std::fmt;

/// how much a [`Note`] matters, least first
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// what was done, as asked
    Note,
    /// what was not done, and why
    Warning,
    /// what could not be done, which makes `by run` exit 1 for a program that exited 0
    Error,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        })
    }
}

/// one line of what `by run` says once the program has ended
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub level: Level,
    pub message: String,
}

impl Note {
    pub fn new(level: Level, message: impl Into<String>) -> Self {
        Self {
            level,
            message: message.into(),
        }
    }

    /// the notes `--notes` wrote, read back
    pub fn read_all(written: &str) -> serde_json::Result<Vec<Self>> {
        written
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str)
            .collect()
    }

    /// the line `--notes` writes for this note, without its line ending
    pub fn to_json_line(&self) -> serde_json::Result<String> {
        serde_json::to_string(self)
    }
}

/// as stderr shows it: `note: wrote back app/migrations/0001_initial.py`
impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.level, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::{Level, Note};

    #[test]
    fn notes_read_back_as_they_were_written() {
        let notes = [
            Note::new(Level::Note, "wrote back app/migrations/0001_initial.py"),
            Note::new(Level::Warning, "not written back: a \"quoted\"\nname"),
        ];
        let written: String = notes
            .iter()
            .map(|note| note.to_json_line().unwrap() + "\n")
            .collect();

        assert_eq!(Note::read_all(&written).unwrap(), notes);
        assert_eq!(
            written.lines().next(),
            Some(r#"{"level":"note","message":"wrote back app/migrations/0001_initial.py"}"#)
        );
        assert_eq!(
            notes[0].to_string(),
            "note: wrote back app/migrations/0001_initial.py"
        );
    }
}
