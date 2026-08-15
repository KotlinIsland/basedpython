//! What a stopped program's own state says about the code it has not run yet.
//!
//! An editor with a debugger attached knows something no checker does: what the names in a frame
//! actually hold. This is where that gets spent — the branches below the stop line, answered as
//! the definite `true` or `false` they will be rather than as the "could go either way" the source
//! alone can support.
//!
//! The analysis is not a second one. It is the checker's own reachability machinery, reading the
//! same file under a program that pins some names to what was observed — see
//! [`ty_python_core::assumptions`] and `ty_python_semantic::assumed`.

use ruff_source_file::OneIndexed;
use ruff_text_size::TextRange;
use ty_python_core::assumptions::{Assumptions, Observation};
use ty_python_core::{ProgramFile, Truthiness};
use ty_python_semantic::types::ide_support::{UnreachableRange, data_flow};

use crate::Db;

/// One thing the runtime state settles about code that has not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The source it is about.
    pub range: TextRange,
    /// What is settled.
    pub kind: FindingKind,
}

/// What kind of thing was settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// This condition will go this way.
    Condition {
        /// Which way.
        taken: bool,
    },
    /// This code will not run.
    Unreachable,
}

impl Finding {
    /// What to show a reader beside the source.
    ///
    /// Short on purpose: it is drawn inline, in the editor font, beside code somebody is reading
    /// while stopped in a debugger.
    pub fn label(&self) -> &'static str {
        match self.kind {
            FindingKind::Condition { taken: true } => "= true",
            FindingKind::Condition { taken: false } => "= false",
            FindingKind::Unreachable => "will not run",
        }
    }
}

/// What the program's own state decides about the code below `line`.
///
/// `line` is one-based, and is the line the program is stopped on. Everything answered is strictly
/// below it: the statement on the stop line has not finished, so it is not something the state
/// "predicts".
///
/// An empty answer is the ordinary case and is not a failure. Most conditions depend on something
/// the debugger could not observe — a call's result, an object with a `__bool__` of its own — and
/// the honest answer for those is nothing at all.
pub fn data_flow_at(
    db: &dyn Db,
    file: ProgramFile<'_>,
    line: OneIndexed,
    observations: Vec<Observation>,
) -> Vec<Finding> {
    let source_file = file.file(db);
    if !db.should_check_file(source_file) {
        return Vec::new();
    }

    let source = ruff_db::source::source_text(db, source_file);
    let below = ruff_db::source::line_index(db, source_file).line_start(line, &source);

    let assumptions = Assumptions::new(db, line.get() as u32, observations.into_boxed_slice());
    let seeded = file.program(db).seeded(db, assumptions);
    let seeded_file = ProgramFile::new(db, source_file, seeded);

    let flow = data_flow(db, seeded_file, file, below);

    let conditions = flow.conditions.iter().map(|condition| Finding {
        range: condition.range,
        kind: FindingKind::Condition {
            // `Ambiguous` never reaches here — `data_flow` drops it rather than reporting a
            // verdict that is not one
            taken: condition.verdict == Truthiness::AlwaysTrue,
        },
    });

    let unreachable = flow
        .unreachable
        .iter()
        .map(|range: &UnreachableRange| Finding {
            range: range.range,
            kind: FindingKind::Unreachable,
        });

    conditions.chain(unreachable).collect()
}
