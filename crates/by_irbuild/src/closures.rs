//! nested functions, as methods of a generated environment class
//!
//! a closure needs somewhere for the captured values to live that outlives the
//! frame that made it. that place is an object with a fixed layout and one field
//! per capture — which is exactly a [`ClassIr`], so the whole native-class
//! machinery applies: a captured read is a `GetField` at a compile-time offset, the
//! nested function is a method whose receiver is the environment, and binding the
//! name is `PyCFunction_NewEx` with the environment as `self`.
//!
//! ## what is captured
//!
//! python closes over the *variable*, not the value: a write after the `def` is
//! visible through the closure, and every closure a loop makes shares one cell.
//!
//! so a captured name that either frame writes does not live in a register at all.
//! it lives in the environment field for the whole of the enclosing function too —
//! the same cell both frames read and write. that is what [`Captured`] with
//! `shared` means, and it is why the enclosing function's own reads of the name go
//! through [`crate::Place::Field`] rather than a register.
//!
//! a capture nobody writes is still copied in where the `def` runs, because a value
//! that cannot change is the same either way and a register is faster.
//!
//! ## nesting, to any depth
//!
//! a name more than one frame up cannot be copied down: a *shared cell* copied is two
//! cells, and the two frames stop agreeing. so each environment holds the one that
//! encloses it in a [`OUTER_FIELD`] field, and a read walks the chain — see
//! [`crate::Place::Chained`]. a frame gets a field of its own only for a name it
//! *owns*; everything else is reached through the chain, so there is exactly one home
//! for every name.

use std::collections::{HashMap, HashSet};

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};

use crate::mapper::{Decline, Lowered};

/// the field a nested environment holds its *enclosing* environment in
///
/// this is what makes a name more than one frame up reachable: the chain is walked
/// rather than copied, so a *shared cell* two frames up stays one cell
pub(crate) const OUTER_FIELD: &str = "$outer";

/// a nested function, and the enclosing names it reads
///
/// a lambda arrives here as a *synthesized* definition rather than through a second
/// code path. cloning the ast preserves each node's identity, so the semantic model
/// still answers for the body and the parameters — and everything downstream sees an
/// ordinary nested function
pub(crate) struct Nested {
    pub(crate) def: ast::StmtFunctionDef,
    /// the lambda this was synthesized from, by source range
    pub(crate) lambda: Option<ruff_text_size::TextRange>,
    /// the enclosing names the body reads, in a stable order
    pub(crate) captures: Vec<String>,
    /// the captures that either frame *writes*, so both must see one cell
    pub(crate) shared: Vec<String>,
}

/// the nested functions of a body, with their captures
///
/// `bound` is what the enclosing function binds — its parameters and locals. a
/// name the nested function reads that is not its own and *is* bound out here is a
/// capture
pub(crate) fn nested_functions(
    body: &[Stmt],
    bound: &HashSet<String>,
    never_written: &HashSet<String>,
    per_iteration: &HashSet<String>,
) -> Lowered<Vec<Nested>> {
    let mut out = Vec::new();
    // anywhere in the body, not only at the top level: a `def` inside a loop is the
    // case the shared-cell rule exists for
    let mut definitions: Vec<(ast::StmtFunctionDef, Option<ruff_text_size::TextRange>)> =
        Vec::new();
    for stmt in crate::walk(body) {
        if let Stmt::FunctionDef(def) = stmt {
            definitions.push((def.clone(), None));
        }
        // a lambda in any expression position is a nested function with a generated
        // name, so the whole closure machinery applies to it unchanged
        for expr in statement_expressions(stmt) {
            visit_expressions(expr, &mut |child| {
                if let Expr::Lambda(lambda) = child {
                    let name = format!("$lambda{}", definitions.len());
                    definitions.push((synthesize(lambda, &name), Some(lambda.range)));
                }
            });
        }
    }

    // two `def`s of one name in a scope — the `try` / `except` pair is the common
    // shape — bind whichever one *ran*, so a direct call cannot know which function it
    // is calling. they would also mangle to one C symbol, which is how this was found
    let mut seen: HashSet<&str> = HashSet::new();
    for (def, _) in &definitions {
        if !seen.insert(def.name.as_str()) {
            return Err(Decline::new(format!(
                "`{}` is defined more than once in this scope, so a call to it has no \
                 single target",
                def.name
            )));
        }
    }

    for (def, lambda) in definitions {
        let def = &def;
        let own = own_names(def);
        let mut captures: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for name in read_names(&def.body) {
            if own.contains(name) || !bound.contains(name) || !seen.insert(name) {
                continue;
            }
            captures.push(name.to_string());
        }
        // a name either frame writes is a shared cell, so the enclosing function
        // reads and writes the field too rather than a register of its own.
        //
        // a *loop binding* is the exception, and the reason is the language rather
        // than the lowering: basedpython gives each iteration its own binding, so
        // the closure holds the value it was made with instead of sharing one cell
        let mut shared: Vec<String> = captures
            .iter()
            .filter(|name| !never_written.contains(name.as_str()))
            .filter(|name| !per_iteration.contains(name.as_str()))
            .cloned()
            .collect();
        // and a nested `nonlocal` write makes it shared even if it is never read
        for name in nonlocal_names(def) {
            if bound.contains(name) {
                if !captures.iter().any(|capture| capture == name) {
                    captures.push(name.to_string());
                }
                if !shared.iter().any(|entry| entry == name) {
                    shared.push(name.to_string());
                }
            }
        }
        out.push(Nested {
            def: def.clone(),
            lambda,
            captures,
            shared,
        });
    }
    Ok(out)
}

/// the nested functions in `body` that nothing binds except their own `def`
///
/// a frame calls a closure it made itself at that closure's native entry point, which
/// skips reading the name entirely. that is only the same thing while the name still
/// holds what the `def` put there, so
///
/// ```python
/// def outer(n):
///     def step(x):
///         return x + 1
///     step = twice(step)
///     return step(n)
/// ```
///
/// has to read the name: it holds the wrapper by the time it is called. which of the
/// two ran first is a runtime question in a loop, so the answer here is static.
///
/// deliberately over-reporting. a binding of the name anywhere at all disqualifies it,
/// including one inside a *different* nested function that happens to reuse the name
/// for a local of its own. that costs a direct call and never an answer
pub(crate) fn bound_only_by_their_def(body: &[Stmt]) -> HashSet<String> {
    struct Scan<'a> {
        /// how many times each name is bound, the `def` included
        counts: HashMap<&'a str, usize>,
    }
    impl<'a> Scan<'a> {
        fn bind(&mut self, name: &'a str) {
            *self.counts.entry(name).or_default() += 1;
        }
    }
    impl<'a> Visitor<'a> for Scan<'a> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::FunctionDef(node) => self.bind(node.name.as_str()),
                Stmt::ClassDef(node) => self.bind(node.name.as_str()),
                Stmt::TypeAlias(node) => {
                    if let Expr::Name(name) = node.name.as_ref() {
                        self.bind(name.id.as_str());
                    }
                }
                // a `global` or `nonlocal` declaration moves the name's home somewhere
                // this frame's environment is not, which the direct call assumes it is
                Stmt::Global(node) => {
                    for name in &node.names {
                        self.bind(name.as_str());
                    }
                }
                Stmt::Nonlocal(node) => {
                    for name in &node.names {
                        self.bind(name.as_str());
                    }
                }
                _ => {}
            }
            visitor::walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            // every binding that reaches a plain name goes through a store context —
            // an assignment, an augmented one, a `for` target, a `with ... as`, a
            // walrus, a comprehension target, an element of an unpacking. `del` is
            // here too: it unbinds, which is just as much a change
            if let Expr::Name(name) = expr
                && matches!(name.ctx, ast::ExprContext::Store | ast::ExprContext::Del)
            {
                self.bind(name.id.as_str());
            }
            visitor::walk_expr(self, expr);
        }

        fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
            let ast::ExceptHandler::ExceptHandler(node) = handler;
            if let Some(bound) = &node.name {
                self.bind(bound.as_str());
            }
            visitor::walk_except_handler(self, handler);
        }

        fn visit_alias(&mut self, alias: &'a ast::Alias) {
            let bound = alias.asname.as_ref().unwrap_or(&alias.name);
            self.bind(bound.as_str());
        }

        fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
            match pattern {
                ast::Pattern::MatchAs(node) => {
                    if let Some(bound) = &node.name {
                        self.bind(bound.as_str());
                    }
                }
                ast::Pattern::MatchStar(node) => {
                    if let Some(bound) = &node.name {
                        self.bind(bound.as_str());
                    }
                }
                ast::Pattern::MatchMapping(node) => {
                    if let Some(bound) = &node.rest {
                        self.bind(bound.as_str());
                    }
                }
                _ => {}
            }
            visitor::walk_pattern(self, pattern);
        }
    }

    let mut scan = Scan {
        counts: HashMap::new(),
    };
    scan.visit_body(body);
    let counts = scan.counts;
    // this frame's own `def`s: [`crate::walk`] stops at a nested body, and a function
    // two deep is bound in a frame that is not this one
    crate::walk(body)
        .into_iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(def) => Some(def.name.as_str()),
            _ => None,
        })
        .filter(|name| counts.get(name).copied() == Some(1))
        .map(str::to_string)
        .collect()
}

/// a `StmtFunctionDef` equivalent to a lambda: its parameters, and its body as a
/// single `return`
fn synthesize(lambda: &ast::ExprLambda, name: &str) -> ast::StmtFunctionDef {
    ast::StmtFunctionDef {
        node_index: ruff_python_ast::AtomicNodeIndex::NONE,
        range: lambda.range,
        is_async: false,
        decorator_list: thin_vec::ThinVec::new(),
        name: ast::Identifier::new(name, lambda.range),
        type_params: None,
        parameters: lambda
            .parameters
            .clone()
            .unwrap_or_else(|| Box::new(ast::Parameters::default())),
        returns: None,
        raises: None,
        is_asserts_return: false,
        body: thin_vec::thin_vec![Stmt::Return(ast::StmtReturn {
            node_index: ruff_python_ast::AtomicNodeIndex::NONE,
            range: lambda.range,
            value: Some(lambda.body.clone()),
        })],
        is_trailing_lambda: false,
    }
}

/// the names a function binds itself: its parameters, and anything it assigns
fn own_names(def: &ast::StmtFunctionDef) -> HashSet<&str> {
    let mut out: HashSet<&str> = def
        .parameters
        .iter_non_variadic_params()
        .map(|parameter| parameter.parameter.name.as_str())
        .collect();
    if let Some(vararg) = &def.parameters.vararg {
        out.insert(vararg.name.as_str());
    }
    if let Some(kwarg) = &def.parameters.kwarg {
        out.insert(kwarg.name.as_str());
    }
    out.extend(written_names(&def.body));
    // a name declared `global` here resolves in the module namespace whether this body
    // writes it or only reads it, so an enclosing local of the same name must never be
    // captured for it. counting it as this function's own is what says so
    out.extend(global_names(def));
    // a `nonlocal` name is explicitly *not* the nested function's own
    for name in nonlocal_names(def) {
        out.remove(name);
    }
    out
}

/// the names a function declares `nonlocal`
fn nonlocal_names(def: &ast::StmtFunctionDef) -> Vec<&str> {
    crate::walk(&def.body)
        .into_iter()
        .filter_map(|stmt| match stmt {
            Stmt::Nonlocal(node) => {
                Some(node.names.iter().map(ruff_python_ast::Identifier::as_str))
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// the names a function declares `global`
fn global_names(def: &ast::StmtFunctionDef) -> Vec<&str> {
    crate::walk(&def.body)
        .into_iter()
        .filter_map(|stmt| match stmt {
            Stmt::Global(node) => Some(node.names.iter().map(ruff_python_ast::Identifier::as_str)),
            _ => None,
        })
        .flatten()
        .collect()
}

/// every name a body assigns to
pub(crate) fn written_names(body: &[Stmt]) -> Vec<&str> {
    let mut out = Vec::new();
    for stmt in crate::walk(body) {
        match stmt {
            Stmt::Assign(node) => {
                for target in &node.targets {
                    if let Expr::Name(name) = target {
                        out.push(name.id.as_str());
                    }
                }
            }
            Stmt::AnnAssign(node) => {
                if let Expr::Name(name) = node.target.as_ref() {
                    out.push(name.id.as_str());
                }
            }
            Stmt::AugAssign(node) => {
                if let Expr::Name(name) = node.target.as_ref() {
                    out.push(name.id.as_str());
                }
            }
            Stmt::For(node) => {
                if let Expr::Name(name) = node.target.as_ref() {
                    out.push(name.id.as_str());
                }
            }
            Stmt::FunctionDef(node) => out.push(node.name.as_str()),
            _ => {}
        }
    }
    out
}

/// every name a body reads, including in nested expressions *and nested functions*
///
/// a name a function nested inside me reads is a name **I** have to capture — that is
/// what a closure chain is. leaving it out made a function two levels deep resolve the
/// outermost frame's names as globals
fn read_names(body: &[Stmt]) -> Vec<&str> {
    let mut out = Vec::new();
    for stmt in crate::walk(body) {
        for expr in statement_expressions(stmt)
            .into_iter()
            .chain(target_reads(stmt))
        {
            collect_reads(expr, &mut out);
        }
        // and a lambda's body, which is an expression the walk above does reach — but a
        // nested `def`'s body is a statement list the walk deliberately stops at
        if let Stmt::FunctionDef(nested) = stmt {
            // a decorator belongs to the frame the `def` stands in, not to the function
            // it decorates: it is evaluated there, so the names in it are read here and
            // filtered by nothing the nested function binds
            for decorator in &nested.decorator_list {
                collect_reads(&decorator.expression, &mut out);
            }
            let own = own_names(nested);
            out.extend(
                read_names(&nested.body)
                    .into_iter()
                    .filter(|name| !own.contains(name)),
            );
        }
    }
    out
}

/// the expressions a statement evaluates, excluding assignment *targets*
pub(crate) fn statement_expressions(stmt: &Stmt) -> Vec<&Expr> {
    match stmt {
        Stmt::Return(node) => node.value.iter().map(AsRef::as_ref).collect(),
        Stmt::Expr(node) => vec![node.value.as_ref()],
        Stmt::Assign(node) => vec![node.value.as_ref()],
        Stmt::AnnAssign(node) => node.value.iter().map(AsRef::as_ref).collect(),
        Stmt::AugAssign(node) => vec![node.target.as_ref(), node.value.as_ref()],
        Stmt::If(node) => vec![node.test.as_ref()],
        Stmt::While(node) => vec![node.test.as_ref()],
        Stmt::For(node) => vec![node.iter.as_ref()],
        Stmt::Raise(node) => node
            .exc
            .iter()
            .chain(node.cause.iter())
            .map(AsRef::as_ref)
            .collect(),
        Stmt::Assert(node) => {
            let mut all = vec![node.test.as_ref()];
            all.extend(node.msg.iter().map(AsRef::as_ref));
            all
        }
        Stmt::With(node) => node.items.iter().map(|item| &item.context_expr).collect(),
        _ => Vec::new(),
    }
}

/// the expressions a statement's assignment *targets* evaluate
///
/// binding `x` is not a read of `x`, but binding `x.a` or `x[i]` is: python works out
/// which object to store into, and which key, before it stores anything. that is the
/// half of a target [`statement_expressions`] leaves out, and leaving it out of the
/// capture list is what made
///
/// ```python
/// class C:
///     def __init__(self):
///         def go():
///             self.a = 2
///         go()
/// ```
///
/// resolve `self` as a global and raise `NameError`: the nested function's only
/// mention of `self` is inside a target, so nothing recorded that it reads the frame
/// around it at all
fn target_reads(stmt: &Stmt) -> Vec<&Expr> {
    let mut out = Vec::new();
    let targets: Vec<&Expr> = match stmt {
        Stmt::Assign(node) => node.targets.iter().collect(),
        Stmt::AnnAssign(node) => vec![node.target.as_ref()],
        Stmt::For(node) => vec![node.target.as_ref()],
        Stmt::With(node) => node
            .items
            .iter()
            .filter_map(|item| item.optional_vars.as_deref())
            .collect(),
        Stmt::Delete(node) => node.targets.iter().collect(),
        // an augmented target is read as well as written, and
        // [`statement_expressions`] already reports the whole of it
        _ => Vec::new(),
    };
    for target in targets {
        collect_target_reads(target, &mut out);
    }
    out
}

/// the sub-expressions of one assignment target that are evaluated for their value
fn collect_target_reads<'a>(target: &'a Expr, out: &mut Vec<&'a Expr>) {
    match target {
        Expr::Attribute(node) => out.push(node.value.as_ref()),
        Expr::Subscript(node) => {
            out.push(node.value.as_ref());
            out.push(node.slice.as_ref());
        }
        Expr::Starred(node) => collect_target_reads(node.value.as_ref(), out),
        Expr::Tuple(node) => {
            for element in &node.elts {
                collect_target_reads(element, out);
            }
        }
        Expr::List(node) => {
            for element in &node.elts {
                collect_target_reads(element, out);
            }
        }
        // a plain name is bound rather than read, and anything else is not a target
        _ => {}
    }
}

/// every name an expression reads
fn collect_reads<'a>(expr: &'a Expr, out: &mut Vec<&'a str>) {
    visit_expressions(expr, &mut |child| {
        if let Expr::Name(name) = child {
            out.push(name.id.as_str());
        }
    });
}

/// call `f` on `expr` and every expression inside it
pub(crate) fn visit_expressions<'a>(expr: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    struct Walk<'a, 'f, F> {
        f: &'f mut F,
        _marker: std::marker::PhantomData<&'a ()>,
    }
    impl<'a, F: FnMut(&'a Expr)> Visitor<'a> for Walk<'a, '_, F> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            (self.f)(expr);
            visitor::walk_expr(self, expr);
        }
    }
    Walk {
        f,
        _marker: std::marker::PhantomData,
    }
    .visit_expr(expr);
}

/// every name a `for` or a comprehension in `body` binds
///
/// deliberately the whole body rather than the loops enclosing each `def`: a
/// closure that captures one of these is either per-iteration or declined, and
/// over-reporting only ever costs a decline
pub(crate) fn loop_targets(body: &[Stmt]) -> HashSet<String> {
    let mut out = HashSet::new();
    let mut record = |target: &Expr| {
        visit_expressions(target, &mut |child| {
            if let Expr::Name(name) = child {
                out.insert(name.id.to_string());
            }
        });
    };
    for stmt in crate::walk(body) {
        if let Stmt::For(node) = stmt {
            record(&node.target);
        }
        for expr in statement_expressions(stmt) {
            visit_expressions(expr, &mut |child| {
                let generators = match child {
                    Expr::ListComp(node) => &node.generators,
                    Expr::SetComp(node) => &node.generators,
                    Expr::DictComp(node) => &node.generators,
                    Expr::Generator(node) => &node.generators,
                    _ => return,
                };
                for generator in generators {
                    record(&generator.target);
                }
            });
        }
    }
    out
}

/// the environment class's name
///
/// qualified by the enclosing chain, not just the function's own name: two different
/// outer functions may each nest a `middle`, and one C struct cannot be both
pub(crate) fn environment_name(enclosing: Option<&str>, owner: &str) -> String {
    match enclosing {
        Some(enclosing) => format!("{enclosing}${owner}$env"),
        None => format!("{owner}$env"),
    }
}

/// the environment class for a function's nested functions, if it has any
///
/// one field per distinct capture, and one method per nested function. the method
/// bodies are lowered by the caller, which is what knows how to lower a body
///
/// `owned` is the names *this* frame has of its own — a parameter or a local. anything
/// else a nested function reads lives further up, and is reached through the chain
/// rather than copied: giving it a field of its own here would shadow the real one
/// with a copy that is never seeded
pub(crate) fn environment(
    name: &str,
    enclosing: Option<&str>,
    nested: &[Nested],
    representation: &impl Fn(&str) -> Option<by_ir::rtype::RType>,
    owned: &HashSet<String>,
) -> Lowered<Option<Environment>> {
    if nested.is_empty() {
        return Ok(None);
    }
    let shared: HashSet<&str> = nested
        .iter()
        .flat_map(|entry| entry.shared.iter().map(String::as_str))
        .collect();
    let mut fields: Vec<by_ir::function::FieldDecl> = Vec::new();
    for capture in nested.iter().flat_map(|entry| entry.captures.iter()) {
        if fields.iter().any(|field| field.name == *capture) || !owned.contains(capture) {
            continue;
        }
        // a *shared* cell is always `object`: it starts unset, and NULL has to be
        // distinguishable from every value it could hold. an unboxed zero would not be
        let ty = if shared.contains(capture.as_str()) {
            by_ir::rtype::RType::OBJECT
        } else {
            representation(capture).ok_or_else(|| {
                Decline::new(format!("`{capture}` has no representation to capture"))
            })?
        };
        fields.push(by_ir::function::FieldDecl {
            name: capture.clone(),
            ty,
            default: None,
            optional: false,
            defaulted_by: None,
        });
    }
    // a nested environment holds its enclosing one, so a name further up is a chained
    // read rather than a copy — which is the only way a *shared cell* up there stays
    // one cell
    if let Some(enclosing) = enclosing {
        fields.push(by_ir::function::FieldDecl {
            optional: false,
            defaulted_by: None,
            name: OUTER_FIELD.to_string(),
            ty: by_ir::rtype::RType::Instance {
                class: enclosing.to_string(),
                exact: false,
            },
            default: None,
        });
    }
    // a class with no fields has no layout, and an environment capturing nothing
    // still needs an object to hang the method on
    if fields.is_empty() {
        fields.push(by_ir::function::FieldDecl {
            name: "$empty".to_string(),
            ty: by_ir::rtype::RType::NONE,
            default: None,
            optional: false,
            defaulted_by: None,
        });
    }
    Ok(Some(Environment {
        name: name.to_string(),
        fields,
    }))
}

/// a generated closure environment
pub(crate) struct Environment {
    pub(crate) name: String,
    pub(crate) fields: Vec<by_ir::function::FieldDecl>,
}
