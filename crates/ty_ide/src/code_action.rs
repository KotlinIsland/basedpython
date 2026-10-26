use crate::add_dependency;
use crate::add_dependency::AddDependency;
use crate::completion;
use crate::django_template::django_template_code_actions;

use ruff_db::system::SystemPathBuf;
use ruff_db::{files::File, parsed::parsed_module};
use ruff_diagnostics::Edit;
use ruff_python_ast::find_node::covering_node;
use ruff_text_size::TextRange;
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::lint::LintId;
use ty_python_semantic::types::{
    MISPLACED_DEPENDENCY, UNDECLARED_DEPENDENCY, UNRESOLVED_IMPORT, UNRESOLVED_REFERENCE,
};
use ty_python_semantic::{suppress_in_file, suppress_single};

/// A `QuickFix` Code Action
#[derive(Debug, Clone)]
pub struct QuickFix {
    pub title: String,
    pub edits: Vec<FileEdit>,
    pub preferred: bool,
    /// A file the action creates, for a fix to a reference to something that
    /// isn't there yet.
    pub create: Option<SystemPathBuf>,
    /// A dependency for the client to have `uv` add, for a fix that no edit can
    /// carry out on its own because it has to reach the environment.
    pub add_dependency: Option<AddDependency>,
    /// The fix is the one import that resolves an unresolved name: no other module
    /// has a member of that name, so nothing is left to choose. An editor may add an
    /// import like this one as the user types, without asking, as IDEs do for
    /// unambiguous imports.
    pub unambiguous_import: bool,
}

/// An edit and the file it applies to.
///
/// An action is offered against one file's diagnostic but does not have to change
/// that file: declaring a missing dependency edits `pyproject.toml`.
#[derive(Debug, Clone)]
pub struct FileEdit {
    pub file: File,
    pub edit: Edit,
}

impl QuickFix {
    fn new(title: String, file: File, edits: Vec<Edit>, preferred: bool) -> Self {
        Self {
            title,
            edits: edits
                .into_iter()
                .map(|edit| FileEdit { file, edit })
                .collect(),
            preferred,
            create: None,
            add_dependency: None,
            unambiguous_import: false,
        }
    }
}

/// The quick fixes offered for a diagnostic of `file`.
///
/// `template` says the file is a django template rather than python, which the
/// caller knows and this cannot: nothing about a template may be answered by
/// parsing it as python.
pub fn code_actions(
    db: &dyn Db,
    file: ProgramFile<'_>,
    diagnostic_range: TextRange,
    diagnostic_id: &str,
    template: bool,
) -> Vec<QuickFix> {
    let registry = db.lint_registry();
    let Ok(lint_id) = registry.get(diagnostic_id) else {
        return Vec::new();
    };

    if template {
        return django_template_code_actions(db, file.file(db), diagnostic_range, lint_id);
    }

    let mut actions = Vec::new();

    // Suggest imports/qualifications for unresolved references (often ideal)
    if lint_id == LintId::of(&UNRESOLVED_REFERENCE)
        && let Some(import_quick_fix) = unresolved_fixes(db, file, diagnostic_range)
    {
        actions.extend(import_quick_fix);
    }

    // An import of something the project does not depend on is fixed by
    // declaring it, which is an edit to a different file than the diagnostic's.
    if lint_id == LintId::of(&UNDECLARED_DEPENDENCY)
        || lint_id == LintId::of(&MISPLACED_DEPENDENCY)
        || lint_id == LintId::of(&UNRESOLVED_IMPORT)
    {
        actions.extend(add_dependency::code_actions(
            db,
            file.file(db),
            diagnostic_range,
        ));
    }

    // Suggest just suppressing the lint (always a valid option, but never ideal)
    actions.push(QuickFix::new(
        format!("Ignore '{}' for this line", lint_id.name()),
        file.file(db),
        suppress_single(db, file.python_file(db), lint_id, diagnostic_range).into_edits(),
        false,
    ));
    actions.push(QuickFix::new(
        format!("Ignore '{}' in this file", lint_id.name()),
        file.file(db),
        suppress_in_file(db, file.python_file(db), lint_id).into_edits(),
        false,
    ));

    actions
}

fn unresolved_fixes(
    db: &dyn Db,
    file: ProgramFile<'_>,
    diagnostic_range: TextRange,
) -> Option<impl Iterator<Item = QuickFix>> {
    let parsed = parsed_module(db, file.python_file(db)).load(db);
    let node = covering_node(parsed.syntax().into(), diagnostic_range).node();
    let symbol = &node.expr_name()?.id;
    let fixes = completion::unresolved_fixes(db, file, &parsed, symbol, node);
    // every qualification names a module one of the imports imports from, so a
    // single import is a single thing the name can mean
    let unambiguous = fixes.imports.len() == 1;
    let imports = fixes.imports.into_iter().map(move |import| QuickFix {
        unambiguous_import: unambiguous,
        ..QuickFix::new(import.label, file.file(db), vec![import.edit], true)
    });
    let qualifications = fixes
        .qualifications
        .into_iter()
        .map(move |import| QuickFix::new(import.label, file.file(db), vec![import.edit], true));
    Some(imports.chain(qualifications))
}

#[cfg(test)]
mod tests {

    use crate::code_actions;

    use insta::assert_snapshot;
    use ruff_db::{
        diagnostic::{
            Annotation, Diagnostic, DiagnosticId, DisplayDiagnosticConfig, LintName, Span,
            SubDiagnostic,
        },
        files::{File, system_path_to_file},
        system::{DbWithWritableSystem, SystemPathBuf},
    };
    use ruff_diagnostics::Fix;
    use ruff_python_trivia::textwrap::dedent;
    use ruff_text_size::{TextRange, TextSize};
    use ty_project::ProjectMetadata;
    use ty_python_core::ProgramFile;
    use ty_python_semantic::{
        default_lint_registry,
        lint::LintMetadata,
        types::{UNDEFINED_REVEAL, UNRESOLVED_REFERENCE},
    };

    /// a file that already starts with a suppression of its own has the code added to it
    #[test]
    fn add_ignore_in_file_to_an_existing_file_suppression() {
        let test = CodeActionTest::with_source(
            "#!/usr/bin/env python\n# ty: ignore[division-by-zero]\nb = <START>a<END> / 10",
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:5
          |
        3 | b = a / 10
          |     ^
          |
        2 | # ty: ignore[division-by-zero]
          - b = a / 10
        3 + b = a / 10  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:5
          |
        3 | b = a / 10
          |     ^
          |
        1 | #!/usr/bin/env python
          - # ty: ignore[division-by-zero]
        2 + # ty: ignore[division-by-zero, unresolved-reference]
        3 | b = a / 10
          |
        ");
    }

    /// a new file suppression goes after a `#!` line, which has to stay first
    #[test]
    fn add_ignore_in_file_after_a_shebang() {
        let test = CodeActionTest::with_source("#!/usr/bin/env python\nb = <START>a<END> / 10");

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 10
          |     ^
          |
        1 | #!/usr/bin/env python
          - b = a / 10
        2 + b = a / 10  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 10
          |     ^
          |
        1 | #!/usr/bin/env python
        2 + # ty: ignore[unresolved-reference]
        3 | b = a / 10
          |
        ");
    }

    #[test]
    fn add_ignore() {
        let test = CodeActionTest::with_source(r#"b = <START>a<END> / 10"#);

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:1:5
          |
        1 | b = a / 10
          |     ^
          |
          - b = a / 10
        1 + b = a / 10  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:1:5
          |
        1 | b = a / 10
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 | b = a / 10
          |
        ");
    }

    #[test]
    fn add_ignore_existing_comment() {
        let test = CodeActionTest::with_source(r#"b = <START>a<END> / 10  # fmt: off"#);

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:1:5
          |
        1 | b = a / 10  # fmt: off
          |     ^
          |
          - b = a / 10  # fmt: off
        1 + b = a / 10  # fmt: off  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:1:5
          |
        1 | b = a / 10  # fmt: off
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 | b = a / 10  # fmt: off
          |
        ");
    }

    #[test]
    fn add_ignore_trailing_whitespace() {
        let test = CodeActionTest::with_source(r#"b = <START>a<END> / 10  "#);

        // Not an inline snapshot because of trailing whitespace.
        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE));
    }

    #[test]
    fn add_code_existing_ignore() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # ty:ignore[division-by-zero]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero]
          |     ^
          |
        1 |
          - b = a / 0  # ty:ignore[division-by-zero]
        2 + b = a / 0  # ty:ignore[division-by-zero, unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_empty_ignore() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 10  # ty:ignore[]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 10  # ty:ignore[]
          |     ^
          |
        1 |
          - b = a / 10  # ty:ignore[]
        2 + b = a / 10  # ty:ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 10  # ty:ignore[]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_ignore_updates_preceding_own_line_suppression() {
        let test = CodeActionTest::with_source(
            r#"
            seen_code = True
            # ty:ignore[]
            b = <START>a<END> / 10
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:4:5
          |
        4 | b = a / 10
          |     ^
          |
        2 | seen_code = True
          - # ty:ignore[]
        3 + # ty:ignore[unresolved-reference]
        4 | b = a / 10
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:4:5
          |
        4 | b = a / 10
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_ignore_matches_existing_suppression_against_diagnostic_range() {
        // The first suppression is intentional: `not-a-rule` has no indexed suppression, and
        // repeatedly extending the final suppression can't suppress a diagnostic before it.
        let test = CodeActionTest::with_source(
            r#"
            seen_code = True
            # ty:ignore[] # ty:ignore[<START>not-a-rule<END>] # ty:ignore[division-by-zero]
            value = 1 / 0
        "#,
        );

        let lint = default_lint_registry()
            .get("ignore-comment-unknown-rule")
            .unwrap();
        assert_snapshot!(test.code_actions(&lint), @"
        info[code-action]: Ignore 'ignore-comment-unknown-rule' for this line
         --> main.py:3:27
          |
        3 | # ty:ignore[] # ty:ignore[not-a-rule] # ty:ignore[division-by-zero]
          |                           ^^^^^^^^^^
          |
        2 | seen_code = True
          - # ty:ignore[] # ty:ignore[not-a-rule] # ty:ignore[division-by-zero]
        3 + # ty:ignore[ignore-comment-unknown-rule] # ty:ignore[not-a-rule] # ty:ignore[division-by-zero]
        4 | value = 1 / 0
          |

        info[code-action]: Ignore 'ignore-comment-unknown-rule' in this file
         --> main.py:3:27
          |
        3 | # ty:ignore[] # ty:ignore[not-a-rule] # ty:ignore[division-by-zero]
          |                           ^^^^^^^^^^
          |
        1 + # ty: ignore[ignore-comment-unknown-rule]
        2 |
          |
        ");
    }

    #[test]
    fn add_ignore_does_not_make_nested_suppression_unused() {
        let test = CodeActionTest::with_source(
            r#"
            seen_code = True
            # ty:ignore[]
            values = [
                # ty:ignore[unresolved-reference]
                missing,
                <START>absent<END>,
            ]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:7:5
          |
        7 |     absent,
          |     ^^^^^^
          |
        2 | seen_code = True
          - # ty:ignore[]
        3 + # ty:ignore[unresolved-reference]
        4 | values = [
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:7:5
          |
        7 |     absent,
          |     ^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_ignore_reuses_outer_suppression_with_nested_blanket() {
        let test = CodeActionTest::with_source(
            r#"
            def f(value: int) -> int: return value

            seen_code = True
            # ty:ignore[invalid-assignment]
            values: tuple[int] = [
                # ty:ignore
                f("bad"),
                <START>absent<END>,
            ]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:9:5
          |
        9 |     absent,
          |     ^^^^^^
          |
        4 | seen_code = True
          - # ty:ignore[invalid-assignment]
        5 + # ty:ignore[invalid-assignment, unresolved-reference]
        6 | values: tuple[int] = [
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:9:5
          |
        9 |     absent,
          |     ^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_type_ignore() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # type:ignore[ty:division-by-zero]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # type:ignore[ty:division-by-zero]
          |     ^
          |
        1 |
          - b = a / 0  # type:ignore[ty:division-by-zero]
        2 + b = a / 0  # type:ignore[ty:division-by-zero, ty:unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # type:ignore[ty:division-by-zero]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_type_ignore_without_any_ty_code() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # type:ignore[mypy-code]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # type:ignore[mypy-code]
          |     ^
          |
        1 |
          - b = a / 0  # type:ignore[mypy-code]
        2 + b = a / 0  # type:ignore[mypy-code]  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # type:ignore[mypy-code]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_ignore_existing_file_level_ignore() {
        let test = CodeActionTest::with_source(
            r#"
            # ty:ignore[division-by-zero]

            b = <START>a<END> / 0
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:4:5
          |
        4 | b = a / 0
          |     ^
          |
        3 |
          - b = a / 0
        4 + b = a / 0  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:4:5
          |
        4 | b = a / 0
          |     ^
          |
        1 |
          - # ty:ignore[division-by-zero]
        2 + # ty:ignore[division-by-zero, unresolved-reference]
        3 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignore_trailing_comma() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # ty:ignore[division-by-zero,]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero,]
          |     ^
          |
        1 |
          - b = a / 0  # ty:ignore[division-by-zero,]
        2 + b = a / 0  # ty:ignore[division-by-zero, unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero,]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignore_trailing_whitespace() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # ty:ignore[division-by-zero   ]
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero   ]
          |     ^
          |
        1 |
          - b = a / 0  # ty:ignore[division-by-zero   ]
        2 + b = a / 0  # ty:ignore[division-by-zero, unresolved-reference   ]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero   ]
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignore_with_reason() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> / 0  # ty:ignore[division-by-zero] some explanation
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero] some explanation
          |     ^
          |
        1 |
          - b = a / 0  # ty:ignore[division-by-zero] some explanation
        2 + b = a / 0  # ty:ignore[division-by-zero] some explanation  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a / 0  # ty:ignore[division-by-zero] some explanation
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignore_start_line() {
        let test = CodeActionTest::with_source(
            r#"
            b = (
                    <START>a  # ty:ignore[division-by-zero]
                    /
                    0<END>
            )
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:9
          |
        3 | /         a  # ty:ignore[division-by-zero]
        4 | |         /
        5 | |         0
          | |_________^
          |
        2 | b = (
          -         a  # ty:ignore[division-by-zero]
        3 +         a  # ty:ignore[division-by-zero, unresolved-reference]
        4 |         /
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:9
          |
        3 | /         a  # ty:ignore[division-by-zero]
        4 | |         /
        5 | |         0
          | |_________^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignore_end_line() {
        let test = CodeActionTest::with_source(
            r#"
            b = (
                    <START>a
                    /
                    0<END>  # ty:ignore[division-by-zero]
            )
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:9
          |
        3 | /         a
        4 | |         /
        5 | |         0  # ty:ignore[division-by-zero]
          | |_________^
          |
        4 |         /
          -         0  # ty:ignore[division-by-zero]
        5 +         0  # ty:ignore[division-by-zero, unresolved-reference]
        6 | )
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:9
          |
        3 | /         a
        4 | |         /
        5 | |         0  # ty:ignore[division-by-zero]
          | |_________^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_existing_ignores() {
        let test = CodeActionTest::with_source(
            r#"
            b = (
                    <START>a  # ty:ignore[division-by-zero]
                    /
                    0<END>  # ty:ignore[division-by-zero]
            )
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:9
          |
        3 | /         a  # ty:ignore[division-by-zero]
        4 | |         /
        5 | |         0  # ty:ignore[division-by-zero]
          | |_________^
          |
        2 | b = (
          -         a  # ty:ignore[division-by-zero]
        3 +         a  # ty:ignore[division-by-zero, unresolved-reference]
        4 |         /
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:9
          |
        3 | /         a  # ty:ignore[division-by-zero]
        4 | |         /
        5 | |         0  # ty:ignore[division-by-zero]
          | |_________^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_interpolated_string() {
        let test = CodeActionTest::with_source(
            r#"
            b = f"""
                {<START>a<END>}
                more text
            """
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r#"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:6
          |
        3 |     {a}
          |      ^
          |
        4 |     more text
          - """
        5 + """  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:6
          |
        3 |     {a}
          |      ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        "#);
    }

    #[test]
    fn add_code_multiline_interpolation() {
        let test = CodeActionTest::with_source(
            r#"
            b = f"""
                {
                <START>a<END>
                }
                more text
            """
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:4:5
          |
        4 |     a
          |     ^
          |
        3 |     {
          -     a
        4 +     a  # ty: ignore[unresolved-reference]
        5 |     }
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:4:5
          |
        4 |     a
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn add_code_followed_by_multiline_string() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> + """
                more text
            """
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r#"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a + """
          |     ^
          |
        3 |     more text
          - """
        4 + """  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a + """
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        "#);
    }

    #[test]
    fn add_code_followed_by_continuation() {
        let test = CodeActionTest::with_source(
            r#"
            b = <START>a<END> \
            + "test"
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r#"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:5
          |
        2 | b = a \
          |     ^
          |
        2 | b = a \
          - + "test"
        3 + + "test"  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:5
          |
        2 | b = a \
          |     ^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        "#);
    }

    #[test]
    fn add_ignore_line_continuation_empty_lines() {
        let test = CodeActionTest::with_source(
            r#"b = bbbbb \
    [  ccc # test

        + <START>ddd<END>  \

    ] # test
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r"
        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:4:11
          |
        4 |         + ddd  \
          |           ^^^
          |
        4 |         + ddd  \
          -
        5 +   # ty: ignore[unresolved-reference]
        6 |     ] # test
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:4:11
          |
        4 |         + ddd  \
          |           ^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 | b = bbbbb \
          |
        ");
    }

    #[test]
    fn add_ignore_undefined_reveal_type() {
        let test = CodeActionTest::with_source(
            r#"
            <START>reveal_type<END>(1)
        "#,
        );

        assert_snapshot!(test.code_actions(&UNDEFINED_REVEAL), @"
        info[code-action]: Ignore 'undefined-reveal' for this line
         --> main.py:2:1
          |
        2 | reveal_type(1)
          | ^^^^^^^^^^^
          |
        1 |
          - reveal_type(1)
        2 + reveal_type(1)  # ty: ignore[undefined-reveal]
          |

        info[code-action]: Ignore 'undefined-reveal' in this file
         --> main.py:2:1
          |
        2 | reveal_type(1)
          | ^^^^^^^^^^^
          |
        1 + # ty: ignore[undefined-reveal]
        2 |
          |
        ");
    }

    #[test]
    fn unresolved_deprecated() {
        let test = CodeActionTest::with_source(
            r#"
            @<START>deprecated<END>("do not use")
            def my_func(): ...
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r#"
        info[code-action]: import warnings.deprecated
         --> main.py:2:2
          |
        2 | @deprecated("do not use")
          |  ^^^^^^^^^^
        help: This is a preferred code action
          |
        1 + from warnings import deprecated
        2 |
          |

        info[code-action]: import typing_extensions.deprecated
         --> main.py:2:2
          |
        2 | @deprecated("do not use")
          |  ^^^^^^^^^^
        help: This is a preferred code action
          |
        1 + from typing_extensions import deprecated
        2 |
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:2
          |
        2 | @deprecated("do not use")
          |  ^^^^^^^^^^
          |
        1 |
          - @deprecated("do not use")
        2 + @deprecated("do not use")  # ty: ignore[unresolved-reference]
        3 | def my_func(): ...
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:2
          |
        2 | @deprecated("do not use")
          |  ^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        "#);
    }

    /// `dt` is a common alias of `datetime`, so an unresolved one offers the import that binds it.
    #[test]
    fn unresolved_common_alias() {
        let test = CodeActionTest::with_source(
            r#"
            <START>dt<END>.timedelta(days=1)
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: import datetime as dt
         --> main.py:2:1
          |
        2 | dt.timedelta(days=1)
          | ^^
        help: This is a preferred code action
        help: This is the only import that resolves the name
          |
        1 + import datetime as dt
        2 |
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:1
          |
        2 | dt.timedelta(days=1)
          | ^^
          |
        1 |
          - dt.timedelta(days=1)
        2 + dt.timedelta(days=1)  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:1
          |
        2 | dt.timedelta(days=1)
          | ^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn unresolved_deprecated_warnings_imported() {
        let test = CodeActionTest::with_source(
            r#"
            import warnings

            @<START>deprecated<END>("do not use")
            def my_func(): ...
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @r#"
        info[code-action]: import warnings.deprecated
         --> main.py:4:2
          |
        4 | @deprecated("do not use")
          |  ^^^^^^^^^^
        help: This is a preferred code action
          |
        2 | import warnings
        3 + from warnings import deprecated
        4 |
          |

        info[code-action]: import typing_extensions.deprecated
         --> main.py:4:2
          |
        4 | @deprecated("do not use")
          |  ^^^^^^^^^^
        help: This is a preferred code action
          |
        3 |
        4 + from typing_extensions import deprecated
        5 +
        6 | @deprecated("do not use")
          |

        info[code-action]: qualify warnings.deprecated
         --> main.py:4:2
          |
        4 | @deprecated("do not use")
          |  ^^^^^^^^^^
        help: This is a preferred code action
          |
        3 |
          - @deprecated("do not use")
        4 + @warnings.deprecated("do not use")
        5 | def my_func(): ...
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:4:2
          |
        4 | @deprecated("do not use")
          |  ^^^^^^^^^^
          |
        3 |
          - @deprecated("do not use")
        4 + @deprecated("do not use")  # ty: ignore[unresolved-reference]
        5 | def my_func(): ...
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:4:2
          |
        4 | @deprecated("do not use")
          |  ^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        "#);
    }

    // using `importlib.abc.ExecutionLoader` when no imports are in scope
    #[test]
    fn unresolved_loader() {
        let test = CodeActionTest::with_source(
            r#"
            <START>ExecutionLoader<END>
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: import importlib.abc.ExecutionLoader
         --> main.py:2:1
          |
        2 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
        help: This is a preferred code action
        help: This is the only import that resolves the name
          |
        1 + from importlib.abc import ExecutionLoader
        2 |
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:2:1
          |
        2 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        1 |
          - ExecutionLoader
        2 + ExecutionLoader  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:2:1
          |
        2 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    // using `importlib.abc.ExecutionLoader` when `import importlib` is in scope
    //
    // TODO: `importlib.abc` is available whenever `importlib` is, so qualifying
    // `importlib.abc.ExecutionLoader` without adding imports is actually legal here!
    #[test]
    fn unresolved_loader_importlib_imported() {
        let test = CodeActionTest::with_source(
            r#"
            import importlib
            <START>ExecutionLoader<END>
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: import importlib.abc.ExecutionLoader
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
        help: This is a preferred code action
        help: This is the only import that resolves the name
          |
        2 | import importlib
        3 + from importlib.abc import ExecutionLoader
        4 | ExecutionLoader
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        2 | import importlib
          - ExecutionLoader
        3 + ExecutionLoader  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    // Using `importlib.abc.ExecutionLoader` when `import importlib.abc` is in scope
    #[test]
    fn unresolved_loader_abc_imported() {
        let test = CodeActionTest::with_source(
            r#"
            import importlib.abc
            <START>ExecutionLoader<END>
        "#,
        );

        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: import importlib.abc.ExecutionLoader
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
        help: This is a preferred code action
        help: This is the only import that resolves the name
          |
        2 | import importlib.abc
        3 + from importlib.abc import ExecutionLoader
        4 | ExecutionLoader
          |

        info[code-action]: qualify importlib.abc.ExecutionLoader
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
        help: This is a preferred code action
          |
        2 | import importlib.abc
          - ExecutionLoader
        3 + importlib.abc.ExecutionLoader
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        2 | import importlib.abc
          - ExecutionLoader
        3 + ExecutionLoader  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:3:1
          |
        3 | ExecutionLoader
          | ^^^^^^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 |
          |
        ");
    }

    #[test]
    fn unresolved_namespace_descendant() {
        let mut test = CodeActionTest::with_source("<START>NamespaceReport<END>()");
        test.db
            .write_file("acme/reports.py", "class NamespaceReport: ...")
            .expect("write unopened namespace descendant");
        assert_snapshot!(test.code_actions(&UNRESOLVED_REFERENCE), @"
        info[code-action]: import acme.reports.NamespaceReport
         --> main.py:1:1
          |
        1 | NamespaceReport()
          | ^^^^^^^^^^^^^^^
        help: This is a preferred code action
        help: This is the only import that resolves the name
          |
        1 + from acme.reports import NamespaceReport
        2 | NamespaceReport()
          |

        info[code-action]: Ignore 'unresolved-reference' for this line
         --> main.py:1:1
          |
        1 | NamespaceReport()
          | ^^^^^^^^^^^^^^^
          |
          - NamespaceReport()
        1 + NamespaceReport()  # ty: ignore[unresolved-reference]
          |

        info[code-action]: Ignore 'unresolved-reference' in this file
         --> main.py:1:1
          |
        1 | NamespaceReport()
          | ^^^^^^^^^^^^^^^
          |
        1 + # ty: ignore[unresolved-reference]
        2 | NamespaceReport()
          |
        ");
    }

    struct CodeActionTest {
        db: ty_project::TestDb,
        file: File,
        diagnostic_range: TextRange,
    }

    impl CodeActionTest {
        fn with_source(source: &str) -> Self {
            let mut db =
                ty_project::TestDb::new(ProjectMetadata::new("test", SystemPathBuf::from("/")));

            let mut cleansed = dedent(source).to_string();

            let start = cleansed
                .find("<START>")
                .expect("source text should contain a `<START>` marker");
            cleansed.replace_range(start..start + "<START>".len(), "");

            let end = cleansed
                .find("<END>")
                .expect("source text should contain a `<END>` marker");

            cleansed.replace_range(end..end + "<END>".len(), "");

            assert!(start <= end, "<START> marker should be before <END> marker");

            db.write_file("main.py", cleansed)
                .expect("write to memory file system to be successful");

            let file = system_path_to_file(&db, "main.py").expect("newly written file to existing");

            Self {
                db,
                file,
                diagnostic_range: TextRange::new(
                    TextSize::try_from(start).unwrap(),
                    TextSize::try_from(end).unwrap(),
                ),
            }
        }

        fn code_actions(&self, lint: &LintMetadata) -> String {
            use std::fmt::Write;

            let mut buf = String::new();

            let config = DisplayDiagnosticConfig::new("ty").color(false).context(0);

            // every fixture this harness writes is python; the template actions
            // have their own tests
            for action in code_actions(
                &self.db,
                ProgramFile::new(
                    &self.db,
                    self.file,
                    self.db.program_environment().program(&self.db),
                ),
                self.diagnostic_range,
                &lint.name,
                false,
            ) {
                let mut diagnostic = Diagnostic::new(
                    DiagnosticId::Lint(LintName::of("code-action")),
                    ruff_db::diagnostic::Severity::Info,
                    action.title,
                );

                // an action's edits are all in one file, which is not
                // necessarily the file the diagnostic is in
                let edited = action
                    .edits
                    .first()
                    .map_or(self.file, |edit: &crate::FileEdit| edit.file);

                diagnostic.annotate(Annotation::primary(
                    Span::from(edited).with_range(self.diagnostic_range),
                ));

                if action.preferred {
                    diagnostic.sub(SubDiagnostic::new(
                        ruff_db::diagnostic::SubDiagnosticSeverity::Help,
                        "This is a preferred code action",
                    ));
                }

                if action.unambiguous_import {
                    diagnostic.sub(SubDiagnostic::new(
                        ruff_db::diagnostic::SubDiagnosticSeverity::Help,
                        "This is the only import that resolves the name",
                    ));
                }

                let mut edits = action.edits.into_iter().map(|edit| edit.edit);
                if let Some(first) = edits.next() {
                    diagnostic.set_fix(Fix::safe_edits(first, edits));
                }

                write!(buf, "{}", diagnostic.display(&self.db, &config)).unwrap();
            }

            buf
        }
    }
}
