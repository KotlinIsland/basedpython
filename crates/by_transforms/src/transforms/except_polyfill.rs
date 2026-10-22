//! Lowers the bracketless `except` of PEP 758 for a target before python 3.14.
//!
//! python 3.14 lets a handler that catches several types leave out the parentheses
//! around them, as long as it binds nothing with `as`. before that, the types have to
//! be written as a parenthesized tuple, which is what the handler means either way:
//!
//! ```text
//! except TimeoutError, ConnectionError:      except (TimeoutError, ConnectionError):
//! ```
//!
//! the same goes for `except*`. like the `match` lowering, this runs over the finished
//! python, so a handler another lowering writes is lowered by the same code as one the
//! author wrote. only the parentheses are added, so every line keeps its place

use ruff_python_ast::visitor::{Visitor, walk_except_handler};
use ruff_python_ast::{ExceptHandler, Expr, PySourceType, PythonVersion};
use ruff_text_size::{Ranged, TextSize};

use crate::Config;

pub(crate) fn lower(source: String, config: &Config) -> String {
    if config.min_version >= PythonVersion::PY314 {
        return source;
    }
    let parsed = ruff_python_parser::parse_unchecked_source(&source, PySourceType::Python);
    let mut bracketless = Bracketless(Vec::new());
    bracketless.visit_body(parsed.suite());
    if bracketless.0.is_empty() {
        return source;
    }

    let mut insertions: Vec<(TextSize, char)> = bracketless
        .0
        .into_iter()
        .flat_map(|(start, end)| [(start, '('), (end, ')')])
        .collect();
    insertions.sort_by_key(|(at, _)| *at);
    let mut out = String::with_capacity(source.len() + insertions.len());
    let mut at = 0usize;
    for (position, paren) in insertions {
        let position = usize::from(position);
        out.push_str(&source[at..position]);
        out.push(paren);
        at = position;
    }
    out.push_str(&source[at..]);
    out
}

/// the start and end of the types of every handler that writes them without parentheses
struct Bracketless(Vec<(TextSize, TextSize)>);

impl<'a> Visitor<'a> for Bracketless {
    fn visit_except_handler(&mut self, handler: &'a ExceptHandler) {
        let ExceptHandler::ExceptHandler(handler_node) = handler;
        if let Some(Expr::Tuple(types)) = handler_node.type_.as_deref()
            && !types.parenthesized
        {
            self.0.push((types.start(), types.end()));
        }
        walk_except_handler(self, handler);
    }
}

#[cfg(test)]
mod tests {
    use indoc::indoc;
    use ruff_python_ast::PythonVersion;

    use crate::{Config, transpile};

    fn at(min_version: PythonVersion, source: &str) -> String {
        let config = Config {
            min_version,
            ..Config::test_default()
        };
        transpile(source, &config).unwrap()
    }

    #[test]
    fn parenthesizes_the_types_before_3_14() {
        let out = at(
            PythonVersion::PY313,
            indoc! {"
                try:
                    pass
                except ValueError, TypeError:
                    pass
            "},
        );
        assert!(
            out.contains("except (ValueError, TypeError):\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn parenthesizes_an_except_star_before_3_14() {
        let out = at(
            PythonVersion::PY311,
            indoc! {"
                try:
                    pass
                except* ValueError, TypeError:
                    pass
            "},
        );
        assert!(
            out.contains("except* (ValueError, TypeError):\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn leaves_a_nested_handler_and_a_parenthesized_one_in_place() {
        let out = at(
            PythonVersion::PY310,
            indoc! {"
                try:
                    pass
                except (KeyError, IndexError):
                    try:
                        pass
                    except ValueError, TypeError:
                        pass
            "},
        );
        assert!(
            out.contains("except (KeyError, IndexError):\n"),
            "got:\n{out}"
        );
        assert!(
            out.contains("    except (ValueError, TypeError):\n"),
            "got:\n{out}"
        );
    }

    #[test]
    fn keeps_the_bracketless_form_from_3_14() {
        let out = at(
            PythonVersion::PY314,
            indoc! {"
                try:
                    pass
                except ValueError, TypeError:
                    pass
            "},
        );
        assert!(
            out.contains("except ValueError, TypeError:\n"),
            "got:\n{out}"
        );
    }
}
