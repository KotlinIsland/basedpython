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
//! def d(fn: Callable[..., object], *, option: bool = ...) -> int: ...
//! @overload
//! def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
//! def d(fn=None, *, option=False):
//!     if fn is None:
//!         def inner(fn):
//!             return _by_d(fn, option=option)
//!         return inner
//!     return 1 if option else len(str(fn))
//! _by_d = d
//! ```
//!
//! the two overloads are the pair ty models the declaration as
//! (`Signature::decorator_keyword_overloads`), so what a checker reading the
//! transpiled python makes of a decoration is what ty made of it: the decorated
//! parameter carries the type the source declared, the options are declared in
//! both shapes the decorator is applied in, and both state the return type — the
//! one the source wrote, or the one ty reads off the body when it wrote none
//!
//! the dispatcher calls itself under a name of its own, bound beside it, because the
//! options are free to take the name it is declared under: an option named `d` left
//! `d(fn, d=d)` calling the option. `inner` is a name of its own for the same reason,
//! and both are names the module does not spell
//!
//! a stub is never run, so it carries the two overloads alone

use std::cell::RefCell;
use std::collections::HashMap;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{
    Expr, ModModule, ParameterWithDefault, PythonVersion, Stmt, StmtFunctionDef,
};
use ruff_text_size::{Ranged, TextRange, TextSize};

use super::ast_driver::{AstPass, Fragment, PassContext};
use super::repeated_underscore::WrittenNames;
use crate::type_info::{SynthesizedType, TypeInfo};

/// The return type of each `decorator def` that wrote no return annotation, by the
/// function's source range.
///
/// Both emitted overloads state the return type, and a `decorator def` need not have
/// written one — `decorator def d(fn): return len(str(fn))` returns `int`. The lowering
/// runs on the syntax tree and has no type information of its own, so the types are read
/// here, from the parse the checker answered about, in the way
/// [`literal_string::collect`](super::literal_string::collect) reads its own.
#[derive(Default)]
pub(crate) struct InferredReturnTypes {
    by_function: HashMap<TextRange, SynthesizedType>,
}

/// Read the inferred return type of every `decorator def` in `stmts` that left its return
/// annotation out. `stmts` must come from the same parse `types` answers for.
pub(crate) fn collect_return_types(
    stmts: &[Stmt],
    types: &dyn TypeInfo,
    min_version: PythonVersion,
) -> InferredReturnTypes {
    let mut collector = ReturnTypes {
        types,
        min_version,
        found: InferredReturnTypes::default(),
    };
    for stmt in stmts {
        collector.visit_stmt(stmt);
    }
    collector.found
}

struct ReturnTypes<'a> {
    types: &'a dyn TypeInfo,
    min_version: PythonVersion,
    found: InferredReturnTypes,
}

impl<'ast> Visitor<'ast> for ReturnTypes<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt
            && has_decorator_keyword_marker(function)
            && let Some(annotation) = self
                .types
                .inferred_return_annotation(function, self.min_version)
        {
            self.found.by_function.insert(function.range(), annotation);
        }
        walk_stmt(self, stmt);
    }
}

/// whether the parser marked `function` as a `decorator def`. The marker decorator is
/// synthesized, so it carries the invalid expression context rather than an `@`
fn has_decorator_keyword_marker(function: &StmtFunctionDef) -> bool {
    function.decorator_list.iter().any(|decorator| {
        matches!(&decorator.expression, Expr::Name(name)
            if name.ctx.is_invalid() && name.id.as_str() == "decorator_keyword")
    })
}

pub(crate) struct DecoratorKeyword<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    is_stub: bool,
    return_types: InferredReturnTypes,
}

impl<'src> DecoratorKeyword<'src> {
    pub(crate) fn new(
        source: &'src str,
        written: WrittenNames<'src>,
        is_stub: bool,
        return_types: InferredReturnTypes,
    ) -> Self {
        Self {
            source,
            written,
            is_stub,
            return_types,
        }
    }
}

impl AstPass for DecoratorKeyword<'_> {
    /// a `decorator def` writes its options' signature itself, without their parameter
    /// modifiers
    fn subsumes(&self) -> &'static [super::ast_driver::Lowering] {
        &[
            super::ast_driver::Lowering::LocalOnce,
            super::ast_driver::Lowering::ContextParams,
        ]
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut state = State {
            source: self.source,
            written: self.written,
            is_stub: self.is_stub,
            return_types: &self.return_types,
            templates: RefCell::new(Vec::new()),
            errors: RefCell::new(Vec::new()),
            needs_callable: false,
            needs_overload: false,
            synthesized_modules: RefCell::new(Vec::new()),
            synthesized_imports: RefCell::new(Vec::new()),
            annotations_evaluated: ctx.annotations_evaluated,
            class_depth: 0,
        };
        for stmt in &module.body {
            state.visit_stmt(stmt);
        }
        if state.needs_callable {
            ctx.required_imports
                .push(self.written.import_from("typing", &["Callable"]));
        }
        if state.needs_overload {
            ctx.required_imports
                .push(self.written.import_from("typing", &["overload"]));
        }
        ctx.type_only_imports
            .extend(state.synthesized_modules.into_inner());
        ctx.required_imports
            .extend(state.synthesized_imports.into_inner());
        ctx.template_edits.extend(state.templates.into_inner());
        ctx.errors.extend(state.errors.into_inner());
    }
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about the module and the imports the visit needs"
)]
struct State<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
    is_stub: bool,
    return_types: &'src InferredReturnTypes,
    templates: RefCell<Vec<(TextRange, Vec<Fragment>)>>,
    errors: RefCell<Vec<String>>,
    needs_callable: bool,
    needs_overload: bool,
    /// the modules a written-out return type qualifies a class with that the source never
    /// imported under their own names
    synthesized_modules: RefCell<Vec<String>>,
    /// the imports of the `typing` names a written-out return type reads
    synthesized_imports: RefCell<Vec<String>>,
    /// whether python evaluates an overload's annotations as it is defined
    annotations_evaluated: bool,
    class_depth: u32,
}

impl State<'_> {
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

    fn template(&self, range: TextRange, frags: Vec<Fragment>) {
        self.templates.borrow_mut().push((range, frags));
    }

    fn error(&self, msg: String) {
        self.errors.borrow_mut().push(msg);
    }

    fn process_function(&mut self, func: &StmtFunctionDef) {
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

        self.needs_overload = true;
        let callable = self.written.imported("typing", "Callable");
        let overload = self.written.imported("typing", "overload");

        let fn_name = func.name.as_str();

        let base_indent = self.line_indent(func.range().start()).to_owned();
        let body_indent = format!("{base_indent}    ");

        let python_name = |parameter: &ParameterWithDefault| {
            crate::python_parameter_name(params, &parameter.parameter, self.written)
        };
        // the body reads the decorated callable by the name its parameter declares
        let decorated = python_name(fn_param);

        // the declared types and defaults are passed through as source spans, so a
        // lowering inside one — an arrow callable type, a `T?`, a `??` default — is
        // materialized where this template re-emits it. written as text of our own
        // they would be dropped, and the basedpython surface would reach the `.py`
        let decorated_type = |frags: &mut Vec<Fragment>| match fn_param.annotation() {
            Some(annotation) => frags.push(Fragment::Src(annotation.range())),
            // an unannotated decorated parameter accepts any callable
            None => frags.push(Fragment::Lit(format!("{callable}[..., object]"))),
        };
        // a declaration that wrote no return annotation still has a return type, and both
        // overloads state it: written as `object` the emitted python would be weaker than
        // the source, and a caller would lose the type the decoration produces. `object`
        // remains the fallback for a type with no python spelling, which is sound — it
        // says less than the truth rather than something else
        let inferred_return = self.return_types.by_function.get(&func.range());
        if let Some(inferred) = inferred_return {
            self.synthesized_modules
                .borrow_mut()
                .extend(inferred.modules.iter().cloned());
            self.synthesized_imports
                .borrow_mut()
                .extend(inferred.typing_imports_in(self.written));
        }
        let inferred_return = inferred_return
            .map(|inferred| inferred.annotation_in(self.written, self.annotations_evaluated));
        let return_type = |frags: &mut Vec<Fragment>| match (&func.returns, &inferred_return) {
            (Some(returns), _) => frags.push(Fragment::Src(returns.range())),
            (None, Some(inferred)) => frags.push(Fragment::Lit(inferred.clone())),
            (None, None) => frags.push(Fragment::Lit(self.written.builtin("object"))),
        };
        // the second overload's return type is a `Callable` whatever the decorated
        // parameter declares
        self.needs_callable = true;
        // the decorated parameter is declared as the source declares it, so applying the
        // decorator by keyword (`d(fn=f)`) is accepted here exactly where it is accepted
        // of the declaration
        // the first positional is the one in `posonlyargs` whenever there is one there
        let decorated_marker = if params.posonlyargs.is_empty() {
            ""
        } else {
            ", /"
        };
        // the options are keyword-only at every call site: one dispatcher serves each
        // shape, and it tells them apart by whether the decorated function arrived
        // positionally
        let declared_options = |frags: &mut Vec<Fragment>| {
            for option in &options {
                frags.push(Fragment::Lit(format!(
                    ", {name}",
                    name = python_name(option)
                )));
                if let Some(annotation) = option.annotation() {
                    frags.push(Fragment::Lit(": ".to_owned()));
                    frags.push(Fragment::Src(annotation.range()));
                }
                frags.push(Fragment::Lit(" = ...".to_owned()));
            }
        };

        // overload 1: applied to the function itself, with whatever options were given
        let mut header: Vec<Fragment> = Vec::new();
        header.push(Fragment::Lit(format!(
            "@{overload}\n{base_indent}def {fn_name}({decorated}: "
        )));
        decorated_type(&mut header);
        if !options.is_empty() {
            header.push(Fragment::Lit(format!("{decorated_marker}, *")));
        } else if !decorated_marker.is_empty() {
            header.push(Fragment::Lit(decorated_marker.to_owned()));
        }
        declared_options(&mut header);
        header.push(Fragment::Lit(") -> ".to_owned()));
        return_type(&mut header);
        // overload 2: applied to the options alone, and what it hands back takes the
        // function
        header.push(Fragment::Lit(format!(
            ": ...\n{base_indent}@{overload}\n{base_indent}def {fn_name}("
        )));
        if !options.is_empty() {
            header.push(Fragment::Lit("*".to_owned()));
            declared_options(&mut header);
        }
        header.push(Fragment::Lit(format!(") -> {callable}[[")));
        decorated_type(&mut header);
        header.push(Fragment::Lit("], ".to_owned()));
        return_type(&mut header);
        header.push(Fragment::Lit("]: ...".to_owned()));
        // the overloads alone are the declaration; everything past here is what runs
        let declaration_end = header.len();

        header.push(Fragment::Lit(format!(
            "\n{base_indent}def {fn_name}({decorated}=None"
        )));
        if !options.is_empty() {
            header.push(Fragment::Lit(", *".to_owned()));
            for option in &options {
                header.push(Fragment::Lit(format!(
                    ", {name}=",
                    name = python_name(option)
                )));
                match option.default.as_deref() {
                    Some(default) => header.push(Fragment::Src(default.range())),
                    None => header.push(Fragment::Lit("None".to_owned())),
                }
            }
        }
        header.push(Fragment::Lit("):".to_owned()));

        // a stub declares and never runs, so it carries the two overloads alone: the
        // dispatcher and the name it reaches itself under are machinery for calls, and a
        // `.pyi` holds none
        if self.is_stub {
            header.truncate(declaration_end);
            self.template(
                TextRange::new(deco.range().start(), func.range().end()),
                header,
            );
            return;
        }

        let recursive_kw_args: String = options
            .iter()
            .map(|o| {
                let name = python_name(o);
                format!("{name}={name}")
            })
            .collect::<Vec<_>>()
            .join(", ");
        // the dispatcher reaches itself through a name of its own rather than through the
        // one it is declared under, which the options are free to take: an option named `d`
        // left `d(fn, d=d)` calling the option
        let self_name = self.written.fresh(&format!("_by_{fn_name}"));
        // and the function it hands back is bound under a name of its own too, for the same
        // reason — an option named `inner` was handed the dispatcher itself
        let inner_name = self.written.fresh("inner");
        let recursive_call = if options.is_empty() {
            format!("{self_name}({decorated})")
        } else {
            format!("{self_name}({decorated}, {recursive_kw_args})")
        };

        let dispatch = |prefix: &str| {
            format!(
                "{prefix}if {decorated} is None:\n{body_indent}    def {inner_name}({decorated}):\n{body_indent}        return {recursive_call}\n{body_indent}    return {inner_name}\n{body_indent}"
            )
        };
        // the name is bound after the declaration, where the dispatcher exists; a lowering
        // inside it only reads the name when it runs
        let bind_self = format!("\n{base_indent}{self_name} = {fn_name}");

        // a `decorator def` with no body is a declaration — `decorator def d(fn)`
        // means the same as `decorator def d(fn): ...`, just as a bodyless plain
        // `def` does. the source carries no colon and no body statement to anchor
        // edits on, so the whole declaration is replaced in one go
        let Some(first_stmt) = func.body.first() else {
            header.push(Fragment::Lit(format!(
                "{body}...{bind_self}",
                body = dispatch(&format!("\n{body_indent}"))
            )));
            self.template(
                TextRange::new(deco.range().start(), func.range().end()),
                header,
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

        self.template(TextRange::new(deco.range().start(), header_end), header);

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

        self.template(
            TextRange::new(body_first_pos, body_first_pos),
            vec![Fragment::Lit(dispatch(&prefix))],
        );
        let end = func.range().end();
        self.template(TextRange::new(end, end), vec![Fragment::Lit(bind_self)]);
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

    /// the options are passed as `@d(option=...)`, so a repeated `_` among them would
    /// be keyword-only, and the name only a keyword reaches it by is not the author's
    #[test]
    fn a_repeated_underscore_option_is_refused() {
        let error = transpile(
            "decorator def d(fn: (...) -> object, _: int = 1, _: int = 2) -> int:\n    return 7\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(
            error.starts_with("a repeated `_` parameter cannot be keyword-only in `d`"),
            "got:\n{error}"
        );
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
                def d(fn: Callable[..., object], *, option: bool = ...) -> int: ...
                @overload
                def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, option=False):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, option=option)
                        return inner
                    return 1 if option else len(str(fn))
                _by_d = d
            "},
        );
    }

    /// the body reads the decorated callable under the name its parameter declares, which
    /// is not always `fn`: lowered as `fn`, `func` was a `NameError` the first time the
    /// decorator ran
    #[test]
    fn the_decorated_parameter_keeps_its_name() {
        check(
            indoc! {"
                decorator def d(func: (...) -> object, option: bool = False) -> int:
                    return len(str(func))
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(func: Callable[..., object], *, option: bool = ...) -> int: ...
                @overload
                def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(func=None, *, option=False):
                    if func is None:
                        def inner(func):
                            return _by_d(func, option=option)
                        return inner
                    return len(str(func))
                _by_d = d
            "},
        );
    }

    /// the function the dispatcher hands back is bound under a name of its own: named
    /// `inner`, it stood where an option of that name was declared, and decorating with
    /// options handed the decorated function the dispatcher itself
    #[test]
    fn an_option_named_inner_keeps_its_value() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object, inner: bool = False) -> str:
                    return f\"inner={inner}\"
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], *, inner: bool = ...) -> str: ...
                @overload
                def d(*, inner: bool = ...) -> Callable[[Callable[..., object]], str]: ...
                def d(fn=None, *, inner=False):
                    if fn is None:
                        def inner2(fn):
                            return _by_d(fn, inner=inner)
                        return inner2
                    return f\"inner={inner}\"
                _by_d = d
            "},
        );
    }

    /// the dispatcher reaches itself under a name of its own: an option named as the
    /// decorator shadowed the decorator, and the dispatcher called the option instead
    #[test]
    fn an_option_named_as_the_decorator_does_not_shadow_it() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object, d: bool = False) -> str:
                    return f\"d={d}\"
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], *, d: bool = ...) -> str: ...
                @overload
                def d(*, d: bool = ...) -> Callable[[Callable[..., object]], str]: ...
                def d(fn=None, *, d=False):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, d=d)
                        return inner
                    return f\"d={d}\"
                _by_d = d
            "},
        );
    }

    /// the name the dispatcher reaches itself under is one the module does not spell
    #[test]
    fn a_module_that_spells_the_dispatcher_name_is_numbered_around() {
        check(
            indoc! {"
                _by_d = 1

                decorator def d(fn: (...) -> object) -> int:
                    return _by_d
            "},
            indoc! {"
                from typing import Callable, overload
                _by_d = 1

                @overload
                def d(fn: Callable[..., object]) -> int: ...
                @overload
                def d() -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d2(fn)
                        return inner
                    return _by_d
                _by_d2 = d
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
                def d(fn: Callable[..., object]) -> int: ...
                @overload
                def d() -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn)
                        return inner
                    return len(str(fn))
                _by_d = d
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
                def d(fn: Callable[..., object], *, a: int = ..., b: str = ...) -> int: ...
                @overload
                def d(*, a: int = ..., b: str = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, a=1, b=\"x\"):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, a=a, b=b)
                        return inner
                    return a + len(b)
                _by_d = d
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
                def d(fn: Callable[[int], None]) -> None: ...
                @overload
                def d() -> Callable[[Callable[[int], None]], None]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn)
                        return inner
                    ...
                _by_d = d
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
                def d(fn: Callable[..., object], *, option: bool = ...) -> int: ...
                @overload
                def d(*, option: bool = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, option=False):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, option=option)
                        return inner
                    ...
                _by_d = d
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
                def d(fn: Callable[[int], None]) -> None: ...
                @overload
                def d() -> Callable[[Callable[[int], None]], None]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn)
                        return inner
                    ...
                _by_d = d

                d(lambda i: None)
            "},
        );
    }

    /// the declared types reach the overloads through the source, so a type written in
    /// basedpython lowers on its way there. spelled as text of our own they were dropped:
    /// the decorated parameter was declared `Callable[..., object]` however it was written,
    /// and an option's `int?` reached the `.py` with the `?` still on it
    #[test]
    fn the_declared_types_lower_on_their_way_into_the_overloads() {
        check(
            indoc! {"
                decorator def d(fn: (int) -> str, label: int? = None) -> str?:
                    return label
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[[int], str], *, label: int | None = ...) -> str | None: ...
                @overload
                def d(*, label: int | None = ...) -> Callable[[Callable[[int], str]], str | None]: ...
                def d(fn=None, *, label=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, label=label)
                        return inner
                    return label
                _by_d = d
            "},
        );
    }

    /// an option whose default is not a scalar is re-evaluated per call, so the signature
    /// carries the `_MISSING` sentinel and the body carries the default itself. the header
    /// template re-emits the whole signature, and re-emitted the sentinel in the guard too —
    /// `if tags is _MISSING: tags = _MISSING` — so the option arrived as the sentinel object
    /// and `@d` raised `TypeError: object of type 'object' has no len()`
    #[test]
    fn a_mutable_option_default_reaches_the_guard_and_not_the_signature() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object, tags: list[str] = []) -> int:
                    return len(tags)
            "},
            indoc! {"
                from typing import Any, Callable, overload
                _MISSING: Any = object()
                @overload
                def d(fn: Callable[..., object], *, tags: list[str] = ...) -> int: ...
                @overload
                def d(*, tags: list[str] = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, tags=_MISSING):
                    if tags is _MISSING:
                        tags = []
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, tags=tags)
                        return inner
                    return len(tags)
                _by_d = d
            "},
        );
    }

    /// and a lowering written inside that default still lands in the body copy: the guard
    /// re-emits the default's own source, so only the sentinel itself stays behind
    #[test]
    fn a_lowering_inside_a_mutable_option_default_survives_the_relocation() {
        let out = transpile(
            "env: str? = None\n\ndecorator def d(fn: (...) -> object, tag: str = env ?? \"b\") -> int:\n    return len(tag)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def d(fn=None, *, tag=_MISSING):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("tag = env if env is not None else \"b\""),
            "got:\n{out}"
        );
    }

    /// the header the overloads are written from replaces each parameter's name with text
    /// of its own, so an edit a sibling pass made to the modifier prefix in front of that
    /// name lands nowhere. every prefix a parameter can carry — `local`, `once`, `context`
    /// — lowers by being deleted, which is what writing the bare name already does, so the
    /// emitted python is the same either way. this pins that: a prefix that lowered to
    /// anything else would be dropped here without a word
    #[test]
    fn a_parameter_modifier_leaves_the_overloads_as_they_were() {
        let plain = transpile(
            "decorator def d(fn: (...) -> object, tag: str = \"x\") -> int:\n    return 1\n",
            &Config::test_default(),
        )
        .unwrap();
        for modified in [
            "decorator def d(local fn: (...) -> object, tag: str = \"x\") -> int:\n    return 1\n",
            "decorator def d(once fn: (...) -> object, tag: str = \"x\") -> int:\n    return 1\n",
            "decorator def d(fn: (...) -> object, context tag: str = \"x\") -> int:\n    return 1\n",
        ] {
            assert_eq!(
                transpile(modified, &Config::test_default()).unwrap(),
                plain,
                "modifiers changed the output of:\n{modified}"
            );
        }
    }

    /// the declaration is always the pair of overloads, and an option is keyword-only in
    /// both of them, so a call site can fill it: the factory form `@d()` writes the keyword
    /// into the argument list it already has, and a plain call writes it too. the bare `@d`
    /// has no argument list, which is what the checker reports
    #[test]
    fn an_option_declared_context_is_filled_where_there_is_an_argument_list() {
        check(
            indoc! {r#"
                decorator def d(fn: (...) -> object, context tag: str = "x") -> int:
                    return len(tag)

                context t: str = "hello"

                @d()
                def g(): ...

                def plain(): ...

                k = d(plain)
            "#},
            indoc! {r#"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object], *, tag: str = ...) -> int: ...
                @overload
                def d(*, tag: str = ...) -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None, *, tag="x"):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn, tag=tag)
                        return inner
                    return len(tag)
                _by_d = d

                t: str = "hello"

                @d(tag=t)
                def g(): ...

                def plain(): ...

                k = d(plain, tag=t)
            "#},
        );
    }

    /// a declaration that writes no return annotation still has a return type, and both
    /// overloads state it. written as `object` — which is all the lowering can see on its
    /// own — the emitted python would be weaker than the source: `@d` would produce
    /// `object` where the checker reading the `.by` says `int`
    #[test]
    fn an_unwritten_return_type_reaches_the_overloads() {
        check(
            indoc! {"
                decorator def d(fn: (...) -> object):
                    return len(str(fn))
            "},
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object]) -> int: ...
                @overload
                def d() -> Callable[[Callable[..., object]], int]: ...
                def d(fn=None):
                    if fn is None:
                        def inner(fn):
                            return _by_d(fn)
                        return inner
                    return len(str(fn))
                _by_d = d
            "},
        );
    }

    /// a `decorator def` with no body declares a signature and returns `None`, which is
    /// what the checker reads there too
    #[test]
    fn a_bodyless_declaration_returns_none() {
        let out = transpile(
            "decorator def d(fn: (...) -> object)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def d(fn: Callable[..., object]) -> None: ...")
                && out.contains("def d() -> Callable[[Callable[..., object]], None]: ..."),
            "got:\n{out}"
        );
    }

    /// a type with no spelling the emitted file could resolve is not written: `object`
    /// stands, which says less than the truth rather than something else
    #[test]
    fn a_return_type_the_output_could_not_resolve_stays_object() {
        // ty's answer here is `Literal[1]`, and `Literal` is not one of the names
        // basedpython binds implicitly, so there is no import to write for it
        let out = transpile(
            "decorator def d(fn: (...) -> object):\n    return 1\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.contains("def d(fn: Callable[..., object]) -> object: ..."),
            "got:\n{out}"
        );
    }

    /// a stub is never run, so the dispatcher it would declare is a definition nothing
    /// calls, and the name it reaches itself under is a binding nothing reads
    #[test]
    fn a_stub_declares_the_overloads_alone() {
        let out = transpile(
            "decorator def route(fn: (int) -> None, prefix: str = \"\")\n",
            &Config {
                is_stub: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            indoc! {"
                from typing import Callable, overload
                @overload
                def route(fn: Callable[[int], None], *, prefix: str = ...) -> None: ...
                @overload
                def route(*, prefix: str = ...) -> Callable[[Callable[[int], None]], None]: ...
            "}
        );
    }

    /// and one written with a body drops that too — a `.pyi` declares, and the body the
    /// dispatcher would call is not part of the declaration
    #[test]
    fn a_stub_with_a_body_declares_the_overloads_alone() {
        let out = transpile(
            "decorator def d(fn: (...) -> object) -> int:\n    return 1\n",
            &Config {
                is_stub: true,
                ..Config::test_default()
            },
        )
        .unwrap();
        assert_eq!(
            out,
            indoc! {"
                from typing import Callable, overload
                @overload
                def d(fn: Callable[..., object]) -> int: ...
                @overload
                def d() -> Callable[[Callable[..., object]], int]: ...
            "}
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
