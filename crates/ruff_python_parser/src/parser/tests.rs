use ruff_python_ast::{
    Expr, InterpolatedStringElement, IpyEscapeKind, ModModule, Number, Operator, Stmt, UnaryOp,
};

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
fn basedpython_local_once_in_callable_type() {
    // `local` / `once` modifiers inside a callable-type parameter list parse
    // cleanly (stripped, no AST field), on the first and subsequent elements
    for (source, arg_count) in [
        ("f: (local int) -> None", 1usize),
        ("f: (local list[int], once str) -> bool", 2),
        ("f: (int, local str, /, once bool) -> None", 3),
    ] {
        let parsed = parse_basedpython_module(source);
        let [Stmt::AnnAssign(assign)] = parsed.syntax().body.as_slice() else {
            panic!("expected a single AnnAssign for `{source}`");
        };
        let Expr::CallableType(callable) = assign.annotation.as_ref() else {
            panic!("expected a callable type for `{source}`");
        };
        assert_eq!(callable.args.len(), arg_count, "arg count for `{source}`");
    }
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
fn basedpython_extension_rejects_base_classes() {
    let parsed = crate::Parser::new(
        "extension list(Sequence):\n    def second(self): ...\n",
        ParseOptions::from(Mode::Module).with_basedpython(true),
    )
    .parse()
    .try_into_module()
    .unwrap();
    assert!(
        parsed
            .errors()
            .iter()
            .any(|e| e.to_string().contains("cannot have base classes")),
        "expected a base-class error, got: {:?}",
        parsed.errors()
    );
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
