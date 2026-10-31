//! reverse of the runtime helpers an expression is lowered onto:
//!   `_by_subscript(map, int)` → `map[int]` (`crate::transforms::class_subscript`)
//!   `_by_specialization(U, int)` → `U[int]` (`crate::transforms::class_subscript`)
//!   `_by_raise(E, C)` → `raise E from C` (`crate::transforms::statement_expression`)
//!   `B[_by_lit(1)]` → `B[1]` (the spelling of a literal type argument)
//!   `_by_specialized(c.m, int)` → `c.m[int]` (`crate::transforms::reified_generic`)
//!   `_by_cell_is(x, T)` → `x is T` (`crate::transforms::parametric_is`)
//!   `Box[_by_out(int)]` → `Box[out int]` (the spelling of a use-site projection)
//!   `_by_variance(1)(Sequence)` → `Sequence`, and `@_by_variance(1)` is dropped
//!   (`crate::transforms::runtime_variance`): the checker can read the variance off the class
//!
//! the name alone says nothing, since an author may call their own function any of these. a
//! call counts only when the one thing binding the name at module level is the runtime: an
//! import from a `_by_runtime` module, or a definition identical to the one a transpile with
//! nowhere to import from pastes in. a pasted definition stays, as every helper's does, and is
//! not itself rewritten: `_by_cell_is` calls itself
//!
//! a specialization the forward direction injected stays too, as `f[int](1)` does: it says the
//! same thing as the bare call
//!
//! `_by_told(__name__, ...)` tells the runtime what the checker of the source says for itself, so
//! the statement is dropped. `_by_forward(list["F"], __name__)` → `list["F"]`
//!
//! `_by_lit` builds a literal type, so it only reverses where a type is read: a type argument, or
//! the right side of a type test. written as a value, the literal it would leave isn't that type.
//! a projection is the same

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::helpers::is_dotted_name;
use ruff_python_ast::str::Quote;
use ruff_python_ast::token::{Tokens, parentheses_iterator, parenthesized_range};
use ruff_python_ast::visitor::{Visitor, walk_comprehension, walk_expr, walk_f_string, walk_stmt};
use ruff_python_ast::{
    AnyNodeRef, Comprehension, Expr, ExprCall, ExprRef, FString, Identifier, Number, Operator,
    OperatorPrecedence, Stmt, StringFlags as _,
};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::{python_bytes_literal, python_str_literal};

use super::literal_types::is_literal_value;
use crate::runtime::{self, Helper};
use crate::transforms::source_util::{line_past_end, line_start};

const REVERSED: [Helper; 11] = [
    runtime::TOLD,
    runtime::FORWARD,
    runtime::SUBSCRIPT,
    runtime::SPECIALIZATION,
    runtime::RAISE,
    runtime::LITERAL,
    runtime::SPECIALIZED,
    runtime::CELL_IS,
    runtime::VARIANCE,
    runtime::OUT,
    runtime::IN,
];

/// what an expression's replacement has to bind tighter than, to stay the same expression
#[derive(Clone, Copy)]
enum Enclosing {
    /// nothing: it stands in a position of its own, such as a call's argument
    Free,
    /// an operand of something binding this tightly, or a position where a `raise` couldn't
    /// tell where it ends even though an `is` could: a statement's header
    Operand(OperatorPrecedence),
}

impl Enclosing {
    /// what `expr` is to the expressions directly inside it
    fn of(expr: &Expr) -> Self {
        match expr {
            Expr::Tuple(tuple) if !tuple.parenthesized => Self::Operand(OperatorPrecedence::None),
            Expr::Lambda(_)
            | Expr::Named(_)
            | Expr::Yield(_)
            | Expr::YieldFrom(_)
            | Expr::Slice(_) => Self::Free,
            Expr::Starred(_) => Self::Operand(OperatorPrecedence::BitOr),
            _ => match OperatorPrecedence::from_expr(expr) {
                OperatorPrecedence::Atomic => Self::Free,
                precedence => Self::Operand(precedence),
            },
        }
    }
}

pub(crate) struct RuntimeHelpersReverse<'src> {
    source: &'src str,
    tokens: &'src Tokens,
    /// the helpers whose names in this module are the runtime's, each with the name the module
    /// reads it by
    provided: Vec<(Helper, String)>,
    /// the definitions of the runtime pasted into this module
    pasted: Vec<TextRange>,
    enclosing: Enclosing,
    in_type: bool,
    /// the quote of each f-string around the expression being visited
    f_strings: Vec<Quote>,
    pub(crate) edits: Vec<Fix>,
}

impl<'src> RuntimeHelpersReverse<'src> {
    pub(crate) fn new(source: &'src str, tokens: &'src Tokens, suite: &[Stmt]) -> Self {
        let mut reverse = Self {
            source,
            tokens,
            provided: Vec::new(),
            pasted: Vec::new(),
            enclosing: Enclosing::Free,
            in_type: false,
            f_strings: Vec::new(),
            edits: Vec::new(),
        };
        if REVERSED.iter().any(|helper| source.contains(helper.name())) {
            reverse.pasted = runtime::pasted_definitions(suite, source);
            let shadowed = runtime::shadowing(suite).scopes;
            let local_names: Vec<String> = suite
                .iter()
                .flat_map(|stmt| runtime::module_bindings(stmt).into_keys())
                .collect();
            let mut provided = Vec::new();
            for helper in REVERSED {
                let provider = local_names.iter().find(|local| {
                    let mut binders = suite
                        .iter()
                        .filter(|stmt| runtime::module_bindings(stmt).contains_key(*local));
                    let (Some(binder), None) = (binders.next(), binders.next()) else {
                        return false;
                    };
                    (is_runtime_import(binder, helper.name(), local)
                        || (runtime::helper_of_local_name(local) == helper.name()
                            && reverse.is_pasted(binder.range())))
                        && !shadowed.iter().any(|(bound, ..)| bound == *local)
                });
                if let Some(local) = provider {
                    provided.push((helper, local.clone()));
                }
            }
            reverse.provided = provided;
        }
        reverse
    }

    /// the helper `name` is this module's name for
    fn reads_helper(&self, name: &str) -> Option<Helper> {
        self.provided
            .iter()
            .find(|(_, local)| local == name)
            .map(|(helper, _)| *helper)
    }

    fn is_pasted(&self, range: TextRange) -> bool {
        self.pasted
            .iter()
            .any(|pasted| pasted.contains_range(range))
    }

    /// each argument's source with its parentheses, in source order
    fn argument_spans(&self, call: &ExprCall) -> Vec<TextRange> {
        let mut spans: Vec<TextRange> = call
            .arguments
            .args
            .iter()
            .map(|argument| {
                parenthesized_range(
                    ExprRef::from(argument),
                    AnyNodeRef::from(&call.arguments),
                    self.tokens,
                )
                .unwrap_or_else(|| argument.range())
            })
            .chain(call.arguments.keywords.iter().map(Ranged::range))
            .collect();
        spans.sort_by_key(Ranged::start);
        spans
    }

    /// replace what the call writes around `spans`, leaving them as they are so whatever is
    /// rewritten inside them still applies. `around` holds what comes before each span and, last,
    /// what comes after them all, and reports `false` for a shape this can't keep whole: a
    /// comment would go with the punctuation, and a trailing comma would end a subscript's slice
    /// with one, which makes it a tuple
    ///
    /// each replacement is a fix of its own, because a fix applied as a unit advances past
    /// everything between its edits and loses what was rewritten there
    fn respell(&mut self, call: &ExprCall, spans: &[TextRange], around: &[String]) -> bool {
        debug_assert_eq!(spans.len() + 1, around.len());
        let mut gaps = Vec::with_capacity(around.len());
        let mut at = call.start();
        for span in spans {
            gaps.push(TextRange::new(at, span.start()));
            at = span.end();
        }
        let tail = TextRange::new(at, call.end());
        gaps.push(tail);
        if gaps.iter().any(|gap| self.source[*gap].contains('#'))
            || (!spans.is_empty() && self.source[tail].contains(','))
        {
            return false;
        }
        for (gap, text) in gaps.into_iter().zip(around) {
            self.edits.push(Fix::safe_edit(if text.is_empty() {
                Edit::range_deletion(gap)
            } else {
                Edit::range_replacement(text.clone(), gap)
            }));
        }
        true
    }

    /// the parentheses `expr` needs to bind at least as tightly as `least`, given the source of
    /// the argument it is
    fn parentheses(
        expr: &Expr,
        span: TextRange,
        least: OperatorPrecedence,
    ) -> (&'static str, &'static str) {
        if span == expr.range() && OperatorPrecedence::from_expr(expr) < least {
            ("(", ")")
        } else {
            ("", "")
        }
    }

    /// the parentheses a replacement of `call` needs, if `needed` by what encloses it. the source
    /// may already have them: `(x is T) + 1` lowers to `(_by_cell_is(x, T)) + 1`
    fn outer_parentheses(&self, call: &ExprCall, needed: bool) -> (&'static str, &'static str) {
        if needed
            && parentheses_iterator(ExprRef::Call(call), None, self.tokens)
                .next()
                .is_none()
        {
            ("(", ")")
        } else {
            ("", "")
        }
    }

    fn visit_operand(&mut self, expr: &Expr, enclosing: Enclosing) {
        self.enclosing = enclosing;
        self.in_type = false;
        self.visit_expr(expr);
    }

    fn visit_type(&mut self, expr: &Expr, enclosing: Enclosing) {
        self.enclosing = enclosing;
        self.in_type = true;
        self.visit_expr(expr);
    }

    /// `_by_subscript(value, *slice)` and `_by_specialized(value, *items, **fields)` →
    /// `value[slice]`
    fn subscript(&mut self, call: &ExprCall) -> bool {
        let Some((value, items)) = call.arguments.args.split_first() else {
            return false;
        };
        let arguments = items.len() + call.arguments.keywords.len();
        if arguments == 0
            || value.is_starred_expr()
            || call.arguments.keywords.iter().any(|k| k.arg.is_none())
        {
            return false;
        }
        let spans = self.argument_spans(call);
        let (open, close) = Self::parentheses(value, spans[0], OperatorPrecedence::CallAttribute);
        let mut around = vec![open.to_owned(), format!("{close}[")];
        around.extend(std::iter::repeat_n(", ".to_owned(), arguments - 1));
        around.push("]".to_owned());
        if !self.respell(call, &spans, &around) {
            return false;
        }
        self.visit_operand(value, Enclosing::Operand(OperatorPrecedence::CallAttribute));
        for item in items {
            self.visit_type(item, Enclosing::Free);
        }
        for keyword in &call.arguments.keywords {
            self.visit_type(&keyword.value, Enclosing::Free);
        }
        true
    }

    /// `_by_raise(*raised)` → `raise exception from cause`
    fn raise(&mut self, call: &ExprCall, enclosing: Enclosing) -> bool {
        let args = &call.arguments.args;
        if args.len() > 2
            || !call.arguments.keywords.is_empty()
            || args.iter().any(|arg| {
                // an unparenthesized generator's range takes the call's parentheses with it
                arg.is_starred_expr() || matches!(arg, Expr::Generator(g) if !g.parenthesized)
            })
        {
            return false;
        }
        let (start, end) = self.outer_parentheses(call, matches!(enclosing, Enclosing::Operand(_)));
        let spans = self.argument_spans(call);
        let around = match (&args[..], &spans[..]) {
            ([exception], [span]) => {
                let (open, close) = Self::parentheses(exception, *span, OperatorPrecedence::Lambda);
                vec![format!("{start}raise {open}"), format!("{close}{end}")]
            }
            ([exception, cause], [exception_span, cause_span]) => {
                let (open, close) =
                    Self::parentheses(exception, *exception_span, OperatorPrecedence::Lambda);
                let (open_cause, close_cause) =
                    Self::parentheses(cause, *cause_span, OperatorPrecedence::Lambda);
                vec![
                    format!("{start}raise {open}"),
                    format!("{close} from {open_cause}"),
                    format!("{close_cause}{end}"),
                ]
            }
            _ => vec![format!("{start}raise{end}")],
        };
        if !self.respell(call, &spans, &around) {
            return false;
        }
        for arg in args {
            self.visit_operand(arg, Enclosing::Operand(OperatorPrecedence::None));
        }
        true
    }

    /// `_by_lit(value)`, and the code points of a string or bytes written where quotes can't go
    fn literal(&mut self, call: &ExprCall) -> bool {
        match (&*call.arguments.args, &*call.arguments.keywords) {
            ([value], [])
                if is_literal_value(value)
                    || (value.is_attribute_expr() && is_dotted_name(value)) =>
            {
                let spans = self.argument_spans(call);
                self.respell(call, &spans, &[String::new(), String::new()])
            }
            ([], [keyword]) => {
                let Expr::Tuple(values) = &keyword.value else {
                    return false;
                };
                let values: Option<Vec<u64>> = values
                    .elts
                    .iter()
                    .map(|value| match value {
                        Expr::NumberLiteral(number) => match &number.value {
                            Number::Int(int) => int.as_u64(),
                            Number::Float(_) | Number::Complex { .. } => None,
                        },
                        _ => None,
                    })
                    .collect();
                let written = match (keyword.arg.as_ref().map(Identifier::as_str), values) {
                    (Some("text"), Some(values)) => values
                        .into_iter()
                        .map(|point| char::from_u32(u32::try_from(point).ok()?))
                        .collect::<Option<String>>()
                        .and_then(|text| {
                            if self.f_strings.is_empty() {
                                Some(python_str_literal(&text))
                            } else {
                                self.literal_in_field("", &text)
                            }
                        }),
                    (Some("data"), Some(values)) => values
                        .into_iter()
                        .map(|byte| u8::try_from(byte).ok())
                        .collect::<Option<Vec<u8>>>()
                        .and_then(|data| {
                            if self.f_strings.is_empty() {
                                Some(python_bytes_literal(&data))
                            } else {
                                std::str::from_utf8(&data)
                                    .ok()
                                    .filter(|text| text.is_ascii())
                                    .and_then(|text| self.literal_in_field("b", text))
                            }
                        }),
                    _ => None,
                };
                written.is_some_and(|written| self.respell(call, &[], &[written]))
            }
            _ => false,
        }
    }

    /// `text` as a literal an f-string's replacement field can hold below python 3.12, which is
    /// why the forward direction wrote it as code points: without a backslash, and in a quote no
    /// f-string around it is written in
    fn literal_in_field(&self, prefix: &str, text: &str) -> Option<String> {
        if text.chars().any(|c| c.is_control() || c == '\\') {
            return None;
        }
        let quote = [Quote::Double, Quote::Single]
            .into_iter()
            .find(|quote| !self.f_strings.contains(quote) && !text.contains(quote.as_char()))?;
        let quote = quote.as_char();
        Some(format!("{prefix}{quote}{text}{quote}"))
    }

    /// `_by_cell_is(value, cell)` → `value is cell`
    fn cell_is(&mut self, call: &ExprCall, enclosing: Enclosing) -> bool {
        let ([value, cell], []) = (&*call.arguments.args, &*call.arguments.keywords) else {
            return false;
        };
        if value.is_starred_expr() || cell.is_starred_expr() {
            return false;
        }
        let comparison = OperatorPrecedence::ComparisonsMembershipIdentity;
        let (start, end) = self.outer_parentheses(
            call,
            matches!(enclosing, Enclosing::Operand(precedence) if precedence >= comparison),
        );
        let spans = self.argument_spans(call);
        // a comparison chains with one directly inside it, so an operand has to bind tighter
        let (open, close) = Self::parentheses(value, spans[0], OperatorPrecedence::BitOr);
        let (open_cell, close_cell) = Self::parentheses(cell, spans[1], OperatorPrecedence::BitOr);
        let around = [
            format!("{start}{open}"),
            format!("{close} is {open_cell}"),
            format!("{close_cell}{end}"),
        ];
        if !self.respell(call, &spans, &around) {
            return false;
        }
        self.visit_operand(value, Enclosing::Operand(comparison));
        self.visit_type(cell, Enclosing::Operand(comparison));
        true
    }

    /// `_by_out(inner)` → `out inner`
    fn projection(&mut self, call: &ExprCall, keyword: &str) -> bool {
        let spans = self.argument_spans(call);
        let [inner] = &*call.arguments.args else {
            return false;
        };
        if !call.arguments.keywords.is_empty()
            || !self.respell(call, &spans, &[format!("{keyword} "), String::new()])
        {
            return false;
        }
        self.visit_type(inner, Enclosing::Operand(OperatorPrecedence::None));
        true
    }

    /// whether `expr` calls the runtime's `_by_variance`, which is what telling a class's
    /// variance is written as both ways
    fn is_variance_call(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Call(call) if matches!(
            call.func.as_ref(),
            Expr::Name(name)
                if self.reads_helper(name.id.as_str()) == Some(runtime::VARIANCE)
        ))
    }

    /// `_by_variance(1)(class)` → `class`
    fn telling_variance(&mut self, call: &ExprCall, enclosing: Enclosing) -> bool {
        let spans = self.argument_spans(call);
        let [class] = &*call.arguments.args else {
            return false;
        };
        if !call.arguments.keywords.is_empty()
            || !self.respell(call, &spans, &[String::new(), String::new()])
        {
            return false;
        }
        self.visit_operand(class, enclosing);
        true
    }

    /// `_by_forward(list["F"], __name__)` → `list["F"]`
    fn forward(&mut self, call: &ExprCall, enclosing: Enclosing) -> bool {
        let [specialization, Expr::Name(module)] = &*call.arguments.args else {
            return false;
        };
        let span = self.argument_spans(call)[0];
        if !call.arguments.keywords.is_empty()
            || module.id.as_str() != "__name__"
            || self.source[TextRange::new(call.start(), span.start())].contains('#')
            || self.source[TextRange::new(span.end(), call.end())].contains('#')
        {
            return false;
        }
        for gap in [
            TextRange::new(call.start(), span.start()),
            TextRange::new(span.end(), call.end()),
        ] {
            self.edits.push(Fix::safe_edit(Edit::range_deletion(gap)));
        }
        self.visit_operand(specialization, enclosing);
        true
    }

    /// whether `stmt` is what telling the runtime about the classes of a stub is written as
    fn is_telling_classes(&self, stmt: &Stmt) -> bool {
        matches!(stmt, Stmt::Expr(expr) if matches!(expr.value.as_ref(), Expr::Call(call)
            if matches!(call.func.as_ref(), Expr::Name(name)
                if self.reads_helper(name.id.as_str()) == Some(runtime::TOLD))))
    }

    /// rewrite `call` if it is a call of one of the helpers in a shape the forward transforms
    /// write, along with what is inside it
    fn reverse_call(&mut self, call: &ExprCall, enclosing: Enclosing, in_type: bool) -> bool {
        if self.is_variance_call(&call.func) {
            return self.telling_variance(call, enclosing);
        }
        let Expr::Name(name) = call.func.as_ref() else {
            return false;
        };
        let Some(helper) = self.reads_helper(name.id.as_str()) else {
            return false;
        };
        match helper {
            runtime::SUBSCRIPT | runtime::SPECIALIZATION | runtime::SPECIALIZED => {
                self.subscript(call)
            }
            runtime::RAISE => self.raise(call, enclosing),
            runtime::LITERAL => in_type && self.literal(call),
            runtime::CELL_IS => self.cell_is(call, enclosing),
            runtime::OUT => in_type && self.projection(call, "out"),
            runtime::IN => in_type && self.projection(call, "in"),
            runtime::FORWARD => self.forward(call, enclosing),
            _ => false,
        }
    }
}

/// whether the statement is an import of `name`, under that name, from a `_by_runtime` module
/// however a build qualifies it
/// whether `stmt` imports the runtime's `name` under the name `local`
fn is_runtime_import(stmt: &Stmt, name: &str, local: &str) -> bool {
    let Stmt::ImportFrom(import) = stmt else {
        return false;
    };
    import.level == 0
        && import.module.as_ref().is_some_and(|module| {
            module.id.as_str().rsplit('.').next() == Some(runtime::MODULE_NAME)
        })
        && import.names.iter().any(|alias| {
            alias.name.id.as_str() == name
                && alias
                    .asname
                    .as_ref()
                    .map_or(name, |asname| asname.id.as_str())
                    == local
        })
}

impl<'ast> Visitor<'ast> for RuntimeHelpersReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if self.provided.is_empty() || self.is_pasted(stmt.range()) {
            return;
        }
        if self.is_telling_classes(stmt) {
            self.edits
                .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                    line_start(self.source, stmt.start()),
                    line_past_end(self.source, stmt.end()),
                ))));
            return;
        }
        let (enclosing, in_type) = (self.enclosing, self.in_type);
        self.enclosing = match stmt {
            Stmt::Expr(_)
            | Stmt::Assign(_)
            | Stmt::AugAssign(_)
            | Stmt::AnnAssign(_)
            | Stmt::Return(_) => Enclosing::Free,
            _ => Enclosing::Operand(OperatorPrecedence::None),
        };
        self.in_type = false;
        if let Stmt::ClassDef(class) = stmt {
            for decorator in &class.decorator_list {
                if self.is_variance_call(&decorator.expression) {
                    self.edits
                        .push(Fix::safe_edit(Edit::range_deletion(TextRange::new(
                            line_start(self.source, decorator.start()),
                            line_past_end(self.source, decorator.end()),
                        ))));
                }
            }
        }
        walk_stmt(self, stmt);
        self.enclosing = enclosing;
        self.in_type = in_type;
    }

    fn visit_f_string(&mut self, f_string: &'ast FString) {
        self.f_strings.push(f_string.flags.quote_style());
        walk_f_string(self, f_string);
        self.f_strings.pop();
    }

    fn visit_comprehension(&mut self, comprehension: &'ast Comprehension) {
        let enclosing = self.enclosing;
        self.enclosing = Enclosing::Operand(OperatorPrecedence::None);
        walk_comprehension(self, comprehension);
        self.enclosing = enclosing;
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        let (enclosing, in_type) = (self.enclosing, self.in_type);
        match expr {
            Expr::Call(call) if self.reverse_call(call, enclosing, in_type) => {}
            Expr::Call(call) => {
                self.visit_operand(
                    &call.func,
                    Enclosing::Operand(OperatorPrecedence::CallAttribute),
                );
                self.enclosing = Enclosing::Free;
                self.in_type = false;
                self.visit_arguments(&call.arguments);
            }
            Expr::Subscript(subscript) => {
                self.visit_operand(
                    &subscript.value,
                    Enclosing::Operand(OperatorPrecedence::CallAttribute),
                );
                self.visit_type(&subscript.slice, Enclosing::Free);
            }
            _ => {
                self.enclosing = Enclosing::of(expr);
                self.in_type = in_type
                    && match expr {
                        Expr::Tuple(_) => true,
                        Expr::BinOp(union) => union.op == Operator::BitOr,
                        _ => false,
                    };
                walk_expr(self, expr);
            }
        }
        self.enclosing = enclosing;
        self.in_type = in_type;
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    const IMPORT: &str = "from _by_runtime import _by_cell_is, _by_in, _by_lit, _by_out, _by_raise, _by_specialization, _by_specialized, _by_subscript, _by_variance\n";

    /// the reverse of `source` in a module that imports every helper from the runtime
    fn rev(source: &str) -> String {
        let out = reverse_transpile(&format!("{IMPORT}{source}"), &Config::test_default()).unwrap();
        out.strip_prefix(IMPORT)
            .expect("the import should be left in place")
            .to_owned()
    }

    fn config() -> Config {
        Config {
            min_version: PythonVersion::PY313,
            ..Config::test_default()
        }
    }

    /// `source` lowered with the runtime imported, as a build does, and reversed again
    fn round_trip(source: &str) -> (String, String) {
        let config = Config {
            runtime_module: Some("_by_runtime".to_owned()),
            prune_unused_imports_after_reverse: true,
            class_variance: true,
            ..config()
        };
        let lowered = transpile(source, &config).unwrap();
        let reversed = reverse_transpile(&lowered, &config).unwrap();
        (lowered, reversed)
    }

    #[test]
    fn what_a_module_tells_the_runtime_about_stub_classes_is_dropped() {
        let (lowered, reversed) =
            round_trip("class Out[out T]: ...\nclass Later: ...\na = Out[str]()\nb = Later\n");
        assert!(lowered.contains("_by_told(__name__, "), "{lowered}");
        assert!(!reversed.contains("_by_told"), "{reversed}");
        assert!(reversed.contains("a = Out[str]()"), "{reversed}");
    }

    #[test]
    fn a_forward_reference_that_says_whose_it_is_is_written_as_a_name() {
        let (lowered, reversed) =
            round_trip("class Out[out T]: ...\na = Out[Later]()\nclass Later: ...\n");
        assert!(
            lowered.contains("a = _by_forward(Out[\"Later\"], __name__)()"),
            "{lowered}"
        );
        assert!(reversed.contains("a = Out[Later]()"), "{reversed}");
    }

    #[test]
    fn a_class_subscript_is_written_as_one() {
        assert_eq!(rev("a = _by_subscript(map, int)\n"), "a = map[int]\n");
        assert_eq!(
            rev("a = _by_subscript(zip, int, str)\n"),
            "a = zip[int, str]\n"
        );
    }

    #[test]
    fn a_nested_class_subscript_keeps_what_is_inside_it() {
        assert_eq!(
            rev("a = _by_subscript(map, _by_subscript(count, int))\n"),
            "a = map[count[int]]\n"
        );
        assert_eq!(
            rev("a = _by_subscript(map, int)(str, [1])\n"),
            "a = map[int](str, [1])\n"
        );
    }

    #[test]
    fn a_class_subscript_of_a_loose_value_is_parenthesized() {
        assert_eq!(
            rev("a = _by_subscript(b or c, int)\n"),
            "a = (b or c)[int]\n"
        );
        assert_eq!(
            rev("a = _by_subscript((b or c), (int))\n"),
            "a = (b or c)[(int)]\n"
        );
    }

    #[test]
    fn a_class_subscript_round_trips() {
        let source = "a = map[int]\n";
        let (lowered, reversed) = round_trip(source);
        assert!(lowered.contains("_by_subscript(map, int)"), "{lowered}");
        assert_eq!(reversed, source);
    }

    #[test]
    fn a_class_subscript_from_a_pasted_definition_is_written_as_one() {
        let lowered = transpile("a = map[int]\n", &config()).unwrap();
        assert!(lowered.contains("def _by_subscript("), "{lowered}");
        let out = reverse_transpile(&lowered, &config()).unwrap();
        assert!(out.contains("a = map[int]\n"), "{out}");
    }

    /// a module that binds a builtin itself has a pasted definition read it under another name
    #[test]
    fn a_pasted_definition_that_renames_a_builtin_is_still_the_runtime() {
        let lowered = transpile(
            "def hasattr(o: object, n: str) -> bool: return True\na = map[int]\n",
            &config(),
        )
        .unwrap();
        assert!(
            !lowered.contains("if hasattr(origin"),
            "the fixture should rename the read: {lowered}"
        );
        let out = reverse_transpile(&lowered, &config()).unwrap();
        assert!(out.contains("a = map[int]\n"), "{out}");
    }

    #[test]
    fn a_pasted_definition_is_not_rewritten() {
        let lowered = transpile(
            "def f[T](t: object):\n    print(t is T)\nf[int](1)\n",
            &config(),
        )
        .unwrap();
        let out = reverse_transpile(&lowered, &config()).unwrap();
        assert!(out.contains("print(t is T)"), "{out}");
        assert!(
            out.contains("any(_by_cell_is(value, arm) for arm in _by_union_args(cell))"),
            "the runtime's own recursion stays a call: {out}"
        );
    }

    #[test]
    fn a_raise_is_written_in_the_expression() {
        assert_eq!(
            rev("print(1, _by_raise(ValueError))\n"),
            "print(1, raise ValueError)\n"
        );
        assert_eq!(rev("print(_by_raise(a, b))\n"), "print(raise a from b)\n");
        assert_eq!(rev("print(_by_raise(), 1)\n"), "print(raise, 1)\n");
        assert_eq!(rev("x = _by_raise(E)\n"), "x = raise E\n");
    }

    #[test]
    fn a_raise_keeps_its_operands_parentheses() {
        assert_eq!(
            rev("print(_by_raise((A), (B)))\n"),
            "print(raise (A) from (B))\n"
        );
    }

    #[test]
    fn a_raise_that_would_take_in_what_follows_is_parenthesized() {
        assert_eq!(rev("x = _by_raise(E) + 1\n"), "x = (raise E) + 1\n");
        assert_eq!(rev("x = 1 + _by_raise(E) + 2\n"), "x = 1 + (raise E) + 2\n");
        assert_eq!(rev("x = _by_raise() or 1\n"), "x = (raise) or 1\n");
        assert_eq!(
            rev("x = _by_raise(E) if c else d\n"),
            "x = (raise E) if c else d\n"
        );
        assert_eq!(rev("_by_raise(E).attribute\n"), "(raise E).attribute\n");
        assert_eq!(rev("_by_raise(E), 1\n"), "(raise E), 1\n");
    }

    #[test]
    fn a_parenthesized_raise_is_not_parenthesized_again() {
        assert_eq!(rev("x = (_by_raise(E)) + 1\n"), "x = (raise E) + 1\n");
    }

    #[test]
    fn a_raise_operand_a_raise_cannot_take_is_parenthesized() {
        assert_eq!(rev("f(_by_raise(x := 1))\n"), "f(raise (x := 1))\n");
    }

    #[test]
    fn a_raise_round_trips() {
        let source = "def f():\n    print(1, raise ValueError from None)\n";
        let (lowered, reversed) = round_trip(source);
        assert!(lowered.contains("_by_raise(ValueError, None)"), "{lowered}");
        assert_eq!(reversed, source);
    }

    #[test]
    fn a_literal_type_argument_is_written_as_the_literal() {
        assert_eq!(rev("B[_by_lit(1)](1)\n"), "B[1](1)\n");
        assert_eq!(rev("B[_by_lit(\"x\")](\"x\")\n"), "B[\"x\"](\"x\")\n");
        assert_eq!(rev("B[_by_lit(-1)]\n"), "B[-1]\n");
        assert_eq!(rev("B[_by_lit(Color.RED)]\n"), "B[Color.RED]\n");
        assert_eq!(rev("B[_by_lit(True), _by_lit(None)]\n"), "B[True, None]\n");
        assert_eq!(rev("B[list[_by_lit(1)] | str]\n"), "B[list[1] | str]\n");
    }

    #[test]
    fn a_literal_written_as_code_points_is_written_with_quotes() {
        assert_eq!(rev("B[_by_lit(text=(120,))]\n"), "B[\"x\"]\n");
        assert_eq!(rev("B[_by_lit(text=(97, 34))]\n"), "B[\"a\\\"\"]\n");
        assert_eq!(rev("B[_by_lit(text=())]\n"), "B[\"\"]\n");
        assert_eq!(rev("B[_by_lit(data=(1,))]\n"), "B[b\"\\x01\"]\n");
    }

    /// the code points are there because a replacement field below 3.12 can hold neither a
    /// backslash nor its f-string's own quote, which goes for the `.by` written from them too
    #[test]
    fn a_literal_written_as_code_points_in_an_f_string_stays_valid_below_3_12() {
        let reversed = rev("print(f\"{A[_by_lit(text=(120,))]('x')}\")\n");
        assert_eq!(reversed, "print(f\"{A['x']('x')}\")\n");
        let source = format!("class A[T]:\n    def __init__(self, t: T): ...\n{reversed}");
        let config = Config {
            min_version: PythonVersion::PY311,
            ..Config::test_default()
        };
        assert!(transpile(&source, &config).is_ok());

        assert_eq!(rev("f'{B[_by_lit(text=(120,))]}'\n"), "f'{B[\"x\"]}'\n");
        assert_eq!(rev("f\"{B[_by_lit(data=(97,))]}\"\n"), "f\"{B[b'a']}\"\n");
    }

    #[test]
    fn a_literal_no_replacement_field_can_hold_stays_as_code_points() {
        for source in [
            "f\"{B[_by_lit(text=(10,))]}\"\n",
            "f\"{B[_by_lit(text=(39,))]}\"\n",
            "f\"{B[_by_lit(data=(1,))]}\"\n",
            "f\"{f'{B[_by_lit(text=(120,))]}'}\"\n",
        ] {
            assert_eq!(rev(source), source);
        }
    }

    #[test]
    fn a_literal_type_argument_of_another_helper_is_written_as_the_literal() {
        assert_eq!(rev("_by_subscript(map, _by_lit(1))\n"), "map[1]\n");
        assert_eq!(rev("_by_specialized(c.m, _by_lit(1))(1)\n"), "c.m[1](1)\n");
        assert_eq!(rev("_by_cell_is(x, _by_lit(1))\n"), "x is 1\n");
    }

    /// outside a type it is a class, which the literal it holds isn't
    #[test]
    fn a_literal_type_read_as_a_value_is_left_alone() {
        for source in [
            "x = _by_lit(1)\n",
            "_by_protocol_is(x, [(\"attr\", \"a\", _by_lit(True), 0)])\n",
            "B[f(_by_lit(1))]\n",
        ] {
            assert_eq!(rev(source), source);
        }
    }

    #[test]
    fn a_literal_without_a_bare_spelling_is_left_alone() {
        for source in [
            "B[_by_lit(1, 2)]\n",
            "B[_by_lit(text=(1114112,))]\n",
            "B[_by_lit(data=(256,))]\n",
            "B[_by_lit(text=name)]\n",
        ] {
            assert_eq!(rev(source), source);
        }
    }

    #[test]
    fn a_projection_is_written_with_its_keyword() {
        assert_eq!(rev("Box[_by_out(int)]\n"), "Box[out int]\n");
        assert_eq!(rev("Box[_by_in(int)]\n"), "Box[in int]\n");
        assert_eq!(
            rev("Box[_by_out(list[_by_lit(1)])]\n"),
            "Box[out list[1]]\n"
        );
        assert_eq!(
            rev("_by_cell_is(x, Box[_by_out(int)])\n"),
            "x is Box[out int]\n"
        );
    }

    #[test]
    fn a_projection_read_as_a_value_is_left_alone() {
        for source in ["x = _by_out(int)\n", "B[f(_by_in(int))]\n"] {
            assert_eq!(rev(source), source);
        }
    }

    #[test]
    fn telling_a_class_its_variance_is_dropped() {
        assert_eq!(
            rev("x = _by_variance(1)(Sequence)[int]\n"),
            "x = Sequence[int]\n"
        );
        assert_eq!(
            rev("x = _by_subscript(_by_variance(1)(map), int)\n"),
            "x = map[int]\n"
        );
        assert_eq!(
            rev("@dataclass\n@_by_variance(1)\nclass Out[T]:\n    x = 1\n"),
            "@dataclass\nclass Out[T]:\n    x = 1\n"
        );
        assert_eq!(
            rev("def f():\n    @_by_variance(0, 1)\n    class Out[A, B]:\n        x = 1\n"),
            "def f():\n    class Out[A, B]:\n        x = 1\n"
        );
    }

    #[test]
    fn a_decorator_of_their_own_called_the_same_stays() {
        let source = "def _by_variance(*codes):\n    return lambda c: c\n@_by_variance(1)\nclass Out[T]:\n    x = 1\n";
        assert_eq!(reverse_transpile(source, &config()).unwrap(), source);
    }

    #[test]
    fn a_projection_round_trips() {
        let (out, back) = round_trip(indoc! {"
            class Box[T]:
                value: T
            def f[T](x: object) -> bool:
                return x is T
            f[Box[out object]](1)
            f[Box[in object]](1)
        "});
        assert!(out.contains("f[Box[_by_out(object)]](1)"), "{out}");
        assert!(back.contains("f[Box[out object]](1)"), "{back}");
        assert!(back.contains("f[Box[in object]](1)"), "{back}");
    }

    #[test]
    fn a_class_that_varies_round_trips() {
        let (out, back) = round_trip(indoc! {"
            from collections.abc import Sequence

            class Inf[T]:
                def get(self) -> T:
                    raise NotImplementedError
            def f[T](x: object) -> bool:
                return x is T
            f[Sequence[object]](1)
        "});
        assert!(out.contains("@_by_variance(1)\nclass Inf[T]:"), "{out}");
        assert!(
            out.contains("f[_by_variance(1)(Sequence)[object]](1)"),
            "{out}"
        );
        assert!(!back.contains("_by_variance"), "{back}");
        assert!(back.contains("class Inf[T]:"), "{back}");
        assert!(back.contains("f[Sequence[object]](1)"), "{back}");
    }

    #[test]
    fn an_explicit_literal_type_argument_round_trips() {
        let (out, back) = round_trip(indoc! {"
            def f[T](x: object) -> bool:
                return x is T
            f[1](1)
            f[\"a\"](1)
        "});
        assert!(out.contains("f[_by_lit(1)](1)"), "{out}");
        assert!(back.contains("f[1](1)"), "{back}");
        assert!(back.contains("f[\"a\"](1)"), "{back}");
    }

    #[test]
    fn a_specialized_call_is_written_with_its_subscript() {
        assert_eq!(rev("_by_specialized(c.m, int)(1)\n"), "c.m[int](1)\n");
        assert_eq!(
            rev("_by_specialized(c.m, int, str)(1)\n"),
            "c.m[int, str](1)\n"
        );
        assert_eq!(
            rev("_by_specialized(c.m, int, foo=str)(1)\n"),
            "c.m[int, foo=str](1)\n"
        );
    }

    /// it stays spelled, as `f[int](1)` does where the forward direction injected that
    #[test]
    fn an_injected_specialization_round_trips_to_its_subscript() {
        let source = indoc! {"
            class Box[T]:
                def __init__(self, t: T): ...
            class C:
                def m[T](self, t: T) -> Box[T]:
                    return Box(t)
            def f(c: C):
                return c.m(int(1))
        "};
        let (lowered, reversed) = round_trip(source);
        assert!(
            lowered.contains("_by_specialized(c.m, int)(int(1))"),
            "{lowered}"
        );
        assert!(reversed.contains("return c.m[int](int(1))\n"), "{reversed}");
        assert!(!reversed.contains("_by_runtime"), "{reversed}");
    }

    #[test]
    fn a_cell_test_is_written_as_a_type_test() {
        assert_eq!(rev("print(_by_cell_is(x, T))\n"), "print(x is T)\n");
        assert_eq!(
            rev("if _by_cell_is(x, T):\n    pass\n"),
            "if x is T:\n    pass\n"
        );
    }

    #[test]
    fn a_cell_test_is_parenthesized_where_it_would_regroup() {
        assert_eq!(rev("y = _by_cell_is(x, T) + 1\n"), "y = (x is T) + 1\n");
        assert_eq!(rev("y = _by_cell_is(x, T) == 1\n"), "y = (x is T) == 1\n");
        assert_eq!(rev("y = _by_cell_is(x, T).a\n"), "y = (x is T).a\n");
        assert_eq!(rev("y = -_by_cell_is(x, T)\n"), "y = -(x is T)\n");
    }

    #[test]
    fn a_cell_test_is_not_parenthesized_where_it_would_not() {
        assert_eq!(rev("y = not _by_cell_is(x, T)\n"), "y = not x is T\n");
        assert_eq!(rev("y = _by_cell_is(x, T) and z\n"), "y = x is T and z\n");
        assert_eq!(rev("y = (_by_cell_is(x, T)) + 1\n"), "y = (x is T) + 1\n");
    }

    #[test]
    fn a_cell_test_parenthesizes_a_loose_operand() {
        assert_eq!(rev("y = _by_cell_is(a or b, T)\n"), "y = (a or b) is T\n");
        assert_eq!(rev("y = _by_cell_is(a < b, T)\n"), "y = (a < b) is T\n");
        assert_eq!(rev("y = _by_cell_is(a + b, T)\n"), "y = a + b is T\n");
        assert_eq!(rev("y = _by_cell_is(a, T | None)\n"), "y = a is T | None\n");
    }

    #[test]
    fn a_cell_test_round_trips() {
        let source = "def f[T](t: object):\n    print(t is T)\nf[int](1)\n";
        let (lowered, reversed) = round_trip(source);
        assert!(lowered.contains("print(_by_cell_is(t, T))"), "{lowered}");
        assert_eq!(reversed, source);
    }

    #[test]
    fn a_package_qualified_import_is_the_runtime() {
        let out = reverse_transpile(
            "from pkg.sub._by_runtime import _by_subscript\na = _by_subscript(map, int)\n",
            &Config::test_default(),
        )
        .unwrap();
        assert!(out.contains("a = map[int]\n"), "{out}");
    }

    fn unchanged(source: &str) {
        assert_eq!(
            reverse_transpile(source, &Config::test_default()).unwrap(),
            source
        );
    }

    #[test]
    fn a_function_of_the_same_name_is_left_alone() {
        unchanged(indoc! {"
            def _by_subscript(a, b):
                return a

            x = _by_subscript(map, int)
        "});
    }

    #[test]
    fn a_function_that_extends_a_definition_of_the_runtime_is_left_alone() {
        let pasted = transpile("a = map[int]\n", &config()).unwrap();
        let start = pasted.find("def _by_subscript(").unwrap();
        let end = pasted[start..].find("\n\n").unwrap() + start;
        unchanged(&format!(
            "{}\n    return 1\n\nx = _by_subscript(map, int)\n",
            &pasted[start..end]
        ));
    }

    #[test]
    fn an_import_from_elsewhere_is_left_alone() {
        unchanged("from mine import _by_subscript\nx = _by_subscript(map, int)\n");
        unchanged("from ._by_runtime import _by_subscript\nx = _by_subscript(map, int)\n");
        unchanged("from _by_runtime import other as _by_subscript\nx = _by_subscript(map, int)\n");
    }

    #[test]
    fn an_unbound_name_is_left_alone() {
        unchanged("x = _by_subscript(map, int)\n");
    }

    #[test]
    fn a_rebound_name_is_left_alone() {
        unchanged(indoc! {"
            from _by_runtime import _by_subscript
            _by_subscript = print
            x = _by_subscript(map, int)
        "});
    }

    #[test]
    fn a_name_a_scope_takes_over_is_left_alone() {
        unchanged(indoc! {"
            from _by_runtime import _by_subscript
            def f(_by_subscript):
                return _by_subscript(map, int)
        "});
    }

    #[test]
    fn a_shape_the_forward_direction_does_not_write_is_left_alone() {
        for source in [
            "_by_subscript(map)\n",
            "_by_subscript(*args)\n",
            "_by_subscript(map, int,)\n",
            "_by_subscript(map, **kw)\n",
            "_by_raise(a, b, c)\n",
            "_by_raise(*a)\n",
            "_by_raise(x for x in y)\n",
            "_by_raise(a, cause=b)\n",
            "_by_cell_is(x)\n",
            "_by_cell_is(*x, T)\n",
            "_by_specialized(c.m)\n",
        ] {
            assert_eq!(rev(source), source);
        }
    }

    #[test]
    fn a_comment_inside_the_call_keeps_it_as_written() {
        let source = indoc! {"
            x = _by_subscript(  # note
                map,
                int,
            )
        "};
        assert_eq!(rev(source), source);
    }

    #[test]
    fn a_call_written_across_lines_is_kept_balanced() {
        assert_eq!(rev("x = _by_cell_is(\n    a,\n    T\n)\n"), "x = a is T\n");
    }

    #[test]
    fn a_printed_field_is_left_alone() {
        let source = "print(f\"{_by_subscript(map, int) = }\")\n";
        assert_eq!(rev(source), source);
    }

    #[test]
    fn a_specialization_of_a_class_that_answers_a_subscript_is_written_as_one() {
        assert_eq!(
            rev("a = f[_by_specialization(U, int)]\n"),
            "a = f[U[int]]\n"
        );
        assert_eq!(
            rev("a = f[_by_specialization(U, int, str)]\n"),
            "a = f[U[int, str]]\n"
        );
        assert_eq!(
            rev("a = f[_by_specialization(U, _by_lit(1))]\n"),
            "a = f[U[1]]\n"
        );
    }

    #[test]
    fn a_type_argument_a_class_answers_round_trips() {
        let source = indoc! {"
            class U[T]:
                def __class_getitem__(cls, item):
                    return (item, 1)

            def chk[T](x: object) -> bool:
                return x is T

            a = chk[U[int]](1)
            b = chk[U[1]](1)
            c = chk[list[U[int]]]([])
            d = U[1]
        "};
        let (lowered, reversed) = round_trip(source);
        assert!(
            lowered.contains("chk[_by_specialization(U, int)](1)"),
            "{lowered}"
        );
        assert!(lowered.contains("d = U[_by_lit(1)]\n"), "{lowered}");
        assert_eq!(reversed, source);
    }

    /// `tuple[()]` is `.by` as it stands, and lowers to the same python
    #[test]
    fn the_empty_tuple_as_a_type_argument_is_stable_through_a_round_trip() {
        let (lowered, reversed) = round_trip("class Out[T]\n\na = Out[()]\nb: Out[()]\n");
        assert!(lowered.contains("a = Out[tuple[()]]\n"), "{lowered}");
        assert!(lowered.contains("b: Out[tuple[()]]\n"), "{lowered}");
        assert!(reversed.contains("a = Out[tuple[()]]\n"), "{reversed}");
        assert!(reversed.contains("b: Out[tuple[()]]\n"), "{reversed}");
        let (again, _) = round_trip(&reversed);
        assert_eq!(again, lowered);
    }

    /// the module's own binding stays its own, and what the emitted code reads under another
    /// name is reversed all the same
    #[test]
    fn a_helper_read_under_a_name_of_its_own_round_trips() {
        let source = indoc! {"
            class B[T]

            _by_lit = 5
            a = B[1]
        "};
        let (lowered, reversed) = round_trip(source);
        assert!(lowered.contains("import _by_lit as _by_lit2"), "{lowered}");
        assert!(lowered.contains("a = B[_by_lit2(1)]\n"), "{lowered}");
        assert_eq!(reversed, source);
    }

    #[test]
    fn a_helper_pasted_under_a_name_of_its_own_is_read_under_it() {
        let source = indoc! {"
            class B[T]: ...

            _by_lit = 5
            a = B[1]
        "};
        let config = config();
        let lowered = transpile(source, &config).unwrap();
        assert!(lowered.contains("def _by_lit2("), "{lowered}");
        assert!(lowered.contains("a = B[_by_lit2(1)]\n"), "{lowered}");
        let reversed = reverse_transpile(&lowered, &config).unwrap();
        assert!(reversed.contains("a = B[1]\n"), "{reversed}");
    }

    /// a name that only looks like one the module would read a helper under is the author's
    #[test]
    fn a_function_named_like_a_helper_read_under_a_name_of_its_own_is_left_alone() {
        let source = "def _by_lit2(x):\n    return x\na = B[_by_lit2(1)]\n";
        assert_eq!(
            reverse_transpile(source, &Config::test_default()).unwrap(),
            source
        );
    }
}
