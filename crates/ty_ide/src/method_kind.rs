//! which kind of method a `def` a class owns is: what its first parameter is passed

use ruff_python_ast::{self as ast, Expr, ExprContext};
use ty_python_semantic::types::ide_support::{InheritedMethodKind, method_kind_of};
use ty_python_semantic::{HasDefinition, SemanticModel};

/// what a method's first parameter is passed when it is called through an instance
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// the instance, conventionally `self`
    Instance,
    /// the class, conventionally `cls`
    Class,
    /// nothing: every parameter is the caller's
    Static,
}

impl MethodKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            MethodKind::Instance => "instance",
            MethodKind::Class => "class",
            MethodKind::Static => "static",
        }
    }
}

/// the kind of method `function`, a `def` written in a class body of `model`'s file, is: its
/// decorators as the checker resolves them, under whatever name they are bound to, and the
/// methods python makes static or class methods by their name alone. `None` where the checker
/// has no function for it
pub(crate) fn resolved_method_kind(
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
) -> Option<MethodKind> {
    let definition = function.definition(model);
    Some(match method_kind_of(model.db(), definition)? {
        InheritedMethodKind::Instance => MethodKind::Instance,
        InheritedMethodKind::Class => MethodKind::Class,
        InheritedMethodKind::Static => MethodKind::Static,
    })
}

/// the kind of method `function` is, given that a class body defines it, for where there is no
/// semantic model to ask: [`resolved_method_kind`] resolves the decorators instead
///
/// read from what the parser has: `@staticmethod` and `@classmethod` (bare, or through
/// `builtins`), basedpython's `static def` and `class def`, and the methods python makes static
/// or class methods by their name alone — `__new__`, and `__init_subclass__` and
/// `__class_getitem__`. any other `def` in a class body is passed its instance, decorated or not
pub fn method_kind(function: &ast::StmtFunctionDef) -> MethodKind {
    for decorator in &function.decorator_list {
        let name = match &decorator.expression {
            // `static def` and `class def` are parsed into decorators named for the modifier,
            // which no source spells, so they are told apart from a real name by their context
            Expr::Name(name) if name.ctx == ExprContext::Invalid => match name.id.as_str() {
                "static" => Some("staticmethod"),
                "classmethod" => Some("classmethod"),
                _ => None,
            },
            Expr::Name(name) => Some(name.id.as_str()),
            Expr::Attribute(attribute) if matches!(&*attribute.value, Expr::Name(module) if module.id.as_str() == "builtins") => {
                Some(attribute.attr.as_str())
            }
            _ => None,
        };
        match name {
            Some("staticmethod") => return MethodKind::Static,
            Some("classmethod") => return MethodKind::Class,
            _ => {}
        }
    }
    match function.name.as_str() {
        "__new__" => MethodKind::Static,
        "__init_subclass__" | "__class_getitem__" => MethodKind::Class,
        _ => MethodKind::Instance,
    }
}
