use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_body, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtClassDef, StmtFunctionDef, StmtTypeAlias, TypeParam};
use ruff_text_size::{Ranged, TextRange, TextSize};

use crate::config::Config;
use crate::transforms::callable::lower_type_expr_full;
use crate::transforms::repeated_underscore::WrittenNames;
use crate::transforms::type_expr_walker::RootKind;
use crate::type_info::TypeInfo;
use ruff_python_ast::PythonVersion;

/// Polyfills PEP 695 generic syntax (Python 3.12+) and `type` alias statements.
///
/// - `class Foo[T, S](Base):` → `class Foo(Base, Generic[_T, _S]):` + `TypeVar` defs
/// - `def f[T](x: T) -> T:` → `def f(x: T) -> T:` + `TypeVar` defs
/// - `type Alias = T` → `Alias: TypeAlias = T`
pub(crate) struct GenericPolyfill<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    types: &'src dyn TypeInfo,
    config: Config,
    edits: Vec<Fix>,
    // Imports to inject at the top of the file.
    needed_imports: ImportNeeds,
    /// the names the typing constructors this pass writes are called by
    constructors: Constructors,
    /// `TypeVar` definitions already emitted at module scope. Polyfilling each
    /// generic class/function emits its own `_T = TypeVar("_T")` line; without
    /// dedup, a module with several generics over the same name produces
    /// repeated identical declarations (and an `F811 redefinition` warning).
    emitted_typevar_defs: std::collections::HashSet<String>,
    /// how each type-parameter list in the module is lowered, and the name each polyfilled
    /// parameter is declared under
    decided: PolyfilledTypeParams,
    /// names of classes/functions whose first type parameter has a top-parameters
    /// bound (i.e. `class A[P: (*: *, **: *)]`). subscript sites for these
    /// targets get tuple slices rewritten to list form so paramspec
    /// substitution at runtime accepts them
    parameters_targets: HashSet<String>,
    /// set when a Parameters spec lowering used `Any` for a named-only field
    needed_imports_any: bool,
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
    /// the `T`→`_T` maps of the type-parameter lists this pass is currently inside,
    /// outermost first. a bound or default may name a parameter of an enclosing list
    /// (`class Owner[T]: def narrow[U: T]`), and that name is mangled by the enclosing
    /// list, not by the one being processed
    enclosing_renames: Vec<HashMap<String, String>>,
    /// `(range, rendered)` for every symbolic fold in the module. A fold inside a
    /// statement this pass replaces wholesale is dropped unless spliced in here
    symbolic_substitutions: Vec<(TextRange, String)>,
    /// text edits earlier passes already emitted. one of these covering a type
    /// expression hides the source bytes our typevar rename would have patched
    /// — the arrow lowering's `(T) -> R` → `Callable[[T], R]` is the common case
    /// — so the covered expression is re-rendered through the rename instead,
    /// and the pending edit is superseded
    pending_edits: Vec<(TextRange, String)>,
    /// ranges of `pending_edits` this pass re-rendered; the driver drops them so
    /// the stale un-renamed text cannot win the overlap race
    superseded: Vec<TextRange>,
    /// the `TypeVar` definitions a polyfilled `class` / `def` needs, keyed on the
    /// start of the line its definition begins on
    ///
    /// these are statements, so they go through the driver's statement-insert
    /// channel rather than an ordinary text edit: that channel leads every other
    /// insertion at the same offset, which keeps a decorator another lowering
    /// writes there — the `raises` runtime guard on a top-level `def` — below
    /// them. a statement between a decorator and its `def` is not python at all
    statement_prefixes: Vec<(TextSize, String)>,
}

#[derive(Default)]
#[expect(clippy::struct_excessive_bools)]
pub(crate) struct ImportNeeds {
    typevar: bool,
    generic: bool,
    typevar_tuple: bool,
    unpack: bool,
    paramspec: bool,
    typealias_type: bool,
}

impl ImportNeeds {
    /// Build the import lines to prepend to the file.
    fn into_lines(self, constructors: &Constructors, written: WrittenNames) -> Vec<String> {
        // one line per module, in the order each is first asked for
        let mut modules: Vec<(&'static str, Vec<&'static str>)> = Vec::new();
        for (needed, constructor) in [
            (self.typevar, &constructors.type_var),
            (self.typevar_tuple, &constructors.type_var_tuple),
            (self.unpack, &constructors.unpack),
            (self.paramspec, &constructors.param_spec),
            (self.generic, &constructors.generic),
            (self.typealias_type, &constructors.type_alias_type),
        ] {
            if !needed {
                continue;
            }
            match modules
                .iter_mut()
                .find(|(module, _)| *module == constructor.module)
            {
                Some((_, names)) => names.push(constructor.name),
                None => modules.push((constructor.module, vec![constructor.name])),
            }
        }
        // `typing` ahead of `typing_extensions`, as a reader expects
        modules.sort_by_key(|(module, _)| *module != "typing");
        modules
            .into_iter()
            .map(|(module, names)| written.import_from(module, &names))
            .collect()
    }
}

/// a typing constructor the polyfill calls: the module it comes from, and the name it is
/// called by there. one the module binds is imported under a name it does not: the module's
/// own binding need not be the one the polyfill needs. `from typing import TypeVar` binds a
/// `TypeVar` that takes no `default=` below 3.13, and nothing stops a module binding `Generic`
/// to a class of its own
struct Constructor {
    module: &'static str,
    name: &'static str,
    local: String,
}

impl Constructor {
    fn new(written: WrittenNames, module: &'static str, name: &'static str) -> Self {
        Self {
            module,
            name,
            local: written.imported(module, name),
        }
    }
}

impl std::fmt::Display for Constructor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.local)
    }
}

/// the typing constructors the polyfill calls
struct Constructors {
    type_var: Constructor,
    generic: Constructor,
    type_var_tuple: Constructor,
    unpack: Constructor,
    param_spec: Constructor,
    type_alias_type: Constructor,
}

impl Constructors {
    /// `typing`'s own `TypeVar`, `ParamSpec` and `TypeVarTuple` take no `default=` below 3.13,
    /// so a module that declares one with a default calls `typing_extensions`'
    fn new(written: WrittenNames, config: &Config, decided: &PolyfilledTypeParams) -> Self {
        let with_default = |declares_default: bool| {
            if declares_default && config.min_version < PythonVersion::PY313 {
                "typing_extensions"
            } else {
                "typing"
            }
        };
        Self {
            type_var: Constructor::new(written, with_default(decided.defaults.type_var), "TypeVar"),
            generic: Constructor::new(written, "typing", "Generic"),
            type_var_tuple: Constructor::new(
                written,
                with_default(decided.defaults.type_var_tuple),
                "TypeVarTuple",
            ),
            unpack: Constructor::new(written, "typing", "Unpack"),
            param_spec: Constructor::new(
                written,
                with_default(decided.defaults.param_spec),
                "ParamSpec",
            ),
            type_alias_type: Constructor::new(written, "typing_extensions", "TypeAliasType"),
        }
    }
}

/// the pieces a polyfilled type-parameter list lowers to
struct ProcessedTypeParams {
    /// each parameter as it is written *inside a subscript* — a variadic carries
    /// its `*Ts` / `Unpack[Ts]` spelling here
    generic_args: Vec<String>,
    /// each parameter's bare mangled name, for the positions that want the
    /// `TypeVar` / `TypeVarTuple` / `ParamSpec` object itself rather than a
    /// subscript element (`TypeAliasType(..., type_params=…)`)
    param_names: Vec<String>,
    /// the `_T = TypeVar("_T")` definition lines to emit above the statement
    defs: Vec<String>,
    /// `source_name → mangled_name`
    renames: HashMap<String, String>,
}

impl<'src> GenericPolyfill<'src> {
    fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        types: &'src dyn TypeInfo,
        config: Config,
        symbolic_substitutions: Vec<(TextRange, String)>,
        pending_edits: Vec<(TextRange, String)>,
        decided: PolyfilledTypeParams,
    ) -> Self {
        let constructors = Constructors::new(written, &config, &decided);
        Self {
            source,
            written,
            types,
            config,
            edits: Vec::new(),
            needed_imports: ImportNeeds::default(),
            constructors,
            emitted_typevar_defs: std::collections::HashSet::new(),
            decided,
            parameters_targets: HashSet::new(),
            needed_imports_any: false,
            generic_class_renames: HashMap::new(),
            enclosing_renames: Vec::new(),
            private_aliases: HashMap::new(),
            symbolic_substitutions,
            pending_edits,
            superseded: Vec::new(),
            statement_prefixes: Vec::new(),
        }
    }

    /// take over every rewrite that already exists inside `range`, so that a
    /// replacement covering `range` can carry them: the symbolic folds
    /// (`Dim + 1` → `int`) and the text edits earlier passes queued
    /// (`kw_subscript`'s positional rebuild, `unpack`'s `Unpack[…]`). without
    /// the second group a keyword subscript or a variadic inside a polyfilled
    /// `type` alias value or a type-parameter bound reaches the output in its
    /// `.by` spelling.
    ///
    /// **this commits**: every pending edit it returns is marked superseded, so
    /// the driver drops the original. the caller must therefore go on to emit a
    /// replacement spanning `range` — calling this to *inspect* what is inside a
    /// span would delete those edits without reinstating them
    fn subsume_within(&mut self, range: TextRange) -> Vec<(TextRange, String)> {
        let mut within: Vec<(TextRange, String)> = self
            .symbolic_substitutions
            .iter()
            .filter(|(folded, _)| range.contains_range(*folded))
            .cloned()
            .collect();
        // a zero-width entry is an insertion — `unpack` spells `*Ts` as a
        // `Unpack[` replacement plus a `]` insertion — so it has to come along
        // too, or the subsumed rewrite arrives half-applied
        for (pending, text) in &self.pending_edits {
            if range.contains_range(*pending)
                && !within
                    .iter()
                    .any(|(seen, seen_text)| seen == pending && seen_text == text)
            {
                within.push((*pending, text.clone()));
                self.superseded.push(*pending);
            }
        }
        within
    }

    /// reconcile this pass's typevar renames with the edits earlier passes
    /// already queued.
    ///
    /// a rename is a narrow edit on a `Name` node, so it is lost whenever an
    /// earlier pass replaced the whole type expression around it — the arrow
    /// lowering's `(T) -> R` → `Callable[[T], R]` is the common case, and the
    /// emitted module then raises `NameError` because the polyfill bound `_T`.
    /// the fix is to apply the rename to that pass's *replacement text* and drop
    /// the now-redundant narrow edit.
    ///
    /// `from` is the index this construct's renames start at. splitting there is
    /// what keeps an edit an *earlier* construct already reconciled from being
    /// rewritten again under this construct's map, which would undo the renames
    /// only the earlier map knew about
    fn reconcile_pending(&mut self, renames: &HashMap<String, String>, from: usize) {
        if renames.is_empty() || from >= self.edits.len() {
            return;
        }
        let mut rewritten: Vec<(TextRange, String)> = Vec::new();
        let mut kept: Vec<Fix> = Vec::new();
        for fix in self.edits.split_off(from) {
            let covered: Vec<&(TextRange, String)> = fix
                .edits()
                .iter()
                .filter_map(|edit| {
                    // the *widest* covering edit is the one that wins the overlap
                    // race, so it is the one that has to carry the rename —
                    // patching a narrower edit nested inside it is dropped in turn
                    self.pending_edits
                        .iter()
                        .filter(|(pending, _)| {
                            pending.contains_range(edit.range())
                                && !self.superseded.contains(pending)
                        })
                        .max_by_key(|(pending, _)| pending.len())
                })
                .collect();
            if covered.is_empty() {
                kept.push(fix);
                continue;
            }
            for (range, text) in covered {
                if !rewritten.iter().any(|(seen, _)| seen == range) {
                    rewritten.push((*range, apply_renames_to_rendered(text, renames)));
                }
            }
        }
        self.edits.extend(kept);
        for (range, text) in rewritten {
            self.superseded.push(range);
            self.edits
                .push(Fix::safe_edit(Edit::range_replacement(text, range)));
        }
    }

    /// pre-scan for the module's `private type` aliases so a reference inside a
    /// later alias's value can be renamed as the value is re-rendered. like the
    /// module's other private symbols, they are the ones declared outside any class
    /// or function body
    fn collect_private_aliases(&mut self, stmts: &[Stmt]) {
        struct Collect<'a, 'src>(&'a mut HashMap<String, String>, WrittenNames<'src>);
        impl<'ast> Visitor<'ast> for Collect<'_, '_> {
            fn visit_stmt(&mut self, stmt: &'ast Stmt) {
                match stmt {
                    Stmt::TypeAlias(alias)
                        if alias.is_private
                            && let Expr::Name(name) = alias.name.as_ref() =>
                    {
                        self.0
                            .insert(name.id.to_string(), self.1.module_private(&name.id));
                    }
                    Stmt::ClassDef(_) | Stmt::FunctionDef(_) => {}
                    _ => ruff_python_ast::visitor::walk_stmt(self, stmt),
                }
            }
        }
        let mut collect = Collect(&mut self.private_aliases, self.written);
        for stmt in stmts {
            collect.visit_stmt(stmt);
        }
    }

    /// Skip `TypeVar` declarations already written into the suite being visited. a name is
    /// bound in one suite only, so a definition seen before was written into this one
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
                    return format!(
                        "*{}[{}, ...]",
                        self.written.builtin("tuple"),
                        self.src(named.value.range())
                    );
                }
                self.src(named.value.range()).to_owned()
            }
            Expr::Starred(s) => {
                if matches!(s.value.as_ref(), Expr::Starred(_)) {
                    return String::new();
                }
                format!(
                    "*{}[{}, ...]",
                    self.written.builtin("tuple"),
                    self.src(s.value.range())
                )
            }
            _ => self.src(elt.range()).to_owned(),
        }
    }

    fn line_start_of(&self, pos: TextSize) -> (TextSize, &str) {
        let start = super::source_util::line_start(self.source, pos);
        let indent = super::source_util::line_indent(self.source, pos);
        (start, indent)
    }

    /// `default`, a type parameter's default, as the python the `default=` argument of its
    /// declaration is written with. `visible` renames the type parameters in scope
    fn default_arg(&mut self, default: &Expr, visible: &HashMap<String, String>) -> String {
        let rendered = lower_type_expr_full(
            self.source,
            self.written,
            self.types,
            default,
            &self.subsume_within(default.range()),
            &self.config,
            RootKind::Evaluated,
        )
        .unwrap_or_else(|| self.src(default.range()).to_owned());
        apply_renames_to_rendered(&rendered, visible)
    }

    /// the `default=` argument of a `ParamSpec`: a list of types, `...`, or another parameter
    /// specification
    fn param_spec_default(&mut self, default: &Expr, visible: &HashMap<String, String>) -> String {
        match default {
            Expr::Tuple(tuple) if tuple.parenthesized => {
                self.parameter_shape_list(tuple, Some(visible))
            }
            Expr::List(list) => {
                let elements: Vec<String> = list
                    .elts
                    .iter()
                    .map(|element| self.default_arg(element, visible))
                    .collect();
                format!("[{}]", elements.join(", "))
            }
            other => self.default_arg(other, visible),
        }
    }

    /// write the declaration of a `ParamSpec` bound to `mangled`, with `default` as its
    /// `default=` argument
    fn declare_param_spec(
        &mut self,
        mangled: &str,
        default: Option<String>,
        defs: &mut Vec<String>,
    ) {
        let arguments = default
            .map(|default| format!(", default={default}"))
            .unwrap_or_default();
        defs.push(format!(
            "{mangled} = {}(\"{mangled}\"{arguments})",
            self.constructors.param_spec
        ));
        self.needed_imports.paramspec = true;
    }

    /// lower `params`, each declared under the name at the same position in `names`
    fn process_type_params(
        &mut self,
        params: &[TypeParam],
        names: &[String],
        qualified: &HashMap<String, String>,
    ) -> ProcessedTypeParams {
        // a bound that reads a class's name through the class is written ahead of the class,
        // so it is a string, which `TypeVar` keeps as a forward reference
        let through_classes = |text: String| {
            let read_through = apply_renames_to_rendered(&text, qualified);
            if read_through == text {
                text
            } else {
                super::source_util::python_string_literal(&read_through)
            }
        };
        let mut generic_args: Vec<String> = Vec::new();
        let mut param_names: Vec<String> = Vec::new();
        let mut defs: Vec<String> = Vec::new();
        let mut renames: HashMap<String, String> = HashMap::new();
        // a bound or default may name a type parameter that is already in scope — an earlier
        // entry in this list, or one of an enclosing list. those names are mangled, so the text
        // spliced into `bound=` / `default=` has to be mangled with them or the emitted module
        // raises `NameError` on import. an inner list shadows an outer one, and this list's own
        // entries are added as they are processed, so a bound only ever sees names declared
        // before it
        let mut visible: HashMap<String, String> = HashMap::new();
        for enclosing in &self.enclosing_renames {
            visible.extend(enclosing.iter().map(|(k, v)| (k.clone(), v.clone())));
        }

        for (param, mangled) in params.iter().zip(names) {
            let mangled = mangled.clone();
            match param {
                TypeParam::TypeVar(tv) => {
                    let name = tv.name.id.as_str();

                    // top-parameters bound → emit a ParamSpec rather than a TypeVar
                    // so the polyfilled output behaves like `**T` at runtime
                    if let Some(bound) = &tv.bound
                        && is_parameters_bound(bound)
                    {
                        let default = tv
                            .default
                            .as_deref()
                            .map(|default| self.param_spec_default(default, &visible));
                        self.declare_param_spec(&mangled, default, &mut defs);
                        renames.insert(name.to_owned(), mangled.clone());
                        visible.insert(name.to_owned(), mangled.clone());
                        param_names.push(mangled.clone());
                        generic_args.push(mangled);
                        continue;
                    }

                    let mut extra_args: Vec<String> = Vec::new();

                    if let Some(bound) = &tv.bound {
                        // the type mapping `T in (int, str)` → positional `TypeVar` args.
                        // Everything else, including tuple bounds, → bound=.
                        // In basedpython, `T: (int, str)` means bound=(int, str), not
                        // positional constraints — the mapping is written with `in`
                        if tv.is_type_mapping {
                            let inner = match bound.as_ref() {
                                Expr::Tuple(t) if t.parenthesized => t
                                    .elts
                                    .iter()
                                    .map(|e| self.src(e.range()))
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                other => self.src(other.range()).to_owned(),
                            };
                            if !inner.is_empty() {
                                extra_args.push(apply_renames_to_rendered(&inner, &visible));
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
                                    format!("{}[()]", self.written.builtin("tuple"))
                                } else if t.elts.len() == 1
                                    && let Some(rest) = inner.strip_prefix("*")
                                {
                                    rest.to_owned()
                                } else {
                                    format!("{}[{inner}]", self.written.builtin("tuple"))
                                }
                            } else {
                                lower_type_expr_full(
                                    self.source,
                                    self.written,
                                    self.types,
                                    bound,
                                    &self.subsume_within(bound.range()),
                                    &self.config,
                                    RootKind::Evaluated,
                                )
                                .unwrap_or_else(|| self.src(bound.range()).to_owned())
                            };
                            extra_args.push(format!(
                                "bound={}",
                                through_classes(apply_renames_to_rendered(&bound_src, &visible))
                            ));
                        }
                    }

                    if let Some(default) = &tv.default {
                        extra_args.push(format!(
                            "default={}",
                            through_classes(self.default_arg(default, &visible))
                        ));
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

                    renames.insert(name.to_owned(), mangled.clone());
                    visible.insert(name.to_owned(), mangled.clone());
                    let mut args: Vec<String> = vec![format!("\"{mangled}\"")];
                    args.extend(extra_args);
                    let def = format!(
                        "{mangled} = {}({})",
                        self.constructors.type_var,
                        args.join(", ")
                    );

                    self.needed_imports.typevar = true;
                    param_names.push(mangled.clone());
                    generic_args.push(mangled);
                    defs.push(def);
                }

                TypeParam::TypeVarTuple(tvt) => {
                    let name = tvt.name.id.as_str();
                    // a keyword argument cannot be starred, so the default `*tuple[int, str]`
                    // is passed as `Unpack[tuple[int, str]]`, the spelling the call accepts
                    let default = tvt.default.as_deref().map(|default| {
                        let unpacked = match default {
                            Expr::Starred(starred) => starred.value.as_ref(),
                            other => other,
                        };
                        let rendered = self.default_arg(unpacked, &visible);
                        if matches!(default, Expr::Starred(_)) {
                            self.needed_imports.unpack = true;
                            format!("{}[{rendered}]", self.constructors.unpack)
                        } else {
                            rendered
                        }
                    });
                    renames.insert(name.to_owned(), mangled.clone());
                    visible.insert(name.to_owned(), mangled.clone());
                    let arguments = default
                        .map(|default| format!(", default={default}"))
                        .unwrap_or_default();
                    defs.push(format!(
                        "{mangled} = {}(\"{mangled}\"{arguments})",
                        self.constructors.type_var_tuple
                    ));
                    self.needed_imports.typevar_tuple = true;
                    self.needed_imports.unpack = true;
                    // star-in-subscript (`Generic[*T]`) is only valid syntax
                    // on Python 3.11+; below that, emit the equivalent
                    // `Unpack[T]` form so the polyfilled output parses
                    let arg = if self.config.min_version >= PythonVersion::PY311 {
                        format!("*{mangled}")
                    } else {
                        format!("{}[{mangled}]", self.constructors.unpack)
                    };
                    param_names.push(mangled);
                    generic_args.push(arg);
                }

                TypeParam::ParamSpec(ps) => {
                    let name = ps.name.id.as_str();
                    let default = ps
                        .default
                        .as_deref()
                        .map(|default| self.param_spec_default(default, &visible));
                    self.declare_param_spec(&mangled, default, &mut defs);
                    renames.insert(name.to_owned(), mangled.clone());
                    visible.insert(name.to_owned(), mangled.clone());
                    param_names.push(mangled.clone());
                    generic_args.push(mangled);
                }
            }
        }

        ProcessedTypeParams {
            generic_args,
            param_names,
            defs,
            renames,
        }
    }

    /// For 3.12+ pass-through, rewrite the type mapping `T in (int, str)` as
    /// `T: (int, str)`, which is how python spells the same constraint set.
    ///
    /// Also rewrites `.by` tuple bounds `T: (int, str)` → `T: tuple[int, str]`
    /// because Python 3.12+ treats `T: (int, str)` as positional constraints,
    /// not a tuple bound.
    fn lower_type_param_bounds(&mut self, params: &[TypeParam]) {
        for param in params {
            if let TypeParam::TypeVar(tv) = param {
                if let Some(bound) = &tv.bound {
                    // top-parameters bound → `**T` (PEP 695 paramspec syntax)
                    if is_parameters_bound(bound) {
                        let name = tv.name.id.as_str();
                        let default = tv
                            .default
                            .as_deref()
                            .map(|default| {
                                format!(" = {}", self.param_spec_default(default, &HashMap::new()))
                            })
                            .unwrap_or_default();
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            format!("**{name}{default}"),
                            param.range(),
                        )));
                        continue;
                    }
                    if tv.is_type_mapping {
                        // the ` in ` between the name and the mapping becomes python's `: `
                        let edit_range =
                            TextRange::new(tv.name.range().end(), bound.range().start());
                        self.edits.push(Fix::safe_edit(Edit::range_replacement(
                            ": ".to_owned(),
                            edit_range,
                        )));
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
                            format!("{}[()]", self.written.builtin("tuple"))
                        } else if t.elts.len() == 1
                            && let Some(rest) = inner.strip_prefix("*")
                        {
                            // pure variadic `(*: T)` → `tuple[T, ...]`
                            rest.to_owned()
                        } else {
                            format!("{}[{inner}]", self.written.builtin("tuple"))
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

    fn process_class(&mut self, class: &StmtClassDef) -> HashMap<String, String> {
        let Some(tp) = &class.type_params else {
            // a based-enum variant lowers to a module-level subclass of the enum
            // with no type params of its own; rename the enum's params in its
            // field annotations using the enum's recorded map
            self.rename_variant_of_generic_enum(class);
            return HashMap::new();
        };
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(class.name.id.as_str().to_owned());
        }
        // a `protocol P[T]:` with no explicit bases defers its `Protocol` base to
        // this pass, which owns the base list for a type-param class (modifiers
        // skips it to avoid two competing base-parens around the type params)
        let deferred_protocol = class.arguments.is_none() && self.has_protocol_marker(class);
        let Some((names, anchor)) = self.decided.declared(tp) else {
            self.lower_type_param_bounds(&tp.type_params);
            if deferred_protocol {
                // keep the native `[T]`, append the base after it: `[T](Protocol)`
                self.edits.push(Fix::safe_edit(Edit::insertion(
                    format!("({})", self.written.imported("typing", "Protocol")),
                    tp.range().end(),
                )));
            }
            return HashMap::new();
        };

        let ProcessedTypeParams {
            generic_args,
            defs,
            renames: rename_map,
            ..
        } = self.process_type_params(&tp.type_params, &names, &self.decided.qualified(tp));
        // record for module-level variant subclasses that reference these params
        self.generic_class_renames
            .insert(class.name.id.as_str().to_owned(), rename_map.clone());
        let generic_str = format!("{}[{}]", self.constructors.generic, generic_args.join(", "));
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
                format!(
                    "({}, {generic_str})",
                    self.written.imported("typing", "Protocol")
                ),
                tp.range(),
            )));
        } else {
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                format!("({generic_str})"),
                tp.range(),
            )));
        }

        // Insert TypeVar definitions before the statement they are declared ahead of.
        let (line_start, indent) = self.line_start_of(anchor);
        let indent = indent.to_owned();
        let prefix = self.dedupe_defs(&defs, &indent);
        if !prefix.is_empty() {
            self.statement_prefixes.push((line_start, prefix));
        }

        // Rename type param references in class body.
        let mark = self.edits.len();
        for stmt in &class.body {
            rename_in_stmt(stmt, &rename_map, &mut self.edits);
        }
        self.reconcile_pending(&rename_map, mark);
        rename_map
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

    fn process_function(&mut self, func: &StmtFunctionDef) -> HashMap<String, String> {
        let Some(tp) = &func.type_params else {
            return HashMap::new();
        };
        // basedpython: a `type def` is erased by its own pass, so polyfilling its
        // type parameters would leave an orphan `TypeVar` behind
        if ruff_python_ast::helpers::is_type_def(func) {
            return HashMap::new();
        }
        if has_parameters_bound(&tp.type_params) {
            self.parameters_targets
                .insert(func.name.id.as_str().to_owned());
        }
        let Some((names, anchor)) = self.decided.declared(tp) else {
            self.lower_type_param_bounds(&tp.type_params);
            return HashMap::new();
        };

        let ProcessedTypeParams {
            defs,
            renames: rename_map,
            ..
        } = self.process_type_params(&tp.type_params, &names, &self.decided.qualified(tp));

        // Remove `[T, ...]` from the function signature. a list of holes alone is the
        // parser's, and there are no brackets in the source to remove: its range is the
        // parameter list's
        if !tp.type_params.iter().all(is_some_hole) {
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(tp.range())));
        }

        // Insert TypeVar definitions before the statement they are declared ahead of.
        let (line_start, indent) = self.line_start_of(anchor);
        let indent = indent.to_owned();
        let prefix = self.dedupe_defs(&defs, &indent);
        if !prefix.is_empty() {
            self.statement_prefixes.push((line_start, prefix));
        }

        // Rename type param references in parameter annotations, return type, and body.
        let mark = self.edits.len();
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
        if let Some(vararg) = &func.parameters.vararg
            && let Some(ann) = &vararg.annotation
        {
            rename_in_expr(ann, &rename_map, &mut self.edits);
        }
        if let Some(kwarg) = &func.parameters.kwarg
            && let Some(ann) = &kwarg.annotation
        {
            rename_in_expr(ann, &rename_map, &mut self.edits);
        }
        if let Some(ret) = &func.returns {
            rename_in_expr(ret, &rename_map, &mut self.edits);
        }
        // the body reads a parameter under its own name, which a type parameter of that name
        // does not reach: the parameter is bound in the body's scope, the type parameter in
        // the one around it. a `some` hole always has its parameter's name
        let mut body_renames = rename_map.clone();
        for parameter in &func.parameters {
            body_renames.remove(parameter.name().as_str());
        }
        self.reconcile_pending(&rename_map, mark);
        let mark = self.edits.len();
        for stmt in &func.body {
            rename_in_stmt(stmt, &body_renames, &mut self.edits);
        }
        self.reconcile_pending(&body_renames, mark);
        // what the rest of the body sees, a nested definition among it
        body_renames
    }

    fn process_type_alias(&mut self, alias: &StmtTypeAlias) {
        // `type Point = tuple[float, float]`
        //   → `Point = TypeAliasType("Point", tuple[float, float])`
        let names = match &alias.type_params {
            Some(tp) => match self.decided.declared(tp) {
                Some((names, _)) => Some(names),
                None => {
                    self.lower_type_param_bounds(&tp.type_params);
                    return;
                }
            },
            None if is_native(&self.config, &[]) => return,
            None => None,
        };

        // this replacement subsumes `modifiers`' `private ` deletion and the
        // rename of the definition site, so the private name has to be applied
        // here instead: a module's own symbol as the module renames it, and a
        // class's own member as the class body spells it. any other alias keeps
        // its name, as the native `type` statement of a class body does
        let class_member_name = match alias.name.as_ref() {
            Expr::Name(name) => self.types.class_body_member_name(name),
            _ => None,
        };
        let name_src = if let Some(renamed) = class_member_name {
            renamed
        } else if alias.is_private && self.enclosing_renames.is_empty() {
            self.written.module_private(self.src(alias.name.range()))
        } else {
            self.src(alias.name.range()).to_owned()
        };
        let raw_value_src = self.src(alias.value.range()).to_owned();

        // references to a `private type` alias declared elsewhere in the module
        // also sit inside the subsumed value, so they are renamed here too, and so
        // is a reference to a restricted member of the class the alias is in
        let mut rename_map = self.private_aliases.clone();
        rename_map.extend(class_member_references(&alias.value, self.types));

        let (type_params_arg, defs) = if let Some(tp) = &alias.type_params
            && let Some(names) = names
        {
            let ProcessedTypeParams {
                param_names,
                defs: type_defs,
                renames: tp_renames,
                ..
            } = self.process_type_params(&tp.type_params, &names, &self.decided.qualified(tp));
            rename_map.extend(tp_renames);

            // `type_params=` wants each parameter object itself, so a variadic
            // goes in bare — neither the `*_Ts` nor the `Unpack[_Ts]` spelling
            // a subscript would use is accepted there
            let trailing = if param_names.len() == 1 { "," } else { "" };
            let tps = format!(", type_params=({}{})", param_names.join(", "), trailing);

            (tps, type_defs)
        } else {
            (String::new(), Vec::new())
        };

        // basedpython: a match type has no value expression — `alias.value` is the subject
        // its `case` blocks are matched against. the checker resolves every application, so
        // the runtime alias stands for "whatever it worked out", the same `object` the
        // dedicated pass writes on the native path
        if !alias.cases.is_empty() {
            self.needed_imports.typealias_type = true;
            let (_line_start, indent) = self.line_start_of(alias.range().start());
            let indent = indent.to_owned();
            let mut replacement = self.dedupe_defs(&defs, &indent);
            let _ = write!(
                replacement,
                "{indent}{name_src} = {}(\"{name_src}\", {}{type_params_arg})",
                self.constructors.type_alias_type,
                self.written.builtin("object")
            );
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                at_statement_start(&replacement, &indent),
                alias.range(),
            )));
            return;
        }

        // Everything that rewrites part of the value has to be spliced here: our
        // `alias.range()` edit subsumes the value, so an edit another pass emitted
        // on it alone is dropped. Symbolic folds (`T.a` → `int`, `Dim + 1` → `int`)
        // and typevar / private-alias renames are collected into one substitution
        // set so the lowering below honours both, rather than picking one and
        // silently losing the other.
        let folded = self.subsume_within(alias.value.range());
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

        let value_src = lower_type_expr_full(
            self.source,
            self.written,
            self.types,
            &alias.value,
            &substitutions,
            &self.config,
            RootKind::Evaluated,
        )
        .unwrap_or(raw_value_src);

        self.needed_imports.typealias_type = true;

        let (_line_start, indent) = self.line_start_of(alias.range().start());
        let indent = indent.to_owned();

        let mut replacement = self.dedupe_defs(&defs, &indent);
        let _ = write!(
            replacement,
            "{indent}{name_src} = {}(\"{name_src}\", {value_src}{type_params_arg})",
            self.constructors.type_alias_type
        );

        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            at_statement_start(&replacement, &indent),
            alias.range(),
        )));
    }
}

/// `lines`, each written with the statement's `indent`, as a replacement for a statement:
/// its range starts after the indent the source already wrote, so the first line leaves
/// its own out
fn at_statement_start(lines: &str, indent: &str) -> String {
    lines.strip_prefix(indent).unwrap_or(lines).to_owned()
}

impl GenericPolyfill<'_> {
    /// Rewrites a tuple slice of a parameters-typed subscript to a list.
    /// `A[(int, str)]` → `A[[int, str]]` so the runtime `ParamSpec` accepts
    /// the substitution. Parameters spec syntax (`(int, str, /, name: T)`)
    /// drops the `/` and `*` markers and replaces named-only fields with
    /// `Any` since runtime `ParamSpec` only carries positional types
    /// a parameter-shape tuple (`(int, name: str)`) as the list a `ParamSpec` is specialized by
    /// or defaults to at runtime. each element is written as the source spells it, or, given
    /// the type parameters `visible` in scope, lowered as a default is
    fn parameter_shape_list(
        &mut self,
        t: &ruff_python_ast::ExprTuple,
        visible: Option<&HashMap<String, String>>,
    ) -> String {
        // the inner structure (markers, named, variadic, kwargs) doesn't map
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
                        parts.push(self.written.imported("typing", "Any"));
                        self.needed_imports_any = true;
                    } else {
                        // `name: T`
                        parts.push(self.written.imported("typing", "Any"));
                        self.needed_imports_any = true;
                    }
                }
                Expr::Starred(s) => {
                    if matches!(s.value.as_ref(), Expr::Starred(_)) {
                        // `**: T` — drop
                        continue;
                    }
                    // `*: T`
                    parts.push(self.written.imported("typing", "Any"));
                    self.needed_imports_any = true;
                }
                _ => {
                    parts.push(match visible {
                        Some(visible) => self.default_arg(elt, visible),
                        None => self.src(elt.range()).to_owned(),
                    });
                }
            }
        }
        format!("[{}]", parts.join(", "))
    }

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
            let list = self.parameter_shape_list(t, None);
            self.edits
                .push(Fix::safe_edit(Edit::range_replacement(list, t.range())));
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
        // a nested type-parameter list can name a parameter of an enclosing one in its bounds
        // and defaults, and that name carries the enclosing list's mangling, so the maps stay
        // on a stack for the length of the walk through the body that declared them
        let scoped_renames = match stmt {
            Stmt::ClassDef(class) => Some(self.process_class(class)),
            Stmt::FunctionDef(func) => Some(self.process_function(func)),
            Stmt::TypeAlias(alias) => {
                self.process_type_alias(alias);
                return; // don't recurse into the alias value
            }
            _ => None,
        };
        if let Some(renames) = scoped_renames {
            self.enclosing_renames.push(renames);
            walk_stmt(self, stmt);
            self.enclosing_renames.pop();
        } else {
            walk_stmt(self, stmt);
        }
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

/// apply a type-parameter rename to text an earlier pass already rendered.
///
/// the input is always a type expression another pass built from the very
/// source expression being renamed (`(T) -> R` → `Callable[[T], R]`), so a bare
/// identifier in it is a type name. two things are not:
///
/// - an attribute (`x.T`), which names a member of the preceding expression
/// - anything inside a string. in basedpython a quoted annotation is a string
///   *literal type*, not a forward reference, so `Literal["T"]` means the text
///   `T` — rewriting it to `Literal["_T"]` would change which value the type
///   admits, and only on the polyfilled target
fn apply_renames_to_rendered(rendered: &str, renames: &HashMap<String, String>) -> String {
    if renames.is_empty() {
        return rendered.to_owned();
    }
    let mut out = String::with_capacity(rendered.len());
    let mut chars = rendered.char_indices().peekable();
    let mut quote: Option<char> = None;
    while let Some((offset, ch)) = chars.next() {
        if let Some(open) = quote {
            out.push(ch);
            if ch == '\\' {
                // an escape consumes the next char, so a `\"` cannot close
                if let Some((_, escaped)) = chars.next() {
                    out.push(escaped);
                }
            } else if ch == open {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            out.push(ch);
            continue;
        }
        if !(ch.is_alphabetic() || ch == '_') {
            out.push(ch);
            continue;
        }
        let end = rendered[offset..]
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(rendered.len(), |len| offset + len);
        let ident = &rendered[offset..end];
        while chars.peek().is_some_and(|(next, _)| *next < end) {
            chars.next();
        }
        // `x.T` names a member of `x`, not the type parameter `T`
        let is_attribute = out.trim_end_matches(char::is_whitespace).ends_with('.');
        match renames.get(ident) {
            Some(new) if !is_attribute => out.push_str(new),
            _ => out.push_str(ident),
        }
    }
    out
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

/// the name the polyfill first tries for the `TypeVar` a type parameter named `name` is
/// declared as: `_T` for `T`, spelled so that the module spells it nowhere. the definition is
/// written into the scope the generic stands in, where a name the module binds would be
/// overwritten by it
fn polyfilled_name(written: WrittenNames, name: &str) -> String {
    if name.starts_with('_') {
        written.fresh(name)
    } else {
        written.fresh(&format!("_{name}"))
    }
}

/// whether the target keeps the type-parameter list `params` as native syntax: pep 695 lists
/// need 3.12, and a pep 696 default (`[T = int]`) 3.13. a defaulted list on a 3.12 target is
/// polyfilled exactly as below 3.12
fn is_native(config: &Config, params: &[TypeParam]) -> bool {
    let required = if params.iter().any(|p| p.default().is_some()) {
        PythonVersion::PY313
    } else {
        PythonVersion::PY312
    };
    config.min_version >= required
}

/// basedpython: `some T` declares a type parameter the source writes nowhere in the list, so
/// native syntax has no place for it, and it is always declared as a `TypeVar`
fn is_some_hole(param: &TypeParam) -> bool {
    matches!(param, TypeParam::TypeVar(tv) if tv.is_some_hole)
}

/// the polyfill's decision for every type-parameter list in a module: whether it is lowered,
/// and the name the `TypeVar`, `ParamSpec` or `TypeVarTuple` each parameter of a lowered list
/// is declared under
///
/// a lowering that moves a type out of its generic — an inline protocol or an anonymous named
/// tuple hoisted to module scope — has to name the variable the polyfill declared, so it reads
/// this rather than working the name out again
pub(crate) struct PolyfilledTypeParams {
    /// how each lowered list is declared, keyed on the list's range
    lists: HashMap<TextRange, Declared>,
    /// for each list kept as native syntax whose generic module scope reaches by a path of
    /// names, the expression that reaches each parameter from module scope:
    /// `B.__type_params__[0]` for the `T` of `class B[T]`
    native: HashMap<TextRange, HashMap<String, String>>,
    /// which kinds of declaration a lowered list gives a default
    defaults: Defaults,
}

/// how the polyfill declares one type-parameter list
struct Declared {
    /// the name each parameter is declared under, in order
    names: Vec<String>,
    /// the statement the declarations are written ahead of, in its suite: the outermost `class`
    /// or `def` around the generic that stands at module scope, or the generic itself when
    /// something it declares reads a name only a scope inside that statement binds
    ///
    /// at module scope a declaration is a global, which is where everything that reads it
    /// looks: `typing.get_type_hints` resolves a method's annotations in the module's
    /// namespace, and a method's body cannot see its class's. one written in a class body
    /// reaches neither
    ///
    /// a bound that reads a name of the class around the generic moves out with it, and
    /// reads the name through the class
    anchor: TextSize,
    /// each class-level name a bound, constraint or default reads, by the path that reaches
    /// it from where the declarations are written: `Inner` in `class C` is `C.Inner`. the
    /// class does not exist yet where they are written, so what reads one of these is written
    /// as a string, which `TypeVar` keeps as a forward reference
    qualified: HashMap<String, String>,
}

/// which kinds of type parameter the polyfill declares with a default somewhere in a module
#[derive(Default, Clone, Copy)]
struct Defaults {
    type_var: bool,
    param_spec: bool,
    type_var_tuple: bool,
}

impl PolyfilledTypeParams {
    /// decide every list in `stmts`, the module `source` parses to
    pub(crate) fn decide(
        source: &str,
        written: WrittenNames,
        stmts: &[Stmt],
        config: &Config,
    ) -> Self {
        let mut decider = Decider {
            source,
            written,
            config,
            lists: HashMap::new(),
            native: HashMap::new(),
            defaults: Defaults::default(),
            signatures: HashMap::new(),
            suite: Suite::Module,
            enclosing: Vec::new(),
            scopes: Vec::new(),
            local_type_params: Vec::new(),
            module_statement: None,
        };
        for stmt in stmts {
            decider.visit_stmt(stmt);
        }
        Self {
            lists: decider.lists,
            native: decider.native,
            defaults: decider.defaults,
        }
    }

    /// what each parameter of `type_params` is written as in a type hoisted to module scope
    /// out of its generic — an inline protocol, an anonymous named tuple: the polyfill's name
    /// for it, or for a list kept as native syntax, the parameter read off its generic.
    /// `None` when the generic is not reached from module scope by a path of names — one
    /// local to a function, or renamed or replaced by a decorator
    pub(crate) fn hoisted_renames(
        &self,
        type_params: &ruff_python_ast::TypeParams,
    ) -> Option<HashMap<String, String>> {
        self.renames(type_params)
            .or_else(|| self.native.get(&type_params.range()).cloned())
    }

    /// the name each of `type_params` is declared under, in order, and the start of the
    /// statement the declarations are written ahead of, or `None` when the list is kept as
    /// native syntax
    fn declared(
        &self,
        type_params: &ruff_python_ast::TypeParams,
    ) -> Option<(Vec<String>, TextSize)> {
        self.lists
            .get(&type_params.range())
            .map(|declared| (declared.names.clone(), declared.anchor))
    }

    /// the class-level names the declarations of `type_params` read through their class
    fn qualified(&self, type_params: &ruff_python_ast::TypeParams) -> HashMap<String, String> {
        self.lists
            .get(&type_params.range())
            .map(|declared| declared.qualified.clone())
            .unwrap_or_default()
    }

    /// what each parameter of `type_params` is renamed to, or `None` when the list is kept as
    /// native syntax
    fn renames(
        &self,
        type_params: &ruff_python_ast::TypeParams,
    ) -> Option<HashMap<String, String>> {
        let declared = self.lists.get(&type_params.range())?;
        Some(
            type_params
                .iter()
                .zip(&declared.names)
                .map(|(param, name)| (param.name().id.to_string(), name.clone()))
                .collect(),
        )
    }
}

/// the statement list a definition the polyfill writes lands in, told apart by where its
/// first statement starts
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Suite {
    Module,
    Nested(TextSize),
}

/// walks a module in the order the polyfill writes its definitions, naming each parameter
struct Decider<'a> {
    source: &'a str,
    written: WrittenNames<'a>,
    config: &'a Config,
    lists: HashMap<TextRange, Declared>,
    /// see [`PolyfilledTypeParams::native`]
    native: HashMap<TextRange, HashMap<String, String>>,
    defaults: Defaults,
    /// each name a definition binds, with what the definition declares and the suite it is
    /// written into. a name is only ever bound in one suite: a definition written into a class
    /// body is not visible to a function the module declares later, and one written into an
    /// `if` does not run when the `if` does not
    signatures: HashMap<String, (String, Suite)>,
    /// the suite the statement being visited stands in
    suite: Suite,
    /// the renames of the lists enclosing the statement being visited, outermost first
    enclosing: Vec<HashMap<String, String>>,
    /// each scope around the statement being visited, outermost first: a class body, or a
    /// function's parameters and body. the module is none of them
    scopes: Vec<Scope>,
    /// the type parameters in scope declared inside one of `scopes` rather than at module
    /// scope, by the names they are written with
    local_type_params: Vec<String>,
    /// the start of the outermost statement around the one being visited that stands at
    /// module scope, and the suite it stands in
    module_statement: Option<(TextSize, Suite)>,
}

impl Decider<'_> {
    fn src(&self, expr: &Expr) -> &str {
        &self.source[expr.range()]
    }

    /// `expr` with the type parameters `visible` in scope renamed, which is what tells apart
    /// two bounds that name different variables with the same spelling
    fn spelled(&self, expr: &Expr, visible: &HashMap<String, String>) -> String {
        apply_renames_to_rendered(self.src(expr), visible)
    }

    /// what `param` declares: its kind, bound or constraints, default and variance. two
    /// parameters that declare the same thing in the same suite share one variable
    fn signature(&self, param: &TypeParam, visible: &HashMap<String, String>) -> String {
        let default = param
            .default()
            .map(|default| format!(", default={}", self.spelled(default, visible)))
            .unwrap_or_default();
        match param {
            TypeParam::TypeVar(tv) => {
                if tv.bound.as_deref().is_some_and(is_parameters_bound) {
                    return format!("ParamSpec({default})");
                }
                let bound = match tv.bound.as_deref() {
                    Some(constraints) if tv.is_type_mapping => {
                        format!(", constraints={}", self.spelled(constraints, visible))
                    }
                    Some(bound) => format!(", bound={}", self.spelled(bound, visible)),
                    None => String::new(),
                };
                let variance = match tv.variance {
                    Some(ruff_python_ast::Variance::Covariant) => ", covariant=True",
                    Some(ruff_python_ast::Variance::Contravariant) => ", contravariant=True",
                    Some(ruff_python_ast::Variance::Invariant) | None => "",
                };
                format!("TypeVar({bound}{default}{variance})")
            }
            TypeParam::TypeVarTuple(_) => format!("TypeVarTuple({default})"),
            TypeParam::ParamSpec(_) => format!("ParamSpec({default})"),
        }
    }

    /// the name a parameter declared as `source_name` with `signature` is bound to
    ///
    /// the first definition is named [`polyfilled_name`] (`T` → `_T`). a later one declaring
    /// the same thing in the same suite reuses that name, so python sees one object; any other
    /// takes a numeric suffix (`_T_1`, `_T_2`, …) the module does not spell, so it shadows
    /// neither an earlier definition nor a binding of the module's own
    fn name(&mut self, source_name: &str, signature: String, suite: Suite) -> String {
        let base = polyfilled_name(self.written, source_name);
        let signature = (signature, suite);
        let name = std::iter::once(base.clone())
            .chain((1u32..u32::MAX).map(|number| format!("{base}_{number}")))
            .find(|candidate| match self.signatures.get(candidate) {
                Some(existing) => *existing == signature,
                None => !self.written.taken(candidate),
            })
            .unwrap_or(base);
        self.signatures.entry(name.clone()).or_insert(signature);
        name
    }

    /// where the declarations of `type_params`, the list of the statement starting at `start`,
    /// are written ahead of, and the suite that statement stands in
    ///
    /// at module scope, unless a bound, constraint or default reads a name a scope around the
    /// generic binds — a class-level name, say, or a type parameter declared in such a scope —
    /// which a module-scope declaration would read before it exists
    fn anchor(
        &self,
        type_params: &ruff_python_ast::TypeParams,
        start: TextSize,
    ) -> (TextSize, Suite) {
        let Some(module_statement) = self.module_statement else {
            return (start, self.suite);
        };
        let reads_a_local = type_params.iter().any(|param| {
            let read = [
                match param {
                    TypeParam::TypeVar(tv) => tv.bound.as_deref(),
                    _ => None,
                },
                param.default(),
            ];
            read.into_iter().flatten().flat_map(names_read).any(|name| {
                self.local_type_params.iter().any(|local| *local == name)
                    || self.scopes.iter().any(|scope| scope.names.contains(name))
            })
        });
        if reads_a_local {
            (start, self.suite)
        } else {
            module_statement
        }
    }

    /// where the declarations of `type_params`, the list of a generic standing directly in a
    /// class body, are written when a bound, constraint or default reads a name of that class
    /// — and the path each such name is read through from there
    ///
    /// the class body is where those names are evaluated, and nothing but the class body
    /// sees a declaration written there: not `typing.get_type_hints`, not the method's own
    /// body, and not a function nested in it, whose annotations are evaluated when the method
    /// runs. so the declarations go ahead of the outermost of the classes the generic stands
    /// in, and read the class's names through the classes. a name any other scope binds, or
    /// a type parameter declared in one, keeps them where they are
    fn through_classes(
        &self,
        type_params: &ruff_python_ast::TypeParams,
    ) -> Option<(TextSize, Suite, HashMap<String, String>)> {
        // the classes the generic stands in, the innermost last, with no function between
        let first_class = self
            .scopes
            .iter()
            .rposition(|scope| scope.class.is_none())
            .map_or(0, |function| function + 1);
        let classes = &self.scopes[first_class..];
        let innermost = classes.last()?;
        let path: Vec<&str> = classes
            .iter()
            .filter_map(|scope| scope.class.as_deref())
            .collect();
        let mut qualified = HashMap::new();
        for param in type_params {
            let read = match param {
                TypeParam::TypeVar(tv) if !tv.bound.as_deref().is_some_and(is_parameters_bound) => {
                    [tv.bound.as_deref(), param.default()]
                }
                // a `ParamSpec` or `TypeVarTuple` default is written without a way to read
                // a class's name through the class
                _ => [None, param.default()],
            };
            let cannot_qualify = !matches!(param, TypeParam::TypeVar(tv)
                if !tv.bound.as_deref().is_some_and(is_parameters_bound));
            for name in read.into_iter().flatten().flat_map(names_read) {
                if self.local_type_params.iter().any(|local| *local == name) {
                    return None;
                }
                if innermost.names.contains(name) {
                    if cannot_qualify {
                        return None;
                    }
                    qualified.insert(name.to_owned(), format!("{}.{name}", path.join(".")));
                } else if self.scopes.iter().any(|scope| scope.names.contains(name)) {
                    return None;
                }
            }
        }
        let outermost = classes.first()?;
        (!qualified.is_empty()).then_some((outermost.start, outermost.suite, qualified))
    }

    /// record how a type hoisted out of `name`, a generic kept as native syntax, reads each of
    /// its parameters `type_params`: through the classes around it, when there are only classes,
    /// each bound under its own name
    fn record_native(
        &mut self,
        type_params: &ruff_python_ast::TypeParams,
        name: &ruff_python_ast::Identifier,
        decorators: &[ruff_python_ast::Decorator],
    ) {
        if !keeps_its_name(self.source, decorators)
            || !self
                .scopes
                .iter()
                .all(|scope| scope.class.is_some() && scope.keeps_its_name)
        {
            return;
        }
        let path: Vec<&str> = self
            .scopes
            .iter()
            .filter_map(|scope| scope.class.as_deref())
            .chain(std::iter::once(name.as_str()))
            .collect();
        let path = path.join(".");
        let reads = type_params
            .iter()
            .enumerate()
            .map(|(index, param)| {
                (
                    param.name().id.to_string(),
                    format!("{path}.__type_params__[{index}]"),
                )
            })
            .collect();
        self.native.insert(type_params.range(), reads);
    }

    /// decide `type_params`, the list of the statement starting at `start`, answering what each
    /// parameter is renamed to
    fn decide(
        &mut self,
        type_params: &ruff_python_ast::TypeParams,
        start: TextSize,
        kept_native: bool,
    ) -> HashMap<String, String> {
        if kept_native {
            return HashMap::new();
        }
        let (anchor, suite, qualified) = match self.through_classes(type_params) {
            Some((anchor, suite, qualified)) => (anchor, suite, qualified),
            None => {
                let (anchor, suite) = self.anchor(type_params, start);
                (anchor, suite, HashMap::new())
            }
        };
        // declared inside a scope the module statement opens, where only that scope reads it
        let local = self
            .module_statement
            .is_some_and(|(module_statement, _)| module_statement != anchor);
        let mut visible: HashMap<String, String> = self
            .enclosing
            .iter()
            .flatten()
            .map(|(k, v)| (k.clone(), v.clone()))
            .chain(qualified.iter().map(|(k, v)| (k.clone(), v.clone())))
            .collect();
        let mut renames = HashMap::new();
        let mut names = Vec::new();
        for param in type_params {
            if param.default().is_some() {
                let kind = match param {
                    TypeParam::TypeVar(tv)
                        if tv.bound.as_deref().is_some_and(is_parameters_bound) =>
                    {
                        &mut self.defaults.param_spec
                    }
                    TypeParam::TypeVar(_) => &mut self.defaults.type_var,
                    TypeParam::ParamSpec(_) => &mut self.defaults.param_spec,
                    TypeParam::TypeVarTuple(_) => &mut self.defaults.type_var_tuple,
                };
                *kind = true;
            }
            let signature = self.signature(param, &visible);
            let source_name = param.name().id.as_str();
            let name = self.name(source_name, signature, suite);
            visible.insert(source_name.to_owned(), name.clone());
            renames.insert(source_name.to_owned(), name.clone());
            names.push(name);
        }
        if local {
            self.local_type_params
                .extend(type_params.iter().map(|param| param.name().id.to_string()));
        }
        self.lists.insert(
            type_params.range(),
            Declared {
                names,
                anchor,
                qualified,
            },
        );
        renames
    }
}

impl<'ast> Visitor<'ast> for Decider<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        let entered = matches!(stmt, Stmt::ClassDef(_) | Stmt::FunctionDef(_))
            && self.module_statement.is_none();
        if entered {
            self.module_statement = Some((stmt.start(), self.suite));
        }
        let local_type_params = self.local_type_params.len();
        let renames = match stmt {
            Stmt::ClassDef(class) => class.type_params.as_deref().map(|tp| {
                let native = is_native(self.config, &tp.type_params);
                if native {
                    self.record_native(tp, &class.name, &class.decorator_list);
                }
                self.decide(tp, class.start(), native)
            }),
            // a `type def` is erased by its own pass, so none of its parameters is declared
            Stmt::FunctionDef(func) if ruff_python_ast::helpers::is_type_def(func) => None,
            Stmt::FunctionDef(func) => func.type_params.as_deref().map(|tp| {
                let native = !tp.type_params.iter().any(is_some_hole)
                    && is_native(self.config, &tp.type_params);
                if native {
                    self.record_native(tp, &func.name, &func.decorator_list);
                }
                let mut renames = self.decide(tp, func.start(), native);
                // the body reads a parameter under its own name, which a type parameter of
                // that name does not reach: the parameter is bound in the body's scope, the
                // type parameter in the one around it
                for parameter in &func.parameters {
                    renames.remove(parameter.name().as_str());
                }
                renames
            }),
            // an alias writes its declarations into its own replacement
            Stmt::TypeAlias(alias) => {
                if let Some(tp) = alias.type_params.as_deref() {
                    let native = is_native(self.config, &tp.type_params);
                    let module_statement = self.module_statement.take();
                    self.decide(tp, alias.start(), native);
                    self.module_statement = module_statement;
                    self.local_type_params.truncate(local_type_params);
                }
                return;
            }
            _ => None,
        };
        // the scope the statement opens, which its body's declarations cannot be read in from
        // module scope
        let scope = match stmt {
            Stmt::ClassDef(class) => Some(Scope {
                names: class.body.iter().flat_map(bound_names).collect(),
                class: Some(class.name.id.to_string()),
                keeps_its_name: keeps_its_name(self.source, &class.decorator_list),
                start: class.start(),
                suite: self.suite,
            }),
            Stmt::FunctionDef(func) => Some(Scope {
                names: func
                    .parameters
                    .iter()
                    .map(|parameter| parameter.name().to_string())
                    .chain(func.body.iter().flat_map(bound_names))
                    .collect(),
                class: None,
                keeps_its_name: false,
                start: func.start(),
                suite: self.suite,
            }),
            _ => None,
        };
        let opens_scope = scope.is_some();
        self.scopes.extend(scope);
        self.enclosing.push(renames.unwrap_or_default());
        walk_stmt(self, stmt);
        self.enclosing.pop();
        if opens_scope {
            self.scopes.pop();
        }
        self.local_type_params.truncate(local_type_params);
        if entered {
            self.module_statement = None;
        }
    }

    fn visit_body(&mut self, body: &'ast [Stmt]) {
        let outer = self.suite;
        if let Some(first) = body.first() {
            self.suite = Suite::Nested(first.start());
        }
        walk_body(self, body);
        self.suite = outer;
    }
}

/// whether a definition with `decorators` is bound under the name it is written with, and to
/// the definition itself: no decorator of the author's, which may return anything, and no
/// visibility keyword, which renames it
fn keeps_its_name(source: &str, decorators: &[ruff_python_ast::Decorator]) -> bool {
    decorators.iter().all(|decorator| {
        super::source_util::is_synthetic_decorator(source, decorator)
            && !matches!(
                &decorator.expression,
                Expr::Name(name)
                    if matches!(name.id.as_str(), "private" | "protected" | "decorator_keyword")
            )
    })
}

/// a scope around a statement the polyfill visits
struct Scope {
    /// the names the scope binds
    names: HashSet<String>,
    /// the class's name, when the scope is a class body
    class: Option<String>,
    /// whether the class is bound under its own name, and to itself
    keeps_its_name: bool,
    /// where the statement opening the scope starts, and the suite it stands in
    start: TextSize,
    suite: Suite,
}

/// the names `stmt` binds in the scope it stands in
fn bound_names(stmt: &Stmt) -> impl Iterator<Item = String> {
    crate::runtime::bindings(stmt).into_keys()
}

/// the names `expr` reads
fn names_read(expr: &Expr) -> Vec<&str> {
    struct Reads<'a>(Vec<&'a str>);
    impl<'a> Visitor<'a> for Reads<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr {
                self.0.push(name.id.as_str());
            }
            walk_expr(self, expr);
        }
    }
    let mut reads = Reads(Vec::new());
    reads.visit_expr(expr);
    reads.0
}

pub(crate) struct GenericPolyfillPass<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    config: Config,
}

impl<'src> GenericPolyfillPass<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>, config: Config) -> Self {
        Self {
            source,
            written,
            config,
        }
    }
}

impl super::ast_driver::TypeAwarePass for GenericPolyfillPass<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::GenericPolyfill)
    }

    /// a header or alias it polyfills is re-printed with its type parameters renamed, and it
    /// re-renders what the lowerings inside wrote rather than their source — the symbolic
    /// folds, the private alias names, the tuple, named tuple and match types, and the variance
    /// keywords as `TypeVar` arguments
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        &[
            super::ast_driver::Lowering::TupleLiteralType,
            super::ast_driver::Lowering::AnonNamedTuple,
            super::ast_driver::Lowering::MatchType,
            super::ast_driver::Lowering::SymbolicTypeOp,
            super::ast_driver::Lowering::Modifiers,
            super::ast_driver::Lowering::VisibilityRename,
            super::ast_driver::Lowering::VarianceStrip,
        ]
    }

    fn run(
        &self,
        stmts: &[ruff_python_ast::Stmt],
        types: &dyn TypeInfo,
        ctx: &mut super::ast_driver::PassContext,
    ) {
        let mut inner = GenericPolyfill::new(
            self.source,
            self.written,
            types,
            self.config.clone(),
            ctx.symbolic_substitutions.clone(),
            ctx.text_edits.clone(),
            PolyfilledTypeParams::decide(self.source, self.written, stmts, &self.config),
        );
        inner.collect_private_aliases(stmts);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        // an edit we re-rendered with the typevar rename applied has to lose its
        // original, or the un-renamed text can still win the overlap race
        let superseded = std::mem::take(&mut inner.superseded);
        let withdrawn: Vec<usize> = ctx
            .text_edits
            .iter()
            .enumerate()
            .filter(|(_, (range, _))| superseded.contains(range))
            .map(|(index, _)| index)
            .collect();
        ctx.withdrawn_text_edits.extend(withdrawn);
        let emits_any = inner.needed_imports_any;
        for line in
            std::mem::take(&mut inner.needed_imports).into_lines(&inner.constructors, self.written)
        {
            ctx.required_imports.push(line);
        }
        if emits_any {
            ctx.required_imports
                .push(self.written.import_from("typing", &["Any"]));
        }
        for (at, prefix) in std::mem::take(&mut inner.statement_prefixes) {
            ctx.statement_inserts
                .push((at, vec![super::ast_driver::Fragment::Lit(prefix)]));
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

/// each name `value`, the value of a type alias, reads that is a restricted member of the
/// class the alias is declared in, with the spelling a read of it takes
fn class_member_references(value: &Expr, types: &dyn TypeInfo) -> HashMap<String, String> {
    struct Collect<'a> {
        types: &'a dyn TypeInfo,
        found: HashMap<String, String>,
    }
    impl<'ast> Visitor<'ast> for Collect<'_> {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let Expr::Name(name) = expr
                && let Some(renamed) = self.types.class_body_member_name(name)
            {
                self.found.insert(name.id.to_string(), renamed);
            }
            walk_expr(self, expr);
        }
    }
    let mut collect = Collect {
        types,
        found: HashMap::new(),
    };
    collect.visit_expr(value);
    collect.found
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
    fn bound_naming_an_earlier_type_parameter() {
        // the name a bound refers to is mangled by the list that declares it, so the text
        // spliced into `bound=` has to be mangled too — an unrenamed `T` here is a `NameError`
        // when the emitted module is imported, because the `TypeVar` call evaluates its bound
        check(
            indoc! {"
                def f[T, R: T](t: T, r: R) -> None: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                _R = TypeVar(\"_R\", bound=_T)
                def f(t: _T, r: _R) -> None: ...
            "},
        );
    }

    #[test]
    fn default_naming_an_earlier_type_parameter() {
        check(
            indoc! {"
                def f[T, R = T](t: T, r: R) -> None: ...
            "},
            indoc! {"
                from typing_extensions import TypeVar
                _T = TypeVar(\"_T\")
                _R = TypeVar(\"_R\", default=_T)
                def f(t: _T, r: _R) -> None: ...
            "},
        );
    }

    #[test]
    fn bound_naming_an_enclosing_type_parameter() {
        // `T` belongs to the class's list, so the method's own list does not mangle it; the
        // enclosing list's map has to reach the method's `TypeVar` call
        check(
            indoc! {"
                class Owner[T]:
                    def narrow[U: T](self, u: U) -> None: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\")
                _U = TypeVar(\"_U\", bound=_T)
                class Owner(Generic[_T]):
                    def narrow(self, u: _U) -> None: ...
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
    fn rendered_rename_skips_string_and_attribute() {
        let mut renames = std::collections::HashMap::new();
        renames.insert("T".to_owned(), "_T".to_owned());
        // a quoted annotation is a string *literal type* in basedpython, not a
        // forward reference, so its contents are data and must not be renamed
        assert_eq!(
            super::apply_renames_to_rendered("Callable[[T], Literal[\"T\", 'T']]", &renames),
            "Callable[[_T], Literal[\"T\", 'T']]"
        );
        assert_eq!(
            super::apply_renames_to_rendered("Callable[[T], x.T]", &renames),
            "Callable[[_T], x.T]"
        );
        // an escaped quote does not close the string
        assert_eq!(
            super::apply_renames_to_rendered("Literal[\"a\\\"T\"] | T", &renames),
            "Literal[\"a\\\"T\"] | _T"
        );
        // a name that merely contains a parameter's name is untouched
        assert_eq!(
            super::apply_renames_to_rendered("List[TT] | T", &renames),
            "List[TT] | _T"
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
        // defaults on any parameter kind make the header 3.13-only syntax, so the whole
        // declaration polyfills. `typing`'s own `ParamSpec` and `TypeVarTuple` take no
        // `default=` below 3.13, so both come from `typing_extensions`. a keyword argument
        // cannot be starred, so a variadic's default is passed unpacked with `Unpack`
        check_at(
            indoc! {"
                class A[**P = [int]]: ...
                class B[*Ts = *tuple[int, str]]: ...
            "},
            indoc! {"
                from typing import Unpack, Generic
                from typing_extensions import TypeVarTuple, ParamSpec
                _P = ParamSpec(\"_P\", default=[int])
                class A(Generic[_P]): ...
                _Ts = TypeVarTuple(\"_Ts\", default=Unpack[tuple[int, str]])
                class B(Generic[*_Ts]): ...
            "},
            PythonVersion::PY312,
        );
    }

    /// a parameter specification spelled with the top-parameters bound takes its default as a
    /// parameter list, which python spells as a list
    #[test]
    fn a_parameters_bound_default_is_a_list() {
        check_at(
            "class A[P: (*: *, **: *) = (int, str)]: ...\n",
            indoc! {"
                from typing import Generic
                from typing_extensions import ParamSpec
                _P = ParamSpec(\"_P\", default=[int, str])
                class A(Generic[_P]): ...
            "},
            PythonVersion::PY312,
        );
        check_at(
            "class A[P: (*: *, **: *) = (int, str)]: ...\n",
            "class A[**P = [int, str]]: ...\n",
            PythonVersion::PY313,
        );
    }

    /// a default needs the `TypeVar` from `typing_extensions` below 3.13, and the one a module
    /// imports from `typing` itself takes none. so the polyfill calls its own by a name the
    /// module does not spell, and the module's `TypeVar` keeps meaning what it imported
    #[test]
    fn a_default_calls_a_type_var_the_module_does_not_bind() {
        check_at(
            indoc! {"
                from typing import TypeVar
                U = TypeVar(\"U\")
                class A[T = int]: ...
            "},
            indoc! {"
                from typing_extensions import TypeVar as TypeVar2
                from typing import TypeVar, Generic
                U = TypeVar(\"U\")
                _T = TypeVar2(\"_T\", default=int)
                class A(Generic[_T]): ...
            "},
            PythonVersion::PY312,
        );
    }

    /// a constructor the polyfill writes is called by a name the module does not spell when the
    /// module binds that name to something other than the constructor
    #[test]
    fn a_constructor_the_module_binds_is_imported_under_another_name() {
        check_at(
            indoc! {"
                class Generic: ...
                class A[T, *Ts, **P]: ...
            "},
            indoc! {"
                from typing import TypeVar, TypeVarTuple, Unpack, ParamSpec, Generic as Generic2
                class Generic: ...
                _T = TypeVar(\"_T\")
                _Ts = TypeVarTuple(\"_Ts\")
                _P = ParamSpec(\"_P\")
                class A(Generic2[_T, *_Ts, _P]): ...
            "},
            PythonVersion::PY311,
        );
    }

    /// the `TypeVar` a type parameter is declared as is written into the module, where a
    /// binding of the module's own under the same name would be overwritten by it
    #[test]
    fn a_type_variable_the_module_binds_keeps_its_name() {
        check_at(
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\", bound=int)
                def legacy(x: _T) -> _T: ...
                def modern[T](x: T) -> T: ...
                class A[_T]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", bound=int)
                def legacy(x: _T) -> _T: ...
                _T2 = TypeVar(\"_T2\")
                def modern(x: _T2) -> _T2: ...
                class A(Generic[_T2]): ...
            "},
            PythonVersion::PY311,
        );
    }

    /// a definition written into an `if` does not run when the `if` does not, so a function the
    /// module declares later gets a definition of its own, under a name that shadows neither.
    /// a method's is written ahead of its class, at module scope, where everything that reads
    /// it looks
    #[test]
    fn a_type_variable_is_bound_in_one_suite() {
        check_at(
            indoc! {"
                class C:
                    def m[T](self, x: T) -> T: ...
                if flag:
                    def g[T](x: T) -> T: ...
                def f[T](x: T) -> T: ...
                def h[T](x: T) -> T: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                class C:
                    def m(self, x: _T) -> _T: ...
                if flag:
                    _T_1 = TypeVar(\"_T_1\")
                    def g(x: _T_1) -> _T_1: ...
                def f(x: _T) -> _T: ...
                def h(x: _T) -> _T: ...
            "},
            PythonVersion::PY311,
        );
    }

    /// a function nested in another has its type variable declared at module scope too, ahead
    /// of the statement at module scope around it
    #[test]
    fn a_nested_function_declares_its_type_variable_at_module_scope() {
        check_at(
            indoc! {"
                @decorate
                def outer() -> None:
                    def inner[T](x: T) -> T: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                @decorate
                def outer() -> None:
                    def inner(x: _T) -> _T: ...
            "},
            PythonVersion::PY311,
        );
    }

    /// a bound that reads a class-level name is declared at module scope with the rest, and
    /// reads the name through the class. the class does not exist yet where the declaration
    /// is written, so the bound is a string, which `TypeVar` keeps as a forward reference. a
    /// declaration in the class body would be out of reach of `typing.get_type_hints`, and of
    /// a function nested in the method, whose annotations are evaluated when the method runs
    #[test]
    fn a_bound_reading_a_class_level_name_reads_it_through_the_class() {
        check_at(
            indoc! {"
                class C:
                    class Inner: ...
                    def m[T: Inner, U: T](self, x: T, y: U) -> T: ...
                    def n[V](self, v: V) -> V: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\", bound=\"C.Inner\")
                _U = TypeVar(\"_U\", bound=_T)
                _V = TypeVar(\"_V\")
                class C:
                    class Inner: ...
                    def m(self, x: _T, y: _U) -> _T: ...
                    def n(self, v: _V) -> _V: ...
            "},
            PythonVersion::PY311,
        );
    }

    /// in a class nested in another, the name is read through both
    #[test]
    fn a_bound_reading_a_nested_class_level_name_reads_it_through_every_class() {
        check_at(
            indoc! {"
                class A:
                    class B:
                        class Inner: ...
                        def m[T: Inner](self, x: T) -> T: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\", bound=\"A.B.Inner\")
                class A:
                    class B:
                        class Inner: ...
                        def m(self, x: _T) -> _T: ...
            "},
            PythonVersion::PY311,
        );
    }

    /// a class inside a function has its declarations written ahead of it in the function,
    /// where the method's body sees them as it sees the function's other names
    #[test]
    fn a_bound_reading_a_class_level_name_in_a_function_is_declared_in_the_function() {
        check_at(
            indoc! {"
                def f() -> object:
                    class C:
                        class Inner: ...
                        def m[T: Inner](self, x: T) -> T: ...
                    return C
            "},
            indoc! {"
                from typing import TypeVar
                def f() -> object:
                    _T = TypeVar(\"_T\", bound=\"C.Inner\")
                    class C:
                        class Inner: ...
                        def m(self, x: _T) -> _T: ...
                    return C
            "},
            PythonVersion::PY311,
        );
    }

    /// a bound that reads a name of the function around the generic is declared where that
    /// name is bound, as python would evaluate it
    #[test]
    fn a_bound_reading_a_function_local_name_is_declared_in_the_function() {
        check_at(
            indoc! {"
                def f() -> object:
                    class Local: ...
                    def g[T: Local](x: T) -> T:
                        return x
                    return g
            "},
            indoc! {"
                from typing import TypeVar
                def f() -> object:
                    class Local: ...
                    _T = TypeVar(\"_T\", bound=Local)
                    def g(x: _T) -> _T:
                        return x
                    return g
            "},
            PythonVersion::PY311,
        );
    }

    /// a numbered name is skipped when the module spells it, as the first name is
    #[test]
    fn a_numbered_type_variable_skips_a_name_the_module_binds() {
        check_at(
            indoc! {"
                _T_1 = 1
                def f[T: int](x: T) -> T: ...
                def g[T](x: T) -> T: ...
            "},
            indoc! {"
                from typing import TypeVar
                _T_1 = 1
                _T = TypeVar(\"_T\", bound=int)
                def f(x: _T) -> _T: ...
                _T_2 = TypeVar(\"_T_2\")
                def g(x: _T_2) -> _T_2: ...
            "},
            PythonVersion::PY311,
        );
    }

    /// a runtime helper is bound at module scope beside the definition, and the module never
    /// spells it, so a name the module leaves free can still be one
    #[test]
    fn a_type_variable_skips_a_runtime_helper_name() {
        check_at(
            "def f[force_unwrap](x: force_unwrap) -> force_unwrap: ...\n",
            indoc! {"
                from typing import TypeVar
                _force_unwrap2 = TypeVar(\"_force_unwrap2\")
                def f(x: _force_unwrap2) -> _force_unwrap2: ...
            "},
            PythonVersion::PY311,
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
    fn type_mapping_polyfill() {
        check(
            indoc! {"
                class Foo[T in (int, str)]: ...
            "},
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", int, str)
                class Foo(Generic[_T]): ...
            "},
        );
    }

    #[test]
    fn type_mapping_function_polyfill() {
        check(
            indoc! {"
                def f[T in (int, str)](x: T) -> T:
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
    fn type_mapping_lowered_on_312() {
        check_at(
            "class Foo[T in (int, str)]: ...\n",
            "class Foo[T: (int, str)]: ...\n",
            PythonVersion::PY312,
        );
    }

    #[test]
    fn type_mapping_function_lowered_on_312() {
        check_at(
            indoc! {"
                def f[T in (int, str)](x: T) -> T:
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
        // positional constraints. Use the type mapping `T in (int, str)` for that. the
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
    fn type_mapping_on_type_alias_lowered_on_312() {
        check_at(
            "type X[T in (int, str)] = list[T]\n",
            "type X[T: (int, str)] = list[T]\n",
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
    fn mixed_tuple_bound_and_type_mapping_on_314() {
        // TTuple: (int, str) → tuple[int, str]; TConst in (int, str) → (int, str)
        check_at(
            indoc! {"
                class A[
                    TTuple: (int, str),
                    TConst in (int, str),
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
                from typing import TypeVar, ParamSpec, Generic

                _P = ParamSpec(\"_P\")
                class A(Generic[_P]): ...
            "},
        );
    }

    #[test]
    fn by_type_mapping_is_constraints() {
        // In .by files, `T in (int, str)` is constraints.
        check(
            "class Foo[T in (int, str)]: ...\n",
            indoc! {"
                from typing import TypeVar, Generic
                _T = TypeVar(\"_T\", int, str)
                class Foo(Generic[_T]): ...
            "},
        );
    }

    /// `some int` declares a type parameter named after its parameter. the parser gives the
    /// list it synthesizes the parameter list's range, which has no brackets to remove
    #[test]
    fn some_parameter() {
        check(
            indoc! {"
                def inc(n: some int) -> int:
                    return n + 1
            "},
            indoc! {"
                from typing import TypeVar
                _n = TypeVar(\"_n\", bound=int)
                def inc(n: _n) -> int:
                    return n + 1
            "},
        );
    }

    /// native syntax has nowhere to declare a `some` hole, so it is a `TypeVar` at every
    /// version, and the signature's other reads of it are renamed with it
    #[test]
    fn some_parameter_read_by_the_return_type() {
        check_at(
            indoc! {"
                def echo[T](x: T, n: some int) -> n:
                    return n
            "},
            indoc! {"
                from typing import TypeVar
                _T = TypeVar(\"_T\")
                _n = TypeVar(\"_n\", bound=int)
                def echo(x: _T, n: _n) -> _n:
                    return n
            "},
            PythonVersion::PY313,
        );
    }

    /// the body reads a `some` parameter under its own name, which the hole shares, in a
    /// nested definition too
    #[test]
    fn a_some_parameter_is_not_renamed_in_the_body() {
        check(
            indoc! {"
                def show(n: some int) -> n:
                    def inner() -> None:
                        print(n)
                    inner()
                    return n
            "},
            indoc! {"
                from typing import TypeVar
                _n = TypeVar(\"_n\", bound=int)
                def show(n: _n) -> _n:
                    def inner() -> None:
                        print(n)
                    inner()
                    return n
            "},
        );
    }

    /// a bound is evaluated when the `TypeVar` is made, so below 3.10 an optional in it is
    /// the `Union` the optional lowering spells for that version, written whole
    #[test]
    fn an_optional_bound_below_310() {
        check_at(
            indoc! {"
                def f[T: int?](x: T) -> T:
                    return x
                def g(n: some int?) -> int:
                    return 0
                type B = int?
            "},
            indoc! {"
                from __future__ import annotations
                from typing import TypeVar, Union
                from typing_extensions import TypeAliasType
                _T = TypeVar(\"_T\", bound=Union[int, None])
                def f(x: _T) -> _T:
                    return x
                _n = TypeVar(\"_n\", bound=Union[int, None])
                def g(n: _n) -> int:
                    return 0
                B = TypeAliasType(\"B\", Union[int, None])
            "},
            PythonVersion::PY39,
        );
    }

    /// an optional of an optional keeps its outer layer as the runtime `Optional` wrapper,
    /// which the bound writes once
    #[test]
    fn a_nested_optional_bound() {
        let output = crate::transpile(
            "def f[T: int??](x: T) -> T:\n    return x\n",
            &crate::Config {
                min_version: PythonVersion::PY311,
                ..crate::Config::test_default()
            },
        )
        .expect("transpile failed");
        assert!(
            output.contains("TypeVar(\"_T\", bound=Optional[int | None])"),
            "unexpected output:\n{output}"
        );
    }
}
