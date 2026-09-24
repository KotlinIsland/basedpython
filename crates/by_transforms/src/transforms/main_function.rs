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

use ruff_python_ast::name::Name;
use ruff_python_ast::{
    self as ast, CmpOp, Expr, ModModule, ParameterWithDefault, Parameters, Stmt, StmtFunctionDef,
};

use super::ast_driver::{AstPass, PassContext};
use super::repeated_underscore::{WrittenNames, positional_only_count};
use super::source_util::{is_synthetic_decorator, python_string_literal};

pub(crate) struct MainFunction<'src> {
    source: &'src str,
    written: WrittenNames<'src>,
}

impl<'src> MainFunction<'src> {
    pub(crate) fn new(source: &'src str, written: WrittenNames<'src>) -> Self {
        Self { source, written }
    }
}

impl AstPass for MainFunction<'_> {
    // the guard runs `main` when the module is executed as a script, which a
    // stub never is
    fn runtime_only(&self) -> bool {
        true
    }

    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let Some(entry) = entry_point(&module.body, self.source, self.written) else {
            return;
        };
        // a `private main` is renamed, a `main` the command line cannot fill
        // cannot be called, and a module that invokes `main` itself keeps its
        // own entry point — `main` never runs twice
        if !entry.generates_guard() {
            return;
        }

        ctx.epilogue.push("if __name__ == \"__main__\":".to_owned());
        let spec: Vec<String> = entry
            .exposed()
            .map(|parameter| parameter.spec_entry(self.written))
            .collect();
        let extra = entry.extra_arguments;
        let call = if spec.is_empty() && extra.is_none() {
            "main()".to_owned()
        } else {
            ctx.runtime.insert(crate::runtime::MAIN_ARGS);
            ctx.epilogue
                .push("    _by_args, _by_kwargs = _by_main_args(main, [".to_owned());
            for entry in spec {
                ctx.epilogue.push(format!("        {entry},"));
            }
            let close = match extra {
                Some(converter) => format!("    ], {})", converter.spelled(self.written)),
                None => "    ])".to_owned(),
            };
            ctx.epilogue.push(close);
            "main(*_by_args, **_by_kwargs)".to_owned()
        };
        if entry.function.is_async {
            ctx.epilogue.push(format!(
                "    {}.run({call})",
                self.written.imported_module("asyncio")
            ));
            ctx.required_imports
                .push(self.written.import_module("asyncio"));
        } else {
            ctx.epilogue.push(format!("    {call}"));
        }
    }
}

/// A module's top-level `main`, and what the transpiler makes of it as the
/// program's command line.
///
/// This is the one reading of `main` there is: the pass above generates the
/// guard and the argument parser from it, and anything else that needs to know
/// a program's command line — an editor offering to fill in its arguments —
/// asks [`entry_point`] rather than reading the signature a second way.
pub struct EntryPoint<'a> {
    /// the last top-level `def main` / `async def main`
    pub function: &'a StmtFunctionDef,
    /// `private def main` is renamed to `_main`, so it is no entry point and a
    /// synthesised `main()` call would dangle
    pub is_private: bool,
    /// the module already invokes `main` itself — a `__main__` guard or a bare
    /// top-level `main(...)` call
    pub module_invokes_main: bool,
    /// every parameter that is not variadic, in declared order
    pub parameters: Vec<EntryParameter<'a>>,
    /// the converter for the arguments the interface does not claim, when a
    /// leading `*rest` asks for them — see `extra_arguments_converter`
    pub extra_arguments: Option<Converter>,
}

impl EntryPoint<'_> {
    /// The first required parameter the command line cannot supply. Invoking
    /// `main` would raise `TypeError`, so such a `main` is no entry point.
    pub fn blocked_by(&self) -> Option<&EntryParameter<'_>> {
        self.parameters
            .iter()
            .find(|param| param.is_required() && param.spelling.is_none())
    }

    /// Whether the transpiler appends the `__main__` guard that runs `main`.
    pub fn generates_guard(&self) -> bool {
        !self.is_private && !self.module_invokes_main && self.blocked_by().is_none()
    }

    /// The parameters the generated command-line interface fills.
    fn exposed(&self) -> impl Iterator<Item = &EntryParameter<'_>> {
        self.parameters
            .iter()
            .filter(|param| param.spelling.is_some())
    }

    /// `main`'s docstring — the generated parser's `--help` description, which
    /// it reads from `main.__doc__`.
    pub fn docstring(&self) -> Option<&str> {
        let Stmt::Expr(expr) = self.function.body.first()? else {
            return None;
        };
        let Expr::StringLiteral(string) = &*expr.value else {
            return None;
        };
        Some(string.value.to_str())
    }
}

/// How a `main` parameter can be passed to `main` itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterKind {
    /// positional-only: always handed over positionally
    Positional,
    /// either way
    Any,
    /// keyword-only: takes no positional slot
    Keyword,
}

impl ParameterKind {
    /// the spelling the `_by_main_args` spec carries
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Positional => "positional",
            Self::Any => "any",
            Self::Keyword => "keyword",
        }
    }
}

/// One non-variadic parameter of `main`.
pub struct EntryParameter<'a> {
    pub parameter: &'a ParameterWithDefault,
    /// the name python binds it to, which `main` is called with: a repeated `_`
    /// is numbered by the lowering
    pub python_name: Name,
    pub kind: ParameterKind,
    /// how the command line spells it; `None` when the annotation has no
    /// command-line spelling, so the parameter is not exposed
    pub spelling: Option<CliSpelling>,
}

impl EntryParameter<'_> {
    pub fn name(&self) -> &str {
        self.python_name.as_str()
    }

    pub fn is_required(&self) -> bool {
        self.parameter.default.is_none()
    }

    /// The option spellings `_by_main_args` registers for this parameter, dash
    /// form first; a name with an underscore also answers to it as written.
    pub fn flags(&self) -> Vec<String> {
        let name = self.name();
        let mut flags = vec![format!("--{}", name.replace('_', "-"))];
        if name.contains('_') {
            flags.push(format!("--{name}"));
        }
        flags
    }

    /// The `--no-…` spellings `_by_main_args` registers to set a flag false;
    /// empty for a parameter that takes a value.
    pub fn negative_flags(&self) -> Vec<String> {
        if !matches!(
            self.spelling,
            Some(CliSpelling {
                converter: None,
                ..
            })
        ) {
            return Vec::new();
        }
        self.flags()
            .iter()
            .map(|flag| format!("--no-{}", &flag[2..]))
            .collect()
    }

    /// the `(name, converter, kind, required, choices)` tuple `_by_main_args`
    /// consumes; only an exposed parameter has one
    fn spec_entry(&self, written: WrittenNames) -> String {
        let (converter, choices) = match &self.spelling {
            Some(spelling) => (
                spelling
                    .converter
                    .map_or_else(|| "None".to_owned(), |converter| converter.spelled(written)),
                &spelling.choices,
            ),
            None => ("None".to_owned(), &None),
        };
        let required = if self.is_required() { "True" } else { "False" };
        let name = self.name();
        let kind = self.kind.as_str();
        let choices = match choices {
            Some(values) => format!(
                "({},)",
                values
                    .iter()
                    .map(|choice| choice.literal.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => "None".to_owned(),
        };
        format!("(\"{name}\", {converter}, \"{kind}\", {required}, {choices})")
    }
}

/// How a `main` parameter is spelled on the command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliSpelling {
    /// the callable that converts the argument, as the source names it; `None`
    /// for a `--name` / `--no-name` flag pair, which takes no value
    pub converter: Option<Converter>,
    /// the values the annotation admits, when it is a literal union — argparse
    /// rejects anything else before `main` runs
    pub choices: Option<Vec<Choice>>,
}

/// One value a literal-union parameter admits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// the value as a python literal, as the parser's `choices` holds it
    pub literal: String,
    /// the value as it is written on the command line
    pub value: String,
}

/// The module's entry point, or `None` when it has no top-level `main`.
///
/// `source` is the text `body` was parsed from; the `private` modifier is read
/// against it. `written` is the module as its author wrote it, which names a
/// repeated `_` parameter the way the lowering does
pub fn entry_point<'a>(
    body: &'a [Stmt],
    source: &str,
    written: WrittenNames,
) -> Option<EntryPoint<'a>> {
    let function = last_top_level_main(body)?;
    Some(EntryPoint {
        function,
        is_private: is_private(source, function),
        module_invokes_main: module_invokes_main(body),
        parameters: parameters(&function.parameters, written),
        extra_arguments: extra_arguments_converter(&function.parameters),
    })
}

/// true when `main` carries the synthetic `private` modifier, which the
/// modifiers pass renames to `_main`
fn is_private(source: &str, func: &StmtFunctionDef) -> bool {
    func.decorator_list.iter().any(|dec| {
        is_synthetic_decorator(source, dec)
            && matches!(&dec.expression, Expr::Name(name) if name.id.as_str() == "private")
    })
}

/// Every non-variadic parameter of `main`, with its command-line spelling.
///
/// A parameter whose annotation has no command-line spelling is not exposed,
/// so it keeps its default — or, when it has none, stops `main` being an entry
/// point ([`EntryPoint::blocked_by`]). Variadics never require an argument, so
/// they are left out.
fn parameters<'a>(params: &'a Parameters, written: WrittenNames) -> Vec<EntryParameter<'a>> {
    let groups = [
        (&params.posonlyargs, ParameterKind::Positional),
        (&params.args, ParameterKind::Any),
        (&params.kwonlyargs, ParameterKind::Keyword),
    ];
    // a repeated `_` makes the parameters up to the last of them positional-only in the
    // python, whichever group the source wrote them in
    let positional_only = positional_only_count(params, written);
    groups
        .into_iter()
        .flat_map(|(group, kind)| group.iter().map(move |parameter| (parameter, kind)))
        .enumerate()
        .map(|(index, (parameter, kind))| EntryParameter {
            parameter,
            python_name: crate::python_parameter_name(params, &parameter.parameter, written),
            kind: if index < positional_only {
                ParameterKind::Positional
            } else {
                kind
            },
            spelling: parameter
                .parameter
                .annotation
                .as_deref()
                .and_then(cli_type)
                .map(|(ty, choices)| CliSpelling {
                    converter: match ty {
                        CliType::Value(callable) => Some(callable),
                        CliType::Flag => None,
                    },
                    choices,
                }),
        })
        .collect()
}

/// how a `main` parameter is spelled on the command line
enum CliType {
    /// takes a value, converted by the callable
    Value(Converter),
    /// a `--name` / `--no-name` flag pair
    Flag,
}

/// the callable a command-line value is converted by
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Converter {
    /// the name the annotation is written with, which reads whatever the module binds it to
    Written(&'static str),
    /// a builtin the lowering chose for a literal's values, or for arguments nothing
    /// annotates, which is read as the builtin whatever the module binds under its name
    Builtin(&'static str),
}

impl Converter {
    /// the callable's name as the command line reads it
    pub fn name(self) -> &'static str {
        match self {
            Converter::Written(name) | Converter::Builtin(name) => name,
        }
    }

    fn spelled(self, written: WrittenNames) -> String {
        match self {
            Converter::Written(name) => name.to_owned(),
            Converter::Builtin(name) => written.builtin(name),
        }
    }
}

/// The command-line spelling of an annotation — its converter and, for a
/// literal union, the values it admits — or `None` when it has none.
///
/// Matched on the annotation as written: the converter emitted into the spec
/// is the same name the source used, so it resolves to whatever that name is
/// bound to at runtime.
fn cli_type(annotation: &Expr) -> Option<(CliType, Option<Vec<Choice>>)> {
    match annotation {
        Expr::Name(name) => {
            let ty = match name.id.as_str() {
                "bool" => CliType::Flag,
                "str" => CliType::Value(Converter::Written("str")),
                "int" => CliType::Value(Converter::Written("int")),
                "float" => CliType::Value(Converter::Written("float")),
                "Path" => CliType::Value(Converter::Written("Path")),
                _ => return None,
            };
            Some((ty, None))
        }
        Expr::Attribute(attr) => (attr.attr.as_str() == "Path"
            && matches!(&*attr.value, Expr::Name(name) if name.id.as_str() == "pathlib"))
        .then_some((CliType::Value(Converter::Written("pathlib.Path")), None)),
        // `T?` — an absent argument is what the `None` stands for, so the
        // spelling is `T`'s
        Expr::UnaryOp(unary) if matches!(unary.op, ast::UnaryOp::Optional) => {
            cli_type(&unary.operand)
        }
        // a union: either `T | None`, which is `T?` written out, or a union of
        // literals, which argparse expresses as `choices`
        Expr::BinOp(bin_op) if matches!(bin_op.op, ast::Operator::BitOr) => {
            let mut named: Option<(CliType, Option<Vec<Choice>>)> = None;
            let mut literals = Vec::new();
            let mut literal_converter: Option<&'static str> = None;
            for operand in union_operands(annotation) {
                if is_none_literal(operand) {
                    continue;
                }
                if let Some((converter, rendered)) = literal_choice(operand) {
                    if *literal_converter.get_or_insert(converter) != converter {
                        return None;
                    }
                    literals.push(rendered);
                    continue;
                }
                // a second named operand is a union with no single converter
                if named.is_some() {
                    return None;
                }
                named = Some(cli_type(operand)?);
            }
            match (named, literal_converter) {
                // `T | None` — the `None` is what leaving the argument out means
                (Some(spelling), None) => Some(spelling),
                // every operand a literal of one kind: the values it admits
                (None, Some(converter)) => Some((
                    CliType::Value(Converter::Builtin(converter)),
                    Some(literals),
                )),
                // a named type beside a literal (`int | "a"`) admits neither the
                // type's values nor the literal's, so nothing on the command
                // line could satisfy both. nothing but `None`s says nothing
                _ => None,
            }
        }
        // `Literal["a", "b"]` — the same set spelled the typing way, bare or
        // through the module it comes from
        Expr::Subscript(subscript) if trailing_name(&subscript.value) == Some("Literal") => {
            let mut literals = Vec::new();
            let mut converter: Option<&'static str> = None;
            for element in slice_elements(&subscript.slice) {
                let (ty, rendered) = literal_choice(element)?;
                if *converter.get_or_insert(ty) != ty {
                    return None;
                }
                literals.push(rendered);
            }
            Some((
                CliType::Value(Converter::Builtin(converter?)),
                Some(literals),
            ))
        }
        _ => None,
    }
}

/// The converter for the arguments `main`'s interface does not claim, when it
/// asks for them.
///
/// A leading `*rest` is the ask: everything declared after it is keyword-only,
/// so the unclaimed arguments are the only positional ones and bind to `rest`.
/// A `*rest` written *after* an ordinary parameter cannot mean that — the
/// parameter ahead of it would take the first unclaimed argument as its own —
/// so there it stays what python makes it, a variadic nothing fills.
///
/// The arguments arrive as the strings the command line carried, so the
/// vararg's annotation converts them, exactly as a declared parameter's does.
/// An annotation with no command-line spelling has nothing to convert with, and
/// the vararg goes back to being one nothing fills.
fn extra_arguments_converter(params: &Parameters) -> Option<Converter> {
    let vararg = params.vararg.as_ref()?;
    if !(params.posonlyargs.is_empty() && params.args.is_empty()) {
        return None;
    }
    match vararg.annotation.as_deref() {
        None => Some(Converter::Builtin("str")),
        Some(annotation) => match cli_type(annotation) {
            Some((CliType::Value(converter), None)) => Some(converter),
            // a flag is not a value, and a literal union's `choices` have no
            // argparse slot to be checked against here
            _ => None,
        },
    }
}

/// the name a reference ends in — `Literal` for both `Literal` and
/// `typing.Literal`
fn trailing_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Name(name) => Some(name.id.as_str()),
        Expr::Attribute(attribute) => Some(attribute.attr.as_str()),
        _ => None,
    }
}

/// the operands of a `|` union, flattened — `a | b | c` nests to the left
fn union_operands(annotation: &Expr) -> Vec<&Expr> {
    fn walk<'a>(expr: &'a Expr, out: &mut Vec<&'a Expr>) {
        if let Expr::BinOp(bin_op) = expr
            && matches!(bin_op.op, ast::Operator::BitOr)
        {
            walk(&bin_op.left, out);
            walk(&bin_op.right, out);
            return;
        }
        out.push(expr);
    }
    let mut out = Vec::new();
    walk(annotation, &mut out);
    out
}

/// the elements of a subscript slice, which is a tuple when there is more than
/// one
fn slice_elements(slice: &Expr) -> Vec<&Expr> {
    match slice {
        Expr::Tuple(tuple) => tuple.elts.iter().collect(),
        single => vec![single],
    }
}

fn is_none_literal(expr: &Expr) -> bool {
    expr.is_none_literal_expr()
}

/// a literal the command line can carry, as its converter and the value
fn literal_choice(expr: &Expr) -> Option<(&'static str, Choice)> {
    match expr {
        Expr::StringLiteral(string) => {
            let value = string.value.to_str();
            Some((
                "str",
                Choice {
                    literal: python_string_literal(value),
                    value: value.to_owned(),
                },
            ))
        }
        Expr::NumberLiteral(number) => match &number.value {
            ast::Number::Int(int) => Some((
                "int",
                Choice {
                    literal: int.to_string(),
                    value: int.to_string(),
                },
            )),
            _ => None,
        },
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

/// The module's own top-level `if __name__ == "__main__":` statements — each an
/// entry point written by hand, whatever the module's `main` is.
pub fn main_guards(body: &[Stmt]) -> impl Iterator<Item = &ast::StmtIf> {
    body.iter().filter_map(|stmt| match stmt {
        Stmt::If(if_stmt) if is_dunder_main_guard(&if_stmt.test) => Some(if_stmt),
        _ => None,
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
    use crate::{Config, WrittenNames, transpile};
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
                        (\"name\", str, \"any\", True, None),
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
                .contains("(\"count\", int, \"any\", False, None),"),
            "got:\n{}",
            guard("def main(count: int = 1):\n    pass\n")
        );
    }

    #[test]
    fn bool_parameter_becomes_a_flag() {
        // `None` as the converter is what marks a `--name` / `--no-name` pair
        assert!(
            guard("def main(verbose: bool = False):\n    pass\n")
                .contains("(\"verbose\", None, \"any\", False, None),"),
            "got:\n{}",
            guard("def main(verbose: bool = False):\n    pass\n")
        );
    }

    #[test]
    fn path_converter_keeps_the_annotation_spelling() {
        // the converter runs at runtime, so it must name whatever the module
        // actually imported
        assert!(
            guard("def main(out: Path):\n    pass\n")
                .contains("(\"out\", Path, \"any\", True, None),"),
            "got:\n{}",
            guard("def main(out: Path):\n    pass\n")
        );
        assert!(
            guard("def main(out: pathlib.Path):\n    pass\n")
                .contains("(\"out\", pathlib.Path, \"any\", True, None),"),
            "got:\n{}",
            guard("def main(out: pathlib.Path):\n    pass\n")
        );
    }

    #[test]
    fn float_parameter_is_supported() {
        assert!(
            guard("def main(ratio: float):\n    pass\n")
                .contains("(\"ratio\", float, \"any\", True, None),"),
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
                        (\"a\", str, \"positional\", True, None),
                        (\"b\", int, \"any\", False, None),
                        (\"c\", str, \"keyword\", False, None),
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
            out.contains("(\"name\", str, \"any\", True, None),"),
            "got:\n{out}"
        );
        assert!(!out.contains("argv"), "got:\n{out}");
    }

    #[test]
    fn variadic_parameters_are_not_exposed() {
        let out = guard("def main(name: str, *extra: str, **rest: str):\n    pass\n");
        assert!(
            out.contains("(\"name\", str, \"any\", True, None),"),
            "got:\n{out}"
        );
        assert!(!out.contains("extra"), "got:\n{out}");
        assert!(!out.contains("rest"), "got:\n{out}");
    }

    #[test]
    fn an_optional_parameter_takes_its_inner_spelling() {
        // leaving the argument out is what the `None` stands for
        let out = guard("def main(name: str? = None):\n    pass\n");
        assert!(
            out.contains("(\"name\", str, \"any\", False, None),"),
            "got:\n{out}"
        );

        let out = guard("def main(name: str | None = None):\n    pass\n");
        assert!(
            out.contains("(\"name\", str, \"any\", False, None),"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_literal_union_becomes_the_values_it_admits() {
        let out = guard("def main(mode: \"fast\" | \"slow\" = \"fast\"):\n    pass\n");
        assert!(
            out.contains("(\"mode\", str, \"any\", False, (\"fast\", \"slow\",)),"),
            "got:\n{out}"
        );

        let out = guard(indoc! {"
            from typing import Literal

            def main(mode: Literal[\"fast\", \"slow\"] = \"fast\"):
                pass
        "});
        assert!(
            out.contains("(\"mode\", str, \"any\", False, (\"fast\", \"slow\",)),"),
            "got:\n{out}"
        );
    }

    /// the converter for a literal's values is the lowering's choice, not a name the
    /// source wrote, so it is the builtin even where the module binds the name itself
    #[test]
    fn a_literal_union_converts_with_the_builtin_the_module_shadows() {
        let out = transpile(
            indoc! {"
                def str(o: object) -> int:
                    return 7

                def main(mode: \"fast\" | \"slow\" = \"fast\"):
                    pass
            "},
            &Config::test_default(),
        )
        .unwrap();
        assert!(
            out.starts_with("from builtins import str as str2\n")
                && out.contains("(\"mode\", str2, \"any\", False, (\"fast\", \"slow\",)),"),
            "got:\n{out}"
        );
    }

    #[test]
    fn an_optional_literal_union_keeps_its_values() {
        let out = guard("def main(mode: (\"fast\" | \"slow\")? = None):\n    pass\n");
        assert!(
            out.contains("(\"mode\", str, \"any\", False, (\"fast\", \"slow\",)),"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_mixed_union_has_no_command_line_spelling() {
        // the choices would not describe the type, so the parameter keeps its
        // default instead of being exposed under a wrong one
        let out = guard("def main(mode: \"fast\" | int = \"fast\"):\n    pass\n");
        assert!(!out.contains("\"mode\""), "got:\n{out}");
    }

    #[test]
    fn a_leading_variadic_takes_the_unclaimed_arguments() {
        let out = guard("def main(*rest: str, games: int = 1):\n    pass\n");
        assert!(
            out.contains("(\"games\", int, \"keyword\", False, None),"),
            "got:\n{out}"
        );
        assert!(out.contains("    ], str)"), "got:\n{out}");
    }

    #[test]
    fn a_trailing_variadic_is_still_not_filled() {
        // `games` would take the first unclaimed argument as its own, so `rest`
        // cannot mean "the rest of the command line" here
        let out = guard("def main(games: int = 1, *rest: str):\n    pass\n");
        assert!(out.contains("    ])"), "got:\n{out}");
        assert!(!out.contains("], True)"), "got:\n{out}");
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
    fn variadic_main_takes_the_command_line() {
        // a leading `*args` asks for the arguments the interface does not claim,
        // and with no declared parameter that is all of them. `**kwargs` takes
        // no positional slot, so it neither receives them nor blocks them
        let out = guard("def main(*args, **kwargs):\n    pass\n");
        assert!(out.contains("    ], str)"), "got:\n{out}");
    }

    #[test]
    fn a_trailing_variadic_main_is_still_not_filled() {
        // `name` would take the first unclaimed argument as its own, so `args`
        // cannot mean the rest of the command line here
        let out = guard("def main(name: str, *args):\n    pass\n");
        assert!(out.contains("    ])"), "got:\n{out}");
        assert!(!out.contains("], str)"), "got:\n{out}");
    }

    #[test]
    fn a_literal_union_admits_the_empty_string() {
        // `\"\"` is a value like any other: it is a choice the command line can
        // carry, and the default the parameter falls back to
        let out = guard("def main(a: \"a\" | \"b\" | \"\" = \"\"):\n    pass\n");
        assert!(
            out.contains("(\"a\", str, \"any\", False, (\"a\", \"b\", \"\",)),"),
            "got:\n{out}"
        );
    }

    /// `main` is called with each value under the name python binds its parameter to,
    /// and a repeated `_` is numbered there. two specs named `_` register one option
    /// twice, which `argparse` refuses before anything runs. the numbered parameters
    /// are positional-only, so they are filled by position
    #[test]
    fn a_repeated_underscore_is_filled_by_its_numbered_name() {
        let out = guard("def main(_: int, _: str):\n    pass\n");
        assert!(
            out.contains(
                "(\"_\", int, \"positional\", True, None),\n        (\"_2\", str, \"positional\", True, None),"
            ),
            "got:\n{out}"
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

    fn parsed(source: &str) -> ruff_python_parser::Parsed<ruff_python_ast::ModModule> {
        ruff_python_parser::parse_unchecked_source(
            source,
            ruff_python_ast::PySourceType::BasedPython,
        )
    }

    /// the reading an editor asks for is the one the guard is generated from, so
    /// a generic `main` is still `main`, and its parameters are still its own
    #[test]
    fn a_generic_main_is_the_entry_point() {
        let source = "def main[T](name: str, count: int = 1):\n    pass\n";
        assert!(guard(source).contains("(\"name\", str, \"any\", True, None),"));
        let module = parsed(source);
        let entry = super::entry_point(&module.syntax().body, source, WrittenNames::new(source))
            .expect("a main");
        assert!(entry.generates_guard());
        let names: Vec<&str> = entry
            .parameters
            .iter()
            .map(super::EntryParameter::name)
            .collect();
        assert_eq!(names, ["name", "count"]);
    }

    #[test]
    fn a_main_the_command_line_cannot_fill_names_what_blocks_it() {
        let source = "def main(a: int, argv, b: str = \"x\"):\n    \"\"\"Adds.\"\"\"\n";
        let module = parsed(source);
        let entry = super::entry_point(&module.syntax().body, source, WrittenNames::new(source))
            .expect("a main");
        assert_eq!(
            entry.blocked_by().map(super::EntryParameter::name),
            Some("argv")
        );
        assert!(!entry.generates_guard());
        assert_eq!(entry.docstring(), Some("Adds."));
    }

    #[test]
    fn a_main_call_inside_a_docstring_is_not_an_invocation() {
        let source = "\"\"\"\nmain()\n\"\"\"\ndef main():\n    pass\n";
        let module = parsed(source);
        let entry = super::entry_point(&module.syntax().body, source, WrittenNames::new(source))
            .expect("a main");
        assert!(!entry.module_invokes_main);
        assert!(guard(source).contains("    main()"));
    }

    #[test]
    fn flags_are_the_spellings_the_runtime_registers() {
        let source = "def main(out_dir: Path, dry_run: bool = False, mode: \"a\" | \"b\" = \"a\"):\n    pass\n";
        let module = parsed(source);
        let entry = super::entry_point(&module.syntax().body, source, WrittenNames::new(source))
            .expect("a main");
        let [out_dir, dry_run, mode] = entry.parameters.as_slice() else {
            panic!("three parameters");
        };
        assert_eq!(out_dir.flags(), ["--out-dir", "--out_dir"]);
        assert!(out_dir.negative_flags().is_empty());
        assert_eq!(dry_run.negative_flags(), ["--no-dry-run", "--no-dry_run"]);
        let choices: Vec<&str> = mode
            .spelling
            .as_ref()
            .and_then(|spelling| spelling.choices.as_ref())
            .expect("choices")
            .iter()
            .map(|choice| choice.value.as_str())
            .collect();
        assert_eq!(choices, ["a", "b"]);
    }

    /// a stub is never run as a script, so it declares `main` and nothing calls it
    #[test]
    fn a_stub_gets_no_entry_point() {
        let source = "def main(name: str) -> None: ...\n";
        let config = Config {
            is_stub: true,
            ..Config::test_default()
        };
        assert_eq!(transpile(source, &config).unwrap(), source);
    }
}
