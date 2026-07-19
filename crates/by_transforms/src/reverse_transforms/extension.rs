//! reverse of `crate::transforms::extension`:
//!   `def __by_ext__list__second(self): …  # basedpython: extension method list`
//!   → `extension list:` block, and `__by_ext__list__second(xs)` → `xs.second()`
//!
//! the forward lowering tags each backing function's header line with a
//! `# basedpython: extension <kind> <header>` marker — provenance carrying the
//! member kind (method / property / static / classmethod) and the original
//! extension header, bracket bounds included. only marked functions re-sugar;
//! backing-shaped functions written by hand (no marker) are ordinary python.
//! call sites re-sugar only when the backing function is defined (and marked)
//! in the same file — a cross-module import of a backing function stays as the
//! explicit call, conservatively

use std::collections::HashMap;

use ruff_diagnostics::{Edit, Fix};
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, Stmt, StmtFunctionDef};
use ruff_text_size::{Ranged, TextRange};
use ty_python_semantic::ExtensionMemberKind;

use crate::transforms::extension::{EXTENSION_MARKER, parse_kind_word};
use crate::transforms::source_util::line_start;

/// a backing function recognised by its marker, keyed for call re-sugaring
struct BackingFn {
    member: String,
    kind: ExtensionMemberKind,
    /// the extension target's name (`list` from a `list[Element: int]` header)
    target: String,
}

pub(crate) struct ExtensionReverse<'src> {
    source: &'src str,
    functions: HashMap<String, BackingFn>,
    pub(crate) edits: Vec<Fix>,
}

/// the `<kind> <header>` payload of a marker comment found inside `span`
fn marker_payload(source: &str, span: TextRange) -> Option<(&str, &str)> {
    let text = &source[usize::from(span.start())..usize::from(span.end())];
    let after = text.split_once(EXTENSION_MARKER)?.1;
    let line = after.split('\n').next().unwrap_or(after).trim();
    line.split_once(' ')
}

/// the member name of `name` under the mangle for `target`:
/// `__by_ext__list__second` / `__by_ext2__list__second` → `second`
fn member_of(name: &str, target: &str) -> Option<String> {
    let rest = name.strip_prefix("__by_ext")?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
    let rest = rest.strip_prefix("__")?;
    let rest = rest.strip_prefix(target)?;
    let member = rest.strip_prefix("__")?;
    (!member.is_empty()).then(|| member.to_owned())
}

impl<'src> ExtensionReverse<'src> {
    pub(crate) fn new(source: &'src str, suite: &[Stmt]) -> Self {
        let mut reverse = Self {
            source,
            functions: HashMap::new(),
            edits: Vec::new(),
        };
        reverse.resugar_blocks(suite);
        reverse
    }

    /// a top-level function's marker data, when it is a recognisable backing
    /// function: (kind, header, target, member)
    fn backing_marker(
        &self,
        func: &StmtFunctionDef,
    ) -> Option<(ExtensionMemberKind, String, String, String)> {
        let (kind_word, header) = marker_payload(self.source, func.range())?;
        let kind = parse_kind_word(kind_word)?;
        let header = header.to_owned();
        let target = header
            .split_once('[')
            .map_or(header.as_str(), |(target, _)| target)
            .trim()
            .to_owned();
        let member = member_of(func.name.as_str(), &target)?;
        Some((kind, header, target, member))
    }

    /// group consecutive top-level backing functions with the same header into
    /// one `extension <header>:` block, and register every backing function
    /// for the call-site re-sugar
    fn resugar_blocks(&mut self, suite: &[Stmt]) {
        let mut index = 0;
        while index < suite.len() {
            let Stmt::FunctionDef(func) = &suite[index] else {
                index += 1;
                continue;
            };
            let Some((kind, header, target, member)) = self.backing_marker(func) else {
                index += 1;
                continue;
            };

            let mut group = vec![(func, kind, member.clone())];
            self.functions.insert(
                func.name.to_string(),
                BackingFn {
                    member,
                    kind,
                    target: target.clone(),
                },
            );
            let mut end = index + 1;
            while let Some(Stmt::FunctionDef(next)) = suite.get(end) {
                let Some((next_kind, next_header, next_target, next_member)) =
                    self.backing_marker(next)
                else {
                    break;
                };
                if next_header != header {
                    break;
                }
                self.functions.insert(
                    next.name.to_string(),
                    BackingFn {
                        member: next_member.clone(),
                        kind: next_kind,
                        target: next_target,
                    },
                );
                group.push((next, next_kind, next_member));
                end += 1;
            }

            let start = line_start(
                self.source,
                group[0]
                    .0
                    .decorator_list
                    .first()
                    .map_or(group[0].0.range().start(), |dec| dec.range().start()),
            );
            let span = TextRange::new(start, group[end - index - 1].0.range().end());
            let mut replacement = format!("extension {header}:\n");
            for (position, (func, kind, member)) in group.iter().enumerate() {
                if position > 0 {
                    replacement.push('\n');
                }
                replacement.push_str(&self.resugar_member(func, *kind, member));
            }
            self.edits
                .push(Fix::safe_edit(Edit::range_replacement(replacement, span)));
            index = end;
        }
    }

    /// one member of the block: the backing function's own source, marker
    /// stripped, renamed to the surface member name, indented one level, and
    /// re-spelled by kind (`@property`, `static def`, `class def`)
    fn resugar_member(
        &self,
        func: &StmtFunctionDef,
        kind: ExtensionMemberKind,
        member: &str,
    ) -> String {
        let start = line_start(
            self.source,
            func.decorator_list
                .first()
                .map_or(func.range().start(), |dec| dec.range().start()),
        );
        let text = &self.source[usize::from(start)..usize::from(func.range().end())];

        let mut result = String::new();
        for (position, line) in text.split('\n').enumerate() {
            if position > 0 {
                result.push('\n');
            }
            // strip the marker comment (and the spacing before it)
            let line = line
                .split_once(EXTENSION_MARKER)
                .map_or(line, |(before, _)| before.trim_end());
            if line.is_empty() {
                continue;
            }
            result.push_str("    ");
            result.push_str(line);
        }

        // rename the mangled def to the surface member name
        let result = result.replacen(
            &format!("def {}", func.name.as_str()),
            &format!("def {member}"),
            1,
        );

        match kind {
            ExtensionMemberKind::Method => result,
            ExtensionMemberKind::Property => format!("    @property\n{result}"),
            ExtensionMemberKind::StaticMethod => result.replacen("    def ", "    static def ", 1),
            ExtensionMemberKind::ClassMethod => result.replacen("    def ", "    class def ", 1),
        }
    }

    /// receivers re-sugared from an argument keep working as a postfix base;
    /// anything lower-precedence gets wrapped
    fn receiver_source(&self, receiver: &Expr) -> String {
        let text = self.source
            [usize::from(receiver.range().start())..usize::from(receiver.range().end())]
            .to_owned();
        if matches!(
            receiver,
            Expr::Name(_)
                | Expr::Attribute(_)
                | Expr::Subscript(_)
                | Expr::Call(_)
                | Expr::StringLiteral(_)
                | Expr::List(_)
                | Expr::Dict(_)
                | Expr::Set(_)
                | Expr::Tuple(_)
        ) {
            text
        } else {
            format!("({text})")
        }
    }

    fn resugar_call(&mut self, call: &ruff_python_ast::ExprCall) {
        // `functools.partial(__by_ext__list__second, xs)` → `xs.second`
        if let Expr::Attribute(attr) = call.func.as_ref()
            && attr.attr.as_str() == "partial"
            && matches!(attr.value.as_ref(), Expr::Name(n) if n.id.as_str() == "functools")
            && let [Expr::Name(backing), receiver] = call.arguments.args.as_ref()
            && call.arguments.keywords.is_empty()
            && let Some(function) = self.functions.get(backing.id.as_str())
            && matches!(
                function.kind,
                ExtensionMemberKind::Method | ExtensionMemberKind::ClassMethod
            )
        {
            let replacement = format!("{}.{}", self.receiver_source(receiver), function.member);
            self.edits.push(Fix::safe_edit(Edit::range_replacement(
                replacement,
                call.range(),
            )));
            return;
        }

        let Expr::Name(name) = call.func.as_ref() else {
            return;
        };
        let Some(function) = self.functions.get(name.id.as_str()) else {
            return;
        };
        let rest = |args: &[Expr]| -> String {
            let spans: Vec<&str> = args
                .iter()
                .map(|arg| {
                    &self.source[usize::from(arg.range().start())..usize::from(arg.range().end())]
                })
                .chain(call.arguments.keywords.iter().map(|kw| {
                    &self.source[usize::from(kw.range().start())..usize::from(kw.range().end())]
                }))
                .collect();
            spans.join(", ")
        };
        let replacement = match function.kind {
            ExtensionMemberKind::Property => {
                let [receiver] = call.arguments.args.as_ref() else {
                    return;
                };
                if !call.arguments.keywords.is_empty() {
                    return;
                }
                format!("{}.{}", self.receiver_source(receiver), function.member)
            }
            ExtensionMemberKind::Method | ExtensionMemberKind::ClassMethod => {
                let Some((receiver, rest_args)) = call.arguments.args.split_first() else {
                    return;
                };
                format!(
                    "{}.{}({})",
                    self.receiver_source(receiver),
                    function.member,
                    rest(rest_args)
                )
            }
            ExtensionMemberKind::StaticMethod => {
                format!(
                    "{}.{}({})",
                    function.target,
                    function.member,
                    rest(&call.arguments.args)
                )
            }
        };
        self.edits.push(Fix::safe_edit(Edit::range_replacement(
            replacement,
            call.range(),
        )));
    }
}

impl<'ast> Visitor<'ast> for ExtensionReverse<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.resugar_call(call);
        }
        ruff_python_ast::visitor::walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, reverse_transpile, transpile};

    fn rev(source: &str) -> String {
        reverse_transpile(source, &Config::test_default()).unwrap()
    }

    #[test]
    fn backing_functions_resugar_to_an_extension_block() {
        let src = "def __by_ext__list__second(self):  # basedpython: extension method list\n    return self[1]\n\nxs = [1, 2]\nprint(__by_ext__list__second(xs))\n";
        let out = rev(src);
        assert!(out.contains("extension list:"), "got:\n{out}");
        assert!(out.contains("    def second(self):"), "got:\n{out}");
        assert!(out.contains("print(xs.second())"), "got:\n{out}");
        assert!(!out.contains("__by_ext__"), "got:\n{out}");
    }

    #[test]
    fn consecutive_members_share_one_block() {
        let src = "def __by_ext__list__second(self):  # basedpython: extension method list\n    return self[1]\n\ndef __by_ext__list__third(self):  # basedpython: extension method list\n    return self[2]\n";
        let out = rev(src);
        assert_eq!(out.matches("extension list:").count(), 1, "got:\n{out}");
        assert!(out.contains("    def second(self):"), "got:\n{out}");
        assert!(out.contains("    def third(self):"), "got:\n{out}");
    }

    #[test]
    fn property_resugars_with_decorator_and_bare_access() {
        let src = "def __by_ext__str__shouty(self):  # basedpython: extension property str\n    return self.upper()\n\nname = \"hi\"\nprint(__by_ext__str__shouty(name))\n";
        let out = rev(src);
        assert!(out.contains("extension str:"), "got:\n{out}");
        assert!(
            out.contains("    @property\n    def shouty(self):"),
            "got:\n{out}"
        );
        assert!(out.contains("print(name.shouty)"), "got:\n{out}");
    }

    #[test]
    fn bounds_come_back_from_the_marker() {
        let src = "def __by_ext__list__total(self):  # basedpython: extension method list[Element: int]\n    return sum(self)\n";
        let out = rev(src);
        assert!(out.contains("extension list[Element: int]:"), "got:\n{out}");
    }

    #[test]
    fn partial_reference_resugars_to_bare_attribute() {
        let src = "import functools\n\ndef __by_ext__list__second(self):  # basedpython: extension method list\n    return self[1]\n\nxs = [1, 2]\nf = functools.partial(__by_ext__list__second, xs)\n";
        let out = rev(src);
        assert!(out.contains("f = xs.second"), "got:\n{out}");
    }

    #[test]
    fn unmarked_backing_shaped_function_is_ordinary_python() {
        let src = "def __by_ext__list__second(self):\n    return self[1]\n\nprint(__by_ext__list__second([1, 2]))\n";
        let out = rev(src);
        assert!(!out.contains("extension"), "got:\n{out}");
        assert!(
            out.contains("print(__by_ext__list__second([1, 2]))"),
            "got:\n{out}"
        );
    }

    #[test]
    fn round_trip_reproduces_the_lowering() {
        // reverse then forward reproduces the same *AST* (the documented
        // round-trip contract) — indentation inside re-sugared bodies may
        // legitimately differ
        let by = "extension list:\n    def second(self) -> Element:\n        return self[1]\n\nxs = [1, 2, 3]\nprint(xs.second())\n";
        let lowered = transpile(by, &Config::test_default()).unwrap();
        let resugared = rev(&lowered);
        let relowered = transpile(&resugared, &Config::test_default()).unwrap();
        let parse = |source: &str| {
            ruff_python_parser::parse_module(source)
                .expect("round-trip output should parse")
                .into_syntax()
        };
        assert_eq!(
            ruff_python_ast::comparable::ComparableMod::from(&ruff_python_ast::Mod::Module(parse(
                &lowered
            ))),
            ruff_python_ast::comparable::ComparableMod::from(&ruff_python_ast::Mod::Module(parse(
                &relowered
            ))),
            "lowered:\n{lowered}\nrelowered:\n{relowered}"
        );
    }
}
