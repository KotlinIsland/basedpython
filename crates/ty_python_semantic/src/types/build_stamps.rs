//! basedpython: the experimental gate on a `build:` block.
//!
//! Everything a build stamp *does* happens in the transpiler, which fills each
//! declared stamp in from the values the build settled, so there is nothing here
//! for ty to check about a block the project has asked for. What is left is the
//! opposite case: a block written while the feature is off.
//!
//! It is reported rather than ignored, because ignoring it is the one outcome the
//! feature cannot afford. The block parses and lowers either way — a program that
//! reads `build.GIT_SHA` has to keep working when the project turns the feature
//! off — so nothing at the point of use says the value is unsettled. A stamp
//! declared without a default would then be a hard transpile error, and one with
//! a default would quietly stand for the default in an artifact that claims to
//! know what commit it came from.

use ruff_python_ast as ast;
use ruff_python_ast::visitor::{Visitor, walk_stmt};
use ruff_text_size::TextRange;

use crate::types::context::InferContext;
use crate::types::diagnostic::INVALID_BUILD_STAMPS;

/// Reports every `build:` block in a project that has not opted in to the
/// feature.
pub(super) fn check_build_stamps<'ast>(context: &InferContext<'_, 'ast>, body: &'ast [ast::Stmt]) {
    let db = context.db();
    if db.experimental_settings().build_stamps {
        return;
    }
    // `build:` is basedpython-only syntax, so a python file cannot hold one
    if !context
        .program_file()
        .file(db)
        .source_type(db)
        .is_basedpython()
    {
        return;
    }

    let mut blocks = BuildBlocks::default();
    for statement in body {
        blocks.visit_stmt(statement);
    }

    for keyword in blocks.keywords {
        let Some(builder) = context.report_lint(&INVALID_BUILD_STAMPS, keyword) else {
            continue;
        };
        let mut diagnostic =
            builder.into_diagnostic("`build` is an experimental feature, and is off");
        diagnostic.info("nothing settles these stamps until the project opts in");
        diagnostic.help(
            "Enable it with `build-stamps = true` under `[experimental]` in `basedpython.toml`",
        );
    }
}

/// The `build` keyword of every block in the file.
///
/// A whole-tree walk rather than a scan of the module body, because the lowering
/// fills in a block wherever it is written — including one nested in a class or a
/// function, which is a stamp the same way a module-level one is.
#[derive(Default)]
struct BuildBlocks {
    keywords: Vec<TextRange>,
}

impl<'ast> Visitor<'ast> for BuildBlocks {
    fn visit_stmt(&mut self, statement: &'ast ast::Stmt) {
        if let ast::Stmt::ClassDef(class) = statement
            && let Some(keyword) = class.build_stamps_range()
        {
            self.keywords.push(keyword);
        }
        walk_stmt(self, statement);
    }
}
