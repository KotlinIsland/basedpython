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
use ruff_python_ast::{Expr, ExprSubscript, PythonVersion, Stmt, TypeParamKind};
use ruff_text_size::Ranged;

use crate::runtime::{self, Helper};
use crate::transforms::ast_driver::{Fragment, Lowering, PassContext, TypeAwarePass};
use crate::type_info::{SubscriptTypeParam, SubscriptTypeParamKind, TypeInfo};

/// one part of what a keyword subscript is written as
pub(crate) enum Piece<'a> {
    Text(String),
    Value(&'a Expr),
    /// the name of a runtime helper, which the module has to be given
    Helper(Helper),
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

/// the type argument a keyword subscript gives one type parameter, once its keywords are resolved
/// to positions
enum Slot<'a> {
    /// written for it, by name or by position
    Given(&'a Expr),
    /// the run of positional arguments a `*Ts` takes, which may be empty
    Run(Vec<&'a Expr>),
    /// nothing, so it takes its declared default
    Default,
}

/// the type argument each of `params` takes from `elements`, in declaration order, bound the way
/// the checker binds a keyword subscript: a keyword names the parameter it binds, the positional
/// arguments fill the parameters no keyword named in order, a `*Ts` takes the run of them the
/// parameters after it leave, and a parameter given nothing takes its default. a parameter after
/// a bare `*` is only ever given by name, and one before a `/` only by position
///
/// `None` for a subscript the checker reports: a keyword naming no parameter, a `*Ts` or a
/// positional-only one, an argument left over, or a parameter without a default given nothing
/// `None` too when `params` has a keyword-variadic pack, whose fields every keyword is
fn by_position<'a, 'p>(
    params: &'p [SubscriptTypeParam],
    elements: &[&'a Expr],
) -> Option<Vec<(&'p SubscriptTypeParam, Slot<'a>)>> {
    let mut by_name: Vec<(&str, &'a Expr)> = Vec::new();
    let mut positional: Vec<&'a Expr> = Vec::new();
    for element in elements {
        if let Expr::Named(n) = element
            && let Some(target) = n.label()
        {
            by_name.push((target.id.as_str(), &n.value));
        } else {
            positional.push(element);
        }
    }
    let named = |by_name: &[(&str, &Expr)], param: &SubscriptTypeParam| {
        by_name.iter().position(|(name, _)| *name == param.name)
    };
    let mut positional = positional.into_iter();
    let mut slots = Vec::with_capacity(params.len());
    for (index, param) in params.iter().enumerate() {
        if let Some(at) = named(&by_name, param) {
            if param.kind != SubscriptTypeParamKind::Single
                || param.given == TypeParamKind::PositionalOnly
            {
                return None;
            }
            slots.push((param, Slot::Given(by_name.remove(at).1)));
            continue;
        }
        let slot = match param.kind {
            SubscriptTypeParamKind::KeywordVariadic => return None,
            SubscriptTypeParamKind::Variadic => {
                let claimed_after = params[index + 1..]
                    .iter()
                    .filter(|later| {
                        later.given != TypeParamKind::KeywordOnly
                            && named(&by_name, later).is_none()
                    })
                    .count();
                let run = positional.len().saturating_sub(claimed_after);
                Slot::Run(positional.by_ref().take(run).collect())
            }
            SubscriptTypeParamKind::Single => match positional.next() {
                Some(value) if param.given != TypeParamKind::KeywordOnly => Slot::Given(value),
                Some(_) => return None,
                None if param.has_default => Slot::Default,
                None => return None,
            },
        };
        slots.push((param, slot));
    }
    (by_name.is_empty() && positional.next().is_none()).then_some(slots)
}

/// the positional arguments `slot` is written as, one given nothing written as `default` makes
/// it, `None` when there is no `default` to write
fn arguments_of<'a>(slot: Slot<'a>, default: Option<Piece<'a>>) -> Option<Vec<Vec<Piece<'a>>>> {
    Some(match slot {
        Slot::Given(given) => vec![vec![Piece::Value(given)]],
        Slot::Run(run) => run
            .into_iter()
            .map(|given| vec![Piece::Value(given)])
            .collect(),
        Slot::Default => vec![vec![default?]],
    })
}

/// a class's `value[slots]`, every type argument at the position of its parameter, a default it is
/// given nothing for spelled out
fn class_specialization<'a>(
    value: &'a Expr,
    slots: Vec<(&SubscriptTypeParam, Slot<'a>)>,
) -> Option<Vec<Piece<'a>>> {
    let mut arguments = Vec::new();
    for (param, slot) in slots {
        arguments.extend(arguments_of(
            slot,
            param.spelled_default.clone().map(Piece::Text),
        )?);
    }
    Some(subscript_of(value, arguments))
}

/// a reified function's specialization: what is given a parameter that may be given by position
/// at that position, a parameter given nothing told to take the default of the function the call
/// reaches, and what is given a keyword-only one by name, through the `generic` wrapper's
/// `__getitem__`. a keyword-only one given nothing is left out, and the wrapper reads its default
/// by name
fn function_specialization<'a>(
    value: &'a Expr,
    slots: Vec<(&SubscriptTypeParam, Slot<'a>)>,
) -> Vec<Piece<'a>> {
    let mut positional = Vec::new();
    let mut keywords = Vec::new();
    for (param, slot) in slots {
        if param.given != TypeParamKind::KeywordOnly {
            positional.extend(
                arguments_of(slot, Some(Piece::Helper(runtime::TYPE_DEFAULT))).unwrap_or_default(),
            );
        } else if let Slot::Given(given) = slot {
            keywords.push(vec![
                Piece::Text(format!("{}=", param.name)),
                Piece::Value(given),
            ]);
        }
    }
    if keywords.is_empty() {
        return subscript_of(value, positional);
    }
    positional.extend(keywords);
    let mut pieces = vec![Piece::Value(value), Piece::Text(".__getitem__(".to_owned())];
    pieces.extend(joined(positional, ", "));
    pieces.push(Piece::Text(")".to_owned()));
    pieces
}

/// what `sub` is written as in python, or `None` when it is already python
pub(crate) fn lowered<'a>(
    sub: &'a ExprSubscript,
    types: &dyn TypeInfo,
    min_version: PythonVersion,
) -> Option<Vec<Piece<'a>>> {
    let elements = subscript_elements(sub);
    let has_keyword = elements.iter().any(|element| is_keyword_field(element));
    // subscripting a function or a bound method is a *type* specialization, not a runtime
    // keyword subscript. an erased generic has its whole `[…]` stripped by `generic_call`. a
    // reified one is given its type arguments the way an override is matched to what it
    // overrides, so that they bind as the checker bound them against the static signature: by
    // position, which an override may rename, and by name after a bare `*`. a keyword-variadic
    // pack's fields reach the `generic` wrapper as keywords too
    if types.subscripts_a_function(&sub.value) {
        if !has_keyword || !types.subscript_takes_type_arguments(&sub.value, min_version) {
            return None;
        }
        let params = types.subscript_type_params(&sub.value);
        return Some(
            match params
                .as_deref()
                .and_then(|params| by_position(params, &elements))
            {
                Some(slots) => function_specialization(&sub.value, slots),
                None => getitem_call(sub),
            },
        );
    }
    let params = types.subscript_type_params(&sub.value);
    if let Some(pack_index) = params.as_deref().and_then(|params| {
        params
            .iter()
            .position(|param| param.kind == SubscriptTypeParamKind::KeywordVariadic)
    }) && let Some(pieces) = keyword_pack_subscript(sub, pack_index)
    {
        return Some(pieces);
    }
    if !has_keyword {
        return None;
    }
    // a generic class's type arguments are written by position, a default it is given nothing
    // for spelled out (`A[R=str, T=int]` → `A[int, str]`; `C[int, D=bytes]` on `class C[A, B, D]`
    // with `B`'s default → `C[int, <B default>, bytes]`)
    if let Some(params) = &params
        && let Some(pieces) =
            by_position(params, &elements).and_then(|slots| class_specialization(&sub.value, slots))
    {
        return Some(pieces);
    }
    // a class the subscript specializes erases a single keyword with the rest of its type
    // arguments, even one ty reports. anything else — an instance, a `Mapping` whose
    // `__getitem__` takes keywords — is a runtime call, the reading ty checks. a value ty could
    // not resolve counts as a class: the keyword form spells type arguments in practice, and
    // rewriting an annotation into a call is the more damaging of the two guesses
    if let Expr::Named(n) = sub.slice.as_ref()
        && n.label().is_some()
        && !types.subscript_is_getitem_call(&sub.value)
    {
        return Some(subscript_of(&sub.value, vec![vec![Piece::Value(&n.value)]]));
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
                    Piece::Helper(helper) => {
                        self.ctx.runtime.insert(helper);
                        Fragment::Lit(helper.name().to_owned())
                    }
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
    use ruff_db::files::system_path_to_file;
    use ruff_db::system::{DbWithWritableSystem, SystemPathBuf};
    use ty_project::{ProjectMetadata, TestDb};

    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile, transpile_typed};

    /// `source`, a `main.by` of a project, transpiled with what the checker read of it
    fn typed_transpile(source: &str, config: &Config) -> String {
        let mut db = TestDb::new(ProjectMetadata::new(
            ruff_python_ast::name::Name::new_static(""),
            SystemPathBuf::from("/proj"),
        ));
        db.write_file("/proj/main.by", source)
            .expect("write file failed");
        db.init_program().expect("program init failed");
        let file = system_path_to_file(&db, "/proj/main.by").expect("file not in db");
        transpile_typed(&db, file, config, None).expect("transpile failed")
    }

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

    /// a reified function is given its type arguments the way an override is matched to what
    /// it overrides, against the signature the checker read, and an erased one has nothing to
    /// subscript
    mod function {
        use ruff_python_ast::PythonVersion;

        use crate::Config;

        fn transpiled_at(source: &str, min_version: PythonVersion) -> String {
            super::typed_transpile(
                source,
                &Config {
                    min_version,
                    ..Config::test_default()
                },
            )
        }

        /// reification keeps python's own type parameter syntax, and a default in it needs 3.13
        fn transpiled(source: &str) -> String {
            transpiled_at(source, PythonVersion::PY313)
        }

        #[track_caller]
        fn writes(output: &str, written: &[&str]) {
            for line in written {
                assert!(
                    output.lines().any(|output_line| output_line == *line),
                    "`{line}` in:\n{output}"
                );
            }
        }

        #[test]
        fn keywords_are_written_at_their_positions() {
            writes(
                &transpiled_at(
                    "def g[T, U]():\n    return (T, U)\n\nprint(g[U=int, T=bytes]())\n\
                     print(g[int, U=str]())\n",
                    PythonVersion::PY312,
                ),
                &["print(g[bytes, int]())", "print(g[int, str]())"],
            );
        }

        /// the wrapper fills it from the function the call reaches
        #[test]
        fn a_parameter_given_nothing_is_told_to_take_its_default() {
            writes(
                &transpiled(
                    "def g[T, U = str, V = bytes]():\n    return (T, U, V)\n\n\
                     print(g[T=int, V=bool]())\nprint(g[T=int]())\n",
                ),
                &[
                    "print(g[int, _ByTypeDefault, bool]())",
                    "print(g[int, _ByTypeDefault, _ByTypeDefault]())",
                ],
            );
        }

        #[test]
        fn a_variadic_takes_the_run_the_named_parameters_leave() {
            writes(
                &transpiled_at(
                    "def f[*Ts, T]():\n    return (Ts, T)\n\ndef g[T, *Ts, U]():\n    \
                     return (T, Ts, U)\n\nprint(f[int, str, T=bytes]())\nprint(g[int, U=str]())\n",
                    PythonVersion::PY312,
                ),
                &["print(f[int, str, bytes]())", "print(g[int, str]())"],
            );
        }

        /// every keyword is a field of the pack, which only the wrapper can take by name
        #[test]
        fn a_pack_takes_its_keywords_in_the_wrapper_call() {
            writes(
                &transpiled_at(
                    "def f[T, **Kwargs]():\n    return (T, Kwargs)\n\nprint(f[int, T=str]())\n",
                    PythonVersion::PY312,
                ),
                &["print(f.__getitem__(int, T=str)())"],
            );
        }

        /// a single keyword on a method is not a class's to drop, which would have bound the
        /// first type parameter
        #[test]
        fn a_reified_method_is_given_its_keywords_by_position() {
            writes(
                &transpiled(
                    "class D:\n    def m[T = bytes, U = str](self):\n        return (T, U)\n\n\
                     print(D().m[U=int]())\nprint(D().m[U=int, T=str]())\n",
                ),
                &[
                    "print(D().m[_ByTypeDefault, int]())",
                    "print(D().m[str, int]())",
                ],
            );
        }

        /// an override may rename its type parameters, so a name is read off the receiver's
        /// declared class
        #[test]
        fn a_name_binds_against_the_static_receiver() {
            writes(
                &transpiled_at(
                    "class A:\n    def f[T, U](self):\n        return (T, U)\n\n\
                     class C(A):\n    def f[U, T](self):\n        return (U, T)\n\n\
                     def use(a: A):\n    return a.f[U=int, T=str]()\n",
                    PythonVersion::PY312,
                ),
                &["    return a.f[str, int]()"],
            );
        }

        /// an override matches a keyword-only parameter by name, so it is given by name, and one
        /// given nothing is left to the default the wrapper reads by name
        #[test]
        fn a_keyword_only_parameter_is_given_by_name() {
            writes(
                &transpiled(
                    "def f[T, *, B, C = str]():\n    return (T, B, C)\n\n\
                     print(f[C=bytes, B=int, T=str]())\nprint(f[int, B=str]())\n",
                ),
                &[
                    "print(f.__getitem__(str, B=int, C=bytes)())",
                    "print(f.__getitem__(int, B=str)())",
                ],
            );
        }

        #[test]
        fn a_positional_parameter_beside_a_positional_only_one_is_written_at_its_position() {
            writes(
                &transpiled_at(
                    "def f[A, /, B]():\n    return (A, B)\n\nprint(f[int, B=str]())\n",
                    PythonVersion::PY312,
                ),
                &["print(f[int, str]())"],
            );
        }

        #[test]
        fn an_erased_method_drops_its_keywords() {
            writes(
                &transpiled(
                    "class F:\n    def m[T, U](self, t: T, u: U) -> U:\n        return u\n\n\
                     print(F().m[U=int, T=str](\"a\", 1))\n",
                ),
                &["print(F().m(\"a\", 1))"],
            );
        }
    }

    /// a keyword-variadic pack is a `ParamSpec` at runtime, so its fields lower to the
    /// `ParamSpec` list form. needs type info to know the class declares a pack
    mod keyword_pack {
        use crate::Config;

        fn transpiled(source: &str) -> String {
            super::typed_transpile(source, &Config::test_default())
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

        /// a variadic takes the run of positional arguments the named parameters leave
        #[test]
        fn a_parameter_after_a_variadic_named_by_keyword() {
            let output = transpiled("class C[*Ts, T]: ...\n\nx: C[int, str, T=bytes]\n");
            assert!(
                output.contains("x: C[int, str, bytes]"),
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
