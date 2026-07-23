//! `protocol _X` → `private protocol X`
//!
//! the counterpart of [`private_type_aliases`](super::private_type_aliases) for
//! the protocols typeshed hides behind a leading underscore. which ones are safe
//! to convert is decided by the whole-tree scan in the crate-internal
//! `private_names` module; this patch only performs the rewrite: a `private `
//! insertion before the `protocol` keyword plus a whole-file rename of the
//! protocol. a converted protocol is no longer spelled `_X`, so re-running is a
//! no-op

use std::collections::BTreeSet;
use std::path::Path;

use ruff_python_ast::ModModule;
use ruff_python_parser::Parsed;

use crate::patches::private_names::{Convertible, each_private_protocol, strip_underscore_edits};
use crate::{Edit, Patch};

pub struct PrivateProtocols {
    convertible: Convertible,
}

impl PrivateProtocols {
    pub(crate) fn new(convertible: Convertible) -> Self {
        Self { convertible }
    }
}

impl Patch for PrivateProtocols {
    fn name(&self) -> &'static str {
        "private-protocols"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &[]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let Some(convertible) = self.convertible.get(module_path) else {
            return Vec::new();
        };

        let mut converted: BTreeSet<&str> = BTreeSet::new();
        let mut edits = Vec::new();
        each_private_protocol(&parsed.syntax().body, source, &mut |class, keyword| {
            if !convertible.contains(class.name.as_str()) {
                return;
            }
            converted.insert(class.name.as_str());
            // the modifier sits immediately before the `protocol` introducer,
            // after any real decorators
            edits.push(Edit {
                start: keyword,
                end: keyword,
                replacement: "private ".to_string(),
            });
        });
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

    fn patch_for(file: &str, names: &[&str]) -> PrivateProtocols {
        let mut convertible = Convertible::new();
        convertible.insert(
            PathBuf::from(file),
            names.iter().map(|n| (*n).to_string()).collect(),
        );
        PrivateProtocols::new(convertible)
    }

    fn run(patch: &PrivateProtocols, file: &str, src: &str) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let edits = patch.rewrite(Path::new(file), &parsed, src);
        apply_edits(src, edits)
    }

    #[test]
    fn converts_and_renames_references() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "\
protocol _Reader:
    def read(self) -> str: ...

def load(r: _Reader) -> str: ...
";
        assert_eq!(
            run(&patch, "m.byi", src),
            "\
private protocol Reader:
    def read(self) -> str: ...

def load(r: Reader) -> str: ...
"
        );
    }

    #[test]
    fn idempotent() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "protocol _Reader: ...\n\ndef load(r: _Reader) -> str: ...\n";
        let once = run(&patch, "m.byi", src);
        assert_eq!(run(&patch, "m.byi", &once), once);
    }

    #[test]
    fn generic_protocol() {
        let patch = patch_for("m.byi", &["_SupportsRound1"]);
        let src = "\
protocol _SupportsRound1[out Element]:
    def __round__(self) -> Element: ...

def round[Element](number: _SupportsRound1[Element]) -> Element: ...
";
        assert_eq!(
            run(&patch, "m.byi", src),
            "\
private protocol SupportsRound1[out Element]:
    def __round__(self) -> Element: ...

def round[Element](number: SupportsRound1[Element]) -> Element: ...
"
        );
    }

    #[test]
    fn protocol_with_bases() {
        let patch = patch_for("m.byi", &["_Input"]);
        let src =
            "protocol _Input(SupportsRead[bytes], Sized): ...\n\ndef f(x: _Input) -> None: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private protocol Input(SupportsRead[bytes], Sized): ...\n\ndef f(x: Input) -> None: ...\n"
        );
    }

    #[test]
    fn indented_under_a_version_guard() {
        let patch = patch_for("m.byi", &["_Guarded"]);
        let src = "\
import sys

if sys.version_info >= (3, 12):
    protocol _Guarded: ...
    def f(x: _Guarded) -> None: ...
";
        assert_eq!(
            run(&patch, "m.byi", src),
            "\
import sys

if sys.version_info >= (3, 12):
    private protocol Guarded: ...
    def f(x: Guarded) -> None: ...
"
        );
    }

    #[test]
    fn keeps_a_real_decorator_ahead_of_the_modifier() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "@runtime_checkable\nprotocol _Reader: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "@runtime_checkable\nprivate protocol Reader: ...\n"
        );
    }

    #[test]
    fn renames_inside_string_annotations() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "protocol _Reader: ...\n\ndef f(x: \"_Reader\") -> None: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private protocol Reader: ...\n\ndef f(x: \"Reader\") -> None: ...\n"
        );
    }

    #[test]
    fn leaves_unlisted_and_non_protocol_declarations_alone() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "protocol _Reader: ...\nprotocol _Writer: ...\nclass _Plain: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private protocol Reader: ...\nprotocol _Writer: ...\nclass _Plain: ...\n"
        );
    }

    #[test]
    fn does_not_touch_similarly_named_identifiers() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "protocol _Reader: ...\n\ndef f(a: _Reader, b: _ReaderPair, c: __Reader) -> None: ...\n";
        assert_eq!(
            run(&patch, "m.byi", src),
            "private protocol Reader: ...\n\ndef f(a: Reader, b: _ReaderPair, c: __Reader) -> None: ...\n"
        );
    }

    #[test]
    fn ignores_files_with_nothing_to_convert() {
        let patch = patch_for("m.byi", &["_Reader"]);
        let src = "protocol _Reader: ...\n";
        assert_eq!(run(&patch, "other.byi", src), src);
    }
}
