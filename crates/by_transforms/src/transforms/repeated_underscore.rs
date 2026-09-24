//! the lowering of a parameter list that repeats `_`
//!
//! basedpython allows `def f(_, _, _): ...` as a shorthand for ignoring several
//! positional parameters. python rejects it with a duplicate-parameter error, so each
//! `_` after the first is renamed to a fresh `_<n>` (`_2`, `_3`, ...), skipping any
//! name the module spells, and a `/` after the last of them makes them positional-only:
//! the numbered name is the lowering's rather than the author's, so no call may spell
//! it. a method that overrides one takes the names the overridden method gives those
//! positions instead, and their kinds with them
//!
//! which names and where the `/` goes is ty's answer, the one its signature of the
//! definition is built from (`ty_python_semantic::types::repeated_underscore`), so a
//! call ty accepts is one the python accepts. a shape ty refuses is refused here too.
//! references to `_` inside the body are left alone and resolve to the first parameter

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_expr, walk_stmt};
use ruff_python_ast::visitor::{
    Visitor, walk_expr as visit_walk_expr, walk_stmt as visit_walk_stmt,
};
use ruff_python_ast::{Expr, ModModule, Parameter, Parameters, Stmt};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::types::repeated_underscore::{
    self as decision, UnderscoreLowering, UnderscoreRefusal, callable_parameter_slots,
    lower_repeated_underscores, lowered_parameter_names, parameter_slots,
};

pub(crate) use ty_python_semantic::types::repeated_underscore::{
    ParameterSlot, protocol_method_receiver,
};

use super::ast_driver::{AstPass, PassContext};
use crate::type_info::TypeInfo;

type Decision = Result<UnderscoreLowering, UnderscoreRefusal>;

/// how every parameter list of a module that repeats `_` is lowered, by the range of the
/// list — read off ty before any pass rewrites the syntax tree, so the passes that walk a
/// tree of their own can still ask
#[derive(Debug)]
pub struct UnderscoreLowerings {
    decisions: HashMap<TextRange, Decision>,
    /// whether the python the module targets has the `/` a repeated `_` needs
    slash: bool,
}

/// ty's answer for every parameter list in `suite` that repeats `_`, the module targeting a
/// python that has the `/` when `slash`
pub(crate) fn collect(suite: &[Stmt], types: &dyn TypeInfo, slash: bool) -> UnderscoreLowerings {
    struct Collector<'a> {
        types: &'a dyn TypeInfo,
        lowerings: UnderscoreLowerings,
    }
    impl Visitor<'_> for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &Stmt) {
            if let Stmt::FunctionDef(function) = stmt
                && repeats_underscore(&function.parameters)
                && let Some(decision) = self.types.repeated_underscore_lowering(function)
            {
                self.lowerings
                    .decisions
                    .insert(function.parameters.range, decision);
            }
            visit_walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Lambda(lambda) = expr
                && let Some(parameters) = lambda.parameters.as_deref()
                && let Some(decision) = standalone_lowering(parameters, self.lowerings.slash)
            {
                self.lowerings.decisions.insert(parameters.range, decision);
            }
            visit_walk_expr(self, expr);
        }
    }
    let mut collector = Collector {
        types,
        lowerings: UnderscoreLowerings {
            decisions: HashMap::new(),
            slash,
        },
    };
    collector.visit_body(suite);
    collector.lowerings
}

/// ty's answer for every parameter list in `suite` that repeats `_`, for a native build of
/// the module `model` is of. that build runs on the interpreter it is compiled for, which
/// has the `/`
///
/// the native build publishes the same definitions the transpiled module does, so it
/// reads the same answer: [`WrittenNames::with_lowerings`] these, and every name and `/`
/// it writes is the transpiler's
pub fn repeated_underscore_lowerings(
    model: &ty_python_semantic::SemanticModel<'_>,
    suite: &[Stmt],
) -> UnderscoreLowerings {
    collect(suite, model, true)
}

/// how `parameters` is lowered read on its own, with no method it overrides and no receiver
/// to consult. a lambda has neither, so this is the answer ty gives a lambda too
fn standalone_lowering(parameters: &Parameters, slash: bool) -> Option<Decision> {
    lower_repeated_underscores(&parameter_slots(parameters), false, None, |_| false, slash)
}

fn repeats_underscore(parameters: &Parameters) -> bool {
    parameters
        .iter()
        .filter(|parameter| parameter.name() == "_")
        .nth(1)
        .is_some()
}

/// the names the module spells as written, and how the parameter lists in it that repeat
/// `_` are lowered — every name the lowering writes that the source did not
///
/// a repeated `_` is numbered to a name outside the ones the module spells, so the
/// parameter cannot shadow a name the function reads from an enclosing scope:
///
/// ```by
/// _2 = [9]
///
/// def f(_: int, _: int) -> list[int]:
///     return _2  # the module's `_2`, so the second parameter is `_3`
/// ```
///
/// without the lowerings ty decided, a repeated `_` is numbered and nothing takes a name
/// from an overridden method
#[derive(Clone, Copy, Debug)]
pub struct WrittenNames<'src> {
    names: decision::WrittenNames<'src>,
    lowerings: Option<&'src UnderscoreLowerings>,
    /// every name the module's code binds or reads, which a typing name a lowering writes
    /// has to stay clear of
    code: Option<&'src CodeNames>,
    /// the names the module's `private` symbols are emitted under, which no other name a
    /// lowering writes may take
    private: Option<&'src PrivateNames>,
    /// the names of the classes the module's lowerings synthesize for a shape
    synthesized: Option<&'src SynthesizedClasses>,
}

/// the name of each class a lowering synthesizes for a shape — an anonymous named tuple, a
/// typed dict literal, an inline protocol, a callable type python has no spelling for —
/// decided once for the module, however many lowerings meet the shape and in whatever order
///
/// a class is named after a 32-bit hash of its shape, `_TypedDict_1a2b3c4d`, and two shapes
/// can hash alike. the first of them takes the name and each later one the next name nothing
/// has, `_TypedDict_1a2b3c4d2`, so two shapes are never declared under one name, which would
/// leave whichever class came last read for both
#[derive(Default)]
pub(crate) struct SynthesizedClasses {
    /// each shape named so far, by the prefix and hash its name was made from
    named: RefCell<HashMap<(&'static str, u32), NamedShapes>>,
}

/// the shapes named after one prefix and hash, each with its name
type NamedShapes = Vec<(Box<dyn std::any::Any>, String)>;

impl std::fmt::Debug for SynthesizedClasses {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let named = self.named.borrow();
        f.debug_set()
            .entries(named.values().flatten().map(|(_, name)| name))
            .finish()
    }
}

impl SynthesizedClasses {
    /// the 32 bits of `shape`'s hash a synthesized class is named after
    pub(crate) fn hash_of(shape: &impl std::hash::Hash) -> u32 {
        use std::hash::Hasher;

        let mut hasher = std::hash::DefaultHasher::new();
        shape.hash(&mut hasher);
        // the low half: eight hex digits keep the name readable, and a collision is
        // numbered apart
        let [a, b, c, d, ..] = hasher.finish().to_le_bytes();
        u32::from_le_bytes([a, b, c, d])
    }

    /// the name of the class declaring `shape`, its hash spelled after `prefix`, which
    /// `written` has no other use of
    fn name<S: std::hash::Hash + PartialEq + Clone + 'static>(
        &self,
        written: WrittenNames,
        prefix: &'static str,
        shape: &S,
        hash: u32,
    ) -> String {
        let mut named = self.named.borrow_mut();
        let bucket = named.entry((prefix, hash)).or_default();
        if let Some((_, name)) = bucket
            .iter()
            .find(|(named, _)| named.downcast_ref::<S>() == Some(shape))
        {
            return name.clone();
        }
        // a numbered name is longer than every name made from a hash alone, so only the
        // names in this bucket can be one it would take
        let name = written.fresh_outside(&format!("{prefix}{hash:08x}"), |candidate| {
            bucket.iter().any(|(_, name)| name == candidate)
        });
        bucket.push((Box::new(shape.clone()), name.clone()));
        name
    }
}

/// the name each module-level `private` symbol is emitted under
///
/// `private` hides a symbol by giving it a leading underscore, `private def helper` becoming
/// `_helper`. when the module already has that name — its own `_helper`, a runtime helper's,
/// or one a lowering wrote before this — the renamed symbol would take it over, so it is
/// emitted under the next name nothing has, `_helper2`, `_helper3`, …:
///
/// ```by
/// private def force_unwrap(x: int) -> int:  # `_force_unwrap` is the helper `x!` calls
///     return x + 100
/// ```
///
/// a name the author already wrote with a leading underscore is emitted as written
#[derive(Debug, Default)]
pub(crate) struct PrivateNames {
    emitted: HashMap<String, String>,
    /// the names in `emitted` a symbol's own spelling does not account for, which every
    /// name a lowering takes afterwards has to stay clear of
    claimed: std::collections::HashSet<String>,
}

impl PrivateNames {
    /// how each of `symbols`, the module-level names a module declares `private`, is emitted.
    /// `sources` are the texts of the module the lowering reads — as written, and as the
    /// passes that run before the name is decided left it. `reserved` are names the lowering
    /// binds at module level that were decided before these, which the module spells nowhere
    /// either: the backing functions of its extensions
    pub(crate) fn decide<'a>(
        symbols: impl IntoIterator<Item = &'a str>,
        sources: &[&str],
        reserved: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let reserved: std::collections::HashSet<&str> = reserved.into_iter().collect();
        let mut symbols: Vec<&str> = symbols.into_iter().collect();
        // the order a name is taken in decides who gets `_helper` and who `_helper2`, so it
        // must not be the order of a hash set
        symbols.sort_unstable();
        symbols.dedup();
        let mut private = Self::default();
        for symbol in symbols {
            if symbol.starts_with('_') {
                private.emitted.insert(symbol.to_owned(), symbol.to_owned());
                continue;
            }
            let stem = format!("_{symbol}");
            let emitted = decision::WrittenNames::new("").fresh_outside(&stem, |candidate| {
                crate::runtime::defines(candidate)
                    || reserved.contains(candidate)
                    || private.claimed.contains(candidate)
                    || sources
                        .iter()
                        .any(|source| decision::WrittenNames::new(source).spells(candidate))
            });
            private.claimed.insert(emitted.clone());
            private.emitted.insert(symbol.to_owned(), emitted);
        }
        private
    }
}

/// how a module's own code binds each name, in any scope, which says whether a name a lowering
/// writes reads what the lowering means by it
///
/// a keyword the parser stands up as a name — the `final` of `final class`, the `cast` of
/// `x cast int` — binds nothing, and a read binds nothing either: a name only read is a builtin,
/// or a typing name basedpython imports implicitly, which is the lowering's own
#[derive(Debug, Default)]
pub(crate) struct CodeNames {
    bindings: std::collections::HashMap<String, Vec<Binding>>,
    /// whether the module imports `*` from somewhere, which binds names it never spells
    star_import: bool,
    /// each builtin the module's top level imports under a name of its own, with the names
    /// it is imported under — `from builtins import str as str2`
    builtin_aliases: std::collections::HashMap<String, Vec<String>>,
    /// the names of those imported by the imports a preamble is spliced after, which
    /// whatever is spliced there can read as it is defined
    /// ([`super::source_util::preamble_offset`])
    leading_builtin_aliases: std::collections::HashSet<String>,
}

/// one way a module binds a name
#[derive(Debug, Clone, PartialEq, Eq)]
enum Binding {
    /// `from <module> import <name>`, which binds the name to what `module` exports as it
    From(String),
    /// `from <module> import <name> as <other>`, which binds `other` to what `module`
    /// exports as `name`
    FromAs { module: String, name: String },
    /// `import <name>`, which binds the name to the module of that name
    Module,
    /// anything else
    Other,
}

impl CodeNames {
    /// the names `suite`, a module's statements as the author wrote them, binds
    pub(crate) fn of(suite: &[Stmt]) -> Self {
        use ruff_python_ast::visitor::source_order::{
            SourceOrderVisitor, walk_except_handler, walk_expr, walk_parameter, walk_pattern,
            walk_stmt, walk_type_param,
        };
        use ruff_python_ast::{ExceptHandler, Pattern, TypeParam};

        #[derive(Default)]
        struct Collect(CodeNames);
        impl Collect {
            fn bind(&mut self, name: &str, binding: Binding) {
                self.0
                    .bindings
                    .entry(name.to_owned())
                    .or_default()
                    .push(binding);
            }
        }
        impl<'a> SourceOrderVisitor<'a> for Collect {
            fn visit_stmt(&mut self, stmt: &'a Stmt) {
                match stmt {
                    Stmt::FunctionDef(function) => self.bind(&function.name, Binding::Other),
                    Stmt::ClassDef(class) => self.bind(&class.name, Binding::Other),
                    Stmt::Global(global) => {
                        for name in &global.names {
                            self.bind(name, Binding::Other);
                        }
                    }
                    Stmt::Nonlocal(nonlocal) => {
                        for name in &nonlocal.names {
                            self.bind(name, Binding::Other);
                        }
                    }
                    Stmt::Import(import) => {
                        for alias in &import.names {
                            match &alias.asname {
                                Some(asname) => self.bind(asname, Binding::Other),
                                None => {
                                    let root = alias.name.split('.').next().unwrap_or_default();
                                    self.bind(root, Binding::Module);
                                }
                            }
                        }
                    }
                    Stmt::ImportFrom(import) => {
                        for alias in &import.names {
                            if &*alias.name == "*" {
                                self.0.star_import = true;
                                continue;
                            }
                            match (&alias.asname, &import.module) {
                                (None, Some(module)) if import.level == 0 => {
                                    self.bind(&alias.name, Binding::From(module.to_string()));
                                }
                                (Some(asname), Some(module)) if import.level == 0 => self.bind(
                                    asname,
                                    Binding::FromAs {
                                        module: module.to_string(),
                                        name: alias.name.to_string(),
                                    },
                                ),
                                (asname, _) => self
                                    .bind(asname.as_ref().unwrap_or(&alias.name), Binding::Other),
                            }
                        }
                    }
                    _ => {}
                }
                walk_stmt(self, stmt);
            }

            fn visit_expr(&mut self, expr: &'a Expr) {
                match expr {
                    Expr::Name(name) if name.ctx.is_store() || name.ctx.is_del() => {
                        self.bind(&name.id, Binding::Other);
                    }
                    // the callee of `x cast int` is the keyword, not a name
                    Expr::Call(call) if call.cast_kind.is_some() => {
                        self.visit_arguments(&call.arguments);
                        return;
                    }
                    _ => {}
                }
                walk_expr(self, expr);
            }

            fn visit_parameter(&mut self, parameter: &'a Parameter) {
                self.bind(&parameter.name, Binding::Other);
                walk_parameter(self, parameter);
            }

            fn visit_except_handler(&mut self, handler: &'a ExceptHandler) {
                let ExceptHandler::ExceptHandler(handler_node) = handler;
                if let Some(name) = &handler_node.name {
                    self.bind(name, Binding::Other);
                }
                walk_except_handler(self, handler);
            }

            fn visit_pattern(&mut self, pattern: &'a Pattern) {
                let name = match pattern {
                    Pattern::MatchAs(pattern) => pattern.name.as_ref(),
                    Pattern::MatchStar(pattern) => pattern.name.as_ref(),
                    Pattern::MatchMapping(pattern) => pattern.rest.as_ref(),
                    _ => None,
                };
                if let Some(name) = name {
                    self.bind(name, Binding::Other);
                }
                walk_pattern(self, pattern);
            }

            fn visit_type_param(&mut self, type_param: &'a TypeParam) {
                self.bind(type_param.name(), Binding::Other);
                walk_type_param(self, type_param);
            }
        }

        let mut collect = Collect::default();
        // the statements before the first that is neither a docstring nor a leading import
        let mut leading = true;
        for (index, stmt) in suite.iter().enumerate() {
            collect.visit_stmt(stmt);
            let docstring = index == 0
                && matches!(stmt, Stmt::Expr(expr) if expr.value.is_string_literal_expr());
            leading &= docstring || super::source_util::is_leading_import(stmt);
            // an import in a nested scope binds its name there alone
            if let Stmt::ImportFrom(import) = stmt
                && import.level == 0
                && import.module.as_deref() == Some("builtins")
            {
                for alias in &import.names {
                    if let Some(asname) = &alias.asname {
                        collect
                            .0
                            .builtin_aliases
                            .entry(alias.name.to_string())
                            .or_default()
                            .push(asname.to_string());
                        if leading {
                            collect.0.leading_builtin_aliases.insert(asname.to_string());
                        }
                    }
                }
            }
        }
        collect.0
    }

    /// a name the module's top level imports the builtin `name` under, and binds to nothing
    /// else anywhere — the first of them, when there are several
    fn builtin_alias(&self, name: &str) -> Option<&str> {
        let binding = Binding::FromAs {
            module: "builtins".to_owned(),
            name: name.to_owned(),
        };
        self.builtin_aliases
            .get(name)?
            .iter()
            .find(|alias| self.only_binds(alias, &binding))
            .map(String::as_str)
    }

    /// whether every binding of `name` is `binding`, so a lowering can read the module's own
    /// `name` as the thing `binding` binds it to
    fn only_binds(&self, name: &str, binding: &Binding) -> bool {
        !self.star_import
            && self
                .bindings
                .get(name)
                .is_none_or(|bindings| bindings.iter().all(|bound| bound == binding))
    }
}

impl<'src> WrittenNames<'src> {
    /// the names spelled in `source`, the text of a module as it was written
    pub fn new(source: &'src str) -> Self {
        Self {
            names: decision::WrittenNames::new(source),
            lowerings: None,
            code: None,
            private: None,
            synthesized: None,
        }
    }

    /// these names, with `synthesized` the names of the classes the module's lowerings
    /// synthesize for a shape
    pub(crate) fn with_synthesized<'a>(
        self,
        synthesized: &'a SynthesizedClasses,
    ) -> WrittenNames<'a>
    where
        'src: 'a,
    {
        WrittenNames {
            synthesized: Some(synthesized),
            ..self
        }
    }

    /// these names, with `code` the names the module's code binds or reads
    pub(crate) fn with_code<'a>(self, code: &'a CodeNames) -> WrittenNames<'a>
    where
        'src: 'a,
    {
        WrittenNames {
            names: self.names,
            lowerings: self.lowerings,
            code: Some(code),
            private: self.private,
            synthesized: self.synthesized,
        }
    }

    /// these names, with `private` the names the module's `private` symbols are emitted under
    pub(crate) fn with_private<'a>(self, private: &'a PrivateNames) -> WrittenNames<'a>
    where
        'src: 'a,
    {
        WrittenNames {
            names: self.names,
            lowerings: self.lowerings,
            code: self.code,
            private: Some(private),
            synthesized: self.synthesized,
        }
    }

    /// these names, with `lowerings` saying how each parameter list that repeats `_` is
    /// lowered
    pub fn with_lowerings<'a>(self, lowerings: &'a UnderscoreLowerings) -> WrittenNames<'a>
    where
        'src: 'a,
    {
        WrittenNames {
            names: self.names,
            lowerings: Some(lowerings),
            code: self.code,
            private: self.private,
            synthesized: self.synthesized,
        }
    }

    /// a name the module spells nowhere, `stem` itself when the module does not spell it
    /// and `stem2`, `stem3`, … when it does
    ///
    /// a binding the lowering writes under a name the source wrote would shadow it — the
    /// `inner` a `decorator def` writes standing where an option of that name is declared,
    /// which the dispatcher then passed the dispatcher itself for
    ///
    /// nor is it a runtime helper's, or one a `private` symbol is emitted under: those are
    /// bound at module scope too, and the source never spells them
    pub(crate) fn fresh(self, stem: &str) -> String {
        self.names
            .fresh_outside(stem, |name| self.lowering_binds(name))
    }

    /// [`Self::fresh`], also passing over every name `taken` answers for
    fn fresh_outside(self, stem: &str, taken: impl Fn(&str) -> bool) -> String {
        self.names
            .fresh_outside(stem, |name| self.lowering_binds(name) || taken(name))
    }

    /// the name of the class a lowering synthesizes for `shape`, named after its hash
    /// behind `prefix` ([`SynthesizedClasses`]). without the module's decision, two shapes
    /// hashing alike share a name
    pub(crate) fn synthesized_class<S: std::hash::Hash + PartialEq + Clone + 'static>(
        self,
        prefix: &'static str,
        shape: &S,
    ) -> String {
        let hash = SynthesizedClasses::hash_of(shape);
        match self.synthesized {
            Some(synthesized) => synthesized.name(self, prefix, shape, hash),
            None => self.fresh(&format!("{prefix}{hash:08x}")),
        }
    }

    /// whether `name` is one a lowering cannot bind: the module spells it, a runtime
    /// helper is bound under it, or a `private` symbol is emitted under it
    pub(crate) fn taken(self, name: &str) -> bool {
        self.names.spells(name) || self.lowering_binds(name)
    }

    /// whether the module has `name` bound without spelling it: a runtime helper is bound
    /// under it, or a `private` symbol is emitted under it
    fn lowering_binds(self, name: &str) -> bool {
        crate::runtime::defines(name)
            || self
                .private
                .is_some_and(|private| private.claimed.contains(name))
    }

    /// the name `symbol`, a module-level name the module declares `private`, is emitted
    /// under ([`PrivateNames`]). without the module's decision, it gains a leading
    /// underscore unless it has one already
    pub(crate) fn module_private(self, symbol: &str) -> String {
        match self.private.and_then(|private| private.emitted.get(symbol)) {
            Some(emitted) => emitted.clone(),
            None => super::modifiers::module_private_name(symbol),
        }
    }

    /// the name a lowering writes `name`, which `module` exports, under — `typing`,
    /// `typing_extensions`, `collections.abc` and the like
    ///
    /// a module that binds `name` itself may bind it to anything at all — `Union = …` — so the
    /// lowering's is imported under a name the module does not spell ([`Self::import_from`]).
    /// one that binds it only by importing it from `module` has it already, and one that never
    /// binds it reads the lowering's. without the module's bindings, every name the module
    /// spells counts as bound
    pub(crate) fn imported(self, module: &str, name: &str) -> String {
        self.name_for(name, &Binding::From(module.to_owned()))
    }

    /// the name a lowering writes `name`, a builtin, under: the builtin's own unless the
    /// module binds that name to something else, anywhere — a parameter named `type`, a
    /// class attribute named `staticmethod` — and then a name of the lowering's own, which
    /// [`Self::builtin_imports`] binds
    ///
    /// a module that already imports the builtin under a name of its own has it read under
    /// that name. a lowering run over the output of an earlier one finds the earlier one's
    /// import there, so every lowering of a module reads a builtin under one name
    pub(crate) fn builtin(self, name: &str) -> String {
        if let Some(code) = self.code
            && !code.only_binds(name, &Binding::From("builtins".to_owned()))
            && let Some(alias) = code.builtin_alias(name)
            && !self.lowering_binds(alias)
        {
            return alias.to_owned();
        }
        self.imported("builtins", name)
    }

    /// the import that binds each builtin `output`, python a lowering wrote, reads under a
    /// name of the lowering's own ([`Self::builtin`]), on a python of `minor`
    ///
    /// such a name is spelled nowhere in the module, so only a lowering can have written it.
    /// one the module imports among the imports a preamble is spliced after has its import
    /// already, ahead of anything a lowering adds. imported anywhere else, it is imported
    /// again, since a definition spliced ahead of that import may read it as it is defined
    pub(crate) fn builtin_imports(self, output: &str, minor: u8) -> Vec<String> {
        let written = decision::WrittenNames::new(output);
        ruff_python_stdlib::builtins::python_builtins(minor, false)
            .filter_map(|name| {
                let local = self.builtin(name);
                let imported = self
                    .code
                    .is_some_and(|code| code.leading_builtin_aliases.contains(&local));
                (local != name && !imported && written.spells(&local))
                    .then(|| format!("from builtins import {name} as {local}"))
            })
            .collect()
    }

    /// the import that binds each of `names`, which `module` exports, to the name
    /// [`Self::imported`] answers for it
    pub(crate) fn import_from(self, module: &str, names: &[&str]) -> String {
        let names: Vec<String> = names
            .iter()
            .map(|name| {
                let local = self.imported(module, name);
                if local == *name {
                    local
                } else {
                    format!("{name} as {local}")
                }
            })
            .collect();
        format!("from {module} import {}", names.join(", "))
    }

    /// the name a lowering reads `module`, a module it reads attributes of, under, as
    /// [`Self::imported`] answers for an exported name
    pub(crate) fn imported_module(self, module: &str) -> String {
        self.name_for(module, &Binding::Module)
    }

    /// the import that binds `module` to the name [`Self::imported_module`] answers for it
    pub(crate) fn import_module(self, module: &str) -> String {
        let local = self.imported_module(module);
        if local == module {
            format!("import {module}")
        } else {
            format!("import {module} as {local}")
        }
    }

    fn name_for(self, name: &str, binding: &Binding) -> String {
        let free = self.code.map_or_else(
            || !self.names.spells(name),
            |code| code.only_binds(name, binding),
        );
        if free && !self.lowering_binds(name) {
            name.to_owned()
        } else {
            // the name itself is taken even when the module never spells it: a star import
            // binds it unseen
            self.names.fresh_outside(name, |candidate| {
                candidate == name || self.lowering_binds(candidate)
            })
        }
    }

    /// how `parameters` is lowered when it repeats `_`. without ty's answer, the list is
    /// read on its own, as a lambda's is: it takes no name from a method it overrides
    /// whether the python the module targets has the `/` a repeated `_` needs. without ty's
    /// answers it is assumed to
    fn slash(self) -> bool {
        self.lowerings.is_none_or(|lowerings| lowerings.slash)
    }

    fn lowering(self, parameters: &Parameters) -> Option<Decision> {
        match self.lowerings {
            Some(lowerings) => lowerings.decisions.get(&parameters.range).cloned(),
            None => standalone_lowering(parameters, true),
        }
    }

    /// the name each of `parameters` binds in the python, in declaration order
    fn names(self, parameters: &Parameters) -> Vec<Name> {
        let slots = parameter_slots(parameters);
        match self.lowering(parameters) {
            Some(Ok(lowering)) => lowering.names(&slots, self.names),
            _ => lowered_parameter_names(&slots, self.names),
        }
    }
}

/// the name `parameter`, one of `parameters`, binds in the python a basedpython
/// definition lowers to. `written` is the module the definition is in
///
/// a native build publishes the same definition, and has to answer to the same
/// names: a keyword argument, the forwarder's signature and `__code__` all spell
/// them
pub fn python_parameter_name(
    parameters: &Parameters,
    parameter: &Parameter,
    written: WrittenNames,
) -> Name {
    parameters
        .iter()
        .zip(written.names(parameters))
        .find(|(candidate, _)| std::ptr::eq(candidate.as_parameter(), parameter))
        .map_or_else(|| parameter.name.id.clone(), |(_, name)| name)
}

/// the parameters a callable type is declared with in the python — the `__call__` of the
/// protocol a callable type naming its parameters lowers to, or the method of an inline
/// protocol
pub(crate) struct LoweredCallableParameters {
    /// the name each written parameter is declared under, in order, the receiver of a
    /// protocol's method left out
    pub(crate) names: Vec<Name>,
    /// how many of the written parameters the `/` comes after, when a repeated `_` puts
    /// one there
    pub(crate) slash: Option<usize>,
}

/// how the parameters of `ct` are declared, `method` saying it is the signature of an
/// inline protocol's method. a repeated `_` is lowered as in a `def`: every `_` after the
/// first is numbered, and made positional-only by a `/` after the last of them — ty's
/// answer for the same callable type. a shape it refuses is reported by the pass that
/// lowers parameter lists, and is written here with its `_`s numbered alone
pub(crate) fn lowered_callable_parameters(
    ct: &ruff_python_ast::ExprCallableType,
    method: bool,
    written: WrittenNames,
) -> LoweredCallableParameters {
    let slots = callable_parameter_slots(ct, method);
    let spelled: Vec<(ParameterSlot, &str)> = slots
        .iter()
        .map(|(slot, name)| (*slot, name.as_str()))
        .collect();
    let method_receiver = method && protocol_method_receiver(ct).is_some();
    let receivers = usize::from(method_receiver) + usize::from(ct.receiver.is_some());
    let (mut names, positional_only) = match lower_repeated_underscores(
        &spelled,
        method_receiver,
        None,
        |_| false,
        written.slash(),
    ) {
        Some(Ok(lowering)) => (
            lowering.names(&spelled, written.names),
            lowering.positional_only(),
        ),
        _ => (lowered_parameter_names(&spelled, written.names), 0),
    };
    LoweredCallableParameters {
        names: names.split_off(receivers.min(names.len())),
        slash: positional_only
            .checked_sub(receivers)
            .filter(|&written| written > 0),
    }
}

/// how many of the leading positional `parameters` the python takes by position alone
pub fn positional_only_count(parameters: &Parameters, written: WrittenNames) -> usize {
    match written.lowering(parameters) {
        Some(Ok(lowering)) => lowering.positional_only(),
        _ => parameters.posonlyargs.len(),
    }
}

/// the name a read of `_` in the body of a definition with `parameters` means, when the
/// lowering binds `_` to anything but the first `_`: the first `_` took a name from the
/// method the definition overrides, and a read of `_` is still the first `_`'s value
pub(crate) fn rebound_underscore(parameters: &Parameters, written: WrittenNames) -> Option<Name> {
    let Some(Ok(lowering)) = written.lowering(parameters) else {
        return None;
    };
    if !lowering.is_inherited() {
        return None;
    }
    parameters
        .iter()
        .zip(written.names(parameters))
        .find(|(parameter, _)| parameter.name() == "_")
        .map(|(_, name)| name)
        .filter(|name| name != "_")
}

/// whether `body` reads `_`, anywhere in it
pub(crate) fn reads_underscore(body: &[Stmt]) -> bool {
    struct Reads {
        found: bool,
    }
    impl Visitor<'_> for Reads {
        fn visit_stmt(&mut self, stmt: &Stmt) {
            if let Stmt::AugAssign(node) = stmt
                && matches!(node.target.as_ref(), Expr::Name(name) if name.id == "_")
            {
                self.found = true;
            }
            visit_walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Name(name) = expr
                && name.id == "_"
                && !name.ctx.is_store()
            {
                self.found = true;
            }
            visit_walk_expr(self, expr);
        }
    }
    let mut reads = Reads { found: false };
    reads.visit_body(body);
    reads.found
}

pub(crate) struct RepeatedUnderscore<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
}

impl<'src> RepeatedUnderscore<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>) -> Self {
        Self { source, written }
    }
}

impl AstPass for RepeatedUnderscore<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::RepeatedUnderscore)
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        for (index, stmt) in module.body.iter_mut().enumerate() {
            let lowerer = Lowerer {
                pass: self,
                changed: Cell::new(false),
                edits: RefCell::default(),
                errors: RefCell::default(),
                method_signatures: RefCell::default(),
            };
            lowerer.visit_stmt(stmt);
            if lowerer.changed.get() {
                ctx.changed.push(index);
            }
            ctx.text_edits.extend(lowerer.edits.into_inner());
            ctx.errors.extend(lowerer.errors.into_inner());
        }
    }
}

struct Lowerer<'a, 'src> {
    pass: &'a RepeatedUnderscore<'src>,
    changed: Cell<bool>,
    edits: RefCell<Vec<(TextRange, String)>>,
    errors: RefCell<Vec<String>>,
    /// the signatures of the inline protocol methods met so far, which are not callable
    /// types of their own
    method_signatures: RefCell<Vec<TextRange>>,
}

impl Lowerer<'_, '_> {
    fn lower(&self, params: &mut Parameters, function: Option<&str>) {
        let Some(decision) = self.pass.written.lowering(params) else {
            return;
        };
        let lowering = match decision {
            Ok(lowering) => lowering,
            Err(refusal) => {
                let owner =
                    function.map_or_else(|| "a lambda".to_owned(), |name| format!("`{name}`"));
                self.errors.borrow_mut().push(format!(
                    "{} in {owner}: {}",
                    refusal.message(),
                    refusal.help()
                ));
                return;
            }
        };
        self.place_slash(params, lowering.positional_only());
        let names = self.pass.written.names(params);
        let parameters = params
            .posonlyargs
            .iter_mut()
            .chain(params.args.iter_mut())
            .map(|parameter| &mut parameter.parameter)
            .chain(params.vararg.as_deref_mut())
            .chain(
                params
                    .kwonlyargs
                    .iter_mut()
                    .map(|parameter| &mut parameter.parameter),
            )
            .chain(params.kwarg.as_deref_mut());
        for (parameter, name) in parameters.zip(names) {
            if parameter.name.id != name {
                parameter.name.id = name;
                self.changed.set(true);
            }
        }
    }

    /// report the parameters of `callable` when they repeat `_` in a shape the lowering
    /// refuses, `owner` naming what they belong to
    fn refuse_callable(
        &self,
        callable: &ruff_python_ast::ExprCallableType,
        method: bool,
        owner: &str,
    ) {
        let slots = callable_parameter_slots(callable, method);
        let spelled: Vec<(ParameterSlot, &str)> = slots
            .iter()
            .map(|(slot, name)| (*slot, name.as_str()))
            .collect();
        let receiver = method && protocol_method_receiver(callable).is_some();
        if let Some(Err(refusal)) = lower_repeated_underscores(
            &spelled,
            receiver,
            None,
            |_| false,
            self.pass.written.slash(),
        ) {
            self.errors.borrow_mut().push(format!(
                "{} in {owner}: {}",
                refusal.message(),
                refusal.help()
            ));
        }
    }

    /// move the `/` to after the first `positional_only` parameters, as edits of the
    /// source. the header is where the passes before this one made their edits, and
    /// printing it from the tree would drop them
    fn place_slash(&self, params: &Parameters, positional_only: usize) {
        let written = params.posonlyargs.len();
        if positional_only <= written {
            return;
        }
        let Some(last) = params
            .posonlyargs
            .iter()
            .chain(&params.args)
            .nth(positional_only - 1)
        else {
            return;
        };
        let mut edits = self.edits.borrow_mut();
        if let (Some(before), Some(after)) = (params.posonlyargs.last(), params.args.first()) {
            let gap = TextRange::new(before.end(), after.start());
            if let Some(slash) = SimpleTokenizer::new(self.pass.source, gap)
                .find(|token| token.kind() == SimpleTokenKind::Slash)
            {
                edits.push((TextRange::new(slash.start(), after.start()), String::new()));
            }
        }
        edits.push((TextRange::empty(last.end()), ", /".to_owned()));
    }
}

impl Transformer for Lowerer<'_, '_> {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            let name = f.name.id.to_string();
            self.lower(&mut f.parameters, Some(&name));
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        if let Expr::Lambda(l) = expr
            && let Some(params) = l.parameters.as_deref_mut()
        {
            self.lower(params, None);
        }
        // a callable type or an inline protocol's method is written as a `__call__` or a
        // method by the lowerings of those types, from the same answer. a shape it refuses
        // is refused here, where every other parameter list's is
        match expr {
            Expr::ProtocolMethod(method) => {
                if let Expr::CallableType(signature) = method.signature.as_ref() {
                    self.method_signatures.borrow_mut().push(signature.range);
                    self.refuse_callable(signature, true, &format!("`{}`", method.name.id));
                }
            }
            Expr::CallableType(callable)
                if !self.method_signatures.borrow().contains(&callable.range) =>
            {
                self.refuse_callable(callable, false, "a callable type");
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use super::{SynthesizedClasses, WrittenNames};
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    /// a shape every other one of its kind hashes alike with
    #[derive(Clone, PartialEq)]
    struct Collides(&'static str);

    impl std::hash::Hash for Collides {
        fn hash<H: std::hash::Hasher>(&self, _: &mut H) {}
    }

    /// a shape keeps the name it was first given, and one hashing alike with it takes the
    /// next name the module has no use of, whichever lowering asks
    #[test]
    fn shapes_hashing_alike_are_named_apart_once_for_the_module() {
        let stem = format!("_S_{:08x}", SynthesizedClasses::hash_of(&Collides("")));
        // the module spells the name the second shape would otherwise take
        let source = format!("{stem}2 = 1\n");
        let synthesized = SynthesizedClasses::default();
        let one = WrittenNames::new(&source).with_synthesized(&synthesized);
        let other = WrittenNames::new(&source).with_synthesized(&synthesized);
        assert_eq!(one.synthesized_class("_S_", &Collides("a")), stem);
        assert_eq!(
            other.synthesized_class("_S_", &Collides("b")),
            format!("{stem}3")
        );
        assert_eq!(other.synthesized_class("_S_", &Collides("a")), stem);
        assert_eq!(
            one.synthesized_class("_S_", &Collides("c")),
            format!("{stem}4")
        );
        assert_eq!(
            one.synthesized_class("_S_", &Collides("b")),
            format!("{stem}3")
        );
        // another kind of class is named apart by its prefix alone
        assert_eq!(
            one.synthesized_class("_T_", &Collides("b")),
            format!("_T_{:08x}", SynthesizedClasses::hash_of(&Collides("")))
        );
    }

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    fn refused(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap_err()
    }

    fn check_at(input: &str, expected: &str, min_version: crate::PythonVersion) {
        let config = Config {
            min_version,
            ..Config::test_default()
        };
        assert_eq!(transpile(input, &config).unwrap(), expected);
    }

    /// a typing name a lowering writes is imported under a name the module does not spell
    /// when the module binds that name to something of its own
    #[test]
    fn a_typing_name_the_module_binds_is_imported_under_another_name() {
        check_at(
            indoc! {"
                Union = 1
                x = list[int?]
            "},
            indoc! {"
                from __future__ import annotations
                from typing import Union as Union2
                Union = 1
                x = list[Union2[int, None]]
            "},
            crate::PythonVersion::PY39,
        );
    }

    /// a module that binds the name only by importing it from where the lowering imports it
    /// has the lowering's already
    #[test]
    fn a_typing_name_the_module_imports_from_the_same_module_is_the_modules_own() {
        check(
            indoc! {"
                from typing import Literal
                x: 1 | 2 = 1
            "},
            indoc! {"
                from typing import Literal
                x: Literal[1, 2] = 1
            "},
        );
    }

    /// the same name exported by another module need not be the same object
    #[test]
    fn a_typing_name_the_module_imports_from_elsewhere_is_not() {
        check(
            indoc! {"
                from typing_extensions import Literal
                x: 1 | 2 = 1
            "},
            indoc! {"
                from typing import Literal as Literal2
                from typing_extensions import Literal
                x: Literal2[1, 2] = 1
            "},
        );
    }

    /// a star import binds names the module never spells
    #[test]
    fn a_star_import_takes_every_typing_name() {
        check(
            indoc! {"
                from os.path import *
                x: 1 | 2 = 1
            "},
            indoc! {"
                from typing import Literal as Literal2
                from os.path import *
                x: Literal2[1, 2] = 1
            "},
        );
    }

    /// a keyword the parser writes as a name binds nothing, and a parameter is a binding like
    /// any other
    #[test]
    fn a_keyword_binds_no_typing_name() {
        check(
            indoc! {"
                final class A: ...
                def f(Literal: int) -> None: ...
                x: 1 | 2 = 1
            "},
            indoc! {"
                from typing import Literal as Literal2, final
                @final
                class A: ...
                def f(Literal: int) -> None: ...
                x: Literal2[1, 2] = 1
            "},
        );
    }

    #[test]
    fn two_underscores() {
        check(
            "def f(_, _):\n    print(_)\n",
            "def f(_, _2, /):\n    print(_)\n",
        );
    }

    /// the `/` goes after the last `_`, so a named parameter after it keeps its keyword
    #[test]
    fn three_underscores() {
        check(
            "def g(_, _, _, x):\n    return _\n",
            "def g(_, _2, _3, /, x):\n    return _\n",
        );
    }

    #[test]
    fn lambda_underscores() {
        check("f = lambda _, _: 1\n", "f = lambda _, _2, /: 1\n");
    }

    #[test]
    fn nested_function() {
        check(
            "def outer(_, _):\n    def inner(_, _):\n        return _\n    return _\n",
            "def outer(_, _2, /):\n    def inner(_, _2, /):\n        return _\n    return _\n",
        );
    }

    /// a method's receiver is never passed by keyword, so the `/` may pass it
    #[test]
    fn a_method_receiver_is_made_positional_only() {
        check(
            "class A:\n    def f(self, _, _):\n        pass\n",
            "class A:\n    def f(self, _, _2, /):\n        pass\n",
        );
    }

    #[test]
    fn a_default_stays_before_the_slash() {
        check(
            "def f(_: int, _: int = 1) -> None: ...\n",
            "def f(_: int, _2: int = 1, /) -> None: ...\n",
        );
    }

    /// a `/` the source wrote before the last `_` moves after it
    #[test]
    fn a_written_slash_moves_after_the_last_underscore() {
        check(
            "def f(a, /, _, _):\n    return a\n",
            "def f(a, _, _2, /):\n    return a\n",
        );
    }

    #[test]
    fn a_written_slash_after_the_last_underscore_stays() {
        check(
            "def h(a, _, b, _, /):\n    return a + b\n",
            "def h(a, _, b, _2, /):\n    return a + b\n",
        );
    }

    /// `*_` is reached by no keyword, so nothing before it has to be positional-only
    #[test]
    fn vararg_underscore() {
        check(
            "def f(a, _, *_):\n    return _\n",
            "def f(a, _, *_2):\n    return _\n",
        );
    }

    /// a `/` would make `a` and `b` positional-only too, which is the author's to write
    #[test]
    fn a_named_parameter_before_the_last_underscore_is_refused() {
        assert_eq!(
            refused("def h(a, _, b, _):\n    return a + b\n"),
            "the repeated `_` parameters after `a` make it positional-only in `h`: write the `/` \
             after the last `_` to make them positional-only"
        );
        assert_eq!(
            refused("f = lambda a, _, _: a\n"),
            "the repeated `_` parameters after `a` make it positional-only in a lambda: write the \
             `/` after the last `_` to make them positional-only"
        );
    }

    #[test]
    fn a_keyword_only_repeated_underscore_is_refused() {
        assert_eq!(
            refused("def f(_, *, _):\n    pass\n"),
            "a repeated `_` parameter cannot be keyword-only in `f`: only a keyword reaches it, \
             and it has no name of its own: move it before the `*`, or name it"
        );
    }

    /// the numbered name of a repeated `_` is a local of the function, so a name the
    /// module binds under it would read the argument instead. python's answer here is
    /// `[9]`, and the second parameter answered it — in `g`'s body, and in the guard
    /// that re-evaluates `f`'s mutable default there
    #[test]
    fn a_numbered_name_skips_a_name_the_module_binds() {
        check(
            "_2: list[int] = [9]\n\ndef f(_: int, _: int, x: list[int] = _2) -> list[int]:\n    return x\n\ndef g(_: int, _: int) -> list[int]:\n    return _2\n",
            "from typing import Any\n_MISSING: Any = object()\n_2: list[int] = [9]\n\ndef f(_: int, _3: int, /, x: list[int] = _MISSING) -> list[int]:\n    if x is _MISSING:\n        x = _2\n    return x\n\ndef g(_: int, _3: int, /) -> list[int]:\n    return _2\n",
        );
    }

    /// what the module spells is read off its text, so a name reached any other way —
    /// through `globals()`, or as an attribute — is skipped too, and so is one written
    /// only in a comment
    #[test]
    fn a_numbered_name_skips_a_name_spelled_anywhere() {
        check(
            "def f(_, _):\n    return globals()[\"_2\"]\n",
            "def f(_, _3, /):\n    return globals()[\"_2\"]\n",
        );
        check(
            "def f(_, _, _):\n    return self._3  # _2\n",
            "def f(_, _4, _5, /):\n    return self._3  # _2\n",
        );
    }

    /// a name is spelled only as a whole: `__2`, `a_2` and `_2b` spell no `_2`
    #[test]
    fn a_longer_name_does_not_spell_a_numbered_one() {
        check(
            "__2 = a_2 = _2b = 1\ndef f(_, _):\n    return __2 + a_2 + _2b\n",
            "__2 = a_2 = _2b = 1\ndef f(_, _2, /):\n    return __2 + a_2 + _2b\n",
        );
    }

    /// a name the source gives a parameter is not a repeated `_`, so the numbering skips it
    #[test]
    fn existing_collision() {
        check(
            "def f(_, _2, _, /):\n    return _\n",
            "def f(_, _2, _3, /):\n    return _\n",
        );
    }

    #[test]
    fn single_underscore_unchanged() {
        unchanged("def f(_):\n    return _\n");
    }

    #[test]
    fn no_underscore_unchanged() {
        unchanged("def f(a, b):\n    return a + b\n");
    }

    /// an override takes the names the method it overrides gives those positions, so a
    /// call that passes them by keyword works on it as it does on the base
    #[test]
    fn an_override_takes_the_base_names() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, y: int) -> None: ...

                class B(A):
                    override def f(self, _: int, _: int) -> None: ...
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, y: int) -> None: ...

                class B(A):
                    @override
                    def f(self, x: int, y: int) -> None: ...
            "},
        );
    }

    /// a base parameter that is positional-only stays so
    #[test]
    fn an_override_keeps_a_positional_only_base_parameter() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, /, y: int) -> None: ...

                class B(A):
                    override def f(self, _: int, _: int) -> None: ...
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, /, y: int) -> None: ...

                class B(A):
                    @override
                    def f(self, x: int, /, y: int) -> None: ...
            "},
        );
    }

    /// no parameter is named `_` once they take the base's names, so a body that reads `_`
    /// is handed the first of them, as it was before
    #[test]
    fn an_override_that_reads_underscore_binds_it() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return _
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    @override
                    def f(self, x: int, y: int) -> int:
                        _ = x
                        return _
            "},
        );
    }

    /// a base that repeats `_` numbers its own, and the override takes those names. its
    /// first is `_`, which the body's `_` already reads
    #[test]
    fn an_override_of_numbered_underscores_keeps_them() {
        check(
            indoc! {"
                class A:
                    def f(self, _: int, _: int) -> int:
                        return _

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return _
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, _: int, _2: int, /) -> int:
                        return _

                class B(A):
                    @override
                    def f(self, _: int, _2: int, /) -> int:
                        return _
            "},
        );
    }

    /// a base parameter its author named `_` is reached by that keyword, so the `_` in its
    /// position keeps the name
    #[test]
    fn a_base_parameter_named_underscore_keeps_it() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, _: int) -> int:
                        return x

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return _
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, _: int) -> int:
                        return x

                class B(A):
                    @override
                    def f(self, x: int, _: int) -> int:
                        _ = x
                        return _
            "},
        );
    }

    /// the name an override would take from its base is one its body reads from an
    /// enclosing scope, where the parameter would shadow it
    #[test]
    fn an_inherited_name_the_body_reads_is_refused() {
        assert_eq!(
            refused(indoc! {"
                x = 1

                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return x
            "}),
            "a repeated `_` parameter named `x` shadows a `x` the body reads in `f`: the \
             parameter takes its name from the method it overrides; rename the `x` the body reads"
        );
    }
}
