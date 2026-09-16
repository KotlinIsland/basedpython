//! Refactorings offered as code actions: rewrites the user asks for at a
//! position or selection, as opposed to the quick fixes a diagnostic carries.
//!
//! Every refactoring answers from the syntax tree and the semantic index rather
//! than from the text, and refuses with a reason whenever it cannot show that
//! the rewrite leaves the program meaning what it meant. A refusal is part of
//! the answer: a client that asked for a refactoring explicitly shows why it is
//! unavailable.

mod data_class;
mod evaluation;
mod extract_function;
mod extract_variable;
mod flow;
mod hazards;
mod inline_variable;
mod names;
mod return_annotation;
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

/// The refactorings the server knows how to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefactorKind {
    InlineVariable,
    ExtractVariable,
    IntroduceConstant,
    ExtractFunction,
    AddReturnAnnotation,
    ConvertToDataClass,
    ConvertFromDataClass,
}

impl RefactorKind {
    pub const ALL: [RefactorKind; 7] = [
        RefactorKind::InlineVariable,
        RefactorKind::ExtractVariable,
        RefactorKind::IntroduceConstant,
        RefactorKind::ExtractFunction,
        RefactorKind::AddReturnAnnotation,
        RefactorKind::ConvertToDataClass,
        RefactorKind::ConvertFromDataClass,
    ];

    /// The LSP code action kind the refactoring is offered as.
    pub const fn code_action_kind(self) -> &'static str {
        match self {
            RefactorKind::InlineVariable => "refactor.inline.variable",
            RefactorKind::ExtractVariable => "refactor.extract.variable",
            RefactorKind::IntroduceConstant => "refactor.extract.constant",
            RefactorKind::ExtractFunction => "refactor.extract.function",
            RefactorKind::AddReturnAnnotation => "refactor.rewrite.returnAnnotation",
            RefactorKind::ConvertToDataClass => "refactor.rewrite.toDataClass",
            RefactorKind::ConvertFromDataClass => "refactor.rewrite.fromDataClass",
        }
    }

    /// A stable name for the refactoring, which round-trips through a code
    /// action's `data` so the edits can be computed when the action is resolved.
    pub const fn id(self) -> &'static str {
        match self {
            RefactorKind::InlineVariable => "inline-variable",
            RefactorKind::ExtractVariable => "extract-variable",
            RefactorKind::IntroduceConstant => "introduce-constant",
            RefactorKind::ExtractFunction => "extract-function",
            RefactorKind::AddReturnAnnotation => "add-return-annotation",
            RefactorKind::ConvertToDataClass => "convert-to-data-class",
            RefactorKind::ConvertFromDataClass => "convert-from-data-class",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.id() == id)
    }
}

/// A refactoring available at a range, or the reason it is not.
#[derive(Debug, Clone)]
pub struct RefactorOffer {
    pub kind: RefactorKind,
    pub title: String,
    /// `Err` holds why the refactoring cannot be applied here.
    pub availability: Result<(), String>,
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

    fn plan(&self, kind: RefactorKind, range: TextRange) -> Result<Plan, Refusal> {
        match kind {
            RefactorKind::InlineVariable => inline_variable::plan(self, range),
            RefactorKind::ExtractVariable => {
                extract_variable::plan(self, range, extract_variable::Target::Variable)
            }
            RefactorKind::IntroduceConstant => {
                extract_variable::plan(self, range, extract_variable::Target::Constant)
            }
            RefactorKind::ExtractFunction => extract_function::plan(self, range),
            RefactorKind::AddReturnAnnotation => return_annotation::plan(self, range),
            RefactorKind::ConvertToDataClass => {
                data_class::plan(self, range, data_class::Direction::ToDataClass)
            }
            RefactorKind::ConvertFromDataClass => {
                data_class::plan(self, range, data_class::Direction::FromDataClass)
            }
        }
    }
}

/// The refactorings that are about what is at `range` in `file`, each with
/// whether it can be applied.
pub fn refactors(db: &dyn Db, file: ProgramFile<'_>, range: TextRange) -> Vec<RefactorOffer> {
    if !is_refactorable(db, file) {
        return Vec::new();
    }
    let context = RefactorContext::new(db, file);
    if context.parsed.has_invalid_syntax() {
        return Vec::new();
    }

    RefactorKind::ALL
        .into_iter()
        .filter_map(|kind| match context.plan(kind, range) {
            Ok(plan) => Some(RefactorOffer {
                kind,
                title: plan.title,
                availability: Ok(()),
            }),
            Err(Refusal::NotApplicable) => None,
            Err(Refusal::Refused { title, reason }) => Some(RefactorOffer {
                kind,
                title,
                availability: Err(reason),
            }),
        })
        .collect()
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
            edits: plan
                .edits
                .into_iter()
                .map(|edit| FileEdit {
                    file: file.file(db),
                    edit,
                })
                .collect(),
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

        /// The file after applying `kind`, or why it was not offered or refused.
        pub(crate) fn apply(&self, kind: RefactorKind) -> String {
            let program_file = self.db.program_file(self.file);
            let offers = refactors(&self.db, program_file, self.range);
            let Some(offer) = offers.iter().find(|offer| offer.kind == kind) else {
                return "not offered".to_string();
            };
            match refactor(&self.db, program_file, kind, self.range) {
                Ok(result) => {
                    assert_eq!(
                        offer.availability,
                        Ok(()),
                        "a refactoring that applies must be offered as available"
                    );
                    assert_eq!(offer.title, result.title);
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
                    assert_eq!(offer.availability, Err(reason.clone()));
                    format!("refused: {} ({reason})", offer.title)
                }
            }
        }
    }
}
