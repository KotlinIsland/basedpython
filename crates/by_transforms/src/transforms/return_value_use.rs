//! Erases the return-value markers.
//!
//! `@ignorable_return_value` and `@must_use_return_value` say how a *caller*
//! may treat what a declaration returns. Nothing runs: the checker reads them,
//! `unused-return-value` acts on them, and the emitted python has no use for
//! either. So lowering deletes the decorator, the way the `raises` clause is
//! deleted.
//!
//! Which names are markers is a question for ty, not for the spelling: both are
//! implicitly available, so a file that binds one of those names itself means
//! its own function, and that decorator is a real call to keep. That is why the
//! markers are collected up front from the db's own parse, like
//! [`literal_string`](super::literal_string), and handed to the pass.
//!
//! The pass both deletes the decorator's source line *and* removes it from the
//! AST. Either alone would be enough on its own — but a statement another pass
//! re-renders is rebuilt from the AST and drops the text edit, while a
//! statement nothing re-renders keeps its source bytes and ignores the AST. A
//! marker that survived to the output would be an undefined name at runtime.

use ruff_python_ast::visitor::transformer::{Transformer, walk_stmt};
use ruff_python_ast::visitor::{Visitor, walk_stmt as walk_stmt_ref};
use ruff_python_ast::{Decorator, Expr, ModModule, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, PassContext};
use crate::type_info::TypeInfo;

/// The source ranges of the marker decorators in a file.
#[derive(Default)]
pub(crate) struct ReturnValueMarkers {
    /// each marker decorator's own range, as the db's parse reports it
    decorators: Vec<TextRange>,
}

impl ReturnValueMarkers {
    fn contains(&self, decorator: &Decorator) -> bool {
        self.decorators.contains(&decorator.range())
    }

    fn is_empty(&self) -> bool {
        self.decorators.is_empty()
    }
}

/// Collect every decorator in `stmts` that names a return-value marker.
/// `stmts` must come from the same parse `types` answers for.
pub(crate) fn collect(stmts: &[Stmt], types: &dyn TypeInfo) -> ReturnValueMarkers {
    let mut collector = Collector {
        types,
        markers: ReturnValueMarkers::default(),
    };
    for stmt in stmts {
        collector.visit_stmt(stmt);
    }
    collector.markers
}

struct Collector<'a> {
    types: &'a dyn TypeInfo,
    markers: ReturnValueMarkers,
}

impl Collector<'_> {
    fn collect_from(&mut self, decorators: &[Decorator]) {
        for decorator in decorators {
            if let Expr::Name(name) = &decorator.expression
                && self.types.is_return_value_marker(name)
            {
                self.markers.decorators.push(decorator.range());
            }
        }
    }
}

impl<'ast> Visitor<'ast> for Collector<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => self.collect_from(&function.decorator_list),
            Stmt::ClassDef(class) => self.collect_from(&class.decorator_list),
            _ => {}
        }
        walk_stmt_ref(self, stmt);
    }
}

/// Deletes every marker decorator, from the source and from the AST.
pub(crate) struct ReturnValueUsePass<'src> {
    source: &'src str,
    markers: ReturnValueMarkers,
}

impl<'src> ReturnValueUsePass<'src> {
    pub(crate) fn new(source: &'src str, markers: ReturnValueMarkers) -> Self {
        Self { source, markers }
    }
}

impl AstPass for ReturnValueUsePass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        if self.markers.is_empty() {
            return;
        }
        let mut edits = Vec::new();
        for decorator in &self.markers.decorators {
            edits.push((decorator_line(self.source, *decorator), String::new()));
        }
        ctx.text_edits.extend(edits);

        let stripper = Stripper {
            markers: &self.markers,
        };
        for stmt in &mut module.body {
            stripper.visit_stmt(stmt);
        }
    }
}

/// The whole line a decorator occupies, trailing newline included.
///
/// A decorator is alone on its line, so taking the line rather than the
/// decorator leaves neither the indentation in front of it nor a blank line
/// where it was. A trailing comment on that line explains the marker, and goes
/// with it.
fn decorator_line(source: &str, decorator: TextRange) -> TextRange {
    let line_start = source[..usize::from(decorator.start())]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let line_end = source[usize::from(decorator.end())..]
        .find('\n')
        .map_or(source.len(), |newline| {
            usize::from(decorator.end()) + newline + 1
        });
    TextRange::new(
        TextSize::try_from(line_start).unwrap_or_default(),
        TextSize::try_from(line_end).unwrap_or_default(),
    )
}

struct Stripper<'a> {
    markers: &'a ReturnValueMarkers,
}

impl Transformer for Stripper<'_> {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => function
                .decorator_list
                .retain(|decorator| !self.markers.contains(decorator)),
            Stmt::ClassDef(class) => class
                .decorator_list
                .retain(|decorator| !self.markers.contains(decorator)),
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    #[track_caller]
    fn check(input: &str, expected: &str) {
        assert_eq!(
            transpile(input, &Config::test_default()).unwrap(),
            crate::python_passthrough::lazify_expected(expected)
        );
    }

    #[test]
    fn marker_on_a_function_is_erased() {
        check(
            "\
@ignorable_return_value
def f() -> int:
    return 1
",
            "\
def f() -> int:
    return 1
",
        );
    }

    #[test]
    fn marker_on_a_class_and_a_method_is_erased() {
        check(
            "\
@ignorable_return_value
class A:
    @must_use_return_value
    def f(self) -> int:
        return 1
",
            "\
class A:
    def f(self) -> int:
        return 1
",
        );
    }

    #[test]
    fn a_marker_keeps_the_decorators_around_it() {
        check(
            "\
import functools

@functools.cache
@ignorable_return_value
def f() -> int:
    return 1
",
            "\
import functools

@functools.cache
def f() -> int:
    return 1
",
        );
    }

    /// the marker sits above a modifier keyword, which is a decorator the parser
    /// synthesised rather than one the file wrote — the two are lowered by
    /// different passes and must not tread on each other
    #[test]
    fn a_marker_above_a_modifier_keyword() {
        check(
            "\
class A:
    @ignorable_return_value
    final def f(self) -> int:
        return 1
",
            "\
from typing import final
class A:
    @final
    def f(self) -> int:
        return 1
",
        );
    }

    /// a bodyless `def` redeclared below it is an implicit overload, and the
    /// overload pass writes an `@overload` decorator onto the same lines this
    /// pass is deleting from
    #[test]
    fn a_marker_on_an_implicit_overload() {
        check(
            "\
@ignorable_return_value
def f(a: int) -> int

@ignorable_return_value
def f(a: str) -> str

def f(a: int | str) -> int | str:
    return a
",
            "\
from typing import overload
@overload
def f(a: int) -> int: ...

@overload
def f(a: str) -> str: ...

def f(a: int | str) -> int | str:
    return a
",
        );
    }

    /// a file that binds the name itself means its own function, which is a
    /// real decorator with a real runtime effect
    #[test]
    fn a_shadowing_definition_is_left_alone() {
        unchanged(
            "\
def ignorable_return_value(fn):
    return fn

@ignorable_return_value
def f() -> int:
    return 1
",
        );
    }
}
