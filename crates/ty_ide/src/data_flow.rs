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
use ty_python_semantic::stop_offset;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// this condition will go this way
    Condition {
        /// which way
        taken: bool,
    },
    /// this code will not run
    Unreachable,
    /// this read will find this value
    Value {
        /// the name being read, as the source spells it
        name: String,
        /// what it will hold, written the way a source writes it
        value: String,
    },
}

impl Finding {
    /// what to show a reader beside the source
    ///
    /// short on purpose: it is drawn inline, in the editor font, beside code somebody is reading
    /// while stopped in a debugger
    ///
    /// a value's label names the name it is about — `discount = 0.0` — where a condition's does
    /// not. that is not a style difference, it is where the label goes: a client draws these in the
    /// margin past the end of the line, not against the expression, because an inlay there reflows
    /// the code it is annotating. a `= false` in that margin is unambiguous when the line holds one
    /// condition; a bare `= 0.0` past `total = base + discount` would be read as being about
    /// `total`. the `a: 1` an IDE's own debugger draws was the alternative, and it loses for the
    /// same reason — that hint is drawn *at* the variable, where the subject needs no naming
    pub fn label(&self) -> String {
        match &self.kind {
            FindingKind::Condition { taken: true } => "= true".to_string(),
            FindingKind::Condition { taken: false } => "= false".to_string(),
            FindingKind::Unreachable => "will not run".to_string(),
            FindingKind::Value { name, value } => format!("{name} = {value}"),
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
    // asked for rather than computed here, so that the boundary deciding which findings are below
    // the stop and the one deciding which seeds survive it are the one offset. they were computed
    // separately once, agreed on every file anybody tried, and disagreed about a stop on the first
    // statement of a function body — see [`ty_python_semantic::stop_offset`]
    let below = stop_offset(db, source_file, line);

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

    let values = flow.values.iter().filter_map(|read| {
        let name = &source[read.range];
        // a read written across lines — `obj.\n    attr` — has no one-line spelling, and a label
        // with a newline in it cannot be drawn in a margin. dropping it loses a fact; drawing it
        // would break the line the reader is looking at
        if name.contains('\n') {
            return None;
        }
        Some(Finding {
            range: read.range,
            kind: FindingKind::Value {
                name: name.to_string(),
                value: read.value.clone(),
            },
        })
    });

    let mut findings: Vec<Finding> = conditions.chain(unreachable).chain(values).collect();
    // in source order, because a client stacks the labels for one line in the order it is given
    // them and a margin that reads back-to-front is one the reader has to sort out
    findings.sort_by_key(|finding| finding.range.start());
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::CursorTest;
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
    ///
    /// the fixture is a `.by` file because that is the only kind this feature is ever asked about:
    /// the plugin fires on a basedpython file type and on nothing else. it is not a formality —
    /// basedpython infers a literal type for a float and python does not, so `discount = 0.0` is
    /// `float` in a `.py` fixture and `0.0` in the file a user is actually stopped in
    fn at(source: &str, observations: Vec<(&str, Observed)>) -> Vec<String> {
        let test = CursorTest::builder().source("main.by", source).build();
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

    /// the case the float observation exists for: the value came out of a call, so the file alone
    /// says `float` and cannot say which one. this is what reaches the reader beside the code
    #[test]
    fn a_float_read_off_the_program_is_shown_as_the_value_it_holds() {
        let found = at(
            "\
ratio = measure()
<CURSOR>
scaled = ratio
",
            vec![("ratio", Observed::IsFloat("0.25".to_string()))],
        );
        assert!(
            found.iter().any(|f| f == "ratio: ratio = 0.25"),
            "the read below the stop should say what it holds, and found {found:?}"
        );
    }

    /// every float, including the two source cannot write. a reading is a statement about the
    /// value, and `nan` really is what the name holds — replacing it with `float` would drop a
    /// fact to defend against a comparison nothing folds. see the note on
    /// `fold_literal_rich_comparison`, which is where that defence belongs
    #[test]
    fn the_floats_source_cannot_write_are_still_shown() {
        for text in ["nan", "-0.0", "inf"] {
            let found = at(
                "\
ratio = measure()
<CURSOR>
scaled = ratio
",
                vec![("ratio", Observed::IsFloat(text.to_string()))],
            );
            assert!(
                found.iter().any(|f| f.starts_with("ratio: ratio = ")),
                "{text} is a value the debugger really read, and found {found:?}"
            );
        }
    }

    /// the boundary, pinned deliberately rather than left to be discovered: `by` folds `Int`,
    /// `Bool`, `String` and `Bytes` literal comparisons and not `Float`, so a float seed narrows
    /// and displays but decides no branch. if this ever starts finding something, the `Float` arm
    /// has been added and the `nan` / `-0.0` cases above it have to have been handled
    #[test]
    fn a_float_does_not_yet_decide_a_comparison() {
        let found = at(
            "\
ratio = measure()
<CURSOR>
if ratio > 0.5:
    high = 1
",
            vec![("ratio", Observed::IsFloat("0.25".to_string()))],
        );
        assert!(
            !found.iter().any(|f| f.contains("ratio > 0.5")),
            "float comparisons are not folded, and found {found:?}"
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

    /// the function a user reported both of this module's bugs against, called with `qty=3` and
    /// `member=False`
    ///
    /// worth keeping verbatim: the first defect only appeared because line 2 is the *first*
    /// statement of the body, and the second only appeared because `discount` is a float
    const PRICE: &str = "\
def price(qty: int, member: bool):
    discount = 0.0
    if qty >= 10:
        discount = 0.1
    if member:
        discount += 0.05
    return discount
";

    /// what the two parameters were, at whichever line the program stopped on
    fn priced() -> Vec<(&'static str, Observed)> {
        vec![
            ("qty", Observed::IsInt("3".to_string())),
            ("member", Observed::IsBool(false)),
        ]
    }

    #[test]
    fn a_stop_on_the_first_statement_of_a_function_body_is_still_inside_that_function() {
        // reported as "nothing is shown until the stop reaches the `if`". a statement's range
        // begins at its first token, so the indentation in front of the first statement of a body
        // belonged to no statement — and a stop offset taken at the start of the line landed just
        // before the body, which made `stopped_scope` answer with the module and every seed get
        // refused as being about another frame. one line further down the same file decided
        // everything, which is what made it look like the analysis rather than the offset
        let found = at(
            &PRICE.replacen("    discount = 0.0", "    <CURSOR>discount = 0.0", 1),
            priced(),
        );
        assert!(
            found.iter().any(|f| f == "qty >= 10: = false")
                && found.iter().any(|f| f == "member: = false"),
            "both branches are below this stop and both parameters were observed, and found {found:?}"
        );
    }

    #[test]
    fn a_stop_one_line_lower_reaches_exactly_the_same_answer() {
        // the control for the test above. these two stops differ only in which line the program is
        // held on, and nothing between them binds or reads anything — so an answer that differed
        // would be the offset showing through again
        let first = at(
            &PRICE.replacen("    discount = 0.0", "    <CURSOR>discount = 0.0", 1),
            priced(),
        );
        let second = at(
            &PRICE.replacen("    if qty >= 10:", "    <CURSOR>if qty >= 10:", 1),
            priced(),
        );
        assert_eq!(first, second, "the two stops disagree");
    }

    #[test]
    fn the_value_a_name_still_holds_below_two_dead_branches_is_reported() {
        // the whole point of the feature past reachability: neither `if` runs, so neither
        // assignment to `discount` runs, so the `0.0` from line 2 is what `return discount` finds.
        // no observation of `discount` is involved — a float is not an observation this can carry,
        // and it does not need to be. the source says what it was assigned and the seeds say which
        // of the later assignments are dead
        let found = at(
            &PRICE.replacen("    if qty >= 10:", "    <CURSOR>if qty >= 10:", 1),
            priced(),
        );
        assert!(
            found.iter().any(|f| f == "discount: discount = 0.0"),
            "found {found:?}"
        );
    }

    #[test]
    fn a_read_inside_a_decided_condition_gets_no_value_of_its_own() {
        // `qty >= 10` already carries a `= false`, and it is drawn in the same margin. `qty = 3`
        // beside it is the working rather than the answer
        let found = at(
            &PRICE.replacen("    if qty >= 10:", "    <CURSOR>if qty >= 10:", 1),
            priced(),
        );
        assert!(
            !found.iter().any(|f| f.starts_with("qty: ")),
            "found {found:?}"
        );
    }

    #[test]
    fn a_value_that_depends_on_something_unobserved_is_not_guessed_at() {
        // the control for the value half. `qty` decides one branch and nothing decides the other,
        // so `discount` at the return is `0.0` or `0.15` and the honest answer is neither. a
        // feature that picked the likelier one would be worth less than one that says nothing,
        // because the reason to trust it at all is that it only reports what follows
        let found = at(
            "\
def price(qty: int, member: bool):
    discount = 0.0
    <CURSOR>if qty >= 10:
        discount = 0.1
    if member:
        discount += 0.05
    return discount
",
            vec![("qty", Observed::IsInt("3".to_string()))],
        );
        assert!(
            !found.iter().any(|f| f.starts_with("discount: ")),
            "found {found:?}"
        );
    }

    #[test]
    fn a_value_the_source_alone_already_fixes_is_not_reported_as_the_debuggers_doing() {
        // `rate` is 0.2 whether or not anything is being debugged, and the editor is not being
        // told that by a debugger. reporting it would credit ordinary inference to the stop
        let found = at(
            "\
def price(qty: int):
    rate = 0.2
    <CURSOR>if qty >= 10:
        big = 1
    return rate
",
            vec![("qty", Observed::IsInt("3".to_string()))],
        );
        assert!(
            !found.iter().any(|f| f.starts_with("rate: ")),
            "found {found:?}"
        );
    }

    #[test]
    fn a_container_whose_length_a_dead_branch_would_have_changed_reports_no_value() {
        // the rule that a fact only travels to code that has not run when it will still be true
        // there. this needs no guard of its own: a list is a `list[int]` and a `list[int]` is not
        // one value, so there is nothing for the value half to report. the test is here because
        // that is a property of the design rather than of anything written down, and a future
        // change that started reporting a container would break it silently
        let found = at(
            "\
def collect(flag: bool):
    items = []
    <CURSOR>if flag:
        items.append(1)
    return items
",
            vec![("flag", Observed::IsBool(false))],
        );
        assert!(
            !found.iter().any(|f| f.starts_with("items: ")),
            "found {found:?}"
        );
    }

    #[test]
    fn a_store_is_not_annotated_with_the_value_it_is_storing() {
        // `discount = 0.1` says `0.1` on its own line. the value half is for reads, where somebody
        // has to work out what arrived
        let found = at(
            &PRICE.replacen("    if qty >= 10:", "    <CURSOR>if qty >= 10:", 1),
            priced(),
        );
        assert!(
            found.iter().all(|f| f != "discount: discount = 0.1"),
            "found {found:?}"
        );
    }
}
