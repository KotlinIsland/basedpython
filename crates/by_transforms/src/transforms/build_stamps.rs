//! AST pass: a `build:` block is the program's build stamps — values settled
//! when the artifact was produced rather than read at startup.
//!
//! the block parses to a `ClassDef` carrying a synthetic `build_def` marker,
//! whose members are annotation-only. this pass fills each one in from
//! [`Config::stamps`] and emits an ordinary class, so `build.GIT_SHA` needs no
//! runtime support at all:
//!
//! ```text
//! build:                      class build:
//!     GIT_SHA: str        ->      GIT_SHA: str = "e6f9ac1d"
//!     PORT: int = 8000            PORT: int = 8000
//! ```
//!
//! the values come in through the config because the pipeline must never go
//! looking for them itself. asking git here would make the emitted python a
//! function of the working tree as well as the source, and a re-stage — which
//! re-transpiles one file into a tree an earlier build wrote — would quietly
//! disagree with the rest of that tree about what commit it is
//!
//! a stamp with no supplied value falls back to the default written in the
//! block. one with neither is a hard error: declaring a stamp without a default
//! is precisely the claim that the build has to supply it, and that claim is the
//! whole reason to write it down
//!
//! there is deliberately no reverse transform. what this emits is an ordinary
//! class, indistinguishable from one somebody wrote by hand, and turning a class
//! named `build` back into a block would throw away the values it holds — which
//! for the one class where the values *are* the point is the worst direction to
//! be lossy in
//!
//! [`Config::stamps`]: crate::Config

use std::collections::BTreeMap;
use std::fmt::Write as _;

use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_python_ast::{Expr, ModModule, Stmt, StmtClassDef};
use ruff_text_size::{Ranged, TextRange};

use super::ast_driver::{AstPass, PassContext};
use super::source_util::python_string_literal;

/// the annotations a stamp may be declared with.
///
/// a stamp arrives from the build as text — a commit hash, the output of `git
/// status`, a number a CI job counted — so the set is the types that text has
/// one obvious reading as. anything else would need a convention about how the
/// string becomes the value, and inventing one silently is worse than saying the
/// annotation is not supported
#[derive(Clone, Copy)]
enum StampType {
    Str,
    Int,
    Bool,
}

impl StampType {
    fn from_annotation(annotation: &Expr) -> Option<Self> {
        match annotation.as_name_expr()?.id.as_str() {
            "str" => Some(Self::Str),
            "int" => Some(Self::Int),
            "bool" => Some(Self::Bool),
            _ => None,
        }
    }

    fn spelled(self) -> &'static str {
        match self {
            Self::Str => "str",
            Self::Int => "int",
            Self::Bool => "bool",
        }
    }

    /// the python literal `value` stands for, or `None` when the text the build
    /// supplied is not one of this type at all
    fn literal(self, value: &str) -> Option<String> {
        match self {
            Self::Str => Some(python_string_literal(value)),
            // rendered back from the parsed number rather than passed through:
            // `007` and `+3` are values a shell hands over quite naturally, and
            // neither is a python integer literal
            Self::Int => value.parse::<i64>().ok().map(|parsed| parsed.to_string()),
            Self::Bool => match value.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => Some("True".to_owned()),
                "false" | "0" | "no" | "off" | "" => Some("False".to_owned()),
                _ => None,
            },
        }
    }
}

pub(crate) struct BuildStampsPass<'a> {
    source: &'a str,
    stamps: BTreeMap<String, String>,
}

impl<'a> BuildStampsPass<'a> {
    pub(crate) fn new(source: &'a str, stamps: BTreeMap<String, String>) -> Self {
        Self { source, stamps }
    }

    /// the lowered class, or the reasons it could not be lowered
    fn lower(&self, class: &StmtClassDef) -> Result<String, Vec<String>> {
        let mut rendered = String::from("class build:");
        let mut errors = Vec::new();

        for statement in &class.body {
            match statement {
                Stmt::AnnAssign(declaration) => {
                    let Some(target) = declaration.target.as_name_expr() else {
                        errors
                            .push("a `build` stamp is a plain name with an annotation".to_owned());
                        continue;
                    };
                    let name = target.id.as_str();

                    let Some(stamp_type) = StampType::from_annotation(&declaration.annotation)
                    else {
                        errors.push(format!(
                            "the stamp `{name}` is annotated `{}`, which the build has no way to \
                             supply — a stamp reaches the program as text, so it must be \
                             annotated `str`, `int` or `bool`",
                            self.text(declaration.annotation.range())
                        ));
                        continue;
                    };

                    let value = match self.stamps.get(name) {
                        Some(supplied) => match stamp_type.literal(supplied) {
                            Some(literal) => literal,
                            None => {
                                errors.push(format!(
                                    "the stamp `{name}` is declared `{}`, but the build supplied \
                                     `{supplied}`, which is not one",
                                    stamp_type.spelled()
                                ));
                                continue;
                            }
                        },
                        // no value: the default stands in, and its source is
                        // carried across as written
                        None => match &declaration.value {
                            Some(default) => self.text(default.range()).to_owned(),
                            None => {
                                errors.push(format!(
                                    "the build supplied no value for the stamp `{name}`, and it \
                                     has no default"
                                ));
                                continue;
                            }
                        },
                    };

                    let _ = write!(rendered, "\n    {name}: {} = {value}", stamp_type.spelled());
                }
                // a docstring describes the block and belongs to the class it
                // becomes
                Stmt::Expr(expression) if expression.value.is_string_literal_expr() => {
                    let _ = write!(rendered, "\n    {}", self.text(expression.range()));
                }
                other => errors.push(format!(
                    "a `build` block holds stamp declarations and nothing else, but this is \
                     `{}`",
                    self.text(other.range()).lines().next().unwrap_or_default()
                )),
            }
        }

        if errors.is_empty() {
            Ok(rendered)
        } else {
            Err(errors)
        }
    }

    fn text(&self, range: TextRange) -> &str {
        self.source
            .get(usize::from(range.start())..usize::from(range.end()))
            .unwrap_or_default()
    }
}

impl AstPass for BuildStampsPass<'_> {
    fn run(&self, module: &mut ModModule, ctx: &mut PassContext) {
        let mut seen = false;
        for statement in &module.body {
            if let Stmt::ClassDef(class) = statement
                && class.is_build_stamps()
            {
                // both lower to `class build`, so the second would shadow the
                // first and every stamp the first declared would quietly stop
                // being there
                if seen {
                    ctx.errors.push(
                        "a module declares its build stamps once, and this is a second `build` \
                         block — the stamps of both belong in one"
                            .to_owned(),
                    );
                    continue;
                }
                seen = true;
                match self.lower(class) {
                    Ok(rendered) => ctx.text_edits.push((class.range(), rendered)),
                    Err(errors) => ctx.errors.extend(errors),
                }
            }
        }

        // a block anywhere but the module body describes the same one program's
        // build, from a place its readers cannot see. rather than lower it into
        // a class nobody can reach, say so
        let mut nested = Nested { depth: 0, found: 0 };
        for statement in &module.body {
            nested.visit_stmt(statement);
        }
        for _ in 0..nested.found {
            ctx.errors.push(
                "a `build` block declares the whole program's stamps, so it belongs at the top \
                 level of a module"
                    .to_owned(),
            );
        }
    }
}

/// finds `build:` blocks below the module body
struct Nested {
    depth: usize,
    found: usize,
}

impl<'ast> Visitor<'ast> for Nested {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if self.depth > 0
            && let Stmt::ClassDef(class) = stmt
            && class.is_build_stamps()
        {
            self.found += 1;
        }
        self.depth += 1;
        walk_stmt(self, stmt);
        self.depth -= 1;
    }
}

#[cfg(test)]
mod tests {
    use crate::{Config, transpile};

    fn with_stamps(source: &str, stamps: &[(&str, &str)]) -> Result<String, String> {
        let mut config = Config::test_default();
        config.stamps = stamps
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        transpile(source, &config)
    }

    #[test]
    fn a_supplied_stamp_becomes_a_literal() {
        let out = with_stamps("build:\n    GIT_SHA: str\n", &[("GIT_SHA", "e6f9ac1")]).unwrap();
        assert!(
            out.contains("class build:") && out.contains("GIT_SHA: str = \"e6f9ac1\""),
            "{out}"
        );
    }

    #[test]
    fn a_supplied_value_beats_the_default() {
        let out = with_stamps(
            "build:\n    VERSION: str = \"0.0.0+dev\"\n",
            &[("VERSION", "1.4.0")],
        )
        .unwrap();
        assert!(out.contains("VERSION: str = \"1.4.0\""), "{out}");
    }

    #[test]
    fn an_unsupplied_stamp_falls_back_to_its_default() {
        let out = with_stamps("build:\n    VERSION: str = \"0.0.0+dev\"\n", &[]).unwrap();
        assert!(out.contains("VERSION: str = \"0.0.0+dev\""), "{out}");
    }

    #[test]
    fn an_unsupplied_stamp_with_no_default_is_an_error() {
        let error = with_stamps("build:\n    GIT_SHA: str\n", &[]).unwrap_err();
        assert!(
            error.contains("supplied no value for the stamp `GIT_SHA`"),
            "{error}"
        );
    }

    #[test]
    fn a_bool_stamp_reads_the_spellings_a_shell_produces() {
        for (supplied, expected) in [
            ("true", "True"),
            ("1", "True"),
            ("false", "False"),
            ("0", "False"),
            ("", "False"),
        ] {
            let out = with_stamps("build:\n    DIRTY: bool\n", &[("DIRTY", supplied)]).unwrap();
            assert!(
                out.contains(&format!("DIRTY: bool = {expected}")),
                "{supplied:?} -> {out}"
            );
        }
    }

    #[test]
    fn an_int_stamp_is_rendered_from_its_value() {
        // `007` is a perfectly ordinary thing for a build system to hand over,
        // and a python literal it is not
        let out = with_stamps("build:\n    RUN: int\n", &[("RUN", "007")]).unwrap();
        assert!(out.contains("RUN: int = 7"), "{out}");
    }

    #[test]
    fn a_value_that_is_not_the_declared_type_is_an_error() {
        let error = with_stamps("build:\n    RUN: int\n", &[("RUN", "later")]).unwrap_err();
        assert!(
            error.contains("`RUN` is declared `int`, but the build supplied `later`"),
            "{error}"
        );
    }

    #[test]
    fn an_unsupported_annotation_is_an_error() {
        let error = with_stamps("build:\n    WHEN: float\n", &[("WHEN", "1.0")]).unwrap_err();
        assert!(error.contains("annotated `float`"), "{error}");
    }

    #[test]
    fn a_statement_that_is_not_a_stamp_is_an_error() {
        let error = with_stamps("build:\n    def f(self): ...\n", &[]).unwrap_err();
        assert!(
            error.contains("holds stamp declarations and nothing else"),
            "{error}"
        );
    }

    #[test]
    fn a_docstring_is_carried_across() {
        let out = with_stamps(
            "build:\n    \"what this build was\"\n    V: str = \"x\"\n",
            &[],
        )
        .unwrap();
        assert!(out.contains("\"what this build was\""), "{out}");
    }

    #[test]
    fn a_second_block_is_an_error() {
        let error = with_stamps(
            "build:\n    A: str = \"a\"\n\nbuild:\n    B: str = \"b\"\n",
            &[],
        )
        .unwrap_err();
        assert!(error.contains("second `build` block"), "{error}");
    }

    #[test]
    fn a_nested_block_is_an_error() {
        let error = with_stamps("def f():\n    build:\n        V: str = \"x\"\n", &[]).unwrap_err();
        assert!(error.contains("belongs at the top level"), "{error}");
    }
}
