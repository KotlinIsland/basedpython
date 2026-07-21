//! ast patches applied to the basedpython typeshed (`.byi`) after fresh
//! reverse-transpile from upstream `.pyi`
//!
//! each patch declares a set of target symbols and emits text edits over a
//! parsed module. patches run as phase 2 of the sync — after reverse-transpile
//! but before the pep 695 `ruff-fix` — so they operate on the legacy
//! `TypeVar(...)` + `Generic[...]` form, where typevars appear as plain name
//! references in class bases and method signatures
//!
//! see `docs/basedpython/development/typeshed-patches.md` for the full design
//! and the ongoing typeshed sync workflow

pub mod patches;
pub mod pep695;

use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_parser::Parsed;
use ruff_text_size::TextRange;

/// a single semantic adjustment applied to one or more typeshed modules
pub trait Patch {
    /// stable identifier used in logs and drift alerts
    fn name(&self) -> &'static str;

    /// qualified symbols this patch touches, e.g. `["typing.Mapping"]`. used
    /// for drift detection: if any of these symbols changed in an upstream
    /// sync, the patch is flagged for review
    fn target_symbols(&self) -> &'static [&'static str];

    /// return text edits over `parsed` if this patch applies to the module at
    /// `module_path` (relative to the typeshed `stdlib/` root). empty vec
    /// means no-op for this file
    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit>;
}

/// minimal text edit. (start, end, replacement). end is exclusive
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
}

/// registry of every legacy-form patch, applied in pass 1 before the pep 695
/// conversion. these see the `TypeVar(...)` + `Generic[...]` form
pub fn all_patches() -> Vec<Box<dyn Patch>> {
    // patches are added here as upstream syncs surface concrete drift. each
    // entry must have a corresponding module in `src/patches/` with tests
    vec![
        Box::new(patches::mapping::MappingKeyCovariance),
        Box::new(patches::container_overlapping::ContainerMembershipOverlapping),
    ]
}

/// registry of every post-conversion patch, applied in pass 3 after the pep 695
/// conversion. these see the final idiomatic `.byi` form — pep 695 headers with
/// explicit variance, `dynamic`, `final class`, and so on — and rewrite it into
/// still-more-idiomatic basedpython (deleting mypy-only overloads and comments,
/// unwrapping `Literal`/`Final`/`Callable`, tightening variance and bounds, ...)
///
/// each post-patch is applied on its own re-parse so two patches never have to
/// coordinate disjoint edits; they run in declared order
pub fn all_post_patches() -> Vec<Box<dyn Patch>> {
    vec![
        // widens invariant list/set/dict output typevars over the explicit-variance
        // form; runs first so later idiom patches (e.g. `any_to_dynamic`) still see
        // and normalise anything it introduces
        Box::new(patches::output_widening::OutputWidening),
        Box::new(patches::cleanup::StripIgnoreComments),
        Box::new(patches::cleanup::BodylessStubs),
        Box::new(patches::dead_symbols::DeleteDeadSymbols),
        Box::new(patches::redundant_overloads::DeleteRedundantOverloads),
        Box::new(patches::builtins_tweaks::FrozendictCovariant),
        Box::new(patches::builtins_tweaks::TypeDictProxyCovariant),
        Box::new(patches::builtins_tweaks::HashableKeyBound),
        Box::new(patches::protocol_keyword::ProtocolKeyword),
        Box::new(patches::property_to_let::PropertyToLet),
        Box::new(patches::stray_comments::DeleteStrayComments),
        Box::new(patches::dead_typevars::DeleteDeadTypevars),
        Box::new(patches::final_modifier::FinalModifier),
        Box::new(patches::final_annotation::FinalAnnotation),
        Box::new(patches::init_shorthand::InitShorthand),
        Box::new(patches::context_manager_abstract::ContextManagerAbstractEnter),
        Box::new(patches::arrow_callable::ArrowCallable),
        Box::new(patches::literal_unwrap::UnwrapLiteral),
        Box::new(patches::type_aliases::TypeAliasStatements),
        Box::new(patches::homogeneous_tuple::HomogeneousTuple),
        Box::new(patches::any_to_dynamic::AnyToDynamic),
        Box::new(patches::strip_typing_imports::StripTypingImports),
    ]
}

/// dotted module name for a typeshed file path relative to `stdlib/`, e.g.
/// `typing.byi` -> `typing`, `os/path.byi` -> `os.path`,
/// `asyncio/__init__.byi` -> `asyncio`
pub(crate) fn module_qualname(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let mut parts: Vec<&str> = path
        .parent()
        .into_iter()
        .flat_map(Path::components)
        .filter_map(|component| component.as_os_str().to_str())
        .collect();
    if stem != "__init__" {
        parts.push(stem);
    }
    Some(parts.join("."))
}

/// deletion edit covering `range` (a statement, decorators included) plus the
/// run of comment lines immediately above it and the trailing newline. stops at
/// the first blank line or code line so an unrelated section header is spared.
/// `range` must begin at the statement's own line (offsets are extended to line
/// bounds, so a mid-line start would swallow preceding code)
pub(crate) fn delete_with_leading_comments(range: TextRange, source: &str) -> Edit {
    let bytes = source.as_bytes();

    // start of the statement's own line (decorators are inside `range`, so this
    // is the `@` or `class`/`def` line)
    let mut start = range.start().to_usize();
    while start > 0 && bytes[start - 1] != b'\n' {
        start -= 1;
    }
    // walk upward over whole comment lines that abut the statement
    while start > 0 {
        // `start` sits just after a newline; find the bounds of the line before it
        let line_end = start - 1; // the '\n'
        let mut line_start = line_end;
        while line_start > 0 && bytes[line_start - 1] != b'\n' {
            line_start -= 1;
        }
        if source[line_start..line_end].trim_start().starts_with('#') {
            start = line_start;
        } else {
            break;
        }
    }

    // extend past the statement's trailing newline
    let mut end = range.end().to_usize();
    while end < bytes.len() && bytes[end] != b'\n' {
        end += 1;
    }
    if end < bytes.len() {
        end += 1;
    }
    Edit {
        start,
        end,
        replacement: String::new(),
    }
}

/// apply `edits` to `source`, returning the new text. edits must be disjoint;
/// applied in reverse start order so earlier offsets remain valid
pub fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut out = source.to_string();
    let mut last_start = usize::MAX;
    for edit in edits {
        assert!(
            edit.end <= last_start,
            "overlapping edits: {edit:?} overlaps prior at {last_start}"
        );
        last_start = edit.start;
        out.replace_range(edit.start..edit.end, &edit.replacement);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_edits_disjoint() {
        let src = "hello world";
        let edits = vec![
            Edit {
                start: 0,
                end: 5,
                replacement: "HI".into(),
            },
            Edit {
                start: 6,
                end: 11,
                replacement: "THERE".into(),
            },
        ];
        assert_eq!(apply_edits(src, edits), "HI THERE");
    }

    #[test]
    fn apply_edits_empty() {
        assert_eq!(apply_edits("unchanged", vec![]), "unchanged");
    }
}
