//! After reverse transforms rewrite `@dataclass`, `@abstractmethod`, `@final`,
//! `NamedTuple` subclassing, `Callable[...]` annotations, etc. into the
//! basedpython keyword forms, the original `from ... import ...` lines that
//! supplied those names become dead. This pass scans the final output for
//! references to each imported name and removes (or trims) imports whose
//! bindings are unused.
//!
//! Operates on the post-reverse-transform string output, not the AST. We
//! reparse, walk for identifier references, then re-emit the file with
//! unused imports stripped. Conservative: a binding is "used" if its name
//! appears as a Name expression OR inside any string literal (covers
//! forward references like `"_AnonNT"`).

use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{Alias, Expr, ExprName, ExprStringLiteral, Stmt, StmtImport, StmtImportFrom};
use std::collections::HashSet;

/// Reparse `source` and prune unused `import` / `from import` bindings.
/// Returns the cleaned source. If parsing fails, returns the input
/// unchanged — pruning is a polish step, not a correctness requirement.
pub(crate) fn prune_unused_imports(source: &str) -> String {
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    // reverse-transpile output is basedpython source (`abstract def`,
    // `static def`, modifier prefixes etc.) so parse with the BasedPython
    // source type rather than plain Python
    let parsed = parse_unchecked_source(source, PySourceType::BasedPython);
    if !parsed.errors().is_empty() {
        return source.to_owned();
    }
    let module = parsed.into_syntax();

    // collect all top-level imports we may want to prune
    let mut import_stmts: Vec<ImportInfo> = Vec::new();
    for (idx, stmt) in module.body.iter().enumerate() {
        if let Stmt::ImportFrom(node) = stmt {
            if node.level > 0 {
                continue;
            }
            import_stmts.push(ImportInfo::FromImport(idx, node));
        } else if let Stmt::Import(node) = stmt {
            import_stmts.push(ImportInfo::Import(idx, node));
        }
    }
    if import_stmts.is_empty() {
        return source.to_owned();
    }

    // gather every name binding introduced by these imports
    let mut binding_names: HashSet<String> = HashSet::new();
    for info in &import_stmts {
        for binding in info.bindings() {
            binding_names.insert(binding.to_owned());
        }
    }

    // walk the module for uses, excluding the import statements themselves
    let mut usage = UsageCollector {
        names: HashSet::new(),
        skip_idx: import_stmts.iter().map(ImportInfo::idx).collect(),
    };
    for (idx, stmt) in module.body.iter().enumerate() {
        if usage.skip_idx.contains(&idx) {
            continue;
        }
        usage.visit_stmt(stmt);
    }

    // also scan import statements that we're NOT considering (e.g. `.` relative
    // imports) so we don't wrongly prune a name that's still referenced through one
    // of those — defensive
    let used = usage.names;

    // produce edits. operate by line spans for simplicity: drop the whole
    // import line when no binding is used; otherwise rewrite to keep used ones
    let mut deletions: Vec<(u32, u32)> = Vec::new();
    let mut replacements: Vec<(u32, u32, String)> = Vec::new();
    for info in &import_stmts {
        let used_aliases: Vec<&Alias> = info
            .aliases()
            .iter()
            .filter(|a| {
                used.contains(binding_name(a)) || is_wildcard(a) || info.is_explicit_reexport(a)
            })
            .collect();
        let total = info.aliases().len();
        if used_aliases.len() == total {
            continue;
        }
        let range = info.range();
        if used_aliases.is_empty() {
            // drop including trailing newline
            let end = consume_trailing_newline(source, range.1);
            deletions.push((range.0, end));
        } else {
            let rewritten = info.rewrite_with(used_aliases.as_slice());
            replacements.push((range.0, range.1, rewritten));
        }
    }

    if deletions.is_empty() && replacements.is_empty() {
        return source.to_owned();
    }

    // apply in reverse order so positions stay valid
    let mut out = source.to_owned();
    let mut edits: Vec<(u32, u32, Option<String>)> = deletions
        .into_iter()
        .map(|(s, e)| (s, e, None))
        .chain(replacements.into_iter().map(|(s, e, r)| (s, e, Some(r))))
        .collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    for (start, end, repl) in edits {
        let s = start as usize;
        let e = end as usize;
        match repl {
            Some(text) => out.replace_range(s..e, &text),
            None => out.replace_range(s..e, ""),
        }
    }
    out
}

enum ImportInfo<'a> {
    Import(usize, &'a StmtImport),
    FromImport(usize, &'a StmtImportFrom),
}

impl ImportInfo<'_> {
    fn idx(&self) -> usize {
        match self {
            Self::Import(i, _) | Self::FromImport(i, _) => *i,
        }
    }

    fn aliases(&self) -> &[Alias] {
        match self {
            Self::Import(_, n) => &n.names,
            Self::FromImport(_, n) => &n.names,
        }
    }

    fn bindings(&self) -> Vec<&str> {
        self.aliases().iter().map(binding_name).collect()
    }

    fn range(&self) -> (u32, u32) {
        use ruff_text_size::Ranged;
        let r = match self {
            Self::Import(_, n) => n.range(),
            Self::FromImport(_, n) => n.range(),
        };
        (u32::from(r.start()), u32::from(r.end()))
    }

    fn rewrite_with(&self, kept: &[&Alias]) -> String {
        match self {
            Self::Import(_, _) => format!(
                "import {}",
                kept.iter()
                    .map(|a| alias_to_string(a))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::FromImport(_, n) => {
                let module = n.module.as_ref().map(|m| m.id.as_str()).unwrap_or("");
                let keyword = if n.is_export { "export" } else { "import" };
                format!(
                    "from {module} {keyword} {}",
                    kept.iter()
                        .map(|a| alias_to_string(a))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        }
    }

    /// Whether `alias` is deliberately part of this module's public api, so
    /// pruning must leave it alone even when nothing references it locally
    fn is_explicit_reexport(&self, alias: &Alias) -> bool {
        match self {
            // basedpython's `from x export y` says so in the keyword
            Self::FromImport(_, n) if n.is_export => true,
            _ => is_redundant_alias(alias),
        }
    }
}

/// `from module import *` re-exports the module's public surface. its names are
/// never referenced locally, so it must be exempt from unused-import pruning —
/// dropping it silently empties re-export modules like `collections/abc`
fn is_wildcard(alias: &Alias) -> bool {
    alias.name.id.as_str() == "*"
}

/// `from module import name as name` (a redundant alias) is the PEP 484 explicit
/// re-export convention used throughout typeshed stubs: the binding is
/// intentionally part of the module's public api even when nothing references it
/// locally, so it must not be pruned
fn is_redundant_alias(alias: &Alias) -> bool {
    alias
        .asname
        .as_ref()
        .is_some_and(|asname| asname.id == alias.name.id)
}

fn binding_name(alias: &Alias) -> &str {
    match &alias.asname {
        Some(a) => a.id.as_str(),
        None => {
            // for `import a.b`, the binding is `a` (the top package)
            let dotted = alias.name.id.as_str();
            dotted.split('.').next().unwrap_or(dotted)
        }
    }
}

fn alias_to_string(alias: &Alias) -> String {
    match &alias.asname {
        Some(a) => format!("{} as {}", alias.name.id.as_str(), a.id.as_str()),
        None => alias.name.id.as_str().to_owned(),
    }
}

fn consume_trailing_newline(source: &str, end: u32) -> u32 {
    let bytes = source.as_bytes();
    let e = end as usize;
    if e < bytes.len() && bytes[e] == b'\n' {
        end.saturating_add(1)
    } else {
        end
    }
}

struct UsageCollector {
    names: HashSet<String>,
    skip_idx: HashSet<usize>,
}

impl<'ast> Visitor<'ast> for UsageCollector {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        match expr {
            Expr::Name(ExprName { id, .. }) => {
                self.names.insert(id.as_str().to_owned());
            }
            Expr::StringLiteral(ExprStringLiteral { value, .. }) => {
                // forward-reference quoted annotations may reference imports
                let text = value.to_string();
                for ident in extract_identifiers(&text) {
                    self.names.insert(ident);
                }
            }
            _ => {}
        }
        walk_expr(self, expr);
    }
}

/// Collect identifier references from a string-literal annotation. Tries to
/// parse the contents as a Python expression (the canonical forward-reference
/// form) and walks the resulting AST for `Name` nodes
fn extract_identifiers(text: &str) -> Vec<String> {
    use ruff_python_parser::{Mode, ParseOptions, parse};

    struct Collector(Vec<String>);
    impl<'ast> Visitor<'ast> for Collector {
        fn visit_expr(&mut self, expr: &'ast Expr) {
            if let Expr::Name(n) = expr {
                self.0.push(n.id.to_string());
            }
            walk_expr(self, expr);
        }
    }

    let Ok(parsed) = parse(text, ParseOptions::from(Mode::Expression)) else {
        return Vec::new();
    };
    let Some(expr) = parsed.syntax().as_expression() else {
        return Vec::new();
    };
    let mut c = Collector(Vec::new());
    c.visit_expr(&expr.body);
    c.0
}

#[cfg(test)]
mod tests {
    use super::prune_unused_imports;

    #[test]
    fn unused_import_dropped() {
        let src = "from typing import Final, TypeVar\nT = TypeVar(\"T\")\n";
        let out = prune_unused_imports(src);
        assert_eq!(out, "from typing import TypeVar\nT = TypeVar(\"T\")\n");
    }

    #[test]
    fn fully_unused_line_dropped() {
        let src = "from typing import Final\nx = 1\n";
        let out = prune_unused_imports(src);
        assert_eq!(out, "x = 1\n");
    }

    #[test]
    fn all_used_kept() {
        let src = "from typing import Final\nx: Final = 1\n";
        let out = prune_unused_imports(src);
        assert_eq!(out, src);
    }

    #[test]
    fn quoted_annotation_counts_as_use() {
        let src = "from typing import List\nx: \"List[int]\" = []\n";
        let out = prune_unused_imports(src);
        assert_eq!(out, src);
    }

    #[test]
    fn keeps_module_import() {
        let src = "import os\nos.getcwd()\n";
        let out = prune_unused_imports(src);
        assert_eq!(out, src);
    }

    #[test]
    fn wildcard_reexport_preserved() {
        // `from x import *` re-exports x's public surface; pruning it empties
        // re-export modules like `collections/abc`
        let src = "from _collections_abc import *\n";
        assert_eq!(prune_unused_imports(src), src);
    }

    #[test]
    fn explicit_reexport_preserved() {
        // `name as name` is the typeshed re-export convention, kept even though
        // nothing references it locally
        let src = "from typing import MutableSet as MutableSet\n";
        assert_eq!(prune_unused_imports(src), src);
    }

    #[test]
    fn dunder_all_reexport_preserved() {
        let src = "from _collections_abc import __all__ as __all__\n";
        assert_eq!(prune_unused_imports(src), src);
    }
}
