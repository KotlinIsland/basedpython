//! ast pass: a top-level `main` function is the module entry point
//!
//! when a module defines a top-level `def main` (or `async def main`), this
//! pass appends an `if __name__ == "__main__":` guard that invokes it, so
//! running the file as a script executes `main`. an `async def main` is driven
//! through `asyncio.run`
//!
//! `main`'s parameters become the program's command-line interface. each
//! annotated parameter is filled either positionally or by `--name`, and the
//! guard hands the parsed values to `main`. the parsing itself lives in the
//! `_by_main_args` runtime helper, which is handed a spec derived from the
//! signature — see [`MAIN_ARGS_RUNTIME`]
//!
//! the guard is suppressed when the module already invokes `main` itself — an
//! existing `__main__` guard or a bare top-level `main()` call — so the entry
//! point never runs twice

use ruff_python_ast::{CmpOp, Expr, ModModule, Parameters, Stmt, StmtFunctionDef};

use super::ast_driver::{AstPass, PassContext};
use super::source_util::is_synthetic_decorator;

/// Parses `sys.argv` into `main`'s parameters. Driven by a spec of
/// `(name, converter, kind, required)` tuples emitted from the signature, so
/// the helper itself never introspects the function.
///
/// each parameter accepts both spellings — a positional slot and a `--name`
/// option — which argparse cannot express with a single argument, so they are
/// registered as two arguments over internal `p<i>` / `o<i>` destinations and
/// merged afterwards. `bool` is a flag pair (`--name` / `--no-name`) and takes
/// no positional slot
const MAIN_ARGS_RUNTIME: &str = r#"def _by_main_args(_fn, _params):
    import argparse

    _parser = argparse.ArgumentParser(description=_fn.__doc__)
    for _i, (_name, _type, _kind, _required) in enumerate(_params):
        _flags = [f"--{_name.replace('_', '-')}"]
        if "_" in _name:
            _flags.append(f"--{_name}")
        if _type is None:
            _parser.add_argument(*_flags, dest=f"o{_i}", action="store_true", default=None)
            _parser.add_argument(
                *[f"--no-{_flag[2:]}" for _flag in _flags],
                dest=f"o{_i}",
                action="store_false",
                default=None,
            )
            continue
        if _kind != "keyword":
            _parser.add_argument(f"p{_i}", metavar=_name, nargs="?", type=_type, default=None)
        _parser.add_argument(
            *_flags, dest=f"o{_i}", metavar=_name.upper(), type=_type, default=None
        )
    _parsed = vars(_parser.parse_args())
    _args = []
    _kwargs = {}
    _omitted = None
    for _i, (_name, _type, _kind, _required) in enumerate(_params):
        _value = _parsed.get(f"o{_i}")
        _positional = _parsed.get(f"p{_i}")
        if _value is not None and _positional is not None:
            _parser.error(f"argument {_name}: given both positionally and as an option")
        if _value is None:
            _value = _positional
        if _value is None:
            if _required:
                _parser.error(f"the following arguments are required: {_name}")
            if _kind == "positional":
                _omitted = _name
            continue
        if _kind == "positional":
            if _omitted is not None:
                _parser.error(f"argument {_name}: cannot be given without {_omitted}")
            _args.append(_value)
        else:
            _kwargs[_name] = _value
    return _args, _kwargs
"#;

pub(crate) struct MainFunction<'src> {
    source: &'src str,
    is_stub: bool,
}

impl<'src> MainFunction<'src> {
    pub(crate) fn new(source: &'src str, is_stub: bool) -> Self {
        Self { source, is_stub }
    }

    /// true when `main` carries the synthetic `private` modifier, which the
    /// modifiers pass renames to `_main` — so it is not a public entry point
    /// and a synthesised `main()` call would dangle
    fn is_private(&self, func: &StmtFunctionDef) -> bool {
        func.decorator_list.iter().any(|dec| {
            is_synthetic_decorator(self.source, dec)
                && matches!(&dec.expression, Expr::Name(name) if name.id.as_str() == "private")
        })
    }
}

impl AstPass for MainFunction<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        // stubs declare types only; they are never executed as scripts
        if self.is_stub {
            return;
        }
        let Some(main) = last_top_level_main(&module.body) else {
            return;
        };
        if self.is_private(main) {
            return;
        }
        let Some(params) = cli_params(&main.parameters) else {
            return;
        };
        // respect a hand-written entry point; never invoke `main` twice
        if module_invokes_main(&module.body) {
            return;
        }

        ctx.epilogue.push("if __name__ == \"__main__\":".to_owned());
        let call = if params.is_empty() {
            "main()".to_owned()
        } else {
            ctx.required_imports.push(MAIN_ARGS_RUNTIME.to_owned());
            ctx.epilogue
                .push("    _by_args, _by_kwargs = _by_main_args(main, [".to_owned());
            for param in &params {
                ctx.epilogue
                    .push(format!("        {},", param.spec_entry()));
            }
            ctx.epilogue.push("    ])".to_owned());
            "main(*_by_args, **_by_kwargs)".to_owned()
        };
        if main.is_async {
            ctx.epilogue.push(format!("    asyncio.run({call})"));
            ctx.required_imports.push("import asyncio".to_owned());
        } else {
            ctx.epilogue.push(format!("    {call}"));
        }
    }
}

/// how a `main` parameter is spelled on the command line
enum CliType {
    /// takes a value, converted by the named callable
    Value(&'static str),
    /// a `--name` / `--no-name` flag pair
    Flag,
}

/// a `main` parameter the generated command-line interface fills
struct CliParam {
    name: String,
    ty: CliType,
    /// how the parameter can be passed to `main` itself: `positional`
    /// (positional-only), `keyword` (keyword-only), or `any`
    kind: &'static str,
    required: bool,
}

impl CliParam {
    /// the `(name, converter, kind, required)` tuple `_by_main_args` consumes
    fn spec_entry(&self) -> String {
        let converter = match self.ty {
            CliType::Value(callable) => callable,
            CliType::Flag => "None",
        };
        let required = if self.required { "True" } else { "False" };
        let name = &self.name;
        let kind = self.kind;
        format!("(\"{name}\", {converter}, \"{kind}\", {required})")
    }
}

/// The parameters of `main` that the command line fills, or `None` when the
/// signature has a required parameter the command line can't supply — such a
/// `main` isn't an entry point, because invoking it would raise `TypeError`.
///
/// A parameter whose annotation has no command-line spelling is skipped rather
/// than exposed, so it keeps its default. Variadics are skipped for the same
/// reason: they never require an argument.
fn cli_params(params: &Parameters) -> Option<Vec<CliParam>> {
    let groups = [
        (&params.posonlyargs, "positional"),
        (&params.args, "any"),
        (&params.kwonlyargs, "keyword"),
    ];
    let mut cli = Vec::new();
    for (group, kind) in groups {
        for param in group {
            let required = param.default.is_none();
            match param.parameter.annotation.as_deref().and_then(cli_type) {
                Some(ty) => cli.push(CliParam {
                    name: param.parameter.name.to_string(),
                    ty,
                    kind,
                    required,
                }),
                None if required => return None,
                None => {}
            }
        }
    }
    Some(cli)
}

/// The command-line spelling of an annotation, or `None` when it has none.
///
/// Matched on the annotation as written: the converter emitted into the spec
/// is the same name the source used, so it resolves to whatever that name is
/// bound to at runtime.
fn cli_type(annotation: &Expr) -> Option<CliType> {
    match annotation {
        Expr::Name(name) => match name.id.as_str() {
            "bool" => Some(CliType::Flag),
            "str" => Some(CliType::Value("str")),
            "int" => Some(CliType::Value("int")),
            "float" => Some(CliType::Value("float")),
            "Path" => Some(CliType::Value("Path")),
            _ => None,
        },
        Expr::Attribute(attr) => (attr.attr.as_str() == "Path"
            && matches!(&*attr.value, Expr::Name(name) if name.id.as_str() == "pathlib"))
        .then_some(CliType::Value("pathlib.Path")),
        _ => None,
    }
}

/// the last top-level `def main` / `async def main`, if any. the last
/// definition wins because that is the binding `main` resolves to once the
/// module body has finished executing
fn last_top_level_main(body: &[Stmt]) -> Option<&StmtFunctionDef> {
    body.iter().rev().find_map(|stmt| match stmt {
        Stmt::FunctionDef(func) if func.name.as_str() == "main" => Some(func),
        _ => None,
    })
}

/// true when the module already invokes `main` at the top level — either an
/// `if __name__ == "__main__":` guard or a bare `main(...)` call statement
fn module_invokes_main(body: &[Stmt]) -> bool {
    body.iter().any(|stmt| match stmt {
        Stmt::If(if_stmt) => is_dunder_main_guard(&if_stmt.test),
        Stmt::Expr(expr) => is_main_call(&expr.value),
        _ => false,
    })
}

/// matches `__name__ == "__main__"`, accepting either operand order
fn is_dunder_main_guard(test: &Expr) -> bool {
    let Expr::Compare(cmp) = test else {
        return false;
    };
    let [CmpOp::Eq] = cmp.ops.as_ref() else {
        return false;
    };
    let [right] = cmp.comparators.as_ref() else {
        return false;
    };
    let operands = [cmp.left.as_ref(), right];
    operands.iter().copied().any(|e| is_name(e, "__name__"))
        && operands.iter().copied().any(|e| is_str(e, "__main__"))
}

fn is_main_call(value: &Expr) -> bool {
    matches!(value, Expr::Call(call) if is_name(&call.func, "main"))
}

fn is_name(expr: &Expr, id: &str) -> bool {
    matches!(expr, Expr::Name(name) if name.id.as_str() == id)
}

fn is_str(expr: &Expr, value: &str) -> bool {
    matches!(expr, Expr::StringLiteral(s) if s.value.to_str() == value)
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    /// transpile and assert the output is byte-for-byte the input
    fn unchanged(input: &str) {
        check(input, input);
    }

    #[test]
    fn top_level_main_gets_guard() {
        check(
            indoc! {"
                def main():
                    print(\"hi\")
            "},
            indoc! {"
                def main():
                    print(\"hi\")
                if __name__ == \"__main__\":
                    main()
            "},
        );
    }

    #[test]
    fn bodyless_main_gets_guard() {
        check(
            "def main(): ...\n",
            indoc! {"
                def main(): ...
                if __name__ == \"__main__\":
                    main()
            "},
        );
    }

    #[test]
    fn async_main_uses_asyncio_run() {
        check(
            indoc! {"
                async def main():
                    print(\"hi\")
            "},
            indoc! {"
                import asyncio
                async def main():
                    print(\"hi\")
                if __name__ == \"__main__\":
                    asyncio.run(main())
            "},
        );
    }

    #[test]
    fn no_main_unchanged() {
        unchanged("def helper():\n    pass\n");
    }

    #[test]
    fn main_method_is_not_entry_point() {
        // a `main` method on a class is not a module entry point
        unchanged(indoc! {"
            class App:
                def main(self):
                    pass
        "});
    }

    #[test]
    fn existing_guard_not_duplicated() {
        unchanged(indoc! {"
            def main():
                print(\"hi\")
            if __name__ == \"__main__\":
                main()
        "});
    }

    #[test]
    fn reversed_guard_recognised() {
        unchanged(indoc! {"
            def main():
                print(\"hi\")
            if \"__main__\" == __name__:
                main()
        "});
    }

    #[test]
    fn bare_top_level_call_not_duplicated() {
        // a hand-written unconditional call already runs main; don't add a
        // second invocation under the guard
        unchanged(indoc! {"
            def main():
                print(\"hi\")
            main()
        "});
    }

    #[test]
    fn private_main_is_not_entry_point() {
        // `private` renames the function to `_main`; no dangling `main()` guard
        let out = transpile("private def main():\n    pass\n", &Config::test_default()).unwrap();
        assert!(
            !out.contains("__main__"),
            "private main should not get an entry-point guard, got:\n{out}"
        );
        assert!(
            out.contains("_main"),
            "private main should still be renamed, got:\n{out}"
        );
    }

    #[test]
    fn export_main_keeps_all_then_guard() {
        // `__all__` (from the export modifier) precedes the entry-point guard
        check(
            "export def main():\n    pass\n",
            indoc! {"
                def main():
                    pass
                __all__ = [\"main\"]
                if __name__ == \"__main__\":
                    main()
            "},
        );
    }

    #[test]
    fn main_with_unannotated_required_argument_is_not_wired_up() {
        // an unannotated parameter has no command-line spelling, so `main`
        // can't be called and isn't treated as the entry point
        unchanged("def main(argv):\n    pass\n");
    }

    /// transpile and return the `__main__` guard, without the runtime preamble
    fn guard(input: &str) -> String {
        let out = transpile(input, &Config::test_default()).unwrap();
        let at = out
            .find("if __name__ == \"__main__\":")
            .unwrap_or_else(|| panic!("no entry-point guard in:\n{out}"));
        out[at..].to_owned()
    }

    #[test]
    fn annotated_parameter_becomes_a_cli_argument() {
        assert_eq!(
            guard("def main(name: str):\n    print(name)\n"),
            indoc! {"
                if __name__ == \"__main__\":
                    _by_args, _by_kwargs = _by_main_args(main, [
                        (\"name\", str, \"any\", True),
                    ])
                    main(*_by_args, **_by_kwargs)
            "},
        );
    }

    #[test]
    fn arg_parsing_pulls_in_the_runtime_helper() {
        let out = transpile("def main(name: str):\n    pass\n", &Config::test_default()).unwrap();
        assert!(out.contains("def _by_main_args("), "got:\n{out}");
        // argparse is only needed when the program actually runs
        assert!(out.contains("    import argparse"), "got:\n{out}");
    }

    #[test]
    fn zero_argument_main_needs_no_parsing() {
        let out = transpile("def main():\n    pass\n", &Config::test_default()).unwrap();
        assert!(!out.contains("_by_main_args"), "got:\n{out}");
    }

    #[test]
    fn defaulted_parameter_is_optional() {
        assert!(
            guard("def main(count: int = 1):\n    pass\n")
                .contains("(\"count\", int, \"any\", False),"),
            "got:\n{}",
            guard("def main(count: int = 1):\n    pass\n")
        );
    }

    #[test]
    fn bool_parameter_becomes_a_flag() {
        // `None` as the converter is what marks a `--name` / `--no-name` pair
        assert!(
            guard("def main(verbose: bool = False):\n    pass\n")
                .contains("(\"verbose\", None, \"any\", False),"),
            "got:\n{}",
            guard("def main(verbose: bool = False):\n    pass\n")
        );
    }

    #[test]
    fn path_converter_keeps_the_annotation_spelling() {
        // the converter runs at runtime, so it must name whatever the module
        // actually imported
        assert!(
            guard("def main(out: Path):\n    pass\n").contains("(\"out\", Path, \"any\", True),"),
            "got:\n{}",
            guard("def main(out: Path):\n    pass\n")
        );
        assert!(
            guard("def main(out: pathlib.Path):\n    pass\n")
                .contains("(\"out\", pathlib.Path, \"any\", True),"),
            "got:\n{}",
            guard("def main(out: pathlib.Path):\n    pass\n")
        );
    }

    #[test]
    fn float_parameter_is_supported() {
        assert!(
            guard("def main(ratio: float):\n    pass\n")
                .contains("(\"ratio\", float, \"any\", True),"),
            "got:\n{}",
            guard("def main(ratio: float):\n    pass\n")
        );
    }

    #[test]
    fn parameter_kind_is_recorded() {
        // positional-only parameters can't be passed by keyword, and
        // keyword-only ones can't be passed positionally — the helper needs
        // to know which side each value goes to
        assert_eq!(
            guard("def main(a: str, /, b: int = 1, *, c: str = \"z\"):\n    pass\n"),
            indoc! {"
                if __name__ == \"__main__\":
                    _by_args, _by_kwargs = _by_main_args(main, [
                        (\"a\", str, \"positional\", True),
                        (\"b\", int, \"any\", False),
                        (\"c\", str, \"keyword\", False),
                    ])
                    main(*_by_args, **_by_kwargs)
            "},
        );
    }

    #[test]
    fn unsupported_defaulted_parameter_is_not_exposed() {
        // `argv` has no command-line spelling, but it has a default — so it
        // keeps that default instead of blocking the entry point
        let out = guard("def main(name: str, argv: list[str] | None = None):\n    pass\n");
        assert!(
            out.contains("(\"name\", str, \"any\", True),"),
            "got:\n{out}"
        );
        assert!(!out.contains("argv"), "got:\n{out}");
    }

    #[test]
    fn variadic_parameters_are_not_exposed() {
        let out = guard("def main(name: str, *extra: str, **rest: str):\n    pass\n");
        assert!(
            out.contains("(\"name\", str, \"any\", True),"),
            "got:\n{out}"
        );
        assert!(!out.contains("extra"), "got:\n{out}");
        assert!(!out.contains("rest"), "got:\n{out}");
    }

    #[test]
    fn async_main_with_arguments_is_awaited() {
        assert!(
            guard("async def main(name: str):\n    pass\n")
                .contains("    asyncio.run(main(*_by_args, **_by_kwargs))"),
            "got:\n{}",
            guard("async def main(name: str):\n    pass\n")
        );
    }

    #[test]
    fn hand_written_guard_suppresses_arg_parsing() {
        let out = transpile(
            "def main(name: str):\n    pass\nif __name__ == \"__main__\":\n    main(\"x\")\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(!out.contains("_by_main_args"), "got:\n{out}");
    }

    #[test]
    fn main_with_defaulted_arguments_gets_guard() {
        check(
            "def main(argv=None):\n    pass\n",
            indoc! {"
                def main(argv=None):
                    pass
                if __name__ == \"__main__\":
                    main()
            "},
        );
    }

    #[test]
    fn variadic_main_gets_guard() {
        check(
            "def main(*args, **kwargs):\n    pass\n",
            indoc! {"
                def main(*args, **kwargs):
                    pass
                if __name__ == \"__main__\":
                    main()
            "},
        );
    }

    #[test]
    fn last_main_definition_decides() {
        // the trailing `def main` (with a required arg) is the live binding,
        // so the zero-arg earlier definition does not make it an entry point
        unchanged(indoc! {"
            def main():
                pass
            def main(argv):
                pass
        "});
    }
}
