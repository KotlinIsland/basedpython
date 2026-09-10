//! reverse of `crate::transforms::overload`:
//!   consecutive `@overload`-decorated stubs → bodyless function declarations
//!
//! detects groups of ≥2 consecutive same-name function defs where all but the
//! last carry `@overload`, and strips the decorator plus the `: ...` stub body
//! from each. the implementation (last def) is left unchanged.
//!
//! conservative: only fires on exact `@overload` decorator (not `typing.overload`)
//! and only when the overloaded stub bodies are exactly `...`

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_body, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct OverloadReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> OverloadReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn src(&self, range: TextRange) -> &str {
        &self.source[usize::from(range.start())..usize::from(range.end())]
    }

    fn has_overload_decorator(func: &StmtFunctionDef) -> bool {
        func.decorator_list
            .iter()
            .any(|dec| matches!(&dec.expression, Expr::Name(n) if n.id.as_str() == "overload"))
    }

    fn is_ellipsis_body(func: &StmtFunctionDef) -> bool {
        matches!(
            func.body.as_slice(),
            [Stmt::Expr(e)] if matches!(e.value.as_ref(), Expr::EllipsisLiteral(_))
        )
    }

    fn is_docstring_body(func: &StmtFunctionDef) -> bool {
        matches!(
            func.body.as_slice(),
            [Stmt::Expr(e)] if matches!(e.value.as_ref(), Expr::StringLiteral(_))
        )
    }

    fn strip_overload_stub(&mut self, func: &StmtFunctionDef) {
        // pick the first `@overload` decorator; deletion runs from there to
        // the next decorator (or the `def` keyword if it's the last)
        let dec_pos = func
            .decorator_list
            .iter()
            .position(|d| matches!(&d.expression, Expr::Name(n) if n.id.as_str() == "overload"));
        let Some(dec_idx) = dec_pos else {
            return;
        };
        let dec = &func.decorator_list[dec_idx];

        let next_start = if let Some(next) = func.decorator_list.get(dec_idx + 1) {
            next.range().start()
        } else {
            let after_dec = usize::from(dec.range().end());
            // search for the header keyword that introduces this function;
            // `async def` must be matched before `def` so we don't strip the
            // `async ` prefix when present
            let header_kw = if func.is_async { "async " } else { "def " };
            let def_offset = self.source[after_dec..].find(header_kw).unwrap_or(0);
            TextSize::from(u32::try_from(after_dec + def_offset).expect("fits u32"))
        };

        self.edits
            .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                dec.range().start(),
                next_start,
            ))));

        // remove `: ...` stub body
        if Self::is_ellipsis_body(func) {
            let body_stmt = &func.body[0];
            let pre_body_end = func
                .returns
                .as_ref()
                .map(|r| r.range().end())
                .unwrap_or_else(|| func.parameters.range().end());
            let colon_and_body = TextRange::new(pre_body_end, body_stmt.range().end());
            if self.src(colon_and_body).trim() == ": ..." {
                self.edits
                    .push(Fix::safe_edit(Edit::range_deletion(colon_and_body)));
            }
        }
    }

    fn process_body(&mut self, body: &[Stmt]) {
        let mut i = 0;
        while i < body.len() {
            let Stmt::FunctionDef(first) = &body[i] else {
                i += 1;
                continue;
            };
            let name = first.name.id.as_str();

            let mut run_end = i + 1;
            while run_end < body.len() {
                if let Stmt::FunctionDef(f) = &body[run_end] {
                    if f.name.id.as_str() == name {
                        run_end += 1;
                        continue;
                    }
                }
                break;
            }

            let is_overloaded_stub = |s: &Stmt| {
                matches!(s, Stmt::FunctionDef(f)
                    if Self::has_overload_decorator(f)
                        && (Self::is_ellipsis_body(f) || Self::is_docstring_body(f)))
            };

            if run_end > i + 1 {
                // Stub-style group: every def is `@overload def f(...): ...`,
                // no implementation. Strip @overload from every def.
                let all_overloads = body[i..run_end].iter().all(is_overloaded_stub);
                // Implementation-style group: all but the last are
                // @overload-decorated stubs; the last is the runtime impl.
                let stubs_only_overloads =
                    !all_overloads && body[i..run_end - 1].iter().all(is_overloaded_stub);

                let strip_range = if all_overloads {
                    Some(&body[i..run_end])
                } else if stubs_only_overloads {
                    Some(&body[i..run_end - 1])
                } else {
                    None
                };
                if let Some(stubs) = strip_range {
                    for stmt in stubs {
                        if let Stmt::FunctionDef(f) = stmt {
                            self.strip_overload_stub(f);
                        }
                    }
                }
            } else if is_overloaded_stub(&body[i]) {
                // Lone `@overload` stub: an overload separated from its siblings
                // by a `sys.version_info` guard isn't part of a ≥2 run, but
                // basedpython still spells overloads as consecutive bodyless
                // defs. strip the `@overload` here too — otherwise it survives in
                // front of a `class def` / `static def` modifier and produces
                // unparsable `@overload class def ...` (as in `tarfile.open`).
                if let Stmt::FunctionDef(f) = &body[i] {
                    self.strip_overload_stub(f);
                }
            }

            i = run_end;
        }
    }
}

impl<'ast> Visitor<'ast> for OverloadReverse<'_> {
    fn visit_body(&mut self, body: &'ast [Stmt]) {
        self.process_body(body);
        walk_body(self, body);
    }

    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};
    use indoc::indoc;

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn simple_overload_group() {
        check(
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                @overload
                def f(a: str) -> str: ...
                def f(a): ...
            "},
            indoc! {"
                from typing import overload
                def f(a: int) -> int
                def f(a: str) -> str
                def f(a): ...
            "},
        );
    }

    #[test]
    fn overload_in_class() {
        check(
            indoc! {"
                from typing import overload
                class A:
                    @overload
                    def method(self, x: int) -> int: ...
                    @overload
                    def method(self, x: str) -> str: ...
                    def method(self, x): ...
            "},
            indoc! {"
                from typing import overload
                class A:
                    def method(self, x: int) -> int
                    def method(self, x: str) -> str
                    def method(self, x): ...
            "},
        );
    }

    #[test]
    fn single_def_overload_pass_unchanged() {
        // overload reverse leaves a lone def alone, and outside a stub the
        // empty-declarations reverse pass leaves its `: ...` body alone too
        check("def f(a: int) -> int: ...\n", "def f(a: int) -> int: ...\n");
    }

    #[test]
    fn docstring_bearing_overload_group() {
        check(
            indoc! {"
                from typing import overload
                @overload
                def f(a: int) -> int:
                    \"\"\"int variant\"\"\"
                @overload
                def f(a: str) -> str: ...
            "},
            indoc! {"
                from typing import overload
                def f(a: int) -> int:
                    \"\"\"int variant\"\"\"
                def f(a: str) -> str
            "},
        );
    }

    #[test]
    fn no_overload_decorator_unchanged() {
        // defs without @overload are not part of an overload group
        check(
            indoc! {"
                def f(a: int) -> int: ...
                def f(a: str) -> str: ...
                def f(a): ...
            "},
            indoc! {"
                def f(a: int) -> int: ...
                def f(a: str) -> str: ...
                def f(a): ...
            "},
        );
    }

    #[test]
    fn lone_overload_split_by_version_guard() {
        // overloads split across a `sys.version_info` guard: the first def is a
        // "run of 1" (the next statement is the `if`, not another `f`). its
        // `@overload` must still be stripped — otherwise it survives in front of
        // a later `class def` / `static def` modifier as unparsable
        // `@overload class def` (the `tarfile.open` bug)
        check(
            indoc! {"
                import sys
                from typing import overload
                @overload
                def f(a: int) -> int: ...
                if sys.version_info >= (3, 13):
                    @overload
                    def f(a: str) -> str: ...
            "},
            indoc! {"
                import sys
                from typing import overload
                def f(a: int) -> int
                if sys.version_info >= (3, 13):
                    def f(a: str) -> str
            "},
        );
    }
}
