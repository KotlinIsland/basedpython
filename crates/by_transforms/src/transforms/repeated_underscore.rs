//! the lowering of a parameter list that repeats `_`
//!
//! basedpython allows `def f(_, _, _): ...` as a shorthand for ignoring several
//! positional parameters. python rejects it with a duplicate-parameter error, so each
//! `_` after the first is renamed to a fresh `_<n>` (`_2`, `_3`, ...), skipping any
//! name the module spells, and a `/` after the last of them makes them positional-only:
//! the numbered name is the lowering's rather than the author's, so no call may spell
//! it. a method that overrides one takes the names the overridden method gives those
//! positions instead, and their kinds with them
//!
//! which names and where the `/` goes is ty's answer, the one its signature of the
//! definition is built from (`ty_python_semantic::types::repeated_underscore`), so a
//! call ty accepts is one the python accepts. a shape ty refuses is refused here too.
//! references to `_` inside the body are left alone and resolve to the first parameter

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::transformer::{Transformer, walk_expr, walk_stmt};
use ruff_python_ast::visitor::{
    Visitor, walk_expr as visit_walk_expr, walk_stmt as visit_walk_stmt,
};
use ruff_python_ast::{Expr, ModModule, Parameter, Parameters, Stmt};
use ruff_python_trivia::{SimpleTokenKind, SimpleTokenizer};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::types::repeated_underscore::{
    self as decision, UnderscoreLowering, UnderscoreRefusal, callable_parameter_slots,
    lower_repeated_underscores, lowered_parameter_names, parameter_slots,
};

pub(crate) use ty_python_semantic::types::repeated_underscore::{
    ParameterSlot, protocol_method_receiver,
};

use super::ast_driver::{AstPass, PassContext};
use crate::type_info::TypeInfo;

type Decision = Result<UnderscoreLowering, UnderscoreRefusal>;

/// how every parameter list of a module that repeats `_` is lowered, by the range of the
/// list — read off ty before any pass rewrites the syntax tree, so the passes that walk a
/// tree of their own can still ask
#[derive(Debug)]
pub(crate) struct UnderscoreLowerings {
    decisions: HashMap<TextRange, Decision>,
    /// whether the python the module targets has the `/` a repeated `_` needs
    slash: bool,
}

/// ty's answer for every parameter list in `suite` that repeats `_`, the module targeting a
/// python that has the `/` when `slash`
pub(crate) fn collect(suite: &[Stmt], types: &dyn TypeInfo, slash: bool) -> UnderscoreLowerings {
    struct Collector<'a> {
        types: &'a dyn TypeInfo,
        lowerings: UnderscoreLowerings,
    }
    impl Visitor<'_> for Collector<'_> {
        fn visit_stmt(&mut self, stmt: &Stmt) {
            if let Stmt::FunctionDef(function) = stmt
                && repeats_underscore(&function.parameters)
                && let Some(decision) = self.types.repeated_underscore_lowering(function)
            {
                self.lowerings
                    .decisions
                    .insert(function.parameters.range, decision);
            }
            visit_walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Lambda(lambda) = expr
                && let Some(parameters) = lambda.parameters.as_deref()
                && let Some(decision) = standalone_lowering(parameters, self.lowerings.slash)
            {
                self.lowerings.decisions.insert(parameters.range, decision);
            }
            visit_walk_expr(self, expr);
        }
    }
    let mut collector = Collector {
        types,
        lowerings: UnderscoreLowerings {
            decisions: HashMap::new(),
            slash,
        },
    };
    collector.visit_body(suite);
    collector.lowerings
}

/// how `parameters` is lowered read on its own, with no method it overrides and no receiver
/// to consult. a lambda has neither, so this is the answer ty gives a lambda too
fn standalone_lowering(parameters: &Parameters, slash: bool) -> Option<Decision> {
    lower_repeated_underscores(&parameter_slots(parameters), false, None, |_| false, slash)
}

fn repeats_underscore(parameters: &Parameters) -> bool {
    parameters
        .iter()
        .filter(|parameter| parameter.name() == "_")
        .nth(1)
        .is_some()
}

/// the names the module spells as written, and how the parameter lists in it that repeat
/// `_` are lowered — every name the lowering writes that the source did not
///
/// a repeated `_` is numbered to a name outside the ones the module spells, so the
/// parameter cannot shadow a name the function reads from an enclosing scope:
///
/// ```by
/// _2 = [9]
///
/// def f(_: int, _: int) -> list[int]:
///     return _2  # the module's `_2`, so the second parameter is `_3`
/// ```
///
/// without the lowerings ty decided — the native compiler reads a module on its own —
/// a repeated `_` is numbered and nothing takes a name from an overridden method
#[derive(Clone, Copy, Debug)]
pub struct WrittenNames<'src> {
    names: decision::WrittenNames<'src>,
    lowerings: Option<&'src UnderscoreLowerings>,
}

impl<'src> WrittenNames<'src> {
    /// the names spelled in `source`, the text of a module as it was written
    pub fn new(source: &'src str) -> Self {
        Self {
            names: decision::WrittenNames::new(source),
            lowerings: None,
        }
    }

    /// these names, with `lowerings` saying how each parameter list that repeats `_` is
    /// lowered
    pub(crate) fn with_lowerings<'a>(self, lowerings: &'a UnderscoreLowerings) -> WrittenNames<'a>
    where
        'src: 'a,
    {
        WrittenNames {
            names: self.names,
            lowerings: Some(lowerings),
        }
    }

    /// a name the module spells nowhere, `stem` itself when the module does not spell it
    /// and `stem2`, `stem3`, … when it does
    ///
    /// a binding the lowering writes under a name the source wrote would shadow it — the
    /// `inner` a `decorator def` writes standing where an option of that name is declared,
    /// which the dispatcher then passed the dispatcher itself for
    pub(crate) fn fresh(self, stem: &str) -> String {
        self.names.fresh(stem)
    }

    /// how `parameters` is lowered when it repeats `_`. without ty's answer, the list is
    /// read on its own, as a lambda's is: it takes no name from a method it overrides
    /// whether the python the module targets has the `/` a repeated `_` needs. without ty's
    /// answers, as for a native build, it is assumed to
    fn slash(self) -> bool {
        self.lowerings.is_none_or(|lowerings| lowerings.slash)
    }

    fn lowering(self, parameters: &Parameters) -> Option<Decision> {
        match self.lowerings {
            Some(lowerings) => lowerings.decisions.get(&parameters.range).cloned(),
            None => standalone_lowering(parameters, true),
        }
    }

    /// the name each of `parameters` binds in the python, in declaration order
    fn names(self, parameters: &Parameters) -> Vec<Name> {
        let slots = parameter_slots(parameters);
        match self.lowering(parameters) {
            Some(Ok(lowering)) => lowering.names(&slots, self.names),
            _ => lowered_parameter_names(&slots, self.names),
        }
    }
}

/// the name `parameter`, one of `parameters`, binds in the python a basedpython
/// definition lowers to. `written` is the module the definition is in
///
/// a native build publishes the same definition, and has to answer to the same
/// names: a keyword argument, the forwarder's signature and `__code__` all spell
/// them
pub fn python_parameter_name(
    parameters: &Parameters,
    parameter: &Parameter,
    written: WrittenNames,
) -> Name {
    parameters
        .iter()
        .zip(written.names(parameters))
        .find(|(candidate, _)| std::ptr::eq(candidate.as_parameter(), parameter))
        .map_or_else(|| parameter.name.id.clone(), |(_, name)| name)
}

/// the parameters a callable type is declared with in the python — the `__call__` of the
/// protocol a callable type naming its parameters lowers to, or the method of an inline
/// protocol
pub(crate) struct LoweredCallableParameters {
    /// the name each written parameter is declared under, in order, the receiver of a
    /// protocol's method left out
    pub(crate) names: Vec<Name>,
    /// how many of the written parameters the `/` comes after, when a repeated `_` puts
    /// one there
    pub(crate) slash: Option<usize>,
}

/// how the parameters of `ct` are declared, `method` saying it is the signature of an
/// inline protocol's method. a repeated `_` is lowered as in a `def`: every `_` after the
/// first is numbered, and made positional-only by a `/` after the last of them — ty's
/// answer for the same callable type. a shape it refuses is reported by the pass that
/// lowers parameter lists, and is written here with its `_`s numbered alone
pub(crate) fn lowered_callable_parameters(
    ct: &ruff_python_ast::ExprCallableType,
    method: bool,
    written: WrittenNames,
) -> LoweredCallableParameters {
    let slots = callable_parameter_slots(ct, method);
    let spelled: Vec<(ParameterSlot, &str)> = slots
        .iter()
        .map(|(slot, name)| (*slot, name.as_str()))
        .collect();
    let method_receiver = method && protocol_method_receiver(ct).is_some();
    let receivers = usize::from(method_receiver) + usize::from(ct.receiver.is_some());
    let (mut names, positional_only) = match lower_repeated_underscores(
        &spelled,
        method_receiver,
        None,
        |_| false,
        written.slash(),
    ) {
        Some(Ok(lowering)) => (
            lowering.names(&spelled, written.names),
            lowering.positional_only(),
        ),
        _ => (lowered_parameter_names(&spelled, written.names), 0),
    };
    LoweredCallableParameters {
        names: names.split_off(receivers.min(names.len())),
        slash: positional_only
            .checked_sub(receivers)
            .filter(|&written| written > 0),
    }
}

/// how many of the leading positional `parameters` the python takes by position alone
pub(crate) fn positional_only_count(parameters: &Parameters, written: WrittenNames) -> usize {
    match written.lowering(parameters) {
        Some(Ok(lowering)) => lowering.positional_only(),
        _ => parameters.posonlyargs.len(),
    }
}

/// the name a read of `_` in the body of a definition with `parameters` means, when the
/// lowering binds no parameter to `_` any more: the first `_` took a name from the method
/// the definition overrides, and a read of `_` is still the first `_`'s value
pub(crate) fn rebound_underscore(parameters: &Parameters, written: WrittenNames) -> Option<Name> {
    let Some(Ok(lowering)) = written.lowering(parameters) else {
        return None;
    };
    if !lowering.is_inherited() {
        return None;
    }
    parameters
        .iter()
        .zip(written.names(parameters))
        .find(|(parameter, _)| parameter.name() == "_")
        .map(|(_, name)| name)
}

/// whether `body` reads `_`, anywhere in it
pub(crate) fn reads_underscore(body: &[Stmt]) -> bool {
    struct Reads {
        found: bool,
    }
    impl Visitor<'_> for Reads {
        fn visit_stmt(&mut self, stmt: &Stmt) {
            if let Stmt::AugAssign(node) = stmt
                && matches!(node.target.as_ref(), Expr::Name(name) if name.id == "_")
            {
                self.found = true;
            }
            visit_walk_stmt(self, stmt);
        }

        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Name(name) = expr
                && name.id == "_"
                && !name.ctx.is_store()
            {
                self.found = true;
            }
            visit_walk_expr(self, expr);
        }
    }
    let mut reads = Reads { found: false };
    reads.visit_body(body);
    reads.found
}

pub(crate) struct RepeatedUnderscore<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
}

impl<'src> RepeatedUnderscore<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>) -> Self {
        Self { source, written }
    }
}

impl AstPass for RepeatedUnderscore<'_> {
    fn lowering(&self) -> Option<super::ast_driver::Lowering> {
        Some(super::ast_driver::Lowering::RepeatedUnderscore)
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        for (index, stmt) in module.body.iter_mut().enumerate() {
            let lowerer = Lowerer {
                pass: self,
                changed: Cell::new(false),
                edits: RefCell::default(),
                errors: RefCell::default(),
                method_signatures: RefCell::default(),
            };
            lowerer.visit_stmt(stmt);
            if lowerer.changed.get() {
                ctx.changed.push(index);
            }
            ctx.text_edits.extend(lowerer.edits.into_inner());
            ctx.errors.extend(lowerer.errors.into_inner());
        }
    }
}

struct Lowerer<'a, 'src> {
    pass: &'a RepeatedUnderscore<'src>,
    changed: Cell<bool>,
    edits: RefCell<Vec<(TextRange, String)>>,
    errors: RefCell<Vec<String>>,
    /// the signatures of the inline protocol methods met so far, which are not callable
    /// types of their own
    method_signatures: RefCell<Vec<TextRange>>,
}

impl Lowerer<'_, '_> {
    fn lower(&self, params: &mut Parameters, function: Option<&str>) {
        let Some(decision) = self.pass.written.lowering(params) else {
            return;
        };
        let lowering = match decision {
            Ok(lowering) => lowering,
            Err(refusal) => {
                let owner =
                    function.map_or_else(|| "a lambda".to_owned(), |name| format!("`{name}`"));
                self.errors.borrow_mut().push(format!(
                    "{} in {owner}: {}",
                    refusal.message(),
                    refusal.help()
                ));
                return;
            }
        };
        self.place_slash(params, lowering.positional_only());
        let names = self.pass.written.names(params);
        let parameters = params
            .posonlyargs
            .iter_mut()
            .chain(params.args.iter_mut())
            .map(|parameter| &mut parameter.parameter)
            .chain(params.vararg.as_deref_mut())
            .chain(
                params
                    .kwonlyargs
                    .iter_mut()
                    .map(|parameter| &mut parameter.parameter),
            )
            .chain(params.kwarg.as_deref_mut());
        for (parameter, name) in parameters.zip(names) {
            if parameter.name.id != name {
                parameter.name.id = name;
                self.changed.set(true);
            }
        }
    }

    /// report the parameters of `callable` when they repeat `_` in a shape the lowering
    /// refuses, `owner` naming what they belong to
    fn refuse_callable(
        &self,
        callable: &ruff_python_ast::ExprCallableType,
        method: bool,
        owner: &str,
    ) {
        let slots = callable_parameter_slots(callable, method);
        let spelled: Vec<(ParameterSlot, &str)> = slots
            .iter()
            .map(|(slot, name)| (*slot, name.as_str()))
            .collect();
        let receiver = method && protocol_method_receiver(callable).is_some();
        if let Some(Err(refusal)) = lower_repeated_underscores(
            &spelled,
            receiver,
            None,
            |_| false,
            self.pass.written.slash(),
        ) {
            self.errors.borrow_mut().push(format!(
                "{} in {owner}: {}",
                refusal.message(),
                refusal.help()
            ));
        }
    }

    /// move the `/` to after the first `positional_only` parameters, as edits of the
    /// source. the header is where the passes before this one made their edits, and
    /// printing it from the tree would drop them
    fn place_slash(&self, params: &Parameters, positional_only: usize) {
        let written = params.posonlyargs.len();
        if positional_only <= written {
            return;
        }
        let Some(last) = params
            .posonlyargs
            .iter()
            .chain(&params.args)
            .nth(positional_only - 1)
        else {
            return;
        };
        let mut edits = self.edits.borrow_mut();
        if let (Some(before), Some(after)) = (params.posonlyargs.last(), params.args.first()) {
            let gap = TextRange::new(before.end(), after.start());
            if let Some(slash) = SimpleTokenizer::new(self.pass.source, gap)
                .find(|token| token.kind() == SimpleTokenKind::Slash)
            {
                edits.push((TextRange::new(slash.start(), after.start()), String::new()));
            }
        }
        edits.push((TextRange::empty(last.end()), ", /".to_owned()));
    }
}

impl Transformer for Lowerer<'_, '_> {
    fn visit_stmt(&self, stmt: &mut Stmt) {
        if let Stmt::FunctionDef(f) = stmt {
            let name = f.name.id.to_string();
            self.lower(&mut f.parameters, Some(&name));
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&self, expr: &mut Expr) {
        if let Expr::Lambda(l) = expr
            && let Some(params) = l.parameters.as_deref_mut()
        {
            self.lower(params, None);
        }
        // a callable type or an inline protocol's method is written as a `__call__` or a
        // method by the lowerings of those types, from the same answer. a shape it refuses
        // is refused here, where every other parameter list's is
        match expr {
            Expr::ProtocolMethod(method) => {
                if let Expr::CallableType(signature) = method.signature.as_ref() {
                    self.method_signatures.borrow_mut().push(signature.range);
                    self.refuse_callable(signature, true, &format!("`{}`", method.name.id));
                }
            }
            Expr::CallableType(callable)
                if !self.method_signatures.borrow().contains(&callable.range) =>
            {
                self.refuse_callable(callable, false, "a callable type");
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;

    use crate::python_passthrough::unchanged;
    use crate::{Config, transpile};

    fn check(input: &str, expected: &str) {
        assert_eq!(transpile(input, &Config::test_default()).unwrap(), expected);
    }

    fn refused(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap_err()
    }

    #[test]
    fn two_underscores() {
        check(
            "def f(_, _):\n    print(_)\n",
            "def f(_, _2, /):\n    print(_)\n",
        );
    }

    /// the `/` goes after the last `_`, so a named parameter after it keeps its keyword
    #[test]
    fn three_underscores() {
        check(
            "def g(_, _, _, x):\n    return _\n",
            "def g(_, _2, _3, /, x):\n    return _\n",
        );
    }

    #[test]
    fn lambda_underscores() {
        check("f = lambda _, _: 1\n", "f = lambda _, _2, /: 1\n");
    }

    #[test]
    fn nested_function() {
        check(
            "def outer(_, _):\n    def inner(_, _):\n        return _\n    return _\n",
            "def outer(_, _2, /):\n    def inner(_, _2, /):\n        return _\n    return _\n",
        );
    }

    /// a method's receiver is never passed by keyword, so the `/` may pass it
    #[test]
    fn a_method_receiver_is_made_positional_only() {
        check(
            "class A:\n    def f(self, _, _):\n        pass\n",
            "class A:\n    def f(self, _, _2, /):\n        pass\n",
        );
    }

    #[test]
    fn a_default_stays_before_the_slash() {
        check(
            "def f(_: int, _: int = 1) -> None: ...\n",
            "def f(_: int, _2: int = 1, /) -> None: ...\n",
        );
    }

    /// a `/` the source wrote before the last `_` moves after it
    #[test]
    fn a_written_slash_moves_after_the_last_underscore() {
        check(
            "def f(a, /, _, _):\n    return a\n",
            "def f(a, _, _2, /):\n    return a\n",
        );
    }

    #[test]
    fn a_written_slash_after_the_last_underscore_stays() {
        check(
            "def h(a, _, b, _, /):\n    return a + b\n",
            "def h(a, _, b, _2, /):\n    return a + b\n",
        );
    }

    /// `*_` is reached by no keyword, so nothing before it has to be positional-only
    #[test]
    fn vararg_underscore() {
        check(
            "def f(a, _, *_):\n    return _\n",
            "def f(a, _, *_2):\n    return _\n",
        );
    }

    /// a `/` would make `a` and `b` positional-only too, which is the author's to write
    #[test]
    fn a_named_parameter_before_the_last_underscore_is_refused() {
        assert_eq!(
            refused("def h(a, _, b, _):\n    return a + b\n"),
            "the repeated `_` parameters after `a` make it positional-only in `h`: write the `/` \
             after the last `_` to make them positional-only"
        );
        assert_eq!(
            refused("f = lambda a, _, _: a\n"),
            "the repeated `_` parameters after `a` make it positional-only in a lambda: write the \
             `/` after the last `_` to make them positional-only"
        );
    }

    #[test]
    fn a_keyword_only_repeated_underscore_is_refused() {
        assert_eq!(
            refused("def f(_, *, _):\n    pass\n"),
            "a repeated `_` parameter cannot be keyword-only in `f`: only a keyword reaches it, \
             and it has no name of its own: move it before the `*`, or name it"
        );
    }

    /// the numbered name of a repeated `_` is a local of the function, so a name the
    /// module binds under it would read the argument instead. python's answer here is
    /// `[9]`, and the second parameter answered it — in `g`'s body, and in the guard
    /// that re-evaluates `f`'s mutable default there
    #[test]
    fn a_numbered_name_skips_a_name_the_module_binds() {
        check(
            "_2: list[int] = [9]\n\ndef f(_: int, _: int, x: list[int] = _2) -> list[int]:\n    return x\n\ndef g(_: int, _: int) -> list[int]:\n    return _2\n",
            "from typing import Any\n_MISSING: Any = object()\n_2: list[int] = [9]\n\ndef f(_: int, _3: int, /, x: list[int] = _MISSING) -> list[int]:\n    if x is _MISSING:\n        x = _2\n    return x\n\ndef g(_: int, _3: int, /) -> list[int]:\n    return _2\n",
        );
    }

    /// what the module spells is read off its text, so a name reached any other way —
    /// through `globals()`, or as an attribute — is skipped too, and so is one written
    /// only in a comment
    #[test]
    fn a_numbered_name_skips_a_name_spelled_anywhere() {
        check(
            "def f(_, _):\n    return globals()[\"_2\"]\n",
            "def f(_, _3, /):\n    return globals()[\"_2\"]\n",
        );
        check(
            "def f(_, _, _):\n    return self._3  # _2\n",
            "def f(_, _4, _5, /):\n    return self._3  # _2\n",
        );
    }

    /// a name is spelled only as a whole: `__2`, `a_2` and `_2b` spell no `_2`
    #[test]
    fn a_longer_name_does_not_spell_a_numbered_one() {
        check(
            "__2 = a_2 = _2b = 1\ndef f(_, _):\n    return __2 + a_2 + _2b\n",
            "__2 = a_2 = _2b = 1\ndef f(_, _2, /):\n    return __2 + a_2 + _2b\n",
        );
    }

    /// a name the source gives a parameter is not a repeated `_`, so the numbering skips it
    #[test]
    fn existing_collision() {
        check(
            "def f(_, _2, _, /):\n    return _\n",
            "def f(_, _2, _3, /):\n    return _\n",
        );
    }

    #[test]
    fn single_underscore_unchanged() {
        unchanged("def f(_):\n    return _\n");
    }

    #[test]
    fn no_underscore_unchanged() {
        unchanged("def f(a, b):\n    return a + b\n");
    }

    /// an override takes the names the method it overrides gives those positions, so a
    /// call that passes them by keyword works on it as it does on the base
    #[test]
    fn an_override_takes_the_base_names() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, y: int) -> None: ...

                class B(A):
                    override def f(self, _: int, _: int) -> None: ...
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, y: int) -> None: ...

                class B(A):
                    @override
                    def f(self, x: int, y: int) -> None: ...
            "},
        );
    }

    /// a base parameter that is positional-only stays so
    #[test]
    fn an_override_keeps_a_positional_only_base_parameter() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, /, y: int) -> None: ...

                class B(A):
                    override def f(self, _: int, _: int) -> None: ...
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, /, y: int) -> None: ...

                class B(A):
                    @override
                    def f(self, x: int, /, y: int) -> None: ...
            "},
        );
    }

    /// no parameter is named `_` once they take the base's names, so a body that reads `_`
    /// is handed the first of them, as it was before
    #[test]
    fn an_override_that_reads_underscore_binds_it() {
        check(
            indoc! {"
                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return _
            "},
            indoc! {"
                from typing_extensions import override
                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    @override
                    def f(self, x: int, y: int) -> int:
                        _ = x
                        return _
            "},
        );
    }

    /// the name an override would take from its base is one its body reads from an
    /// enclosing scope, where the parameter would shadow it
    #[test]
    fn an_inherited_name_the_body_reads_is_refused() {
        assert_eq!(
            refused(indoc! {"
                x = 1

                class A:
                    def f(self, x: int, y: int) -> int:
                        return x

                class B(A):
                    override def f(self, _: int, _: int) -> int:
                        return x
            "}),
            "a repeated `_` parameter named `x` shadows a `x` the body reads in `f`: the \
             parameter takes its name from the method it overrides; rename the `x` the body reads"
        );
    }
}
