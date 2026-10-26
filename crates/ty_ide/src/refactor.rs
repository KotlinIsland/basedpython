//! Refactorings offered as code actions: rewrites the user asks for at a
//! position or selection, as opposed to the quick fixes a diagnostic carries.
//!
//! Every refactoring answers from the syntax tree and the semantic index rather
//! than from the text, and refuses with a reason whenever it cannot show that
//! the rewrite leaves the program meaning what it meant. A refusal is part of
//! the answer: a client that asked for a refactoring explicitly shows why it is
//! unavailable.
//!
//! Each of them answers both questions at once — whether it applies here, and
//! what it would rewrite — because the second *is* the first: a refactoring
//! applies exactly when a rewrite that preserves meaning can be spelled, and
//! there is no cheaper test that does not amount to a second implementation of
//! the same reasoning, which could then disagree with it. So [`refactors`]
//! hands its edits back rather than throwing them away, and the caller tells it
//! which refactorings to consider rather than being given all of them.

mod class_members;
mod data_class;
mod evaluation;
mod extract_function;
mod extract_variable;
mod flow;
mod hazards;
mod inline_variable;
mod move_symbol;
mod names;
mod return_annotation;
mod split_join;
mod text;

use ruff_db::files::File;
use ruff_db::parsed::{ParsedModuleRef, parsed_module};
use ruff_db::source::{SourceText, source_text};
use ruff_diagnostics::Edit;
use ruff_python_codegen::Stylist;
use ruff_text_size::TextRange;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::SemanticModel;

use crate::FileEdit;

pub use class_members::{
    ClassMember, HierarchyClass, MemberKind, MovableMembers, movable_members, pull_members_up,
    push_members_down,
};
pub use move_symbol::{MovableKind, MovableSymbol, SymbolMove, movable_symbol, move_symbol};

/// The refactorings the server knows how to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefactorKind {
    InlineVariable,
    ExtractVariable,
    IntroduceConstant,
    IntroduceParameter,
    IntroduceField,
    ExtractFunction,
    AddReturnAnnotation,
    ConvertToDataClass,
    ConvertFromDataClass,
    SplitList,
    JoinList,
}

impl RefactorKind {
    pub const ALL: &'static [RefactorKind] = &[
        RefactorKind::InlineVariable,
        RefactorKind::ExtractVariable,
        RefactorKind::IntroduceConstant,
        RefactorKind::IntroduceParameter,
        RefactorKind::IntroduceField,
        RefactorKind::ExtractFunction,
        RefactorKind::AddReturnAnnotation,
        RefactorKind::ConvertToDataClass,
        RefactorKind::ConvertFromDataClass,
        RefactorKind::SplitList,
        RefactorKind::JoinList,
    ];

    /// The LSP code action kind the refactoring is offered as.
    pub const fn code_action_kind(self) -> &'static str {
        match self {
            RefactorKind::InlineVariable => "refactor.inline.variable",
            RefactorKind::ExtractVariable => "refactor.extract.variable",
            RefactorKind::IntroduceConstant => "refactor.extract.constant",
            RefactorKind::IntroduceParameter => "refactor.extract.parameter",
            RefactorKind::IntroduceField => "refactor.extract.field",
            RefactorKind::ExtractFunction => "refactor.extract.function",
            RefactorKind::AddReturnAnnotation => "refactor.rewrite.returnAnnotation",
            RefactorKind::ConvertToDataClass => "refactor.rewrite.toDataClass",
            RefactorKind::ConvertFromDataClass => "refactor.rewrite.fromDataClass",
            RefactorKind::SplitList => "refactor.rewrite.splitList",
            RefactorKind::JoinList => "refactor.rewrite.joinList",
        }
    }

    /// A stable name for the refactoring, which round-trips through a code
    /// action's `data` so the edits can be computed when the action is resolved.
    pub const fn id(self) -> &'static str {
        match self {
            RefactorKind::InlineVariable => "inline-variable",
            RefactorKind::ExtractVariable => "extract-variable",
            RefactorKind::IntroduceConstant => "introduce-constant",
            RefactorKind::IntroduceParameter => "introduce-parameter",
            RefactorKind::IntroduceField => "introduce-field",
            RefactorKind::ExtractFunction => "extract-function",
            RefactorKind::AddReturnAnnotation => "add-return-annotation",
            RefactorKind::ConvertToDataClass => "convert-to-data-class",
            RefactorKind::ConvertFromDataClass => "convert-from-data-class",
            RefactorKind::SplitList => "split-list",
            RefactorKind::JoinList => "join-list",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|kind| kind.id() == id)
    }
}

/// A refactoring available at a range, or the reason it is not.
#[derive(Debug, Clone)]
pub struct RefactorOffer {
    pub kind: RefactorKind,
    pub title: String,
    /// The range to ask about to have this rewrite again: the range the offer was asked for,
    /// or, for one of the expressions offered around a caret, that expression.
    pub range: TextRange,
    /// `Ok` holds the rewrite and `Err` why there is none.
    ///
    /// The edits come with the offer because working them out is how the question was answered:
    /// a refactoring applies exactly when its rewrite can be spelled, so there is no cheaper
    /// test to run first. A caller that only needs to know *whether* to offer it may drop them.
    pub availability: Result<Vec<FileEdit>, String>,
}

/// A refactoring worked out in full.
pub struct Refactor {
    pub kind: RefactorKind,
    pub title: String,
    pub edits: Vec<FileEdit>,
}

/// Why a refactoring does not apply: either it is not about what is at the
/// range at all, or it is but would change what the program means.
pub(crate) enum Refusal {
    /// Nothing at the range is the kind of thing the refactoring rewrites.
    NotApplicable,
    /// The refactoring is about what is at the range, but cannot be applied.
    Refused { title: String, reason: String },
}

impl Refusal {
    fn refused(title: impl Into<String>, reason: impl Into<String>) -> Self {
        Refusal::Refused {
            title: title.into(),
            reason: reason.into(),
        }
    }
}

/// A planned rewrite of one file.
pub(crate) struct Plan {
    title: String,
    edits: Vec<Edit>,
}

/// Everything a refactoring reads about the file it rewrites.
pub(crate) struct RefactorContext<'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    model: SemanticModel<'db>,
    parsed: ParsedModuleRef,
    source: SourceText,
    stylist: Stylist<'static>,
}

impl<'db> RefactorContext<'db> {
    fn new(db: &'db dyn Db, file: ProgramFile<'db>) -> Self {
        let parsed = parsed_module(db, file.python_file(db)).load(db);
        let source = source_text(db, file.file(db));
        let stylist = Stylist::from_tokens(parsed.tokens(), source.as_str()).into_owned();
        Self {
            db,
            file,
            model: SemanticModel::new(db, file),
            parsed,
            source,
            stylist,
        }
    }

    fn source(&self) -> &str {
        self.source.as_str()
    }

    fn file(&self) -> File {
        self.file.file(self.db)
    }

    fn is_basedpython(&self) -> bool {
        self.file().source_type(self.db).is_basedpython()
    }

    /// A plan's edits, each carrying the file it applies to.
    fn file_edits(&self, edits: Vec<Edit>) -> Vec<FileEdit> {
        let file = self.file();
        edits
            .into_iter()
            .map(|edit| FileEdit { file, edit })
            .collect()
    }

    fn plan(&self, kind: RefactorKind, range: TextRange) -> Result<Plan, Refusal> {
        match kind {
            RefactorKind::InlineVariable => inline_variable::plan(self, range),
            RefactorKind::ExtractVariable => {
                extract_variable::plan(self, range, extract_variable::Target::Variable)
            }
            RefactorKind::IntroduceConstant => {
                extract_variable::plan(self, range, extract_variable::Target::Constant)
            }
            RefactorKind::IntroduceParameter => {
                extract_variable::plan(self, range, extract_variable::Target::Parameter)
            }
            RefactorKind::IntroduceField => {
                extract_variable::plan(self, range, extract_variable::Target::Field)
            }
            RefactorKind::ExtractFunction => extract_function::plan(self, range),
            RefactorKind::AddReturnAnnotation => return_annotation::plan(self, range),
            RefactorKind::ConvertToDataClass => {
                data_class::plan(self, range, data_class::Direction::ToDataClass)
            }
            RefactorKind::ConvertFromDataClass => {
                data_class::plan(self, range, data_class::Direction::FromDataClass)
            }
            RefactorKind::SplitList => split_join::plan(self, range, split_join::Direction::Split),
            RefactorKind::JoinList => split_join::plan(self, range, split_join::Direction::Join),
        }
    }
}

/// Those of `kinds` that are about what is at `range` in `file`, each with its rewrite or the
/// reason there is none.
///
/// `kinds` is asked for rather than assumed because answering for one costs working its rewrite
/// out, so a caller that will not offer a refactoring should not name it — a client narrowing a
/// `textDocument/codeAction` with `only` then pays for nothing it asked no question about.
///
/// With `around_caret`, an empty `range` is answered for extracting a variable or a constant
/// with one offer for each expression around the caret, innermost first, each carrying the
/// expression's range: what an editor offers to choose from when extract is asked for with
/// nothing selected. Without it a caret extracts nothing, which is what a client asking while
/// the caret moves wants, rather than an offer for every expression the caret is in.
pub fn refactors(
    db: &dyn Db,
    file: ProgramFile<'_>,
    range: TextRange,
    kinds: &[RefactorKind],
    around_caret: bool,
) -> Vec<RefactorOffer> {
    if kinds.is_empty() || !is_refactorable(db, file) {
        return Vec::new();
    }
    let context = RefactorContext::new(db, file);
    if context.parsed.has_invalid_syntax() {
        return Vec::new();
    }

    let mut offers = Vec::new();
    for &kind in kinds {
        let extracted = match kind {
            RefactorKind::ExtractVariable => Some(extract_variable::Target::Variable),
            RefactorKind::IntroduceConstant => Some(extract_variable::Target::Constant),
            RefactorKind::IntroduceParameter => Some(extract_variable::Target::Parameter),
            RefactorKind::IntroduceField => Some(extract_variable::Target::Field),
            _ => None,
        };
        let plans = match extracted {
            Some(target) if around_caret && range.is_empty() => {
                extract_variable::around_caret(&context, range.start(), target)
            }
            _ => vec![(range, context.plan(kind, range))],
        };
        offers.extend(plans.into_iter().filter_map(|(range, plan)| match plan {
            Ok(plan) => Some(RefactorOffer {
                kind,
                title: plan.title,
                range,
                availability: Ok(context.file_edits(plan.edits)),
            }),
            Err(Refusal::NotApplicable) => None,
            Err(Refusal::Refused { title, reason }) => Some(RefactorOffer {
                kind,
                title,
                range,
                availability: Err(reason),
            }),
        }));
    }
    offers
}

/// The edits of the refactoring `kind` at `range`, or why it cannot be applied.
pub fn refactor(
    db: &dyn Db,
    file: ProgramFile<'_>,
    kind: RefactorKind,
    range: TextRange,
) -> Result<Refactor, String> {
    const NOT_HERE: &str = "the refactoring does not apply here";
    if !is_refactorable(db, file) {
        return Err(NOT_HERE.to_string());
    }
    let context = RefactorContext::new(db, file);
    if context.parsed.has_invalid_syntax() {
        return Err("the file has syntax errors".to_string());
    }
    match context.plan(kind, range) {
        Ok(plan) => Ok(Refactor {
            kind,
            title: plan.title,
            edits: context.file_edits(plan.edits),
        }),
        Err(Refusal::NotApplicable) => Err(NOT_HERE.to_string()),
        Err(Refusal::Refused { reason, .. }) => Err(reason),
    }
}

/// Only python and basedpython source is rewritten: a notebook's cells and a
/// stub's declarations are not code these refactorings reason about.
fn is_refactorable(db: &dyn Db, file: ProgramFile<'_>) -> bool {
    file.file(db).source_type(db).is_py_file()
}

#[cfg(test)]
pub(crate) mod test_support {
    use ruff_db::files::{File, system_path_to_file};
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{Ranged, TextRange, TextSize};
    use ty_project::{ProjectMetadata, SemanticDb as _};

    use super::{RefactorKind, refactor, refactors};

    /// A file with a `<CURSOR>`, or a `<START>`…`<END>` selection, to refactor.
    pub(crate) struct RefactorTest {
        db: ty_project::TestDb,
        file: File,
        range: TextRange,
    }

    impl RefactorTest {
        pub(crate) fn python(source: &str) -> Self {
            Self::with_files("main.py", source, &[])
        }

        pub(crate) fn basedpython(source: &str) -> Self {
            Self::with_files("main.by", source, &[])
        }

        pub(crate) fn with_files(path: &str, source: &str, others: &[(&str, &str)]) -> Self {
            let mut db =
                ty_project::TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));
            let mut text = dedent(source).into_owned();
            let range = if let Some(cursor) = text.find("<CURSOR>") {
                text.replace_range(cursor..cursor + "<CURSOR>".len(), "");
                TextRange::empty(TextSize::try_from(cursor).unwrap())
            } else {
                let start = text
                    .find("<START>")
                    .expect("a `<CURSOR>` or `<START>` marker");
                text.replace_range(start..start + "<START>".len(), "");
                let end = text.find("<END>").expect("an `<END>` marker");
                text.replace_range(end..end + "<END>".len(), "");
                TextRange::new(
                    TextSize::try_from(start).unwrap(),
                    TextSize::try_from(end).unwrap(),
                )
            };
            for (other, contents) in others {
                db.write_file(other, dedent(contents))
                    .expect("write to memory file system to be successful");
            }
            db.write_file(path, text)
                .expect("write to memory file system to be successful");
            let file = system_path_to_file(&db, path).expect("newly written file to exist");
            Self { db, file, range }
        }

        /// What `kind` offers around the test's caret, one line for each offer: the text it
        /// is about, its title, and why it is refused when it is. Each offer that applies is
        /// checked to be the rewrite asking about its own range gives.
        pub(crate) fn around_caret(&self, kind: RefactorKind) -> String {
            let program_file = self.db.program_file(self.file);
            let source = ruff_db::source::source_text(&self.db, self.file);
            refactors(&self.db, program_file, self.range, &[kind], true)
                .into_iter()
                .map(|offer| {
                    let text = &source.as_str()[offer.range];
                    match offer.availability {
                        Ok(edits) => {
                            let again = refactor(&self.db, program_file, kind, offer.range)
                                .expect("an offer's range to give its rewrite again");
                            assert_eq!(
                                edits.iter().map(|edit| &edit.edit).collect::<Vec<_>>(),
                                again
                                    .edits
                                    .iter()
                                    .map(|edit| &edit.edit)
                                    .collect::<Vec<_>>(),
                            );
                            format!("{text}: {}", offer.title)
                        }
                        Err(reason) => format!("{text}: refused: {} ({reason})", offer.title),
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        /// The file after applying `kind`, or why it was not offered or refused.
        pub(crate) fn apply(&self, kind: RefactorKind) -> String {
            let program_file = self.db.program_file(self.file);
            let offers = refactors(&self.db, program_file, self.range, RefactorKind::ALL, false);
            let Some(offer) = offers.iter().find(|offer| offer.kind == kind) else {
                return "not offered".to_string();
            };
            match refactor(&self.db, program_file, kind, self.range) {
                Ok(result) => {
                    let offered = offer
                        .availability
                        .as_ref()
                        .expect("a refactoring that applies must be offered as available");
                    assert_eq!(offer.title, result.title);
                    // the offer and the resolve are two computations of one rewrite, and a
                    // client applies whichever it was handed
                    assert_eq!(
                        offered.iter().map(|edit| &edit.edit).collect::<Vec<_>>(),
                        result
                            .edits
                            .iter()
                            .map(|edit| &edit.edit)
                            .collect::<Vec<_>>(),
                        "the offered edits and the resolved edits must be the same rewrite"
                    );
                    let source = ruff_db::source::source_text(&self.db, self.file);
                    let mut text = source.as_str().to_string();
                    let mut edits: Vec<_> =
                        result.edits.into_iter().map(|edit| edit.edit).collect();
                    edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
                    for edit in edits {
                        text.replace_range(
                            edit.start().to_usize()..edit.end().to_usize(),
                            edit.content().unwrap_or_default(),
                        );
                    }
                    format!("{}\n---\n{text}", offer.title)
                }
                Err(reason) => {
                    assert_eq!(offer.availability.as_ref().err(), Some(&reason));
                    format!("refused: {} ({reason})", offer.title)
                }
            }
        }
    }
}
