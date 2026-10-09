//! basedpython: the python the checker spells for the transpiled program
//!
//! a lowering that writes a type out at runtime — a soundness check's `isinstance`
//! target, the specialization a reified generic is called with, a type test's probe —
//! asks the checker how the type is spelled, and the checker names each class by the
//! binding the module's global scope reaches it through. a module-level `private`
//! symbol is not emitted under that binding's name, though: `private class Kept` is
//! emitted as `_Kept`, or under another name where the module already has that one, and
//! only the transpiler decides which. so a spelling keeps the names of those symbols
//! apart, and whoever writes the spelling out spells them
//!
//! every binding of the module's global scope a spelling reads is kept apart the same way,
//! so the checker, which spells the same thing while it checks, can record that the
//! transpiled program reads it — an import read only by a lowering is still used

use ruff_db::files::File;
use ruff_python_ast::name::Name;
use ruff_text_size::{Ranged, TextRange};

use crate::Db;
use crate::place::{RuntimeBinding, RuntimeRead, runtime_binding};
use crate::types::ProgramEnvironment;
use crate::types::visibility::private_symbols;

/// where a lowering writes a spelling into code that runs, which decides what each name it
/// reads holds there
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeSite {
    range: TextRange,
}

impl RuntimeSite {
    /// the code `node` is written as
    pub(crate) fn at(node: &impl Ranged) -> Self {
        Self {
            range: node.range(),
        }
    }
}

/// a python expression the checker spells for the transpiled program, which evaluates in
/// the module it was spelled for to the runtime object of a type (`list[int]`) or to a
/// value (`Color.RED`)
///
/// each module-level `private` symbol it reads is kept apart, by the name the source
/// declares it under, for [`Self::spelled`] to write as the transpiled module binds it
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimeSpelling {
    /// never two [`SpellingPart::Text`] in a row and never an empty one, so two spellings
    /// of the same expression compare equal however they were put together
    parts: Vec<SpellingPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SpellingPart {
    Text(String),
    /// a binding of the module's global scope, written under its own name
    Global(Name),
    /// a module-level `private` symbol, by the name the source declares it under
    ModulePrivate(Name),
}

impl RuntimeSpelling {
    /// a spelling that reads no module-level `private` symbol
    pub(crate) fn text(text: impl Into<String>) -> Self {
        let mut spelling = Self::default();
        spelling.push_str(&text.into());
        spelling
    }

    /// `name` as read through `file`'s global scope, kept apart when it is one of the
    /// module's `private` symbols and written as it is otherwise
    ///
    /// the caller has resolved `name` to a binding of `file`'s own global scope. a name
    /// that reaches a builtin because the module leaves it unbound is spelled with
    /// [`Self::text`] instead: the module may declare a `private` symbol of that name
    /// without binding it, and the builtin is not that symbol
    pub(crate) fn global(db: &dyn Db, file: File, name: &Name) -> Self {
        let part = if private_symbols(db, file).contains(name) {
            SpellingPart::ModulePrivate(name.clone())
        } else {
            SpellingPart::Global(name.clone())
        };
        Self { parts: vec![part] }
    }

    /// every binding of the module's global scope the spelling reads, by the name the
    /// source binds it under
    pub(crate) fn global_reads(&self) -> impl Iterator<Item = &Name> {
        self.parts.iter().filter_map(|part| match part {
            SpellingPart::Global(name) | SpellingPart::ModulePrivate(name) => Some(name),
            SpellingPart::Text(_) => None,
        })
    }

    /// the first module-level name the spelling reads that does not hold, where `site` runs,
    /// the binding the checker resolved — with what it holds instead
    pub(crate) fn unbound_read<'db>(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        file: File,
        site: RuntimeSite,
    ) -> Option<(Name, RuntimeBinding)> {
        self.global_reads().find_map(|name| {
            let binding = runtime_binding(
                db,
                env,
                db.program_file(file),
                RuntimeRead::Global {
                    name,
                    range: site.range,
                },
            );
            (binding != RuntimeBinding::Bound).then(|| (name.clone(), binding))
        })
    }

    pub(crate) fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if let Some(SpellingPart::Text(last)) = self.parts.last_mut() {
            last.push_str(text);
        } else {
            self.parts.push(SpellingPart::Text(text.to_owned()));
        }
    }

    pub(crate) fn push(&mut self, other: &Self) {
        for part in &other.parts {
            match part {
                SpellingPart::Text(text) => self.push_str(text),
                SpellingPart::Global(_) | SpellingPart::ModulePrivate(_) => {
                    self.parts.push(part.clone());
                }
            }
        }
    }

    /// `spellings`, each after the first preceded by `separator`
    pub(crate) fn join(spellings: impl IntoIterator<Item = Self>, separator: &str) -> Self {
        let mut joined = Self::default();
        for (index, spelling) in spellings.into_iter().enumerate() {
            if index > 0 {
                joined.push_str(separator);
            }
            joined.push(&spelling);
        }
        joined
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// the expression as the transpiled module reads it, each module-level `private`
    /// symbol written under the name `module_private` gives it
    pub fn spelled(&self, module_private: impl Fn(&str) -> String) -> String {
        let mut spelled = String::new();
        for part in &self.parts {
            match part {
                SpellingPart::Text(text) => spelled.push_str(text),
                SpellingPart::Global(name) => spelled.push_str(name),
                SpellingPart::ModulePrivate(name) => spelled.push_str(&module_private(name)),
            }
        }
        spelled
    }

    /// the expression as the source reads it, each symbol by the name the source declares
    /// it under — for a diagnostic, and for source a later transpile phase renames itself
    pub fn source(&self) -> String {
        self.spelled(ToOwned::to_owned)
    }
}
