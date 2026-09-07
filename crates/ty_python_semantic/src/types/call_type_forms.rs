//! The typing constructs that spell a type through a *call* rather than through an
//! annotation.
//!
//! Most type expressions sit somewhere a reader can point at syntactically — after a
//! `:`, after a `->`, on the right of a `type` alias. These do not: `NewType("D", int)`
//! names its base in an argument, `TypeVar("T", bound=int)` names its bound in a
//! keyword, and the functional `NamedTuple` and `TypedDict` name their field types
//! inside a list or dict literal. Nothing about the call syntax says so — the answer
//! comes from what the callee resolves to.
//!
//! Two consumers need that answer. Type inference needs it to check the arguments as
//! type expressions, and the basedpython transpiler needs it to lower the surface syntax
//! written in them: `TypeVar("T", bound=A & B)` has to reach the emitted python as
//! `bound=Intersection[A, B]`, exactly as `x: A & B` does. Stating it once, here, is
//! what keeps a form the type checker accepts from being one the transpiler leaves
//! behind — which is a silent miscompilation, because the unlowered expression is
//! perfectly good python that means something else at runtime.

use ruff_python_ast as ast;

use crate::Db;
use crate::types::{KnownClass, SpecialFormType, Type, TypingModule};

/// A typing construct whose call arguments hold type expressions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallTypeForm {
    /// `NewType("D", int)` — the second argument is the base type
    NewType,
    /// `TypeAliasType("X", int | str)` — the second argument is the alias value
    TypeAliasType,
    /// `NamedTuple("NT", [("f", int)])` — the second argument holds `(name, type)` pairs
    NamedTuple,
    /// `TypedDict("TD", {"f": int})` — the second argument holds `key: type` items
    TypedDict,
    /// `TypeVar("T", int, str, bound=…, default=…)` — the constraints, the bound and the
    /// default are all type expressions
    TypeVar,
    /// `ParamSpec("P", default=[int, str])` — the default is a parameter list
    ParamSpec,
    /// `TypeVarTuple("Ts", default=Unpack[tuple[int, ...]])` — the default is a type
    TypeVarTuple,
}

impl CallTypeForm {
    /// Which construct, if any, `callee` is. `callee` is the inferred type of the
    /// expression in call position, so an unresolved or shadowed name answers `None`
    /// rather than being matched by spelling.
    pub fn of<'db>(db: &'db dyn Db, callee: Type<'db>) -> Option<Self> {
        if callee == Type::SpecialForm(SpecialFormType::NamedTuple) {
            return Some(Self::NamedTuple);
        }
        if TypingModule::from_typed_dict_type(db, callee).is_some() {
            return Some(Self::TypedDict);
        }
        match callee.as_class_literal()?.known(db)? {
            KnownClass::NewType => Some(Self::NewType),
            KnownClass::TypeVar | KnownClass::ExtensionsTypeVar => Some(Self::TypeVar),
            KnownClass::ParamSpec | KnownClass::ExtensionsParamSpec => Some(Self::ParamSpec),
            KnownClass::TypeVarTuple | KnownClass::ExtensionsTypeVarTuple => {
                Some(Self::TypeVarTuple)
            }
            known_class if TypingModule::from_type_alias_class(known_class).is_some() => {
                Some(Self::TypeAliasType)
            }
            _ => None,
        }
    }

    /// The sub-expressions of `arguments` that this form reads as type expressions.
    ///
    /// Each one is a complete type expression, so a caller that walks type expressions
    /// can hand them to the same traversal it uses for an annotation. Arguments a form
    /// reads as ordinary values — every form's leading name, a `TypedDict`'s field keys,
    /// `TypeAliasType`'s `type_params` — are not returned.
    pub fn type_expressions(self, arguments: &ast::Arguments) -> Vec<&ast::Expr> {
        let mut found = Vec::new();
        match self {
            Self::NewType | Self::TypeAliasType => found.extend(arguments.args.get(1)),
            Self::NamedTuple => {
                // `[("f", int), ("g", str)]`, or the same as a tuple, each field written
                // as either a list or a tuple of its name and its type
                if let Some(fields) = arguments.args.get(1) {
                    for field in sequence_elements(fields).into_iter().flatten() {
                        found.extend(sequence_elements(field).and_then(|pair| pair.get(1)));
                    }
                }
            }
            Self::TypedDict => {
                if let Some(ast::Expr::Dict(fields)) = arguments.args.get(1) {
                    found.extend(
                        fields
                            .items
                            .iter()
                            .filter(|item| item.key.is_some())
                            .map(|item| &item.value),
                    );
                }
                found.extend(
                    arguments
                        .find_keyword("extra_items")
                        .map(|keyword| &keyword.value),
                );
            }
            Self::TypeVar => {
                // every positional argument after the name is a constraint
                found.extend(arguments.args.iter().skip(1));
                found.extend(arguments.find_keyword("bound").map(|kw| &kw.value));
                found.extend(arguments.find_keyword("default").map(|kw| &kw.value));
            }
            Self::TypeVarTuple => {
                found.extend(arguments.find_keyword("default").map(|kw| &kw.value));
            }
            Self::ParamSpec => {
                // a `ParamSpec` default is a parameter list, `[int, str]`, whose elements
                // are the type expressions — or `...`, which holds none
                if let Some(default) = arguments.find_keyword("default") {
                    match &default.value {
                        ast::Expr::List(list) => found.extend(&list.elts),
                        value => found.push(value),
                    }
                }
            }
        }
        found
    }
}

/// The elements of a list or tuple literal, or `None` for anything else.
fn sequence_elements(expr: &ast::Expr) -> Option<&[ast::Expr]> {
    match expr {
        ast::Expr::List(list) => Some(&list.elts),
        ast::Expr::Tuple(tuple) => Some(&tuple.elts),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_parser::parse_expression;
    use ruff_text_size::Ranged;

    /// The source text of each argument `form` reads as a type expression in `call`.
    fn type_expressions(form: CallTypeForm, call: &str) -> Vec<String> {
        let parsed = parse_expression(call).expect("a call expression");
        let ast::Expr::Call(call_expr) = parsed.expr() else {
            panic!("expected a call expression");
        };
        form.type_expressions(&call_expr.arguments)
            .into_iter()
            .map(|expr| call[expr.range()].to_string())
            .collect()
    }

    #[test]
    fn new_type_reads_its_base() {
        assert_eq!(
            type_expressions(CallTypeForm::NewType, r#"NewType("D", int | str)"#),
            ["int | str"]
        );
    }

    #[test]
    fn type_alias_type_reads_its_value_but_not_its_type_params() {
        assert_eq!(
            type_expressions(
                CallTypeForm::TypeAliasType,
                r#"TypeAliasType("X", list[T], type_params=(T,))"#
            ),
            ["list[T]"]
        );
    }

    #[test]
    fn type_var_reads_its_constraints_bound_and_default() {
        assert_eq!(
            type_expressions(
                CallTypeForm::TypeVar,
                r#"TypeVar("T", int, str, bound=object, default=int, covariant=True)"#
            ),
            ["int", "str", "object", "int"]
        );
    }

    #[test]
    fn param_spec_reads_the_elements_of_its_default() {
        assert_eq!(
            type_expressions(
                CallTypeForm::ParamSpec,
                r#"ParamSpec("P", default=[int, str])"#
            ),
            ["int", "str"]
        );
        assert_eq!(
            type_expressions(CallTypeForm::ParamSpec, r#"ParamSpec("P", default=...)"#),
            ["..."]
        );
    }

    #[test]
    fn type_var_tuple_reads_its_default() {
        assert_eq!(
            type_expressions(
                CallTypeForm::TypeVarTuple,
                r#"TypeVarTuple("Ts", default=Unpack[tuple[int, ...]])"#
            ),
            ["Unpack[tuple[int, ...]]"]
        );
    }

    #[test]
    fn named_tuple_reads_each_field_type_but_not_its_name() {
        assert_eq!(
            type_expressions(
                CallTypeForm::NamedTuple,
                r#"NamedTuple("NT", [("f", int), ["g", str]])"#
            ),
            ["int", "str"]
        );
    }

    #[test]
    fn named_tuple_reads_nothing_from_a_field_list_it_cannot_see_into() {
        assert!(
            type_expressions(CallTypeForm::NamedTuple, r#"NamedTuple("NT", fields)"#).is_empty()
        );
    }

    #[test]
    fn typed_dict_reads_each_field_type_and_extra_items() {
        assert_eq!(
            type_expressions(
                CallTypeForm::TypedDict,
                r#"TypedDict("TD", {"f": int, **rest}, total=False, extra_items=str)"#
            ),
            ["int", "str"]
        );
    }
}
