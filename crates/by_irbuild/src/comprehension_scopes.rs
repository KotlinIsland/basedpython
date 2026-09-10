//! the names a comprehension binds, kept apart from the frame it is lowered into
//!
//! a comprehension is lowered inline, as a loop in the function that holds it, but
//! python gives it a scope of its own:
//!
//! ```python
//! def f(xs):
//!     x = 5
//!     out = [x + 1 for x in xs]
//!     return x  # 5, whatever `xs` held
//! ```
//!
//! every part of the lowering that asks about a name — which register it is, what
//! representation it has, whether a closure captures it and whether that capture is a
//! shared cell — asks by the name's spelling. so rather than teach each of them about a
//! second kind of scope, each variable a comprehension's `for` clauses bind is given a
//! spelling of its own before any of them look, everywhere the comprehension's scope
//! can see it. no python identifier contains `$`, so the new spelling can meet nothing
//! the source wrote.
//!
//! what python's scoping rules say, and so what is renamed:
//!
//! - the first clause's iterable is evaluated in the enclosing scope, and every later
//!   clause's iterable, every condition and the element inside the comprehension's
//! - a lambda inside reads the comprehension's variable unless a parameter of its own
//!   has the name
//! - a walrus binds in the enclosing function, and python refuses one whose target is
//!   a comprehension's variable, so a walrus target is never renamed
//! - a comprehension nested inside another binds its own variables, which hide the
//!   outer one's of the same name
//!
//! a later clause's iterable reading a variable that clause or a later one binds reads
//! it before anything is bound, which python answers with `UnboundLocalError`. that
//! read is declined rather than given a spelling the error would then name

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_expr, walk_parameters};
use ruff_python_ast::{self as ast, Expr};

use crate::mapper::{Decline, Lowered};

/// the marker every spelling this gives out contains, and no source name can
const MARKER: char = '$';

/// `function`, with each comprehension variable spelled apart from every other name
pub(crate) fn scoped(function: &ast::StmtFunctionDef) -> Lowered<ast::StmtFunctionDef> {
    let mut function = function.clone();
    let renamer = Renamer {
        scopes: RefCell::new(Vec::new()),
        next: Cell::new(0),
        refused: RefCell::new(None),
    };
    for stmt in &mut function.body {
        renamer.visit_stmt(stmt);
    }
    match renamer.refused.into_inner() {
        Some(reason) => Err(Decline::new(reason)),
        None => Ok(function),
    }
}

/// what one scope does to a name it binds
#[derive(Clone)]
enum Binding {
    /// the comprehension's variable, under its own spelling
    Renamed(Name),
    /// the same, before the clause that binds it has run
    Unbound(Name),
    /// a lambda parameter, which hides every scope outside it
    Hidden,
}

struct Renamer {
    /// innermost last
    scopes: RefCell<Vec<HashMap<Name, Binding>>>,
    next: Cell<usize>,
    refused: RefCell<Option<String>>,
}

impl Renamer {
    fn lookup(&self, name: &Name) -> Option<Binding> {
        self.scopes
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).cloned())
    }

    fn comprehension(&self, generators: &mut [ast::Comprehension], rest: &mut [&mut Expr]) {
        let Some((first, _)) = generators.split_first_mut() else {
            return;
        };
        self.visit_expr(&mut first.iter);
        // every variable is the comprehension's from its first clause on, bound or not
        let mut scope: HashMap<Name, Binding> = HashMap::new();
        for generator in generators.iter() {
            target_names(&generator.target, &mut |name| {
                if !name.contains(MARKER) && !scope.contains_key(name) {
                    let at = self.next.get();
                    self.next.set(at + 1);
                    let spelling = Name::new(format!("{name}{MARKER}{at}"));
                    scope.insert(name.clone(), Binding::Unbound(spelling));
                }
            });
        }
        self.scopes.borrow_mut().push(scope);
        for (clause, generator) in generators.iter_mut().enumerate() {
            if clause > 0 {
                self.visit_expr(&mut generator.iter);
            }
            self.target(&mut generator.target);
            for condition in &mut generator.ifs {
                self.visit_expr(condition);
            }
        }
        for expr in rest {
            self.visit_expr(expr);
        }
        self.scopes.borrow_mut().pop();
    }

    /// a clause's target, whose names are bound from here on
    fn target(&self, target: &mut Expr) {
        match target {
            Expr::Name(name) => {
                let mut scopes = self.scopes.borrow_mut();
                if let Some(scope) = scopes.last_mut()
                    && let Some(binding) = scope.get_mut(&name.id)
                {
                    let spelling = match binding {
                        Binding::Renamed(spelling) | Binding::Unbound(spelling) => spelling.clone(),
                        Binding::Hidden => return,
                    };
                    *binding = Binding::Renamed(spelling.clone());
                    name.id = spelling;
                }
            }
            Expr::Tuple(tuple) => tuple
                .elts
                .iter_mut()
                .for_each(|element| self.target(element)),
            Expr::List(list) => list
                .elts
                .iter_mut()
                .for_each(|element| self.target(element)),
            Expr::Starred(starred) => self.target(&mut starred.value),
            // an attribute or a subscript is a store through a value the target reads
            other => self.visit_expr(other),
        }
    }
}

impl Transformer for Renamer {
    fn visit_expr(&self, expr: &mut Expr) {
        match expr {
            Expr::ListComp(node) => self.comprehension(&mut node.generators, &mut [&mut node.elt]),
            Expr::SetComp(node) => self.comprehension(&mut node.generators, &mut [&mut node.elt]),
            Expr::Generator(node) => {
                self.comprehension(&mut node.generators, &mut [&mut node.elt]);
            }
            Expr::DictComp(node) => match &mut node.key {
                Some(key) => {
                    self.comprehension(&mut node.generators, &mut [key, &mut node.value]);
                }
                None => self.comprehension(&mut node.generators, &mut [&mut node.value]),
            },
            Expr::Lambda(node) => {
                // the defaults are read where the lambda is made, before its
                // parameters exist
                let mut hidden: HashMap<Name, Binding> = HashMap::new();
                if let Some(parameters) = &mut node.parameters {
                    walk_parameters(self, parameters);
                    for parameter in parameters.iter() {
                        hidden.insert(parameter.name().id.clone(), Binding::Hidden);
                    }
                }
                self.scopes.borrow_mut().push(hidden);
                self.visit_expr(&mut node.body);
                self.scopes.borrow_mut().pop();
            }
            // python binds a walrus in the function, and refuses one naming a variable a
            // comprehension around it binds
            Expr::Named(node) => self.visit_expr(&mut node.value),
            Expr::Name(node) => match self.lookup(&node.id) {
                Some(Binding::Renamed(spelling)) => node.id = spelling,
                Some(Binding::Unbound(_)) => {
                    self.refused.borrow_mut().get_or_insert_with(|| {
                        format!(
                            "a comprehension clause reads `{}` before the clause that binds it",
                            node.id
                        )
                    });
                }
                Some(Binding::Hidden) | None => {}
            },
            _ => walk_expr(self, expr),
        }
    }
}

fn target_names(target: &Expr, found: &mut impl FnMut(&Name)) {
    match target {
        Expr::Name(name) => found(&name.id),
        Expr::Tuple(tuple) => tuple
            .iter()
            .for_each(|element| target_names(element, found)),
        Expr::List(list) => list.iter().for_each(|element| target_names(element, found)),
        Expr::Starred(starred) => target_names(&starred.value, found),
        _ => {}
    }
}
