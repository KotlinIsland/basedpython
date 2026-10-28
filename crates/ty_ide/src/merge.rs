//! resolving a merge's conflicts by what the code means: what an editor's *resolve simple
//! conflicts* asks a language for
//!
//! a version control system merges line by line, and reports a conflict wherever both sides
//! changed the same lines, whether or not the changes have anything to do with each other:
//!
//! ```py
//! import os
//! <<<<<<< ours
//! import json
//! =======
//! import sys
//! >>>>>>> theirs
//! ```
//!
//! each conflict is read here as the statements, or the elements of a display, that the lines
//! of each of the three versions hold, located in that version's whole syntax tree, and is
//! resolved only where the lines are in the same suite or display in all three, and the two
//! sides' changes provably touch nothing in common:
//!
//! - imports that each side added or removed, which are united, and written as buff's isort
//!   configuration sorts them;
//! - functions and classes each side added, methods among them, under names neither side binds
//!   otherwise, which are kept at the place each was added;
//! - elements each side added to a list, set, tuple or dict display, or to a decorator list,
//!   which are kept at the place each was added, a dict's only under keys the other side does not
//!   add;
//! - two sides whose statements or elements are the same syntax tree, with the same comments,
//!   which differ only in how they are formatted, and which resolve to ours
//!
//! what both sides added first or last in one place is kept once. where both sides added
//! something else in one place, their order is one neither side chose, and only definitions
//! whose running binds their name and does nothing else are kept, ours first: a display's
//! elements are evaluated and kept in order, and decorators apply in order
//!
//! anything else is not resolved: a conflict that is not whole statements or whole elements, a
//! statement changed rather than added, and two sides binding one name differently

use std::ops::Range;

use ruff_db::system::{System, SystemPath};
use ruff_python_ast::comparable::{ComparableDecorator, ComparableExpr, ComparableStmt};
use ruff_python_ast::token::{TokenKind, parenthesized_range};
use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{
    self as ast, AnyNodeRef, ExprContext, ModModule, PySourceType, PythonVersion, Stmt,
};
use ruff_python_parser::{Parsed, parse_unchecked_source};
use ruff_python_stdlib::builtins::is_python_builtin;
use ruff_python_trivia::leading_indentation;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::FxHashSet;

use crate::Db;
use crate::conflict_markers::{ConflictSide, ConflictedText};
use crate::import_order::IsortConfig;
use crate::refactor::evaluation::is_pure;

/// the lines a conflict spans in each version, 0-based and end-exclusive
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictLines {
    pub base: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
}

/// the three versions a merge combines, each a whole file
#[derive(Debug, Clone, Copy)]
pub struct MergeTexts<'a> {
    pub base: &'a str,
    pub ours: &'a str,
    pub theirs: &'a str,
}

/// the text that resolves each of `conflicts`, in order, or `None` for one that is not
/// resolved, lines that end before they start or lie outside their version among them. the text
/// replaces the conflict's lines, and ends with a line ending wherever they did
///
/// `sort_imports` writes a run of import statements as the project's isort configuration sorts
/// them, and answers `None` for anything else
pub fn resolve_merge_conflicts(
    texts: MergeTexts<'_>,
    conflicts: &[ConflictLines],
    source_type: PySourceType,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> Vec<Option<String>> {
    let base = Version::new(texts.base, source_type);
    let ours = Version::new(texts.ours, source_type);
    let theirs = Version::new(texts.theirs, source_type);
    conflicts
        .iter()
        .map(|conflict| {
            let (Some(base), Some(ours), Some(theirs)) = (
                base.as_ref().and_then(|base| base.region(&conflict.base)),
                ours.as_ref().and_then(|ours| ours.region(&conflict.ours)),
                theirs
                    .as_ref()
                    .and_then(|theirs| theirs.region(&conflict.theirs)),
            ) else {
                return None;
            };
            resolve(&base, &ours, &theirs, sort_imports)
        })
        .collect()
}

/// [`resolve_merge_conflicts`] for the file at `path` in `db`'s project, whose extension says
/// how it is parsed, and whose imports are sorted with the isort configuration buff reads for it:
/// a project file's, or with no `path` the project root directory's
pub fn resolve_file_merge_conflicts(
    db: &dyn Db,
    path: Option<&SystemPath>,
    texts: MergeTexts<'_>,
    conflicts: &[ConflictLines],
) -> Vec<Option<String>> {
    let root = db.project().root(db);
    let source_type = path
        .and_then(|path| path.extension())
        .and_then(PySourceType::try_from_extension)
        .unwrap_or_default();
    let isort = match path {
        Some(path) => IsortConfig::for_file(db.system(), path.as_std_path(), root.as_std_path()),
        None => IsortConfig::for_directory(db.system(), root.as_std_path(), root.as_std_path()),
    };
    resolve_merge_conflicts(texts, conflicts, source_type, &|source| {
        isort.sort(source, source_type)
    })
}

/// one version of the file, parsed
struct Version<'a> {
    text: &'a str,
    parsed: Parsed<ModModule>,
    lines: LineIndex,
}

impl<'a> Version<'a> {
    /// `None` for a version that does not parse: what its lines hold cannot be told
    fn new(text: &'a str, source_type: PySourceType) -> Option<Self> {
        let parsed = parse_unchecked_source(text, source_type);
        if parsed.has_syntax_errors() {
            return None;
        }
        Some(Self {
            text,
            parsed,
            lines: LineIndex::from_source_text(text),
        })
    }

    /// where line `line` starts, or the end of the text for the line after the last
    fn line_start(&self, line: usize) -> Option<TextSize> {
        match self.lines.line_starts().get(line) {
            Some(start) => Some(*start),
            None if line == self.lines.line_starts().len() => Some(TextSize::of(self.text)),
            None => None,
        }
    }

    /// where the line after the one `offset` is on starts: the end of `offset`'s line, its line
    /// ending included
    fn next_line_start(&self, offset: TextSize) -> TextSize {
        let line = self.lines.line_index(offset).to_zero_indexed();
        self.line_start(line + 1).unwrap_or(TextSize::of(self.text))
    }

    /// whether `offset` is not inside a token, as the start of a line in a multi-line string is
    fn between_tokens(&self, offset: TextSize) -> bool {
        let tokens = self.parsed.tokens();
        let after = tokens.partition_point(|token| token.end() <= offset);
        tokens
            .get(after)
            .is_none_or(|token| offset <= token.start())
    }

    fn module(&self) -> &[Stmt] {
        self.parsed.suite()
    }

    fn region(&self, lines: &Range<usize>) -> Option<Region<'_>> {
        if lines.start > lines.end {
            return None;
        }
        let range = TextRange::new(self.line_start(lines.start)?, self.line_start(lines.end)?);
        if !self.between_tokens(range.start()) || !self.between_tokens(range.end()) {
            return None;
        }
        let module = Location {
            path: Vec::new(),
            scope: Scope::Module(self.module()),
        };
        let (held, locations) = in_suite(self.module(), range, module)?;
        let comments = self
            .parsed
            .tokens()
            .in_range(range)
            .iter()
            .filter(|token| token.kind() == TokenKind::Comment)
            .map(|token| self.text[token.range()].trim().to_string())
            .collect();
        Some(Region {
            version: self,
            range,
            held,
            comments,
            locations,
        })
    }
}

/// what the lines of a conflict hold in one version
struct Region<'a> {
    version: &'a Version<'a>,
    range: TextRange,
    held: Held<'a>,
    comments: Vec<String>,
    /// where the lines are. lines holding no statement, after the last statement of a function
    /// or class, are at the end of its body as much as after it, and have a location for each
    /// body they end
    locations: Vec<Location<'a>>,
}

enum Held<'a> {
    /// whole statements of a suite
    Statements { statements: Vec<&'a Stmt> },
    /// whole elements of a display or a decorator list
    Elements {
        container: Container<'a>,
        elements: Vec<Element<'a>>,
    },
}

/// the suite or display a conflict's lines are in: the path to it from the module, and the
/// scope its names are bound in
#[derive(Clone)]
struct Location<'a> {
    path: Vec<Step<'a>>,
    scope: Scope<'a>,
}

impl<'a> Location<'a> {
    fn then(&self, steps: impl IntoIterator<Item = Step<'a>>) -> Self {
        let mut path = self.path.clone();
        path.extend(steps);
        Self {
            path,
            scope: self.scope,
        }
    }

    /// whether two versions' locations are the same place: the same steps, from the module down
    fn same_place(&self, other: &Self) -> bool {
        self.path.len() == other.path.len()
            && self.path.iter().zip(&other.path).all(|(a, b)| a.same(*b))
    }
}

/// one step from a suite or a display towards the lines of a conflict
#[derive(Clone, Copy)]
enum Step<'a> {
    /// into a statement of the suite, told apart from the others by what it binds or assigns
    /// and by how many statements before it in the suite do the same
    Statement { statement: &'a Stmt, ordinal: usize },
    /// into the body of the function or class entered
    Body,
    /// into the decorators of the function or class entered
    Decorators,
    /// into the display that is the value of the statement entered
    Value,
    /// into the display that is, or is the value of, the `index`th element of the display
    /// entered
    Element(usize),
}

impl Step<'_> {
    fn same(self, other: Self) -> bool {
        match (self, other) {
            (
                Step::Statement {
                    statement: a,
                    ordinal: a_ordinal,
                },
                Step::Statement {
                    statement: b,
                    ordinal: b_ordinal,
                },
            ) => {
                a_ordinal == b_ordinal
                    && statement_key(a).is_some_and(|a| statement_key(b) == Some(a))
            }
            (Step::Body, Step::Body)
            | (Step::Decorators, Step::Decorators)
            | (Step::Value, Step::Value) => true,
            (Step::Element(a), Step::Element(b)) => a == b,
            _ => false,
        }
    }
}

/// what tells a statement holding a conflict apart from the others of its suite
#[derive(PartialEq)]
enum StatementKey<'a> {
    Function(&'a str),
    Class(&'a str),
    Assign(Vec<ComparableExpr<'a>>),
    AnnAssign(ComparableExpr<'a>),
    AugAssign(ComparableExpr<'a>, ast::Operator),
    Expr,
    Return,
}

/// `None` for a statement whose insides a conflict is not looked for in
fn statement_key(statement: &Stmt) -> Option<StatementKey<'_>> {
    Some(match statement {
        Stmt::FunctionDef(function) => StatementKey::Function(function.name.as_str()),
        Stmt::ClassDef(class) => StatementKey::Class(class.name.as_str()),
        Stmt::Assign(assign) => {
            StatementKey::Assign(assign.targets.iter().map(ComparableExpr::from).collect())
        }
        Stmt::AnnAssign(assign) => StatementKey::AnnAssign(ComparableExpr::from(&*assign.target)),
        Stmt::AugAssign(assign) => {
            StatementKey::AugAssign(ComparableExpr::from(&*assign.target), assign.op)
        }
        Stmt::Expr(_) => StatementKey::Expr,
        Stmt::Return(_) => StatementKey::Return,
        _ => return None,
    })
}

fn statement_step(suite: &[Stmt], index: usize) -> Option<Step<'_>> {
    let statement = &suite[index];
    let key = statement_key(statement)?;
    let ordinal = suite[..index]
        .iter()
        .filter(|earlier| statement_key(earlier).as_ref() == Some(&key))
        .count();
    Some(Step::Statement { statement, ordinal })
}

/// a scope the names of a suite are bound in
#[derive(Clone, Copy)]
enum Scope<'a> {
    Module(&'a [Stmt]),
    Class(&'a ast::StmtClassDef),
    Function(&'a ast::StmtFunctionDef),
}

impl<'a> Scope<'a> {
    fn statements(self) -> &'a [Stmt] {
        match self {
            Scope::Module(suite) => suite,
            Scope::Class(class) => &class.body,
            Scope::Function(function) => &function.body,
        }
    }
}

#[derive(Clone, Copy)]
enum Container<'a> {
    List(&'a ast::ExprList),
    Set(&'a ast::ExprSet),
    Tuple(&'a ast::ExprTuple),
    Dict(&'a ast::ExprDict),
    Decorators,
}

impl<'a> Container<'a> {
    fn same_kind(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Container::List(_), Container::List(_))
                | (Container::Set(_), Container::Set(_))
                | (Container::Tuple(_), Container::Tuple(_))
                | (Container::Dict(_), Container::Dict(_))
                | (Container::Decorators, Container::Decorators)
        )
    }

    /// the display, which a decorator list is not
    fn display(self) -> Option<AnyNodeRef<'a>> {
        match self {
            Container::List(list) => Some(list.into()),
            Container::Set(set) => Some(set.into()),
            Container::Tuple(tuple) => Some(tuple.into()),
            Container::Dict(dict) => Some(dict.into()),
            Container::Decorators => None,
        }
    }
}

#[derive(Clone, Copy)]
enum Element<'a> {
    Expr(&'a ast::Expr),
    DictItem(&'a ast::DictItem),
    Decorator(&'a ast::Decorator),
}

impl<'a> Element<'a> {
    fn range(self) -> TextRange {
        match self {
            Element::Expr(expr) => expr.range(),
            Element::DictItem(item) => match &item.key {
                Some(key) => TextRange::new(key.start(), item.value.end()),
                None => item.value.range(),
            },
            Element::Decorator(decorator) => decorator.range(),
        }
    }

    fn same(self, other: Self) -> bool {
        match (self, other) {
            (Element::Expr(a), Element::Expr(b)) => {
                ComparableExpr::from(a) == ComparableExpr::from(b)
            }
            (Element::DictItem(a), Element::DictItem(b)) => {
                a.key.as_ref().map(ComparableExpr::from) == b.key.as_ref().map(ComparableExpr::from)
                    && ComparableExpr::from(&a.value) == ComparableExpr::from(&b.value)
            }
            (Element::Decorator(a), Element::Decorator(b)) => {
                ComparableDecorator::from(a) == ComparableDecorator::from(b)
            }
            _ => false,
        }
    }

    /// the key a dict item binds, which two sides must not both add
    fn key(self) -> Option<ComparableExpr<'a>> {
        match self {
            Element::DictItem(item) => item.key.as_ref().map(ComparableExpr::from),
            _ => None,
        }
    }

    /// what evaluating the element binds where the display is
    fn names(self) -> Names<'a> {
        let mut names = Names::default();
        match self {
            Element::Expr(expr) => names.visit_expr(expr),
            Element::DictItem(item) => {
                if let Some(key) = &item.key {
                    names.visit_expr(key);
                }
                names.visit_expr(&item.value);
            }
            Element::Decorator(decorator) => names.visit_decorator(decorator),
        }
        names
    }

    /// whether a comma follows the element, and any parentheses around it, in `version`
    fn has_trailing_comma(self, version: &Version, container: Container) -> bool {
        let (expr, parent) = match (self, container.display()) {
            (Element::Expr(expr), Some(display)) => (expr, display),
            (Element::DictItem(item), Some(display)) => (&item.value, display),
            _ => return false,
        };
        let tokens = version.parsed.tokens();
        let end =
            parenthesized_range(expr.into(), parent, tokens).map_or(expr.end(), TextRange::end);
        tokens
            .after(end)
            .iter()
            .find(|token| !token.kind().is_trivia())
            .is_some_and(|token| token.kind() == TokenKind::Comma)
    }
}

/// whether a statement or element shares text with a range of whole lines, or, for an empty range,
/// holds the place between two lines it stands for
fn overlaps(statement: TextRange, range: TextRange) -> bool {
    statement.start() < range.end() && range.start() < statement.end()
        || (range.is_empty()
            && statement.start() < range.start()
            && range.start() < statement.end())
}

/// what `range` holds in `suite`, which is at `at`: some of its statements, all of each, or
/// something inside the one statement it is in. with where that is
fn in_suite<'a>(
    suite: &'a [Stmt],
    range: TextRange,
    at: Location<'a>,
) -> Option<(Held<'a>, Vec<Location<'a>>)> {
    let overlapping: Vec<usize> = (0..suite.len())
        .filter(|index| overlaps(suite[*index].range(), range))
        .collect();
    if overlapping
        .iter()
        .all(|index| range.contains_range(suite[*index].range()))
    {
        let mut locations = Vec::new();
        if overlapping.is_empty() {
            ended_bodies(suite, range, &at, &mut locations);
        }
        locations.insert(0, at);
        let statements = overlapping.iter().map(|index| &suite[*index]).collect();
        return Some((Held::Statements { statements }, locations));
    }
    match overlapping.as_slice() {
        [index] => in_statement(suite, *index, range, &at),
        _ => None,
    }
}

/// the bodies that `range`, holding no statement of `suite`, comes right after the end of: the
/// body of the function or class it follows, then of the last statement of that body, and so on
fn ended_bodies<'a>(
    suite: &'a [Stmt],
    range: TextRange,
    at: &Location<'a>,
    locations: &mut Vec<Location<'a>>,
) {
    let Some(index) = suite
        .iter()
        .rposition(|statement| statement.end() <= range.start())
    else {
        return;
    };
    let (scope, body) = match &suite[index] {
        Stmt::FunctionDef(function) => (Scope::Function(function), &function.body),
        Stmt::ClassDef(class) => (Scope::Class(class), &class.body),
        _ => return,
    };
    let Some(step) = statement_step(suite, index) else {
        return;
    };
    let mut location = at.then([step, Step::Body]);
    location.scope = scope;
    ended_bodies(body, range, &location, locations);
    locations.insert(0, location);
}

fn in_statement<'a>(
    suite: &'a [Stmt],
    index: usize,
    range: TextRange,
    at: &Location<'a>,
) -> Option<(Held<'a>, Vec<Location<'a>>)> {
    let step = statement_step(suite, index)?;
    let (decorators, name, header_end, body, scope) = match &suite[index] {
        Stmt::FunctionDef(function) => (
            &function.decorator_list,
            &function.name,
            function
                .returns
                .as_deref()
                .map_or(function.parameters.end(), Ranged::end),
            &function.body,
            Scope::Function(function),
        ),
        Stmt::ClassDef(class) => (
            &class.decorator_list,
            &class.name,
            class
                .arguments
                .as_deref()
                .map_or(class.name.end(), Ranged::end),
            &class.body,
            Scope::Class(class),
        ),
        Stmt::Assign(assign) => return in_expr(&assign.value, range, at.then([step, Step::Value])),
        Stmt::AnnAssign(assign) => {
            return in_expr(
                assign.value.as_deref()?,
                range,
                at.then([step, Step::Value]),
            );
        }
        Stmt::AugAssign(assign) => {
            return in_expr(&assign.value, range, at.then([step, Step::Value]));
        }
        Stmt::Expr(expr) => return in_expr(&expr.value, range, at.then([step, Step::Value])),
        Stmt::Return(ret) => {
            return in_expr(ret.value.as_deref()?, range, at.then([step, Step::Value]));
        }
        _ => return None,
    };
    if !decorators.is_empty() && range.end() <= name.start() {
        // the lines above the header hold decorators, all of each
        let overlapping: Vec<Element> = decorators
            .iter()
            .filter(|decorator| overlaps(decorator.range(), range))
            .map(Element::Decorator)
            .collect();
        if !overlapping
            .iter()
            .all(|decorator| range.contains_range(decorator.range()))
        {
            return None;
        }
        let held = Held::Elements {
            container: Container::Decorators,
            elements: overlapping,
        };
        return Some((held, vec![at.then([step, Step::Decorators])]));
    }
    // the lines after the header hold statements of the body
    if range.start() > header_end {
        let mut location = at.then([step, Step::Body]);
        location.scope = scope;
        return in_suite(body, range, location);
    }
    None
}

fn in_expr<'a>(
    expr: &'a ast::Expr,
    range: TextRange,
    at: Location<'a>,
) -> Option<(Held<'a>, Vec<Location<'a>>)> {
    // strictly between the brackets
    if !(expr.start() < range.start() && range.end() < expr.end()) {
        return None;
    }
    let (container, elements): (Container, Vec<Element>) = match expr {
        ast::Expr::List(list) => (
            Container::List(list),
            list.elts.iter().map(Element::Expr).collect(),
        ),
        ast::Expr::Set(set) => (
            Container::Set(set),
            set.elts.iter().map(Element::Expr).collect(),
        ),
        ast::Expr::Tuple(tuple) if tuple.parenthesized => (
            Container::Tuple(tuple),
            tuple.elts.iter().map(Element::Expr).collect(),
        ),
        ast::Expr::Dict(dict) => (
            Container::Dict(dict),
            dict.items.iter().map(Element::DictItem).collect(),
        ),
        _ => return None,
    };
    let overlapping: Vec<(usize, Element)> = elements
        .into_iter()
        .enumerate()
        .filter(|(_, element)| overlaps(element.range(), range))
        .collect();
    if overlapping
        .iter()
        .all(|(_, element)| range.contains_range(element.range()))
    {
        let held = Held::Elements {
            container,
            elements: overlapping
                .into_iter()
                .map(|(_, element)| element)
                .collect(),
        };
        return Some((held, vec![at]));
    }
    match overlapping.as_slice() {
        [(index, Element::Expr(inner))] => in_expr(inner, range, at.then([Step::Element(*index)])),
        [(index, Element::DictItem(item))] => {
            in_expr(&item.value, range, at.then([Step::Element(*index)]))
        }
        _ => None,
    }
}

/// the location the three regions share, in each version, or `None` where the lines of the
/// conflict are in different suites or displays in different versions
fn shared_location<'r, 'a>(
    base: &'r Region<'a>,
    ours: &'r Region<'a>,
    theirs: &'r Region<'a>,
) -> Option<[&'r Location<'a>; 3]> {
    ours.locations.iter().find_map(|ours_location| {
        let at = |region: &'r Region<'a>| {
            region
                .locations
                .iter()
                .find(|location| location.same_place(ours_location))
        };
        Some([at(base)?, ours_location, at(theirs)?])
    })
}

fn resolve(
    base: &Region,
    ours: &Region,
    theirs: &Region,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let [_, ours_location, theirs_location] = shared_location(base, ours, theirs)?;
    let sides = Sides {
        ours: Side {
            region: ours,
            scope: ours_location.scope,
        },
        theirs: Side {
            region: theirs,
            scope: theirs_location.scope,
        },
    };
    match (&base.held, &ours.held, &theirs.held) {
        (
            Held::Statements { statements: b },
            Held::Statements { statements: o },
            Held::Statements { statements: t },
        ) => {
            if same_statements(o, t) && ours.comments == theirs.comments {
                return Some(ours.text().to_string());
            }
            let all_imports = |statements: &[&Stmt]| {
                statements
                    .iter()
                    .all(|statement| statement.is_import_stmt() || statement.is_import_from_stmt())
            };
            if all_imports(b) && all_imports(o) && all_imports(t) {
                return merge_imports(b, o, t, &sides, sort_imports);
            }
            merge_definitions(b, o, t, &sides)
        }
        (
            Held::Elements {
                container: base_container,
                elements: b,
            },
            Held::Elements {
                container: ours_container,
                elements: o,
            },
            Held::Elements {
                container: theirs_container,
                elements: t,
            },
        ) if base_container.same_kind(*ours_container)
            && ours_container.same_kind(*theirs_container) =>
        {
            if o.len() == t.len()
                && o.iter().zip(t).all(|(a, b)| a.same(*b))
                && ours.comments == theirs.comments
            {
                return Some(ours.text().to_string());
            }
            merge_elements(b, o, t, &sides, *ours_container, *theirs_container)
        }
        _ => None,
    }
}

/// one side's region, and the scope the names it binds are bound in
struct Side<'r, 'a> {
    region: &'r Region<'a>,
    scope: Scope<'a>,
}

struct Sides<'r, 'a> {
    ours: Side<'r, 'a>,
    theirs: Side<'r, 'a>,
}

impl<'a> Side<'_, 'a> {
    /// what the side binds in its scope, leaving out the statements in `excluded`
    fn bound(&self, excluded: &[&Stmt]) -> Bound<'a> {
        let module = self.region.version.module();
        let statements = self.scope.statements().iter().filter(|statement| {
            !excluded
                .iter()
                .any(|excluded| std::ptr::eq(*excluded, *statement))
        });
        let names = Names::of_statements(statements);
        let mut bound: FxHashSet<&str> = names.binding_in(self.scope).collect();
        let mut attributes = FxHashSet::default();
        match self.scope {
            Scope::Function(function) => {
                bound.extend(
                    function
                        .parameters
                        .iter()
                        .map(|parameter| parameter.name().as_str()),
                );
            }
            Scope::Class(_) => {
                // an instance's attribute shadows the class's, and can be set from anywhere
                let mut stores = AttributeStores::default();
                stores.visit_body(module);
                attributes = stores.attributes;
            }
            Scope::Module(_) => {}
        }
        Bound {
            names: bound,
            attributes,
            star: names.star,
        }
    }
}

impl Region<'_> {
    fn text(&self) -> &str {
        &self.version.text[self.range]
    }
}

fn same_statements(a: &[&Stmt], b: &[&Stmt]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(a, b)| ComparableStmt::from(*a) == ComparableStmt::from(*b))
}

// ── names ───────────────────────────────────────────────────────────────────────────────────────

/// the names some statements or expressions bind in the scope they are in. a function, class,
/// lambda or comprehension nested in them is a scope of its own, whose names are not among them,
/// but a name a nested function declares `global` or `nonlocal` binds in an enclosing scope
#[derive(Default)]
struct Names<'a> {
    bound: Vec<&'a str>,
    /// what nested functions declare `global`, which binds in the module
    global: Vec<&'a str>,
    /// what nested functions declare `nonlocal`, which binds in an enclosing function
    nonlocal: Vec<&'a str>,
    /// whether a star import binds names the source does not tell
    star: bool,
}

impl<'a> Names<'a> {
    fn of_statements(statements: impl IntoIterator<Item = &'a Stmt>) -> Self {
        let mut names = Self::default();
        for statement in statements {
            names.visit_stmt(statement);
        }
        names
    }

    /// the names bound in a scope like `scope`
    fn binding_in(&self, scope: Scope) -> impl Iterator<Item = &'a str> {
        let (global, nonlocal) = match scope {
            Scope::Module(_) => (self.global.as_slice(), [].as_slice()),
            Scope::Class(_) => ([].as_slice(), [].as_slice()),
            Scope::Function(_) => ([].as_slice(), self.nonlocal.as_slice()),
        };
        self.bound.iter().chain(global).chain(nonlocal).copied()
    }

    /// the `global` and `nonlocal` declarations in a nested scope's body, at any depth
    fn declarations_in(&mut self, body: &'a [Stmt]) {
        let mut declarations = Declarations::default();
        declarations.visit_body(body);
        self.global.extend(declarations.global);
        self.nonlocal.extend(declarations.nonlocal);
    }
}

impl<'a> Visitor<'a> for Names<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                self.bound.push(function.name.as_str());
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
                self.visit_parameters(&function.parameters);
                if let Some(returns) = &function.returns {
                    self.visit_annotation(returns);
                }
                self.declarations_in(&function.body);
            }
            Stmt::ClassDef(class) => {
                self.bound.push(class.name.as_str());
                for decorator in &class.decorator_list {
                    self.visit_decorator(decorator);
                }
                if let Some(arguments) = &class.arguments {
                    self.visit_arguments(arguments);
                }
                self.declarations_in(&class.body);
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                    // `import a.b` binds `a`
                    self.bound.push(bound.split('.').next().unwrap_or(bound));
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.asname.is_none() && &alias.name == "*" {
                        self.star = true;
                    } else {
                        self.bound
                            .push(alias.asname.as_ref().unwrap_or(&alias.name).as_str());
                    }
                }
            }
            Stmt::Global(global) => self
                .bound
                .extend(global.names.iter().map(ast::Identifier::as_str)),
            Stmt::Nonlocal(nonlocal) => {
                self.bound
                    .extend(nonlocal.names.iter().map(ast::Identifier::as_str));
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        match expr {
            ast::Expr::Name(name) => {
                if matches!(name.ctx, ExprContext::Store | ExprContext::Del) {
                    self.bound.push(name.id.as_str());
                }
            }
            // only the defaults are evaluated where the lambda is
            ast::Expr::Lambda(lambda) => {
                if let Some(parameters) = &lambda.parameters {
                    self.visit_parameters(parameters);
                }
            }
            _ => visitor::walk_expr(self, expr),
        }
    }

    /// a comprehension's targets are its own; an assignment expression in it binds where the
    /// comprehension is
    fn visit_comprehension(&mut self, comprehension: &'a ast::Comprehension) {
        self.visit_expr(&comprehension.iter);
        for condition in &comprehension.ifs {
            self.visit_expr(condition);
        }
    }

    fn visit_except_handler(&mut self, except_handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(handler) = except_handler;
        if let Some(name) = &handler.name {
            self.bound.push(name.as_str());
        }
        visitor::walk_except_handler(self, except_handler);
    }

    fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
        let name = match pattern {
            ast::Pattern::MatchAs(pattern) => pattern.name.as_ref(),
            ast::Pattern::MatchStar(pattern) => pattern.name.as_ref(),
            ast::Pattern::MatchMapping(pattern) => pattern.rest.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            self.bound.push(name.as_str());
        }
        visitor::walk_pattern(self, pattern);
    }
}

/// the names declared `global` or `nonlocal` anywhere in some statements
#[derive(Default)]
struct Declarations<'a> {
    global: Vec<&'a str>,
    nonlocal: Vec<&'a str>,
}

impl<'a> Visitor<'a> for Declarations<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::Global(global) => self
                .global
                .extend(global.names.iter().map(ast::Identifier::as_str)),
            Stmt::Nonlocal(nonlocal) => {
                self.nonlocal
                    .extend(nonlocal.names.iter().map(ast::Identifier::as_str));
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }
}

/// the attributes some statements assign or delete, of whatever object
#[derive(Default)]
struct AttributeStores<'a> {
    attributes: FxHashSet<&'a str>,
}

impl<'a> Visitor<'a> for AttributeStores<'a> {
    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        if let ast::Expr::Attribute(attribute) = expr
            && matches!(attribute.ctx, ExprContext::Store | ExprContext::Del)
        {
            self.attributes.insert(attribute.attr.as_str());
        }
        visitor::walk_expr(self, expr);
    }
}

/// what one side binds in a scope
struct Bound<'a> {
    names: FxHashSet<&'a str>,
    /// for a class's scope, the attributes the side assigns anywhere, which an instance's
    /// attribute of that name would shadow the class's with
    attributes: FxHashSet<&'a str>,
    star: bool,
}

impl Bound<'_> {
    fn binds(&self, name: &str) -> bool {
        self.star || self.names.contains(name) || self.attributes.contains(name)
    }
}

// ── imports ─────────────────────────────────────────────────────────────────────────────────────

/// one name an import statement binds
#[derive(Debug, Clone, PartialEq, Eq)]
struct Imported {
    /// the module a `from` import imports from, with its leading dots, or `None` for `import`
    from: Option<String>,
    name: String,
    asname: Option<String>,
}

impl Imported {
    /// the name the import binds in the module
    fn binds(&self) -> &str {
        match (&self.asname, &self.from) {
            (Some(asname), _) => asname,
            (None, Some(_)) => &self.name,
            // `import a.b` binds `a`
            (None, None) => self.name.split('.').next().unwrap_or(&self.name),
        }
    }

    /// what the bound name is bound to: two imports binding one name to one thing do not
    /// conflict, as `import a.b` and `import a.c` both bind the package `a`
    fn bound_to(&self) -> (Option<&str>, &str) {
        match (&self.from, &self.asname) {
            (Some(from), _) => (Some(from), &self.name),
            (None, Some(_)) => (None, &self.name),
            (None, None) => (None, self.binds()),
        }
    }

    fn statement(&self) -> String {
        let alias = self
            .asname
            .as_ref()
            .map(|asname| format!(" as {asname}"))
            .unwrap_or_default();
        match &self.from {
            Some(from) => format!("from {from} import {}{alias}", self.name),
            None => format!("import {}{alias}", self.name),
        }
    }
}

fn imported(statements: &[&Stmt]) -> Vec<Imported> {
    let mut found = Vec::new();
    for statement in statements {
        match statement {
            Stmt::Import(import) => {
                for alias in &import.names {
                    found.push(Imported {
                        from: None,
                        name: alias.name.to_string(),
                        asname: alias.asname.as_ref().map(ToString::to_string),
                    });
                }
            }
            Stmt::ImportFrom(import) => {
                let module = format!(
                    "{}{}",
                    ".".repeat(import.level as usize),
                    import.module.as_deref().unwrap_or_default()
                );
                for alias in &import.names {
                    found.push(Imported {
                        from: Some(module.clone()),
                        name: alias.name.to_string(),
                        asname: alias.asname.as_ref().map(ToString::to_string),
                    });
                }
            }
            _ => {}
        }
    }
    found
}

/// every import either side added, without those either side removed, sorted. refused where both
/// sides changed what one name is bound to, differently, where the result binds one name to two
/// things, where a side adds an import of a name the other binds elsewhere, and where a star
/// import binds names nothing here can tell
fn merge_imports(
    base: &[&Stmt],
    ours: &[&Stmt],
    theirs: &[&Stmt],
    sides: &Sides,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let (ours_region, theirs_region) = (sides.ours.region, sides.theirs.region);
    // a comment beside an import goes where isort puts the import, which this does not decide
    if !ours_region.comments.is_empty() || !theirs_region.comments.is_empty() {
        return None;
    }
    let (b, o, t) = (imported(base), imported(ours), imported(theirs));
    if o.iter().chain(&t).any(|import| import.name == "*") {
        return None;
    }
    let changed = |side: &[Imported]| -> Vec<String> {
        side.iter()
            .filter(|import| !b.contains(import))
            .chain(b.iter().filter(|import| !side.contains(import)))
            .map(|import| import.binds().to_string())
            .collect()
    };
    let (ours_changed, theirs_changed) = (changed(&o), changed(&t));
    for name in ours_changed
        .iter()
        .filter(|name| theirs_changed.contains(name))
    {
        let bound_to = |side: &'_ [Imported]| {
            let mut bound_to: Vec<(Option<String>, String)> = side
                .iter()
                .filter(|import| import.binds() == name)
                .map(|import| {
                    let (from, name) = import.bound_to();
                    (from.map(str::to_string), name.to_string())
                })
                .collect();
            bound_to.sort();
            bound_to.dedup();
            bound_to
        };
        if bound_to(&o) != bound_to(&t) {
            return None;
        }
    }
    // a name one side imports that the other binds outside the conflict is bound twice, unless
    // by another import to the same thing
    for (side, other, other_statements) in [(&o, &sides.theirs, theirs), (&t, &sides.ours, ours)] {
        let imports: Vec<&Stmt> = other
            .scope
            .statements()
            .iter()
            .filter(|statement| {
                (statement.is_import_stmt() || statement.is_import_from_stmt())
                    && !other_statements
                        .iter()
                        .any(|held| std::ptr::eq(*held, *statement))
            })
            .collect();
        let outside = imported(&imports);
        if outside.iter().any(|import| import.name == "*") {
            return None;
        }
        let excluded: Vec<&Stmt> = other_statements.iter().chain(&imports).copied().collect();
        let bound = other.bound(&excluded);
        if side
            .iter()
            .filter(|import| !b.contains(import))
            .any(|import| {
                bound.binds(import.binds())
                    || outside.iter().any(|outside| {
                        outside.binds() == import.binds() && outside.bound_to() != import.bound_to()
                    })
            })
        {
            return None;
        }
    }
    let merged: Vec<&Imported> = o
        .iter()
        .filter(|import| t.contains(import) || !b.contains(import))
        .chain(
            t.iter()
                .filter(|import| !o.contains(import) && !b.contains(import)),
        )
        .collect();
    for (index, import) in merged.iter().enumerate() {
        if merged[..index].iter().any(|earlier| {
            earlier.binds() == import.binds() && earlier.bound_to() != import.bound_to()
        }) {
            return None;
        }
    }

    let tail = ours_region.tail();
    if merged.is_empty() {
        return Some(tail.to_string());
    }
    let indentation =
        region_indentation(ours_region).or_else(|| region_indentation(theirs_region))?;
    let statements: String = merged
        .iter()
        .map(|import| import.statement() + "\n")
        .collect();
    let sorted = sort_imports(&statements)?;
    let mut out = String::new();
    for line in sorted.split_inclusive('\n') {
        if !line.trim().is_empty() {
            out.push_str(indentation);
        }
        out.push_str(line);
    }
    out.push_str(tail);
    Some(out)
}

/// the indentation the statements of a region are written at, or `None` for a region with none
fn region_indentation<'a>(region: &Region<'a>) -> Option<&'a str> {
    let Held::Statements { statements } = &region.held else {
        return None;
    };
    let first = statements.first()?;
    let version = region.version;
    let line_start =
        version.line_start(version.lines.line_index(first.start()).to_zero_indexed())?;
    Some(leading_indentation(
        &version.text[TextRange::new(line_start, first.start())],
    ))
}

impl Region<'_> {
    /// what follows the last statement or element of the region on lines of their own: the blank
    /// lines before what comes after it
    fn tail(&self) -> &str {
        let last = match &self.held {
            Held::Statements { statements } => statements.last().map(Ranged::end),
            Held::Elements { elements, .. } => elements.last().map(|last| last.range().end()),
        };
        let start = last.map_or(self.range.start(), |end| {
            self.version.next_line_start(end).min(self.range.end())
        });
        &self.version.text[TextRange::new(start, self.range.end())]
    }

    /// the text each statement or element of the region is written as: from the end of the line
    /// the one before it ends on, or the start of the region, to the end of its own last line
    fn pieces(&self) -> Vec<&str> {
        let ends: Vec<TextSize> = match &self.held {
            Held::Statements { statements } => statements.iter().map(Ranged::end).collect(),
            Held::Elements { elements, .. } => elements.iter().map(|e| e.range().end()).collect(),
        };
        let mut start = self.range.start();
        ends.into_iter()
            .map(|end| {
                let end = self.version.next_line_start(end).min(self.range.end());
                let piece = &self.version.text[TextRange::new(start, end)];
                start = end;
                piece
            })
            .collect()
    }
}

// ── added statements and elements ───────────────────────────────────────────────────────────────

/// where each statement of `side` is in `base`, which it must hold in order: `None` for one the
/// side added. `None` overall for a side that changed or removed one of `base`'s
fn alignment<T>(
    base: &[T],
    side: &[T],
    same: impl Fn(&T, &T) -> bool,
) -> Option<Vec<Option<usize>>> {
    let mut next = 0;
    let aligned: Vec<Option<usize>> = side
        .iter()
        .map(|item| {
            if next < base.len() && same(&base[next], item) {
                next += 1;
                Some(next - 1)
            } else {
                None
            }
        })
        .collect();
    (next == base.len()).then_some(aligned)
}

/// the items a side added in each place: before each of base's, and after the last
fn added_in_places(aligned: &[Option<usize>], base_len: usize) -> Vec<Vec<usize>> {
    let mut places = vec![Vec::new(); base_len + 1];
    let mut place = 0;
    for (index, position) in aligned.iter().enumerate() {
        match position {
            Some(position) => place = position + 1,
            None => places[place].push(index),
        }
    }
    places
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Ours,
    Theirs,
}

/// how two sides' additions to base's items are kept
struct Merged {
    /// each item kept, in order, by the side and the index in it of the copy kept
    order: Vec<(Origin, usize)>,
    /// what each side added that the other did not
    ours_only: Vec<usize>,
    theirs_only: Vec<usize>,
    /// everything each side added
    ours_added: Vec<usize>,
    theirs_added: Vec<usize>,
}

/// base's items, with what each side added in each place between two of them. where both sides
/// added in one place, what both start and end with there is kept once, ours, and the rest of
/// ours goes before the rest of theirs, which is an order neither side chose: only where every
/// one of them is `reorderable`. `None` for that, and for a side that changed or removed one of
/// base's
fn merge_sequences<T>(
    base: &[T],
    ours: &[T],
    theirs: &[T],
    same: impl Fn(&T, &T) -> bool,
    reorderable: impl Fn(&T) -> bool,
) -> Option<Merged> {
    let ours_aligned = alignment(base, ours, &same)?;
    let theirs_aligned = alignment(base, theirs, &same)?;
    let ours_places = added_in_places(&ours_aligned, base.len());
    let theirs_places = added_in_places(&theirs_aligned, base.len());
    let ours_base: Vec<usize> = (0..ours.len())
        .filter(|index| ours_aligned[*index].is_some())
        .collect();
    let mut merged = Merged {
        order: Vec::new(),
        ours_only: Vec::new(),
        theirs_only: Vec::new(),
        ours_added: ours_places.concat(),
        theirs_added: theirs_places.concat(),
    };
    for (place, (o, t)) in ours_places.iter().zip(&theirs_places).enumerate() {
        let both = |(a, b): (&usize, &usize)| same(&ours[*a], &theirs[*b]);
        let first = o.iter().zip(t).take_while(|pair| both(*pair)).count();
        let last = o[first..]
            .iter()
            .rev()
            .zip(t[first..].iter().rev())
            .take_while(|pair| both(*pair))
            .count();
        let ours_only = &o[first..o.len() - last];
        let theirs_only = &t[first..t.len() - last];
        let interleaved = !ours_only.is_empty() && !theirs_only.is_empty();
        let all_reorderable = ours_only.iter().all(|index| reorderable(&ours[*index]))
            && theirs_only.iter().all(|index| reorderable(&theirs[*index]));
        if interleaved && !all_reorderable {
            return None;
        }
        let ours_items = |items: &[usize]| -> Vec<(Origin, usize)> {
            items.iter().map(|index| (Origin::Ours, *index)).collect()
        };
        merged.order.extend(ours_items(&o[..o.len() - last]));
        merged
            .order
            .extend(theirs_only.iter().map(|index| (Origin::Theirs, *index)));
        merged.order.extend(ours_items(&o[o.len() - last..]));
        if let Some(index) = ours_base.get(place) {
            merged.order.push((Origin::Ours, *index));
        }
        merged.ours_only.extend(ours_only);
        merged.theirs_only.extend(theirs_only);
    }
    Some(merged)
}

/// whether running a definition binds its name and does nothing else, so that two of them run
/// in either order bind the same things: no decorator, base class or class keyword runs code of
/// the program's, and every default and annotation evaluated with it is a name or a literal
fn definition_is_quiet(statement: &Stmt) -> bool {
    match statement {
        Stmt::FunctionDef(function) => {
            function.decorator_list.is_empty()
                && function.parameters.iter().all(|parameter| {
                    parameter.default().is_none_or(is_pure)
                        && parameter.annotation().is_none_or(is_pure)
                })
                && function.returns.as_deref().is_none_or(is_pure)
        }
        Stmt::ClassDef(class) => {
            class.decorator_list.is_empty()
                && class.arguments.as_deref().is_none_or(|arguments| {
                    arguments.args.is_empty() && arguments.keywords.is_empty()
                })
                && class.body.iter().all(|statement| match statement {
                    Stmt::FunctionDef(_) | Stmt::ClassDef(_) => definition_is_quiet(statement),
                    Stmt::Pass(_) => true,
                    Stmt::Expr(expr) => is_pure(&expr.value),
                    Stmt::Assign(assign) => {
                        assign.targets.iter().all(ast::Expr::is_name_expr) && is_pure(&assign.value)
                    }
                    Stmt::AnnAssign(assign) => {
                        assign.target.is_name_expr()
                            && is_pure(&assign.annotation)
                            && assign.value.as_deref().is_none_or(is_pure)
                    }
                    _ => false,
                })
        }
        _ => false,
    }
}

/// the functions and classes each side added at the conflict, both kept. refused where a side
/// changed or removed a statement, added something else, or added a name the other side binds
fn merge_definitions(
    base: &[&Stmt],
    ours: &[&Stmt],
    theirs: &[&Stmt],
    sides: &Sides,
) -> Option<String> {
    let same = |a: &&Stmt, b: &&Stmt| ComparableStmt::from(*a) == ComparableStmt::from(*b);
    let merged = merge_sequences(base, ours, theirs, same, |statement| {
        definition_is_quiet(statement)
    })?;
    if !merged
        .ours_added
        .iter()
        .map(|index| ours[*index])
        .chain(merged.theirs_added.iter().map(|index| theirs[*index]))
        .all(|statement| statement.is_function_def_stmt() || statement.is_class_def_stmt())
    {
        return None;
    }
    for (only, statements, side, other) in [
        (&merged.ours_only, ours, &sides.ours, &sides.theirs),
        (&merged.theirs_only, theirs, &sides.theirs, &sides.ours),
    ] {
        let bound = other.bound(&[]);
        for index in only {
            let names = Names::of_statements([statements[*index]]);
            if names.binding_in(side.scope).any(|name| bound.binds(name)) {
                return None;
            }
        }
    }
    let (ours_region, theirs_region) = (sides.ours.region, sides.theirs.region);
    if merged.theirs_only.is_empty() {
        return Some(ours_region.text().to_string());
    }

    let ours_pieces = ours_region.pieces();
    let theirs_pieces = theirs_region.pieces();
    let blank_lines = if matches!(sides.ours.scope, Scope::Module(_)) {
        2
    } else {
        1
    };
    let mut out = String::new();
    let mut previous = None;
    for (from, index) in merged.order {
        let piece = match from {
            Origin::Ours => ours_pieces[index],
            Origin::Theirs => theirs_pieces[index],
        };
        push_separated(
            &mut out,
            piece,
            blank_lines,
            previous.is_some_and(|previous| previous != from),
        );
        previous = Some(from);
    }
    out.push_str(ours_region.tail());
    Some(out)
}

/// append `piece`, after at least `blank_lines` blank lines when it follows a piece from the
/// other side, which the two sides' own blank lines may not have put there
fn push_separated(out: &mut String, piece: &str, blank_lines: usize, other_side: bool) {
    if other_side && !out.is_empty() {
        let trailing = out.trim_end_matches(['\n', '\r']).len();
        let had = out[trailing..].matches('\n').count().saturating_sub(1);
        let leading = piece
            .split_inclusive('\n')
            .take_while(|line| line.trim().is_empty())
            .count();
        for _ in had + leading..blank_lines {
            out.push('\n');
        }
    }
    out.push_str(piece);
}

/// the elements each side added to a display or a decorator list, each kept in the place it was
/// added. refused where a side changed or removed one, where both added different ones in one
/// place, as the order they are evaluated and kept in, or applied in, would be one neither side
/// chose, where two elements share a line, where an element that would no longer be last has no
/// comma after it, or where both sides add one key of a dict
fn merge_elements<'a>(
    base: &[Element<'a>],
    ours: &[Element<'a>],
    theirs: &[Element<'a>],
    sides: &Sides<'_, 'a>,
    ours_container: Container<'a>,
    theirs_container: Container<'a>,
) -> Option<String> {
    let same = |a: &Element, b: &Element| a.same(*b);
    let merged = merge_sequences(base, ours, theirs, same, |_| false)?;
    if let (Container::Dict(ours_dict), Container::Dict(theirs_dict)) =
        (ours_container, theirs_container)
    {
        let keys = |dict: &'a ast::ExprDict| -> Vec<ComparableExpr<'a>> {
            dict.items
                .iter()
                .filter_map(|item| Element::DictItem(item).key())
                .collect()
        };
        let (ours_keys, theirs_keys) = (keys(ours_dict), keys(theirs_dict));
        for (only, elements, other_keys) in [
            (&merged.ours_only, ours, &theirs_keys),
            (&merged.theirs_only, theirs, &ours_keys),
        ] {
            if only.iter().any(|index| {
                elements[*index]
                    .key()
                    .is_some_and(|key| other_keys.contains(&key))
            }) {
                return None;
            }
        }
    }
    for (only, elements, side, other) in [
        (&merged.ours_only, ours, &sides.ours, &sides.theirs),
        (&merged.theirs_only, theirs, &sides.theirs, &sides.ours),
    ] {
        let bound = other.bound(&[]);
        for index in only {
            if elements[*index]
                .names()
                .binding_in(side.scope)
                .any(|name| bound.binds(name))
            {
                return None;
            }
        }
    }
    let (ours_region, theirs_region) = (sides.ours.region, sides.theirs.region);
    if merged.theirs_only.is_empty() {
        return Some(ours_region.text().to_string());
    }
    for (region, elements) in [(ours_region, ours), (theirs_region, theirs)] {
        // each element on lines of its own, which is what lets them be moved by the line
        for element in elements {
            let line = region.version.lines.line_index(element.range().start());
            if elements.iter().any(|other| {
                other.range() != element.range()
                    && region.version.lines.line_index(other.range().end()) == line
            }) {
                return None;
            }
        }
    }

    let ours_pieces = ours_region.pieces();
    let theirs_pieces = theirs_region.pieces();
    let mut out = String::new();
    for (position, (from, index)) in merged.order.iter().enumerate() {
        let (region, element, container, piece) = match from {
            Origin::Ours => (
                ours_region,
                ours[*index],
                ours_container,
                ours_pieces[*index],
            ),
            Origin::Theirs => (
                theirs_region,
                theirs[*index],
                theirs_container,
                theirs_pieces[*index],
            ),
        };
        // the last may go without its comma only where it is ours' own last, before what
        // follows the conflict
        let ours_last = *from == Origin::Ours && *index + 1 == ours.len();
        if container.display().is_some()
            && !element.has_trailing_comma(region.version, container)
            && !(position + 1 == merged.order.len() && ours_last)
        {
            return None;
        }
        out.push_str(piece);
    }
    out.push_str(ours_region.tail());
    Some(out)
}

// ── imports after a merge ───────────────────────────────────────────────────────────────────────

/// one of the three versions a merge combines
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeVersion {
    Ours,
    Base,
    Theirs,
}

/// `range` of a text, replaced by `replacement`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeEdit {
    pub range: TextRange,
    pub replacement: String,
}

/// a name the versions of a merged file import from different places, and the import
/// statements of each version that bind it, which nothing here chooses between
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportChoice {
    pub name: String,
    /// each version's import statements binding the name, in the order ours, base, theirs: none
    /// for a version that does not import it
    pub imports: Vec<(MergeVersion, Vec<String>)>,
    /// where in the merged text the import belongs: after the imports it starts with
    pub offset: TextSize,
    /// the lines of the merged text's import statements that bind the name and nothing else,
    /// which whatever is chosen replaces
    pub bindings: Vec<TextRange>,
}

/// what reconciling a merged file's imports with its three versions answers
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconciledImports {
    /// the edits that reconcile the merged file's imports, in order: none where its imports need
    /// none, and none where the merged file or one of its versions does not parse, since what it
    /// imports or reads cannot then be told
    Edits(Vec<MergeEdit>),
    /// the versions import a name from different places
    Refused(ImportChoice),
}

/// the edits that make the imports of `merged`, the file a merge of `texts` wrote, say what its
/// code reads, or why they cannot be decided
///
/// a line merge can be clean and still leave a broken file: ours removes `import json` with the
/// last function reading it, theirs adds another that reads it, and the merge writes a file
/// reading `json` that nothing imports. the three versions' imports say where each name comes
/// from, so the module-level imports of the merged file are reconciled with them:
///
/// - a name the merged file reads, which nothing in it binds and which is not a builtin, is
///   imported as the versions import it, where exactly one place to import it from is among
///   their module-level imports. there is nothing to do where none of them imports it, or where
///   the merged file has a star import, which may bind it
/// - an import a side removed, which binds a name the merged file does not mention, in its code
///   or in any string, is removed. an import neither side removed stays, a re-export or an
///   import for its side effects among them
/// - refused where the versions import one name from different places and the merged file
///   needs it, and where the merged file's own imports bind one name to two things: nothing
///   here can say which the code means
///
/// the imports the file starts with are then written as `sort_imports` writes them. where they
/// have a comment among them, which a sort would have to place, they are left as they are, and
/// an import added after them
pub fn reconcile_merge_imports(
    texts: MergeTexts<'_>,
    merged: &str,
    source_type: PySourceType,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> ReconciledImports {
    reconcile(texts, &[merged], source_type, sort_imports)
}

/// [`reconcile_merge_imports`] for a merged file that still has conflicts nothing resolved
///
/// the file is read as it would be with each conflict resolved to ours, and as it would be
/// resolved to theirs, so that whatever is chosen in a conflict has the imports it needs: a name
/// either side of a conflict reads is read, and an import either side of one mentions is
/// mentioned. the edits are to the lines the merge took without a conflict: one that would
/// change a conflict's lines is left out, the imports of a file whose imports are themselves in
/// a conflict among them
pub fn reconcile_conflicted_merge_imports(
    texts: MergeTexts<'_>,
    merged: &ConflictedText,
    source_type: PySourceType,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> ReconciledImports {
    let ours = merged.side(ConflictSide::Ours);
    let theirs = merged.side(ConflictSide::Theirs);
    match reconcile(texts, &[&ours, &theirs], source_type, sort_imports) {
        ReconciledImports::Edits(edits) => ReconciledImports::Edits(
            edits
                .into_iter()
                .filter_map(|edit| {
                    Some(MergeEdit {
                        range: merged.range_from_ours(edit.range)?,
                        replacement: edit.replacement,
                    })
                })
                .collect(),
        ),
        ReconciledImports::Refused(choice) => ReconciledImports::Refused(ImportChoice {
            offset: merged.offset_from_ours(choice.offset),
            bindings: choice
                .bindings
                .iter()
                .filter_map(|range| merged.range_from_ours(*range))
                .collect(),
            ..choice
        }),
    }
}

/// [`reconcile_merge_imports`] for a merged file that reads as each of `variants`, the edits,
/// and where a choice goes, as edits to the first
fn reconcile(
    texts: MergeTexts<'_>,
    variants: &[&str],
    source_type: PySourceType,
    sort_imports: &dyn Fn(&str) -> Option<String>,
) -> ReconciledImports {
    let (Some(base), Some(ours), Some(theirs)) = (
        Version::new(texts.base, source_type),
        Version::new(texts.ours, source_type),
        Version::new(texts.theirs, source_type),
    ) else {
        return ReconciledImports::Edits(Vec::new());
    };
    let Some(readings) = variants
        .iter()
        .map(|variant| Version::new(variant, source_type))
        .collect::<Option<Vec<_>>>()
    else {
        return ReconciledImports::Edits(Vec::new());
    };
    let Some(result) = readings.first() else {
        return ReconciledImports::Edits(Vec::new());
    };
    let module_imports = |version: &Version| imported(&module_import_statements(version.module()));
    let (b, o, t, m) = (
        module_imports(&base),
        module_imports(&ours),
        module_imports(&theirs),
        module_imports(result),
    );
    let versions = [
        (MergeVersion::Ours, &o),
        (MergeVersion::Base, &b),
        (MergeVersion::Theirs, &t),
    ];
    let section = ImportSection::of(result);
    let choice = |name: &str| {
        ReconciledImports::Refused(ImportChoice {
            name: name.to_string(),
            imports: versions
                .iter()
                .map(|(version, imports)| {
                    let mut statements: Vec<String> = imports
                        .iter()
                        .filter(|import| import.binds() == name)
                        .map(Imported::statement)
                        .collect();
                    statements.dedup();
                    (*version, statements)
                })
                .collect(),
            offset: section.end,
            bindings: module_import_statements(result.module())
                .into_iter()
                .filter(|statement| {
                    imported(&[statement])
                        .iter()
                        .all(|import| import.binds() == name)
                        && result.has_own_lines(statement)
                        && !result.has_comment_in(statement.range())
                })
                .filter_map(|statement| result.statement_lines(statement))
                .collect(),
        })
    };

    let imports_of_readings: Vec<Vec<Imported>> = readings.iter().map(module_imports).collect();
    for imports in &imports_of_readings {
        for (index, import) in imports.iter().enumerate() {
            if imports[..index].iter().any(|earlier| {
                earlier.binds() == import.binds() && earlier.bound_to() != import.bound_to()
            }) {
                return choice(import.binds());
            }
        }
    }

    // a name is needed where a reading reads it and binds it nowhere
    let usages: Vec<Usage> = readings
        .iter()
        .map(|reading| Usage::of(reading.module()))
        .collect();
    let star = usages.iter().any(|usage| usage.star)
        || imports_of_readings
            .iter()
            .flatten()
            .any(|import| import.name == "*");
    let mut added: Vec<&Imported> = Vec::new();
    if !star {
        let mut unbound: Vec<&str> = usages
            .iter()
            .flat_map(|usage| {
                usage
                    .reads
                    .iter()
                    .filter(|name| !usage.bound.contains(*name))
            })
            .filter(|name| {
                !is_python_builtin(
                    name,
                    PythonVersion::latest_ty().minor,
                    source_type.is_ipynb(),
                )
            })
            .copied()
            .collect();
        unbound.sort_unstable();
        unbound.dedup();
        for name in unbound {
            let mut places: Vec<&Imported> = Vec::new();
            for import in versions
                .iter()
                .flat_map(|(_, imports)| imports.iter())
                .filter(|import| import.binds() == name)
            {
                if !places
                    .iter()
                    .any(|place| place.bound_to() == import.bound_to())
                {
                    places.push(import);
                }
            }
            match places.as_slice() {
                [] => {}
                [place] => added.push(place),
                _ => return choice(name),
            }
        }
    }

    let removed_by_a_side =
        |import: &Imported| b.contains(import) && (!o.contains(import) || !t.contains(import));
    let dropped: Vec<&Imported> = m
        .iter()
        .filter(|import| {
            import.name != "*"
                && removed_by_a_side(import)
                && !readings
                    .iter()
                    .any(|reading| reading.mentions_outside_imports(import.binds()))
        })
        .collect();

    if added.is_empty() && dropped.is_empty() {
        return ReconciledImports::Edits(Vec::new());
    }
    ReconciledImports::Edits(section.edits(result, &added, &dropped, sort_imports))
}

/// [`reconcile_merge_imports`] for the file at `path` in `db`'s project, read and sorted as
/// [`resolve_file_merge_conflicts`] reads and sorts it, and [`reconcile_conflicted_merge_imports`]
/// where `merged` still has conflicts, in markers of `marker_size` characters
pub fn reconcile_file_merge_imports(
    db: &dyn Db,
    path: Option<&SystemPath>,
    texts: MergeTexts<'_>,
    merged: &str,
    marker_size: usize,
) -> ReconciledImports {
    let root = db.project().root(db);
    let source_type = path
        .and_then(|path| path.extension())
        .and_then(PySourceType::try_from_extension)
        .unwrap_or_default();
    let isort = match path {
        Some(path) => IsortConfig::for_file(db.system(), path.as_std_path(), root.as_std_path()),
        None => IsortConfig::for_directory(db.system(), root.as_std_path(), root.as_std_path()),
    };
    let sort_imports = |source: &str| isort.sort(source, source_type);
    match ConflictedText::parse(merged, marker_size) {
        Some(conflicted) if conflicted.conflicts() > 0 => {
            reconcile_conflicted_merge_imports(texts, &conflicted, source_type, &sort_imports)
        }
        _ => reconcile_merge_imports(texts, merged, source_type, &sort_imports),
    }
}

/// writes a run of import statements as buff's isort configuration for the file at `path` sorts
/// them, `project_root` standing in for the directory buff resolves `src` against where no
/// configuration file says: for a tool that has no project database, as a merge driver has not
pub fn file_import_sorter<'a>(
    system: &'a dyn System,
    path: &SystemPath,
    project_root: &SystemPath,
) -> impl Fn(&str) -> Option<String> + 'a {
    let source_type = path
        .extension()
        .and_then(PySourceType::try_from_extension)
        .unwrap_or_default();
    let isort = IsortConfig::for_file(system, path.as_std_path(), project_root.as_std_path());
    move |source| isort.sort(source, source_type)
}

/// `edits` applied to `text`, which are in order and do not overlap
pub fn apply_merge_edits(text: &str, edits: &[MergeEdit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = TextSize::default();
    for edit in edits {
        out.push_str(&text[TextRange::new(at, edit.range.start())]);
        out.push_str(&edit.replacement);
        at = edit.range.end();
    }
    out.push_str(&text[TextRange::new(at, TextSize::of(text))]);
    out
}

/// the import statements of a module's own body, not those in a function, a class or a block,
/// which bind only where they run
fn module_import_statements(module: &[Stmt]) -> Vec<&Stmt> {
    module
        .iter()
        .filter(|statement| statement.is_import_stmt() || statement.is_import_from_stmt())
        .collect()
}

impl Version<'_> {
    /// whether `name` is a token of the text outside its import statements, or a word in one of
    /// its strings, where an annotation in quotes, `__all__` and the like name it
    fn mentions_outside_imports(&self, name: &str) -> bool {
        let imports: Vec<TextRange> = module_import_statements(self.module())
            .iter()
            .map(Ranged::range)
            .collect();
        self.parsed.tokens().iter().any(|token| match token.kind() {
            TokenKind::Identifier => {
                &self.text[token.range()] == name
                    && !imports
                        .iter()
                        .any(|range| range.contains_range(token.range()))
            }
            TokenKind::String | TokenKind::FStringMiddle | TokenKind::TStringMiddle => self.text
                [token.range()]
            .split(|character: char| !(character.is_alphanumeric() || character == '_'))
            .any(|word| word == name),
            _ => false,
        })
    }

    /// whether `statement` has lines of its own: nothing before it on its first line, and only
    /// whitespace after it on its last
    fn has_own_lines(&self, statement: &Stmt) -> bool {
        let line = self.lines.line_index(statement.start()).to_zero_indexed();
        let after = TextRange::new(statement.end(), self.next_line_start(statement.end()));
        self.line_start(line) == Some(statement.start()) && self.text[after].trim().is_empty()
    }

    /// the lines `statement` is on, their line endings included
    fn statement_lines(&self, statement: &Stmt) -> Option<TextRange> {
        let line = self.lines.line_index(statement.start()).to_zero_indexed();
        Some(TextRange::new(
            self.line_start(line)?,
            self.next_line_start(statement.end()),
        ))
    }

    /// whether a comment is in `range`
    fn has_comment_in(&self, range: TextRange) -> bool {
        self.parsed
            .tokens()
            .in_range(range)
            .iter()
            .any(|token| token.kind() == TokenKind::Comment)
    }
}

/// the import statements a module starts with, after its docstring, which is where an import
/// it needs goes
struct ImportSection<'a> {
    statements: &'a [Stmt],
    /// the lines the statements are on, empty at where an import goes where there are none
    range: TextRange,
    /// where what is after the section starts
    end: TextSize,
    /// whether something other than an import follows the section on the next line, which an
    /// import added where there was none is set apart from by a blank line
    followed: bool,
}

impl<'a> ImportSection<'a> {
    fn of(version: &'a Version<'a>) -> Self {
        let module = version.module();
        let first = usize::from(module.first().is_some_and(|statement| {
            statement
                .as_expr_stmt()
                .is_some_and(|expr| expr.value.is_string_literal_expr())
        }));
        let count = module[first..]
            .iter()
            .take_while(|statement| statement.is_import_stmt() || statement.is_import_from_stmt())
            .count();
        let statements = &module[first..first + count];
        let line_start = |offset: TextSize| {
            let line = version.lines.line_index(offset).to_zero_indexed();
            version.line_start(line).unwrap_or(offset)
        };
        let range = match (statements.first(), statements.last()) {
            (Some(first), Some(last)) => TextRange::new(
                line_start(first.start()),
                version.next_line_start(last.end()),
            ),
            _ => match (
                first.checked_sub(1).and_then(|index| module.get(index)),
                module.get(first),
            ) {
                // after the docstring
                (Some(docstring), _) => TextRange::empty(version.next_line_start(docstring.end())),
                // before the first statement, after any comments the module starts with
                (None, Some(statement)) => TextRange::empty(line_start(statement.start())),
                (None, None) => TextRange::empty(TextSize::of(version.text)),
            },
        };
        Self {
            statements,
            range,
            end: range.end(),
            followed: statements.is_empty() && module.get(first).is_some(),
        }
    }

    /// whether the section can be written again from its imports: each statement on lines of its
    /// own, and no comment among them that a sort would have to place
    fn rewritable(&self, version: &Version) -> bool {
        !self.statements.is_empty()
            && self
                .statements
                .iter()
                .all(|statement| version.has_own_lines(statement))
            && !version.has_comment_in(self.range)
    }

    fn edits(
        &self,
        version: &Version,
        added: &[&Imported],
        dropped: &[&Imported],
        sort_imports: &dyn Fn(&str) -> Option<String>,
    ) -> Vec<MergeEdit> {
        let write = |imports: &[&Imported]| {
            let statements: String = imports
                .iter()
                .map(|import| import.statement() + "\n")
                .collect();
            sort_imports(&statements).unwrap_or(statements)
        };
        let rewritable = self.rewritable(version);
        let mut edits = Vec::new();
        // a statement outside a section written again loses only what is dropped from it
        for statement in module_import_statements(version.module()) {
            let in_section = self
                .statements
                .iter()
                .any(|held| std::ptr::eq(held, statement));
            if rewritable && in_section {
                continue;
            }
            let imports = imported(&[statement]);
            let kept: Vec<&Imported> = imports
                .iter()
                .filter(|import| !dropped.contains(import))
                .collect();
            if kept.len() == imports.len()
                || !version.has_own_lines(statement)
                || version.has_comment_in(statement.range())
            {
                continue;
            }
            let Some(range) = version.statement_lines(statement) else {
                continue;
            };
            let replacement = if kept.is_empty() {
                String::new()
            } else {
                write(&kept)
            };
            edits.push(MergeEdit { range, replacement });
        }
        if rewritable {
            let imports = imported(&self.statements.iter().collect::<Vec<_>>());
            let mut kept: Vec<&Imported> = imports
                .iter()
                .filter(|import| !dropped.contains(import))
                .collect();
            kept.extend(added.iter().copied());
            edits.push(MergeEdit {
                range: self.range,
                replacement: write(&kept),
            });
        } else if !added.is_empty() {
            let mut replacement = write(added);
            if self.followed {
                replacement.push('\n');
            }
            edits.push(MergeEdit {
                range: TextRange::empty(self.end),
                replacement,
            });
        }
        edits.sort_by_key(|edit| edit.range.start());
        edits
    }
}

/// what a module reads and binds, at any depth
#[derive(Default)]
struct Usage<'a> {
    /// the names it reads, and the names its `__all__` lists, which it exports
    reads: FxHashSet<&'a str>,
    /// the names it binds anywhere, in any scope
    bound: FxHashSet<&'a str>,
    /// whether a star import binds names the source does not tell
    star: bool,
}

impl<'a> Usage<'a> {
    fn of(module: &'a [Stmt]) -> Self {
        let mut usage = Self::default();
        usage.visit_body(module);
        for statement in module {
            usage.reads.extend(dunder_all_names(statement));
        }
        usage
    }
}

/// the names a statement adds to `__all__`: `__all__ = [...]`, `__all__ += [...]`,
/// `__all__.extend([...])` or `__all__.append("…")`
fn dunder_all_names(statement: &Stmt) -> Vec<&str> {
    fn strings(expr: &ast::Expr) -> Vec<&str> {
        let elements = match expr {
            ast::Expr::List(list) => &list.elts,
            ast::Expr::Tuple(tuple) => &tuple.elts,
            _ => return Vec::new(),
        };
        elements
            .iter()
            .filter_map(|element| Some(element.as_string_literal_expr()?.value.to_str()))
            .collect()
    }
    let is_all = |expr: &ast::Expr| expr.as_name_expr().is_some_and(|name| name.id == "__all__");
    match statement {
        Stmt::Assign(assign) if assign.targets.iter().any(is_all) => strings(&assign.value),
        Stmt::AnnAssign(assign) if is_all(&assign.target) => {
            assign.value.as_deref().map(strings).unwrap_or_default()
        }
        Stmt::AugAssign(assign) if is_all(&assign.target) => strings(&assign.value),
        Stmt::Expr(expr) => {
            let Some(call) = expr.value.as_call_expr() else {
                return Vec::new();
            };
            let Some(attribute) = call.func.as_attribute_expr() else {
                return Vec::new();
            };
            if !is_all(&attribute.value) {
                return Vec::new();
            }
            match (attribute.attr.as_str(), &*call.arguments.args) {
                ("extend", [argument]) => strings(argument),
                ("append", [argument]) => argument
                    .as_string_literal_expr()
                    .map(|string| vec![string.value.to_str()])
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

impl<'a> Visitor<'a> for Usage<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                self.bound.insert(function.name.as_str());
            }
            Stmt::ClassDef(class) => {
                self.bound.insert(class.name.as_str());
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name).as_str();
                    // `import a.b` binds `a`
                    self.bound.insert(bound.split('.').next().unwrap_or(bound));
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    if alias.asname.is_none() && &alias.name == "*" {
                        self.star = true;
                    } else {
                        self.bound
                            .insert(alias.asname.as_ref().unwrap_or(&alias.name).as_str());
                    }
                }
            }
            _ => {}
        }
        visitor::walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'a ast::Expr) {
        if let ast::Expr::Name(name) = expr {
            if name.ctx == ExprContext::Load {
                self.reads.insert(name.id.as_str());
            } else {
                self.bound.insert(name.id.as_str());
            }
        }
        visitor::walk_expr(self, expr);
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        self.bound.insert(parameter.name.as_str());
        visitor::walk_parameter(self, parameter);
    }

    fn visit_except_handler(&mut self, except_handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(handler) = except_handler;
        if let Some(name) = &handler.name {
            self.bound.insert(name.as_str());
        }
        visitor::walk_except_handler(self, except_handler);
    }

    fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
        let name = match pattern {
            ast::Pattern::MatchAs(pattern) => pattern.name.as_ref(),
            ast::Pattern::MatchStar(pattern) => pattern.name.as_ref(),
            ast::Pattern::MatchMapping(pattern) => pattern.rest.as_ref(),
            _ => None,
        };
        if let Some(name) = name {
            self.bound.insert(name.as_str());
        }
        visitor::walk_pattern(self, pattern);
    }

    fn visit_type_param(&mut self, type_param: &'a ast::TypeParam) {
        self.bound.insert(type_param.name().as_str());
        visitor::walk_type_param(self, type_param);
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_linter::rules::isort::placement::sort_import_statements;
    use ruff_linter::settings::LinterSettings;
    use ruff_python_ast::{PySourceType, PythonVersion};

    use std::ops::Range;

    use super::{
        ConflictLines, MergeTexts, MergeVersion, ReconciledImports, apply_merge_edits,
        reconcile_conflicted_merge_imports, reconcile_merge_imports, resolve_merge_conflicts,
    };
    use crate::ConflictedText;

    fn sort(source: &str) -> Option<String> {
        sort_import_statements(
            source,
            &LinterSettings::default(),
            None,
            PySourceType::BasedPython,
            PythonVersion::PY312,
        )
    }

    /// the file a merge left with git's diff3 conflict markers in it, each conflict replaced by
    /// what resolves it, or left as git wrote it where nothing does
    fn resolved(conflicted: &str) -> String {
        let text = ConflictedText::parse(conflicted, 7).expect("well formed conflict markers");
        let versions = text.versions().expect("diff3 conflict markers");
        let resolutions = resolve_merge_conflicts(
            MergeTexts {
                base: &versions.base,
                ours: &versions.ours,
                theirs: &versions.theirs,
            },
            &versions.conflicts,
            PySourceType::BasedPython,
            &sort,
        );
        text.write(&resolutions, true)
    }

    /// what `git merge` wrote for a branch adding an import, an `__all__` entry, a method and a
    /// new limit, merged into one adding others: the two `__all__` entries, added in one place,
    /// would be kept in an order neither side chose
    #[test]
    fn independent_additions_are_kept_and_a_changed_line_is_not() {
        assert_snapshot!(resolved(r#"import os
<<<<<<< HEAD
import json
||||||| dbcf8e7
=======
import sys
>>>>>>> theirs
from typing import Any

__all__ = [
    "Shape",
    "area",
<<<<<<< HEAD
    "scale",
||||||| dbcf8e7
=======
    "volume",
>>>>>>> theirs
]


class Shape:
    def area(self) -> float:
        return 0.0

<<<<<<< HEAD
    def name(self) -> str:
        return 'shape'

||||||| dbcf8e7
=======
    def perimeter(self) -> float:
        return 1.0

>>>>>>> theirs

def area(shape: Shape) -> float:
    return shape.area()


<<<<<<< HEAD
LIMIT = 30
||||||| dbcf8e7
LIMIT = 10
=======
LIMIT = 20
>>>>>>> theirs
"#), @r#"
        import os
        import json
        import sys
        from typing import Any

        __all__ = [
            "Shape",
            "area",
        <<<<<<< HEAD
            "scale",
        ||||||| dbcf8e7
        =======
            "volume",
        >>>>>>> theirs
        ]


        class Shape:
            def area(self) -> float:
                return 0.0

            def name(self) -> str:
                return 'shape'

            def perimeter(self) -> float:
                return 1.0


        def area(shape: Shape) -> float:
            return shape.area()


        <<<<<<< HEAD
        LIMIT = 30
        ||||||| dbcf8e7
        LIMIT = 10
        =======
        LIMIT = 20
        >>>>>>> theirs
        "#);
    }

    #[test]
    fn imports_binding_one_name_to_two_things_are_not_merged() {
        assert_snapshot!(resolved("import os
<<<<<<< ours
from a import x
||||||| base
=======
from b import x
>>>>>>> theirs
"), @"
        import os
        <<<<<<< ours
        from a import x
        ||||||| base
        =======
        from b import x
        >>>>>>> theirs
        ");
    }

    #[test]
    fn an_import_one_side_removes_is_removed() {
        assert_snapshot!(resolved("import os
<<<<<<< ours
import json
||||||| base
import re
=======
import sys
import re
>>>>>>> theirs
"), @"
        import os
        import json
        import sys
        ");
    }

    #[test]
    fn definitions_of_one_name_are_not_merged() {
        assert_snapshot!(resolved("X = 1
<<<<<<< ours


def f():
    return 1
||||||| base
=======


def f():
    return 2
>>>>>>> theirs
"), @"
        X = 1
        <<<<<<< ours


        def f():
            return 1
        ||||||| base
        =======


        def f():
            return 2
        >>>>>>> theirs
        ");
    }

    #[test]
    fn top_level_definitions_are_kept_two_blank_lines_apart() {
        assert_snapshot!(resolved("X = 1
<<<<<<< ours


def f():
    return 1
||||||| base
=======


def g():
    return 2
>>>>>>> theirs
"), @"
        X = 1


        def f():
            return 1


        def g():
            return 2
        ");
    }

    #[test]
    fn sides_that_differ_only_in_formatting_resolve_to_ours() {
        assert_snapshot!(resolved("<<<<<<< ours
x = [1,2]
||||||| base
x = []
=======
x = [
    1,
    2,
]
>>>>>>> theirs
"), @"x = [1,2]");
    }

    #[test]
    fn a_dict_key_both_sides_add_is_not_merged() {
        assert_snapshot!(resolved(r#"D = {
    "a": 1,
<<<<<<< ours
    "b": 2,
||||||| base
=======
    "b": 3,
>>>>>>> theirs
}
"#), @r#"
        D = {
            "a": 1,
        <<<<<<< ours
            "b": 2,
        ||||||| base
        =======
            "b": 3,
        >>>>>>> theirs
        }
        "#);
    }

    /// what is left unresolved is exactly what git wrote
    fn assert_unresolved(conflicted: &str) {
        assert_eq!(resolved(conflicted), conflicted, "resolved:\n{conflicted}");
    }

    /// decorators compose, so two added in one place apply in an order neither side chose
    #[test]
    fn decorators_both_sides_add_in_one_place_are_not_merged() {
        assert_unresolved(
            "class A:
    @a
<<<<<<< ours
    @abstractmethod
||||||| base
=======
    @property
>>>>>>> theirs
    def f(self): ...
",
        );
    }

    #[test]
    fn decorators_each_side_adds_in_its_own_place_are_kept() {
        assert_snapshot!(resolved("<<<<<<< ours
@b
@a
||||||| base
@a
=======
@a
@c
>>>>>>> theirs
def f(): ...
"), @"
        @b
        @a
        @c
        def f(): ...
        ");
    }

    #[test]
    fn elements_both_sides_add_in_one_place_are_not_merged() {
        assert_unresolved(
            r#"MIDDLEWARE = [
    "a",
<<<<<<< ours
    "b",
||||||| base
=======
    "c",
>>>>>>> theirs
]
"#,
        );
    }

    #[test]
    fn elements_each_side_adds_in_its_own_place_are_kept() {
        assert_snapshot!(resolved(r#"MIDDLEWARE = [
<<<<<<< ours
    "b",
    "a",
||||||| base
    "a",
=======
    "a",
    "c",
>>>>>>> theirs
]
"#), @r#"
        MIDDLEWARE = [
            "b",
            "a",
            "c",
        ]
        "#);
    }

    /// `"p,#q"` ends with no comma, which a scan of its text for `#` misreads
    #[test]
    fn an_element_without_a_comma_that_would_not_be_last_is_not_merged() {
        assert_unresolved(
            r#"X = [
<<<<<<< ours
    0,
    1,
    "p,#q"
||||||| base
    1,
    "p,#q"
=======
    1,
    "p,#q",
    2,
>>>>>>> theirs
]
"#,
        );
    }

    /// registering decorators run when the module does, in an order neither side chose
    #[test]
    fn decorated_definitions_both_sides_add_in_one_place_are_not_merged() {
        assert_unresolved(
            r#"app = App()
<<<<<<< ours


@app.get("/users/me")
def me():
    return 1
||||||| base
=======


@app.get("/users/{id}")
def user(id):
    return 2
>>>>>>> theirs
"#,
        );
    }

    /// ours' function ends the module and theirs' method ends the class: the method would land
    /// in the function's body
    #[test]
    fn additions_to_two_suites_ending_in_one_place_are_not_merged() {
        assert_unresolved(
            "class Shape:
    def area(self) -> float:
        return 0.0
<<<<<<< ours


def helper():
    return 1
||||||| base
=======

    def perimeter(self) -> float:
        return 1.0
>>>>>>> theirs
",
        );
    }

    /// the same statement, in two suites, means two different things
    #[test]
    fn the_same_statement_in_two_suites_is_not_merged() {
        assert_unresolved(
            "class Shape:
    def area(self) -> float:
        return 0.0
<<<<<<< ours


def size():
    return 1
||||||| base
=======

    def size():
        return 1
>>>>>>> theirs
",
        );
    }

    #[test]
    fn a_definition_both_sides_add_first_is_kept_once() {
        assert_snapshot!(resolved("X = 1
<<<<<<< ours


def f():
    return 1


def g():
    return 2
||||||| base
=======


def f():
    return 1


def h():
    return 3
>>>>>>> theirs
"), @"
        X = 1


        def f():
            return 1


        def g():
            return 2


        def h():
            return 3
        ");
    }

    /// `A` stays after theirs' `Other`, as it is in theirs
    #[test]
    fn a_definition_both_sides_add_last_is_kept_once_after_both_sides_others() {
        assert_snapshot!(resolved("class Base:
    pass
<<<<<<< ours


class Mixin:
    pass


class A(Base):
    pass
||||||| base
=======


class Other:
    pass


class A(Base):
    pass
>>>>>>> theirs
"), @"
        class Base:
            pass


        class Mixin:
            pass


        class Other:
            pass


        class A(Base):
            pass
        ");
    }

    /// `f` is between additions of each side's own, in an order neither side chose
    #[test]
    fn a_definition_both_sides_add_between_others_is_not_merged() {
        assert_unresolved(
            "X = 1
<<<<<<< ours


def g():
    return 2


def f():
    return 1
||||||| base
=======


def f():
    return 1


def h():
    return 3
>>>>>>> theirs
",
        );
    }

    #[test]
    fn a_dict_item_both_sides_add_first_is_kept_once() {
        assert_snapshot!(resolved(r#"D = {
    "a": 1,
<<<<<<< ours
    "b": 2,
    "c": 3,
||||||| base
=======
    "b": 2,
>>>>>>> theirs
}
"#), @r#"
        D = {
            "a": 1,
            "b": 2,
            "c": 3,
        }
        "#);
    }

    #[test]
    fn submodule_imports_binding_one_package_are_merged() {
        assert_snapshot!(resolved("import os
<<<<<<< ours
import a.b
||||||| base
=======
import a.c
>>>>>>> theirs
"), @"
        import os
        import a.b
        import a.c
        ");
    }

    #[test]
    fn an_import_of_a_name_the_other_side_binds_elsewhere_is_not_merged() {
        assert_unresolved(
            "import os
<<<<<<< ours
import json
||||||| base
=======
import sys
>>>>>>> theirs


def json():
    pass
",
        );
    }

    /// `helper` is bound, outside the conflict, in each of the ways python binds a name; the
    /// same conflict under another name is resolved
    #[test]
    fn a_definition_of_a_name_bound_in_any_form_is_not_merged() {
        let bindings = [
            "for {} in range(3):\n    pass\n",
            "with open('f') as {}:\n    pass\n",
            "try:\n    pass\nexcept ValueError as {}:\n    pass\n",
            "first, *{} = 1, 2\n",
            "type {} = int\n",
            "if flag:\n    {} = 1\n",
            "while ({} := next(it)):\n    pass\n",
            "print([({} := x) for x in range(3)])\n",
            "match x:\n    case [{}]:\n        pass\n",
            "match x:\n    case {**{}}:\n        pass\n",
            "def bump():\n    global {}\n    {} = 2\n",
            "del {}\n",
            "import {}.path\n",
        ];
        let conflicted = |binding: &str| {
            format!(
                "{binding}
<<<<<<< ours


def helper():
    return 1
||||||| base
=======


def other():
    return 2
>>>>>>> theirs
"
            )
        };
        for binding in bindings {
            assert_unresolved(&conflicted(&binding.replace("{}", "helper")));
            let elsewhere = conflicted(&binding.replace("{}", "unrelated"));
            assert_ne!(
                resolved(&elsewhere),
                elsewhere,
                "not resolved:\n{elsewhere}"
            );
        }
    }

    /// what a star import binds cannot be told
    #[test]
    fn a_definition_beside_a_star_import_is_not_merged() {
        assert_unresolved(
            "from m import *
<<<<<<< ours


def helper():
    return 1
||||||| base
=======


def other():
    return 2
>>>>>>> theirs
",
        );
    }

    #[test]
    fn a_method_named_like_an_attribute_the_other_side_sets_is_not_merged() {
        assert_unresolved(
            r#"class Shape:
    def __init__(self):
        self.name = "shape"
<<<<<<< ours

    def name(self):
        return 1
||||||| base
=======

    def perimeter(self):
        return 2
>>>>>>> theirs
"#,
        );
    }

    #[test]
    fn a_conflict_whose_lines_end_before_they_start_is_not_resolved() {
        let text = "import os\nimport sys\n";
        let resolutions = resolve_merge_conflicts(
            MergeTexts {
                base: text,
                ours: text,
                theirs: text,
            },
            &[ConflictLines {
                base: Range { start: 2, end: 1 },
                ours: 0..1,
                theirs: 0..1,
            }],
            PySourceType::Python,
            &|_| None,
        );
        assert_eq!(resolutions, [None]);
    }

    /// a conflict cannot start inside a token, the string here, but a client can say it does
    #[test]
    fn a_conflict_starting_inside_a_string_is_not_resolved() {
        let text = "x = '''\na\n'''\n";
        let resolutions = resolve_merge_conflicts(
            MergeTexts {
                base: text,
                ours: text,
                theirs: text,
            },
            &[ConflictLines {
                base: 1..2,
                ours: 1..2,
                theirs: 1..2,
            }],
            PySourceType::Python,
            &|_| None,
        );
        assert_eq!(resolutions, [None]);
    }

    /// the merged file with its imports reconciled, or the choice that refused it
    fn reconciled(base: &str, ours: &str, theirs: &str, merged: &str) -> String {
        let texts = MergeTexts { base, ours, theirs };
        match reconcile_merge_imports(texts, merged, PySourceType::BasedPython, &sort) {
            ReconciledImports::Edits(edits) => apply_merge_edits(merged, &edits),
            ReconciledImports::Refused(choice) => format!("refused: {choice:?}"),
        }
    }

    /// ours removed the last function reading `json`, and its import; theirs added another.
    /// `git merge` writes this without a conflict
    #[test]
    fn an_import_a_clean_merge_dropped_that_the_merged_code_reads_is_added_back() {
        let base = "import json\nimport os\n\n\ndef a():\n    return os.sep\n\n\ndef b():\n    return json.dumps(1)\n";
        let ours = "import os\n\n\ndef a():\n    return os.sep\n";
        let theirs = "import json\nimport os\n\n\ndef a():\n    return os.sep\n\n\ndef b():\n    return json.dumps(1)\n\n\ndef c():\n    return json.loads(\"3\")\n";
        let merged = "import os\n\n\ndef a():\n    return os.sep\n\n\ndef c():\n    return json.loads(\"3\")\n";
        assert_snapshot!(reconciled(base, ours, theirs, merged), @r#"
        import json
        import os


        def a():
            return os.sep


        def c():
            return json.loads("3")
        "#);
    }

    /// a module with no imports left gets the one it needs before its code, apart from it
    #[test]
    fn an_import_is_added_after_the_docstring_where_there_are_no_imports() {
        let base = "\"\"\"doc\"\"\"\nimport json\n\nX = json\n";
        let ours = "\"\"\"doc\"\"\"\n";
        let merged = "\"\"\"doc\"\"\"\nX = json\n";
        assert_snapshot!(reconciled(base, ours, base, merged), @r#"
        """doc"""
        import json

        X = json
        "#);
    }

    /// ours stopped reading `json` and removed its import; theirs added `sys` beside it, and the
    /// conflict was resolved keeping `json`
    #[test]
    fn an_import_a_side_removed_that_nothing_reads_is_dropped() {
        let base = "import json\nimport os\n\nX = json, os\n";
        let ours = "import os\n\nX = os\n";
        let theirs = "import json\nimport os\nimport sys\n\nX = json, os, sys\n";
        let merged = "import json\nimport os\nimport sys\n\nX = os, sys\n";
        assert_snapshot!(reconciled(base, ours, theirs, merged), @"
        import os
        import sys

        X = os, sys
        ");
    }

    /// an import nothing reads stays where no side removed it, and one a side removed stays
    /// where a string names it, as an annotation in quotes does
    #[test]
    fn an_import_is_dropped_only_where_a_side_removed_it_and_nothing_names_it() {
        let base = "import json\nimport os\nimport sys\n";
        let ours = "import os\nimport sys\n";
        let merged = "import json\nimport os\nimport sys\n\nX: \"json.JSONDecoder\"\n";
        assert_eq!(reconciled(base, ours, base, merged), merged);
    }

    #[test]
    fn a_name_in_dunder_all_is_read() {
        let base = "from m import thing\n\n__all__ = [\"thing\"]\n";
        let ours = "__all__ = [\"thing\"]\n";
        let merged = "__all__ = [\"thing\"]\n";
        assert_snapshot!(reconciled(base, ours, base, merged), @r#"
        from m import thing

        __all__ = ["thing"]
        "#);
    }

    /// `open` is the builtin's wherever nothing binds it, and nothing here can say whether the
    /// code reading it means the builtin or the import a side removed
    #[test]
    fn a_builtin_is_not_imported() {
        let base = "from os import open\n\nX = open\n";
        let ours = "X = open\n";
        assert_eq!(reconciled(base, ours, base, ours), ours);
    }

    #[test]
    fn a_name_the_versions_import_from_different_places_is_refused() {
        let base = "from a import x\n\nY = x\n";
        let theirs = "from b import x\n\nY = x\n";
        let merged = "Y = x\n";
        let texts = MergeTexts {
            base,
            ours: merged,
            theirs,
        };
        let ReconciledImports::Refused(choice) =
            reconcile_merge_imports(texts, merged, PySourceType::BasedPython, &sort)
        else {
            panic!("expected a refusal");
        };
        assert_eq!(choice.name, "x");
        assert_eq!(
            choice.imports,
            vec![
                (MergeVersion::Ours, vec![]),
                (MergeVersion::Base, vec!["from a import x".to_string()]),
                (MergeVersion::Theirs, vec!["from b import x".to_string()]),
            ]
        );
    }

    /// each side added an import of `x` on lines of its own, from a different module, which a
    /// line merge keeps both of, the later silently winning
    #[test]
    fn a_name_the_merged_imports_bind_twice_is_refused() {
        let base = "import os\n";
        let ours = "from a import x\nimport os\n";
        let theirs = "import os\nfrom b import x\n";
        let merged = "from a import x\nimport os\nfrom b import x\n";
        let texts = MergeTexts { base, ours, theirs };
        let ReconciledImports::Refused(choice) =
            reconcile_merge_imports(texts, merged, PySourceType::BasedPython, &sort)
        else {
            panic!("expected a refusal");
        };
        // both statements binding `x` are what is chosen between
        let bindings: Vec<&str> = choice
            .bindings
            .iter()
            .map(|range| &merged[*range])
            .collect();
        assert_eq!(bindings, ["from a import x\n", "from b import x\n"]);
    }

    /// the imports are left as they are where a comment is among them, and the one the code
    /// needs is added after them
    #[test]
    fn imports_with_a_comment_among_them_are_not_sorted() {
        let base = "import os  # the os\nimport json\n\nX = json\n";
        let ours = "import os  # the os\n\nY = 1\n";
        let merged = "import os  # the os\n\nY = 1\nX = json\n";
        assert_snapshot!(reconciled(base, ours, base, merged), @"
        import os  # the os
        import json

        Y = 1
        X = json
        ");
    }

    /// the merged file as the driver leaves it with a conflict nothing resolves, its imports
    /// reconciled outside the conflict
    fn reconciled_conflicted(base: &str, ours: &str, theirs: &str, merged: &str) -> String {
        let texts = MergeTexts { base, ours, theirs };
        let conflicted = ConflictedText::parse(merged, 7).expect("well formed markers");
        match reconcile_conflicted_merge_imports(
            texts,
            &conflicted,
            PySourceType::BasedPython,
            &sort,
        ) {
            ReconciledImports::Edits(edits) => apply_merge_edits(merged, &edits),
            ReconciledImports::Refused(choice) => format!("refused: {choice:?}"),
        }
    }

    /// ours removed the last reader of `json` and its import, theirs made another function read
    /// it, and both changed the limit: the import goes back, and the conflict stays as it is
    #[test]
    fn an_import_is_added_back_around_a_conflict_left_open() {
        let base = "import json\nimport os\n\n\ndef b():\n    return json.dumps(1)\n\n\ndef c():\n    return 3\n\n\nLIMIT = 1\n";
        let ours = "import os\n\n\ndef c():\n    return 3\n\n\nLIMIT = 2\n";
        let theirs = "import json\nimport os\n\n\ndef b():\n    return json.dumps(1)\n\n\ndef c():\n    return json.loads(\"3\")\n\n\nLIMIT = 3\n";
        let merged = "import os\n\n\ndef c():\n    return json.loads(\"3\")\n\n\n<<<<<<< HEAD\nLIMIT = 2\n=======\nLIMIT = 3\n>>>>>>> theirs\n";
        assert_snapshot!(reconciled_conflicted(base, ours, theirs, merged), @r#"
        import json
        import os


        def c():
            return json.loads("3")


        <<<<<<< HEAD
        LIMIT = 2
        =======
        LIMIT = 3
        >>>>>>> theirs
        "#);
    }

    /// what either side of an open conflict reads is read: theirs's side reads `sys`, which ours
    /// removed the import of
    #[test]
    fn what_a_side_of_an_open_conflict_reads_keeps_its_import() {
        let base = "import os\nimport sys\n\nA = os\nLIMIT = sys\n";
        let ours = "import os\n\nA = os\nLIMIT = 2\n";
        let theirs = "import os\nimport sys\n\nA = os\nLIMIT = sys, 3\n";
        let merged = "import os\nimport sys\n\nA = os\n<<<<<<< HEAD\nLIMIT = 2\n=======\nLIMIT = sys, 3\n>>>>>>> theirs\n";
        assert_eq!(reconciled_conflicted(base, ours, theirs, merged), merged);
    }

    /// imports that are themselves in a conflict are the conflict's: nothing touches them
    #[test]
    fn imports_in_an_open_conflict_are_left_as_they_are() {
        let base = "import os\n\nA = os\n";
        let ours = "import os\nimport re\n\nA = os, json\n";
        let theirs = "import json\nimport os\n\nA = os\n";
        let merged = "<<<<<<< HEAD\nimport os\nimport re\n=======\nimport json\nimport os\n>>>>>>> theirs\n\nA = os, json\n";
        assert_eq!(reconciled_conflicted(base, ours, theirs, merged), merged);
    }
}
