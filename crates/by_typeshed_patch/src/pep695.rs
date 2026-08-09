//! legacy `TypeVar` + `Generic`/`Protocol` -> pep 695 conversion with explicit
//! variance keywords and nice type-parameter names
//!
//! this is the heart of the basedpython typeshed: upstream stubs declare
//! generic classes the legacy way —
//!
//! ```python
//! _KT_co = TypeVar("_KT_co", covariant=True)
//! _VT_co = TypeVar("_VT_co", covariant=True)
//! class Mapping(Collection[_KT_co], Generic[_KT_co, _VT_co]):
//!     def __getitem__(self, key: _KT_co) -> _VT_co: ...
//! ```
//!
//! and basedpython wants the pep 695 form with readable names and explicit
//! variance —
//!
//! ```by
//! class Mapping[out Key, out Value](Collection[Key]):
//!     def __getitem__(self, key: Key) -> Value: ...
//! ```
//!
//! the conversion is module-local: it reads every module-level `TypeVar`/
//! `TypeVarTuple`/`ParamSpec` declaration (recording variance, bound,
//! constraints and default), then rewrites each generic class header into a
//! pep 695 type-parameter list, renaming references inside the class body to
//! match. a typevar declaration is removed once nothing outside a converted
//! class still references it
//!
//! ## naming
//!
//! a curated table gives the core container/protocol typevars names
//! (`_KT_co` -> `Key`, `_T_co` -> `Element`, ...). everything else is named
//! mechanically by stripping the leading underscore and the `_co`/`_contra`
//! variance suffix. within one class, colliding names get a numeric suffix
//!
//! a candidate name is also rejected when the module already binds it — an
//! import, a class/function definition, an assignment. `xml.etree.ElementPath`
//! imports `Element`, so `_T` there must not become `Element`: the type
//! parameter would shadow the class and `Element[dynamic]` would subscript the
//! type parameter instead
//!
//! ## variance
//!
//! covariant -> `out`, contravariant -> `in`, invariant -> `in out`.
//! basedpython has no bivariant spelling; `in out` is explicit invariance
//!
//! ## conservatism
//!
//! a class is only rewritten when every one of its type parameters resolves to
//! a known module-level typevar. anything we can't fully characterise (an
//! imported typevar, an unusual base) is left in legacy form rather than
//! risking a broken stub. generic functions and type aliases are likewise left
//! alone — they keep referencing the legacy declarations, which are retained

use std::collections::{HashMap, HashSet};

use ruff_python_ast::helpers::TOP_PARAMETERS_FORM;
use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr, walk_stmt};
use ruff_python_ast::{
    Arguments, Expr, ExprSubscript, Keyword, ModModule, Stmt, StmtAssign, StmtClassDef,
    StmtFunctionDef,
};
use ruff_python_parser::Parsed;
use ruff_text_size::{Ranged, TextRange};

use crate::Edit;

/// declared variance of a type parameter
#[derive(Clone, Copy, PartialEq, Eq)]
enum Variance {
    Covariant,
    Contravariant,
    Invariant,
}

impl Variance {
    /// keyword prefix in a pep 695 type-parameter list, e.g. `out `
    fn keyword(self) -> &'static str {
        match self {
            Variance::Covariant => "out ",
            Variance::Contravariant => "in ",
            Variance::Invariant => "in out ",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TvKind {
    TypeVar,
    TypeVarTuple,
    ParamSpec,
}

/// a module-level `TypeVar`/`TypeVarTuple`/`ParamSpec` declaration
struct Decl<'a> {
    kind: TvKind,
    variance: Variance,
    bound: Option<&'a Expr>,
    constraints: Vec<&'a Expr>,
    default: Option<&'a Expr>,
    /// range of the whole `NAME = TypeVar(...)` statement, for removal
    stmt_range: TextRange,
}

type Table<'a> = HashMap<&'a str, Decl<'a>>;

/// convert every legacy generic class in `parsed` to pep 695 form. returns
/// disjoint text edits over `source` (see [`crate::apply_edits`])
pub fn convert_module(parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
    let module = parsed.syntax();
    let table = collect_decls(&module.body);
    if table.is_empty() {
        return Vec::new();
    }

    // walk every scope, converting generic classes and generic functions and
    // tracking the typevars bound by enclosing classes/functions so a method's
    // own type parameters exclude the ones its class already declares
    let mut ctx = Ctx {
        table: &table,
        source,
        module_names: collect_module_names(&module.body, &table),
        edits: Vec::new(),
        renamed: Vec::new(),
        covered: Vec::new(),
    };
    convert_scope(&module.body, &mut Vec::new(), &mut ctx);
    let Ctx {
        mut edits,
        renamed,
        covered,
        ..
    } = ctx;

    // remove declarations whose every remaining reference was consumed by a
    // conversion. a reference inside another surviving declaration's
    // bound/default keeps the typevar alive, which is what we want.
    //
    // only private (`_`-prefixed) typevars are removed: typeshed reserves the
    // leading underscore for module-private symbols, whereas a public typevar
    // (e.g. `AnyStr`) may be re-exported and imported by other modules, so it
    // must survive even when unused within its own module
    let uses = collect_uses(&module.body, &table);
    let mut decls: Vec<_> = table.iter().collect();
    decls.sort_by_key(|(name, _)| **name);
    for (name, decl) in decls {
        if !name.starts_with('_') {
            continue;
        }
        let occurrences = uses.get(*name).map_or(&[][..], Vec::as_slice);
        let all_consumed = occurrences.iter().all(|span| {
            renamed.iter().any(|(n, r)| n == name && r == span)
                || covered.iter().any(|c| c.contains_range(*span))
        });
        if all_consumed && !occurrences.is_empty() {
            edits.push(remove_stmt(decl.stmt_range, source));
        }
    }

    edits
}

/// accumulators threaded through the scope walk
struct Ctx<'a> {
    table: &'a Table<'a>,
    source: &'a str,
    /// every name the module itself binds, which a type-parameter name must not
    /// shadow
    module_names: HashSet<String>,
    edits: Vec<Edit>,
    /// (legacy name, original span) for every reference a conversion renamed —
    /// used to decide whether a declaration is now dead
    renamed: Vec<(String, TextRange)>,
    /// class-header argument spans rewritten wholesale; any legacy reference
    /// inside one of these is gone from the output
    covered: Vec<TextRange>,
}

/// the typevars an enclosing scope declares: their legacy names (so a nested
/// function doesn't re-capture them as its own parameters) and their rendered
/// pep 695 names (so a nested scope doesn't pick a colliding name)
struct Scope<'a> {
    legacy: HashSet<&'a str>,
    rendered: HashSet<String>,
}

/// recursively convert classes and functions, threading the stack of enclosing
/// [`Scope`]s
fn convert_scope<'a>(body: &'a [Stmt], bound: &mut Vec<Scope<'a>>, ctx: &mut Ctx<'a>) {
    for stmt in body {
        match stmt {
            Stmt::ClassDef(class) => {
                let scope = match convert_class(class, bound, ctx) {
                    Some(conv) => {
                        let scope = Scope {
                            legacy: conv.param_legacy.iter().copied().collect(),
                            rendered: conv.param_new.iter().cloned().collect(),
                        };
                        ctx.edits.extend(conv.edits);
                        ctx.renamed.extend(conv.renamed);
                        ctx.covered.push(conv.args_range);
                        scope
                    }
                    None => Scope {
                        legacy: class_bound_typevars(class, ctx.table),
                        rendered: HashSet::new(),
                    },
                };
                bound.push(scope);
                convert_scope(&class.body, bound, ctx);
                bound.pop();
            }
            Stmt::FunctionDef(func) => {
                let (legacy, rendered) = convert_function(func, bound, ctx);
                bound.push(Scope {
                    legacy: legacy.into_iter().collect(),
                    rendered: rendered.into_iter().collect(),
                });
                convert_scope(&func.body, bound, ctx);
                bound.pop();
            }
            Stmt::If(node) => {
                convert_scope(&node.body, bound, ctx);
                for clause in &node.elif_else_clauses {
                    convert_scope(&clause.body, bound, ctx);
                }
            }
            Stmt::Try(node) => {
                convert_scope(&node.body, bound, ctx);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    convert_scope(&handler.body, bound, ctx);
                }
                convert_scope(&node.orelse, bound, ctx);
                convert_scope(&node.finalbody, bound, ctx);
            }
            Stmt::With(node) => convert_scope(&node.body, bound, ctx),
            _ => {}
        }
    }
}

/// the typevars a class declares: every known typevar appearing in its base
/// list. these are excluded from any method's own type parameters
fn class_bound_typevars<'a>(class: &'a StmtClassDef, table: &Table<'a>) -> HashSet<&'a str> {
    let mut names = Vec::new();
    if let Some(args) = &class.arguments {
        for base in &args.args {
            collect_free_typevars(base, table, &mut names);
        }
    }
    names.into_iter().collect()
}

/// convert a generic function/method to pep 695 form, returning the legacy
/// names it claimed as type parameters (empty if it wasn't converted).
///
/// a function's type parameters are the known typevars in its signature that
/// aren't already bound by an enclosing scope. unlike classes, function type
/// parameters carry no variance
fn convert_function<'a>(
    func: &'a StmtFunctionDef,
    bound: &[Scope<'a>],
    ctx: &mut Ctx<'a>,
) -> (Vec<&'a str>, Vec<String>) {
    // already pep 695
    if func.type_params.is_some() {
        return (Vec::new(), Vec::new());
    }

    let signature = signature_exprs(func);
    let mut params: Vec<&str> = Vec::new();
    for expr in &signature {
        collect_free_typevars(expr, ctx.table, &mut params);
    }
    params.retain(|name| !bound.iter().any(|scope| scope.legacy.contains(name)));
    if params.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let mut used = claimed_names(&ctx.module_names, bound);
    let mut new_names = Vec::with_capacity(params.len());
    for legacy in &params {
        new_names.push(pick_name(legacy, ctx.table[*legacy].kind, &mut used));
    }
    let renames: HashMap<&str, &str> = params
        .iter()
        .zip(&new_names)
        .map(|(legacy, new)| (*legacy, new.as_str()))
        .collect();

    // type-parameter list inserted right after the function name (no variance)
    let param_list = render_params(&params, ctx.table, &new_names, &renames, ctx.source, false);
    let name_end = func.name.range().end().to_usize();
    ctx.edits.push(Edit {
        start: name_end,
        end: name_end,
        replacement: param_list,
    });

    // rename references in the signature and body, not descending into nested
    // classes or functions (which own their scopes)
    let mut renamer = RefRenamer {
        renames: &renames,
        skip_functions: true,
        hits: Vec::new(),
    };
    for expr in &signature {
        renamer.visit_expr(expr);
    }
    for stmt in &func.body {
        renamer.visit_stmt(stmt);
    }
    for (legacy, range) in renamer.hits {
        ctx.edits.push(Edit {
            start: range.start().to_usize(),
            end: range.end().to_usize(),
            replacement: renames[legacy].to_string(),
        });
        ctx.renamed.push((legacy.to_string(), range));
    }

    (params, new_names)
}

/// the names a new type parameter must not take: everything the module binds
/// plus the rendered pep 695 names already claimed by enclosing scopes, so a
/// nested scope never reuses an outer name
fn claimed_names(module_names: &HashSet<String>, bound: &[Scope]) -> HashSet<String> {
    let mut claimed = module_names.clone();
    claimed.extend(
        bound
            .iter()
            .flat_map(|scope| scope.rendered.iter().cloned()),
    );
    claimed
}

/// the annotation expressions of a function signature, in source order:
/// parameter annotations followed by the return annotation
fn signature_exprs(func: &StmtFunctionDef) -> Vec<&Expr> {
    let params = &func.parameters;
    let mut exprs = Vec::new();
    for param in params.posonlyargs.iter().chain(&params.args) {
        if let Some(annotation) = &param.parameter.annotation {
            exprs.push(annotation.as_ref());
        }
    }
    if let Some(vararg) = &params.vararg
        && let Some(annotation) = &vararg.annotation
    {
        exprs.push(annotation.as_ref());
    }
    for param in &params.kwonlyargs {
        if let Some(annotation) = &param.parameter.annotation {
            exprs.push(annotation.as_ref());
        }
    }
    if let Some(kwarg) = &params.kwarg
        && let Some(annotation) = &kwarg.annotation
    {
        exprs.push(annotation.as_ref());
    }
    if let Some(returns) = &func.returns {
        exprs.push(returns.as_ref());
    }
    exprs
}

/// outcome of converting a single class
struct ClassConv<'a> {
    edits: Vec<Edit>,
    renamed: Vec<(String, TextRange)>,
    args_range: TextRange,
    /// the class's legacy type-parameter names and their rendered pep 695 names
    param_legacy: Vec<&'a str>,
    param_new: Vec<String>,
}

fn convert_class<'a>(
    class: &'a StmtClassDef,
    bound: &[Scope<'a>],
    ctx: &Ctx<'a>,
) -> Option<ClassConv<'a>> {
    let (table, source) = (ctx.table, ctx.source);
    // already pep 695
    if class.type_params.is_some() {
        return None;
    }
    let args = class.arguments.as_deref()?;
    let params = class_params(args, table)?;
    if params.is_empty() {
        return None;
    }

    // assign each parameter a unique nice/mechanical name, avoiding names the
    // module binds or an enclosing scope already uses as well as collisions
    // within this class
    let mut used = claimed_names(&ctx.module_names, bound);
    let mut new_names = Vec::with_capacity(params.len());
    for legacy in &params {
        new_names.push(pick_name(legacy, table[*legacy].kind, &mut used));
    }
    let renames: HashMap<&str, &str> = params
        .iter()
        .zip(&new_names)
        .map(|(legacy, new)| (*legacy, new.as_str()))
        .collect();

    let mut edits = Vec::new();

    // rewrite the header from the end of the class name through the base list
    // in one edit: insert the pep 695 type-parameter list, then the rebuilt
    // bases (dropping `Generic[...]`, baring `Protocol[...]`, renaming typevar
    // references in the survivors). doing it as a single edit avoids a
    // zero-width insert colliding with the base-list replacement when the name
    // abuts the opening paren
    let param_list = render_params(&params, table, &new_names, &renames, source, true);
    let rebuilt = rebuild_args(args, &renames, source);
    let name_end = class.name.range().end().to_usize();
    let args_range = args.range();
    edits.push(Edit {
        start: name_end,
        end: args_range.end().to_usize(),
        replacement: format!("{param_list}{rebuilt}"),
    });

    // rename class-parameter references throughout the class body. descend into
    // methods (they use the class parameters) but not into nested classes
    let mut body = RefRenamer {
        renames: &renames,
        skip_functions: false,
        hits: Vec::new(),
    };
    for stmt in &class.body {
        body.visit_stmt(stmt);
    }
    let mut renamed = Vec::new();
    for (legacy, range) in body.hits {
        edits.push(Edit {
            start: range.start().to_usize(),
            end: range.end().to_usize(),
            replacement: renames[legacy].to_string(),
        });
        renamed.push((legacy.to_string(), range));
    }

    Some(ClassConv {
        edits,
        renamed,
        args_range,
        param_legacy: params,
        param_new: new_names,
    })
}

/// the ordered legacy type-parameter names of a class, or `None` if the class
/// is non-generic or uses a typevar we don't recognise (in which case it is
/// left untouched)
fn class_params<'a>(args: &'a Arguments, table: &Table<'a>) -> Option<Vec<&'a str>> {
    // an explicit `Generic[...]`/`Protocol[...]` base is the source of truth.
    // `Generic` wins if both appear
    let mut decl_subscript: Option<&ExprSubscript> = None;
    for base in &args.args {
        match generic_decl(base) {
            Some(("Generic", sub)) => {
                decl_subscript = Some(sub);
                break;
            }
            Some(("Protocol", sub)) if decl_subscript.is_none() => decl_subscript = Some(sub),
            _ => {}
        }
    }

    if let Some(sub) = decl_subscript {
        return subscript_params(sub, table);
    }

    // otherwise the parameters are the free typevars across all bases, in order
    let mut seen = Vec::new();
    for base in &args.args {
        collect_free_typevars(base, table, &mut seen);
    }
    (!seen.is_empty()).then_some(seen)
}

/// type-parameter names from a `Generic[...]`/`Protocol[...]` subscript. every
/// element must be a known typevar, else `None`
fn subscript_params<'a>(sub: &'a ExprSubscript, table: &Table<'a>) -> Option<Vec<&'a str>> {
    let mut names = Vec::new();
    match &*sub.slice {
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                names.push(subscript_element(elt, table)?);
            }
        }
        single => names.push(subscript_element(single, table)?),
    }
    Some(names)
}

fn subscript_element<'a>(elt: &'a Expr, table: &Table<'a>) -> Option<&'a str> {
    let name = match elt {
        Expr::Name(name) => name.id.as_str(),
        // `Generic[*Ts]` for a TypeVarTuple
        Expr::Starred(starred) => starred.value.as_name_expr()?.id.as_str(),
        _ => return None,
    };
    table.contains_key(name).then_some(name)
}

/// `Generic[...]` or `Protocol[...]` -> (`"Generic"`/`"Protocol"`, subscript)
fn generic_decl(base: &Expr) -> Option<(&'static str, &ExprSubscript)> {
    let Expr::Subscript(sub) = base else {
        return None;
    };
    let head = match &*sub.value {
        Expr::Name(name) => name.id.as_str(),
        Expr::Attribute(attr) => attr.attr.as_str(),
        _ => return None,
    };
    match head {
        "Generic" => Some(("Generic", sub)),
        "Protocol" => Some(("Protocol", sub)),
        _ => None,
    }
}

/// rebuild a class's base list: drop `Generic[...]`, replace `Protocol[...]`
/// with bare `Protocol`, keep the rest with typevar references renamed
fn rebuild_args(args: &Arguments, renames: &HashMap<&str, &str>, source: &str) -> String {
    let mut parts = Vec::new();
    for base in &args.args {
        match generic_decl(base) {
            Some(("Generic", _)) => continue,
            Some(("Protocol", _)) => parts.push("Protocol".to_string()),
            _ => parts.push(render_expr(base, source, renames)),
        }
    }
    for keyword in &args.keywords {
        parts.push(render_keyword(keyword, source, renames));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn render_keyword(keyword: &Keyword, source: &str, renames: &HashMap<&str, &str>) -> String {
    let value = render_expr(&keyword.value, source, renames);
    match &keyword.arg {
        Some(arg) => format!("{arg}={value}"),
        None => format!("**{value}"),
    }
}

/// render the `[...]` type-parameter list. `with_variance` controls whether
/// `out`/`in`/`in out` keywords are emitted — classes carry variance, functions
/// don't
fn render_params(
    params: &[&str],
    table: &Table,
    new_names: &[String],
    renames: &HashMap<&str, &str>,
    source: &str,
    with_variance: bool,
) -> String {
    let mut out = String::from("[");
    for (i, (legacy, new_name)) in params.iter().zip(new_names).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        let decl = &table[*legacy];
        match decl.kind {
            TvKind::TypeVar => {
                if with_variance {
                    out.push_str(decl.variance.keyword());
                }
                out.push_str(new_name);
                if let Some(bound) = decl.bound {
                    out.push_str(": ");
                    out.push_str(&render_expr(bound, source, renames));
                } else if !decl.constraints.is_empty() {
                    // basedpython: `T: (a, b)` is a tuple bound, so a constraint set is
                    // written as the type mapping `T in (a, b)`
                    out.push_str(" in (");
                    for (j, constraint) in decl.constraints.iter().enumerate() {
                        if j > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&render_expr(constraint, source, renames));
                    }
                    out.push(')');
                }
            }
            TvKind::TypeVarTuple => {
                out.push('*');
                out.push_str(new_name);
            }
            // basedpython spells a `ParamSpec` as a type variable bound by the
            // *top parameters* form — every parameter list is a subtype of it,
            // so the variable ranges over parameter lists. `[**P]` is reserved
            // for a keyword-variadic pack
            TvKind::ParamSpec => {
                out.push_str(new_name);
                out.push_str(": ");
                out.push_str(TOP_PARAMETERS_FORM);
            }
        }
        if let Some(default) = decl.default {
            out.push_str(" = ");
            out.push_str(&render_expr(default, source, renames));
        }
    }
    out.push(']');
    out
}

/// render `expr` from source, substituting any typevar names found in
/// `renames`
fn render_expr(expr: &Expr, source: &str, renames: &HashMap<&str, &str>) -> String {
    let mut collector = NameCollector {
        renames,
        hits: Vec::new(),
    };
    collector.visit_expr(expr);
    collector.hits.sort_by_key(|(range, _)| range.start());

    let base = expr.range().start().to_usize();
    let slice = &source[expr.range()];
    let mut out = String::new();
    let mut cursor = 0;
    for (range, replacement) in collector.hits {
        let start = range.start().to_usize() - base;
        let end = range.end().to_usize() - base;
        out.push_str(&slice[cursor..start]);
        out.push_str(replacement);
        cursor = end;
    }
    out.push_str(&slice[cursor..]);
    out
}

/// collects renamed name references within an expression
struct NameCollector<'a, 'src> {
    renames: &'a HashMap<&'src str, &'src str>,
    hits: Vec<(TextRange, &'src str)>,
}

impl<'a> SourceOrderVisitor<'a> for NameCollector<'_, '_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && let Some(replacement) = self.renames.get(name.id.as_str())
        {
            self.hits.push((name.range(), replacement));
        }
        walk_expr(self, expr);
    }
}

/// collects type-parameter references to rename, without descending into
/// scopes that own their own type parameters. a class renamer descends into its
/// methods (`skip_functions = false`) since they use the class parameters; a
/// function renamer skips nested functions (`skip_functions = true`). both skip
/// nested classes
struct RefRenamer<'a, 'src> {
    renames: &'a HashMap<&'src str, &'src str>,
    skip_functions: bool,
    hits: Vec<(&'src str, TextRange)>,
}

impl<'a> SourceOrderVisitor<'a> for RefRenamer<'_, 'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::ClassDef(_) => return,
            Stmt::FunctionDef(_) if self.skip_functions => return,
            _ => {}
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && let Some((legacy, _)) = self.renames.get_key_value(name.id.as_str())
        {
            self.hits.push((legacy, name.range()));
        }
        walk_expr(self, expr);
    }
}

/// pick a fresh pep 695 name for `legacy`, recording it in `used`. tries the
/// curated nice name first, then the mechanical name, then a numeric suffix —
/// so a collision with a module binding or an enclosing scope's name (e.g. a
/// method's element type vs its class's `Element`) degrades to a clean
/// alternative rather than `Element2`
fn pick_name(legacy: &str, kind: TvKind, used: &mut HashSet<String>) -> String {
    let nice = nice_name(legacy, kind);
    let mechanical = mechanical_name(legacy);
    for candidate in nice
        .map(str::to_string)
        .into_iter()
        .chain([mechanical.clone()])
    {
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    let base = nice.map_or(mechanical, str::to_string);
    let mut n = 2;
    loop {
        let candidate = format!("{base}{n}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
        n += 1;
    }
}

/// curated names for the core container and protocol typevars. keyed by kind as
/// well as legacy name: `_P` is `Parameters` only when it really is a
/// `ParamSpec`, never when a module happens to call a plain `TypeVar` that
///
/// the keys are the names upstream declares, variance suffix included — a
/// generator yields `_YieldT_co`, not `_YieldT`
fn nice_name(legacy: &str, kind: TvKind) -> Option<&'static str> {
    Some(match (kind, legacy) {
        (TvKind::TypeVar, "_T" | "_T_co") => "Element",
        (TvKind::TypeVar, "_T_contra") => "Input",
        (TvKind::TypeVar, "_KT" | "_KT_co") => "Key",
        (TvKind::TypeVar, "_VT" | "_VT_co") => "Value",
        (TvKind::TypeVar, "_S") => "Other",
        (TvKind::TypeVar, "_YieldT_co") => "Yield",
        (TvKind::TypeVar, "_SendT_contra" | "_SendT_and_contra") => "Send",
        (TvKind::TypeVar, "_ReturnT_co" | "_ReturnT_and_co") => "Return",
        (TvKind::TypeVarTuple, "_Ts") => "Args",
        (TvKind::ParamSpec, "_P") => "Parameters",
        _ => return None,
    })
}

/// mechanical fallback: strip the leading underscore and the `_co`/`_contra`
/// variance suffix
fn mechanical_name(legacy: &str) -> String {
    let trimmed = legacy.trim_start_matches('_');
    let stripped = trimmed
        .strip_suffix("_co")
        .or_else(|| trimmed.strip_suffix("_contra"))
        .unwrap_or(trimmed);
    if stripped.is_empty() {
        trimmed.to_string()
    } else {
        stripped.to_string()
    }
}

/// every name bound at module level — imports, class/function definitions,
/// assignment targets — that a type-parameter name would shadow
///
/// the typevar declarations themselves are excluded: a public typevar like
/// `AnyStr` keeps its declaration and lends its own name to the parameter it
/// becomes, which is a deliberate reuse rather than a shadow
fn collect_module_names<'a>(body: &'a [Stmt], table: &Table<'a>) -> HashSet<String> {
    let mut names = HashSet::new();
    walk_module_names(body, &mut names);
    names.retain(|name| !table.contains_key(name.as_str()));
    names
}

fn walk_module_names(body: &[Stmt], names: &mut HashSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Import(node) => {
                for alias in &node.names {
                    // `import a.b` binds `a`; `import a.b as c` binds `c`
                    let bound = match &alias.asname {
                        Some(asname) => asname.as_str(),
                        None => alias.name.split('.').next().unwrap_or(&alias.name),
                    };
                    names.insert(bound.to_string());
                }
            }
            Stmt::ImportFrom(node) => {
                for alias in &node.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    if bound.as_str() != "*" {
                        names.insert(bound.to_string());
                    }
                }
            }
            Stmt::ClassDef(class) => {
                names.insert(class.name.to_string());
            }
            Stmt::FunctionDef(func) => {
                names.insert(func.name.to_string());
            }
            Stmt::TypeAlias(alias) => bind_target(&alias.name, names),
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    bind_target(target, names);
                }
            }
            Stmt::AnnAssign(assign) => bind_target(&assign.target, names),
            Stmt::AugAssign(assign) => bind_target(&assign.target, names),
            Stmt::If(node) => {
                walk_module_names(&node.body, names);
                for clause in &node.elif_else_clauses {
                    walk_module_names(&clause.body, names);
                }
            }
            Stmt::Try(node) => {
                walk_module_names(&node.body, names);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    walk_module_names(&handler.body, names);
                }
                walk_module_names(&node.orelse, names);
                walk_module_names(&node.finalbody, names);
            }
            Stmt::With(node) => {
                for item in &node.items {
                    if let Some(target) = &item.optional_vars {
                        bind_target(target, names);
                    }
                }
                walk_module_names(&node.body, names);
            }
            _ => {}
        }
    }
}

/// record the names an assignment target binds
fn bind_target(target: &Expr, names: &mut HashSet<String>) {
    match target {
        Expr::Name(name) => {
            names.insert(name.id.to_string());
        }
        Expr::Tuple(tuple) => {
            for elt in &tuple.elts {
                bind_target(elt, names);
            }
        }
        Expr::List(list) => {
            for elt in &list.elts {
                bind_target(elt, names);
            }
        }
        Expr::Starred(starred) => bind_target(&starred.value, names),
        _ => {}
    }
}

/// collect module-level typevar declarations, descending into version guards
/// (`if`/`try`/`with`) but not into class or function scopes
fn collect_decls(body: &[Stmt]) -> Table<'_> {
    let mut table = Table::new();
    walk_decls(body, &mut table);
    table
}

fn walk_decls<'a>(body: &'a [Stmt], table: &mut Table<'a>) {
    for stmt in body {
        match stmt {
            Stmt::Assign(assign) => {
                if let Some((name, decl)) = parse_decl(assign) {
                    table.insert(name, decl);
                }
            }
            Stmt::If(node) => {
                walk_decls(&node.body, table);
                for clause in &node.elif_else_clauses {
                    walk_decls(&clause.body, table);
                }
            }
            Stmt::Try(node) => {
                walk_decls(&node.body, table);
                for handler in &node.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    walk_decls(&handler.body, table);
                }
                walk_decls(&node.orelse, table);
                walk_decls(&node.finalbody, table);
            }
            Stmt::With(node) => walk_decls(&node.body, table),
            _ => {}
        }
    }
}

/// parse a `NAME = TypeVar(...)`-style declaration
fn parse_decl(assign: &StmtAssign) -> Option<(&str, Decl<'_>)> {
    let [Expr::Name(target)] = assign.targets.as_slice() else {
        return None;
    };
    let Expr::Call(call) = &*assign.value else {
        return None;
    };
    let kind = match callee_name(&call.func)? {
        "TypeVar" => TvKind::TypeVar,
        "TypeVarTuple" => TvKind::TypeVarTuple,
        "ParamSpec" => TvKind::ParamSpec,
        _ => return None,
    };

    let arguments = &call.arguments;
    let constraints = if kind == TvKind::TypeVar {
        // positional args after the name string are constraints
        arguments.args.iter().skip(1).collect()
    } else {
        Vec::new()
    };

    let mut variance = Variance::Invariant;
    let mut bound = None;
    let mut default = None;
    for keyword in &arguments.keywords {
        match keyword
            .arg
            .as_ref()
            .map(ruff_python_ast::Identifier::as_str)
        {
            Some("bound") => bound = Some(&keyword.value),
            Some("default") => default = Some(&keyword.value),
            Some("covariant") if is_true(&keyword.value) => variance = Variance::Covariant,
            Some("contravariant") if is_true(&keyword.value) => variance = Variance::Contravariant,
            _ => {}
        }
    }

    Some((
        target.id.as_str(),
        Decl {
            kind,
            variance,
            bound,
            constraints,
            default,
            stmt_range: assign.range(),
        },
    ))
}

fn callee_name(func: &Expr) -> Option<&str> {
    match func {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attr) => Some(attr.attr.as_str()),
        _ => None,
    }
}

fn is_true(expr: &Expr) -> bool {
    matches!(expr, Expr::BooleanLiteral(literal) if literal.value)
}

/// collect, per typevar name, every load-context reference in the module
fn collect_uses<'a>(body: &'a [Stmt], table: &Table<'a>) -> HashMap<&'a str, Vec<TextRange>> {
    let mut collector = UseCollector {
        names: table.keys().copied().collect(),
        uses: HashMap::new(),
    };
    for stmt in body {
        collector.visit_stmt(stmt);
    }
    collector.uses
}

struct UseCollector<'a> {
    names: HashSet<&'a str>,
    uses: HashMap<&'a str, Vec<TextRange>>,
}

impl<'a> SourceOrderVisitor<'a> for UseCollector<'a> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && let Some(known) = self.names.get(name.id.as_str())
        {
            self.uses.entry(known).or_default().push(name.range());
        }
        walk_expr(self, expr);
    }
}

/// collect free typevar references in `expr`, in source order, deduplicated
fn collect_free_typevars<'a>(expr: &'a Expr, table: &Table<'a>, out: &mut Vec<&'a str>) {
    let mut collector = FreeTypevars {
        table_names: table.keys().copied().collect(),
        out,
    };
    collector.visit_expr(expr);
}

struct FreeTypevars<'a, 'out> {
    table_names: HashSet<&'a str>,
    out: &'out mut Vec<&'a str>,
}

impl<'a> SourceOrderVisitor<'a> for FreeTypevars<'a, '_> {
    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && name.ctx.is_load()
            && self.table_names.contains(name.id.as_str())
            && !self.out.contains(&name.id.as_str())
        {
            self.out.push(name.id.as_str());
        }
        walk_expr(self, expr);
    }
}

/// edit that deletes the declaration's entire physical line(s): from the start
/// of the line (so leading indentation under a version guard goes too) through
/// the trailing comment and newline. typeshed declares one typevar per line, so
/// extending to line bounds never swallows a neighbour
fn remove_stmt(range: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();
    // back up to the start of the line
    let mut start = range.start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    // extend past the trailing comment and the newline
    let mut end = range.end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1;
    }
    Edit {
        start,
        end,
        replacement: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn convert(src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = convert_module(&parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn covariant_protocol() {
        let src = "\
_T_co = TypeVar(\"_T_co\", covariant=True)
class Iterable(Protocol[_T_co]):
    def __iter__(self) -> Iterator[_T_co]: ...
";
        let expected = "\
class Iterable[out Element](Protocol):
    def __iter__(self) -> Iterator[Element]: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn covariant_mapping_with_generic_base() {
        let src = "\
_KT_co = TypeVar(\"_KT_co\", covariant=True)
_VT_co = TypeVar(\"_VT_co\", covariant=True)
class Mapping(Collection[_KT_co], Generic[_KT_co, _VT_co]):
    def __getitem__(self, key: _KT_co) -> _VT_co: ...
";
        let expected = "\
class Mapping[out Key, out Value](Collection[Key]):
    def __getitem__(self, key: Key) -> Value: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn invariant_dict_implicit_params() {
        let src = "\
_KT = TypeVar(\"_KT\")
_VT = TypeVar(\"_VT\")
class dict(MutableMapping[_KT, _VT]):
    def __getitem__(self, key: _KT) -> _VT: ...
";
        let expected = "\
class dict[in out Key, in out Value](MutableMapping[Key, Value]):
    def __getitem__(self, key: Key) -> Value: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn single_param_only_generic_base_removes_parens() {
        let src = "\
_T = TypeVar(\"_T\")
class Box(Generic[_T]):
    value: _T
";
        let expected = "\
class Box[in out Element]:
    value: Element
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn contravariant_and_default() {
        let src = "\
_T_contra = TypeVar(\"_T_contra\", contravariant=True)
_ReturnT_co = TypeVar(\"_ReturnT_co\", covariant=True, default=None)
class C(Generic[_T_contra, _ReturnT_co]):
    def f(self, x: _T_contra) -> _ReturnT_co: ...
";
        let expected = "\
class C[in Input, out Return = None]:
    def f(self, x: Input) -> Return: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn bound_is_preserved_and_renamed() {
        let src = "\
_T = TypeVar(\"_T\")
_AwaitableT = TypeVar(\"_AwaitableT\", bound=Awaitable[_T])
class C(Generic[_T, _AwaitableT]):
    x: _AwaitableT
";
        // _T is a parameter of C, so the bound's reference to it is renamed.
        // _T's declaration survives: it is still referenced inside
        // _AwaitableT's bound, and we conservatively never remove a
        // declaration whose references aren't all consumed by a conversion
        let expected = "\
_T = TypeVar(\"_T\")
class C[in out Element, in out AwaitableT: Awaitable[Element]]:
    x: AwaitableT
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn constraints_use_type_mapping_form() {
        let src = "\
_Str = TypeVar(\"_Str\", str, bytes)
class C(Generic[_Str]):
    x: _Str
";
        let expected = "\
class C[in out Str in (str, bytes)]:
    x: Str
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn mechanical_names_dedupe_within_class() {
        let src = "\
_T1 = TypeVar(\"_T1\")
_T2 = TypeVar(\"_T2\")
class Pair(Generic[_T1, _T2]):
    first: _T1
    second: _T2
";
        let expected = "\
class Pair[in out T1, in out T2]:
    first: T1
    second: T2
";
        assert_eq!(convert(src), expected);
    }

    /// a `ParamSpec` becomes a type variable bound by the top parameters form —
    /// `[**P]` in a basedpython file declares a keyword-variadic pack instead
    #[test]
    fn paramspec_and_typevartuple() {
        let src = "\
_P = ParamSpec(\"_P\")
_Ts = TypeVarTuple(\"_Ts\")
class C(Generic[_P, _Ts]):
    args: _Ts
";
        let expected = "\
class C[Parameters: (*: *, **: *), *Args]:
    args: Args
";
        assert_eq!(convert(src), expected);
    }

    /// the curated nice name is keyed by kind: a plain `TypeVar` named `_P` is
    /// not a `ParamSpec` and keeps its mechanical name
    #[test]
    fn a_typevar_named_p_is_not_named_parameters() {
        let src = "\
_P = TypeVar(\"_P\")
class C(Generic[_P]):
    value: _P
";
        let expected = "\
class C[in out P]:
    value: P
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn shared_typevar_kept_when_used_by_unconverted_construct() {
        // _T is used by a converted class AND a type alias (which the engine
        // doesn't convert); the alias keeps referencing `_T`, so its
        // declaration must survive
        let src = "\
_T = TypeVar(\"_T\")
class Box(Generic[_T]):
    value: _T
Alias = list[_T]
";
        let expected = "\
_T = TypeVar(\"_T\")
class Box[in out Element]:
    value: Element
Alias = list[_T]
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn public_typevar_declaration_is_kept() {
        // AnyStr has no leading underscore, so it may be imported by other
        // modules; its declaration must survive even though the only use in
        // this module is consumed by the conversion
        let src = "\
AnyStr = TypeVar(\"AnyStr\", str, bytes)
class C(Generic[AnyStr]):
    x: AnyStr
";
        let expected = "\
AnyStr = TypeVar(\"AnyStr\", str, bytes)
class C[in out AnyStr in (str, bytes)]:
    x: AnyStr
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn generic_function_with_bound() {
        // typing.overload: a top-level generic function. no variance keyword,
        // bound preserved, and the now-unused private decl removed
        let src = "\
_F = TypeVar(\"_F\", bound=Callable[..., Any])
def overload(func: _F) -> _F: ...
";
        let expected = "\
def overload[F: Callable[..., Any]](func: F) -> F: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn generic_function_shared_decl_kept() {
        // _F is used by two functions; converting both consumes every reference
        let src = "\
_F = TypeVar(\"_F\", bound=Callable[..., Any])
def overload(func: _F) -> _F: ...
def no_type_check(arg: _F) -> _F: ...
";
        let expected = "\
def overload[F: Callable[..., Any]](func: F) -> F: ...
def no_type_check[F: Callable[..., Any]](arg: F) -> F: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn generic_method_excludes_class_parameters() {
        // _KT_co/_VT_co are the class's parameters (renamed to Key/Value); _S is
        // the method's own type parameter and becomes a pep 695 method generic
        let src = "\
_KT_co = TypeVar(\"_KT_co\", covariant=True)
_VT_co = TypeVar(\"_VT_co\", covariant=True)
_S = TypeVar(\"_S\")
class Mapping(Generic[_KT_co, _VT_co]):
    def get(self, key: _KT_co, default: _S) -> _VT_co | _S: ...
";
        let expected = "\
class Mapping[out Key, out Value]:
    def get[Other](self, key: Key, default: Other) -> Value | Other: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn method_param_avoids_colliding_with_class_param_name() {
        // both _T_co (class) and _T (method) map to the nice name `Element`.
        // the method must not reuse `Element` or it would shadow the class's
        // parameter and collapse `Element | _T` into a single type (this is the
        // tuple.__add__ shape)
        let src = "\
_T_co = TypeVar(\"_T_co\", covariant=True)
_T = TypeVar(\"_T\")
class tuple(Sequence[_T_co]):
    def __add__(self, value: tuple[_T, ...], /) -> tuple[_T_co | _T, ...]: ...
";
        let expected = "\
class tuple[out Element](Sequence[Element]):
    def __add__[T](self, value: tuple[T, ...], /) -> tuple[Element | T, ...]: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn nice_name_avoids_an_imported_module_binding() {
        // xml.etree.ElementPath: `Element` is imported, so `_T` must not take
        // that name or `Element[Any]` would subscript the type parameter
        let src = "\
from xml.etree.ElementTree import Element
_T = TypeVar(\"_T\")
def findtext(elem: Element[Any], default: _T) -> _T | str: ...
";
        let expected = "\
from xml.etree.ElementTree import Element
def findtext[T](elem: Element[Any], default: T) -> T | str: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn mechanical_name_avoids_a_module_binding_too() {
        // both `Element` (nice) and `T` (mechanical) are taken, so the numeric
        // suffix is the only way out
        let src = "\
from a import Element, T
_T = TypeVar(\"_T\")
def f(x: _T) -> _T: ...
";
        let expected = "\
from a import Element, T
def f[Element2](x: Element2) -> Element2: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn class_and_assignment_bindings_are_avoided() {
        let src = "\
class Key: ...
Value: int
_KT = TypeVar(\"_KT\")
_VT = TypeVar(\"_VT\")
class d(Generic[_KT, _VT]):
    def get(self, key: _KT) -> _VT: ...
";
        let expected = "\
class Key: ...
Value: int
class d[in out KT, in out VT]:
    def get(self, key: KT) -> VT: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn binding_under_a_version_guard_is_avoided() {
        let src = "\
if sys.version_info >= (3, 12):
    from a import Element
_T = TypeVar(\"_T\")
class Box(Generic[_T]):
    value: _T
";
        let expected = "\
if sys.version_info >= (3, 12):
    from a import Element
class Box[in out T]:
    value: T
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn generic_function_with_paramspec() {
        let src = "\
_P = ParamSpec(\"_P\")
_R = TypeVar(\"_R\")
def wrap(f: Callable[_P, _R]) -> Callable[_P, _R]: ...
";
        let expected = "\
def wrap[Parameters: (*: *, **: *), R](f: Callable[Parameters, R]) -> Callable[Parameters, R]: ...
";
        assert_eq!(convert(src), expected);
    }

    #[test]
    fn non_generic_function_untouched() {
        let src = "\
_T = TypeVar(\"_T\")
def f(x: int) -> int: ...
";
        assert_eq!(convert(src), src);
    }

    #[test]
    fn unknown_typevar_leaves_class_untouched() {
        // _Imported is not declared in this module, so the class is left alone
        let src = "\
class C(Generic[_Imported]):
    x: _Imported
";
        assert_eq!(convert(src), src);
    }

    #[test]
    fn non_generic_class_untouched() {
        let src = "\
_T = TypeVar(\"_T\")
class C(object):
    pass
";
        // _T is unused, but we only remove a declaration once a conversion has
        // consumed all of its references; an already-unused one is left alone
        assert_eq!(convert(src), src);
    }

    #[test]
    fn idempotent_on_pep695() {
        let src = "\
class C[out Element](Protocol):
    x: Element
";
        assert_eq!(convert(src), src);
    }
}
