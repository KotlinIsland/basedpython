//! Lowers keyword arguments in subscriptions to a `__getitem__` call.
//!
//! `x[a, z=1]` → `x.__getitem__(a, z=1)`
//!
//! Python's subscript grammar doesn't accept keyword args (PEP 637 was
//! rejected), so basedpython's surface syntax falls back to the explicit
//! method call. positional and keyword args are forwarded in source order
//!
//! A value is written where it lands rather than printed from its source, so whatever lowers
//! inside it, a type form above all, reaches the output, however the arguments are reordered
//! around it: [`KwSubscriptPass`] passes it through, and a lowerer writing a whole type
//! expression renders it.

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Expr, ExprSubscript, PythonVersion, Stmt};
use ruff_text_size::Ranged;

use crate::transforms::ast_driver::{Fragment, Lowering, PassContext, TypeAwarePass};
use crate::type_info::TypeInfo;

/// one part of what a keyword subscript is written as
pub(crate) enum Piece<'a> {
    Text(String),
    Value(&'a Expr),
}

/// `arguments` with `separator` between them
fn joined<'a>(arguments: Vec<Vec<Piece<'a>>>, separator: &str) -> Vec<Piece<'a>> {
    let mut pieces = Vec::new();
    for (index, argument) in arguments.into_iter().enumerate() {
        if index > 0 {
            pieces.push(Piece::Text(separator.to_owned()));
        }
        pieces.extend(argument);
    }
    pieces
}

/// `value[arguments]`
fn subscript_of<'a>(value: &'a Expr, arguments: Vec<Vec<Piece<'a>>>) -> Vec<Piece<'a>> {
    let mut pieces = vec![Piece::Value(value), Piece::Text("[".to_owned())];
    pieces.extend(joined(arguments, ", "));
    pieces.push(Piece::Text("]".to_owned()));
    pieces
}

/// `sub` as a call of `__getitem__`, its keyword fields passed as `name=value`
fn getitem_call(sub: &ExprSubscript) -> Vec<Piece<'_>> {
    let arguments = subscript_elements(sub)
        .into_iter()
        .map(|element| {
            if let Expr::Named(n) = element
                && let Some(target) = n.label()
            {
                return vec![
                    Piece::Text(format!("{}=", target.id.as_str())),
                    Piece::Value(&n.value),
                ];
            }
            vec![Piece::Value(element)]
        })
        .collect();
    let mut pieces = vec![
        Piece::Value(&sub.value),
        Piece::Text(".__getitem__(".to_owned()),
    ];
    pieces.extend(joined(arguments, ", "));
    pieces.push(Piece::Text(")".to_owned()));
    pieces
}

/// whether the subscript carries a keyword field (`x[a=1]`) — the parser
/// spells one as a named expression whose target is [`ExprContext::Invalid`]
fn is_keyword_field(element: &Expr) -> bool {
    matches!(element, Expr::Named(n) if n.label().is_some())
}

fn subscript_elements(sub: &ExprSubscript) -> Vec<&Expr> {
    match sub.slice.as_ref() {
        Expr::Tuple(t) if !t.parenthesized => t.elts.iter().collect(),
        single => vec![single],
    }
}

/// Lower a subscript of a class declaring a keyword-variadic pack.
///
/// `class A[**Kwargs]` is a `ParamSpec` at runtime, and python has no keyword subscript, so
/// the pack's fields lower to the `ParamSpec` list form — `A[foo=int, bar=str]` → `A[[int,
/// str]]`, `A[()]` → `A[[]]`. field names are erased, which matches python's own erasure of
/// type arguments; the names are checked against the `.by` source, not this output.
///
/// `None` for an all-positional subscript with no pack slot to fill, which is already valid
/// python
fn keyword_pack_subscript(sub: &ExprSubscript, pack_index: usize) -> Option<Vec<Piece<'_>>> {
    let elements: Vec<&Expr> = match sub.slice.as_ref() {
        Expr::Tuple(t) => t.elts.iter().collect(),
        single => vec![single],
    };

    let mut fields = Vec::new();
    let mut parts = Vec::new();
    for element in elements {
        if let Expr::Named(n) = element
            && n.label().is_some()
        {
            fields.push(vec![Piece::Value(&n.value)]);
        } else {
            parts.push(vec![Piece::Value(element)]);
        }
    }

    if fields.is_empty() && parts.len() > pack_index {
        return None;
    }

    let mut pack = vec![Piece::Text("[".to_owned())];
    pack.extend(joined(fields, ", "));
    pack.push(Piece::Text("]".to_owned()));
    if pack_index <= parts.len() {
        parts.insert(pack_index, pack);
    } else {
        parts.push(pack);
    }
    Some(subscript_of(&sub.value, parts))
}

/// what `sub` is written as in python, or `None` when it is already python
pub(crate) fn lowered<'a>(
    sub: &'a ExprSubscript,
    types: &dyn TypeInfo,
    min_version: PythonVersion,
) -> Option<Vec<Piece<'a>>> {
    // subscripting a function is a *type* specialization, not a runtime
    // keyword subscript: an erased generic has its whole `[…]` stripped by
    // `generic_call`, and a reified one routes the fields through the
    // `generic` wrapper's `__getitem__`, which takes them as keywords
    if let Expr::Name(name) = sub.value.as_ref()
        && types.is_function(name)
    {
        return (types.is_reified_function(name, min_version)
            && subscript_elements(sub)
                .iter()
                .any(|element| is_keyword_field(element)))
        .then(|| getitem_call(sub));
    }
    if let Some(pack_index) = types.class_keyword_pack_index(&sub.value)
        && let Some(pieces) = keyword_pack_subscript(sub, pack_index)
    {
        return Some(pieces);
    }
    // single keyword arg, e.g. `A[T=int]` (no surrounding tuple).
    // for a multi-typevar class with declared defaults, expand to a
    // positional list filling unbound slots with their declared defaults
    // (`A[R=int]` with `class A[T=int, R=str]` → `A[int, int]`).
    // single-typevar class drops the kw name
    if let Expr::Named(n) = sub.slice.as_ref()
        && let Some(target) = n.label()
    {
        let drop_name = || subscript_of(&sub.value, vec![vec![Piece::Value(&n.value)]]);
        if let Some(typevars) = types
            .class_typevars(&sub.value)
            .filter(|typevars| typevars.len() > 1)
        {
            let mut parts = Vec::with_capacity(typevars.len());
            for (tv_name, tv_default) in &typevars {
                if tv_name == target.id.as_str() {
                    parts.push(vec![Piece::Value(&n.value)]);
                } else if let Some(default) = tv_default {
                    parts.push(vec![Piece::Text(default.clone())]);
                } else {
                    // typevar has no default and no kw arg — fall back
                    // to drop-name behavior; ty's diagnostics will catch
                    // the missing-arg case
                    return Some(drop_name());
                }
            }
            return Some(subscript_of(&sub.value, parts));
        }
        // a class the subscript specializes erases the keyword with the rest
        // of its type arguments. anything else — an instance, a `Mapping`
        // whose `__getitem__` takes keywords — is a runtime call, the same
        // reading the tuple form below takes and the one ty checks. a value
        // ty could not resolve counts as a class: the keyword form spells type
        // arguments in practice, and rewriting an annotation into a call is the
        // more damaging of the two guesses
        if !types.subscript_is_getitem_call(&sub.value) {
            return Some(drop_name());
        }
        return Some(getitem_call(sub));
    }
    let Expr::Tuple(t) = sub.slice.as_ref() else {
        return None;
    };
    if t.parenthesized || !t.elts.iter().any(is_keyword_field) {
        return None;
    }
    // when the value is a known generic class, reorder by typevar declaration and emit a
    // positional subscript. positional arguments fill the leading typevars in source order,
    // keyword arguments bind by name, and any remaining typevar falls back to its declared
    // default (`A[R=str, T=int]` → `A[int, str]`; `C[int, D=bytes]` on `class C[A, B, D]` with
    // `B`'s default → `C[int, <B default>, bytes]`)
    if let Some(typevars) = types.class_typevars(&sub.value) {
        let mut by_name: std::collections::HashMap<&str, &Expr> = std::collections::HashMap::new();
        let mut positional: Vec<&Expr> = Vec::new();
        for elt in &t.elts {
            if let Expr::Named(n) = elt
                && let Some(target) = n.label()
            {
                by_name.insert(target.id.as_str(), n.value.as_ref());
            } else {
                positional.push(elt);
            }
        }
        let mut positional_iter = positional.iter();
        let mut parts = Vec::with_capacity(typevars.len());
        let mut filled_all = true;
        for (tv_name, tv_default) in &typevars {
            if let Some(value_expr) = by_name.get(tv_name.as_str()) {
                parts.push(vec![Piece::Value(value_expr)]);
            } else if let Some(value_expr) = positional_iter.next() {
                parts.push(vec![Piece::Value(value_expr)]);
            } else if let Some(default) = tv_default {
                parts.push(vec![Piece::Text(default.clone())]);
            } else {
                filled_all = false;
                break;
            }
        }
        // a leftover positional argument means the source over-specified the class; leave it
        // for the generic form below so ty's diagnostic is the authority on the arity
        if filled_all && positional_iter.next().is_none() {
            return Some(subscript_of(&sub.value, parts));
        }
        // missing typevar without default — fall through to the
        // generic `value.__getitem__(name=value, ...)` form so the
        // output is at least syntactically valid Python
    }
    Some(getitem_call(sub))
}

pub(crate) struct KwSubscriptPass {
    min_version: PythonVersion,
}

impl KwSubscriptPass {
    pub(crate) fn new(min_version: PythonVersion) -> Self {
        Self { min_version }
    }
}

impl TypeAwarePass for KwSubscriptPass {
    fn lowering(&self) -> Option<Lowering> {
        Some(Lowering::KwSubscript)
    }

    /// a value is passed through with whatever lowers inside it, and one that is printed in
    /// place of the subscript, inside a type a wider edit writes, is printed lowered
    fn subsumes(&self) -> &'static [Lowering] {
        super::callable::TYPE_EXPRESSION
    }

    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = KwSubscript {
            types,
            min_version: self.min_version,
            ctx,
        };
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
    }
}

struct KwSubscript<'a> {
    types: &'a dyn TypeInfo,
    min_version: PythonVersion,
    ctx: &'a mut PassContext,
}

impl<'ast> Visitor<'ast> for KwSubscript<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Subscript(sub) = expr
            && let Some(pieces) = lowered(sub, self.types, self.min_version)
        {
            let fragments = pieces
                .into_iter()
                .map(|piece| match piece {
                    Piece::Text(text) => Fragment::Lit(text),
                    Piece::Value(value) => Fragment::Src(value.range()),
                })
                .collect();
            self.ctx.template_edits.push((sub.range(), fragments));
        }
        walk_expr(self, expr);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    // multi-arg kw reorder lives under transpile_typed (needs ty type info).
    // exercised via mdtest `basedpython_kw_subscript`

    #[test]
    fn simple_kwarg() {
        check("x[a, z=1]\n", "x.__getitem__(a, z=1)\n");
    }

    #[test]
    fn multiple_kwargs() {
        check(
            "x[a, b, key=1, val=\"v\"]\n",
            "x.__getitem__(a, b, key=1, val=\"v\")\n",
        );
    }

    #[test]
    fn no_kwargs_unchanged() {
        check("x[a, b]\n", "x[a, b]\n");
    }

    #[test]
    fn single_kw_drops_name() {
        check("a: A[T=int]\n", "a: A[int]\n");
    }

    /// a `?` on a kw-subscript value lowers instead of leaking the bare token
    /// (our whole-subscript edit would otherwise subsume `optional_type`'s)
    /// an *instance* is not specializable, so a single keyword field on one is
    /// the runtime call the tuple form already lowers to — dropping the name
    /// silently turned it into a positional index
    #[test]
    fn single_kw_on_an_instance_is_a_getitem_call() {
        check(
            "class A:\n    def __getitem__(self, *, foo: int) -> int: ...\n\nprint(A()[foo=1])\n",
            "class A:\n    def __getitem__(self, *, foo: int) -> int: ...\n\nprint(A().__getitem__(foo=1))\n",
        );
    }

    #[test]
    fn single_kw_on_a_single_typevar_class_still_drops_the_name() {
        let out = transpile("class B[T]: ...\n\na: B[T=int]\n", &Config::test_default()).unwrap();
        assert!(out.ends_with("a: B[int]\n"), "got:\n{out}");
    }

    #[test]
    fn single_kw_value_optional_lowers() {
        check("a: A[T=int?]\n", "a: A[int | None]\n");
    }

    #[test]
    fn getitem_kw_value_optional_lowers() {
        check(
            "d = data[idx, mode=int?]\n",
            "d = data.__getitem__(idx, mode=int | None)\n",
        );
    }

    #[test]
    fn python_unchanged() {
        unchanged("x[a, b]\n");
    }

    /// a keyword-variadic pack is a `ParamSpec` at runtime, so its fields lower to the
    /// `ParamSpec` list form. needs type info to know the class declares a pack
    mod keyword_pack {
        use ruff_db::files::system_path_to_file;
        use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
        use ty_project::{ProjectMetadata, TestDb};

        use crate::{Config, transpile_typed};

        fn transpiled(source: &str) -> String {
            let mut db = TestDb::new(ProjectMetadata::new(
                ruff_python_ast::name::Name::new_static(""),
                SystemPathBuf::from("/proj"),
            ));
            db.write_file("/proj/main.by", source)
                .expect("write file failed");
            db.init_program().expect("program init failed");
            let file = system_path_to_file(&db, "/proj/main.by").expect("file not in db");
            transpile_typed(&db, file, &Config::test_default(), None).expect("transpile failed")
        }

        #[test]
        fn fields_lower_to_a_parameter_list() {
            let output =
                transpiled("class A[**Kwargs]: ...\n\ndef f(a: A[foo=int, bar=str]): ...\n");
            assert!(
                output.contains("def f(a: A[[int, str]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        /// what `form` is written as in the type argument of a class with no keywords involved
        fn as_a_type_argument(form: &str) -> String {
            let output = transpiled(&format!("x: list[{form}]\n"));
            let line = output.lines().find(|line| line.starts_with("x: ")).unwrap();
            line.strip_prefix("x: list[")
                .and_then(|line| line.strip_suffix(']'))
                .unwrap()
                .to_owned()
        }

        /// a keyword names a position and says nothing about the type written at it
        #[test]
        fn a_field_lowers_as_any_other_type_argument() {
            for form in [
                "1",
                "1?",
                "1??",
                "(1 | 2)?",
                "list[1]?",
                "int?",
                "int & str",
                "dynamic",
                "int or str",
                "not int",
                "float",
                "complex",
                "1.5",
                "(int, str)",
                "(1?, int)",
                "(1, \"a\")",
                "list[(int, str)]",
                "(int) -> str",
                "(int) -> 1?",
                "literal str",
                "list[literal str]",
                "final int",
            ] {
                let lowered = as_a_type_argument(form);
                let output = transpiled(&format!(
                    "class M[**Kwargs]: ...\nclass N[T]: ...\nclass A[T, U = int]: ...\n\n\
                     m: M[a={form}, b=str]\nn: N[T={form}]\na: A[U={form}, T=str]\n"
                ));
                for written in [
                    format!("m: M[[{lowered}, str]]"),
                    format!("n: N[{lowered}]"),
                    format!("a: A[str, {lowered}]"),
                ] {
                    assert!(
                        output.lines().any(|line| line == written),
                        "{form}: expected `{written}` in:\n{output}"
                    );
                }
            }
        }

        #[test]
        fn a_field_holding_a_keyword_subscript_lowers_too() {
            let output = transpiled(
                "class M[**Kwargs]: ...\nclass N[T]: ...\n\nx: M[a=N[T=1?], b=M[c=int?]]\n",
            );
            assert!(
                output.contains("x: M[[N[Literal[1] | None], M[[int | None]]]]"),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn a_keyword_subscript_inside_another_type_form_lowers() {
            let output = transpiled(
                "class M[**Kwargs]: ...\n\nx: (M[a=1?], int)\ny: (int) -> M[a=1?]\nz: M[a=1?]?\n",
            );
            for written in [
                "x: tuple[M[[Literal[1] | None]], int]",
                "y: Callable[[int], M[[Literal[1] | None]]]",
                "z: M[[Literal[1] | None]] | None",
            ] {
                assert!(output.contains(written), "{written}:\n{output}");
            }
        }

        #[test]
        fn a_field_read_as_a_value_is_written_through_the_literal_helper() {
            let output = transpiled("class M[**Kwargs]: ...\n\ny = M[a=1?]\n");
            assert!(
                output.contains("y = M[[_by_lit(1) | None]]"),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn empty_pack_lowers_to_an_empty_list() {
            let output = transpiled("class A[**Kwargs]: ...\n\ndef f(a: A[()]): ...\n");
            assert!(
                output.contains("def f(a: A[[]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn positional_type_arguments_keep_their_slots() {
            let output =
                transpiled("class Two[T, **Kwargs]: ...\n\ndef f(t: Two[bytes, foo=int]): ...\n");
            assert!(
                output.contains("def f(t: Two[bytes, [int]]): ..."),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn mixed_positional_and_keyword_reorder_to_positional() {
            // a positional argument followed by keyword ones must resolve to a positional
            // subscript, not a `__getitem__` call that crashes at runtime
            let output =
                transpiled("class C[A, B, D]: ...\n\ndef f(c: C[int, B=str, D=bytes]): ...\n");
            assert!(
                output.contains("def f(c: C[int, str, bytes]): ..."),
                "unexpected output:\n{output}"
            );
            assert!(
                !output.contains("__getitem__"),
                "unexpected output:\n{output}"
            );
        }

        #[test]
        fn positional_fills_leading_slots_with_default_gap() {
            let output = transpiled(
                "class C[A, B = str, D = bytes]: ...\n\ndef f(c: C[int, D=complex]): ...\n",
            );
            assert!(
                output.contains("def f(c: C[int, str, JustComplex]): ..."),
                "unexpected output:\n{output}"
            );
        }
    }
}
