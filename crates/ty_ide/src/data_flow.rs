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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::cursor_test;
    use ruff_python_ast::name::Name;
    use ty_python_core::assumptions::{ClassName, Observed};

    /// What the analysis says about a file, given what a debugger saw where `<CURSOR>` is.
    ///
    /// The whole feature end to end: source in, findings out. Every other test of this reads one
    /// layer — which seeds survive, what an observation becomes — and none of them would notice if
    /// the layers stopped agreeing.
    ///
    /// `<CURSOR>` marks the line the program is stopped on, which is what the test is really
    /// about: everything below it is the question and everything above it has already run.
    fn at(source: &str, observations: Vec<(&str, Observed)>) -> Vec<String> {
        let test = cursor_test(source);
        let file = test.cursor.file;
        let text = ruff_db::source::source_text(&test.db, file);
        let line = text[..usize::from(test.cursor.offset)]
            .matches('\n')
            .count()
            + 1;
        let observed = observations
            .into_iter()
            .map(|(name, observed)| Observation {
                name: Name::new(name),
                observed,
            })
            .collect();

        data_flow_at(
            &test.db,
            test.program_file(file),
            OneIndexed::from_zero_indexed(line - 1),
            observed,
        )
        .into_iter()
        .map(|finding| format!("{}: {}", &text[finding.range], finding.label()))
        .collect()
    }

    #[test]
    fn a_condition_the_source_cannot_decide_is_decided_by_what_was_observed() {
        // `limit` comes from a call, so a checker reading this file alone can say only that it is
        // an `int` and nothing about which way the branch goes. the debugger saw a 5
        let found = at(
            "\
limit = compute()
<CURSOR>
if limit > 100:
    over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.iter().any(|f| f == "limit > 100: = false"),
            "the observation should settle the branch, and found {found:?}"
        );
    }

    #[test]
    fn without_the_observation_the_same_file_settles_nothing() {
        // the control for the test above. if this ever finds something, the feature is reporting
        // ordinary static analysis as though a debugger had produced it
        let found = at(
            "\
limit = compute()
<CURSOR>
if limit > 100:
    over = 1
",
            Vec::new(),
        );
        assert!(
            found.is_empty(),
            "nothing was observed, and found {found:?}"
        );
    }

    #[test]
    fn a_condition_above_the_stop_line_is_not_answered() {
        // it already ran, and it ran before the observation was taken. answering it would be
        // describing the wrong moment
        let found = at(
            "\
limit = compute()
if limit > 100:
    over = 1
<CURSOR>
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            !found.iter().any(|f| f.starts_with("limit > 100")),
            "a condition above the stop line was answered: {found:?}"
        );
    }

    #[test]
    fn a_name_a_loop_rebinds_around_the_stop_line_settles_nothing() {
        // `item` is bound above the stop line and rebound by the back edge, so what was observed
        // is true for this iteration and false for the next. this is the case the use-def map
        // cannot see, and the one that would put a confident wrong answer on screen
        let found = at(
            "\
for item in [1, 2, 3]:
    <CURSOR>
    if item > 2:
        big = 1
",
            vec![("item", Observed::IsInt("1".to_string()))],
        );
        assert!(found.is_empty(), "a loop-bound name was seeded: {found:?}");
    }

    #[test]
    fn a_binding_between_the_stop_and_the_use_wins_over_the_observation() {
        let found = at(
            "\
limit = compute()
<CURSOR>
limit = other()
if limit > 100:
    over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.is_empty(),
            "the program's own assignment happens in between: {found:?}"
        );
    }

    #[test]
    fn an_is_none_check_is_settled_by_what_the_debugger_saw() {
        let found = at(
            "\
value = lookup()
<CURSOR>
if value is None:
    missing = 1
",
            vec![("value", Observed::IsNone)],
        );
        assert!(
            found.iter().any(|f| f == "value is None: = true"),
            "found {found:?}"
        );
    }

    #[test]
    fn a_private_class_is_resolved_from_the_name_the_debugger_saw() {
        // basedpython renames a `private` declaration on the way out, so an instance of
        // `private class Runner` is a `_Runner` at runtime — a name the source does not have.
        // without the translation this observation resolves to nothing and the branch stays
        // undecided, which is a fact lost rather than a wrong answer, but lost all the same
        let found = at(
            "\
private class Runner:
    pass

thing = build()
<CURSOR>
if isinstance(thing, Runner):
    ran = 1
",
            vec![(
                "thing",
                Observed::IsExactly(ClassName {
                    module: "main".to_string(),
                    qualname: "_Runner".to_string(),
                }),
            )],
        );
        assert!(
            found
                .iter()
                .any(|f| f == "isinstance(thing, Runner): = true"),
            "found {found:?}"
        );
    }

    #[test]
    fn a_name_that_only_looks_renamed_is_not_invented() {
        // an underscore is not evidence of anything. `_Runner` with no `private Runner` behind it
        // is a class this file does not have, and guessing that it means `Runner` would be the
        // analysis making up a type from a naming convention
        let found = at(
            "\
class Runner:
    pass

thing = build()
<CURSOR>
if isinstance(thing, Runner):
    ran = 1
",
            vec![(
                "thing",
                Observed::IsExactly(ClassName {
                    module: "main".to_string(),
                    qualname: "_Runner".to_string(),
                }),
            )],
        );
        assert!(
            found.is_empty(),
            "an underscore was read as a rename: {found:?}"
        );
    }

    #[test]
    fn a_class_observation_settles_an_isinstance_check() {
        let found = at(
            "\
class Runner:
    pass

thing = build()
<CURSOR>
if isinstance(thing, Runner):
    ran = 1
",
            vec![(
                "thing",
                Observed::IsExactly(ClassName {
                    module: "main".to_string(),
                    qualname: "Runner".to_string(),
                }),
            )],
        );
        assert!(
            found
                .iter()
                .any(|f| f == "isinstance(thing, Runner): = true"),
            "found {found:?}"
        );
    }
}
