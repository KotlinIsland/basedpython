//! reverse of `crate::transforms::modifiers`:
//!   `@abstractmethod\ndef f(self):` → `abstract def f(self):`
//!   `@final\nclass A:` → `final class A:`
//!   `@dataclass(slots=True)\nclass A:` → `data class A:`
//!   etc.
//!
//! conservative: only fires on exact unqualified decorator names matching the
//! precise signatures produced by the forward transform. does not reverse
//! `abstract class` or `open class` since those produce no output decorator.

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Decorator, Expr, Stmt, StmtClassDef, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange, TextSize};

pub(crate) struct ModifiersReverse<'src> {
    source: &'src str,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> ModifiersReverse<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        Self {
            source,
            edits: Vec::new(),
        }
    }

    fn apply(&mut self, dec_start: TextSize, next_start: TextSize, modifier: &str) {
        let range = TextRange::new(dec_start, next_start);
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            format!("{modifier} "),
            range,
        )));
    }

    /// Find the source position of the def/class header keyword that follows
    /// the decorator at `decorators[idx]`. Either the next decorator's start,
    /// or — if this is the last decorator — the keyword (`def`/`async`/`class`).
    fn next_header_start(
        &self,
        decorators: &[Decorator],
        idx: usize,
        header_keyword: &str,
    ) -> Option<TextSize> {
        if let Some(next) = decorators.get(idx + 1) {
            return Some(next.range().start());
        }
        let after_dec = usize::from(decorators[idx].range().end());
        let rest = &self.source[after_dec..];
        let offset = rest.find(header_keyword)?;
        Some(TextSize::from(u32::try_from(after_dec + offset).ok()?))
    }

    /// Pick the decorator to fold into a modifier keyword. The modifier
    /// becomes part of the def/class header, so we only reverse a decorator
    /// when it's the *last* in the list — putting `final ` before a remaining
    /// `@type_check_only` would produce `final @type_check_only class …`,
    /// which is not valid syntax. Forward emits modifier-keyword decorators
    /// last for exactly this reason.
    fn last_modifier_decorator<F>(
        decorators: &[Decorator],
        classify: F,
    ) -> Option<(usize, &'static str)>
    where
        F: Fn(&Expr) -> Option<&'static str>,
    {
        let last_idx = decorators.len().checked_sub(1)?;
        let last = &decorators[last_idx];
        classify(&last.expression).map(|m| (last_idx, m))
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
        let Some((idx, modifier)) =
            Self::last_modifier_decorator(&func.decorator_list, func_modifier)
        else {
            return;
        };
        let header_keyword = if func.is_async { "async " } else { "def " };
        let Some(next_start) = self.next_header_start(&func.decorator_list, idx, header_keyword)
        else {
            return;
        };
        self.apply(
            func.decorator_list[idx].range().start(),
            next_start,
            modifier,
        );
    }

    fn process_class(&mut self, class: &StmtClassDef) {
        let Some((idx, modifier)) =
            Self::last_modifier_decorator(&class.decorator_list, class_modifier)
        else {
            return;
        };
        let Some(next_start) = self.next_header_start(&class.decorator_list, idx, "class ") else {
            return;
        };
        self.apply(
            class.decorator_list[idx].range().start(),
            next_start,
            modifier,
        );
    }
}

fn func_modifier(expr: &Expr) -> Option<&'static str> {
    let Expr::Name(n) = expr else { return None };
    match n.id.as_str() {
        "abstractmethod" => Some("abstract"),
        "override" => Some("override"),
        "staticmethod" => Some("static"),
        "classmethod" => Some("class"),
        "final" => Some("final"),
        _ => None,
    }
}

fn class_modifier(expr: &Expr) -> Option<&'static str> {
    match expr {
        Expr::Name(n) => match n.id.as_str() {
            "final" => Some("final"),
            _ => None,
        },
        Expr::Call(call) => {
            if !matches!(call.func.as_ref(), Expr::Name(n) if n.id == "dataclass") {
                return None;
            }
            if !call.arguments.args.is_empty() {
                return None;
            }
            let kws = &call.arguments.keywords;
            let has_slots = kws
                .iter()
                .any(|k| k.arg.as_deref() == Some("slots") && is_true(&k.value));
            let has_frozen = kws
                .iter()
                .any(|k| k.arg.as_deref() == Some("frozen") && is_true(&k.value));
            match (kws.len(), has_slots, has_frozen) {
                (1, true, false) => Some("data"),
                (2, true, true) => Some("frozen data"),
                _ => None,
            }
        }
        _ => None,
    }
}

fn is_true(expr: &Expr) -> bool {
    matches!(expr, Expr::BooleanLiteral(b) if b.value)
}

impl<'ast> Visitor<'ast> for ModifiersReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        match stmt {
            Stmt::FunctionDef(f) => {
                self.process_function(f);
            }
            Stmt::ClassDef(c) => {
                self.process_class(c);
            }
            _ => {}
        }
        walk_stmt(self, stmt);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(
            reverse_transpile(input, &Config::test_default()).unwrap(),
            expected
        );
    }

    #[test]
    fn abstract_method() {
        // non-stub: the modifier reverses but the `: ...` body is kept, since a
        // bodyless `abstract def` forward-maps to `: raise NotImplementedError`
        check(
            "class A:\n    @abstractmethod\n    def f(self): ...\n",
            "class A:\n    abstract def f(self): ...\n",
        );
    }

    #[test]
    fn final_class() {
        // empty_class reverse also strips `: ...`
        check("@final\nclass A: ...\n", "final class A\n");
    }

    #[test]
    fn final_method() {
        check(
            "class A:\n    @final\n    def f(self): ...\n",
            "class A:\n    final def f(self): ...\n",
        );
    }

    #[test]
    fn override_method() {
        check(
            "class A:\n    @override\n    def f(self): ...\n",
            "class A:\n    override def f(self): ...\n",
        );
    }

    #[test]
    fn static_method() {
        check(
            "class A:\n    @staticmethod\n    def f(): ...\n",
            "class A:\n    static def f(): ...\n",
        );
    }

    #[test]
    fn class_method() {
        check(
            "class A:\n    @classmethod\n    def f(cls): ...\n",
            "class A:\n    class def f(cls): ...\n",
        );
    }

    #[test]
    fn data_class() {
        check(
            "@dataclass(slots=True)\nclass A:\n    x: int\n",
            "data class A:\n    x: int\n",
        );
    }

    #[test]
    fn frozen_data_class() {
        check(
            "@dataclass(frozen=True, slots=True)\nclass A:\n    x: int\n",
            "frozen data class A:\n    x: int\n",
        );
    }

    #[test]
    fn stacked_overload_abstract() {
        // All bodies are `: ...` so the overload reverse strips `@overload`
        // and the colon body; modifier reverse still rewrites
        // `@abstractmethod` even though it isn't the first decorator.
        check(
            "from typing import overload\nclass A:\n    @overload\n    @abstractmethod\n    def f(self, x: int) -> int: ...\n    @overload\n    @abstractmethod\n    def f(self, x: str) -> str: ...\n",
            "from typing import overload\nclass A:\n    abstract def f(self, x: int) -> int\n    abstract def f(self, x: str) -> str\n",
        );
    }

    #[test]
    fn stacked_overload_abstract_with_docstring() {
        // Docstring-bearing stub keeps `@overload` removed and its body intact;
        // `@abstractmethod` still reverses to `abstract`.
        check(
            "from typing import overload\nclass A:\n    @overload\n    @abstractmethod\n    def f(self, x: int) -> int:\n        \"\"\"doc\"\"\"\n    @overload\n    @abstractmethod\n    def f(self, x: str) -> str: ...\n",
            "from typing import overload\nclass A:\n    abstract def f(self, x: int) -> int:\n        \"\"\"doc\"\"\"\n    abstract def f(self, x: str) -> str\n",
        );
    }

    #[test]
    fn stacked_decorator_unrelated_first() {
        // A non-modifier first decorator should not block reversal of a later
        // modifier decorator.
        check(
            "class A:\n    @property\n    @final\n    def f(self) -> int: ...\n",
            "class A:\n    @property\n    final def f(self) -> int: ...\n",
        );
    }

    #[test]
    fn modifier_not_last_unchanged() {
        // `@final` is not the last decorator — leaving it as a keyword would
        // give `final @type_check_only class C`, which is invalid syntax.
        // Reverse must leave the whole decorator stack alone in this case.
        check(
            "@final\n@type_check_only\nclass C: ...\n",
            "@final\n@type_check_only\nclass C\n",
        );
    }

    #[test]
    fn plain_dataclass_not_reversed() {
        // @dataclass without the exact signature produced by the forward transform
        // should not be reversed
        check(
            "@dataclass\nclass A:\n    x: int\n",
            "@dataclass\nclass A:\n    x: int\n",
        );
    }
}
