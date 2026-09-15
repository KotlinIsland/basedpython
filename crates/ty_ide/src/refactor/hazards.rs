//! basedpython constructs whose meaning depends on where they are written.
//!
//! Most of basedpython reads the same wherever it is moved to, but a few forms
//! are resolved against their surroundings rather than against what they spell:
//! a bare enum member reads its enum off the type expected where it is written,
//! a call fills `context` parameters from the declarations in scope, a name or
//! an attribute can resolve through an implicit receiver, and a trailing lambda
//! block binds names of its own. Moving code that contains one of those can
//! silently change what it means, so the refactorings that move code refuse it.

use ruff_python_ast::visitor::source_order::{SourceOrderVisitor, TraversalSignal, walk_node};
use ruff_python_ast::{self as ast, AnyNodeRef};
use ty_python_semantic::types::context_params::implicit_context_arguments;
use ty_python_semantic::{HasType, SemanticModel};

use super::RefactorContext;

/// The first construct in `node` whose meaning depends on where it is written,
/// described for a refusal. Always `None` outside basedpython source.
pub(crate) fn context_dependent_construct(
    context: &RefactorContext<'_>,
    node: AnyNodeRef<'_>,
) -> Option<String> {
    if !context.is_basedpython() {
        return None;
    }
    let mut visitor = HazardVisitor {
        model: &context.model,
        found: None,
    };
    walk_node(&mut visitor, node);
    visitor.found
}

struct HazardVisitor<'m, 'db> {
    model: &'m SemanticModel<'db>,
    found: Option<String>,
}

impl<'a> SourceOrderVisitor<'a> for HazardVisitor<'_, '_> {
    fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
        if self.found.is_some() {
            return TraversalSignal::Skip;
        }
        self.found = self.hazard(node);
        if self.found.is_some() {
            TraversalSignal::Skip
        } else {
            TraversalSignal::Traverse
        }
    }
}

impl HazardVisitor<'_, '_> {
    fn hazard(&self, node: AnyNodeRef<'_>) -> Option<String> {
        match node {
            AnyNodeRef::ExprName(name) if name.ctx.is_load() => {
                if self.model.implicit_receiver_name(name).is_some() {
                    return Some(format!(
                        "`{}` resolves through the receiver of an enclosing block",
                        name.id
                    ));
                }
                if self.model.context_sensitive_qualifier(name).is_some() {
                    return Some(format!(
                        "`{}` is resolved from the type expected where it is written",
                        name.id
                    ));
                }
                None
            }
            AnyNodeRef::ExprAttribute(attribute) => {
                self.model.implicit_receiver_attribute(attribute).then(|| {
                    format!(
                        "`.{}` resolves through an implicit receiver in scope",
                        attribute.attr
                    )
                })
            }
            AnyNodeRef::ExprCall(call) => {
                let callee = call.func.inferred_type(self.model)?;
                let env = self.model.program_environment();
                let arguments = implicit_context_arguments(
                    self.model.db(),
                    &env,
                    self.model.file(),
                    callee,
                    call,
                );
                (!arguments.is_empty()).then(|| {
                    "a call fills `context` parameters from the declarations in scope".to_string()
                })
            }
            AnyNodeRef::ExprStatement(_) => {
                Some("it contains a statement written as an expression".to_string())
            }
            AnyNodeRef::StmtFunctionDef(function) if function.is_trailing_lambda => {
                Some("it contains a trailing lambda block".to_string())
            }
            _ => None,
        }
    }
}

/// Whether `expr` contains a walrus, which binds a name wherever it is evaluated.
pub(crate) fn contains_named_expression(expr: &ast::Expr) -> bool {
    struct Finder {
        found: bool,
    }
    impl<'a> SourceOrderVisitor<'a> for Finder {
        fn enter_node(&mut self, node: AnyNodeRef<'a>) -> TraversalSignal {
            if node.is_expr_named() {
                self.found = true;
            }
            if self.found {
                TraversalSignal::Skip
            } else {
                TraversalSignal::Traverse
            }
        }
    }
    let mut finder = Finder { found: false };
    walk_node(&mut finder, AnyNodeRef::from(expr));
    finder.found
}
