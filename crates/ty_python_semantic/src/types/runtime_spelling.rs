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
use ruff_python_ast::helpers::UseSiteVariance;
use ruff_python_ast::name::Name;
use ruff_text_size::{Ranged, TextRange};

use crate::Db;
use crate::place::{RuntimeBinding, RuntimeRead, runtime_binding};
use crate::types::ProgramEnvironment;
use crate::types::check_order::InferredInCheckOrder;
use crate::types::reified_infer::{ArgVariance, python_bytes_literal, python_str_literal};
use crate::types::told_classes::ToldClasses;
use crate::types::visibility::private_symbols;

/// where a lowering writes a spelling into code that runs, which decides what each name it
/// reads holds there. what a name holds there is asked only once the scopes around the code
/// are inferred, so a site is made only with the proof that they are
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeSite<'db> {
    range: TextRange,
    inferred: InferredInCheckOrder<'db>,
}

impl<'db> RuntimeSite<'db> {
    /// the code `node` is written as, in the file `inferred` is for
    pub(crate) fn at(node: &impl Ranged, inferred: InferredInCheckOrder<'db>) -> Self {
        Self {
            range: node.range(),
            inferred,
        }
    }

    pub(crate) fn range(self) -> TextRange {
        self.range
    }

    pub(crate) fn inferred(self) -> InferredInCheckOrder<'db> {
        self.inferred
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
    /// the runtime helper that builds a literal type, which a module has only if asked for
    LiteralHelper,
    /// a `str` value, whose quotes depend on the python it is written for
    Str(String),
    /// a `bytes` value, whose quotes depend on the python it is written for
    Bytes(Vec<u8>),
    /// what opens telling the runtime the variance of the class spelled up to [`Self::Close`],
    /// which a class no transpiled module defines has told wherever it is subscripted. a
    /// matter for the program that runs, so the source reads as if it weren't there
    TellVariance(Box<[u8]>),
    /// what opens the projection of the type spelled up to [`Self::Close`]
    Project(UseSiteVariance),
    /// what closes [`Self::TellVariance`] or [`Self::Project`]
    Close,
    /// what the runtime is to be told of the classes the spelling names, which a module says
    /// once rather than where it writes the spelling. it writes nothing here
    Told(Box<ToldClasses>),
    /// what opens a subscript the program that runs writes through a helper, because the class
    /// would answer it for itself or has none to answer it with: the class up to
    /// [`Self::HelperSubscriptArguments`] and its arguments up to
    /// [`Self::HelperSubscriptClose`]. the source reads it as the subscript it is
    HelperSubscriptOpen(SpellingHelper),
    HelperSubscriptArguments,
    HelperSubscriptClose,
    /// the name the module reads an imported module by, for [`RuntimeSpelling::spelled`] to write as the
    /// transpiled module binds it
    Module(String),
}

/// a runtime helper a spelling writes a call to, which a module has only if asked for
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpellingHelper {
    /// `_by_lit`, which builds a literal type
    Literal,
    /// `_by_variance`, which tells the runtime what a class varies by
    Variance,
    /// `_by_out`, the projection `out T`
    Out,
    /// `_by_in`, the projection `in T`
    In,
    /// `_by_specialization`, the specialization of a class that answers a subscript for itself
    Specialization,
    /// `_by_subscript`, the subscript of a class the runtime may refuse to subscript
    Subscript,
}

impl RuntimeSpelling {
    /// a spelling that reads no module-level `private` symbol
    pub fn text(text: impl Into<String>) -> Self {
        let mut spelling = Self::default();
        spelling.push_str(&text.into());
        spelling
    }

    /// the literal type holding `value`, which is a python expression: `_by_lit(1)`
    pub fn literal(value: &Self) -> Self {
        let mut spelling = Self {
            parts: vec![SpellingPart::LiteralHelper],
        };
        spelling.push_str("(");
        spelling.push(value);
        spelling.push_str(")");
        spelling
    }

    /// the `str` value `value`
    pub fn string(value: &str) -> Self {
        Self {
            parts: vec![SpellingPart::Str(value.to_owned())],
        }
    }

    /// the `bytes` value `value`
    pub fn bytes(value: &[u8]) -> Self {
        Self {
            parts: vec![SpellingPart::Bytes(value.to_vec())],
        }
    }

    /// `class`, telling the runtime the variance of each of its type parameters as it is read
    pub fn telling_variance(class: &Self, variances: &[ArgVariance]) -> Self {
        let mut spelling = Self {
            parts: vec![SpellingPart::TellVariance(
                variances.iter().copied().map(ArgVariance::code).collect(),
            )],
        };
        spelling.push(class);
        spelling.parts.push(SpellingPart::Close);
        spelling
    }

    /// `inner`, projected as `variance` says. `in out` is the plain form
    pub fn projected(inner: Self, variance: UseSiteVariance) -> Self {
        if variance == UseSiteVariance::InOut {
            return inner;
        }
        let mut spelling = Self {
            parts: vec![SpellingPart::Project(variance)],
        };
        spelling.push(&inner);
        spelling.parts.push(SpellingPart::Close);
        spelling
    }

    /// `origin[arguments]`, which the program that runs writes through `helper`
    pub(crate) fn helper_subscript(
        helper: SpellingHelper,
        origin: &Self,
        arguments: &Self,
    ) -> Self {
        let mut spelling = Self {
            parts: vec![SpellingPart::HelperSubscriptOpen(helper)],
        };
        spelling.push(origin);
        spelling.parts.push(SpellingPart::HelperSubscriptArguments);
        spelling.push(arguments);
        spelling.parts.push(SpellingPart::HelperSubscriptClose);
        spelling
    }

    /// `path`, a class or the classes around it, as reached through the imported `module`
    pub(crate) fn in_module(module: &str, path: &str) -> Self {
        let mut spelling = Self {
            parts: vec![SpellingPart::Module(module.to_owned())],
        };
        spelling.push_str(".");
        spelling.push_str(path);
        spelling
    }

    /// each module the spelling reads, which a module has only if it imports it
    pub fn modules(&self) -> impl Iterator<Item = &str> {
        self.parts.iter().filter_map(|part| match part {
            SpellingPart::Module(module) => Some(module.as_str()),
            _ => None,
        })
    }

    /// this spelling, with what the runtime is to be told of what it names
    pub(crate) fn telling(mut self, told: ToldClasses) -> Self {
        if !told.is_empty() {
            self.parts.push(SpellingPart::Told(Box::new(told)));
        }
        self
    }

    /// what the runtime is to be told of the classes the spelling names
    pub fn told(&self) -> impl Iterator<Item = &ToldClasses> {
        self.parts.iter().filter_map(|part| match part {
            SpellingPart::Told(told) => Some(&**told),
            _ => None,
        })
    }

    /// each runtime helper the spelling writes a call to
    pub fn helpers(&self) -> impl Iterator<Item = SpellingHelper> {
        self.parts.iter().filter_map(|part| match part {
            SpellingPart::LiteralHelper => Some(SpellingHelper::Literal),
            SpellingPart::TellVariance(_) => Some(SpellingHelper::Variance),
            SpellingPart::Project(UseSiteVariance::Out) => Some(SpellingHelper::Out),
            SpellingPart::Project(UseSiteVariance::In) => Some(SpellingHelper::In),
            SpellingPart::HelperSubscriptOpen(helper) => Some(*helper),
            SpellingPart::Project(UseSiteVariance::InOut)
            | SpellingPart::Text(_)
            | SpellingPart::Global(_)
            | SpellingPart::ModulePrivate(_)
            | SpellingPart::Str(_)
            | SpellingPart::Bytes(_)
            | SpellingPart::HelperSubscriptArguments
            | SpellingPart::HelperSubscriptClose
            | SpellingPart::Module(_)
            | SpellingPart::Told(_)
            | SpellingPart::Close => None,
        })
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
            SpellingPart::Text(_)
            | SpellingPart::LiteralHelper
            | SpellingPart::Str(_)
            | SpellingPart::Bytes(_)
            | SpellingPart::TellVariance(_)
            | SpellingPart::Project(_)
            | SpellingPart::HelperSubscriptOpen(_)
            | SpellingPart::HelperSubscriptArguments
            | SpellingPart::HelperSubscriptClose
            | SpellingPart::Module(_)
            | SpellingPart::Told(_)
            | SpellingPart::Close => None,
        })
    }

    /// every binding of the module's global scope the spelling reads, each with the spelling
    /// of that read alone
    pub(crate) fn global_read_spellings(&self) -> impl Iterator<Item = (&Name, Self)> {
        self.parts.iter().filter_map(|part| match part {
            SpellingPart::Global(name) | SpellingPart::ModulePrivate(name) => Some((
                name,
                Self {
                    parts: vec![part.clone()],
                },
            )),
            SpellingPart::Text(_)
            | SpellingPart::LiteralHelper
            | SpellingPart::Str(_)
            | SpellingPart::Bytes(_)
            | SpellingPart::TellVariance(_)
            | SpellingPart::Project(_)
            | SpellingPart::HelperSubscriptOpen(_)
            | SpellingPart::HelperSubscriptArguments
            | SpellingPart::HelperSubscriptClose
            | SpellingPart::Module(_)
            | SpellingPart::Told(_)
            | SpellingPart::Close => None,
        })
    }

    /// the first module-level name the spelling reads that does not hold, where `site` runs,
    /// the binding the checker resolved — with what it holds instead
    pub(crate) fn unbound_read<'db>(
        &self,
        db: &'db dyn Db,
        env: &ProgramEnvironment<'db>,
        site: RuntimeSite<'db>,
    ) -> Option<(Name, RuntimeBinding)> {
        first_unbound_read(db, env, self.global_reads(), site)
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
                SpellingPart::Global(_)
                | SpellingPart::ModulePrivate(_)
                | SpellingPart::LiteralHelper
                | SpellingPart::Str(_)
                | SpellingPart::Bytes(_)
                | SpellingPart::TellVariance(_)
                | SpellingPart::Project(_)
                | SpellingPart::HelperSubscriptOpen(_)
                | SpellingPart::HelperSubscriptArguments
                | SpellingPart::HelperSubscriptClose
                | SpellingPart::Module(_)
                | SpellingPart::Told(_)
                | SpellingPart::Close => {
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
    /// symbol written under the name `module_private` gives it and each runtime helper under
    /// the one `helper` gives it
    pub fn spelled(
        &self,
        module_private: impl Fn(&str) -> String,
        helper: impl Fn(SpellingHelper) -> String,
        module: impl Fn(&str) -> String,
    ) -> String {
        self.written(module_private, helper, module, true, true)
    }

    /// [`Self::spelled`], writing a `str` or `bytes` value of a literal type as its code points.
    /// below python 3.12 an f-string's replacement field can hold neither a backslash nor the
    /// f-string's own quote, and what writes a spelling doesn't know whether it is in one
    pub fn spelled_without_quotes(
        &self,
        module_private: impl Fn(&str) -> String,
        helper: impl Fn(SpellingHelper) -> String,
        module: impl Fn(&str) -> String,
    ) -> String {
        self.written(module_private, helper, module, false, true)
    }

    /// what `running` adds to a spelling of what the source reads is for the program as it
    /// runs: where it tells the runtime something, and how it writes a projection
    fn written(
        &self,
        module_private: impl Fn(&str) -> String,
        helper: impl Fn(SpellingHelper) -> String,
        module: impl Fn(&str) -> String,
        quotes: bool,
        running: bool,
    ) -> String {
        let code_points = |keyword: &str, points: Vec<String>| {
            let trailing = if points.len() == 1 { "," } else { "" };
            format!("{keyword}=({}{trailing})", points.join(", "))
        };
        let mut spelled = String::new();
        for part in &self.parts {
            match part {
                SpellingPart::Text(text) => spelled.push_str(text),
                SpellingPart::Global(name) => spelled.push_str(name),
                SpellingPart::ModulePrivate(name) => spelled.push_str(&module_private(name)),
                SpellingPart::LiteralHelper => {
                    spelled.push_str(&helper(SpellingHelper::Literal));
                }
                SpellingPart::Str(value) if quotes => {
                    spelled.push_str(&python_str_literal(value));
                }
                SpellingPart::Bytes(value) if quotes => {
                    spelled.push_str(&python_bytes_literal(value));
                }
                SpellingPart::Str(value) => spelled.push_str(&code_points(
                    "text",
                    value.chars().map(|c| u32::from(c).to_string()).collect(),
                )),
                SpellingPart::Bytes(value) => spelled.push_str(&code_points(
                    "data",
                    value.iter().map(ToString::to_string).collect(),
                )),
                SpellingPart::TellVariance(codes) if running => {
                    let codes = codes.iter().map(ToString::to_string).collect::<Vec<_>>();
                    spelled.push_str(&helper(SpellingHelper::Variance));
                    spelled.push('(');
                    spelled.push_str(&codes.join(", "));
                    spelled.push_str(")(");
                }
                SpellingPart::Project(UseSiteVariance::Out) if running => {
                    spelled.push_str(&helper(SpellingHelper::Out));
                    spelled.push('(');
                }
                SpellingPart::Project(UseSiteVariance::In) if running => {
                    spelled.push_str(&helper(SpellingHelper::In));
                    spelled.push('(');
                }
                SpellingPart::Project(UseSiteVariance::Out) => spelled.push_str("out "),
                SpellingPart::Project(UseSiteVariance::In) => spelled.push_str("in "),
                SpellingPart::Close if running => spelled.push(')'),
                SpellingPart::HelperSubscriptOpen(subscript) if running => {
                    spelled.push_str(&helper(*subscript));
                    spelled.push('(');
                }
                SpellingPart::HelperSubscriptArguments => {
                    spelled.push_str(if running { ", " } else { "[" });
                }
                SpellingPart::HelperSubscriptClose => {
                    spelled.push(if running { ')' } else { ']' });
                }
                SpellingPart::Module(imported) => spelled.push_str(&module(imported)),
                SpellingPart::TellVariance(_)
                | SpellingPart::Project(UseSiteVariance::InOut)
                | SpellingPart::HelperSubscriptOpen(_)
                | SpellingPart::Told(_)
                | SpellingPart::Close => {}
            }
        }
        spelled
    }

    /// the expression as the source reads it, each symbol by the name the source declares
    /// it under — for a diagnostic, and for source a later transpile phase renames itself
    pub fn source(&self) -> String {
        self.written(
            ToOwned::to_owned,
            |_| String::new(),
            ToOwned::to_owned,
            true,
            false,
        )
    }
}

/// the first of the module-level names `reads` that does not hold, where `site` runs, the
/// binding the checker resolved — with what it holds instead
fn first_unbound_read<'a, 'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    reads: impl IntoIterator<Item = &'a Name>,
    site: RuntimeSite<'db>,
) -> Option<(Name, RuntimeBinding)> {
    reads.into_iter().find_map(|name| {
        let binding = runtime_binding(
            db,
            env,
            RuntimeRead::Global {
                name,
                range: site.range,
            },
            site.inferred,
        );
        (binding != RuntimeBinding::Bound).then(|| (name.clone(), binding))
    })
}
