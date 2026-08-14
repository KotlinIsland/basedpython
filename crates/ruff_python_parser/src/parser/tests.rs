use ruff_python_ast::helpers::{UseSiteVariance, use_site_variance_marker};
use ruff_python_ast::{
    Expr, InterpolatedStringElement, IpyEscapeKind, ModModule, Number, Operator, Pattern, Stmt,
    TypeParam, UnaryOp, is_destructure_binder,
};
use ruff_text_size::Ranged;

use crate::{
    Mode, ParseError, ParseErrorType, ParseOptions, Parsed, parse, parse_expression, parse_module,
};

/// Parse a module in basedpython mode so tests for `.by`-only syntax don't
/// trigger the `error_if_not_basedpython` parse-error gates.
fn parse_basedpython_module(source: &str) -> Parsed<ModModule> {
    crate::Parser::new(
        source,
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .parse()
    .try_into_module()
    .unwrap()
    .into_result()
    .unwrap()
}

/// Like [`parse_basedpython_module`], but keeps the parse errors so tests can assert on them.
fn parse_basedpython_module_with_errors(source: &str) -> Parsed<ModModule> {
    crate::Parser::new(
        source,
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .parse()
    .try_into_module()
    .unwrap()
}

#[test]
fn test_modes() {
    let source = "a[0][1][2][3][4]";

    assert!(parse(source, ParseOptions::from(Mode::Expression)).is_ok());
    assert!(parse(source, ParseOptions::from(Mode::Module)).is_ok());
}

#[test]
fn basedpython_let_keyword_never_panics() {
    // `let` is only the declaration keyword when shaped like `let NAME =` or
    // `let NAME :`. anything else is an ordinary identifier and must parse
    // without panicking — regression for a `bump(Equal)` assertion that fired
    // when ERA001 parsed a comment such as `# the OS will let us`
    for source in [
        "let us",
        "let",
        "let = 5",
        "let(x)",
        "x = let + 1",
        "for let in items:\n    pass",
        "let x = 5",
        "let x: int = 5",
        "let x: int",
    ] {
        // success here is simply not panicking
        let _ = parse(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        );
    }
}

#[test]
fn basedpython_declares_a_soft_keyword_name() {
    // a soft keyword is a keyword only where it introduces its own construct, so
    // everywhere else it names things like any identifier. typeshed has fields
    // called `type` (`socket.SocketType.type`, `asyncio.TransportSocket.type`),
    // and every declaration form has to accept one
    for source in [
        "let type: int",
        "let match = 1",
        "var case: bytes = b\"\"",
        "final type: int",
        "private match: str",
        "override type: int = 1",
        "sentinel type",
        "context match = 1",
    ] {
        let parsed = parse_basedpython_module(source);
        assert_eq!(parsed.syntax().body.len(), 1, "{source}");
    }

    // the one construct `type` does introduce still wins on its own shape
    let parsed = parse_basedpython_module("private type Alias = int\n");
    let [ruff_python_ast::Stmt::TypeAlias(alias)] = parsed.syntax().body.as_slice() else {
        panic!("expected a type alias");
    };
    assert!(alias.is_private);
}

#[test]
fn basedpython_valueless_typed_let_parses_cleanly() {
    // `let x: T` with no initializer is a read-only declaration; it must parse
    // without error and produce an `AnnAssign` with no value
    let parsed = parse(
        "let x: int",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .expect("valueless typed `let` should parse");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let ruff_python_ast::Mod::Module(module) = parsed.syntax() else {
        panic!("expected a module");
    };
    let [ruff_python_ast::Stmt::AnnAssign(assign)] = module.body.as_slice() else {
        panic!("expected a single AnnAssign statement");
    };
    assert!(assign.value.is_none(), "valueless `let` must have no value");
}

#[test]
fn basedpython_valueless_untyped_let_parses_cleanly() {
    // a bare `let x` with neither type nor initializer is an uninitialized
    // declaration; it must parse without error and produce an `AnnAssign` with
    // no value (the `__let__` marker as a bare annotation)
    let parsed = parse(
        "let x\nx = 1\n",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .expect("valueless untyped `let` should parse");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let ruff_python_ast::Mod::Module(module) = parsed.syntax() else {
        panic!("expected a module");
    };
    let [ruff_python_ast::Stmt::AnnAssign(assign), _] = module.body.as_slice() else {
        panic!("expected an AnnAssign followed by an assignment");
    };
    assert!(assign.value.is_none(), "valueless `let` must have no value");
}

#[test]
fn basedpython_var_declaration_shapes() {
    // `var NAME = v` and `var NAME: T [= v]` are mutable declarations: the
    // keyword is carried by a synthetic marker annotation and the statement
    // means exactly `NAME [: T] = v`
    for (source, has_value) in [
        ("var x = 5", true),
        ("var x: int = 5", true),
        ("var x: int", false),
        ("private var x = 5", true),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(assign)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single AnnAssign for `{source}`");
        };
        assert_eq!(
            assign.value.is_some(),
            has_value,
            "wrong initializer for `{source}`"
        );
        let Expr::Name(target) = assign.target.as_ref() else {
            panic!("expected a name target for `{source}`");
        };
        assert_eq!(target.id.as_str(), "x");
    }
}

#[test]
fn basedpython_bare_var_is_rejected() {
    // unlike `let x` (an uninitialized `Final`), `var x` declares nothing —
    // neither a type nor a value — so it is a parse error rather than a
    // statement that lowers to nothing
    let error = parse(
        "var x\n",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .expect_err("bare `var` must be rejected");
    assert!(
        matches!(
            &error.error,
            ParseErrorType::OtherError(message)
                if message == "`var` declaration requires a type or an initializer"
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn basedpython_var_keyword_never_panics() {
    // as with `let`, `var` is only the declaration keyword in the declaration
    // shapes; every other use is an ordinary identifier and must parse
    for source in [
        "var",
        "var = 5",
        "var: int = 5",
        "var(x)",
        "x = var + 1",
        "var, y = 1, 2",
        "for var in items:\n    pass",
        "var x = 5",
        "var x: int = 5",
    ] {
        // success here is simply not panicking
        let _ = parse(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        );
    }
}

#[test]
fn basedpython_var_as_identifier_is_not_a_declaration() {
    // `var = 5` binds a variable named `var`; the declaration path must not
    // claim it
    let parsed = parse_basedpython_module("var = 5\n");
    let [Stmt::Assign(assign)] = parsed.syntax().body.as_slice() else {
        panic!("expected a plain assignment");
    };
    let [Expr::Name(target)] = assign.targets.as_slice() else {
        panic!("expected a single name target");
    };
    assert_eq!(target.id.as_str(), "var");
}

#[test]
fn basedpython_local_once_param_modifiers() {
    // `local` / `once` before a parameter name are lifetime modifiers that the
    // parser consumes (no AST field). the parameter keeps its real name, and the
    // two may combine in either order (`once local fn`)
    for (source, param_name) in [
        ("def f(local x): ...", "x"),
        ("def f(once fn): ...", "fn"),
        ("def f(local x: int): ...", "x"),
        ("def f(once fn: int): ...", "fn"),
        ("def f(once local fn): ...", "fn"),
        ("def f(local once fn): ...", "fn"),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::FunctionDef(func)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single FunctionDef for `{source}`");
        };
        let param = func.parameters.args.first().expect("one positional param");
        assert_eq!(
            param.parameter.name.as_str(),
            param_name,
            "modifier keywords must be stripped from the name in `{source}`"
        );
    }
}

#[test]
fn basedpython_local_once_as_bare_param_name() {
    // a parameter literally named `local` or `once` (not followed by another
    // name) is an ordinary parameter — the trailing-`Name` guard keeps it from
    // being read as a modifier
    for (source, name) in [
        ("def f(local): ...", "local"),
        ("def f(once): ...", "once"),
        ("def f(local, once): ...", "local"),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::FunctionDef(func)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single FunctionDef for `{source}`");
        };
        assert_eq!(
            func.parameters.args[0].parameter.name.as_str(),
            name,
            "bare `{name}` must stay a parameter name in `{source}`"
        );
    }
}

#[test]
fn basedpython_local_once_param_rejected_in_py() {
    // the modifiers are `.by`-only; a `.py` file using them collects a
    // `BasedPythonOnly` gate error
    for source in ["def f(local x): ...", "def f(once fn): ..."] {
        let parsed = crate::Parser::new(source, ParseOptions::from(Mode::Module))
            .parse()
            .try_into_module()
            .expect("recovers to a module");
        assert!(
            parsed.errors().iter().any(ParseError::is_basedpython_only),
            "expected a BasedPythonOnly error for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn basedpython_asserts_return_annotation() {
    // `-> asserts x` records the keyword on the function and keeps the asserted
    // expression as the annotation
    for (source, asserts) in [
        ("def f(x) -> asserts x: ...", true),
        ("def f(x) -> asserts not x: ...", true),
        ("def f(x) -> asserts x is int: ...", true),
        ("def f(x) -> asserts x is not None: ...", true),
        // a bare `asserts` is the ordinary type named `asserts`
        ("def f(x) -> asserts: ...", false),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::FunctionDef(func)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single FunctionDef for `{source}`");
        };
        assert_eq!(
            func.is_asserts_return, asserts,
            "unexpected assertion guard for `{source}`"
        );
        assert!(
            func.returns.is_some(),
            "the asserted expression is the annotation in `{source}`"
        );
    }
}

#[test]
fn basedpython_asserts_return_rejected_in_py() {
    // the keyword is `.by`-only; a `.py` file using it collects a
    // `BasedPythonOnly` gate error
    let source = "def f(x) -> asserts x: ...";
    let parsed = crate::Parser::new(source, ParseOptions::from(Mode::Module))
        .parse()
        .try_into_module()
        .expect("recovers to a module");
    assert!(
        parsed.errors().iter().any(ParseError::is_basedpython_only),
        "expected a BasedPythonOnly error for `{source}`, got {:?}",
        parsed.errors()
    );
}

#[test]
fn basedpython_local_once_in_callable_type() {
    // `local` / `once` modifiers inside a callable-type parameter list are
    // stripped from the element and recorded positionally against it, whichever
    // of the parameter-list shapes the list took: a lone parenthesized type, a
    // plain tuple, a parameter spec with separators, or named fields
    use ruff_python_ast::ParameterBorrow::{Local, None as NoBorrow, Once};
    for (source, expected) in [
        ("f: (local int) -> None", &[Local][..]),
        ("f: (once fn) -> None", &[Once]),
        // no modifier anywhere stays an empty slice rather than a run of `None`
        ("f: (int, str) -> None", &[]),
        ("f: (local list[int], once str) -> bool", &[Local, Once]),
        ("f: (int, local str) -> None", &[NoBorrow, Local]),
        (
            "f: (int, local str, /, once bool) -> None",
            &[NoBorrow, Local, Once],
        ),
        ("f: (local int, /, str) -> None", &[Local]),
        // named parameters carry the modifier before the name
        ("f: (local resource: Resource) -> None", &[Local]),
        ("f: (a: int, once cb: Callback) -> None", &[NoBorrow, Once]),
        // `once` implies the borrow, so it wins over a `local` written with it
        ("f: (once local fn) -> None", &[Once]),
        ("f: (local once fn) -> None", &[Once]),
        // an implicit receiver is not one of `args`, so the modifiers still
        // line up with the written parameters
        ("f: int.(local str) -> None", &[Local]),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(assign)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single AnnAssign for `{source}`");
        };
        let Expr::CallableType(callable) = assign.annotation.as_ref() else {
            panic!("expected a callable type for `{source}`");
        };
        let borrows = callable
            .callable_shape
            .as_ref()
            .map(|shape| shape.borrows.as_ref())
            .unwrap_or_default();
        assert_eq!(borrows, expected, "recorded borrows for `{source}`");
        assert!(
            borrows.len() <= callable.args.len(),
            "borrows are recorded positionally, so they cannot outnumber the \
             parameters in `{source}`"
        );
    }
}

#[test]
fn basedpython_local_once_in_callable_type_rejected_in_py() {
    // the modifiers are `.by`-only here too
    let source = "f: (local int) -> None";
    let parsed = crate::Parser::new(source, ParseOptions::from(Mode::Module))
        .parse()
        .try_into_module()
        .expect("recovers to a module");
    assert!(
        parsed.errors().iter().any(ParseError::is_basedpython_only),
        "expected a BasedPythonOnly error for `{source}`, got {:?}",
        parsed.errors()
    );
}

#[test]
fn basedpython_receiver_callable_type() {
    // `T.(...) -> R` records the receiver alongside the ordinary parameter list
    for (source, receiver, arg_count) in [
        ("f: int.() -> str", "int", 0usize),
        ("f: list[int].(str) -> bool", "list[int]", 1),
        ("f: int.(str, /, name: bytes) -> None", "int", 2),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(assign)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single AnnAssign for `{source}`");
        };
        let Expr::CallableType(callable) = assign.annotation.as_ref() else {
            panic!("expected a callable type for `{source}`");
        };
        let spelled = callable
            .receiver
            .as_ref()
            .map(|receiver| &source[ruff_text_size::Ranged::range(receiver.as_ref())])
            .expect("the receiver should be recorded");
        assert_eq!(spelled, receiver, "receiver for `{source}`");
        assert_eq!(callable.args.len(), arg_count, "arg count for `{source}`");
    }
}

#[test]
fn basedpython_receiver_callable_rejected_in_py() {
    // `.` followed by `(` is never valid python, so the form is `.by`-only
    let parsed = crate::Parser::new("f: int.() -> str", ParseOptions::from(Mode::Module))
        .parse()
        .try_into_module()
        .expect("recovers to a module");
    assert!(
        parsed.errors().iter().any(ParseError::is_basedpython_only),
        "expected a BasedPythonOnly error, got {:?}",
        parsed.errors()
    );
}

#[test]
fn basedpython_local_in_callable_rejected_in_py() {
    // the modifier is `.by`-only inside a callable type too
    let parsed = crate::Parser::new("f: (local int) -> None", ParseOptions::from(Mode::Module))
        .parse()
        .try_into_module()
        .expect("recovers to a module");
    assert!(
        parsed.errors().iter().any(ParseError::is_basedpython_only),
        "expected a BasedPythonOnly error, got {:?}",
        parsed.errors()
    );
}

#[test]
fn basedpython_decorated_protocol_keyword() {
    // a decorator before the `protocol` introducer (e.g. `@runtime_checkable
    // protocol P:`) must route through the protocol parser, carrying the
    // decorator, rather than erroring with "expected class after decorator"
    let parsed = parse(
        "@runtime_checkable\nprotocol P:\n    def m(self) -> int: ...\n",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .expect("decorated `protocol` should parse");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let ruff_python_ast::Mod::Module(module) = parsed.syntax() else {
        panic!("expected a module");
    };
    let [ruff_python_ast::Stmt::ClassDef(class)] = module.body.as_slice() else {
        panic!("expected a single ClassDef");
    };
    // the real decorator plus the synthetic `protocol_class` marker
    let names: Vec<&str> = class
        .decorator_list
        .iter()
        .filter_map(|d| d.expression.as_name_expr().map(|n| n.id.as_str()))
        .collect();
    assert_eq!(names, ["runtime_checkable", "protocol_class"]);
}

#[test]
fn basedpython_final_in_modifier_chain_keeps_final_marker() {
    // `final` combined with another modifier must still carry the `__final__`
    // marker (a `__final__[T]` subscript) so ty applies `Final`, rather than the
    // no-op `__modifier_annot__` that drops the qualifier
    let parsed = parse_basedpython_module("final override x: int\n");
    let [Stmt::AnnAssign(assign)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single AnnAssign");
    };
    let Expr::Subscript(sub) = assign.annotation.as_ref() else {
        panic!("expected a `__final__[T]` subscript annotation");
    };
    assert!(matches!(sub.value.as_ref(), Expr::Name(n) if n.id == "__final__"));
}

#[test]
fn basedpython_paramspec_arrow_params_parse() {
    // a bare `**P` in an arrow parameter list encodes as `Starred(Starred(Name))`
    // (a `ParamSpec`) — `(**P)` and the Concatenate `(T, **P)` both parse cleanly
    // rather than dropping `**P` or erroring
    for source in ["f: (**P) -> int\n", "g: (str, int, **P) -> bool\n"] {
        let parsed = parse(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        )
        .expect("should parse");
        assert!(
            parsed.errors().is_empty(),
            "unexpected parse errors in {source:?}: {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn basedpython_unpacked_variadic_arrow_params_parse() {
    // a starred element in an arrow parameter list unpacks a variadic type. the lone
    // `(*Ts)` group is the interesting one: `(*x)` is otherwise rejected, and the arrow
    // is only seen after the group has been parsed
    for source in [
        "f: (*Ts) -> int\n",
        "g: (int, *Ts) -> int\n",
        "h: (*args: *Ts) -> int\n",
    ] {
        let parsed = parse(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        )
        .expect("should parse");
        assert!(
            parsed.errors().is_empty(),
            "unexpected parse errors in {source:?}: {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn basedpython_lone_starred_group_without_arrow_still_errors() {
    let parsed = parse(
        "x = (*a)\n",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    );
    assert!(
        parsed.is_err(),
        "a parenthesized starred expression is only valid as a callable's parameter list"
    );
}

#[test]
fn basedpython_extension_parses_to_marked_class() {
    let parsed = parse_basedpython_module(
        "extension list:\n    def second(self) -> Element:\n        return self[1]\n",
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single ClassDef");
    };
    assert_eq!(class.name.as_str(), "list");
    assert!(class.type_params.is_none());
    assert!(class.arguments.is_none());
    let names: Vec<&str> = class
        .decorator_list
        .iter()
        .filter_map(|d| d.expression.as_name_expr().map(|n| n.id.as_str()))
        .collect();
    assert_eq!(names, ["extension_def"]);
    assert!(matches!(class.body.as_slice(), [Stmt::FunctionDef(f)] if f.name.as_str() == "second"));
}

#[test]
fn basedpython_extension_with_bounds_parses_type_params() {
    let parsed = parse_basedpython_module(
        "extension list[Element: int]:\n    def total(self) -> int:\n        return sum(self)\n",
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single ClassDef");
    };
    let type_params = class.type_params.as_ref().expect("expected type params");
    assert_eq!(type_params.type_params.len(), 1);
    let param = type_params.type_params[0]
        .as_type_var()
        .expect("expected a plain TypeVar param");
    assert_eq!(param.name.as_str(), "Element");
    assert!(param.bound.is_some());
}

#[test]
fn basedpython_extension_conformances_parse_as_bases() {
    // the conformance list rides where a class's bases do, so the extension
    // literal derives the interfaces it declares conformance to
    let parsed = parse_basedpython_module(
        "extension list[Element: int](Show, Size):\n    override def show(self): ...\n",
    );
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single ClassDef");
    };
    assert_eq!(class.name.as_str(), "list");
    assert!(class.is_extension());
    assert!(class.type_params.is_some());
    let bases: Vec<&str> = class
        .bases()
        .iter()
        .filter_map(|base| base.as_name_expr().map(|name| name.id.as_str()))
        .collect();
    assert_eq!(bases, ["Show", "Size"]);
}

#[test]
fn basedpython_extension_conformance_list_rejects_keywords_and_unpacking() {
    // a conformance list names interfaces; a keyword has no meaning there, and
    // an unpacking cannot be resolved to the interfaces to register under
    for (source, expected) in [
        (
            "extension str(metaclass=type):\n    def f(self): ...\n",
            "not keyword arguments",
        ),
        (
            "extension str(*bases):\n    def f(self): ...\n",
            "cannot be unpacked",
        ),
    ] {
        let parsed = crate::Parser::new(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        )
        .parse()
        .try_into_module()
        .unwrap();
        assert!(
            parsed
                .errors()
                .iter()
                .any(|e| e.to_string().contains(expected)),
            "expected {expected:?} for {source:?}, got: {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn basedpython_extension_soft_keyword_stays_a_name() {
    // `extension` only introduces a declaration when followed by a name.
    // every other shape must remain an ordinary identifier
    for source in [
        "extension = 1",
        "extension(x)",
        "extension[0]",
        "print(extension)",
        "extension.field = 2",
        "extension: int = 3",
    ] {
        let parsed = parse_basedpython_module(source);
        assert!(
            parsed.errors().is_empty(),
            "unexpected parse errors in {source:?}: {:?}",
            parsed.errors()
        );
        assert!(
            !matches!(parsed.syntax().body.as_slice(), [Stmt::ClassDef(_)]),
            "{source:?} must not parse as an extension declaration"
        );
    }
}

#[test]
fn extension_rejected_in_py_file() {
    // the `extension` introducer is basedpython-only
    let has_error = match parse(
        "extension list:\n    def second(self): ...\n",
        ParseOptions::from(Mode::Module),
    ) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "`extension` must be rejected in a .py file");
}

#[test]
fn final_annotation_rejected_in_py_file() {
    // `final x: T` is basedpython-only; a plain `.py` parse must report the gate
    let has_error = match parse("final x: int\n", ParseOptions::from(Mode::Module)) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(
        has_error,
        "`final` annotation must be rejected in a .py file"
    );
}

#[test]
fn decorated_protocol_rejected_in_py_file() {
    // the `protocol` introducer is basedpython-only
    let has_error = match parse(
        "@runtime_checkable\nprotocol P: ...\n",
        ParseOptions::from(Mode::Module),
    ) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "`protocol` must be rejected in a .py file");
}

#[test]
fn test_expr_mode_invalid_syntax1() {
    let source = "first second";
    let error = parse_expression(source).unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_expr_mode_invalid_syntax2() {
    let source = r"first

second
";
    let error = parse_expression(source).unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_expr_mode_invalid_syntax3() {
    let source = r"first

second

third
";
    let error = parse_expression(source).unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_expr_mode_valid_syntax() {
    let source = "first

";
    let parsed = parse_expression(source).unwrap();

    insta::assert_debug_snapshot!(parsed.expr());
}

#[test]
fn test_unicode_aliases() {
    // https://github.com/RustPython/RustPython/issues/4566
    let source = r#"x = "\N{BACKSPACE}another cool trick""#;
    let suite = parse_module(source).unwrap().into_suite();

    insta::assert_debug_snapshot!(suite);
}

#[test]
fn nfkc_normalizes_names() {
    let parsed = parse_expression("𝒞").unwrap();
    let Expr::Name(name) = parsed.expr() else {
        panic!("expected name expression, got {:?}", parsed.expr());
    };

    assert_eq!(name.id.as_str(), "C");
}

#[test]
fn nfkc_normalizes_dotted_names() {
    let suite = parse_module("import 𝒞.𝒟").unwrap().into_suite();
    let [Stmt::Import(import)] = suite.as_slice() else {
        panic!("expected a single import statement, got {suite:?}");
    };
    let [alias] = import.names.as_slice() else {
        panic!("expected a single import alias, got {:?}", import.names);
    };

    assert_eq!(alias.name.id.as_str(), "C.D");
}

#[test]
fn number_values() {
    let cases = [
        ("1E400", Number::Float(f64::INFINITY)),
        (
            "1E400J",
            Number::Complex {
                real: 0.0,
                imag: f64::INFINITY,
            },
        ),
        (
            "123_456_789_123_456_789_123_456_789_123_456_789",
            Number::Int("123456789123456789123456789123456789".parse().unwrap()),
        ),
        (
            "000_123_456_789_123_456_789_123_456_789_123_456_789J",
            Number::Complex {
                real: 0.0,
                imag: 1.234_567_891_234_567_8e35,
            },
        ),
    ];

    for (source, expected) in cases {
        let parsed = parse_expression(source).unwrap();
        let Expr::NumberLiteral(number) = parsed.expr() else {
            panic!(
                "expected number expression for {source:?}, got {:?}",
                parsed.expr()
            );
        };

        assert_eq!(number.value, expected, "source: {source:?}");
    }
}

#[test]
fn malformed_radix_literals() {
    for source in ["0x", "0o", "0b", "0x_", "0x__1"] {
        assert!(parse_expression(source).is_err(), "source: {source:?}");
    }
}

#[test]
fn interpolated_string_escaped_brace_values() {
    let cases = [
        (r"f'\{{1}}'", r"\{1}"),
        (r"f'\}}'", r"\}"),
        (r"f'\\{{1}}'", r"\{1}"),
        (r"f'\\\{{1}}'", r"\\{1}"),
        (r"t'\{{1}}'", r"\{1}"),
        (r"t'\}}'", r"\}"),
        (r"t'\\{{1}}'", r"\{1}"),
        (r"t'\\\{{1}}'", r"\\{1}"),
        (r"rf'\{{1}}'", r"\{1}"),
        (r"rt'\{{1}}'", r"\{1}"),
    ];

    for (source, expected) in cases {
        let parsed = parse_expression(source).unwrap();
        let elements = match parsed.expr() {
            Expr::FString(string) => &string.as_single_part_fstring().unwrap().elements,
            Expr::TString(string) => &string.as_single_part_tstring().unwrap().elements,
            expression => panic!("expected interpolated string for {source:?}, got {expression:?}"),
        };
        let [InterpolatedStringElement::Literal(literal)] = &**elements else {
            panic!("expected one literal element for {source:?}");
        };

        assert_eq!(&*literal.value, expected, "source: {source:?}");
    }
}

#[test]
fn ipython_escape_command_values() {
    let cases = [
        ("?foo?", IpyEscapeKind::Help, "foo"),
        ("??   foo?", IpyEscapeKind::Help, "foo"),
        ("??   foo  ?", IpyEscapeKind::Help2, "   foo  ?"),
        ("?foo??", IpyEscapeKind::Help2, "foo"),
        ("%foo?", IpyEscapeKind::Help, "%foo"),
        ("%foo??", IpyEscapeKind::Help2, "%foo"),
        ("%%foo???", IpyEscapeKind::Magic2, "foo???"),
        ("!pwd?", IpyEscapeKind::Shell, "pwd?"),
        ("?? \\\n    foo?", IpyEscapeKind::Help, "foo"),
        ("?? \\\r    foo?", IpyEscapeKind::Help, "foo"),
        ("?? \\\r\n    foo?", IpyEscapeKind::Help, "foo"),
    ];

    for (source, expected_kind, expected_value) in cases {
        let suite = parse(source, ParseOptions::from(Mode::Ipython))
            .unwrap()
            .try_into_module()
            .unwrap()
            .into_suite();
        let [Stmt::IpyEscapeCommand(command)] = suite.as_slice() else {
            panic!("expected one IPython escape command for {source:?}, got {suite:?}");
        };

        assert_eq!(command.kind, expected_kind, "source: {source:?}");
        assert_eq!(&*command.value, expected_value, "source: {source:?}");
    }
}

#[test]
fn ipython_escape_command_expression_values() {
    let cases = [
        ("x = !!foo", IpyEscapeKind::Shell, "!foo"),
        ("x = %%foo", IpyEscapeKind::Magic, "%foo"),
    ];

    for (source, expected_kind, expected_value) in cases {
        let suite = parse(source, ParseOptions::from(Mode::Ipython))
            .unwrap()
            .try_into_module()
            .unwrap()
            .into_suite();
        let [Stmt::Assign(assign)] = suite.as_slice() else {
            panic!("expected one assignment for {source:?}, got {suite:?}");
        };
        let Expr::IpyEscapeCommand(command) = assign.value.as_ref() else {
            panic!(
                "expected an IPython escape command for {source:?}, got {:?}",
                assign.value
            );
        };

        assert_eq!(command.kind, expected_kind, "source: {source:?}");
        assert_eq!(&*command.value, expected_value, "source: {source:?}");
    }
}

#[test]
fn test_ipython_escape_commands() {
    let parsed = parse(
        r"
# Normal Python code
(
    a
    %
    b
)

# Dynamic object info
??a.foo
?a.foo
?a.foo?
??a.foo()??

# Line magic
%timeit a = b
%timeit foo(b) % 3
%alias showPath pwd && ls -a
%timeit a =\
  foo(b); b = 2
%matplotlib --inline
%matplotlib \
    --inline

# System shell access
!pwd && ls -a | sed 's/^/\    /'
!pwd \
  && ls -a | sed 's/^/\\    /'
!!cd /Users/foo/Library/Application\ Support/

# Let's add some Python code to make sure that earlier escapes were handled
# correctly and that we didn't consume any of the following code as a result
# of the escapes.
def foo():
    return (
        a
        !=
        b
    )

# Transforms into `foo(..)`
/foo 1 2
;foo 1 2
,foo 1 2

# Indented escape commands
for a in range(5):
    !ls

p1 = !pwd
p2: str = !pwd
foo = %foo \
    bar
bar = %foo?
baz = !pwd?

% foo
foo = %foo  # comment

# Help end line magics
foo?
foo.bar??
foo.bar.baz?
foo[0]??
foo[0][1]?
foo.bar[0].baz[1]??
foo.bar[0].baz[2].egg??
"
        .trim(),
        ParseOptions::from(Mode::Ipython),
    )
    .unwrap();
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_fstring_expr_inner_line_continuation_and_t_string() {
    let source = r#"f'{\t"i}'"#;

    let parsed = parse_expression(source);

    let error = parsed.unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_fstring_expr_inner_line_continuation_newline_t_string() {
    let source = r#"f'{\
t"i}'"#;

    let parsed = parse_expression(source);

    let error = parsed.unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_tstring_fstring_middle() {
    let source = "t'{:{F'{\0}F";
    let parsed = parse_expression(source);

    let error = parsed.unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_tstring_fstring_middle_fuzzer() {
    let source = "A1[A\u{c}\0:+,>1t'{:f\0:{f\"f\0:\0{fm\0:{f:\u{10}\0\0\0:bb\0{@f>f\u{1}'\0f";
    let parsed = parse_expression(source);

    let error = parsed.unwrap_err();

    insta::assert_debug_snapshot!(error);
}

#[test]
fn test_anon_named_tuple_alias() {
    let source = "a = (name: str, age: int)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_decorated_modifier_method() {
    // `@overload class def open(...)` (a real decorator before a `class def` /
    // `static def` modifier) must parse — the real decorator is kept and the
    // modifier becomes a synthetic decorator, so each method carries two. this
    // is the `tarfile.open` shape that previously failed to parse.
    let parsed = parse_basedpython_module(
        "\
class C:
    @overload
    class def open(cls, x: int) -> int: ...
    @overload
    static def make(x: str) -> str: ...
",
    );
    let Some(Stmt::ClassDef(class)) = parsed.syntax().body.first() else {
        panic!("expected a class definition");
    };
    let methods: Vec<_> = class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(func) => Some(func),
            _ => None,
        })
        .collect();
    assert_eq!(methods.len(), 2);
    for method in methods {
        // `@overload` plus the synthetic modifier decorator (`classmethod` /
        // `static`)
        assert_eq!(method.decorator_list.len(), 2);
    }
}

#[test]
fn test_modifier_before_classmethod() {
    // a modifier keyword before the `class def` classmethod modifier — e.g.
    // `override class def __prepare__(...)`. the leading `class` must be
    // recognised as the classmethod modifier, not as a class definition (which
    // would parse `def` as the class name and drop the method)
    let parsed = parse_basedpython_module(
        "\
class C:
    override class def f(cls) -> int: ...
    final class def g(cls) -> int: ...
",
    );
    let Some(Stmt::ClassDef(class)) = parsed.syntax().body.first() else {
        panic!("expected a class definition");
    };
    let methods: Vec<_> = class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(func) => Some(func),
            _ => None,
        })
        .collect();
    assert_eq!(
        methods.len(),
        2,
        "both classmethods must parse as functions"
    );
    assert_eq!(methods[0].name.as_str(), "f");
    assert_eq!(methods[1].name.as_str(), "g");
    for method in methods {
        // the outer modifier (`override` / `final`) plus the synthetic
        // `classmethod` decorator
        assert_eq!(method.decorator_list.len(), 2);
    }
}

/// `frozen` qualifies `data`, so it consumes a second keyword only when that
/// keyword is `data`. consuming whatever followed swallowed a neighbouring
/// modifier — `frozen final class` silently lost its `final` — and asserted on a
/// chain that had no second keyword at all
#[test]
fn basedpython_frozen_needs_its_data() {
    for source in [
        "frozen def f(): ...\n",
        "frozen class A: ...\n",
        "frozen async def f(): ...\n",
        "frozen final class A: ...\n",
        "frozen protocol P: ...\n",
        "class A:\n    frozen def m(self): ...\n",
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed.errors().iter().any(|error| error
                .to_string()
                .contains("`frozen` qualifies `data class`")),
            "expected the `frozen` error for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

/// `frozen data`, and the chains around it, still parse cleanly
#[test]
fn basedpython_frozen_data_still_parses() {
    for source in [
        "frozen data class A:\n    x: int\n",
        "final frozen data class A:\n    x: int\n",
        "open frozen data class A:\n    x: int\n",
        "private frozen data class A:\n    x: int\n",
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed.errors().is_empty(),
            "unexpected parse errors for `{source}`: {:?}",
            parsed.errors()
        );
    }
}

/// a modifier that reads on one definition kind decides nothing on the other.
/// it used to parse clean and be carried into a lowering with no arm for it,
/// which left the keyword in the emitted python
#[test]
fn basedpython_a_modifier_names_its_definition_kind() {
    for (source, expected) in [
        (
            "sealed def f(): ...\n",
            "`sealed` is not a modifier on a `def`",
        ),
        (
            "class A:\n    sealed def m(self): ...\n",
            "`sealed` is not a modifier on a `def`",
        ),
        ("data def f(): ...\n", "`data` is not a modifier on a `def`"),
        (
            "frozen data def f(): ...\n",
            "`frozen data` is not a modifier on a `def`",
        ),
        (
            "override class A: ...\n",
            "`override` is not a modifier on a `class`",
        ),
        (
            "static class A: ...\n",
            "`static` is not a modifier on a `class`",
        ),
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed
                .errors()
                .iter()
                .any(|error| error.to_string().contains(expected)),
            "expected `{expected}` for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

/// each modifier on the definition kind it is documented for stays clean,
/// including the ones that read on both
#[test]
fn basedpython_a_modifier_on_its_own_definition_kind_parses() {
    for source in [
        "sealed class A: ...\n",
        "data class A:\n    x: int\n",
        "class A:\n    override def m(self): ...\n",
        "class A:\n    static def m(): ...\n",
        "class A:\n    class def m(cls): ...\n",
        "final class A: ...\n",
        "class A:\n    final def m(self): ...\n",
        "abstract class A: ...\n",
        "class A:\n    abstract def m(self): ...\n",
        "private class A: ...\n",
        "private def f(): ...\n",
        "export def f(): ...\n",
        "private protocol P: ...\n",
        "private enum class E:\n    case A\n",
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed.errors().is_empty(),
            "unexpected parse errors for `{source}`: {:?}",
            parsed.errors()
        );
    }
}

/// `late` prefixes a property's `var`; on a `def` or a `class` it modifies
/// nothing, and used to reach an `unreachable!` in the modifier chain
#[test]
fn basedpython_late_is_not_a_definition_modifier() {
    for source in [
        "late def f(): ...\n",
        "late class A: ...\n",
        "late async def f(): ...\n",
        "class A:\n    late def m(self): ...\n",
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed.errors().iter().any(|error| {
                error
                    .to_string()
                    .contains("`late` is not a modifier on a `def` or a `class`")
            }),
            "expected the `late` error for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

/// `late var x: T` is the shape the keyword exists for, and stays valid
#[test]
fn basedpython_late_var_still_parses() {
    let parsed = parse_basedpython_module_with_errors("class C:\n    late var x: int\n");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
}

#[test]
fn test_modifier_async_def() {
    // a modifier keyword on an `async def` — e.g. `contextlib`'s
    // `abstract async def __aexit__(...)`. previously the modifier was parsed as
    // a bare name and `async def` as a separate compound statement.
    let parsed = parse_basedpython_module(
        "\
class C:
    abstract async def f(self) -> int: ...
    final async def g(self) -> int: ...
",
    );
    let Some(Stmt::ClassDef(class)) = parsed.syntax().body.first() else {
        panic!("expected a class definition");
    };
    let methods: Vec<_> = class
        .body
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(func) => Some(func),
            _ => None,
        })
        .collect();
    assert_eq!(methods.len(), 2);
    for method in methods {
        assert!(method.is_async, "the modifier must apply to an async def");
        // the modifier becomes one synthetic decorator
        assert_eq!(method.decorator_list.len(), 1);
    }
}

#[test]
fn test_anon_named_tuple_in_annotation() {
    let source = "a: (name: str, age: int)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_function_signature() {
    let source = "def f(x: (name: str, age: int)) -> (name: str, age: int): pass\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_single_field() {
    let source = "a: (name: str)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_trailing_comma() {
    let source = "a: (name: str, age: int,)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_value_construction() {
    let source = "a = (name=\"asdf\", age=20)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_value_complex_value() {
    let source = "a = (name=foo() + 1, age=x.y)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_value_trailing_comma() {
    let source = "a = (name=\"asdf\", age=20,)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_mixed_value() {
    let source = "a = (1, name=\"a\")\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_anon_named_tuple_mixed_type() {
    let source = "a: (int, name: str)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_inline_protocol_type() {
    let source = "a: protocol(a: int; b: str; def f(self) -> int)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_inline_protocol_keyword_pack() {
    let source = "a: protocol(**Kwargs)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_inline_protocol_method_parameter_spec() {
    let source = "a: protocol(def f(self, x: int, /, *args: str, **kw: int) -> str | None)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_inline_protocol_multiline_trailing_semicolon() {
    let source = "a: protocol(\n    a: int;\n    def f(self) -> None;\n)\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

/// `protocol` is a soft keyword — a call to something named `protocol` still parses as a call.
#[test]
fn test_protocol_call_is_not_inline_protocol() {
    let source = "a = protocol(x)\nb = protocol()\nc = protocol(y := 1)\nd = protocol(z[1:2])\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript() {
    let source = "a: list[*]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_attribute() {
    let source = "a: collections.abc.Mapping[*]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_multi() {
    let source = "a: dict[*, *]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_triple() {
    let source = "a: X[*, *, *]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_mixed_str_then_star() {
    let source = "a: dict[str, *]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_mixed_star_then_int() {
    let source = "a: dict[*, int]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_mixed_middle_star() {
    let source = "a: X[int, *, str]\n";
    let parsed = parse_basedpython_module(source);
    insta::assert_debug_snapshot!(parsed.syntax());
}

#[test]
fn test_top_star_subscript_in_py_errors() {
    let parsed = crate::parse_unchecked("a: list[*]\n", ParseOptions::from(Mode::Module));
    let errors: Vec<_> = parsed.errors().iter().map(ToString::to_string).collect();
    assert!(
        errors.iter().any(|e| e.contains("bare `*`")),
        "expected parse error mentioning bare `*`, got: {errors:?}"
    );
}

/// The elements of the subscript annotation in `a: X[...]`.
fn annotation_slice_elements(module: &ModModule) -> &[Expr] {
    let [Stmt::AnnAssign(assign)] = module.body.as_slice() else {
        panic!("expected a single annotated assignment");
    };
    let Expr::Subscript(subscript) = assign.annotation.as_ref() else {
        panic!("expected a subscript annotation");
    };
    match subscript.slice.as_ref() {
        Expr::Tuple(tuple) if !tuple.parenthesized => &tuple.elts,
        single => std::slice::from_ref(single),
    }
}

/// The use-site variance of each element of the subscript annotation in
/// `a: X[...]`, with `None` where an element carries no variance keyword.
fn annotation_slice_variances(module: &ModModule) -> Vec<Option<UseSiteVariance>> {
    annotation_slice_elements(module)
        .iter()
        .map(|element| use_site_variance_marker(element).map(|(variance, _)| variance))
        .collect()
}

/// basedpython: every slice element takes a use-site variance keyword, not just
/// the first. `in` is a hard keyword and so never starts an expression, which
/// made it invisible to the comma-separated element path that later elements go
/// through, while `out` (a name) sailed past it.
#[test]
fn basedpython_use_site_variance_in_any_slice_element() {
    for (source, expected) in [
        ("a: dict[in str]\n", vec![Some(UseSiteVariance::In)]),
        (
            "a: dict[out int, in str]\n",
            vec![Some(UseSiteVariance::Out), Some(UseSiteVariance::In)],
        ),
        (
            "a: dict[int, in str]\n",
            vec![None, Some(UseSiteVariance::In)],
        ),
        (
            "a: X[in int, in out str, out bytes, int]\n",
            vec![
                Some(UseSiteVariance::In),
                Some(UseSiteVariance::InOut),
                Some(UseSiteVariance::Out),
                None,
            ],
        ),
    ] {
        let parsed = parse_basedpython_module(source);
        assert_eq!(
            annotation_slice_variances(parsed.syntax()),
            expected,
            "unexpected variances for `{source}`"
        );
    }
}

/// basedpython: a later element's marker range covers exactly the variance
/// keywords, the way the first element's does — the formatter reprints the
/// keywords from that range.
#[test]
fn basedpython_use_site_variance_marker_range_covers_the_keywords() {
    let source = "a: X[in out int, in str, out bytes]\n";
    let parsed = parse_basedpython_module(source);
    let keywords: Vec<&str> = annotation_slice_elements(parsed.syntax())
        .iter()
        .map(|element| {
            let Expr::Subscript(marker) = element else {
                panic!("expected a variance marker subscript");
            };
            &source[marker.value.range()]
        })
        .collect();
    assert_eq!(keywords, ["in out", "in", "out"]);
}

/// basedpython: a variance keyword is rejected in `.py` files wherever it
/// appears, not only in the first slice element.
#[test]
fn basedpython_use_site_variance_rejected_in_py() {
    for source in ["a: dict[in str]\n", "a: dict[int, in str]\n"] {
        let parsed = crate::parse_unchecked(source, ParseOptions::from(Mode::Module));
        let errors: Vec<_> = parsed.errors().iter().map(ToString::to_string).collect();
        assert!(
            errors
                .iter()
                .any(|error| error.contains("use-site variance keywords")),
            "expected a use-site variance error for `{source}`, got: {errors:?}"
        );
    }
}

#[test]
fn glued_circumflex_before_unary_is_xor_in_py() {
    // `a^-b`, `a^+b`, `a^~b` are valid standard Python — `a ^ (-b)` and friends.
    // basedpython reads a glued `^` before a unary sign as the postfix propagate
    // operator (`(a^) - b`), but that disambiguation must stay off in `.py`
    // mode: stealing it turns valid python into a parse error, which the
    // formatter ecosystem check (it parses `.py` with basedpython disabled)
    // counts as a syntax error and trips over.
    for source in ["a^-b\n", "a^+b\n", "a^~b\n"] {
        let parsed = crate::parse_unchecked(source, ParseOptions::from(Mode::Module));
        assert!(
            parsed.errors().is_empty(),
            "expected {source:?} to parse cleanly in .py mode, got: {:?}",
            parsed.errors()
        );
        let module = parsed.try_into_module().unwrap();
        let Some(Stmt::Expr(stmt)) = module.suite().first() else {
            panic!("expected an expression statement for {source:?}");
        };
        let Expr::BinOp(binop) = &*stmt.value else {
            panic!(
                "expected `a ^ <unary>` for {source:?}, got {:?}",
                stmt.value
            );
        };
        assert_eq!(binop.op, Operator::BitXor, "operator for {source:?}");
        assert!(
            matches!(&*binop.right, Expr::UnaryOp(_)),
            "rhs of {source:?} should be the unary operand, got {:?}",
            binop.right
        );
    }

    // the same glued source keeps its basedpython meaning in `.by`: a postfix
    // propagate followed by a binary subtract, i.e. `(a^) - b`
    let parsed = parse_basedpython_module("a^-b\n");
    let Some(Stmt::Expr(stmt)) = parsed.syntax().body.first() else {
        panic!("expected an expression statement");
    };
    let Expr::BinOp(binop) = &*stmt.value else {
        panic!("expected a binary op, got {:?}", stmt.value);
    };
    assert_eq!(binop.op, Operator::Sub);
    assert!(
        matches!(&*binop.left, Expr::UnaryOp(unary) if unary.op == UnaryOp::Propagate),
        "lhs should be the postfix propagate, got {:?}",
        binop.left
    );
}

#[test]
fn fstring_conversion_after_ternary_is_not_force_unwrap() {
    // the `!s` / `!r` in an interpolation is the conversion flag, not the
    // basedpython postfix force-unwrap. for a conditional the conversion lands
    // on the `else` tail, which must inherit the interpolation context so the
    // `!` isn't eaten as force-unwrap. regression: poetry's
    // `f"{p if not W else g(p)!s}"` became a spurious `.py` syntax error and
    // tipped the formatter ecosystem check over its allowed error count.
    for source in [
        r#"f"{a if b else c!s}""#,
        r#"f"{a if b else c!r:>{w}}""#,
        r#"f"{a if b else c if d else e!s}""#,
        r#"f"{x!r}""#,
        r#"f"{a + b!s}""#,
    ] {
        let parsed = crate::parse_unchecked(source, ParseOptions::from(Mode::Module));
        assert!(
            parsed.errors().is_empty(),
            "expected {source:?} to parse cleanly in .py mode, got: {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn recursion_limit_nested_parens() {
    let src = format!("{}1{}", "(".repeat(1_000), ")".repeat(1_000));
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_nested_receiver_callables() {
    // basedpython `T.(...) -> R` opens a bracket and recurses through its
    // return type, so it needs the same depth guard as a call or subscript
    let src = format!("f: {}int{} -> str", "a.(".repeat(1_000), ")".repeat(1_000));
    let opts = ParseOptions::from(ruff_python_ast::PySourceType::BasedPython)
        .with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_normal_python_unaffected() {
    // 50 levels is well above what real-world Python ever produces and well
    // below the default cap — the point is to confirm the default doesn't
    // reject ordinary input.
    let src = format!("x = {}1{}", "(".repeat(50), ")".repeat(50));
    parse_module(&src).unwrap();
}

#[test]
fn recursion_limit_preserves_prior_statements() {
    // Recursion-limit recovery is limited for now: we drain the rest of the file but keep the
    // statements parsed before the overflowing statement.
    // TODO: Recover at the next newline so the trailing statement is preserved too.
    let src = format!(
        "before = 1\n{}1{}\nafter = 2\n",
        "(".repeat(1_000),
        ")".repeat(1_000),
    );
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let parsed = crate::parse_unchecked(&src, opts)
        .try_into_module()
        .unwrap();

    assert!(matches!(
        parsed.errors().first().map(|error| &error.error),
        Some(ParseErrorType::RecursionLimitExceeded)
    ));
    assert!(matches!(parsed.suite().first(), Some(Stmt::Assign(_))));
}

#[test]
fn recursion_limit_nested_def_blocks() {
    // Nested function definitions exercise instrumentation on
    // `parse_statement` rather than `parse_lhs_expression`. Each level
    // needs one more leading tab to make indentation valid.
    let depth = 400;
    let mut src = String::new();
    for i in 0..depth {
        src.push_str(&"\t".repeat(i));
        src.push_str("def f():\n");
    }
    src.push_str(&"\t".repeat(depth));
    src.push_str("pass\n");
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_nested_lists() {
    let src = format!("{}1{}", "[".repeat(1_000), "]".repeat(1_000));
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_nested_calls() {
    let src = format!("x = {}1{}", "f(".repeat(1_000), ")".repeat(1_000));
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_nested_subscripts() {
    let src = format!("x = {}1{}", "a[".repeat(1_000), "]".repeat(1_000));
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_nested_match_patterns() {
    // Deeply parenthesised match patterns — exercises pattern-parsing
    // instrumentation in addition to statement / expression paths.
    let mut src = String::from("match x:\n case ");
    for _ in 0..600 {
        src.push('(');
    }
    src.push('y');
    for _ in 0..600 {
        src.push(')');
    }
    src.push_str(": pass\n");
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_binary_paren_interplay() {
    // `1+(1+(1+(1+...)))` — each level alternates a binary operator and a
    // parenthesised sub-expression, exactly like the pattern described in
    // the tracking issue.
    let depth = 2_000;
    let mut src = String::new();
    for _ in 0..depth {
        src.push_str("1+(");
    }
    src.push('1');
    for _ in 0..depth {
        src.push(')');
    }
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(matches!(err.error, ParseErrorType::RecursionLimitExceeded));
}

#[test]
fn recursion_limit_first_error_is_recursion_not_noise() {
    // When the limit is hit the outer parser frames will emit secondary
    // errors as they unwind. Callers read the first error via `into_result`
    // / `Parsed::errors()`, so `RecursionLimitExceeded` must come first, and
    // the drain-to-EOF after reporting the recursion limit should keep the total count
    // small rather than producing one noisy error per unwound frame.
    let src = format!("{}1{}", "(".repeat(2_000), ")".repeat(2_000));
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(50);
    let parsed = crate::parse_unchecked(&src, opts);
    let errors = parsed.errors();
    let first = errors.first().expect("expected at least one error");
    assert!(matches!(
        first.error,
        ParseErrorType::RecursionLimitExceeded
    ));
    // Exactly one `RecursionLimitExceeded` — guards against a regression
    // where the unwind loops and re-triggers the limit check.
    let recursion_errors = errors
        .iter()
        .filter(|e| matches!(e.error, ParseErrorType::RecursionLimitExceeded))
        .count();
    assert_eq!(recursion_errors, 1);
    // Small, bounded tail of follow-up errors from the unwinding frames.
    // Today this is 0; the generous cap is a regression gate, not a spec.
    assert!(
        errors.len() <= 8,
        "expected a small number of errors, got {}: {errors:?}",
        errors.len(),
    );
}

#[test]
fn recursion_limit_default_set() {
    let opts = ParseOptions::from(Mode::Module);
    // Guards against someone accidentally unsetting the default. Real-world
    // Python never approaches this depth, and the value must stay within the
    // threading stack's capacity — see the const's docs in `options.rs`.
    assert!(opts.max_recursion_depth() >= 200);
    assert!(opts.max_recursion_depth() <= 2000);
}

#[test]
fn recursion_limit_right_assoc_pow_chain() {
    // `1**1**1**...**1` — `**` is right-associative, so the right operand
    // is parsed by a recursive `parse_binary_expression_or_higher` call
    // *without* any intervening parentheses or atom nesting. This exercises
    // the binary-expression recursion path directly, unlike the
    // `1+(1+(...))` interplay test which recurses through parenthesised
    // atoms.
    let depth = 2_000;
    let mut src = String::with_capacity(depth * 3 + 1);
    for _ in 0..depth {
        src.push_str("1**");
    }
    src.push('1');
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(
        matches!(err.error, ParseErrorType::RecursionLimitExceeded),
        "expected RecursionLimitExceeded, got {:?}",
        err.error
    );
}

#[test]
fn recursion_limit_ternary_else_chain() {
    // `1 if 1 else 1 if 1 else ...` — the `else` operand recurses at the
    // conditional layer (`parse_if_expression` -> `orelse`), which is not
    // covered by the `parse_lhs_expression` guard.
    let depth = 2_000;
    let mut src = String::with_capacity(depth * 12 + 1);
    for _ in 0..depth {
        src.push_str("1 if 1 else ");
    }
    src.push('1');
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(
        matches!(err.error, ParseErrorType::RecursionLimitExceeded),
        "expected RecursionLimitExceeded, got {:?}",
        err.error
    );
}

#[test]
fn recursion_limit_nested_lambda_chain() {
    // `lambda: lambda: lambda: ...` — the lambda body recurses at the
    // conditional layer (`parse_lambda_expr` -> body), bypassing the
    // `parse_lhs_expression` guard entirely.
    let depth = 2_000;
    let mut src = String::from("x = ");
    for _ in 0..depth {
        src.push_str("lambda: ");
    }
    src.push('1');
    let opts = ParseOptions::from(Mode::Module).with_max_recursion_depth(100);
    let err = parse(&src, opts).unwrap_err();
    assert!(
        matches!(err.error, ParseErrorType::RecursionLimitExceeded),
        "expected RecursionLimitExceeded, got {:?}",
        err.error
    );
}

#[test]
fn basedpython_type_param_bound_range_parse() {
    // `T: Lower..Upper` records both ends; `bound` stays the upper end
    let parsed = parse_basedpython_module("class C[T: int..object]: ...\n");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let type_params = class.type_params.as_ref().expect("type params");
    let [TypeParam::TypeVar(type_var)] = type_params.type_params.as_slice() else {
        panic!("expected a single type variable");
    };
    assert_eq!(
        type_var
            .lower_bound
            .as_ref()
            .and_then(|expr| expr.as_name_expr())
            .map(|name| name.id.as_str()),
        Some("int")
    );
    assert_eq!(
        type_var
            .bound
            .as_ref()
            .and_then(|expr| expr.as_name_expr())
            .map(|name| name.id.as_str()),
        Some("object")
    );
}

#[test]
fn basedpython_plain_bound_has_no_lower_end() {
    let parsed = parse_basedpython_module("class C[T: object]: ...\n");
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let [TypeParam::TypeVar(type_var)] = class
        .type_params
        .as_ref()
        .expect("type params")
        .type_params
        .as_slice()
    else {
        panic!("expected a single type variable");
    };
    assert!(type_var.lower_bound.is_none());
    assert!(type_var.bound.is_some());
}

#[test]
fn basedpython_type_param_bound_range_requires_both_ends() {
    // a range needs both ends, and degrades to the end it does have
    for source in ["class C[T: int..]: ...\n", "class C[T: ..int]: ...\n"] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed
                .errors()
                .iter()
                .any(|error| matches!(error.error, ParseErrorType::IncompleteTypeParamBoundRange)),
            "expected an incomplete-range error for {source:?}, got {:?}",
            parsed.errors()
        );
        let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single class definition");
        };
        let [TypeParam::TypeVar(type_var)] = class
            .type_params
            .as_ref()
            .expect("type params")
            .type_params
            .as_slice()
        else {
            panic!("expected a single type variable");
        };
        assert!(type_var.lower_bound.is_none(), "{source:?}");
        assert!(type_var.bound.is_some(), "{source:?}");
    }
}

#[test]
fn type_param_bound_range_is_basedpython_only() {
    // in a `.py` file the range is rejected and degrades to its upper end
    let parsed = crate::Parser::new(
        "class C[T: int..object]: ...\n",
        ParseOptions::from(Mode::Module),
    )
    .parse()
    .try_into_module()
    .expect("expected a module");
    assert!(
        parsed
            .errors()
            .iter()
            .any(|error| matches!(error.error, ParseErrorType::BasedPythonOnly(_))),
        "expected a basedpython-only error, got {:?}",
        parsed.errors()
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let [TypeParam::TypeVar(type_var)] = class
        .type_params
        .as_ref()
        .expect("type params")
        .type_params
        .as_slice()
    else {
        panic!("expected a single type variable");
    };
    assert!(type_var.lower_bound.is_none());
    assert_eq!(
        type_var
            .bound
            .as_ref()
            .and_then(|expr| expr.as_name_expr())
            .map(|name| name.id.as_str()),
        Some("object")
    );
}

#[test]
fn type_param_bound_range_missing_end_is_basedpython_only() {
    // an incomplete range in a `.py` file is rejected for being a range at all, rather than being
    // told how to complete a range that has no spelling here. the incomplete-range error is
    // suppressed as a cascade because `add_error` keeps only the first error at an offset
    let parsed = crate::Parser::new("class C[T: ..int]: ...\n", ParseOptions::from(Mode::Module))
        .parse()
        .try_into_module()
        .expect("expected a module");
    let kinds: Vec<_> = parsed.errors().iter().map(|error| &error.error).collect();
    assert!(
        matches!(kinds.as_slice(), [ParseErrorType::BasedPythonOnly(_)]),
        "expected only a basedpython-only error, got {kinds:?}"
    );
}

#[test]
fn spaced_dots_are_not_a_bound_range() {
    // `..` has to be written as one unit; `a . . b` stays a malformed attribute access
    let parsed = parse_basedpython_module_with_errors("class C[T: int . . object]: ...\n");
    assert!(
        !parsed.errors().is_empty(),
        "expected `int . . object` to be rejected"
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let [TypeParam::TypeVar(type_var)] = class
        .type_params
        .as_ref()
        .expect("type params")
        .type_params
        .as_slice()
    else {
        panic!("expected a single type variable");
    };
    assert!(type_var.lower_bound.is_none());
}

/// The `reified` flag of each type parameter of the module's single function.
fn function_type_param_reification(module: &ModModule) -> Vec<bool> {
    let [Stmt::FunctionDef(function)] = module.body.as_slice() else {
        panic!("expected a single function definition");
    };
    function
        .type_params
        .as_ref()
        .expect("type params")
        .type_params
        .iter()
        .map(TypeParam::is_reified)
        .collect()
}

#[test]
fn basedpython_reified_type_param_parses_for_every_kind() {
    for (source, expected) in [
        ("def f[reified T](): ...\n", vec![true]),
        ("def f[reified *Ts](): ...\n", vec![true]),
        ("def f[reified **Kwargs](): ...\n", vec![true]),
        ("def f[T, reified U](): ...\n", vec![false, true]),
        ("def f[reified T: int = str](): ...\n", vec![true]),
    ] {
        let parsed = parse_basedpython_module(source);
        assert_eq!(
            function_type_param_reification(parsed.syntax()),
            expected,
            "unexpected reification for {source:?}"
        );
    }
}

#[test]
fn basedpython_reified_precedes_the_variance_keywords() {
    let parsed = parse_basedpython_module("class C[reified in out T]: ...\n");
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let [TypeParam::TypeVar(type_var)] = class
        .type_params
        .as_ref()
        .expect("type params")
        .type_params
        .as_slice()
    else {
        panic!("expected a single type variable");
    };
    assert!(type_var.is_reified);
    assert_eq!(
        type_var.variance,
        Some(ruff_python_ast::Variance::Invariant)
    );
    assert_eq!(type_var.name.id.as_str(), "T");
}

#[test]
fn reified_alone_is_a_type_parameter_name() {
    // the modifier is a soft keyword: without a parameter after it, `reified`
    // is the parameter's own name
    for source in [
        "def f[reified](): ...\n",
        "def f[reified: int](): ...\n",
        "def f[reified = int](): ...\n",
        "def f[reified, T](): ...\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::FunctionDef(function)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single function definition");
        };
        let first = &function
            .type_params
            .as_ref()
            .expect("type params")
            .type_params[0];
        assert_eq!(first.name().id.as_str(), "reified", "for {source:?}");
        assert!(!first.is_reified(), "for {source:?}");
    }
}

#[test]
fn reified_type_param_is_basedpython_only() {
    // in a `.py` file the modifier is rejected, but still consumed so the rest
    // of the list parses
    let parsed = crate::Parser::new(
        "def f[reified T](): ...\n",
        ParseOptions::from(Mode::Module),
    )
    .parse()
    .try_into_module()
    .expect("expected a module");
    assert!(
        parsed
            .errors()
            .iter()
            .any(|error| matches!(error.error, ParseErrorType::BasedPythonOnly(_))),
        "expected a basedpython-only error, got {:?}",
        parsed.errors()
    );
    assert_eq!(function_type_param_reification(parsed.syntax()), vec![true]);
}

#[test]
fn basedpython_type_param_separators_parse() {
    // `/` and a bare `*` divide a type parameter list the way they divide a
    // value parameter list
    let parsed = parse_basedpython_module("class C[A, /, B, *, D]: ...\n");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let type_params = class.type_params.as_ref().expect("type params");
    let names: Vec<&str> = type_params.iter().map(|tp| tp.name().as_str()).collect();
    assert_eq!(names, ["A", "B", "D"]);
    assert_eq!(type_params.separators.positional_only_count, Some(1));
    assert_eq!(type_params.separators.keyword_only_start, Some(2));
}

#[test]
fn basedpython_type_param_star_tuple_is_not_a_separator() {
    // a `*Ts` type variable tuple is a parameter, not a keyword-only marker
    let parsed = parse_basedpython_module("class C[A, *Ts, B]: ...\n");
    assert!(
        parsed.errors().is_empty(),
        "unexpected parse errors: {:?}",
        parsed.errors()
    );
    let [Stmt::ClassDef(class)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single class definition");
    };
    let type_params = class.type_params.as_ref().expect("type params");
    assert!(type_params.separators.is_empty());
    assert_eq!(type_params.type_params.len(), 3);
}

#[test]
fn basedpython_type_param_separators_are_basedpython_only() {
    // in plain python `/` inside a type parameter list is a syntax error
    let has_error = match parse("class C[A, /, B]: ...\n", ParseOptions::from(Mode::Module)) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(
        has_error,
        "`/` in a python type parameter list should be an error"
    );
}

#[test]
fn basedpython_type_param_separator_misuse_reports() {
    for source in [
        "class C[/, A]: ...\n",
        "class C[A, *]: ...\n",
        "class C[A, *, B, *, D]: ...\n",
        "class C[A, /, B, /, D]: ...\n",
        "class C[A, *, B, /, D]: ...\n",
    ] {
        let has_error = match parse(
            source,
            ParseOptions::from(Mode::Module).with_basedpython(true),
        ) {
            Ok(parsed) => !parsed.errors().is_empty(),
            Err(_) => true,
        };
        assert!(has_error, "expected a parse error for `{source}`");
    }
}

#[test]
fn basedpython_raises_clause_parses() {
    let parsed = parse_basedpython_module(
        "def a() raises TypeError: ...\n\
         def b() -> int raises TypeError | ValueError: ...\n\
         def c() raises Never: ...\n\
         def d() raises ...: ...\n\
         def e() raises not TypeError: ...\n\
         def f() -> int: ...\n\
         async def g() raises TypeError: ...\n\
         def h() raises TypeError\n",
    );

    let raises: Vec<bool> = parsed
        .syntax()
        .body
        .iter()
        .map(|stmt| {
            let Stmt::FunctionDef(function) = stmt else {
                panic!("expected function definitions");
            };
            function.raises.is_some()
        })
        .collect();

    assert_eq!(
        raises,
        [true, true, true, true, true, false, true, true],
        "every `raises` clause should be recorded, and only those"
    );
}

#[test]
fn basedpython_raises_clause_is_basedpython_only() {
    let has_error = match parse(
        "def f() raises TypeError: ...\n",
        ParseOptions::from(Mode::Module),
    ) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "a `raises` clause should be a .py syntax error");
}

#[test]
fn basedpython_raises_clause_without_a_type_reports() {
    let parsed = parse_basedpython_module_with_errors("def f() raises: ...\n");
    assert!(
        !parsed.errors().is_empty(),
        "a bare `raises` should report a missing type expression"
    );
}

#[test]
fn basedpython_raises_is_still_an_identifier() {
    // `raises` is a soft keyword: it only introduces the clause between a
    // function header and its body, and stays an ordinary name everywhere else
    for source in [
        "raises = 1\n",
        "raises: int = 1\n",
        "def raises(): ...\n",
        "def f(raises): ...\n",
        "def f() -> raises: ...\n",
        "x = raises\n",
    ] {
        parse_basedpython_module(source);
    }
}

#[test]
fn basedpython_if_let_clauses_carry_a_pattern() {
    let parsed = parse_basedpython_module(
        "if let int(n) := v:\n    pass\nelif let str(s) := v:\n    pass\nelse:\n    pass\n",
    );
    let [Stmt::If(if_stmt)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single if statement");
    };
    assert!(if_stmt.pattern.is_some(), "the `if` clause has a pattern");
    assert!(
        matches!(&*if_stmt.test, Expr::Name(name) if name.id.as_str() == "v"),
        "the test is the subject the pattern matches against"
    );
    let [elif, else_clause] = if_stmt.elif_else_clauses.as_slice() else {
        panic!("expected an elif and an else clause");
    };
    assert!(elif.pattern.is_some(), "the `elif` clause has a pattern");
    assert!(
        else_clause.pattern.is_none() && else_clause.test.is_none(),
        "an `else` clause has neither pattern nor test"
    );
}

#[test]
fn basedpython_if_let_accepts_every_pattern_form() {
    for source in [
        "if let 1 := v:\n    pass\n",
        "if let None := v:\n    pass\n",
        "if let x := v:\n    pass\n",
        "if let [a, b] := v:\n    pass\n",
        "if let a, b := v:\n    pass\n",
        "if let {'k': a} := v:\n    pass\n",
        "if let C(x=1) := v:\n    pass\n",
        "if let int() | str() := v:\n    pass\n",
        "if let int() as n := v:\n    pass\n",
        "if let mod.CONST := v:\n    pass\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::If(if_stmt)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single if statement for `{source}`");
        };
        assert!(
            if_stmt.pattern.is_some(),
            "expected a pattern in `{source}`"
        );
    }
}

#[test]
fn basedpython_if_let_is_still_an_identifier() {
    // `let` only introduces a pattern when a complete pattern followed by `:=`
    // parses; everywhere else it stays an ordinary name
    for source in [
        "if let:\n    pass\n",
        "if let := f():\n    pass\n",
        "if let == 3:\n    pass\n",
        "if let(x):\n    pass\n",
        "if let and other:\n    pass\n",
        "if x:\n    pass\nelif let:\n    pass\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::If(if_stmt)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single if statement for `{source}`");
        };
        assert!(
            if_stmt.pattern.is_none()
                && if_stmt
                    .elif_else_clauses
                    .iter()
                    .all(|clause| clause.pattern.is_none()),
            "`let` should stay an identifier in `{source}`"
        );
    }
}

#[test]
fn basedpython_if_let_is_basedpython_only() {
    let has_error = match parse(
        "if let int(n) := v:\n    pass\n",
        ParseOptions::from(Mode::Module),
    ) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "`if let` should be a .py syntax error");
}

#[test]
fn basedpython_type_modifier_prefix() {
    use ruff_python_ast::helpers::{TypeModifier, type_modifier_marker};

    // each source is a single statement whose annotation/return/value carries
    // exactly one modifier marker at its top level
    for (source, expected) in [
        ("a: literal str\n", TypeModifier::Literal),
        ("a: final int = 1\n", TypeModifier::Final),
        ("a: literal list[*]\n", TypeModifier::Literal),
        ("a: final dict[str, int]\n", TypeModifier::Final),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(ann)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single annotated assignment for `{source}`");
        };
        let Some((modifier, inner)) = type_modifier_marker(&ann.annotation) else {
            panic!("expected a type modifier marker in `{source}`");
        };
        assert_eq!(modifier, expected, "wrong modifier in `{source}`");
        assert!(
            type_modifier_marker(inner).is_none(),
            "unexpected nested marker in `{source}`"
        );
    }
}

#[test]
fn basedpython_type_modifier_binds_tighter_than_union() {
    use ruff_python_ast::helpers::type_modifier_marker;

    // `literal str | None` is `(literal str) | None`, not `literal (str | None)`
    let parsed = parse_basedpython_module("a: literal str | None\n");
    let [Stmt::AnnAssign(ann)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single annotated assignment");
    };
    assert!(
        type_modifier_marker(&ann.annotation).is_none(),
        "the modifier must not swallow the whole union"
    );
    let Expr::BinOp(binop) = ann.annotation.as_ref() else {
        panic!("expected a union");
    };
    assert!(type_modifier_marker(&binop.left).is_some());
    assert!(type_modifier_marker(&binop.right).is_none());
}

#[test]
fn basedpython_type_modifier_in_nested_type_positions() {
    struct MarkerFinder<'a> {
        found: &'a mut bool,
    }

    impl ruff_python_ast::visitor::Visitor<'_> for MarkerFinder<'_> {
        fn visit_expr(&mut self, expr: &Expr) {
            if ruff_python_ast::helpers::type_modifier_marker(expr).is_some() {
                *self.found = true;
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }

    for source in [
        "def f(x: literal int): ...\n",
        "def f() -> final int: ...\n",
        "def f(*args: literal int): ...\n",
        "def f(**kwargs: final int): ...\n",
        "a: list[literal str]\n",
        "type X = literal str\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let mut found = false;
        ruff_python_ast::visitor::walk_body(
            &mut MarkerFinder { found: &mut found },
            &parsed.syntax().body,
        );
        assert!(found, "expected a type modifier marker in `{source}`");
    }
}

#[test]
fn basedpython_type_modifier_keywords_stay_identifiers() {
    use ruff_python_ast::helpers::type_modifier_marker;

    // a modifier is only read when a *name* follows it, so an ordinary
    // reference to a variable called `literal` or `final` is untouched
    for source in [
        "a: literal\n",
        "a: final[int]\n",
        "a: literal.Alias\n",
        "a: final()\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(ann)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single annotated assignment for `{source}`");
        };
        assert!(
            type_modifier_marker(&ann.annotation).is_none(),
            "`{source}` should stay an ordinary reference"
        );
    }
}

#[test]
fn basedpython_type_modifier_not_in_value_position() {
    use ruff_python_ast::helpers::type_modifier_marker;

    // outside a type expression the keywords are ordinary identifiers, so
    // `literal str` there is the syntax error it has always been
    let parsed = parse_basedpython_module("a = literal\n");
    let [Stmt::Assign(assign)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single assignment");
    };
    assert!(type_modifier_marker(&assign.value).is_none());
    assert!(
        !parse_basedpython_module_with_errors("a = literal str\n")
            .errors()
            .is_empty()
    );
}

#[test]
fn basedpython_type_modifier_is_basedpython_only() {
    let has_error = match parse("a: literal str\n", ParseOptions::from(Mode::Module)) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "`literal T` should be a .py syntax error");
}

#[test]
fn basedpython_from_export() {
    // `from x export y` parses as a from-import carrying the `is_export`
    // spelling flag; the aliases stay bare (`as y` is what lowering adds)
    for (source, module, level, names) in [
        ("from x export y", Some("x"), 0, vec!["y"]),
        ("from a.b.c export d, e", Some("a.b.c"), 0, vec!["d", "e"]),
        ("from .mod export y", Some("mod"), 1, vec!["y"]),
        // a module *named* `export` is not the keyword
        ("from export export y", Some("export"), 0, vec!["y"]),
        ("lazy from x export y", Some("x"), 0, vec!["y"]),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::ImportFrom(import)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single ImportFrom for `{source}`");
        };
        assert!(import.is_export, "`{source}` should set is_export");
        assert_eq!(import.module.as_deref(), module, "module in `{source}`");
        assert_eq!(import.level, level, "level in `{source}`");
        assert_eq!(
            import
                .names
                .iter()
                .map(|alias| alias.name.as_str())
                .collect::<Vec<_>>(),
            names,
            "names in `{source}`"
        );
        assert!(
            import.names.iter().all(|alias| alias.asname.is_none()),
            "`export` aliases carry no `as` clause in `{source}`"
        );
    }
}

#[test]
fn basedpython_from_export_module_less_relative() {
    // a relative import may omit its module, so `export` at the module position
    // is the keyword — unless `import` or a further `.` follows it
    let parsed = parse_basedpython_module("from . export y");
    let [Stmt::ImportFrom(import)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single ImportFrom");
    };
    assert!(import.is_export);
    assert_eq!(import.module, None);
    assert_eq!(import.level, 1);

    for (source, module) in [
        ("from . export import y", "export"),
        ("from .export.sub import y", "export.sub"),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::ImportFrom(import)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single ImportFrom for `{source}`");
        };
        assert!(!import.is_export, "`{source}` imports a module `export`");
        assert_eq!(import.module.as_deref(), Some(module));
    }
}

#[test]
fn basedpython_from_export_rejects_star_and_asname() {
    // `export` binds each name under itself, so a star (no single name) and a
    // rename (a different name) are both contradictions
    for source in ["from x export *", "from x export a as b"] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            !parsed.errors().is_empty(),
            "expected a parse error for `{source}`"
        );
    }
}

#[test]
fn basedpython_from_export_rejected_in_py() {
    // the keyword is `.by`-only; a `.py` file using it collects a
    // `BasedPythonOnly` gate error
    for source in ["from x export y", "from . export y"] {
        let parsed = crate::Parser::new(source, ParseOptions::from(Mode::Module))
            .parse()
            .try_into_module()
            .expect("recovers to a module");
        assert!(
            parsed.errors().iter().any(ParseError::is_basedpython_only),
            "expected a BasedPythonOnly error for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

#[test]
fn basedpython_let_statement_binds_a_pattern() {
    let parsed = parse_basedpython_module("let Point(x, y) := origin\n");
    let [Stmt::Let(let_stmt)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single let statement");
    };
    assert!(
        matches!(&*let_stmt.pattern, Pattern::MatchClass(_)),
        "the pattern is the one written"
    );
    assert!(
        matches!(&*let_stmt.value, Expr::Name(name) if name.id.as_str() == "origin"),
        "the value is the subject the pattern matches against"
    );
    assert!(let_stmt.orelse.is_empty(), "no `else` block was written");
}

#[test]
fn basedpython_let_statement_takes_an_else_block() {
    let parsed = parse_basedpython_module("let int(n) := v else:\n    return\n");
    let [Stmt::Let(let_stmt)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single let statement");
    };
    assert_eq!(let_stmt.orelse.len(), 1, "the `else` block was parsed");
}

/// A `let` statement is a simple statement until it has an `else` block, so what
/// follows it on the next line is an ordinary statement either way
#[test]
fn basedpython_let_statement_ends_where_it_should() {
    for source in [
        "let int(n) := v\nprint(n)\n",
        "let int(n) := v else:\n    raise ValueError\nprint(n)\n",
    ] {
        let parsed = parse_basedpython_module(source);
        assert_eq!(
            parsed.syntax().body.len(),
            2,
            "expected the `let` and the `print` for `{source}`"
        );
    }
}

#[test]
fn basedpython_let_is_still_an_identifier() {
    // `let` only introduces a destructuring when a complete pattern followed by
    // `:=` parses; the declaration form and the plain name are untouched
    for source in [
        "let = 5\n",
        "let(3)\n",
        "print(let)\n",
        "let x = 1\n",
        "let x: int = 1\n",
    ] {
        let parsed = parse_basedpython_module(source);
        assert!(
            !matches!(parsed.syntax().body.as_slice(), [Stmt::Let(_)]),
            "`let` should not introduce a destructuring in `{source}`"
        );
    }
}

#[test]
fn basedpython_destructuring_binders_carry_a_pattern() {
    let parsed = parse_basedpython_module(
        "for Point(x, y) in points:\n    pass\nwith ctx() as Point(a, b):\n    pass\n",
    );
    let [Stmt::For(for_stmt), Stmt::With(with_stmt)] = parsed.syntax().body.as_slice() else {
        panic!("expected a for and a with statement");
    };
    assert!(for_stmt.pattern.is_some(), "the loop destructures");
    assert!(
        matches!(&*for_stmt.target, Expr::Name(name) if is_destructure_binder(&name.id)),
        "the target is the pattern's binder"
    );
    let [item] = with_stmt.items.as_slice() else {
        panic!("expected a single with item");
    };
    assert!(item.pattern.is_some(), "the item destructures");
    assert!(
        matches!(item.optional_vars.as_deref(), Some(Expr::Name(name)) if is_destructure_binder(&name.id)),
        "the target is the pattern's binder"
    );
}

/// Only a binder that cannot be assigned to is reparsed as a pattern, so every
/// loop and `with` item python accepts keeps its meaning
#[test]
fn basedpython_ordinary_binders_are_untouched() {
    for source in [
        "for x in xs:\n    pass\n",
        "for a, b in pairs:\n    pass\n",
        "for [a, b] in pairs:\n    pass\n",
        "for obj.attr in xs:\n    pass\n",
        "for xs[0] in ys:\n    pass\n",
        "with ctx() as x:\n    pass\n",
        "with ctx() as (a, b):\n    pass\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let carries_pattern = match parsed.syntax().body.as_slice() {
            [Stmt::For(for_stmt)] => for_stmt.pattern.is_some(),
            [Stmt::With(with_stmt)] => with_stmt.items.iter().any(|item| item.pattern.is_some()),
            _ => panic!("expected a single statement for `{source}`"),
        };
        assert!(!carries_pattern, "`{source}` binds a target, not a pattern");
    }
}

#[test]
fn basedpython_parameters_destructure() {
    let parsed = parse_basedpython_module("def f(a: int, Point(x, y): Point): pass\n");
    let [Stmt::FunctionDef(function)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single function");
    };
    let [plain, destructuring] = function.parameters.args.as_slice() else {
        panic!("expected two parameters");
    };
    assert!(
        plain.parameter.pattern.is_none(),
        "an ordinary parameter carries no pattern"
    );
    assert!(
        destructuring.parameter.pattern.is_some(),
        "the second parameter destructures"
    );
    assert!(
        is_destructure_binder(&destructuring.parameter.name.id),
        "its name is the pattern's binder"
    );
}

/// A parameter named with a soft keyword is a name, not a capture pattern
#[test]
fn basedpython_ordinary_parameters_are_untouched() {
    for source in [
        "def f(x: int): pass\n",
        "def f(match: int): pass\n",
        "def f(type, case): pass\n",
        "def f(x: int = 1): pass\n",
        "def f(*args: int, **kwargs: str): pass\n",
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::FunctionDef(function)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single function for `{source}`");
        };
        assert!(
            function
                .parameters
                .iter()
                .map(ruff_python_ast::AnyParameterRef::as_parameter)
                .all(|parameter| parameter.pattern.is_none()),
            "`{source}` names its parameters, it does not match them"
        );
    }
}

#[test]
fn basedpython_destructuring_parameter_needs_an_annotation() {
    let parsed = parse_basedpython_module_with_errors("def f(Point(x, y)): pass\n");
    assert!(
        parsed
            .errors()
            .iter()
            .any(|error| error.to_string().contains("needs an annotation")),
        "expected the missing-annotation error, got {:?}",
        parsed.errors()
    );
}

#[test]
fn basedpython_and_pattern_binds_tighter_than_or() {
    let parsed = parse_basedpython_module("match v:\n    case A() and B() | C():\n        pass\n");
    let [Stmt::Match(match_stmt)] = parsed.syntax().body.as_slice() else {
        panic!("expected a single match statement");
    };
    let [case] = match_stmt.cases.as_slice() else {
        panic!("expected a single case");
    };
    let Pattern::MatchOr(or_pattern) = &case.pattern else {
        panic!(
            "expected `|` to be the outermost pattern, got {:?}",
            case.pattern
        );
    };
    assert!(
        matches!(or_pattern.patterns.as_slice(), [Pattern::MatchAnd(_), _]),
        "the first alternative is the conjunction"
    );
}

#[test]
fn basedpython_and_pattern_is_basedpython_only() {
    let has_error = match parse(
        "match v:\n    case A() and B():\n        pass\n",
        ParseOptions::from(Mode::Module),
    ) {
        Ok(parsed) => !parsed.errors().is_empty(),
        Err(_) => true,
    };
    assert!(has_error, "an `and` pattern should be a .py syntax error");
}

/// `if let P = v` is how rust spells this, so it is the first thing a reader
/// coming from there types — and the error has to say so rather than fall apart
/// at the `=`
#[test]
fn basedpython_destructuring_let_rejects_plain_equals() {
    for source in [
        "if let Point(x, y) = origin:\n    pass\n",
        "let Point(x, y) = origin\n",
    ] {
        let parsed = parse_basedpython_module_with_errors(source);
        assert!(
            parsed
                .errors()
                .iter()
                .any(|error| error.to_string().contains("binds with `:=`, not `=`")),
            "expected the `:=` error for `{source}`, got {:?}",
            parsed.errors()
        );
    }
}

/// every parameter of an `init(...)` becomes a field of the same name, which a
/// pattern has none of
#[test]
fn basedpython_init_method_rejects_a_destructuring_parameter() {
    let parsed = parse_basedpython_module_with_errors("class C:\n    init(Point(x, y): Point)\n");
    assert!(
        parsed.errors().iter().any(|error| {
            error
                .to_string()
                .contains("not valid in an `init(...)` shorthand")
        }),
        "expected the init-shorthand error, got {:?}",
        parsed.errors()
    );
}
