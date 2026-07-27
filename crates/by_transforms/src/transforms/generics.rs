use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef, StmtTypeAlias, TypeParam};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::config::Config;
use crate::transforms::callable::lower_type_expr_full;
use crate::type_info::TypeInfo;
use ruff_python_ast::PythonVersion;

/// Polyfills PEP 695 generic syntax (Python 3.12+) and `type` alias statements.
///
/// - `class Foo[T, S](Base):` → `class Foo(Base, Generic[_T, _S]):` + `TypeVar` defs
/// - `def f[T](x: T) -> T:` → `def f(x: T) -> T:` + `TypeVar` defs
/// - `type Alias = T` → `Alias: TypeAlias = T`
pub(crate) struct GenericPolyfill<'src> {
    source: &'src str,
    types: &'src dyn TypeInfo,
    config: Config,
    pub(crate) edits: Vec<Fix>,
    // Imports to inject at the top of the file.
    pub(crate) needed_imports: ImportNeeds,
    /// `TypeVar` definitions already emitted at module scope. Polyfilling each
    /// generic class/function emits its own `_T = TypeVar("_T")` line; without
    /// dedup, a module with several generics over the same name produces
    /// repeated identical declarations (and an `F811 redefinition` warning).
    emitted_typevar_defs: std::collections::HashSet<String>,
    /// names already bound to a `TypeVar` at module scope along with the
    /// arguments used. when a later class needs a `TypeVar` with the same name
    /// but different shape (different bound/default/variance) we generate a
    /// fresh suffix to avoid shadowing the earlier definition
    emitted_typevar_signatures: std::collections::HashMap<String, String>,
    /// counter for fresh-suffix typevar names (`_T_2`, `_T_3`, …)
    typevar_suffix_counter: usize,
    /// names of classes/functions whose first type parameter has a top-parameters
    /// bound (i.e. `class A[P: (*: *, **: *)]`). subscript sites for these
    /// targets get tuple slices rewritten to list form so paramspec
    /// substitution at runtime accepts them
    parameters_targets: HashSet<String>,
    /// set when a Parameters spec lowering used `Any` for a named-only field
    pub(crate) needed_imports_any: bool,
    /// generic class name → its `T`→`_T` rename map. based-enum variants lower
    /// to module-level subclasses of the enum (`class _Tree_Node(Tree)`) that
    /// reference the enum's type params in their field annotations; those refs
    /// sit outside the enum body, so they are renamed here using the enum's map
    generic_class_renames: HashMap<String, HashMap<String, String>>,
    /// `private type X = …` aliases in the module, as `X` → `_X`. `modifiers`
    /// renames these globally, but a reference sitting inside a polyfilled
    /// alias value is subsumed by this pass's whole-statement replacement, so
    /// the rename has to be reapplied there
    private_aliases: HashMap<String, String>,
    /// `(range, rendered)` for every symbolic fold in the module. A fold inside a
    /// statement this pass replaces wholesale is dropped unless spliced in here
    symbolic_substitutions: Vec<(TextRange, String)>,
}

#[derive(Default)]
#[expect(clippy::struct_excessive_bools)]
pub(crate) struct ImportNeeds {
    pub(crate) typevar: bool,
    pub(crate) generic: bool,
    pub(crate) typevar_tuple: bool,
    pub(crate) unpack: bool,
    pub(crate) paramspec: bool,
    pub(crate) typealias_type: bool,
    pub(crate) typevar_needs_ext: bool, // TypeVar(default=) on < 3.13
}

impl ImportNeeds {
    /// Build the import lines to prepend to the file.
    pub(crate) fn into_lines(self) -> Vec<String> {
        let mut lines = Vec::new();

        let mut typing_names: Vec<&str> = Vec::new();
        let mut ext_names: Vec<&str> = Vec::new();

        if self.typevar {
            if self.typevar_needs_ext {
                ext_names.push("TypeVar");
            } else {
                typing_names.push("TypeVar");
            }
        }
        if self.typevar_tuple {
            typing_names.push("TypeVarTuple");
        }
        if self.unpack {
            typing_names.push("Unpack");
        }
        if self.paramspec {
            typing_names.push("ParamSpec");
        }
        if self.generic {
            typing_names.push("Generic");
        }
        if self.typealias_type {
            ext_names.push("TypeAliasType");
        }

        if !typing_names.is_empty() {
            lines.push(format!("from typing import {}", typing_names.join(", ")));
        }
        if !ext_names.is_empty() {
            lines.push(format!(
                "from typing_extensions import {}",
                ext_names.join(", ")
            ));
        }

        lines
    }
}

impl<'src> GenericPolyfill<'src> {
    pub(crate) fn new(
        source: &'src str,
        types: &'src dyn TypeInfo,
        config: Config,
        symbolic_substitutions: Vec<(TextRange, String)>,
    ) -> Self {
        Self {
            source,
            types,
            config,
            edits: Vec::new(),
            needed_imports: ImportNeeds::default(),
            emitted_typevar_defs: std::collections::HashSet::new(),
            emitted_typevar_signatures: std::collections::HashMap::new(),
            typevar_suffix_counter: 0,
            parameters_targets: HashSet::new(),
            needed_imports_any: false,
            generic_class_renames: HashMap::new(),
            private_aliases: HashMap::new(),
            symbolic_substitutions,
        }
    }

    /// the symbolic folds that fall inside `range`, ready to splice into a
    /// replacement that subsumes them
    fn substitutions_within(&self, range: TextRange) -> Vec<(TextRange, String)> {
        self.symbolic_substitutions
            .iter()
            .filter(|(folded, _)| range.contains_range(*folded))
            .cloned()
            .collect()
    }

    /// pre-scan for `private type` aliases so a reference inside a later
    /// alias's value can be renamed as the value is re-rendered
    fn collect_private_aliases(&mut self, stmts: &[Stmt]) {
        struct Collect<'a>(&'a mut HashMap<String, String>);
        impl<'ast> Visitor<'ast> for Collect<'_> {
            fn visit_stmt(&mut self, stmt: &'ast Stmt) {
                if let Stmt::TypeAlias(alias) = stmt
                    && alias.is_private
                    && let Expr::Name(name) = alias.name.as_ref()
                {
                    self.0.insert(name.id.to_string(), format!("_{}", name.id));
                }
                ruff_python_ast::visitor::walk_stmt(self, stmt);
            }
        }
        let mut collect = Collect(&mut self.private_aliases);
        for stmt in stmts {
            collect.visit_stmt(stmt);
        }
    }

    /// whether the target can keep this parameter list as native syntax:
    /// pep 695 lists need 3.12+, and a pep 696 default (`[T = int]`) bumps
    /// the requirement to 3.13. a defaulted list on a 3.12 target polyfills
    /// the declaration exactly like pre-3.12 code
    fn supports_native_type_params(&self, params: &[TypeParam]) -> bool {
        let required = if params.iter().any(|p| p.default().is_some()) {
            PythonVersion::PY313
        } else {
            PythonVersion::PY312
        };
        self.config.min_version >= required
    }

    /// Pick a unique mangled name for a `TypeVar`.
    ///
    /// First emission of a given source name returns the standard mangled
    /// form (`T` → `_T`). A *later* emission with a different signature gets
    /// a numeric suffix (`_T_2`, `_T_3`, …) so the per-class `TypeVar` object
    /// isn't shadowed by a later one with a different bound / default /
    /// variance. A later emission whose signature *matches* the existing one
    /// reuses the original mangled name (Python identity is preserved)
    fn unique_typevar_name(&mut self, source_name: &str, signature_args: &str) -> String {
        let base = mangle(source_name);
        let key_existing = self.emitted_typevar_signatures.get(&base).cloned();
        if let Some(existing_sig) = key_existing {
            if existing_sig == signature_args {
                return base;
            }
            self.typevar_suffix_counter += 1;
            let mangled = format!("{base}_{}", self.typevar_suffix_counter);
            self.emitted_typevar_signatures
                .insert(mangled.clone(), signature_args.to_owned());
            return mangled;
        }
        self.emitted_typevar_signatures
            .insert(base.clone(), signature_args.to_owned());
        base
    }

    /// Skip `TypeVar` declarations already emitted elsewhere in the module
    fn dedupe_defs(&mut self, defs: &[String], indent: &str) -> String {
        use std::fmt::Write as _;
        let mut prefix = String::new();
        for d in defs {
            if self.emitted_typevar_defs.insert(d.clone()) {
                let _ = writeln!(prefix, "{indent}{d}");
            }
        }
        prefix
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    /// whether `class` carries the synthetic `protocol_class` marker the parser
    /// emits for a `protocol P:` declaration
    fn has_protocol_marker(&self, class: &StmtClassDef) -> bool {
        class.decorator_list.iter().any(|dec| {
            super::source_util::is_synthetic_decorator(self.source, dec)
                && matches!(&dec.expression, Expr::Name(name) if name.id.as_str() == "protocol_class")
        })
    }

    /// Lower one element of a parameter-shape tuple to a Python type
    /// expression suitable for inclusion inside `tuple[...]`. Mirrors the
    /// rules in `annotation.rs::lower_tuple_element`
    fn lower_param_shape_elt(&self, elt: &Expr) -> String {
        match elt {
            Expr::Named(named) => {
                if let Expr::Starred(starred) = named.target.as_ref() {
                    if matches!(starred.value.as_ref(), Expr::Starred(_)) {
                        return String::new();
                    }
                    return format!("*tuple[{}, ...]", self.src(named.value.range()));
                }
                self.src(named.value.range()).to_owned()
            }
            Expr::Starred(s) => {
                if matches!(s.value.as_ref(), Expr::Starred(_)) {
                    return String::new();
                }
                format!("*tuple[{}, ...]", self.src(s.value.range()))
            }
            _ => self.src(elt.range()).to_owned(),
        }
    }

    fn line_start_of(&self, pos: TextSize) -> (TextSize, &str) {
        let start = super::source_util::line_start(self.source, pos);
        let indent = super::source_util::line_indent(self.source, pos);
        (start, indent)
    }

    /// Returns (`mangled_names_for_Generic`, `TypeVar_definition_lines`,
    /// `source_name → mangled_name rename map`)
    fn process_type_params(
        &mut self,
        params: &[TypeParam],
    ) -> (Vec<String>, Vec<String>, HashMap<String, String>) {
        let mut generic_args: Vec<String> = Vec::new();
        let mut defs: Vec<String> = Vec::new();
        let mut renames: HashMap<String, String> = HashMap::new();

        for param in params {
            match param {
                TypeParam::TypeVar(tv) => {
                    let name = tv.name.id.as_str();

                    // top-parameters bound → emit a ParamSpec rather than a TypeVar
                    // so the polyfilled output behaves like `**T` at runtime
                    if let Some(bound) = &tv.bound
                        && is_parameters_bound(bound)
                    {
                        let mangled = self.unique_typevar_name(name, "ParamSpec");
                        renames.insert(name.to_owned(), mangled.clone());
                        defs.push(format!("{mangled} = ParamSpec(\"{mangled}\")"));
                        self.needed_imports.paramspec = true;
                        generic_args.push(mangled);
                        continue;
                    }

                    // build the non-name TypeVar arguments first so we can
                    // pick a unique mangled name based on the call signature
                    let mut extra_args: Vec<String> = Vec::new();

                    if let Some(bound) = &tv.bound {
                        // `constraints(int, str)` → positional TypeVar args (basedpython form).
                        // Everything else, including tuple bounds, → bound=.
                        // In basedpython, `T: (int, str)` means bound=(int, str), not
                        // positional constraints — the explicit `constraints(...)` keyword
                        // is required.
                        if let Expr::Call(call) = bound.as_ref()
                            && call
                                .func
                                .as_name_expr()
                                .is_some_and(|n| n.id == "constraints")
                        {
                            let inner = call
                                .arguments
                                .args
                                .iter()
                                .map(|a| self.src(a.range()))
                                .collect::<Vec<_>>()
                                .join(", ");
                            if !inner.is_empty() {
                                extra_args.push(inner);
                            }
                        } else {
                            // basedpython parameter-shape tuple bound — lower to
                            // `tuple[...]` form before splicing into the
                            // `bound=` keyword arg
                            let bound_src = if let Expr::Tuple(t) = bound.as_ref()
                                && t.parenthesized
                                && t.has_parameter_shape()
                                && !t.is_anon_named_tuple
                                && !self.config.is_python
                            {
                                let inner = t
                                    .elts
                                    .iter()
                                    .map(|e| self.lower_param_shape_elt(e))
                                    .filter(|s| !s.is_empty())
                                    .collect::<Vec<_>>()
                                    .join(", ");
                                if inner.is_empty() {
                                    "tuple[()]".to_owned()
                                } else if t.elts.len() == 1
                                    && let Some(rest) = inner.strip_prefix("*")
                                {
                                    rest.to_owned()
                                } else {
                                    format!("tuple[{inner}]")
                                }
                            } else {
                                lower_type_expr_full(
                                    self.source,
                                    self.types,
                                    bound,
                                    &self.substitutions_within(bound.range()),
                                )
                                .unwrap_or_else(|| self.src(bound.range()).to_owned())
                            };
                            extra_args.push(format!("bound={bound_src}"));
                        }
                    }

                    if let Some(default) = &tv.default {
                        let default_src = lower_type_expr_full(
                            self.source,
                            self.types,
                            default,
                            &self.substitutions_within(default.range()),
                        )
                        .unwrap_or_else(|| self.src(default.range()).to_owned());
                        if self.config.min_version < PythonVersion::PY313 {
                            self.needed_imports.typevar_needs_ext = true;
                        }
                        extra_args.push(format!("default={default_src}"));
                    }

                    // basedpython variance keywords: forward `out`/`in`/`in out`
                    // into the legacy `TypeVar(..., covariant=, contravariant=)`
                    // call so pre-3.12 polyfilled output preserves variance
                    match tv.variance {
                        Some(ruff_python_ast::Variance::Covariant) => {
                            extra_args.push("covariant=True".to_owned());
                        }
                        Some(ruff_python_ast::Variance::Contravariant) => {
                            extra_args.push("contravariant=True".to_owned());
                        }
                        Some(ruff_python_ast::Variance::Invariant) => {
                            // `in out` is explicit invariance. legacy
                            // `TypeVar(...)` with no variance args is already
                            // invariant, so emit nothing
                        }
                        None => {}
                    }

                    // pick a unique mangled name based on the call signature
                    // so two classes that both declare `T` but with different
                    // bounds / variance / defaults don't shadow each other
                    let signature_args = extra_args.join(", ");
                    let mangled = self.unique_typevar_name(name, &signature_args);
                    renames.insert(name.to_owned(), mangled.clone());
                    let mut args: Vec<String> = vec![format!("\"{mangled}\"")];
                    args.extend(extra_args);
                    let def = format!("{mangled} = TypeVar({})", args.join(", "));

                    self.needed_imports.typevar = true;
                    generic_args.push(mangled.clone());
                    defs.push(def);
                }

                TypeParam::TypeVarTuple(tvt) => {
                    let name = tvt.name.id.as_str();
                    let mangled = self.unique_typevar_name(name, "TypeVarTuple");
                    renames.insert(name.to_owned(), mangled.clone());
                    defs.push(format!("{mangled} = TypeVarTuple(\"{mangled}\")"));
                    self.needed_imports.typevar_tuple = true;
                    self.needed_imports.unpack = true;
                    // star-in-subscript (`Generic[*T]`) is only valid syntax
                    // on Python 3.11+; below that, emit the equivalent
                    // `Unpack[T]` form so the polyfilled output parses
                    let arg = if self.config.min_version >= PythonVersion::PY311 {
                        format!("*{mangled}")
                    } else {
                        format!("Unpack[{mangled}]")
                    };
                    generic_args.push(arg);
                }

                TypeParam::ParamSpec(ps) => {
                    let name = ps.name.id.as_str();
                    let mangled = self.unique_typevar_name(name, "ParamSpec");
                    renames.insert(name.to_owned(), mangled.clone());
                    defs.push(format!("{mangled} = ParamSpec(\"{mangled}\")"));
                    self.needed_imports.paramspec = true;
                    generic_args.push(mangled);
                }
            }
        }

        (generic_args, defs, renames)
    }

    /// For 3.12+ pass-through, strip `constraints` prefix from `TypeVar` bounds
    /// so `T: constraints(int, str)` becomes `T: (int, str)` (valid Python).
    ///
    /// Also rewrites `.by` tuple bounds `T: (int, str)` → `T: tuple[int, str]`
    /// because Python 3.12+ treats `T: (int, str)` as positional constraints,
    /// not a tuple bound.
    fn strip_constraints_keyword(&mut self, params: &[TypeParam]) {
        for param in params {
            if let TypeParam::TypeVar(tv) = param {
                if let Some(bound) = &tv.bound {
                    // top-parameters bound → `**T` (PEP 695 paramspec syntax)
                    if is_parameters_bound(bound) {
                        let name = tv.name.id.as_str();
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("**{name}"),
                            param.range(),
                        )));
                        continue;
                    }
                    if let Expr::Call(call) = bound.as_ref()
                        && call
                            .func
                            .as_name_expr()
                            .is_some_and(|n| n.id == "constraints")
                    {
                        let edit_range = TextRange::new(
                            call.func.range().start(),
                            call.arguments.range().start(),
                        );
                        self.edits
                            .push(Fix::safe_edit(Edit::range_deletion(edit_range)));
                    } else if !self.config.is_python
                        && let Expr::Tuple(t) = bound.as_ref()
                        && t.parenthesized
                        && !t.is_anon_named_tuple
                    {
                        // .by: T: (int, str) is a tuple bound, but Python 3.12+
                        // interprets (int, str) as positional constraints.
                        // Lower each element with parameter-shape awareness:
                        // `*: T` → `*tuple[T, ...]`, `name: T` → `T`,
                        // `**: T` / `**name: T` → dropped
                        let inner = t
                            .elts
                            .iter()
                            .map(|e| self.lower_param_shape_elt(e))
                            .filter(|s| !s.is_empty())
                            .collect::<Vec<_>>()
                            .join(", ");
                        let replacement = if inner.is_empty() {
                            "tuple[()]".to_owned()
                        } else if t.elts.len() == 1
                            && let Some(rest) = inner.strip_prefix("*")
                        {
                            // pure variadic `(*: T)` → `tuple[T, ...]`
                            rest.to_owned()
                        } else {
                            format!("tuple[{inner}]")
                        };
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            replacement,
                            bound.range(),
                        )));
                    }
                }
            }
        }
    }

    fn process_class(&mut self, class: &StmtClassDef) {
        let Some(tp) = &class.type_params else {
            // a based-enum variant lowers to a module-level subclass of the enum
            // with no type params of its own; rename the enum's params in its
            // field annotations using the enum's recorded map
            self.rename_variant_of_generic_enum(class);
            return;
        };
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(class.name.id.as_str().to_owned());
        }
        // a `protocol P[T]:` with no explicit bases defers its `Protocol` base to
        // this pass, which owns the base list for a type-param class (modifiers
        // skips it to avoid two competing base-parens around the type params)
        let deferred_protocol = class.arguments.is_none() && self.has_protocol_marker(class);
        // PEP 695 class type params are native syntax in 3.12+ (3.13+ with defaults)
        if self.supports_native_type_params(&tp.type_params) {
            self.strip_constraints_keyword(&tp.type_params);
            if deferred_protocol {
                // keep the native `[T]`, append the base after it: `[T](Protocol)`
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    "(Protocol)".to_owned(),
                    tp.range().end(),
                )));
            }
            return;
        }

        let (generic_args, defs, rename_map) = self.process_type_params(&tp.type_params);
        // record for module-level variant subclasses that reference these params
        self.generic_class_renames
            .insert(class.name.id.as_str().to_owned(), rename_map.clone());
        let generic_str = format!("Generic[{}]", generic_args.join(", "));
        self.needed_imports.generic = true;

        // Modify or add base classes.
        if let Some(args) = &class.arguments {
            // Emit rename edits for type params within base class expressions
            // as individual edits — this lets literal_types and auto_quote also
            // emit their own non-overlapping edits on the same expressions.
            for base_expr in &args.args {
                rename_in_expr(base_expr, &rename_map, &mut self.edits);
            }
            if args.args.is_empty() && args.keywords.is_empty() {
                // empty `()` → replace with `(Generic[_T])`; 2-char range, safe
                self.edits.push(Fix::safe_edit(Edit::range_replacement(
                    format!("({generic_str})"),
                    args.range(),
                )));
            } else if let Some(first_keyword) = args.keywords.first() {
                // `Generic[_T]` is a positional base, so it has to precede any
                // keyword (e.g. `metaclass=`). insert it just before the first
                // keyword rather than before the closing `)`
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    format!("{generic_str}, "),
                    first_keyword.range().start(),
                )));
            } else {
                // insert `, Generic[_T]` before the closing `)` as a zero-width
                // edit so it doesn't subsume any edits on the base expressions
                let rparen = args.range().end() - TextSize::from(1);
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    format!(", {generic_str}"),
                    rparen,
                )));
            }
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(tp.range())));
        } else if deferred_protocol {
            // the marker protocol's base goes in the same parens as `Generic`,
            // positional and before it: `(Protocol, Generic[_T])`
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("(Protocol, {generic_str})"),
                tp.range(),
            )));
        } else {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("({generic_str})"),
                tp.range(),
            )));
        }

        // Insert TypeVar definitions before the class.
        let (line_start, indent) = self.line_start_of(class.range().start());
        let indent = indent.to_owned();
        let prefix = self.dedupe_defs(&defs, &indent);
        if !prefix.is_empty() {
            self.edits
                .push(Fix::safe_edit(Edit::insertion(prefix, line_start)));
        }

        // Rename type param references in class body.
        for stmt in &class.body {
            rename_in_stmt(stmt, &rename_map, &mut self.edits);
        }
    }

    /// Rename a generic enum's type params in a module-level variant subclass.
    /// A variant lowers to `class _Enum_Variant(Enum): field: T`; its `T` refs
    /// live outside the (already-processed) enum body, so they are renamed using
    /// the enum's recorded `T`→`_T` map.
    fn rename_variant_of_generic_enum(&mut self, class: &StmtClassDef) {
        let Some(args) = &class.arguments else {
            return;
        };
        let Some(Expr::Name(base)) = args.args.first() else {
            return;
        };
        if let Some(rename_map) = self.generic_class_renames.get(base.id.as_str()).cloned() {
            for stmt in &class.body {
                rename_in_stmt(stmt, &rename_map, &mut self.edits);
            }
        }
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        let Some(tp) = &func.type_params else {
            return;
        };
        // basedpython: a `type def` is erased by its own pass, so polyfilling its
        // type parameters would leave an orphan `TypeVar` behind
        if ruff_python_ast::helpers::is_type_def(func) {
            return;
        }
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(func.name.id.as_str().to_owned());
        }
        // PEP 695 function type params are native syntax in 3.12+ (3.13+ with defaults)
        if self.supports_native_type_params(&tp.type_params) {
            self.strip_constraints_keyword(&tp.type_params);
            return;
        }

        let (_, defs, rename_map) = self.process_type_params(&tp.type_params);

        // Remove `[T, ...]` from the function signature.
        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(tp.range())));

        // Insert TypeVar definitions before the function.
        let (line_start, indent) = self.line_start_of(func.range().start());
        let indent = indent.to_owned();
        let prefix = self.dedupe_defs(&defs, &indent);
        if !prefix.is_empty() {
            self.edits
                .push(Fix::safe_edit(Edit::insertion(prefix, line_start)));
        }

        // Rename type param references in parameter annotations, return type, and body.
        let all_params = func
            .parameters
            .posonlyargs
            .iter()
            .chain(func.parameters.args.iter())
            .chain(func.parameters.kwonlyargs.iter());
        for param in all_params {
            if let Some(ann) = &param.parameter.annotation {
                rename_in_expr(ann, &rename_map, &mut self.edits);
            }
        }
        if let Some(vararg) = &func.parameters.vararg {
            if let Some(ann) = &vararg.annotation {
                rename_in_expr(ann, &rename_map, &mut self.edits);
            }
        }
        if let Some(kwarg) = &func.parameters.kwarg {
            if let Some(ann) = &kwarg.annotation {
                rename_in_expr(ann, &rename_map, &mut self.edits);
            }
        }
        if let Some(ret) = &func.returns {
            rename_in_expr(ret, &rename_map, &mut self.edits);
        }
        for stmt in &func.body {
            rename_in_stmt(stmt, &rename_map, &mut self.edits);
        }
    }

    fn process_type_alias(&mut self, alias: &StmtTypeAlias) {
        // `type Point = tuple[float, float]`
        //   → `Point = TypeAliasType("Point", tuple[float, float])`
        let params = alias
            .type_params
            .as_deref()
            .map_or(&[][..], |tp| tp.type_params.as_slice());
        if self.supports_native_type_params(params) {
            if let Some(tp) = &alias.type_params {
                self.strip_constraints_keyword(&tp.type_params);
            }
            return;
        }

        // this replacement subsumes `modifiers`' `private ` deletion and the
        // rename of the definition site, so the private name has to be applied
        // here instead
        let name_src = if alias.is_private {
            format!("_{}", self.src(alias.name.range()))
        } else {
            self.src(alias.name.range()).to_owned()
        };
        let raw_value_src = self.src(alias.value.range()).to_owned();

        // references to a `private type` alias declared elsewhere in the module
        // also sit inside the subsumed value, so they are renamed here too
        let mut rename_map = self.private_aliases.clone();

        let (type_params_arg, defs) = if let Some(tp) = &alias.type_params {
            let (generic_args, type_defs, tp_renames) = self.process_type_params(&tp.type_params);
            rename_map.extend(tp_renames);

            // TypeVarTuple entries have a leading `*` in generic_args (for
            // Generic[*_Ts]) but `type_params=` wants the bare name.
            let param_names: Vec<&str> = generic_args
                .iter()
                .map(|s| s.trim_start_matches('*'))
                .collect();
            let trailing = if param_names.len() == 1 { "," } else { "" };
            let tps = format!(", type_params=({}{})", param_names.join(", "), trailing);

            (tps, type_defs)
        } else {
            (String::new(), Vec::new())
        };

        // Everything that rewrites part of the value has to be spliced here: our
        // `alias.range()` edit subsumes the value, so an edit another pass emitted
        // on it alone is dropped. Symbolic folds (`T.a` → `int`, `Dim + 1` → `int`)
        // and typevar / private-alias renames are collected into one substitution
        // set so the lowering below honours both, rather than picking one and
        // silently losing the other.
        let folded = self.substitutions_within(alias.value.range());
        let mut value_renames: Vec<Fix> = Vec::new();
        rename_in_expr(&alias.value, &rename_map, &mut value_renames);
        let renames = value_renames
            .iter()
            .flat_map(ruff_diagnostics::Fix::edits)
            // a rename inside a folded operation went with the operand it renamed —
            // `type X[T: A] = T.a` folds to `int`, which mentions no `T` to rename
            .filter(|edit| {
                !folded
                    .iter()
                    .any(|(range, _)| range.contains_range(edit.range()))
            })
            .map(|edit| (edit.range(), edit.content().unwrap_or_default().to_owned()));
        let substitutions: Vec<(TextRange, String)> =
            folded.iter().cloned().chain(renames).collect();

        let value_src = lower_type_expr_full(self.source, self.types, &alias.value, &substitutions)
            .unwrap_or(raw_value_src);

        self.needed_imports.typealias_type = true;

        let (_line_start, indent) = self.line_start_of(alias.range().start());
        let indent = indent.to_owned();

        let mut replacement = String::new();
        for d in &defs {
            let _ = writeln!(replacement, "{indent}{d}");
        }
        let _ = write!(
            replacement,
            "{indent}{name_src} = TypeAliasType(\"{name_src}\", {value_src}{type_params_arg})"
        );

        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            alias.range(),
        )));
    }
}

impl GenericPolyfill<'_> {
    /// Rewrites a tuple slice of a parameters-typed subscript to a list.
    /// `A[(int, str)]` → `A[[int, str]]` so the runtime `ParamSpec` accepts
    /// the substitution. Parameters spec syntax (`(int, str, /, name: T)`)
    /// drops the `/` and `*` markers and replaces named-only fields with
    /// `Any` since runtime `ParamSpec` only carries positional types
    fn rewrite_parameters_subscript(&mut self, sub: &ruff_python_ast::ExprSubscript) {
        let Expr::Name(name) = sub.value.as_ref() else {
            return;
        };
        if !self.parameters_targets.contains(name.id.as_str()) {
            return;
        }
        let Expr::Tuple(t) = sub.slice.as_ref() else {
            return;
        };
        if !t.parenthesized {
            return;
        }

        if t.has_parameter_shape() {
            // emit a single replacement for the whole tuple — the inner
            // structure (markers, named, variadic, kwargs) doesn't map
            // 1:1 to runtime ParamSpec list elements, so we lower each
            // element to a positional Python type. mapping:
            //   `int`        → `int`
            //   `name: T`    → `Any` (named-only has no positional slot)
            //   `*: T`       → `Any` (variadic flattened to one element)
            //   `*name: T`   → `Any`
            //   `**: T`      → dropped
            //   `**name: T`  → dropped
            let mut parts: Vec<String> = Vec::new();
            for elt in &t.elts {
                match elt {
                    Expr::Named(named) => {
                        if let Expr::Starred(starred) = named.target.as_ref() {
                            // `**name: T` — Starred(Starred(...)) target → drop
                            if matches!(starred.value.as_ref(), Expr::Starred(_)) {
                                continue;
                            }
                            // `*name: T`
                            parts.push("Any".to_owned());
                            self.needed_imports_any = true;
                        } else {
                            // `name: T`
                            parts.push("Any".to_owned());
                            self.needed_imports_any = true;
                        }
                    }
                    Expr::Starred(s) => {
                        if matches!(s.value.as_ref(), Expr::Starred(_)) {
                            // `**: T` — drop
                            continue;
                        }
                        // `*: T`
                        parts.push("Any".to_owned());
                        self.needed_imports_any = true;
                    }
                    _ => {
                        parts.push(self.src(elt.range()).to_owned());
                    }
                }
            }
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("[{}]", parts.join(", ")),
                t.range(),
            )));
            return;
        }

        // replace just the parens — `(` → `[` and `)` → `]` — so any nested
        // edits inside the elements still apply without overlap
        let open = TextRange::new(t.range().start(), t.range().start() + TextSize::from(1));
        let close = TextRange::new(t.range().end() - TextSize::from(1), t.range().end());
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            "[".to_owned(),
            open,
        )));
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            "]".to_owned(),
            close,
        )));
    }
}

impl<'ast> Visitor<'ast> for GenericPolyfill<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(class) => self.process_class(class),
            Stmt::FunctionDef(func) => self.process_function(func),
            Stmt::TypeAlias(alias) => {
                self.process_type_alias(alias);
                return; // don't recurse into the alias value
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(sub) = expr {
            self.rewrite_parameters_subscript(sub);
        }
        walk_expr(self, expr);
    }
}

fn has_parameters_bound(params: &[TypeParam]) -> bool {
    params.iter().any(|p| {
        if let TypeParam::TypeVar(tv) = p
            && let Some(bound) = &tv.bound
        {
            return is_parameters_bound(bound);
        }
        false
    })
}

/// basedpython spells a `ParamSpec` as a type variable bound by the top parameters form
/// `(*: *, **: *)` — the parameter list every other parameter list is a subtype of
fn is_parameters_bound(bound: &Expr) -> bool {
    ruff_python_ast::helpers::is_top_parameters_form(bound)
}

fn rename_in_expr(expr: &Expr, renames: &HashMap<String, String>, edits: &mut Vec<Fix>) {
    match expr {
        Expr::Name(n) => {
            if let Some(new) = renames.get(n.id.as_str()) {
                edits.push(Fix::safe_edit(Edit::range_replacement(
                    new.clone(),
                    n.range(),
                )));
            }
        }
        Expr::Subscript(s) => {
            rename_in_expr(&s.value, renames, edits);
            rename_in_expr(&s.slice, renames, edits);
        }
        Expr::Attribute(a) => rename_in_expr(&a.value, renames, edits),
        Expr::Tuple(t) => t
            .elts
            .iter()
            .for_each(|e| rename_in_expr(e, renames, edits)),
        Expr::List(l) => l
            .elts
            .iter()
            .for_each(|e| rename_in_expr(e, renames, edits)),
        Expr::BinOp(b) => {
            rename_in_expr(&b.left, renames, edits);
            rename_in_expr(&b.right, renames, edits);
        }
        Expr::Call(c) => {
            rename_in_expr(&c.func, renames, edits);
            c.arguments
                .args
                .iter()
                .for_each(|a| rename_in_expr(a, renames, edits));
        }
        Expr::UnaryOp(u) => rename_in_expr(&u.operand, renames, edits),
        Expr::Starred(s) => rename_in_expr(&s.value, renames, edits),
        // an arrow callable `(**P) -> None` lowers to `Callable[P, None]` via a template edit
        // that passes its operand source through, so a rename on the inner name still lands
        Expr::CallableType(c) => {
            if let Some(receiver) = &c.receiver {
                rename_in_expr(receiver, renames, edits);
            }
            c.args
                .iter()
                .for_each(|a| rename_in_expr(a, renames, edits));
            rename_in_expr(&c.returns, renames, edits);
        }
        _ => {}
    }
}

fn rename_in_stmt(stmt: &Stmt, renames: &HashMap<String, String>, edits: &mut Vec<Fix>) {
    match stmt {
        Stmt::AnnAssign(a) => {
            rename_in_expr(&a.annotation, renames, edits);
            if let Some(v) = &a.value {
                rename_in_expr(v, renames, edits);
            }
        }
        Stmt::FunctionDef(f) => {
            for p in f
                .parameters
                .posonlyargs
                .iter()
                .chain(f.parameters.args.iter())
                .chain(f.parameters.kwonlyargs.iter())
            {
                if let Some(ann) = &p.parameter.annotation {
                    rename_in_expr(ann, renames, edits);
                }
            }
            for variadic in [
                f.parameters.vararg.as_deref(),
                f.parameters.kwarg.as_deref(),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(ann) = &variadic.annotation {
                    rename_in_expr(ann, renames, edits);
                }
            }
            if let Some(ret) = &f.returns {
                rename_in_expr(ret, renames, edits);
            }
            for s in &f.body {
                rename_in_stmt(s, renames, edits);
            }
        }
        Stmt::Return(r) => {
            if let Some(v) = &r.value {
                rename_in_expr(v, renames, edits);
            }
        }
        Stmt::Assign(a) => {
            for t in &a.targets {
                rename_in_expr(t, renames, edits);
            }
            rename_in_expr(&a.value, renames, edits);
        }
        Stmt::Expr(e) => rename_in_expr(&e.value, renames, edits),
        Stmt::If(i) => {
            rename_in_expr(&i.test, renames, edits);
            for s in &i.body {
                rename_in_stmt(s, renames, edits);
            }
            for clause in &i.elif_else_clauses {
                for s in &clause.body {
                    rename_in_stmt(s, renames, edits);
                }
            }
        }
        // descend into a nested class (e.g. an enum's nested variant classes)
        // so the enclosing class's type-param references in its bases and field
        // annotations are renamed too — after the polyfill the mangled `TypeVar`
        // is bound at module scope, so the nested reference resolves to it. a
        // nested class that introduces its *own* type params is polyfilled
        // independently and may shadow the name, so skip it
        Stmt::ClassDef(c) if c.type_params.is_none() => {
            if let Some(args) = &c.arguments {
                for base in &args.args {
                    rename_in_expr(base, renames, edits);
                }
            }
            for s in &c.body {
                rename_in_stmt(s, renames, edits);
            }
        }
        _ => {}
    }
}

pub(crate) fn mangle(name: &str) -> String {
    if name.starts_with('_') {
        name.to_owned()
    } else {
        format!("_{name}")
    }
}

pub(crate) struct GenericPolyfillPass<'src> {
    source: &'src str,
    config: Config,
}

impl<'src> GenericPolyfillPass<'src> {
    pub(crate) fn new(source: &'src str, config: Config) -> Self {
        Self { source, config }
    }
}

impl super::ast_driver::TypeAwarePass for GenericPolyfillPass<'_> {
    fn run(
        &self,
        stmts: &[ruff_python_ast::Stmt],
        types: &dyn TypeInfo,
        ctx: &mut super::ast_driver::PassContext,
    ) {
        let mut inner = GenericPolyfill::new(
            self.source,
            types,
            self.config.clone(),
            ctx.symbolic_substitutions.clone(),
        );
        inner.collect_private_aliases(stmts);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        let emits_any = inner.needed_imports_any;
        for line in std::mem::take(&mut inner.needed_imports).into_lines() {
            ctx.required_imports.push(line);
        }
        if emits_any {
            ctx.required_imports
                .push("from typing import Any".to_owned());
        }
        for fix in inner.edits {
            for edit in fix.edits() {
                let range = edit.range();
                let repl = edit.content().unwrap_or_default().to_owned();
                ctx.text_edits.push((range, repl));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    fn check_at(input: &str, expected: &str, version: PythonVersion) {
        let config = Config {
            min_version: version,
            ..Config::test_default()
        };
        assert_eq!(
            transpile(input, &config).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn class_simple_typevar() {
        check(
            indoc! {"
                class Foo[T]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn class_with_base() {
        check(
            indoc! {"
                class Foo[T](Base): ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Base, Generic[_T]): ...
            "},
        );
    }

    /// regression: a `protocol P[T]:` with no explicit base used to have the
    /// generics pass and the modifiers pass both create a base-parens list,
    /// producing invalid `class A(Protocol)(Generic[_T])`. generics now owns the
    /// whole base list, placing `Protocol` before `Generic`
    #[test]
    fn protocol_class_type_params_legacy() {
        check(
            indoc! {"
                protocol Foo[T]:
                    a: T
            "},
            indoc! {"
                from typing import Protocol, TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Protocol, Generic[_T]):
                    a: _T
            "},
        );
    }

    /// on a native-PEP-695 target the type params stay `[T]` and `Protocol` is
    /// appended after them
    #[test]
    fn protocol_class_type_params_native() {
        check_at(
            indoc! {"
                protocol Foo[T]:
                    a: T
            "},
            indoc! {"
                from typing import Protocol
                class Foo[T](Protocol):
                    a: T
            "},
            PythonVersion::PY313,
        );
    }

    /// an explicit base composes: modifiers adds `Protocol` to the existing
    /// parens and generics adds `Generic` there too
    #[test]
    fn protocol_class_type_params_with_base() {
        check(
            indoc! {"
                protocol Foo[T](Bar):
                    a: T
            "},
            indoc! {"
                from typing import Protocol, TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Bar, Protocol, Generic[_T]):
                    a: _T
            "},
        );
    }

    #[test]
    fn class_with_metaclass_keyword() {
        // the synthesized `Generic[_T]` is a positional base, so it must come
        // before the `metaclass=` keyword, not after the closing paren
        check(
            indoc! {"
                class Foo[T](Base, metaclass=ABCMeta): ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Base, Generic[_T], metaclass=ABCMeta): ...
            "},
        );
    }

    #[test]
    fn class_with_only_metaclass_keyword() {
        check(
            indoc! {"
                class Foo[T](metaclass=ABCMeta): ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Generic[_T], metaclass=ABCMeta): ...
            "},
        );
    }

    #[test]
    fn class_with_empty_parens() {
        check(
            indoc! {"
                class Foo[T](): ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn class_multiple_params() {
        check(
            indoc! {"
                class Map[K, V]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _K = TypeVar(\"_K\")
                _V = TypeVar(\"_V\")
                class Map(Generic[_K, _V]): ...
            "},
        );
    }

    #[test]
    fn class_bound_typevar() {
        check(
            indoc! {"
                class Foo[T: int]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=int)
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn class_bound_float_constant_typevar() {
        // `float.inf` in a bound must be erased to `float` when the bound is
        // copied into the synthesized `TypeVar(...)` call, or it `AttributeError`s
        // at runtime (the bound expression IS evaluated, unlike a native PEP 695
        // bound on 3.12+ under `from __future__ import annotations`).
        check(
            indoc! {"
                class Foo[T: float.inf]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=float)
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn class_bound_literal_typevar() {
        // Bound `1 | 2` must be rewritten to `Literal[1, 2]`, and the default
        // must not be silently dropped when a bound is present.
        check(
            indoc! {"
                class A[T: 1 | 2 = 1 | 2]: ...
            "},
            indoc! {"
                from typing import Generic, Literal
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", bound=Literal[1, 2], default=Literal[1, 2])
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn class_default_typevar() {
        // Default-only TypeVar with literal default should also rewrite.
        check(
            indoc! {"
                class A[T = 1 | 2]: ...
            "},
            indoc! {"
                from typing import Generic, Literal
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=Literal[1, 2])
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn generic_function() {
        check(
            indoc! {"
                def identity[T](x: T) -> T:
                    return x
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                def identity(x: _T) -> _T:
                    return x
            "},
        );
    }

    #[test]
    fn class_body_rename() {
        check(
            indoc! {"
                class A[T]:
                    t: T
                    def method(self, x: T) -> T:
                        return x
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class A(Generic[_T]):
                    t: _T
                    def method(self, x: _T) -> _T:
                        return x
            "},
        );
    }

    #[test]
    fn type_alias_simple() {
        // `float` in type position rewrites to `JustFloat` in basedpython
        check(
            indoc! {"
                type Point = tuple[float, float]
            "},
            indoc! {"
                from ty_extensions import JustFloat
                from typing_extensions import TypeAliasType
                Point = TypeAliasType(\"Point\", tuple[JustFloat, JustFloat])
            "},
        );
    }

    #[test]
    fn type_alias_generic() {
        check(
            indoc! {"
                type Vector[T] = list[T]
            "},
            indoc! {"
                from typing import TypeVar
                from typing_extensions import TypeAliasType
                _T = TypeVar(\"_T\")
                Vector = TypeAliasType(\"Vector\", list[_T], type_params=(_T,))
            "},
        );
    }

    #[test]
    fn no_type_params_unchanged() {
        check(
            indoc! {"
                class Foo(Base): ...
            "},
            indoc! {"
                class Foo(Base): ...
            "},
        );
    }

    #[test]
    fn class_generic_unchanged_on_312() {
        // PEP 695 is native in 3.12+, so the polyfill must not fire
        let src = "class Foo[T]: ...\n";
        check_at(src, src, PythonVersion::PY312);
        check_at(src, src, PythonVersion::PY313);
        check_at(src, src, PythonVersion::PY314);
    }

    #[test]
    fn function_generic_unchanged_on_312() {
        let src = indoc! {"
            def identity[T](x: T) -> T:
                return x
        "};
        check_at(src, src, PythonVersion::PY312);
        check_at(src, src, PythonVersion::PY314);
    }

    #[test]
    fn type_alias_unchanged_on_312() {
        // PEP 695 native, so the alias statement passes through — but `float`
        // in type position still rewrites to `JustFloat`
        let src = "type Point = tuple[float, float]\n";
        let expected = indoc! {"
            from ty_extensions import JustFloat
            type Point = tuple[JustFloat, JustFloat]
        "};
        check_at(src, expected, PythonVersion::PY312);
        check_at(src, expected, PythonVersion::PY314);
    }

    #[test]
    fn function_default_downleveled_on_312() {
        // pep 696 defaults are 3.13-only syntax: a defaulted list on a 3.12
        // target polyfills like pre-3.12 code instead of passing through
        check_at(
            indoc! {"
                def f[T = int](x: T) -> T:
                    return x
            "},
            indoc! {"
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=int)
                def f(x: _T) -> _T:
                    return x
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn function_default_unchanged_on_313() {
        let src = indoc! {"
            def f[T = int](x: T) -> T:
                return x
        "};
        check_at(src, src, PythonVersion::PY313);
        check_at(src, src, PythonVersion::PY314);
    }

    #[test]
    fn class_default_downleveled_on_312() {
        check_at(
            "class A[T = int]: ...\n",
            indoc! {"
                from typing import Generic
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=int)
                class A(Generic[_T]): ...
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn class_bound_and_default_downleveled_on_312() {
        // the bound rides along into the polyfilled call next to the default
        check_at(
            "class A[T: int = int]: ...\n",
            indoc! {"
                from typing import Generic
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", bound=int, default=int)
                class A(Generic[_T]): ...
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn class_default_unchanged_on_313() {
        let src = "class A[T = int]: ...\n";
        check_at(src, src, PythonVersion::PY313);
        check_at(src, src, PythonVersion::PY314);
    }

    #[test]
    fn only_defaulted_declarations_downlevel_on_312() {
        // gating is per declaration: the defaulted class polyfills while the
        // plain one keeps native pep 695 syntax
        check_at(
            indoc! {"
                class A[T = int]: ...
                class B[T]: ...
            "},
            indoc! {"
                from typing import Generic
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\", default=int)
                class A(Generic[_T]): ...
                class B[T]: ...
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn paramspec_and_typevartuple_defaults_downlevel_on_312() {
        // defaults on any parameter kind make the header 3.13-only syntax, so
        // the whole declaration polyfills. the polyfill itself drops paramspec
        // and typevartuple defaults, matching the pre-3.12 path
        check_at(
            indoc! {"
                class A[**P = [int]]: ...
                class B[*Ts = *tuple[int, str]]: ...
            "},
            indoc! {"
                from typing import TypeVarTuple, Unpack, ParamSpec, Generic
                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
                _Ts = TypeVarTuple(\"_Ts\")
                class B(Generic[*_Ts]): ...
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn type_alias_default_downleveled_on_312() {
        check_at(
            "type Vector[T = int] = list[T]\n",
            indoc! {"
                from typing_extensions import TypeVar, TypeAliasType
                _T = TypeVar(\"_T\", default=int)
                Vector = TypeAliasType(\"Vector\", list[_T], type_params=(_T,))
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn type_alias_default_unchanged_on_313() {
        let src = "type Vector[T = int] = list[T]\n";
        check_at(src, src, PythonVersion::PY313);
        check_at(src, src, PythonVersion::PY314);
    }

    #[test]
    fn variance_covariant_polyfill() {
        check(
            indoc! {"
                class A[out T]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", covariant=True)
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn variance_contravariant_polyfill() {
        check(
            indoc! {"
                class A[in T]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", contravariant=True)
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn variance_bivariant_polyfill() {
        check(
            indoc! {"
                class A[in out T]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn variance_stripped_on_312() {
        check_at(
            "class A[out T]: ...\n",
            "class A[T]: ...\n",
            PythonVersion::PY312,
        );
        check_at(
            "class A[in T]: ...\n",
            "class A[T]: ...\n",
            PythonVersion::PY312,
        );
        check_at(
            "class A[in out T]: ...\n",
            "class A[T]: ...\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn variance_with_bound_polyfill() {
        check(
            indoc! {"
                class A[out T: int]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=int, covariant=True)
                class A(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn constraints_keyword_polyfill() {
        check(
            indoc! {"
                class Foo[T: constraints (int, str)]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", int, str)
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn constraints_keyword_function_polyfill() {
        check(
            indoc! {"
                def f[T: constraints (int, str)](x: T) -> T:
                    return x
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\", int, str)
                def f(x: _T) -> _T:
                    return x
            "},
        );
    }

    #[test]
    fn constraints_keyword_stripped_on_312() {
        check_at(
            "class Foo[T: constraints (int, str)]: ...\n",
            "class Foo[T: (int, str)]: ...\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn constraints_keyword_function_stripped_on_312() {
        check_at(
            indoc! {"
                def f[T: constraints (int, str)](x: T) -> T:
                    return x
            "},
            indoc! {"
                def f[T: (int, str)](x: T) -> T:
                    return x
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn tuple_bound_is_not_constraints() {
        // In basedpython, `T: (int, str)` is a tuple-type upper bound, NOT
        // positional constraints. Use `T: constraints(int, str)` for that. the
        // bound lowers like any other type expression (matching the native
        // `class Foo[T: tuple[int, str]]` form), since the polyfill routes the
        // bound through the same type-expression lowerer
        check(
            indoc! {"
                class Foo[T: (int, str)]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=tuple[int, str])
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn constraints_with_space_same_as_without() {
        // `constraints (int, str)` and `constraints(int, str)` produce identical output.
        let with_space = transpile(
            "class Foo[T: constraints (int, str)]: ...\n",
            &Config::test_default(),
        )
        .unwrap();
        let without_space = transpile(
            "class Foo[T: constraints (int, str)]: ...\n",
            &Config::test_default(),
        )
        .unwrap();
        assert_eq!(with_space, without_space);
    }

    #[test]
    fn constraints_with_space_stripped_on_312() {
        check_at(
            "class Foo[T: constraints (int, str)]: ...\n",
            "class Foo[T: (int, str)]: ...\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn tuple_bound_rewritten_on_312() {
        // In .by, T: (int, str) is a tuple bound. Python 3.12+ treats (int, str)
        // as positional constraints, so we must rewrite to tuple[int, str].
        check_at(
            "class Foo[T: (int, str)]: ...\n",
            "class Foo[T: tuple[int, str]]: ...\n",
            PythonVersion::PY312,
        );
        check_at(
            "class Foo[T: (int, str)]: ...\n",
            "class Foo[T: tuple[int, str]]: ...\n",
            PythonVersion::PY314,
        );
    }

    #[test]
    fn mixed_tuple_bound_and_constraints_on_314() {
        // TTuple: (int, str) → tuple[int, str]; TConst: constraints(int, str) → (int, str)
        check_at(
            indoc! {"
                class A[
                    TTuple: (int, str),
                    TConst: constraints (int, str),
                ]: ...
            "},
            indoc! {"
                class A[
                    TTuple: tuple[int, str],
                    TConst: (int, str),
                ]: ...
            "},
            PythonVersion::PY314,
        );
    }

    // --- .py vs .by constraint semantics ---

    #[test]
    fn py_tuple_is_constraints() {
        // In .py files (is_python=true), T: (int, str) is standard Python constraint syntax.
        // The transpiler passes through unchanged; Python itself treats it as constraints.
        let src = "class Foo[T: (int, str)]: ...\n";
        let config = Config {
            is_python: true,
            ..Config::test_default()
        };
        assert_eq!(transpile(src, &config).unwrap(), src);
    }

    #[test]
    fn by_tuple_is_bound() {
        // In .by files, T: (int, str) is an upper bound (tuple type), not
        // constraints — and it lowers to `tuple[int, str]` like the native form
        check(
            "class Foo[T: (int, str)]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=tuple[int, str])
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn intersection_bound_polyfilled() {
        // an `&` bound lowers to `Intersection[...]` even on the < 3.12 polyfill
        // path: the `TypeVar(bound=)` payload routes through the full type
        // lowerer, matching the native `class Foo[T: Intersection[A, B]]`
        check(
            "class Foo[T: A & B]: ...\n",
            indoc! {"
                from ty_extensions import Intersection
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=Intersection[A, B])
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn negation_bound_polyfilled() {
        check(
            "class Foo[T: not int]: ...\n",
            indoc! {"
                from ty_extensions import Not
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=Not[int])
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn leaf_composes_in_polyfilled_bound() {
        // per-leaf lowering still composes inside the spliced bound
        check(
            "class Foo[T: A & float]: ...\n",
            indoc! {"
                from ty_extensions import Intersection, JustFloat
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=Intersection[A, JustFloat])
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn parameters_bound_polyfill() {
        check(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
            "},
            indoc! {"
                from typing import ParamSpec, Generic
                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
            "},
        );
    }

    #[test]
    fn parameters_bound_native_312() {
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
            "},
            indoc! {"
                class A[**P]: ...
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_subscript_tuple_to_list() {
        // call site: tuple slice rewrites to list so the polyfilled
        // ParamSpec receives the right shape at runtime
        check(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, str)]
            "},
            indoc! {"
                from typing import ParamSpec, Generic
                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
                A[[int, str]]
            "},
        );
    }

    #[test]
    fn parameters_subscript_with_markers() {
        // `(int, str, /, name: str)` → `[int, str, Any]` — `/` dropped,
        // named-only field becomes `Any` since runtime ParamSpec only
        // carries positional types
        check(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, str, /, name: str)]
            "},
            indoc! {"
                from typing import Any, ParamSpec, Generic
                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
                A[[int, str, Any]]
            "},
        );
    }

    #[test]
    fn parameters_subscript_with_markers_native_312() {
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, str, /, name: str)]
            "},
            indoc! {"
                from typing import Any
                class A[**P]: ...
                A[[int, str, Any]]
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_subscript_named_only() {
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(/, x: int)]
            "},
            indoc! {"
                from typing import Any
                class A[**P]: ...
                A[[Any]]
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_subscript_double_star_with_type() {
        // `**: T` (anonymous kwargs catch-all) drops in lowering since the
        // runtime ParamSpec list has no kwargs slot
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, **: str)]
            "},
            indoc! {"
                class A[**P]: ...
                A[[int]]
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_subscript_variadic() {
        // `*: T` (anonymous variadic) — encoded as Starred in elts. lowered
        // to `Any` in paramspec list since runtime form has no variadic slot
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, *: str)]
            "},
            indoc! {"
                from typing import Any
                class A[**P]: ...
                A[[int, Any]]
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_subscript_native_312() {
        check_at(
            indoc! {"
                class A[P: (*: *, **: *)]: ...
                A[(int, str)]
            "},
            indoc! {"
                class A[**P]: ...
                A[[int, str]]
            "},
            PythonVersion::PY312,
        );
    }

    #[test]
    fn parameters_function_polyfill() {
        check(
            indoc! {"
                def f[P: (*: *, **: *)](): ...
            "},
            indoc! {"
                from typing import ParamSpec
                _P = ParamSpec(\"_P\")
                def f(): ...
            "},
        );
    }

    /// the top-parameters bound is structural, so it pulls in no import of its own and leaves
    /// the module's existing `typing` imports alone
    #[test]
    fn parameters_bound_needs_no_import() {
        check(
            indoc! {"
                from typing import TypeVar

                class A[P: (*: *, **: *)]: ...
            "},
            indoc! {"
                from typing import ParamSpec, Generic
                from typing import TypeVar

                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
            "},
        );
    }

    #[test]
    fn by_constraints_keyword_is_constraints() {
        // In .by files, T: constraints (int, str) is constraints.
        check(
            "class Foo[T: constraints (int, str)]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", int, str)
                class Foo(Generic[_T]): ...
            "},
        );
    }
}
