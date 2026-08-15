//! AST pass: expands `decorator def` into overload stubs + a runtime
//! dispatcher.
//!
//! ```by
//! decorator def d(fn: (...) -> object, option: bool = False) -> int:
//!     return 1 if option else len(str(fn))
//! ```
//! →
//! ```python
//! @overload
//! def d(fn: Callable[..., object], /) -> int: ...
//! @overload
//! def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
//! def d(fn=None, *, option=False):
//!     if fn is None:
//!         def inner(fn):
//!             return d(fn, option=option)
//!         return inner
//!     return 1 if option else len(str(fn))
//! ```

use std::cell::RefCell;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, ModModule, ParameterWithDefault, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};

pub(crate) struct DecoratorKeyword<'src> {
    source: &'src str,
}

impl<'src> DecoratorKeyword<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self { source }
    }
}

impl AstPass for DecoratorKeyword<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            edits: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
            needs_callable: false,
            needs_overload: false,
            class_depth: 0,
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        if state.needs_callable {
            ctx.required_imports
                .push("from typing import Callable".to_owned());
        }
        if state.needs_overload {
            ctx.required_imports
                .push("from typing import overload".to_owned());
        }
        ctx.text_edits.extend(state.edits.into_inner());
        ctx.errors.extend(state.errors.into_inner());
    }
}

struct State<'src> {
    source: &'src str,
    edits: RefCell<Vec<(TextRange, String)>>,
    errors: RefCell<Vec<String>>,
    needs_callable: bool,
    needs_overload: bool,
    class_depth: u32,
}

impl State<'_> {
    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn line_indent(&self, pos: TextSize) -> &str {
        super::source_util::line_indent(self.source, pos)
    }

    fn find_byte_between(&self, range: TextRange, byte: u8) -> Option<TextSize> {
        let start = usize::from(range.start());
        let end = usize::from(range.end());
        let bytes = self.source.as_bytes();
        for (i, &b) in bytes[start..end].iter().enumerate() {
            if b == byte {
                return TextSize::try_from(start + i).ok();
            }
        }
        None
    }

    fn push(&self, range: TextRange, repl: String) {
        self.edits.borrow_mut().push((range, repl));
    }

    fn error(&self, msg: String) {
        self.errors.borrow_mut().push(msg);
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        use std::fmt::Write as _;

        let Some(deco) = func.decorator_list.iter().find(|d| {
            super::source_util::is_synthetic_decorator(self.source, d)
                && matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "decorator_keyword")
        }) else {
            return;
        };

        let params = func.parameters.as_ref();
        if params.vararg.is_some() || params.kwarg.is_some() {
            self.error("`decorator def` cannot use `*args` or `**kwargs`".to_owned());
            return;
        }
        let positional: Vec<&ParameterWithDefault> =
            params.posonlyargs.iter().chain(&params.args).collect();
        let Some((fn_param, rest_positional)) = positional.split_first() else {
            self.error(
                "`decorator def` must declare at least one parameter (the decorated callable)"
                    .to_owned(),
            );
            return;
        };
        if fn_param.default.is_some() {
            self.error(format!(
                "`decorator def` first parameter `{}` must not have a default",
                fn_param.name().as_str()
            ));
            return;
        }
        let options: Vec<&ParameterWithDefault> = rest_positional
            .iter()
            .copied()
            .chain(params.kwonlyargs.iter())
            .collect();
        for opt in &options {
            if opt.default.is_none() {
                self.error(format!(
                    "`decorator def` option `{}` must have a default value",
                    opt.name().as_str()
                ));
                return;
            }
        }

        self.needs_callable = true;
        self.needs_overload = true;

        let fn_name = func.name.as_str();
        let ret_text = func
            .returns
            .as_ref()
            .map(|r| self.src(r.range()).to_owned())
            .unwrap_or_else(|| "object".to_owned());

        let base_indent = self.line_indent(func.range().start()).to_owned();
        let body_indent = format!("{base_indent}    ");

        let mut header = String::new();
        header.push_str("@overload\n");
        header.push_str(&base_indent);
        let _ = writeln!(
            header,
            "def {fn_name}(fn: Callable[..., object], /) -> {ret_text}: ..."
        );
        header.push_str(&base_indent);
        header.push_str("@overload\n");
        header.push_str(&base_indent);
        let opt_sig: String = options
            .iter()
            .map(|o| {
                let name = o.name().as_str();
                let ann = o
                    .annotation()
                    .map(|a| format!(": {}", self.src(a.range())))
                    .unwrap_or_default();
                format!("{name}{ann} = ...")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let opt_sig_args = if options.is_empty() {
            String::new()
        } else {
            format!("*, {opt_sig}")
        };
        let _ = writeln!(
            header,
            "def {fn_name}({opt_sig_args}) -> Callable[[Callable[..., object]], {ret_text}]: ..."
        );
        header.push_str(&base_indent);
        let impl_options: String = options
            .iter()
            .map(|o| {
                let name = o.name().as_str();
                let default = o
                    .default
                    .as_ref()
                    .map(|d| self.src(d.range()).to_owned())
                    .unwrap_or_else(|| "None".to_owned());
                format!("{name}={default}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let impl_sig_args = if options.is_empty() {
            "fn=None".to_owned()
        } else {
            format!("fn=None, *, {impl_options}")
        };
        let _ = write!(header, "def {fn_name}({impl_sig_args}):");

        let recursive_kw_args: String = options
            .iter()
            .map(|o| {
                let name = o.name().as_str();
                format!("{name}={name}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let recursive_call = if options.is_empty() {
            format!("{fn_name}(fn)")
        } else {
            format!("{fn_name}(fn, {recursive_kw_args})")
        };

        let dispatch = |prefix: &str| {
            format!(
                "{prefix}if fn is None:\n{body_indent}    def inner(fn):\n{body_indent}        return {recursive_call}\n{body_indent}    return inner\n{body_indent}"
            )
        };

        // a `decorator def` with no body is a declaration — `decorator def d(fn)`
        // means the same as `decorator def d(fn): ...`, just as a bodyless plain
        // `def` does. the source carries no colon and no body statement to anchor
        // edits on, so the whole declaration is replaced in one go
        let Some(first_stmt) = func.body.first() else {
            let body = dispatch(&format!("\n{body_indent}"));
            self.push(
                TextRange::new(deco.range().start(), func.range().end()),
                format!("{header}{body}..."),
            );
            return;
        };
        let body_first_pos = first_stmt.range().start();

        let scan_from = func
            .returns
            .as_ref()
            .map(|r| r.range().end())
            .unwrap_or_else(|| params.range().end());
        let Some(colon_pos) =
            self.find_byte_between(TextRange::new(scan_from, body_first_pos), b':')
        else {
            self.error(format!(
                "`decorator def {fn_name}` has a body but no `:` ending its signature"
            ));
            return;
        };
        let header_end = colon_pos + TextSize::from(1);

        self.push(TextRange::new(deco.range().start(), header_end), header);

        let is_inline_body = {
            let body_start = usize::from(body_first_pos);
            let prefix = &self.source[..body_start];
            let mut inline = false;
            for c in prefix.chars().rev() {
                if c == '\n' {
                    break;
                }
                if c == ':' {
                    inline = true;
                    break;
                }
            }
            inline
        };
        let prefix = if is_inline_body {
            format!("\n{body_indent}")
        } else {
            String::new()
        };

        self.push(
            TextRange::new(body_first_pos, body_first_pos),
            dispatch(&prefix),
        );
    }
}

impl<'ast> Visitor<'ast> for State<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::ClassDef(_) => {
                self.class_depth += 1;
                walk_stmt(self, stmt);
                self.class_depth -= 1;
            }
            Stmt::FunctionDef(func) => {
                let has_decorator_kw = func.decorator_list.iter().any(|d| {
                    super::source_util::is_synthetic_decorator(self.source, d)
                        && matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "decorator_keyword")
                });
                if has_decorator_kw {
                    if self.class_depth > 0 {
                        self.error(format!(
                            "`decorator def {}` is only valid at module scope, not inside a class body",
                            func.name.as_str()
                        ));
                    } else {
                        self.process_function(func);
                    }
                }
                walk_stmt(self, stmt);
            }
            _ => walk_stmt(self, stmt),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    #[test]
    fn basic_decorator_with_option() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object, option: bool = False) -> int:
                    return 1 if option else len(str(fn))
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> int: ...
                @overload
                def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, option=False):
                    if fn is None:
                        def inner(fn):
                            return d(fn, option=option)
                        return inner
                    return 1 if option else len(str(fn))
            "},
        );
    }

    #[test]
    fn decorator_no_options() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object) -> int:
                    return len(str(fn))
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> int: ...
                @overload
                def d() -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return d(fn)
                        return inner
                    return len(str(fn))
            "},
        );
    }

    #[test]
    fn decorator_multiple_options() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object, a: int = 1, b: str = \"x\") -> int:
                    return a + len(b)
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> int: ...
                @overload
                def d(*, a: int = ..., b: str = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, a=1, b=\"x\"):
                    if fn is None:
                        def inner(fn):
                            return d(fn, a=a, b=b)
                        return inner
                    return a + len(b)
            "},
        );
    }

    #[test]
    fn bodyless_decorator_declaration() {
        check(
            "decorator def d(fn: (int) -> None)\n",
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> object: ...
                @overload
                def d() -> Callable[[Callable[..., object]], object]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return d(fn)
                        return inner
                    ...
            "},
        );
    }

    #[test]
    fn bodyless_decorator_declaration_with_options() {
        check(
            "decorator def d(fn: (...) -> object, option: bool = False) -> int\n",
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> int: ...
                @overload
                def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, option=False):
                    if fn is None:
                        def inner(fn):
                            return d(fn, option=option)
                        return inner
                    ...
            "},
        );
    }

    // the bodyless form used to scan the rest of the file for the colon that
    // ends a signature, swallowing every statement up to the next one it found
    #[test]
    fn bodyless_decorator_leaves_later_statements_alone() {
        check(
            indoc! {"
                decorator def d(fn: (int) -> None)

                d(lambda i: None)
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], /) -> object: ...
                @overload
                def d() -> Callable[[Callable[..., object]], object]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return d(fn)
                        return inner
                    ...

                d(lambda i: None)
            "},
        );
    }

    #[test]
    fn decorator_no_callable_fails() {
        let result = transpile(
            "decorator def d() -> int: return 1\n",
            &Config::test_default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.contains("must declare at least one parameter"),
            "got: {err}"
        );
    }

    #[test]
    fn decorator_default_on_fn_fails() {
        let result = transpile(
            "decorator def d(fn: object = None) -> int: return 1\n",
            &Config::test_default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("must not have a default"), "got: {err}");
    }

    #[test]
    fn decorator_option_without_default_fails() {
        let result = transpile(
            "decorator def d(fn: object, opt: bool) -> int: return 1\n",
            &Config::test_default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("must have a default value"), "got: {err}");
    }

    #[test]
    fn decorator_in_class_body_rejected() {
        let result = transpile(
            indoc! {"
                class C:
                    decorator def d(fn): return fn
            "},
            &Config::test_default(),
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("only valid at module scope"), "got: {err}");
    }
}
