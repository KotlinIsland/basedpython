//! the runtime helpers the emitted python calls, and the two ways a module gets
//! at them
//!
//! the helpers live in [`SOURCE`], which is `_by_runtime.py`. a build writes that
//! file beside the modules it emits and each module imports the names it calls. a
//! transpile with nowhere to write it (`by transpile <file>`, the language
//! server's `by/transpile`) pastes the definitions in instead. both are slices of
//! the one text, so the two renderings cannot drift apart. the one difference is a
//! builtin the module binds for itself, which a pasted definition would otherwise
//! read from the module's globals: it reads it under a name of its own (`inline`)
//!
//! # naming a helper
//!
//! a transform names a helper through one of the `Helper` constants, never a
//! string, so a misspelled helper does not compile. `every_helper_is_defined`
//! holds each constant to the file
//!
//! # slicing
//!
//! a helper resolves to the top-level statement that binds its name, as our own
//! parser reads the file. a comment written between two definitions falls outside
//! both and never reaches the output; one inside a body is part of it. a
//! top-level statement that binds nothing, such as
//! `_by_forward_operators(_LazyAttr)`, travels with the definition above it
//!
//! # dependencies
//!
//! helpers call each other: `_lazy_attr` resolves through `_lazy_module`,
//! `_soundness_parametric` through `_parametric_is`. what a helper needs is read
//! out of the calls in its body, so asking for one yields a set that runs

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, walk_expr};
use ruff_python_ast::{self as ast, ExceptHandler, Expr, Stmt};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextRange};

/// the runtime, as the file a build writes out
pub const SOURCE: &str = include_str!("runtime/_by_runtime.py");

/// the module a build writes [`SOURCE`] to, without a package qualifier
pub const MODULE_NAME: &str = "_by_runtime";

/// the file a build writes [`SOURCE`] to
pub const FILE_NAME: &str = "_by_runtime.py";

/// a runtime helper the emitted python calls, by the name it calls it under
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Helper(&'static str);

impl Helper {
    pub(crate) const fn name(self) -> &'static str {
        self.0
    }
}

macro_rules! helpers {
    ($($konst:ident = $name:literal,)*) => {
        $(pub(crate) const $konst: Helper = Helper($name);)*

        /// every helper a transform can name
        #[cfg(test)]
        const ALL: &[Helper] = &[$($konst),*];
    };
}

helpers! {
    SOUNDNESS_CHECK = "_soundness_check",
    SOUNDNESS_ITER = "_soundness_iter",
    SOUNDNESS_AITER = "_soundness_aiter",
    SOUNDNESS_PARAMETRIC = "_soundness_parametric",
    SOUNDNESS_ITER_P = "_soundness_iter_p",
    SOUNDNESS_AITER_P = "_soundness_aiter_p",
    CHECKED_CAST = "_checked_cast",
    TRY_CAST = "_try_cast",
    CHECKED_CAST_PRED = "_checked_cast_pred",
    TRY_CAST_PRED = "_try_cast_pred",
    PARAMETRIC_IS = "_parametric_is",
    PARAMETRIC_IS_LENIENT = "_parametric_is_lenient",
    PROTOCOL_IS = "_by_protocol_is",
    LITERAL = "_by_lit",
    PATTERN_IS = "_by_pattern_is",
    CONFORM = "_by_conform",
    CONFORMS = "_by_conforms",
    WITNESS = "_by_witness",
    WITNESS_CLASS = "_by_witness_class",
    WITNESS_GET = "_by_witness_get",
    TEMPLATE = "_Template",
    INTERPOLATION = "_Interpolation",
    TEMPLATE_TEXT = "_by_template",
    GRAPHEMES = "_by_graphemes",
    PREFIX = "_by_prefix",
    SUFFIX = "_by_suffix",
    GENERIC = "generic",
    GENERIC_CLASS = "generic_class",
    TYPE_ARGUMENT = "_type_argument",
    DISCARD = "_by_discard",
    LOOP_BIND = "_by_loop_bind",
    STATIC_PROPERTY = "_by_static_property",
    OPTIONAL = "Optional",
    FORCE_UNWRAP = "_force_unwrap",
    MAIN_ARGS = "_by_main_args",
    RAISES = "_by_raises",
    LAZY_MODULE = "_lazy_module",
    LAZY_ATTR = "_lazy_attr",
    TY_EXT_MARKER = "_TyExtMarker",
    CHARACTER = "Character",
    MATCH_MISS = "_by_match_miss",
    MATCH_SEQ = "_by_match_seq",
    MATCH_MAP = "_by_match_map",
    MATCH_KEY = "_by_match_key",
    MATCH_REST = "_by_match_rest",
    MATCH_ARGS = "_by_match_args",
    MATCH_ATTR = "_by_match_attr",
    DATACLASS_SLOTS = "_by_dataclass_slots",
}

mod global_reads;

/// one definition of the runtime, with whatever finishes setting it up
struct Definition {
    /// its source as the file spells it, newline-terminated like any other
    /// preamble entry
    source: String,
    /// position in the file. a set of definitions renders in this order, which
    /// is the order they can be executed in
    order: usize,
    /// the other definitions its body reads
    needs: Vec<String>,
    /// each read of a name the runtime does not define, which python looks up in the
    /// globals of the module the definition runs in before the builtins, by its range in
    /// [`Self::source`]
    builtin_reads: Vec<(TextRange, String)>,
}

/// every definition, indexed by each name it binds
fn index() -> &'static BTreeMap<String, Definition> {
    static INDEX: OnceLock<BTreeMap<String, Definition>> = OnceLock::new();
    INDEX.get_or_init(build_index)
}

fn build_index() -> BTreeMap<String, Definition> {
    // `the_runtime_parses` pins the file as valid python. were it not, every
    // helper would come back undefined and phase 3 would report the calls
    let Ok(parsed) = parse_module(SOURCE) else {
        return BTreeMap::new();
    };
    let suite = parsed.suite();
    // two passes: the first learns every name the file defines, so the second can
    // tell a call to a sibling from a call to a builtin
    let defined: BTreeSet<String> = suite
        .iter()
        .flat_map(|stmt| bindings(stmt).into_keys())
        .collect();

    let mut definitions = BTreeMap::new();
    let mut order = 0usize;
    let mut at = 0usize;
    while at < suite.len() {
        let names = bindings(&suite[at]);
        if names.is_empty() {
            // the module docstring: every later statement that binds nothing is
            // swept up by the definition before it
            at += 1;
            continue;
        }
        let mut last = at;
        while last + 1 < suite.len() && bindings(&suite[last + 1]).is_empty() {
            last += 1;
        }
        let span = suite[at].range().cover(suite[last].range());
        let mut source = SOURCE[span].to_owned();
        if !source.ends_with('\n') {
            source.push('\n');
        }
        let mut needs: Vec<String> = suite[at..=last]
            .iter()
            .flat_map(reads)
            .filter(|read| defined.contains(read) && !names.contains_key(read))
            .collect();
        needs.sort();
        needs.dedup();
        let builtin_reads: Vec<(TextRange, String)> = global_reads::global_reads(&suite[at..=last])
            .into_iter()
            .filter(|(_, name)| !defined.contains(name))
            .map(|(range, name)| (range - span.start(), name))
            .collect();
        // `every_name_has_one_definition` keeps a name from being bound twice,
        // so first-wins here never decides anything
        for name in names.into_keys() {
            definitions.entry(name).or_insert_with(|| Definition {
                source: source.clone(),
                order,
                needs: needs.clone(),
                builtin_reads: builtin_reads.clone(),
            });
        }
        order += 1;
        at = last + 1;
    }
    definitions
}

/// the names a statement binds, each with the range of the binding itself
///
/// the range is what a refusal points at, so it names the `x` of `x = 1` rather than the
/// whole statement
pub(crate) type Bindings = BTreeMap<String, TextRange>;

/// record `name` as bound at `range`, keeping the first of several bindings of one name —
/// which is the one an author reading the refusal will find first
fn record(names: &mut Bindings, name: impl Into<String>, range: TextRange) {
    names.entry(name.into()).or_insert(range);
}

/// the names a statement binds in the scope it stands in
pub(crate) fn bindings(stmt: &Stmt) -> Bindings {
    let mut names = Bindings::new();
    bind(stmt, &mut names);
    names
}

/// the names a top-level statement binds at module scope: the ones it binds itself
/// ([`bindings`]), and the ones a function or class inside it, at any depth, declares
/// `global` and binds
pub(crate) fn module_bindings(stmt: &Stmt) -> Bindings {
    let mut names = bindings(stmt);
    global_bindings(stmt, &mut names);
    names
}

/// the names the scopes `stmt` opens, and the scopes inside those, declare `global` and bind
fn global_bindings(stmt: &Stmt, names: &mut Bindings) {
    let body = match stmt {
        Stmt::FunctionDef(def) => &def.body,
        Stmt::ClassDef(def) => &def.body,
        _ => {
            let mut blocks = Vec::new();
            push_blocks(stmt, &mut blocks);
            for block in blocks {
                for stmt in block {
                    global_bindings(stmt, names);
                }
            }
            return;
        }
    };
    let mut declared = BTreeSet::new();
    global_declarations(body, &mut declared);
    let mut bound = Bindings::new();
    for stmt in body {
        bind(stmt, &mut bound);
        global_bindings(stmt, names);
    }
    for (name, range) in bound {
        if declared.contains(&name) {
            record(names, name, range);
        }
    }
}

/// the names `body`'s own statements declare `global`, not descending into the scopes they
/// open
fn global_declarations(body: &[Stmt], out: &mut BTreeSet<String>) {
    for stmt in body {
        if let Stmt::Global(node) = stmt {
            out.extend(node.names.iter().map(ToString::to_string));
        } else {
            let mut blocks = Vec::new();
            push_blocks(stmt, &mut blocks);
            for block in blocks {
                global_declarations(block, out);
            }
        }
    }
}

/// a block that runs at module scope (an `if`, a `try`, a loop, a `case`) binds what the
/// statements inside it bind. a function or class body is a scope of its own
///
/// every statement is answered for by name, with no `_` arm, because the misses in this
/// file have all been of one kind: a binding form nobody thought of. a `match` case, a
/// walrus, a destructuring `let`, an `if let` clause and the statement a statement
/// expression holds were each found on their own, after a module that spelled a runtime
/// helper's name with one of them was lowered into a call on its own value. with the arms
/// spelled out, a statement added to the syntax tree stops this file compiling until
/// somebody says what it binds
fn bind(stmt: &Stmt, names: &mut Bindings) {
    let block = |body: &[Stmt], names: &mut Bindings| {
        for stmt in body {
            bind(stmt, names);
        }
    };
    match stmt {
        Stmt::FunctionDef(def) => record(names, def.name.as_str(), def.name.range()),
        Stmt::ClassDef(def) => record(names, def.name.as_str(), def.name.range()),
        Stmt::Assign(assign) => {
            for target in &assign.targets {
                bind_target(target, names);
            }
        }
        Stmt::AnnAssign(assign) => bind_target(&assign.target, names),
        Stmt::AugAssign(assign) => bind_target(&assign.target, names),
        Stmt::TypeAlias(alias) => bind_target(&alias.name, names),
        Stmt::Import(import) => {
            for alias in &import.names {
                let bound = match &alias.asname {
                    Some(asname) => asname.to_string(),
                    // `import a.b` binds `a`
                    None => alias.name.split('.').next().unwrap_or_default().to_string(),
                };
                record(names, bound, alias.range());
            }
        }
        Stmt::ImportFrom(import) => {
            for alias in &import.names {
                let bound = match &alias.asname {
                    Some(asname) => asname.to_string(),
                    None => alias.name.to_string(),
                };
                record(names, bound, alias.range());
            }
        }
        Stmt::If(node) => {
            // basedpython `if let <pattern> := <subject>:` binds what the pattern captures,
            // for the clause it heads
            if let Some(pattern) = &node.pattern {
                bind_pattern(pattern, names);
            }
            block(&node.body, names);
            for clause in &node.elif_else_clauses {
                if let Some(pattern) = &clause.pattern {
                    bind_pattern(pattern, names);
                }
                block(&clause.body, names);
            }
        }
        // basedpython `let <pattern> := <subject>`, whose `else` block runs where the
        // pattern did not match
        Stmt::Let(node) => {
            bind_pattern(&node.pattern, names);
            block(&node.orelse, names);
        }
        Stmt::Try(node) => {
            block(&node.body, names);
            for ExceptHandler::ExceptHandler(handler) in &node.handlers {
                if let Some(bound) = &handler.name {
                    record(names, bound.as_str(), bound.range());
                }
                block(&handler.body, names);
            }
            block(&node.orelse, names);
            block(&node.finalbody, names);
        }
        Stmt::With(node) => {
            for item in &node.items {
                if let Some(target) = &item.optional_vars {
                    bind_target(target, names);
                }
            }
            block(&node.body, names);
        }
        Stmt::For(node) => {
            bind_target(&node.target, names);
            // basedpython `for <pattern> in <iter>:`, where `target` above is the binder the
            // pattern destructures
            if let Some(pattern) = &node.pattern {
                bind_pattern(pattern, names);
            }
            block(&node.body, names);
            block(&node.orelse, names);
        }
        Stmt::While(node) => {
            block(&node.body, names);
            block(&node.orelse, names);
        }
        Stmt::Match(node) => {
            for case in &node.cases {
                bind_pattern(&case.pattern, names);
                block(&case.body, names);
            }
        }
        // these bind nothing of their own. `global` and `nonlocal` name a binding made
        // elsewhere, and `del` removes one rather than making it — a module that deletes a
        // helper's name has taken it away from the lowered lines below, which is the
        // deleted-author-read hole the shadowing check documents rather than answers
        Stmt::Return(_)
        | Stmt::Delete(_)
        | Stmt::Raise(_)
        | Stmt::Assert(_)
        | Stmt::Global(_)
        | Stmt::Nonlocal(_)
        | Stmt::Expr(_)
        | Stmt::Pass(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::IpyEscapeCommand(_) => {}
    }
    bind_in_own_expressions(stmt, names);
}

/// the names a `case` pattern binds: its captures, its `as` names and the `**rest` of a
/// mapping pattern
fn bind_pattern(pattern: &ast::Pattern, names: &mut Bindings) {
    struct Captures<'a>(&'a mut Bindings);
    impl<'b> SourceOrderVisitor<'b> for Captures<'_> {
        fn visit_pattern(&mut self, pattern: &'b ast::Pattern) {
            match pattern {
                ast::Pattern::MatchAs(node) => {
                    if let Some(name) = &node.name {
                        record(self.0, name.as_str(), name.range());
                    }
                }
                ast::Pattern::MatchStar(node) => {
                    if let Some(name) = &node.name {
                        record(self.0, name.as_str(), name.range());
                    }
                }
                ast::Pattern::MatchMapping(node) => {
                    if let Some(rest) = &node.rest {
                        record(self.0, rest.as_str(), rest.range());
                    }
                }
                // the rest hold sub-patterns and nothing of their own; the walk below
                // reaches every capture in them
                ast::Pattern::MatchValue(_)
                | ast::Pattern::MatchSingleton(_)
                | ast::Pattern::MatchSequence(_)
                | ast::Pattern::MatchClass(_)
                | ast::Pattern::MatchOr(_)
                | ast::Pattern::MatchAnd(_) => {}
            }
            ruff_python_ast::visitor::source_order::walk_pattern(self, pattern);
        }
    }
    Captures(names).visit_pattern(pattern);
}

/// the names a statement's own expressions bind: a `:=`, and the statement a basedpython
/// statement expression holds
///
/// a walrus binds where the expression around it is evaluated, which is the scope the
/// statement is written in — a comprehension's own scope is deliberately escaped, so
/// `[y for x in xs if (n := f(x))]` binds `n` beside the comprehension. the one exception is a
/// lambda body, which is a scope of its own
///
/// a statement expression (`a = match x: ...`) holds a statement that runs where the
/// expression is written, so what it binds is bound here too
fn bind_in_own_expressions(stmt: &Stmt, names: &mut Bindings) {
    struct Own<'a>(&'a mut Bindings);
    impl<'b> SourceOrderVisitor<'b> for Own<'_> {
        fn visit_expr(&mut self, expr: &'b Expr) {
            match expr {
                Expr::Named(named) => bind_target(&named.target, self.0),
                Expr::Statement(held) => {
                    bind(&held.stmt, self.0);
                    return;
                }
                // a lambda body is a scope of its own, so a walrus in it binds there
                Expr::Lambda(_) => return,
                _ => {}
            }
            walk_expr(self, expr);
        }
        // the suites this statement opens are walked as statements by `bind` itself
        fn visit_stmt(&mut self, _: &'b Stmt) {}
    }
    ruff_python_ast::visitor::source_order::walk_stmt(&mut Own(names), stmt);
}

/// the names an assignment target binds, unpacking a tuple or list target
///
/// spelled out over every expression, for the reason [`bind`] is
fn bind_target(target: &Expr, names: &mut Bindings) {
    match target {
        Expr::Name(name) => record(names, name.id.as_str(), name.range()),
        Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                bind_target(element, names);
            }
        }
        Expr::List(list) => {
            for element in &list.elts {
                bind_target(element, names);
            }
        }
        Expr::Starred(starred) => bind_target(&starred.value, names),
        // an attribute or a subscript target writes through a value rather than binding a
        // name of its own, and nothing else is a target python accepts — a parenthesised
        // target reaches here as what it parenthesises
        Expr::Attribute(_)
        | Expr::Subscript(_)
        | Expr::BoolOp(_)
        | Expr::Named(_)
        | Expr::BinOp(_)
        | Expr::UnaryOp(_)
        | Expr::Lambda(_)
        | Expr::If(_)
        | Expr::Dict(_)
        | Expr::Set(_)
        | Expr::ListComp(_)
        | Expr::SetComp(_)
        | Expr::DictComp(_)
        | Expr::Generator(_)
        | Expr::Await(_)
        | Expr::Yield(_)
        | Expr::YieldFrom(_)
        | Expr::Compare(_)
        | Expr::Call(_)
        | Expr::FString(_)
        | Expr::TString(_)
        | Expr::StringLiteral(_)
        | Expr::BytesLiteral(_)
        | Expr::NumberLiteral(_)
        | Expr::BooleanLiteral(_)
        | Expr::NoneLiteral(_)
        | Expr::EllipsisLiteral(_)
        | Expr::Slice(_)
        | Expr::IpyEscapeCommand(_)
        | Expr::CallableType(_)
        | Expr::ProtocolType(_)
        | Expr::ProtocolMethod(_)
        | Expr::Statement(_) => {}
    }
}

/// what a module does with a helper's name inside a scope of its own
///
/// the helpers only ever reach a module at module scope, so a `def`, `class`,
/// `lambda` or comprehension that binds one of their names takes it over for
/// everything inside it. this is what such a scope looks like, and what is read
/// under it
pub(crate) struct Shadowing {
    /// each scope that binds a helper name, as `(the name, the range of the binding itself,
    /// how to describe the scope)`, in source order
    ///
    /// the range is the binding's rather than the scope's, because a scope with no name of
    /// its own — a lambda, a comprehension — gives an author nothing to search for, and the
    /// line the name is bound on does
    pub(crate) scopes: Vec<(String, TextRange, String)>,
    /// each load of a helper name that one of those bindings, rather than module scope, answers,
    /// as `(the name, the scope whose binding answers it)`
    pub(crate) reads: Vec<(String, String)>,
}

/// how many reads of each helper name the runtime's own definitions make under a scope of their
/// own, by `(helper name, scope)`
///
/// the preamble is part of the module phase 3 reads, and some of it shadows on purpose:
/// `_by_character_class` builds a local `class Character` and hands it back. Those reads are
/// the transpiler's own and say nothing about the module it was given
///
/// the scope is named so a caller can tell whether the definition that makes the read is in
/// the output at all: the runtime reaches a module either pasted in or imported, and only the
/// pasted rendering brings these reads with it
pub(crate) fn own_shadowed_reads() -> &'static BTreeMap<(String, String), usize> {
    static OWN: OnceLock<BTreeMap<(String, String), usize>> = OnceLock::new();
    OWN.get_or_init(|| {
        let Ok(parsed) = parse_module(SOURCE) else {
            return BTreeMap::new();
        };
        let mut counts = BTreeMap::new();
        for (name, scope) in shadowing(parsed.suite()).reads {
            *counts.entry((name, scope)).or_insert(0) += 1;
        }
        counts
    })
}

/// one scope on the walk, holding only the helper names it binds
struct Frame {
    is_class: bool,
    locals: Bindings,
    /// python's own name for the scope — a `def`'s or `class`'s own, or `<lambda>` /
    /// `<listcomp>` / `<setcomp>` / `<dictcomp>` / `<genexpr>`. it is only ever a key: the
    /// author's parse and the lowered output are asked about the same scope under it
    key: String,
    /// how a refusal names the scope: ``​`go`​`` for a scope the author named, and "a list
    /// comprehension" for one they did not. python's `<listcomp>` names nothing an author
    /// wrote, so it is never shown
    described: String,
}

/// find every helper name a scope of the module's own takes over, and every read
/// that lands on one
///
/// module scope is not a frame here: a name bound there is the one the preamble
/// also binds, which `verify_no_helper_name_clash` answers instead
pub(crate) fn shadowing(suite: &[Stmt]) -> Shadowing {
    let mut walk = Walk {
        stack: Vec::new(),
        found: Shadowing {
            scopes: Vec::new(),
            reads: Vec::new(),
        },
    };
    walk.body(suite);
    walk.found
        .scopes
        .sort_by_key(|(name, range, _)| (range.start(), name.clone()));
    walk.found
}

struct Walk {
    stack: Vec<Frame>,
    found: Shadowing,
}

impl Walk {
    /// the innermost scope a load of `name` resolves to, or `None` for module scope.
    /// a class body's names are invisible to the scopes nested in it, so a class frame
    /// only answers a read written directly in it
    fn resolves_to(&self, name: &str) -> Option<&Frame> {
        self.stack
            .iter()
            .rev()
            .enumerate()
            .filter(|(depth, frame)| !frame.is_class || *depth == 0)
            .map(|(_, frame)| frame)
            .find(|frame| frame.locals.contains_key(name))
    }

    fn enter(&mut self, frame: Frame, body: impl FnOnce(&mut Self)) {
        for (name, range) in &frame.locals {
            self.found
                .scopes
                .push((name.clone(), *range, frame.described.clone()));
        }
        self.stack.push(frame);
        body(self);
        self.stack.pop();
    }

    fn body(&mut self, body: &[Stmt]) {
        for stmt in body {
            self.stmt(stmt);
        }
    }

    fn stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(def) => {
                // a decorator, a default and an annotation are all evaluated where the
                // `def` is written, not inside it
                for decorator in &def.decorator_list {
                    self.expr(&decorator.expression);
                }
                for parameter in &*def.parameters {
                    if let Some(annotation) = parameter.annotation() {
                        self.expr(annotation);
                    }
                    if let Some(default) = parameter.default() {
                        self.expr(default);
                    }
                }
                if let Some(returns) = def.returns.as_deref() {
                    self.expr(returns);
                }
                let mut locals = Bindings::new();
                for parameter in &*def.parameters {
                    let bound = parameter.name();
                    if defines(bound.as_str()) {
                        record(&mut locals, bound.as_str(), bound.range());
                    }
                }
                collect(&def.body, &mut locals);
                self.enter(
                    Frame {
                        is_class: false,
                        locals,
                        key: def.name.to_string(),
                        described: format!("`{}`", def.name),
                    },
                    |walk| walk.body(&def.body),
                );
            }
            Stmt::ClassDef(def) => {
                for decorator in &def.decorator_list {
                    self.expr(&decorator.expression);
                }
                if let Some(arguments) = def.arguments.as_deref() {
                    for arg in &*arguments.args {
                        self.expr(arg);
                    }
                    for keyword in &*arguments.keywords {
                        self.expr(&keyword.value);
                    }
                }
                let mut locals = Bindings::new();
                collect(&def.body, &mut locals);
                self.enter(
                    Frame {
                        is_class: true,
                        locals,
                        key: def.name.to_string(),
                        described: format!("`{}`", def.name),
                    },
                    |walk| walk.body(&def.body),
                );
            }
            _ => {
                let mut nested: Vec<&[Stmt]> = Vec::new();
                push_blocks(stmt, &mut nested);
                for expr in statement_expressions(stmt) {
                    self.expr(expr);
                }
                for block in nested {
                    self.body(block);
                }
            }
        }
    }

    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Name(ast::ExprName { id, ctx, .. }) if ctx.is_load() => {
                if defines(id.as_str())
                    && let Some(frame) = self.resolves_to(id.as_str())
                {
                    let scope = frame.key.clone();
                    self.found.reads.push((id.to_string(), scope));
                }
            }
            Expr::Lambda(lambda) => {
                let mut locals = Bindings::new();
                if let Some(parameters) = lambda.parameters.as_deref() {
                    for parameter in parameters {
                        if let Some(default) = parameter.default() {
                            self.expr(default);
                        }
                        let bound = parameter.name();
                        if defines(bound.as_str()) {
                            record(&mut locals, bound.as_str(), bound.range());
                        }
                    }
                }
                self.enter(
                    Frame {
                        is_class: false,
                        locals,
                        key: "<lambda>".to_owned(),
                        described: "a lambda".to_owned(),
                    },
                    |walk| walk.expr(&lambda.body),
                );
                return;
            }
            Expr::ListComp(comp) => {
                self.comprehension(
                    "<listcomp>",
                    "a list comprehension",
                    &comp.generators,
                    |walk| {
                        walk.expr(&comp.elt);
                    },
                );
                return;
            }
            Expr::SetComp(comp) => {
                self.comprehension(
                    "<setcomp>",
                    "a set comprehension",
                    &comp.generators,
                    |walk| {
                        walk.expr(&comp.elt);
                    },
                );
                return;
            }
            Expr::Generator(comp) => {
                self.comprehension(
                    "<genexpr>",
                    "a generator expression",
                    &comp.generators,
                    |walk| {
                        walk.expr(&comp.elt);
                    },
                );
                return;
            }
            Expr::DictComp(comp) => {
                self.comprehension(
                    "<dictcomp>",
                    "a dict comprehension",
                    &comp.generators,
                    |walk| {
                        if let Some(key) = comp.key.as_deref() {
                            walk.expr(key);
                        }
                        walk.expr(&comp.value);
                    },
                );
                return;
            }
            // a basedpython statement expression holds a statement that runs where the
            // expression is written, and it may open a scope of its own (a `def` in a
            // `match` arm), so it goes back through the statement walk
            Expr::Statement(held) => {
                self.stmt(&held.stmt);
                return;
            }
            // nothing else opens a scope or reads a name of its own; the walk below reaches
            // the sub-expressions. spelled out for the reason [`bind`] is — a scope-opening
            // expression added to the syntax tree has to be decided about here
            Expr::Name(_)
            | Expr::BoolOp(_)
            | Expr::Named(_)
            | Expr::BinOp(_)
            | Expr::UnaryOp(_)
            | Expr::If(_)
            | Expr::Dict(_)
            | Expr::Set(_)
            | Expr::Await(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
            | Expr::Compare(_)
            | Expr::Call(_)
            | Expr::FString(_)
            | Expr::TString(_)
            | Expr::StringLiteral(_)
            | Expr::BytesLiteral(_)
            | Expr::NumberLiteral(_)
            | Expr::BooleanLiteral(_)
            | Expr::NoneLiteral(_)
            | Expr::EllipsisLiteral(_)
            | Expr::Attribute(_)
            | Expr::Subscript(_)
            | Expr::Starred(_)
            | Expr::List(_)
            | Expr::Tuple(_)
            | Expr::Slice(_)
            | Expr::IpyEscapeCommand(_)
            | Expr::CallableType(_)
            | Expr::ProtocolType(_)
            | Expr::ProtocolMethod(_) => {}
        }
        // the sub-expressions, without the visitor trait's statement recursion: a
        // nested `def` is reached through `stmt`, which knows to open a scope for it
        ruff_python_ast::visitor::source_order::walk_expr(
            &mut Children(self, std::marker::PhantomData),
            expr,
        );
    }

    /// a comprehension is a scope of its own, so a target of its that spells a helper name
    /// takes that name over for everything the comprehension evaluates — the element, the
    /// conditions, and every iterable but the first. that first iterable is evaluated where
    /// the comprehension is written, before the scope exists, so it is walked outside it
    fn comprehension(
        &mut self,
        key: &str,
        described: &str,
        generators: &[ast::Comprehension],
        elements: impl FnOnce(&mut Self),
    ) {
        let Some(first) = generators.first() else {
            elements(self);
            return;
        };
        self.expr(&first.iter);
        let mut locals = Bindings::new();
        for generator in generators {
            bind_target(&generator.target, &mut locals);
        }
        locals.retain(|bound, _| defines(bound));
        self.enter(
            Frame {
                is_class: false,
                locals,
                key: key.to_owned(),
                described: described.to_owned(),
            },
            |walk| {
                for (index, generator) in generators.iter().enumerate() {
                    if index > 0 {
                        walk.expr(&generator.iter);
                    }
                    for condition in &generator.ifs {
                        walk.expr(condition);
                    }
                }
                elements(walk);
            },
        );
    }
}

/// hands each of an expression's own sub-expressions back to [`Walk::expr`]
struct Children<'a, 'b>(&'a mut Walk, std::marker::PhantomData<&'b ()>);

impl<'b> SourceOrderVisitor<'b> for Children<'_, 'b> {
    fn visit_expr(&mut self, expr: &'b Expr) {
        self.0.expr(expr);
    }
}

/// the helper names `body`'s own statements bind, not descending into the scopes they open
///
/// a name the body declares `global` or `nonlocal` is bound somewhere else, so an assignment to
/// it is not a local binding and does not take the helper's name over — which is how
/// `_by_match_seq` in the runtime itself assigns `_by_match_seq_types`
fn collect(body: &[Stmt], locals: &mut Bindings) {
    for stmt in body {
        for (name, range) in bindings(stmt) {
            if defines(&name) {
                record(locals, name, range);
            }
        }
    }
    let mut declared = BTreeSet::new();
    declarations(body, &mut declared);
    for name in declared {
        locals.remove(&name);
    }
}

/// the names `body` declares `global` or `nonlocal`, through the blocks it opens
fn declarations(body: &[Stmt], out: &mut BTreeSet<String>) {
    for stmt in body {
        match stmt {
            Stmt::Global(node) => out.extend(node.names.iter().map(ToString::to_string)),
            Stmt::Nonlocal(node) => out.extend(node.names.iter().map(ToString::to_string)),
            _ => {
                let mut blocks = Vec::new();
                push_blocks(stmt, &mut blocks);
                for block in blocks {
                    declarations(block, out);
                }
            }
        }
    }
}

/// the suites a statement runs in its own scope
///
/// spelled out over every statement, for the reason [`bind`] is: this is the other half of
/// the same enumeration, and the two have to agree about which statements hold suites
fn push_blocks<'a>(stmt: &'a Stmt, out: &mut Vec<&'a [Stmt]>) {
    match stmt {
        Stmt::If(node) => {
            out.push(&node.body);
            for clause in &node.elif_else_clauses {
                out.push(&clause.body);
            }
        }
        Stmt::Try(node) => {
            out.push(&node.body);
            for ExceptHandler::ExceptHandler(handler) in &node.handlers {
                out.push(&handler.body);
            }
            out.push(&node.orelse);
            out.push(&node.finalbody);
        }
        Stmt::With(node) => out.push(&node.body),
        Stmt::For(node) => {
            out.push(&node.body);
            out.push(&node.orelse);
        }
        Stmt::While(node) => {
            out.push(&node.body);
            out.push(&node.orelse);
        }
        Stmt::Match(node) => {
            for case in &node.cases {
                out.push(&case.body);
            }
        }
        // basedpython `let <pattern> := <subject>`, whose `else` runs where the pattern
        // did not match
        Stmt::Let(node) => out.push(&node.orelse),
        // a `def` and a `class` hold a suite too, but it is a scope of its own rather than
        // one that runs where the statement is written — the callers open a frame for it
        Stmt::FunctionDef(_)
        | Stmt::ClassDef(_)
        | Stmt::Return(_)
        | Stmt::Delete(_)
        | Stmt::TypeAlias(_)
        | Stmt::Assign(_)
        | Stmt::AugAssign(_)
        | Stmt::AnnAssign(_)
        | Stmt::Raise(_)
        | Stmt::Assert(_)
        | Stmt::Import(_)
        | Stmt::ImportFrom(_)
        | Stmt::Global(_)
        | Stmt::Nonlocal(_)
        | Stmt::Expr(_)
        | Stmt::Pass(_)
        | Stmt::Break(_)
        | Stmt::Continue(_)
        | Stmt::IpyEscapeCommand(_) => {}
    }
}

/// the expressions a statement evaluates in the scope it is written in, leaving the
/// suites [`push_blocks`] hands back to be walked as statements
fn statement_expressions(stmt: &Stmt) -> Vec<&Expr> {
    struct Own<'a> {
        exprs: Vec<&'a Expr>,
    }
    impl<'a> SourceOrderVisitor<'a> for Own<'a> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            self.exprs.push(expr);
        }
        fn visit_stmt(&mut self, _: &'a Stmt) {}
    }
    let mut own = Own { exprs: Vec::new() };
    ruff_python_ast::visitor::source_order::walk_stmt(&mut own, stmt);
    own.exprs
}

/// every name a statement loads, at any depth
fn reads(stmt: &Stmt) -> BTreeSet<String> {
    struct Reads(BTreeSet<String>);
    impl SourceOrderVisitor<'_> for Reads {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Name(ast::ExprName { id, ctx, .. }) = expr
                && ctx.is_load()
            {
                self.0.insert(id.to_string());
            }
            walk_expr(self, expr);
        }
    }
    let mut visitor = Reads(BTreeSet::new());
    ruff_python_ast::visitor::source_order::walk_stmt(&mut visitor, stmt);
    visitor.0
}

/// the definitions `helpers` need, their own included, in the order the file
/// defines them
///
/// a name the file does not define is left out rather than reported here: the
/// module then calls something it never got, and phase 3 says so with a span
fn closure(helpers: impl IntoIterator<Item = Helper>) -> Vec<&'static Definition> {
    let index = index();
    let mut resolved: BTreeMap<usize, &'static Definition> = BTreeMap::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = helpers.into_iter().map(Helper::name).collect();
    while let Some(name) = queue.pop() {
        if !seen.insert(name) {
            continue;
        }
        let Some(definition) = index.get(name) else {
            continue;
        };
        // a statement bound under several names is still one definition
        resolved.insert(definition.order, definition);
        queue.extend(definition.needs.iter().map(String::as_str));
    }
    resolved.into_values().collect()
}

/// whether `name` is one of the runtime's definitions
pub(crate) fn defines(name: &str) -> bool {
    index().contains_key(name)
}

/// the definitions `helpers` need, as preamble entries for a module that has nowhere to
/// import them from, whose top-level statements are `module` and whose names `written`
/// holds
///
/// a definition pasted into a module reads the builtins through the module's globals,
/// so a builtin the module binds at its top level for itself is read under the name a
/// lowering reads it by ([`WrittenNames::builtin`]), which the caller imports from
/// `builtins` ([`WrittenNames::builtin_imports`]). a star import may bind any of them,
/// so it counts as binding each
///
/// [`WrittenNames::builtin`]: crate::transforms::repeated_underscore::WrittenNames::builtin
/// [`WrittenNames::builtin_imports`]: crate::transforms::repeated_underscore::WrittenNames::builtin_imports
pub(crate) fn inline(
    helpers: impl IntoIterator<Item = Helper>,
    module: &[Stmt],
    written: crate::transforms::repeated_underscore::WrittenNames,
) -> Vec<String> {
    let definitions = closure(helpers);
    let read: BTreeSet<&str> = definitions
        .iter()
        .flat_map(|definition| &definition.builtin_reads)
        .map(|(_, name)| name.as_str())
        .collect();
    let bound = if read.is_empty() {
        Bindings::new()
    } else {
        let mut bound = Bindings::new();
        for stmt in module {
            for (name, range) in module_bindings(stmt) {
                record(&mut bound, name, range);
            }
        }
        bound
    };
    let renamed: BTreeMap<&str, String> = read
        .into_iter()
        .filter(|name| bound.contains_key(*name) || bound.contains_key("*"))
        .map(|name| (name, written.builtin(name)))
        .filter(|(name, local)| name != local)
        .collect();

    definitions
        .into_iter()
        .map(|definition| {
            let mut source = definition.source.clone();
            let mut reads: Vec<&(TextRange, String)> = definition
                .builtin_reads
                .iter()
                .filter(|(_, name)| renamed.contains_key(name.as_str()))
                .collect();
            reads.sort_by_key(|(range, _)| std::cmp::Reverse(range.start()));
            for (range, name) in reads {
                source.replace_range(
                    std::ops::Range::<usize>::from(*range),
                    &renamed[name.as_str()],
                );
            }
            source
        })
        .collect()
}

/// the import a module uses to reach helpers written out beside it
///
/// it names only what the emitted code calls: a helper that exists because
/// another helper calls it is resolved inside `_by_runtime`. absolute rather than
/// relative, because a relative import fails in a module run as `__main__`
pub(crate) fn import_line(module: &str, helpers: impl IntoIterator<Item = Helper>) -> String {
    let names: BTreeSet<&str> = helpers.into_iter().map(Helper::name).collect();
    format!(
        "from {module} import {}",
        names.into_iter().collect::<Vec<_>>().join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_parses() {
        if let Err(error) = parse_module(SOURCE) {
            panic!("{FILE_NAME} does not parse: {error}");
        }
    }

    /// a constant naming something the file does not define would reach the
    /// output as a call to nothing
    #[test]
    fn every_helper_is_defined() {
        for helper in ALL {
            assert!(
                defines(helper.name()),
                "`{}` is not defined in {FILE_NAME}",
                helper.name()
            );
        }
    }

    /// two statements binding one name would leave it to file order which one a
    /// module gets
    #[test]
    fn every_name_has_one_definition() {
        let parsed = parse_module(SOURCE).expect("the runtime parses");
        let mut seen = BTreeSet::new();
        for stmt in parsed.suite() {
            for name in bindings(stmt).into_keys() {
                assert!(seen.insert(name.clone()), "`{name}` is bound twice");
            }
        }
    }

    /// the preamble is one entry per line, so an entry that did not end its own
    /// line would run into the next
    #[test]
    fn definitions_end_in_a_newline() {
        for (name, definition) in index() {
            assert!(definition.source.ends_with('\n'), "`{name}` does not");
        }
    }

    #[test]
    fn dependencies_resolve() {
        for (name, definition) in index() {
            for need in &definition.needs {
                assert!(defines(need), "`{name}` reads unknown `{need}`");
            }
        }
    }

    /// the discard adapter is spelled by the checker as well as emitted here
    #[test]
    fn the_discard_adapter_is_the_one_ty_resolves() {
        assert_eq!(DISCARD.name(), ty_python_semantic::DISCARD_ADAPTER);
    }

    /// the proxy's operator forwarding is read by nothing; it has to travel with
    /// the class it patches, or the proxy is left with no operators at all
    #[test]
    fn set_up_code_travels_with_its_definition() {
        let pasted = inline(
            [LAZY_ATTR],
            &[],
            crate::transforms::repeated_underscore::WrittenNames::new(""),
        )
        .concat();
        assert!(pasted.contains("class _LazyAttr:"), "{pasted}");
        assert!(
            pasted.contains("_by_forward_operators(_LazyAttr)"),
            "{pasted}"
        );
        assert!(pasted.contains("def _lazy_module("), "{pasted}");
    }

    /// the proxy's three overrides of `object`'s own members are reported by
    /// `missing-override-decorator`, and the emitted python is read by this project's own
    /// checker. `@override` is `typing.override`, which arrived in 3.12, and the polyfill
    /// runs on 3.9 — so each carries the suppression the decorator would have been
    #[test]
    fn the_proxies_overrides_do_not_report_in_the_emitted_python() {
        let pasted = inline(
            [LAZY_ATTR],
            &[],
            crate::transforms::repeated_underscore::WrittenNames::new(""),
        )
        .concat();
        for member in ["__class__", "__setattr__", "__delattr__"] {
            let line = pasted
                .lines()
                .find(|line| line.trim_start().starts_with(&format!("def {member}(")))
                .unwrap_or_else(|| panic!("`{member}` is no longer defined:\n{pasted}"));
            assert!(
                line.contains("# ty: ignore[missing-override-decorator]"),
                "`{member}` lost its suppression: {line}"
            );
        }
    }

    /// the reads a pasted definition renames are the names python itself looks up in the
    /// module's globals: its compiler's symbol table answers the same question, so the two
    /// are held to agree over the whole runtime. with no python to ask, there is nothing
    /// to compare against
    #[test]
    fn the_global_reads_are_the_ones_python_finds() {
        const SYMTABLE: &str = r#"
import symtable, sys
table = symtable.symtable(sys.stdin.read(), "_by_runtime.py", "exec")
defined = {s.get_name() for s in table.get_symbols() if s.is_assigned() or s.is_imported() or s.is_namespace()}
found = set()
def walk(scope):
    for symbol in scope.get_symbols():
        global_read = symbol.is_global() and not symbol.is_declared_global() if scope.get_type() != "module" else True
        if global_read and symbol.is_referenced() and symbol.get_name() not in defined:
            found.add(symbol.get_name())
    for child in scope.get_children():
        walk(child)
walk(table)
print(" ".join(sorted(found)))
"#;
        let Ok(mut child) = std::process::Command::new("python3")
            .args(["-c", SYMTABLE])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
        else {
            return;
        };
        if let Some(mut stdin) = child.stdin.take() {
            std::io::Write::write_all(&mut stdin, SOURCE.as_bytes()).expect("write the runtime");
        }
        let output = child.wait_with_output().expect("run python");
        assert!(output.status.success(), "{output:?}");
        let python: BTreeSet<String> = String::from_utf8_lossy(&output.stdout)
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let ours: BTreeSet<String> = index()
            .values()
            .flat_map(|definition| &definition.builtin_reads)
            .map(|(_, name)| name.clone())
            .collect();
        assert_eq!(ours, python);
    }

    /// a module that binds a builtin at its top level for itself has the pasted definitions
    /// read that builtin under the name a lowering would, and a module that binds it only
    /// in a function of its own leaves them reading it as written
    #[test]
    fn a_pasted_definition_reads_a_builtin_the_module_binds_under_its_own_name() {
        let module = "def getattr(o, n): return 42\ndef f(format): return format\n";
        let parsed = ruff_python_parser::parse_unchecked_source(
            module,
            ruff_python_ast::PySourceType::Python,
        );
        let code = crate::transforms::repeated_underscore::CodeNames::of(parsed.suite());
        let written =
            crate::transforms::repeated_underscore::WrittenNames::new(module).with_code(&code);
        let pasted = inline([LAZY_ATTR], parsed.suite(), written).concat();
        assert!(
            pasted.contains("v = getattr2(m, self._by_attr)") && !pasted.contains("getattr(m,"),
            "{pasted}"
        );
        assert!(
            pasted.contains("lambda s, f: format(s._by_resolve(), f)"),
            "{pasted}"
        );
    }

    /// a function or class that declares a builtin's name `global` and binds it rebinds the
    /// module's, at any depth, which the pasted definitions would then read
    #[test]
    fn a_pasted_definition_reads_a_builtin_a_global_declaration_rebinds_under_its_own_name() {
        let module = concat!(
            "def outer():\n",
            "    class C:\n",
            "        def rebind(self):\n",
            "            global getattr\n",
            "            for getattr in [len]:\n",
            "                pass\n",
            "def local():\n",
            "    format = 1\n",
        );
        let parsed = ruff_python_parser::parse_unchecked_source(
            module,
            ruff_python_ast::PySourceType::Python,
        );
        let code = crate::transforms::repeated_underscore::CodeNames::of(parsed.suite());
        let written =
            crate::transforms::repeated_underscore::WrittenNames::new(module).with_code(&code);
        let pasted = inline([LAZY_ATTR], parsed.suite(), written).concat();
        assert!(
            pasted.contains("v = getattr2(m, self._by_attr)") && !pasted.contains("getattr(m,"),
            "{pasted}"
        );
        // a local binding leaves the module's alone
        assert!(
            pasted.contains("lambda s, f: format(s._by_resolve(), f)"),
            "{pasted}"
        );
    }
}
