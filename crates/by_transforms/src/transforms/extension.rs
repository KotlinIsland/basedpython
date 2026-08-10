//! Lowering for `extension` declarations and their call sites.
//!
//! An `extension list:` block parses to a `ClassDef` carrying a synthetic
//! `extension_def` marker. Python has no extension methods and builtin C types
//! cannot be monkey-patched, so extensions are resolved entirely at transpile
//! time:
//!
//! - each member lowers to a module-level backing function
//!   (`def second(self)` inside `extension list:` → `def
//!   _by_ext__list__second(self)`), tagged with a `# basedpython: extension
//!   <kind> <header>` marker comment carrying the member kind and the original
//!   header (including any bracket bounds) for the reverse transform
//! - call sites are rewritten by type: ty resolves `xs.second()` to the
//!   extension member, and the call lowers to `_by_ext__list__second(xs)`.
//!   a computed property drops the parentheses (`name.shouty` →
//!   `_by_ext__str__shouty(name)`); an unapplied method reference becomes a
//!   `functools.partial`
//! - when the extension lives in another module, the precise import of the
//!   backing function is emitted (`from textwrap import
//!   _by_ext__str__dedented`), so `import textwrap` on the surface carries
//!   the extensions with no runtime cost
//!
//! backing functions are plain and unannotated: their annotations reference
//! the extended type's type parameters (`Element` on `list`), which have no
//! runtime binding at module level. member bodies pass through as
//! [`Fragment::Src`] spans, so lowerings inside them still compose. when a
//! module declares more than one extension of the same target, later ones
//! mangle with an ordinal (`_by_ext2__list__…`) so their members don't
//! collide — the same rule ty uses when resolving call sites

use std::collections::{BTreeSet, HashMap};

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, PySourceType, Stmt};
use ruff_python_parser::parse_unchecked_source;
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::ExtensionMemberKind;

use super::ast_driver::{Fragment, PassContext, TypeAwarePass};
use super::source_util::{is_synthetic_decorator, line_start};
use crate::type_info::TypeInfo;

/// the marker-comment prefix that ties a backing function to its extension
pub(crate) const EXTENSION_MARKER: &str = "# basedpython: extension";

/// the mangled module-level name of an extension member's backing function.
/// `ordinal` is the extension's occurrence index among same-target extensions
/// in its module — mirrored by ty's `backing_function_name`.
///
/// exactly one leading underscore: python private-name-mangles any `__name`
/// reference inside a class body, so a two-underscore name would break an
/// extension call written in one
pub(crate) fn backing_name(target: &str, ordinal: usize, member: &str) -> String {
    if ordinal == 0 {
        format!("_by_ext__{target}__{member}")
    } else {
        format!("_by_ext{}__{target}__{member}", ordinal + 1)
    }
}

/// one line of whatever came in: runs of whitespace (including newlines) become
/// a single space, and none is left against a bracket or a comma. a marker
/// comment has to be one line, and the source it quotes need not be
fn collapse_whitespace(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    for word in header.split_whitespace() {
        let joins_left = out.ends_with(['(', '[']) || out.is_empty();
        let joins_right = word.starts_with([')', ']', ',']);
        if !joins_left && !joins_right {
            out.push(' ');
        }
        out.push_str(word);
    }
    // a trailing comma before the closing bracket is the formatter's magic
    // trailing comma, not part of the header's meaning
    out.replace(",)", ")").replace(",]", "]")
}

/// the spelled member kind, syntactically: a real `@property` decorator or a
/// synthetic `static` / `classmethod` modifier marker
fn member_kind(func: &ast::StmtFunctionDef, source: &str) -> ExtensionMemberKind {
    for decorator in &func.decorator_list {
        let Expr::Name(name) = &decorator.expression else {
            continue;
        };
        if is_synthetic_decorator(source, decorator) {
            match name.id.as_str() {
                "static" => return ExtensionMemberKind::StaticMethod,
                "classmethod" => return ExtensionMemberKind::ClassMethod,
                // an accessor block writes no `@`, so its getter carries a
                // synthetic marker where a decorated member would say `@property`
                "__property__" => return ExtensionMemberKind::Property,
                "__static_property__" => return ExtensionMemberKind::StaticProperty,
                _ => {}
            }
        } else if name.id.as_str() == "property" {
            return ExtensionMemberKind::Property;
        }
    }
    ExtensionMemberKind::Method
}

/// Whether `func` is the getter the parser synthesized for a property accessor
/// block, rather than a member the author wrote as a `def`.
fn is_accessor_block_member(func: &ast::StmtFunctionDef, source: &str) -> bool {
    func.decorator_list.iter().any(|decorator| {
        is_synthetic_decorator(source, decorator)
            && matches!(&decorator.expression, Expr::Name(name)
                if matches!(name.id.as_str(), "__property__" | "__static_property__"))
    })
}

fn kind_word(kind: ExtensionMemberKind) -> &'static str {
    match kind {
        ExtensionMemberKind::Method => "method",
        ExtensionMemberKind::Property => "property",
        ExtensionMemberKind::StaticMethod => "static",
        ExtensionMemberKind::ClassMethod => "classmethod",
        ExtensionMemberKind::StaticProperty => "staticproperty",
    }
}

pub(crate) fn parse_kind_word(word: &str) -> Option<ExtensionMemberKind> {
    match word {
        "method" => Some(ExtensionMemberKind::Method),
        "property" => Some(ExtensionMemberKind::Property),
        "static" => Some(ExtensionMemberKind::StaticMethod),
        "classmethod" => Some(ExtensionMemberKind::ClassMethod),
        "staticproperty" => Some(ExtensionMemberKind::StaticProperty),
        _ => None,
    }
}

/// render a member's parameter list without annotations (they reference type
/// parameters with no runtime binding). defaults pass through as source spans
/// so lowerings inside them compose
fn parameter_fragments(parameters: &ast::Parameters, fragments: &mut Vec<Fragment>) {
    let mut first = true;
    let mut separate = |fragments: &mut Vec<Fragment>| {
        if !first {
            fragments.push(Fragment::Lit(", ".to_owned()));
        }
        first = false;
    };
    for param in &parameters.posonlyargs {
        separate(fragments);
        fragments.push(Fragment::Lit(param.parameter.name.to_string()));
        if let Some(default) = &param.default {
            fragments.push(Fragment::Lit("=".to_owned()));
            fragments.push(Fragment::Src(default.range()));
        }
    }
    if !parameters.posonlyargs.is_empty() {
        separate(&mut *fragments);
        fragments.push(Fragment::Lit("/".to_owned()));
    }
    for param in &parameters.args {
        separate(fragments);
        fragments.push(Fragment::Lit(param.parameter.name.to_string()));
        if let Some(default) = &param.default {
            fragments.push(Fragment::Lit("=".to_owned()));
            fragments.push(Fragment::Src(default.range()));
        }
    }
    if let Some(vararg) = &parameters.vararg {
        separate(&mut *fragments);
        fragments.push(Fragment::Lit(format!("*{}", vararg.name)));
    } else if !parameters.kwonlyargs.is_empty() {
        separate(&mut *fragments);
        fragments.push(Fragment::Lit("*".to_owned()));
    }
    for param in &parameters.kwonlyargs {
        separate(fragments);
        fragments.push(Fragment::Lit(param.parameter.name.to_string()));
        if let Some(default) = &param.default {
            fragments.push(Fragment::Lit("=".to_owned()));
            fragments.push(Fragment::Src(default.range()));
        }
    }
    if let Some(kwarg) = &parameters.kwarg {
        separate(&mut *fragments);
        fragments.push(Fragment::Lit(format!("**{}", kwarg.name)));
    }
}

/// lowers `extension` blocks to module-level backing functions
pub(crate) struct ExtensionBlockPass<'a> {
    source: &'a str,
}

impl<'a> ExtensionBlockPass<'a> {
    pub(crate) fn new(source: &'a str) -> Self {
        Self { source }
    }

    /// lower one extension block to its backing functions, in place. the block
    /// is replaced where it stands so the method bodies keep their original
    /// source ranges — that lets the sibling passes (extension-call rewrite,
    /// coalesce, cast, …) compose their edits inside the `Src` body spans. the
    /// finished `def`s are later hoisted to the module top by
    /// [`hoist_backing_functions`], because a member may be called before the
    /// block's source position
    fn lower_block(
        &self,
        class: &ast::StmtClassDef,
        ordinal: usize,
        types: &dyn TypeInfo,
        ctx: &mut PassContext,
    ) {
        let source = self.source;
        let target = class.name.as_str();
        // the header spelling between `extension ` and `:`, bounds and
        // conformance list included — carried on each backing function so the
        // reverse transform can re-sugar the block faithfully
        let header_end = class
            .arguments
            .as_deref()
            .map(Ranged::end)
            .or_else(|| {
                class
                    .type_params
                    .as_deref()
                    .map(|params| params.range.end())
            })
            .unwrap_or_else(|| class.name.range().end());
        // collapsed to one line: the header goes into a `#` comment, and the
        // formatter wraps a long conformance list across lines all by itself —
        // splicing that in verbatim produced output that would not parse
        let header = collapse_whitespace(
            &source[usize::from(class.name.range().start())..usize::from(header_end)],
        );
        let header = header.as_str();

        let mut fragments: Vec<Fragment> = Vec::new();
        let mut first_member = true;
        for stmt in &class.body {
            let func = match stmt {
                Stmt::FunctionDef(func) => func,
                // a docstring, `...`, or `pass` adds nothing to the lowering
                Stmt::Expr(expr)
                    if matches!(
                        &*expr.value,
                        Expr::StringLiteral(_) | Expr::EllipsisLiteral(_)
                    ) =>
                {
                    continue;
                }
                Stmt::Pass(_) => continue,
                other => {
                    ctx.errors.push(format!(
                        "extension `{target}` may only contain methods and computed \
                        properties (line with offset {})",
                        u32::from(other.range().start()),
                    ));
                    continue;
                }
            };

            if !first_member {
                fragments.push(Fragment::Lit("\n\n".to_owned()));
            }
            first_member = false;

            let kind = member_kind(func, source);
            // real decorators other than `property` are kept (dedented to the
            // module level); `property` and the synthetic modifier markers are
            // consumed by the lowering itself
            for decorator in &func.decorator_list {
                if is_synthetic_decorator(source, decorator) {
                    continue;
                }
                if matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == "property")
                {
                    continue;
                }
                fragments.push(Fragment::Lit("@".to_owned()));
                fragments.push(Fragment::Src(decorator.expression.range()));
                fragments.push(Fragment::Lit("\n".to_owned()));
            }

            fragments.push(Fragment::Lit(format!(
                "def {}(",
                backing_name(target, ordinal, func.name.as_str())
            )));
            parameter_fragments(&func.parameters, &mut fragments);
            fragments.push(Fragment::Lit(")".to_owned()));

            let marker = format!("{EXTENSION_MARKER} {} {header}", kind_word(kind));
            let (Some(first_stmt), Some(last_stmt)) = (func.body.first(), func.body.last()) else {
                fragments.push(Fragment::Lit(format!(": ...  {marker}")));
                continue;
            };
            // an accessor block's body is synthesized (a `get() = expr` accessor's
            // `return` is nowhere in the source), so it has to be rendered rather
            // than passed through. same trade-off the properties pass already
            // makes: a basedpython construct inside an accessor body is not lowered
            if is_accessor_block_member(func, source) {
                fragments.push(Fragment::Lit(format!(
                    ":  {marker}\n{}",
                    super::properties::render_body(&func.body, "    ")
                )));
                continue;
            }
            let body_line_start = line_start(source, first_stmt.range().start());
            let inline = source
                [usize::from(body_line_start)..usize::from(first_stmt.range().start())]
                .contains(|c: char| !c.is_whitespace());
            if inline {
                fragments.push(Fragment::Lit(": ".to_owned()));
                fragments.push(Fragment::Src(TextRange::new(
                    first_stmt.range().start(),
                    last_stmt.range().end(),
                )));
                fragments.push(Fragment::Lit(format!("  {marker}")));
            } else {
                fragments.push(Fragment::Lit(format!(":  {marker}\n")));
                fragments.push(Fragment::Src(TextRange::new(
                    body_line_start,
                    last_stmt.range().end(),
                )));
            }
        }

        // a conformance extension also registers its witness table, after the
        // backing functions its entries name
        super::conformance::registration_fragments(
            class,
            types,
            ctx,
            &mut fragments,
            !first_member,
        );

        ctx.template_edits.push((class.range, fragments));
    }
}

impl TypeAwarePass for ExtensionBlockPass<'_> {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        // occurrence index per target name, module-wide — the mangle
        // discriminator shared with ty's `backing_function_name`
        let mut ordinals: HashMap<&str, usize> = HashMap::new();
        for stmt in stmts {
            let Stmt::ClassDef(class) = stmt else {
                continue;
            };
            if !class.is_extension() {
                continue;
            }
            let ordinal = *ordinals
                .entry(class.name.as_str())
                .and_modify(|n| *n += 1)
                .or_insert(0);
            self.lower_block(class, ordinal, types, ctx);
        }
    }
}

/// move the extension backing functions to the module top.
///
/// the block lowering re-emits each backing `def` where its `extension` block
/// stood, so the method bodies keep their source ranges and the sibling passes
/// compose their edits inside them. but a member may be *called* before that
/// position — a plain top-level call resolves left-to-right, so the `def` must
/// precede every use. this runs on the finished output text: it re-parses,
/// finds the top-level `_by_ext…` functions, and moves their line blocks to
/// just after the preamble and the leading imports, permuting the line table
/// identically so source maps stay aligned
///
/// `preamble_end` is one past the last byte of the generated preamble. the
/// defs have to stay below it: the preamble carries the runtime helpers a
/// lowered body — or a parameter default, which runs at `def` time — reaches
/// for, and a `def` above them raises `NameError` at import
pub(crate) fn hoist_backing_functions(
    out: String,
    table: Vec<Option<u32>>,
    preamble_end: usize,
) -> (String, Vec<Option<u32>>) {
    // cheap early-out: no backing functions were emitted
    if !out.contains(EXTENSION_MARKER) {
        return (out, table);
    }

    let parsed = parse_unchecked_source(&out, PySourceType::Python);
    if !parsed.errors().is_empty() {
        // the output isn't clean python yet (a later phase may still fix it);
        // don't risk a move against a shape we can't trust
        return (out, table);
    }
    let module = parsed.suite();

    // the byte offset of the last byte that stays above the backing functions:
    // the end of the preamble, then of the leading run of the file's own
    // imports (and its docstring)
    let mut insert_offset = preamble_end.saturating_sub(1);
    let own = module
        .iter()
        .skip_while(|stmt| usize::from(stmt.range().end()) <= preamble_end);
    for (index, stmt) in own.enumerate() {
        let is_docstring =
            index == 0 && matches!(stmt, Stmt::Expr(expr) if expr.value.is_string_literal_expr());
        if matches!(stmt, Stmt::Import(_) | Stmt::ImportFrom(_)) || is_docstring {
            insert_offset = usize::from(stmt.range().end());
        } else {
            break;
        }
    }

    // byte ranges of the top-level backing functions (decorators included)
    let mut backing: Vec<(usize, usize)> = Vec::new();
    for stmt in module {
        if let Stmt::FunctionDef(func) = stmt
            && func.name.starts_with("_by_ext")
        {
            let start = func
                .decorator_list
                .first()
                .map_or(func.range().start(), |dec| dec.range().start());
            backing.push((usize::from(start), usize::from(func.range().end())));
        }
    }
    if backing.is_empty() {
        return (out, table);
    }

    // line-block move. the table has one entry per output line, so permuting
    // whole lines keeps it aligned
    let had_trailing_newline = out.ends_with('\n');
    let body = out.strip_suffix('\n').unwrap_or(&out);
    let lines: Vec<&str> = body.split('\n').collect();
    if table.len() != lines.len() {
        // can't map lines to table entries one-to-one; leave order untouched
        // rather than emit a misaligned table
        return (out, table);
    }

    let line_starts: Vec<usize> = std::iter::once(0)
        .chain(body.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let line_of = |offset: usize| match line_starts.binary_search(&offset) {
        Ok(line) => line,
        Err(next) => next - 1,
    };

    let insert_line = if insert_offset == 0 {
        0
    } else {
        line_of(insert_offset.min(body.len())) + 1
    };

    let mut moved: Vec<usize> = Vec::new();
    for (start, end) in &backing {
        let first = line_of(*start);
        let last = line_of(end.saturating_sub(1));
        moved.extend(first..=last);
    }
    let moved_set: BTreeSet<usize> = moved.iter().copied().collect();

    // head (before the insertion point) + the moved defs + the tail, each with
    // the moved lines removed — a permutation of every original line index
    let mut order: Vec<usize> = Vec::with_capacity(lines.len());
    order.extend((0..insert_line).filter(|i| !moved_set.contains(i)));
    order.extend(moved.iter().copied());
    order.extend((insert_line..lines.len()).filter(|i| !moved_set.contains(i)));

    let new_lines: Vec<&str> = order.iter().map(|&i| lines[i]).collect();
    let new_table: Vec<Option<u32>> = order.iter().map(|&i| table[i]).collect();

    let mut result = new_lines.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    (result, new_table)
}

/// does the postfix spine of `expr` contain an optional (`?.`) segment? the
/// receiver of an extension rewrite must not, because the rewrite hoists the
/// member access out of the chain's short-circuit
pub(super) fn spine_has_optional(expr: &Expr) -> bool {
    match expr {
        Expr::Attribute(attr) => attr.optional || spine_has_optional(&attr.value),
        Expr::Subscript(subscript) => spine_has_optional(&subscript.value),
        Expr::Call(call) => spine_has_optional(&call.func),
        _ => false,
    }
}

/// the source span of a call's arguments (positional and keyword), without
/// the parentheses. `None` when the call has no arguments
pub(super) fn arguments_span(arguments: &ast::Arguments) -> Option<TextRange> {
    let starts = arguments
        .args
        .iter()
        .map(|arg| arg.range().start())
        .chain(arguments.keywords.iter().map(|kw| kw.range().start()));
    let ends = arguments
        .args
        .iter()
        .map(|arg| arg.range().end())
        .chain(arguments.keywords.iter().map(|kw| kw.range().end()));
    Some(TextRange::new(starts.min()?, ends.max()?))
}

/// The fragments an extension member *reference* lowers to, given the fragments
/// that produce its receiver.
///
/// Shared by the attribute form (`xs.second`, whose receiver is a source span)
/// and the unqualified form inside a trailing-lambda block (`second`, whose
/// receiver is the block's receiver parameter). Returns `None` when the member
/// needs `functools`, which the caller must then import.
pub(super) fn member_reference_fragments(
    info: &ty_python_semantic::ExtensionAttributeInfo,
    receiver: &[Fragment],
) -> (Vec<Fragment>, bool) {
    let mut fragments = Vec::new();
    let mut needs_functools = false;
    match info.kind {
        // a property is *read*, so the reference is the call itself
        ExtensionMemberKind::Property => {
            fragments.push(Fragment::Lit(format!("{}(", info.function)));
            fragments.extend_from_slice(receiver);
            fragments.push(Fragment::Lit(")".to_owned()));
        }
        // a class-level property reads like an instance one but its backing
        // function takes the class, so an instance receiver has to be widened
        ExtensionMemberKind::StaticProperty => {
            fragments.push(Fragment::Lit(format!("{}(", info.function)));
            if info.receiver_is_class {
                fragments.extend_from_slice(receiver);
            } else {
                fragments.push(Fragment::Lit("type(".to_owned()));
                fragments.extend_from_slice(receiver);
                fragments.push(Fragment::Lit(")".to_owned()));
            }
            fragments.push(Fragment::Lit(")".to_owned()));
        }
        // a `static def` takes no receiver at all
        ExtensionMemberKind::StaticMethod => {
            fragments.push(Fragment::Lit(info.function.clone()));
        }
        // an unapplied member reference: bind the receiver the way the bound
        // method would have
        ExtensionMemberKind::Method | ExtensionMemberKind::ClassMethod => {
            needs_functools = true;
            fragments.push(Fragment::Lit(format!(
                "functools.partial({}, ",
                info.function
            )));
            if info.kind == ExtensionMemberKind::ClassMethod && !info.receiver_is_class {
                fragments.push(Fragment::Lit("type(".to_owned()));
                fragments.extend_from_slice(receiver);
                fragments.push(Fragment::Lit(")".to_owned()));
            } else {
                fragments.extend_from_slice(receiver);
            }
            fragments.push(Fragment::Lit(")".to_owned()));
        }
    }
    (fragments, needs_functools)
}

/// rewrites attribute accesses that ty resolved to extension members
struct ExtensionCallLower<'a> {
    types: &'a dyn TypeInfo,
    edits: Vec<(TextRange, Vec<Fragment>)>,
    imports: BTreeSet<String>,
    needs_functools: bool,
    errors: Vec<String>,
    /// attribute nodes consumed by a whole-call rewrite, so the attribute
    /// visit doesn't rewrite them a second time
    handled: Vec<TextRange>,
}

impl<'a> ExtensionCallLower<'a> {
    fn new(types: &'a dyn TypeInfo) -> Self {
        Self {
            types,
            edits: Vec::new(),
            imports: BTreeSet::new(),
            needs_functools: false,
            errors: Vec::new(),
            handled: Vec::new(),
        }
    }

    fn note_import(&mut self, info: &ty_python_semantic::ExtensionAttributeInfo) {
        if let Some(module) = &info.import_from {
            self.imports
                .insert(format!("from {module} import {}", info.function));
        }
    }

    /// the receiver a backing function is handed: the receiver itself for
    /// methods, the class object for a `class def` (via `type(…)` when the
    /// access went through an instance)
    fn receiver_fragments(
        info: &ty_python_semantic::ExtensionAttributeInfo,
        receiver: &Expr,
        fragments: &mut Vec<Fragment>,
    ) {
        if info.kind == ExtensionMemberKind::ClassMethod && !info.receiver_is_class {
            fragments.push(Fragment::Lit("type(".to_owned()));
            fragments.push(Fragment::Src(receiver.range()));
            fragments.push(Fragment::Lit(")".to_owned()));
        } else {
            fragments.push(Fragment::Src(receiver.range()));
        }
    }
}

impl<'ast> Visitor<'ast> for ExtensionCallLower<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Call(call) => {
                if let Expr::Attribute(attr) = call.func.as_ref()
                    && attr.ctx.is_load()
                    && let Some(info) = self.types.extension_attribute_info(attr)
                    && !matches!(
                        info.kind,
                        ExtensionMemberKind::Property | ExtensionMemberKind::StaticProperty
                    )
                {
                    if attr.optional || spine_has_optional(&attr.value) {
                        self.errors.push(format!(
                            "extension member `{}` cannot be called through an \
                            optional chain yet",
                            attr.attr,
                        ));
                    } else {
                        self.handled.push(attr.range());
                        self.note_import(&info);
                        let mut fragments = vec![Fragment::Lit(format!("{}(", info.function))];
                        let receiver_counts = info.kind != ExtensionMemberKind::StaticMethod;
                        let mut written = false;
                        if receiver_counts {
                            Self::receiver_fragments(&info, &attr.value, &mut fragments);
                            written = true;
                        }
                        if let Some(span) = arguments_span(&call.arguments) {
                            if written {
                                fragments.push(Fragment::Lit(", ".to_owned()));
                            }
                            fragments.push(Fragment::Src(span));
                            written = true;
                        }
                        // this template replaces the whole call, so `context`
                        // parameters have to be filled here — the injection
                        // `context_params` anchors to the closing paren lands
                        // inside the range being replaced and is dropped
                        for (parameter, variable) in self.types.implicit_context_arguments(call) {
                            if written {
                                fragments.push(Fragment::Lit(", ".to_owned()));
                            }
                            fragments.push(Fragment::Lit(format!("{parameter}={variable}")));
                            written = true;
                        }
                        fragments.push(Fragment::Lit(")".to_owned()));
                        self.edits.push((call.range(), fragments));
                    }
                }
            }
            Expr::Attribute(attr) => {
                if attr.ctx.is_load()
                    && !self.handled.contains(&attr.range())
                    && let Some(info) = self.types.extension_attribute_info(attr)
                {
                    if attr.optional || spine_has_optional(&attr.value) {
                        self.errors.push(format!(
                            "extension member `{}` cannot be accessed through an \
                            optional chain yet",
                            attr.attr,
                        ));
                    } else {
                        self.note_import(&info);
                        let mut fragments = Vec::new();
                        match info.kind {
                            ExtensionMemberKind::Property => {
                                fragments.push(Fragment::Lit(format!("{}(", info.function)));
                                fragments.push(Fragment::Src(attr.value.range()));
                                fragments.push(Fragment::Lit(")".to_owned()));
                            }
                            // a class-level property reads like an instance one but
                            // its backing function takes the class, so an instance
                            // receiver has to be widened the way a `class def` is
                            ExtensionMemberKind::StaticProperty => {
                                fragments.push(Fragment::Lit(format!("{}(", info.function)));
                                if info.receiver_is_class {
                                    fragments.push(Fragment::Src(attr.value.range()));
                                } else {
                                    fragments.push(Fragment::Lit("type(".to_owned()));
                                    fragments.push(Fragment::Src(attr.value.range()));
                                    fragments.push(Fragment::Lit(")".to_owned()));
                                }
                                fragments.push(Fragment::Lit(")".to_owned()));
                            }
                            ExtensionMemberKind::StaticMethod => {
                                fragments.push(Fragment::Lit(info.function.clone()));
                            }
                            ExtensionMemberKind::Method | ExtensionMemberKind::ClassMethod => {
                                // an unapplied member reference: bind the
                                // receiver the way the bound method would have
                                self.needs_functools = true;
                                fragments.push(Fragment::Lit(format!(
                                    "functools.partial({}, ",
                                    info.function
                                )));
                                Self::receiver_fragments(&info, &attr.value, &mut fragments);
                                fragments.push(Fragment::Lit(")".to_owned()));
                            }
                        }
                        self.edits.push((attr.range(), fragments));
                    }
                }
            }
            Expr::UnaryOp(_) | Expr::BinOp(_) | Expr::Compare(_) => self.lower_operator(expr),
            _ => {}
        }
        walk_expr(self, expr);
    }
}

impl ExtensionCallLower<'_> {
    /// rewrite an operator whose dunder an extension supplies to the call it
    /// resolves to: `+text` → `_by_ext__str____pos__(text)`, `a in b` →
    /// `_by_ext__B____contains__(b, a)`
    fn lower_operator(&mut self, expr: &Expr) {
        let Some(rewrite) = self.types.extension_operator_info(expr) else {
            return;
        };
        let Some((left, right)) = operator_operands(expr) else {
            return;
        };
        self.note_import(&rewrite.info);
        let (receiver, argument) = match (rewrite.reflected, right) {
            (true, Some(right)) => (right, Some(left)),
            (true, None) => return,
            (false, right) => (left, right),
        };
        // a membership test is `bool(b.__contains__(a))` — python coerces the
        // dunder's result, and negates it for `not in`
        let wrapper = match expr {
            Expr::Compare(compare) => match compare.ops.as_ref() {
                [ast::CmpOp::In] => Some("bool("),
                [ast::CmpOp::NotIn] => Some("not bool("),
                _ => None,
            },
            _ => None,
        };
        let mut fragments = Vec::new();
        if let Some(wrapper) = wrapper {
            fragments.push(Fragment::Lit(wrapper.to_owned()));
        }
        fragments.push(Fragment::Lit(format!("{}(", rewrite.info.function)));
        fragments.push(Fragment::Src(receiver.range()));
        if let Some(argument) = argument {
            fragments.push(Fragment::Lit(", ".to_owned()));
            fragments.push(Fragment::Src(argument.range()));
        }
        fragments.push(Fragment::Lit(")".to_owned()));
        if wrapper.is_some() {
            fragments.push(Fragment::Lit(")".to_owned()));
        }
        self.edits.push((expr.range(), fragments));
    }
}

/// the operands of an operator expression the extension rewrite handles, as
/// `(left, right)`. `None` for a comparison chain, which the checker also
/// leaves alone — it is two calls joined by a short-circuit, a different rewrite
fn operator_operands(expr: &Expr) -> Option<(&Expr, Option<&Expr>)> {
    match expr {
        Expr::UnaryOp(unary) => Some((&unary.operand, None)),
        Expr::BinOp(binary) => Some((&binary.left, Some(&binary.right))),
        Expr::Compare(compare) => match compare.comparators.as_ref() {
            [right] => Some((&compare.left, Some(right))),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) struct ExtensionCallPass;

impl TypeAwarePass for ExtensionCallPass {
    fn run(&self, stmts: &[Stmt], types: &dyn TypeInfo, ctx: &mut PassContext) {
        let mut inner = ExtensionCallLower::new(types);
        for stmt in stmts {
            inner.visit_stmt(stmt);
        }
        ctx.errors.extend(inner.errors);
        if inner.edits.is_empty() {
            return;
        }
        if inner.needs_functools {
            ctx.required_imports.push("import functools".to_owned());
        }
        ctx.required_imports.extend(inner.imports);
        ctx.template_edits.extend(inner.edits);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn check(input: &str) -> String {
        transpile(input, &Config::test_default()).unwrap()
    }

    #[test]
    fn method_lowers_to_backing_function() {
        let out = check(
            "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs = [1, 2, 3]\nprint(xs.second())\n",
        );
        assert!(
            out.contains("def _by_ext__list__second(self):  # basedpython: extension method list"),
            "got:\n{out}"
        );
        assert!(out.contains("return self[1]"), "got:\n{out}");
        assert!(
            out.contains("print(_by_ext__list__second(xs))"),
            "got:\n{out}"
        );
        assert!(!out.contains("extension list"), "got:\n{out}");
    }

    #[test]
    fn backing_function_is_hoisted_above_an_earlier_call() {
        // a member called before the block's source position must still resolve
        // — the backing `def` is hoisted to the module top
        let out =
            check("\"asdf\".foo()\n\nextension str:\n    def foo(self):\n        print(\"hi\")\n");
        let def_at = out
            .find("def _by_ext__str__foo")
            .expect("backing def present");
        let call_at = out
            .find("_by_ext__str__foo(\"asdf\")")
            .expect("call rewritten");
        assert!(
            def_at < call_at,
            "backing def must precede its call, got:\n{out}"
        );
    }

    #[test]
    fn call_arguments_follow_the_receiver() {
        let out = check(
            "extension list:\n    def first_or(self, default: Element) -> Element:\n        return self[0] if self else default\n\nxs = [1]\nprint(xs.first_or(9))\n",
        );
        assert!(
            out.contains("print(_by_ext__list__first_or(xs, 9))"),
            "got:\n{out}"
        );
        // annotations are dropped from the backing function — its parameters
        // reference type variables with no runtime binding
        assert!(
            out.contains("def _by_ext__list__first_or(self, default):"),
            "got:\n{out}"
        );
    }

    #[test]
    fn property_access_drops_the_parentheses() {
        let out = check(
            "extension str:\n    @property\n    def shouty(self) -> str:\n        return self.upper()\n\nname = \"hi\"\nprint(name.shouty)\n",
        );
        assert!(
            out.contains("def _by_ext__str__shouty(self):  # basedpython: extension property str"),
            "got:\n{out}"
        );
        assert!(!out.contains("@property"), "got:\n{out}");
        assert!(
            out.contains("print(_by_ext__str__shouty(name))"),
            "got:\n{out}"
        );
    }

    /// an accessor block's body is synthesized, so it is rendered rather than passed
    /// through — a `get() = expr` accessor's `return` exists only in the AST
    #[test]
    fn accessor_block_property_keeps_its_return() {
        let out = check(
            "class A: ...\n\nextension A:\n    let size: int\n        get() = 1\n\nprint(A().size)\n",
        );
        assert!(
            out.contains("def _by_ext__A__size(self):  # basedpython: extension property A"),
            "got:\n{out}"
        );
        assert!(out.contains("    return 1"), "got:\n{out}");
        assert!(out.contains("print(_by_ext__A__size(A()))"), "got:\n{out}");
    }

    /// a `static let` member needs no descriptor in an extension: the access site is
    /// rewritten, so the class object is simply passed to the backing function
    #[test]
    fn static_property_passes_the_class() {
        let out = check(
            "class A: ...\n\nextension A:\n    static let kind: str\n        get() = \"a\"\n\nprint(A.kind)\nprint(A().kind)\n",
        );
        assert!(
            out.contains("def _by_ext__A__kind(cls):  # basedpython: extension staticproperty A"),
            "got:\n{out}"
        );
        assert!(out.contains("print(_by_ext__A__kind(A))"), "got:\n{out}");
        // an instance receiver is widened to its class, like a `class def` member
        assert!(
            out.contains("print(_by_ext__A__kind(type(A())))"),
            "got:\n{out}"
        );
        // the plain-class descriptor is not needed here and must not be emitted
        assert!(!out.contains("class _by_static_property"), "got:\n{out}");
    }

    #[test]
    fn conditional_extension_keeps_bounds_in_marker() {
        let out = check(
            "extension list[Element: int]:\n    def total(self) -> int:\n        return sum(self)\n\nxs = [1, 2]\nprint(xs.total())\n",
        );
        assert!(
            out.contains(
                "def _by_ext__list__total(self):  # basedpython: extension method list[Element: int]"
            ),
            "got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__list__total(xs))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn same_target_extensions_get_ordinals() {
        let out = check(
            "extension list[Element: int]:\n    def total(self) -> int:\n        return sum(self)\n\nextension list[Element: str]:\n    def total(self) -> str:\n        return \"\".join(self)\n\nints = [1, 2]\nwords = [\"a\"]\nprint(ints.total())\nprint(words.total())\n",
        );
        assert!(
            out.contains("def _by_ext__list__total(self):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("def _by_ext2__list__total(self):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__list__total(ints))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext2__list__total(words))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn unapplied_method_reference_becomes_partial() {
        let out = check(
            "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs = [1, 2]\nf = xs.second\nprint(f())\n",
        );
        assert!(
            out.contains("f = functools.partial(_by_ext__list__second, xs)"),
            "got:\n{out}"
        );
        assert!(out.contains("import functools"), "got:\n{out}");
    }

    #[test]
    fn nested_lowerings_compose_inside_member_bodies() {
        // the member body's own basedpython constructs still lower: the body
        // passes through as a source span with nested edits applied
        let out = check(
            "extension list:\n    def head(self) -> Element?:\n        return self[0] if self else None\n\nxs = [1]\nprint(xs.head())\n",
        );
        assert!(
            out.contains("def _by_ext__list__head(self):"),
            "got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__list__head(xs))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn extension_method_calling_another_extension_member() {
        let out = check(
            "extension list:\n    def second(self) -> Element:\n        return self[1]\n    def second_twice(self) -> Element:\n        return self.second()\n\nxs = [1, 2]\nprint(xs.second_twice())\n",
        );
        assert!(
            out.contains("return _by_ext__list__second(self)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn real_attributes_win_over_extensions() {
        let out = check(
            "class Widget:\n    def label(self) -> str:\n        return \"real\"\n\nextension Widget:\n    def label(self) -> int:\n        return 0\n\nw = Widget()\nprint(w.label())\n",
        );
        assert!(out.contains("print(w.label())"), "got:\n{out}");
    }

    #[test]
    fn static_and_class_members_bind_the_class_object() {
        let out = check(
            "extension str:\n    static def joined(parts: list[str]) -> str:\n        return \"-\".join(parts)\n    class def empty(cls) -> str:\n        return cls()\n\nprint(str.joined([\"a\", \"b\"]))\nprint(str.empty())\n",
        );
        assert!(
            out.contains("def _by_ext__str__joined(parts):  # basedpython: extension static str"),
            "got:\n{out}"
        );
        assert!(
            out.contains("def _by_ext__str__empty(cls):  # basedpython: extension classmethod str"),
            "got:\n{out}"
        );
        // a static member drops the receiver; a classmethod receives the class
        assert!(
            out.contains("print(_by_ext__str__joined([\"a\", \"b\"]))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("print(_by_ext__str__empty(str))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_call_inside_a_class_body_is_not_name_mangled() {
        // python mangles any `__name` reference in a class body, so the backing
        // function must carry a single leading underscore
        let out = check(
            "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nclass Holder:\n    value: int = [1, 2, 3].second()\n",
        );
        assert!(
            out.contains("value: int = _by_ext__list__second([1, 2, 3])"),
            "got:\n{out}"
        );
        assert!(!out.contains("__by_ext"), "got:\n{out}");
    }

    /// the call template replaces the whole call, so `context_params`' own
    /// injection (anchored to the closing paren) is inside the replaced range —
    /// the extension lowering has to fill the parameter itself, or the emitted
    /// call raises `TypeError` for the missing argument
    #[test]
    fn a_context_parameter_of_an_extension_member_is_filled() {
        let out = check(
            "extension str:\n    def foo(self, context val: int) -> None:\n        print(val)\n\ncontext a = 2\n\"asdf\".foo()\n",
        );
        assert!(
            out.contains("_by_ext__str__foo(\"asdf\", val=a)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_context_parameter_follows_the_explicit_arguments() {
        let out = check(
            "extension str:\n    def foo(self, n: int, context val: int) -> None:\n        print(val)\n\ncontext a = 2\n\"asdf\".foo(1)\n",
        );
        assert!(
            out.contains("_by_ext__str__foo(\"asdf\", 1, val=a)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_context_parameter_of_a_static_extension_member_is_filled() {
        let out = check(
            "class W: ...\n\nextension W:\n    static def make(context val: int) -> None:\n        print(val)\n\ncontext a = 2\nW.make()\n",
        );
        assert!(out.contains("_by_ext__W__make(val=a)"), "got:\n{out}");
    }

    #[test]
    fn a_unary_operator_lowers_to_its_backing_function() {
        let out = check(
            "extension str:\n    def __pos__(self) -> str:\n        return self\n\nprint(+\"asdf\")\n",
        );
        assert!(
            out.contains("print(_by_ext__str____pos__(\"asdf\"))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_binary_operator_lowers_to_its_backing_function() {
        let out = check(
            "class Money: ...\n\nextension Money:\n    def __add__(self, other: Money) -> Money:\n        return self\n\ndef f(a: Money, b: Money) -> Money:\n    return a + b\n",
        );
        assert!(
            out.contains("return _by_ext__Money____add__(a, b)"),
            "got:\n{out}"
        );
    }

    #[test]
    fn a_reflected_binary_operator_takes_the_right_operand_as_receiver() {
        let out = check(
            "class Money: ...\n\nextension Money:\n    def __radd__(self, other: int) -> Money:\n        return self\n\ndef f(a: Money) -> Money:\n    return 1 + a\n",
        );
        assert!(
            out.contains("return _by_ext__Money____radd__(a, 1)"),
            "got:\n{out}"
        );
    }

    /// python coerces `__contains__`'s result and negates it for `not in`, so
    /// the lowered call has to do the same
    #[test]
    fn a_membership_test_lowers_against_the_container() {
        let source = "class Wallet: ...\n\nextension Wallet:\n    def __contains__(self, n: int) -> int:\n        return n\n\ndef f(w: Wallet) -> bool:\n    return 1 in w\n\ndef g(w: Wallet) -> bool:\n    return 1 not in w\n";
        let out = check(source);
        assert!(
            out.contains("return bool(_by_ext__Wallet____contains__(w, 1))"),
            "got:\n{out}"
        );
        assert!(
            out.contains("return not bool(_by_ext__Wallet____contains__(w, 1))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn an_operator_the_operand_supports_is_untouched() {
        let out = check(
            "extension str:\n    def __add__(self, other: int) -> str:\n        return self\n\ndef f(s: str) -> str:\n    return s + s\n",
        );
        assert!(out.contains("return s + s"), "got:\n{out}");
    }

    /// a block puts its receiver's members in scope unqualified; an extension
    /// of the receiver's type supplies members too, and they lower to the
    /// backing function bound to the block's receiver
    #[test]
    fn an_unqualified_extension_member_lowers_inside_a_block() {
        let out = check(
            "class Tag: ...\n\nclass Doc:\n    def div(self, block: Tag.() -> None) -> None:\n        block(Tag())\n\nextension Tag:\n    def p(self, block: Tag.() -> None) -> None:\n        block(self)\n\ndef build(doc: Doc) -> None:\n    doc.div:\n        p:\n            print(1)\n",
        );
        assert!(
            out.contains("functools.partial(_by_ext__Tag__p, _by_self)"),
            "got:\n{out}"
        );
        assert!(out.contains("import functools"), "got:\n{out}");
    }

    #[test]
    fn an_unqualified_extension_property_lowers_to_a_call() {
        let out = check(
            "class Tag:\n    name: str\n\nclass Doc:\n    def div(self, block: Tag.() -> None) -> None:\n        block(Tag())\n\nextension Tag:\n    @property\n    def label(self) -> str:\n        return self.name\n\ndef build(doc: Doc) -> None:\n    doc.div:\n        print(label)\n",
        );
        assert!(
            out.contains("print(_by_ext__Tag__label(_by_self))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn optional_chain_receiver_is_rejected() {
        let err = transpile(
            "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs: list[int]? = [1, 2]\nprint(xs?.second())\n",
            &Config::test_default(),
        )
        .unwrap_err();
        assert!(err.contains("optional chain"), "got:\n{err}");
    }
}
