//! `type _X = …` → `private type X = …`
//!
//! which aliases are safe to convert is decided by the whole-tree scan in the
//! crate-internal `private_names` module; this patch only performs the rewrite:
//! a `private ` insertion plus a whole-file rename of the alias. an
//! already-converted alias is no longer spelled `_X`, so re-running is a no-op

use std::collections::BTreeSet;
use std::path::Path;

use ruff_python_ast::{ModModule, Stmt};
use ruff_python_parser::Parsed;
use ruff_text_size::Ranged;

use crate::patches::private_names::{Convertible, strip_underscore_edits};
use crate::{Edit, Patch};

pub struct PrivateTypeAliases {
    convertible: Convertible,
}

impl PrivateTypeAliases {
    pub(crate) fn new(convertible: Convertible) -> Self {
        Self { convertible }
    }
}

impl Patch for PrivateTypeAliases {
    fn name(&self) -> &'static str {
        "private-type-aliases"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        // the patch is keyed on shape, not on named symbols; drift in any one
        // module is handled by the whole-tree scan rather than a fixed list
        &[]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(convertible) = self.convertible.get(module_path) else {
            return Vec::new();
        };

        let mut converted: BTreeSet<&str> = BTreeSet::new();
        let mut edits = Vec::new();
        for stmt in &parsed.syntax().body {
            if let Stmt::TypeAlias(alias) = stmt
                && !alias.is_private
                && let ruff_python_ast::Expr::Name(name) = alias.name.as_ref()
                && convertible.contains(name.id.as_str())
            {
                converted.insert(name.id.as_str());
                let start = alias.range().start().to_usize();
                edits.push(Edit {
                    start,
                    end: start,
                    replacement: "private ".to_string(),
                });
            }
        }
        if converted.is_empty() {
            return edits;
        }

        edits.extend(strip_underscore_edits(source, &converted));
        edits
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use super::*;
    use crate::apply_edits;

    fn patch_for(file: &str, names: &[&str]) -> PrivateTypeAliases {
        let mut convertible = Convertible::new();
        convertible.insert(
            PathBuf::from(file),
            names.iter().map(|n| (*n).to_string()).collect(),
        );
        PrivateTypeAliases::new(convertible)
    }

    fn run(patch: &PrivateTypeAliases, file: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = patch.rewrite(Path::new(file), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_and_renames_references() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "\
type _Key = str | int

def get(k: _Key) -> int: ...
def put(k: _Key, v: int) -> None: ...
";
        assert_eq!(
            run(&patch, "m.byi", src),
            "\
private type Key = str | int

def get(k: Key) -> int: ...
def put(k: Key, v: int) -> None: ...
"
        );
    }

    #[test]
    fn idempotent() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: _Key) -> int: ...\n";
        let once = run(&patch, "m.byi", src);
        assert_eq!(run(&patch, "m.byi", &once), once);
    }

    #[test]
    fn renames_inside_string_annotations() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: \"_Key\") -> int: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\n\ndef get(k: \"Key\") -> int: ...\n"
        );
    }

    #[test]
    fn leaves_unlisted_aliases_alone() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\ntype _Other = int\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\ntype _Other = int\n"
        );
    }

    #[test]
    fn ignores_files_with_nothing_to_convert() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n";
        assert_eq!(run(&patch, "other.byi", src), src);
    }

    #[test]
    fn does_not_touch_similarly_named_identifiers() {
        let patch = patch_for("m.byi", &["_Key"]);
        let src = "type _Key = str\n\ndef get(k: _Key, j: _KeyPair, i: __Key) -> int: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Key = str\n\ndef get(k: Key, j: _KeyPair, i: __Key) -> int: ...\n"
        );
    }

    #[test]
    fn generic_alias() {
        let patch = patch_for("m.byi", &["_Pair"]);
        let src = "type _Pair[T] = tuple[T, T]\n\ndef f(p: _Pair[int]) -> None: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private type Pair[T] = tuple[T, T]\n\ndef f(p: Pair[int]) -> None: ...\n"
        );
    }
}
