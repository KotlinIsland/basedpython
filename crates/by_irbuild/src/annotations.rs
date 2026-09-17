//! the python that evaluates a nested function's annotations
//!
//! python evaluates a `def`'s annotations over the frames it is written in: on 3.13 where
//! the `def` stands, and from 3.14 when they are first asked for, through an
//! `__annotate__` function that closes over those frames' cells. a compiled nested
//! function has neither the interpreted frames nor, for a `.by` module, annotations that
//! run as python at all — the source may write them in syntax only basedpython has. what
//! does run is the interpreted twin, so the annotations are taken from its text.
//!
//! each annotated nested function is given a *factory*: python source defining
//!
//! ```python
//! def _by_annotations(values, type_params):
//!     kind = values[0]
//!     def inner(y: kind, z: int, /) -> kind:
//!         pass
//!     return inner
//! ```
//!
//! which the runtime compiles against the module's namespace and calls with the values the
//! enclosing names hold — listed by a reader nested beside the function, see
//! `closures::ANNOTATION_READER`. the definition it hands back is python's own, so its
//! `__annotations__` are evaluated as python evaluates them, and on 3.14 its
//! `__annotate__` is a real annotation function over real cells, which is what
//! `annotationlib`'s formats need. the stub keeps the definition's parameters and type
//! parameters and drops everything else: a default and a decorator are evaluated where the
//! `def` stands, and neither is anything the annotations read.
//!
//! a name the factory is given must be exactly a name python would close over, or an
//! annotation reads a global python would not have — so the twin's enclosing frames are
//! read for what they bind, and an annotation that reads one of those the compiled frames
//! do not list is refused. so is a definition the twin does not hold exactly once under its
//! qualified name. a refused factory raises `RuntimeError` when the annotations are asked
//! for, naming why, rather than answering with something python would not

use std::collections::HashSet;
use std::fmt::Write;

use by_ir::function::{Function, ModuleIr, NestedAnnotations};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_python_codegen::{Generator, Indentation};
use ruff_source_file::LineEnding;

/// the name each factory is bound to where the runtime compiles it
pub const FACTORY: &str = "_by_annotations";

/// write the factory of every annotated nested function in `module`, from its twin
pub fn write_factories(module: &mut ModuleIr, twin: &str) {
    let mut wanting: Vec<&mut Function> = module
        .functions
        .iter_mut()
        .chain(
            module
                .classes
                .iter_mut()
                .flat_map(|class| class.methods.iter_mut()),
        )
        .filter(|function| {
            function
                .nested
                .as_ref()
                .is_some_and(|nested| nested.annotations.is_some())
        })
        .collect();
    if wanting.is_empty() {
        return;
    }
    let parsed = ruff_python_parser::parse_module(twin);
    for function in &mut wanting {
        let Some(nested) = function.nested.as_mut() else {
            continue;
        };
        let Some(annotations) = nested.annotations.as_mut() else {
            continue;
        };
        let factory = match &parsed {
            Ok(parsed) => factory(parsed.suite(), &nested.qualname, annotations),
            Err(error) => Err(format!(
                "the interpreted definition does not parse: {error}"
            )),
        };
        annotations.refused = factory.is_err();
        annotations.factory = Some(match factory {
            Ok((source, reads)) => {
                // with nothing read from the enclosing frames — a module compiling its
                // annotations as strings, say — there are no values to keep for them
                if !reads {
                    annotations.reader = None;
                }
                source
            }
            Err(reason) => refusal(&nested.qualname, &reason),
        });
    }
}

/// the factory for the definition `qualname` names and whether it reads any of the values
/// it is handed, or why there cannot be one
fn factory(
    module: &[Stmt],
    qualname: &str,
    annotations: &NestedAnnotations,
) -> Result<(String, bool), String> {
    let (def, enclosing) = locate(module, qualname)?;
    let bound: HashSet<&str> = enclosing
        .iter()
        .filter_map(|scope| match scope {
            Scope::Function(def) => Some(*def),
            Scope::Class => None,
        })
        .flat_map(bound_in)
        .collect();
    let in_a_class = enclosing.iter().any(|scope| matches!(scope, Scope::Class));
    let declared: HashSet<&str> = def
        .type_params
        .iter()
        .flat_map(|params| params.iter())
        .map(|param| param.name().as_str())
        .collect();

    // what python evaluates: every annotation, unless the module has them all compiled as
    // strings, and the bounds, constraints and defaults of the type parameters either way
    let mut evaluated: Vec<&Expr> = Vec::new();
    if !future_annotations(module) {
        evaluated.extend(
            def.parameters
                .iter()
                .filter_map(ast::AnyParameterRef::annotation)
                .chain(def.returns.as_deref()),
        );
    }
    if let Some(type_params) = &def.type_params {
        struct Collect<'a, 'e> {
            evaluated: &'e mut Vec<&'a Expr>,
        }
        impl<'a> Visitor<'a> for Collect<'a, '_> {
            fn visit_expr(&mut self, expr: &'a Expr) {
                self.evaluated.push(expr);
            }
        }
        visitor::walk_type_params(
            &mut Collect {
                evaluated: &mut evaluated,
            },
            type_params,
        );
    }
    let mut read = Vec::new();
    let mut private = false;
    for expr in evaluated {
        visit(expr, &mut |expr| match expr {
            Expr::Name(name) => {
                private |= is_private(name.id.as_str());
                read.push(name.id.as_str());
            }
            Expr::Attribute(attribute) => private |= is_private(attribute.attr.as_str()),
            _ => {}
        });
    }
    // python mangles a private name written anywhere inside a class body, annotations of a
    // nested function included, and a factory outside the class would read it unmangled
    if in_a_class && private {
        return Err("an annotation names a private name inside a class".to_string());
    }

    let mut closed: Vec<(&str, usize)> = Vec::new();
    for name in read {
        if declared.contains(name) || !bound.contains(name) {
            continue;
        }
        if closed.iter().any(|(seen, _)| *seen == name) {
            continue;
        }
        let Some(index) = annotations.names.iter().position(|listed| listed == name) else {
            return Err(format!(
                "an annotation reads `{name}` from an enclosing frame, which the compiled frames do not hold for it"
            ));
        };
        closed.push((name, index));
    }

    let mut stub = def.clone();
    stub.decorator_list.clear();
    stub.is_async = false;
    stub.body = thin_vec::thin_vec![Stmt::Pass(ast::StmtPass {
        node_index: ruff_python_ast::AtomicNodeIndex::NONE,
        range: def.range,
    })];
    let parameters = &mut *stub.parameters;
    for parameter in parameters
        .posonlyargs
        .iter_mut()
        .chain(parameters.args.iter_mut())
        .chain(parameters.kwonlyargs.iter_mut())
    {
        parameter.default = None;
    }
    let render = |stub: ast::StmtFunctionDef| -> Result<String, String> {
        let indentation = Indentation::default();
        let rendered = Generator::new(&indentation, LineEnding::Lf).stmt(&Stmt::FunctionDef(stub));
        // the signature on one line and `pass` on the next. anything else is a string
        // spanning lines, which indenting would change
        if rendered.lines().count() == 2 {
            Ok(rendered)
        } else {
            Err("an annotation is written over more than one line".to_string())
        }
    };
    // python makes a function's type parameters once, with the function, and its
    // annotations name those very objects. the factory makes them the first time it is
    // called, and is handed them back every time after, when the definition it writes
    // declares none of its own and reads them from its frame instead
    let declares = stub
        .type_params
        .as_ref()
        .map(|params| {
            params
                .iter()
                .map(|param| param.name().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut undeclared = stub.clone();
    undeclared.type_params = None;
    let making = render(stub)?;
    let made = render(undeclared)?;

    let values = free_name("_by_values", &making);
    let type_params = free_name("_by_type_params", &making);
    let mut source = String::new();
    if future_annotations(module) {
        source.push_str("from __future__ import annotations\n");
    }
    let _ = writeln!(source, "def {FACTORY}({values}, {type_params}):");
    for (name, index) in &closed {
        let _ = writeln!(source, "    {name} = {values}[{index}]");
    }
    let stub = |source: &mut String, rendered: &str, indent: &str| {
        for line in rendered.lines() {
            source.push_str(indent);
            source.push_str(line);
            source.push('\n');
        }
        let _ = writeln!(source, "{indent}return {}", def.name.as_str());
    };
    if declares.is_empty() {
        stub(&mut source, &making, "    ");
    } else {
        let _ = writeln!(source, "    if {type_params} is None:");
        stub(&mut source, &making, "        ");
        for (index, name) in declares.iter().enumerate() {
            let _ = writeln!(source, "    {name} = {type_params}[{index}]");
        }
        stub(&mut source, &made, "    ");
    }
    Ok((source, !closed.is_empty()))
}

/// a factory that refuses, in words that say which definition and why
fn refusal(qualname: &str, reason: &str) -> String {
    let message = format!(
        "the annotations of `{qualname}` are not evaluated by this compiled build: {reason}"
    );
    format!(
        "def {FACTORY}(values, type_params):\n    raise RuntimeError({})\n",
        python_string(&message)
    )
}

/// a scope enclosing the definition, outermost first
enum Scope<'a> {
    Function(&'a ast::StmtFunctionDef),
    Class,
}

/// the one definition `qualname` names in `module`, and the scopes around it
fn locate<'a>(
    module: &'a [Stmt],
    qualname: &str,
) -> Result<(&'a ast::StmtFunctionDef, Vec<Scope<'a>>), String> {
    let mut components = qualname.split('.').peekable();
    let mut body = module;
    let mut enclosing = Vec::new();
    while let Some(component) = components.next() {
        let found = defined(body, component);
        let [found] = found.as_slice() else {
            return Err(format!(
                "the interpreted module does not define `{qualname}` exactly once"
            ));
        };
        match found {
            Stmt::FunctionDef(def) => {
                if components.peek().is_none() {
                    return Ok((def, enclosing));
                }
                if components.next() != Some("<locals>") {
                    return Err(format!("`{qualname}` names no nested function"));
                }
                enclosing.push(Scope::Function(def));
                body = &def.body;
            }
            Stmt::ClassDef(class) => {
                enclosing.push(Scope::Class);
                body = &class.body;
            }
            _ => break,
        }
    }
    Err(format!("`{qualname}` names no nested function"))
}

/// the `def`s and `class`es named `name` that one scope's body binds, at any depth of the
/// statements that are not scopes of their own
fn defined<'a>(body: &'a [Stmt], name: &str) -> Vec<&'a Stmt> {
    let mut out = Vec::new();
    for stmt in body {
        match stmt {
            Stmt::FunctionDef(def) if def.name.as_str() == name => out.push(stmt),
            Stmt::ClassDef(class) if class.name.as_str() == name => out.push(stmt),
            Stmt::If(node) => {
                out.extend(defined(&node.body, name));
                for clause in &node.elif_else_clauses {
                    out.extend(defined(&clause.body, name));
                }
            }
            Stmt::For(node) => {
                out.extend(defined(&node.body, name));
                out.extend(defined(&node.orelse, name));
            }
            Stmt::While(node) => {
                out.extend(defined(&node.body, name));
                out.extend(defined(&node.orelse, name));
            }
            Stmt::With(node) => out.extend(defined(&node.body, name)),
            Stmt::Try(node) => {
                out.extend(defined(&node.body, name));
                for handler in &node.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    out.extend(defined(&handler.body, name));
                }
                out.extend(defined(&node.orelse, name));
                out.extend(defined(&node.finalbody, name));
            }
            Stmt::Match(node) => {
                for case in &node.cases {
                    out.extend(defined(&case.body, name));
                }
            }
            _ => {}
        }
    }
    out
}

/// every name a function's own scope binds, and so every name a function nested in it
/// closes over rather than reading as a global
///
/// deliberately generous: a name counted here that python would not close over only
/// refuses a factory, where one missed would have an annotation read a global in place of
/// the enclosing frame's value
fn bound_in(def: &ast::StmtFunctionDef) -> HashSet<&str> {
    struct Binds<'a> {
        names: HashSet<&'a str>,
        globals: HashSet<&'a str>,
    }
    impl<'a> Visitor<'a> for Binds<'a> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                // a nested scope binds its own name here and nothing else
                Stmt::FunctionDef(node) => {
                    self.names.insert(node.name.as_str());
                    for decorator in &node.decorator_list {
                        self.visit_expr(&decorator.expression);
                    }
                }
                Stmt::ClassDef(node) => {
                    self.names.insert(node.name.as_str());
                }
                Stmt::Global(node) => {
                    self.globals
                        .extend(node.names.iter().map(ast::Identifier::as_str));
                }
                Stmt::Nonlocal(node) => {
                    self.names
                        .extend(node.names.iter().map(ast::Identifier::as_str));
                }
                Stmt::Import(node) => {
                    for alias in &node.names {
                        let bound = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                        self.names.insert(bound.split('.').next().unwrap_or(bound));
                    }
                }
                Stmt::ImportFrom(node) => {
                    for alias in &node.names {
                        self.names
                            .insert(alias.asname.as_ref().unwrap_or(&alias.name).as_str());
                    }
                }
                _ => visitor::walk_stmt(self, stmt),
            }
        }

        fn visit_expr(&mut self, expr: &'a Expr) {
            match expr {
                Expr::Name(name)
                    if matches!(name.ctx, ast::ExprContext::Store | ast::ExprContext::Del) =>
                {
                    self.names.insert(name.id.as_str());
                }
                Expr::Lambda(_) => {}
                _ => visitor::walk_expr(self, expr),
            }
        }

        fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
            let ast::ExceptHandler::ExceptHandler(node) = handler;
            if let Some(name) = &node.name {
                self.names.insert(name.as_str());
            }
            visitor::walk_except_handler(self, handler);
        }

        fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
            match pattern {
                ast::Pattern::MatchAs(node) => {
                    if let Some(name) = &node.name {
                        self.names.insert(name.as_str());
                    }
                }
                ast::Pattern::MatchStar(node) => {
                    if let Some(name) = &node.name {
                        self.names.insert(name.as_str());
                    }
                }
                ast::Pattern::MatchMapping(node) => {
                    if let Some(name) = &node.rest {
                        self.names.insert(name.as_str());
                    }
                }
                _ => {}
            }
            visitor::walk_pattern(self, pattern);
        }
    }

    let mut binds = Binds {
        names: HashSet::new(),
        globals: HashSet::new(),
    };
    for parameter in &*def.parameters {
        binds.names.insert(parameter.name().as_str());
    }
    for param in def.type_params.iter().flat_map(|params| params.iter()) {
        binds.names.insert(param.name().as_str());
    }
    binds.visit_body(&def.body);
    let Binds { names, globals } = binds;
    names.difference(&globals).copied().collect()
}

/// call `f` on `expr` and every expression inside it
fn visit<'a>(expr: &'a Expr, f: &mut impl FnMut(&'a Expr)) {
    struct Walk<'f, F> {
        f: &'f mut F,
    }
    impl<'a, F: FnMut(&'a Expr)> Visitor<'a> for Walk<'_, F> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            (self.f)(expr);
            visitor::walk_expr(self, expr);
        }
    }
    Walk { f }.visit_expr(expr);
}

/// whether python mangles `name` inside a class body
fn is_private(name: &str) -> bool {
    name.starts_with("__") && !name.ends_with("__")
}

/// whether the module compiles every annotation as a string
fn future_annotations(module: &[Stmt]) -> bool {
    module.iter().any(|stmt| {
        matches!(stmt, Stmt::ImportFrom(node)
            if node.module.as_ref().is_some_and(|module| module.as_str() == "__future__")
                && node.names.iter().any(|alias| alias.name.as_str() == "annotations"))
    })
}

/// a name that occurs nowhere in `text`
fn free_name(base: &str, text: &str) -> String {
    let mut name = base.to_string();
    while text.contains(&name) {
        name.push('_');
    }
    name
}

/// `text` as a python string literal
fn python_string(text: &str) -> String {
    let mut out = String::from("\"");
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            ' '..='~' => out.push(character),
            _ => {
                let _ = write!(out, "\\U{:08x}", u32::from(character));
            }
        }
    }
    out.push('"');
    out
}
