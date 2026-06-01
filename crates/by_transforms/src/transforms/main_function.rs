//! ast pass: a top-level `main` function is the module entry point
//!
//! when a module defines a top-level `def main` (or `async def main`), this
//! pass appends an `if __name__ == "__main__":` guard that invokes it, so
//! running the file as a script executes `main`. an `async def main` is driven
//! through `asyncio.run`
//!
//! the guard is suppressed when the module already invokes `main` itself — an
//! existing `__main__` guard or a bare top-level `main()` call — so the entry
//! point never runs twice. `main` is only wired up when it can be called with
//! no arguments; forwarding command-line arguments is a planned extension

use ruff_python_ast::{CmpOp, Expr, ModModule, Parameters, Stmt, StmtFunctionDef};

use super::ast_driver::{AstPass, PassContext};
use super::source_util::is_synthetic_decorator;

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
        if self.is_private(main) || !callable_with_no_args(&main.parameters) {
            return;
        }
        // respect a hand-written entry point; never invoke `main` twice
        if module_invokes_main(&module.body) {
            return;
        }

        ctx.epilogue.push("if __name__ == \"__main__\":".to_owned());
        if main.is_async {
            ctx.epilogue.push("    asyncio.run(main())".to_owned());
            ctx.required_imports.push("import asyncio".to_owned());
        } else {
            ctx.epilogue.push("    main()".to_owned());
        }
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

/// true when every required parameter has a default, so `main()` is a valid
/// call. variadic `*args` / `**kwargs` never require an argument
fn callable_with_no_args(params: &Parameters) -> bool {
    params
        .iter_non_variadic_params()
        .all(|param| param.default.is_some())
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
    fn main_with_required_argument_is_not_wired_up() {
        // until argument forwarding lands, a `main` that needs an argument
        // can't be invoked, so it isn't treated as the entry point
        unchanged("def main(argv):\n    pass\n");
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
