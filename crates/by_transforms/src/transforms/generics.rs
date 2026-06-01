use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    Expr, Stmt, StmtClassDef, StmtFunctionDef, StmtImportFrom, StmtTypeAlias, TypeParam,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::config::Config;
use crate::transforms::just_float;
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
    /// names of classes/functions whose first type parameter is a `Parameters`
    /// bound (i.e. `class A[P: Parameters]`). subscript sites for these
    /// targets get tuple slices rewritten to list form so paramspec
    /// substitution at runtime accepts them
    parameters_targets: HashSet<String>,
    /// set when a Parameters spec lowering used `Any` for a named-only field
    pub(crate) needed_imports_any: bool,
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
    pub(crate) fn new(source: &'src str, types: &'src dyn TypeInfo, config: Config) -> Self {
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
        }
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

                    // `T: Parameters` → emit a ParamSpec rather than a TypeVar
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
                                just_float::rewrite_type_expr(self.source, self.types, bound)
                                    .unwrap_or_else(|| self.src(bound.range()).to_owned())
                            };
                            extra_args.push(format!("bound={bound_src}"));
                        }
                    }

                    if let Some(default) = &tv.default {
                        let default_src =
                            just_float::rewrite_type_expr(self.source, self.types, default)
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
                    // `T: Parameters` → `**T` (PEP 695 paramspec syntax)
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
            return;
        };
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(class.name.id.as_str().to_owned());
        }
        // PEP 695 class type params are native syntax in 3.12+
        if self.config.min_version >= PythonVersion::PY312 {
            self.strip_constraints_keyword(&tp.type_params);
            return;
        }

        let (generic_args, defs, rename_map) = self.process_type_params(&tp.type_params);
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

    fn process_function(&mut self, func: &StmtFunctionDef) {
        let Some(tp) = &func.type_params else {
            return;
        };
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(func.name.id.as_str().to_owned());
        }
        // PEP 695 function type params are native syntax in 3.12+
        if self.config.min_version >= PythonVersion::PY312 {
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
        if self.config.min_version >= PythonVersion::PY312 {
            if let Some(tp) = &alias.type_params {
                self.strip_constraints_keyword(&tp.type_params);
            }
            return;
        }

        let name_src = self.src(alias.name.range()).to_owned();
        let raw_value_src = self.src(alias.value.range()).to_owned();
        // Pull in the literal-types + just-float rewrite for the RHS — our
        // `alias.range()` edit subsumes anything those emitted on the value
        // alone, so we have to splice the rewrite into our output.
        let literal_rewrite = just_float::rewrite_type_expr(self.source, self.types, &alias.value);

        let (type_params_arg, defs, value_src) = if let Some(tp) = &alias.type_params {
            let (generic_args, type_defs, rename_map) = self.process_type_params(&tp.type_params);

            // Apply renames inside the value expression inline (value is
            // subsumed by the alias.range() edit so can't be emitted globally).
            //
            // Combining renames with the literal rewrite needs care: the
            // literal rewrite emits a single replacement covering the whole
            // value, so any rename edits inside its range would overlap and
            // be lost. For now we use the literal rewrite when one exists
            // (typical case: the value has no type-param references), and
            // fall back to renames-only otherwise.
            let value_src = if let Some(rewrite) = &literal_rewrite {
                rewrite.clone()
            } else {
                let mut value_renames: Vec<Fix> = Vec::new();
                rename_in_expr(&alias.value, &rename_map, &mut value_renames);
                apply_renames_in_slice(&raw_value_src, alias.value.range().start(), &value_renames)
            };

            // TypeVarTuple entries have a leading `*` in generic_args (for
            // Generic[*_Ts]) but `type_params=` wants the bare name.
            let param_names: Vec<&str> = generic_args
                .iter()
                .map(|s| s.trim_start_matches('*'))
                .collect();
            let trailing = if param_names.len() == 1 { "," } else { "" };
            let tps = format!(", type_params=({}{})", param_names.join(", "), trailing);

            (tps, type_defs, value_src)
        } else {
            (
                String::new(),
                Vec::new(),
                literal_rewrite.unwrap_or(raw_value_src),
            )
        };

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
    /// Strips `Parameters` from a `from typing import …` line. Parameters is
    /// a basedpython surface form; the import has no runtime equivalent so
    /// it must not appear in the lowered Python output.
    fn strip_parameters_import(&mut self, node: &StmtImportFrom) {
        if node.level > 0 {
            return;
        }
        let Some(module) = &node.module else {
            return;
        };
        if module.id.as_str() != "typing" {
            return;
        }
        let mut keep: Vec<String> = Vec::new();
        let mut found = false;
        for alias in &node.names {
            let name = alias.name.id.as_str();
            if name == "Parameters" && alias.asname.is_none() {
                found = true;
                continue;
            }
            let formatted = match &alias.asname {
                Some(asname) => format!("{name} as {}", asname.id.as_str()),
                None => name.to_owned(),
            };
            keep.push(formatted);
        }
        if !found {
            return;
        }
        let replacement = if keep.is_empty() {
            // drop the entire line including its trailing newline
            let line_end = self.line_end_of(node.range().end());
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    node.range().start(),
                    line_end,
                ))));
            return;
        } else {
            format!("from typing import {}", keep.join(", "))
        };
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            node.range(),
        )));
    }

    fn line_end_of(&self, pos: TextSize) -> TextSize {
        let offset = usize::from(pos);
        let rest = &self.source[offset..];
        let extra = rest.find('\n').map_or(rest.len(), |i| i + 1);
        TextSize::from(u32::try_from(offset + extra).expect("offset fits u32"))
    }

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
            Stmt::ImportFrom(imp) => self.strip_parameters_import(imp),
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

fn is_parameters_bound(bound: &Expr) -> bool {
    match bound {
        Expr::Name(n) => n.id.as_str() == "Parameters",
        Expr::Attribute(a) => {
            a.attr.id.as_str() == "Parameters"
                && matches!(a.value.as_ref(), Expr::Name(m) if m.id.as_str() == "typing")
        }
        _ => false,
    }
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
        _ => {}
    }
}

fn apply_renames_in_slice(text: &str, text_start: TextSize, renames: &[Fix]) -> String {
    let base = usize::from(text_start);
    let mut local: Vec<(usize, usize, &str)> = renames
        .iter()
        .flat_map(Fix::edits)
        .filter_map(|e| {
            let lo = usize::from(e.start()).checked_sub(base)?;
            let hi = usize::from(e.end()).checked_sub(base)?;
            (hi <= text.len()).then_some((lo, hi, e.content().unwrap_or_default()))
        })
        .collect();
    local.sort_by_key(|&(lo, ..)| std::cmp::Reverse(lo));
    let mut result = text.to_owned();
    for (lo, hi, new) in local {
        result.replace_range(lo..hi, new);
    }
    result
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
        let mut inner = GenericPolyfill::new(self.source, types, self.config.clone());
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
        // In basedpython, `T: (int, str)` means bound=(int, str), NOT positional
        // constraints. Use `T: constraints(int, str)` for that.
        check(
            indoc! {"
                class Foo[T: (int, str)]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=(int, str))
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
        // In .by files, T: (int, str) is an upper bound (tuple type), not constraints.
        check(
            "class Foo[T: (int, str)]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=(int, str))
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn parameters_bound_polyfill() {
        check(
            indoc! {"
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters
                class A[P: Parameters]: ...
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
                from typing import Parameters
                class A[P: Parameters]: ...
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
                from typing import Parameters
                class A[P: Parameters]: ...
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
                from typing import Parameters

                class A[P: Parameters]: ...
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
                from typing import Parameters
                def f[P: Parameters](): ...
            "},
            indoc! {"
                from typing import ParamSpec
                _P = ParamSpec(\"_P\")
                def f(): ...
            "},
        );
    }

    #[test]
    fn parameters_import_kept_when_other_names_present() {
        // only the `Parameters` name is stripped; siblings stay
        check(
            indoc! {"
                from typing import Parameters, TypeVar

                class A[P: Parameters]: ...
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
