//! what a stopped program's own state says about the code it has not run yet
//!
//! an editor with a debugger attached knows something no checker does: what the names in a frame
//! actually hold. this is where that gets spent — the branches below the stop line, answered as
//! the definite `true` or `false` they will be rather than as the "could go either way" the source
//! alone can support
//!
//! the analysis is not a second one. it is the checker's own reachability machinery, reading the
//! same file under a program that pins some names to what was observed — see
//! [`ty_python_core::assumptions`] and `ty_python_semantic::assumed`

use ruff_source_file::OneIndexed;
use ruff_text_size::TextRange;
use ty_python_core::assumptions::{Assumptions, Observation};
use ty_python_core::{ProgramFile, Truthiness};
use ty_python_semantic::types::ide_support::{UnreachableRange, data_flow};

use crate::Db;

/// one thing the runtime state settles about code that has not run
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// the source it is about
    pub range: TextRange,
    /// what is settled
    pub kind: FindingKind,
}

/// what kind of thing was settled
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingKind {
    /// this condition will go this way
    Condition {
        /// which way
        taken: bool,
    },
    /// this code will not run
    Unreachable,
}

impl Finding {
    /// what to show a reader beside the source
    ///
    /// short on purpose: it is drawn inline, in the editor font, beside code somebody is reading
    /// while stopped in a debugger
    pub fn label(&self) -> &'static str {
        match self.kind {
            FindingKind::Condition { taken: true } => "= true",
            FindingKind::Condition { taken: false } => "= false",
            FindingKind::Unreachable => "will not run",
        }
    }
}

/// what the program's own state decides about the code at and below `line`
///
/// `line` is one-based, and is the line the program is stopped on. that line is itself answered,
/// because nothing on it has run yet: a condition written there is still ahead of the program, and
/// a binding written there has not taken effect. everything above it already ran, and ran before
/// the observation was taken, so none of it is answered
///
/// an empty answer is the ordinary case and is not a failure. most conditions depend on something
/// the debugger could not observe — a call's result, an object with a `__bool__` of its own — and
/// the honest answer for those is nothing at all
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

    // the assumptions hold the line as a `u32` to keep the interned value small. a file with more
    // lines than that is not one a debugger stopped in, so there is nothing to answer about it
    let Ok(stop_line) = u32::try_from(line.get()) else {
        return Vec::new();
    };

    let source = ruff_db::source::source_text(db, source_file);
    let below = ruff_db::source::line_index(db, source_file).line_start(line, &source);

    let assumptions = Assumptions::new(db, source_file, stop_line, observations.into_boxed_slice());
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

    /// what the analysis says about a file, given what a debugger saw where `<CURSOR>` is
    ///
    /// the whole feature end to end: source in, findings out. every other test of this reads one
    /// layer — which seeds survive, what an observation becomes — and none of them would notice if
    /// the layers stopped agreeing
    ///
    /// `<CURSOR>` marks the line the program is stopped on, which is what the test is really
    /// about: everything below it is the question and everything above it has already run
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
    fn a_function_below_the_stop_line_is_not_seeded_from_the_frame_above_it() {
        // `f` has not been called and will not be until after `limit = other()`, so what the
        // module-level `limit` held at the stop line says nothing about what `f` will read. the
        // module scope refuses this seed on its own; the point of the test is that `f`'s scope,
        // which binds no `limit` at all and so has no binding to refuse it with, refuses it too
        let found = at(
            "\
limit = compute()
<CURSOR>
limit = other()
def f():
    if limit > 100:
        over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.is_empty(),
            "a global rebound below the stop line was seeded inside a function: {found:?}"
        );
    }

    #[test]
    fn an_observation_of_one_frames_local_does_not_reach_another_scopes_name() {
        // the debugger read `caller`'s local `limit`. `helper` has a `limit` too, and it is a
        // different name that only happens to be spelled the same — reading the first as the
        // second is how a confident wrong answer gets on screen
        let found = at(
            "\
def caller():
    limit = 5
    <CURSOR>
    helper()

def helper():
    if limit > 100:
        over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.is_empty(),
            "an observation crossed into a scope it was never about: {found:?}"
        );
    }

    #[test]
    fn a_seed_reaches_the_rest_of_the_function_it_was_observed_in() {
        // the other side of the two tests above: within the one scope the program is stopped in,
        // a seed is exactly as useful as it is at module level
        let found = at(
            "\
def run():
    limit = compute()
    <CURSOR>
    if limit > 100:
        over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.iter().any(|f| f == "limit > 100: = false"),
            "found {found:?}"
        );
    }

    #[test]
    fn an_attribute_the_stopped_method_assigns_is_seeded() {
        // a dotted path is a place like any other, so an observation of one carries as far as the
        // scope that assigns it — and no further, because a scope that only reads `self.limit` has
        // no binding of it to say what could have happened to it in between
        let found = at(
            "\
class Runner:
    def go(self):
        self.limit = compute()
        <CURSOR>
        if self.limit > 100:
            over = 1
",
            vec![("self.limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.iter().any(|f| f == "self.limit > 100: = false"),
            "found {found:?}"
        );
    }

    #[test]
    fn an_attribute_the_stopped_method_only_reads_is_refused() {
        let found = at(
            "\
class Runner:
    def go(self):
        <CURSOR>
        if self.limit > 100:
            over = 1
",
            vec![("self.limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.is_empty(),
            "an attribute this scope never assigns has no binding to vouch for it: {found:?}"
        );
    }

    #[test]
    fn a_condition_on_the_stop_line_is_answered() {
        // the program is stopped *before* running this line, so the condition on it is still
        // ahead of the program and the observation describes the moment it will be read
        let found = at(
            "\
limit = compute()
<CURSOR>if limit > 100:
    over = 1
",
            vec![("limit", Observed::IsInt("5".to_string()))],
        );
        assert!(
            found.iter().any(|f| f == "limit > 100: = false"),
            "found {found:?}"
        );
    }

    #[test]
    fn an_enum_member_settles_a_comparison_against_that_member() {
        // the class alone cannot decide this — an instance of `Color` is ambiguous against
        // `Color.RED`. the member is the whole value of the observation
        let found = at(
            "\
from enum import Enum

class Color(Enum):
    RED = 1
    BLUE = 2

c = pick()
<CURSOR>
if c is Color.RED:
    r = 1
",
            vec![(
                "c",
                Observed::IsEnumMember {
                    class: ClassName {
                        module: "main".to_string(),
                        qualname: "Color".to_string(),
                    },
                    member: Name::new("RED"),
                },
            )],
        );
        assert!(
            found.iter().any(|f| f == "c is Color.RED: = true"),
            "found {found:?}"
        );
    }

    #[test]
    fn a_member_the_enum_does_not_have_settles_nothing() {
        // falling back to the bare class would be reporting a reading the file contradicts as
        // though it were one the file supports
        let found = at(
            "\
from enum import Enum

class Color(Enum):
    RED = 1
    BLUE = 2

c = pick()
<CURSOR>
if c is Color.RED:
    r = 1
",
            vec![(
                "c",
                Observed::IsEnumMember {
                    class: ClassName {
                        module: "main".to_string(),
                        qualname: "Color".to_string(),
                    },
                    member: Name::new("GREEN"),
                },
            )],
        );
        assert!(found.is_empty(), "found {found:?}");
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
