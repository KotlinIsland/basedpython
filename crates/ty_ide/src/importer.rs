//! Tests for the shared importer using the IDE's cursor fixtures.

#[cfg(test)]
mod tests {
    use insta::assert_snapshot;
    use ruff_db::Db as _;
    use ruff_linter::linter::{ParseSource, lint_only};
    use ruff_linter::package::PackageRoot;
    use ruff_linter::registry::Rule;
    use ruff_linter::settings::flags::Noqa;
    use ruff_linter::settings::rule_table::RuleTable;
    use ruff_linter::source_kind::SourceKind;
    use ruff_python_ast::PySourceType;
    use ty_project::Db as _;

    use crate::import_order::{self, IsortConfig};
    use crate::tests::{CursorTest, cursor_test};
    use ruff_python_ast::find_node::covering_node;
    use ruff_text_size::{Ranged, TextRange};
    use ty_python_core::ProgramFile;
    use ty_python_semantic::importer::{ImportAction, ImportRequest, Importer};

    impl CursorTest {
        fn import(&self, module: &str, member: &str) -> String {
            self.add(ImportRequest::import(module, member))
        }

        fn import_from(&self, module: &str, member: &str) -> String {
            self.add(ImportRequest::import_from(module, member))
        }

        fn module(&self, module: &str) -> String {
            self.add(ImportRequest::module(module))
        }

        fn add(&self, request: ImportRequest<'_>) -> String {
            let node = covering_node(
                self.cursor.parsed.syntax().into(),
                TextRange::empty(self.cursor.offset),
            )
            .node();
            let importer = self.importer();
            let members = importer.members_in_scope_at(node, self.cursor.offset);
            let resp = importer.import(request, &members);

            // We attempt to emulate what an LSP client would
            // do here and "insert" the import into the original
            // source document. I'm not 100% sure this models
            // reality correctly, but in particular, we are
            // careful to insert the symbol name first since
            // it *should* come after the import.
            let mut source = self.cursor.source.to_string();
            source.insert_str(self.cursor.offset.to_usize(), resp.symbol_text());
            if let Some(edit) = resp.import() {
                assert!(
                    edit.range().start() <= self.cursor.offset,
                    "import edit must come at or before <CURSOR>, \
                     but <CURSOR> starts at {} and the import \
                     edit is at {}..{}",
                    self.cursor.offset.to_usize(),
                    edit.range().start().to_usize(),
                    edit.range().end().to_usize(),
                );
                source.replace_range(edit.range().to_std_range(), edit.content().unwrap_or(""));
            }
            // when buff accepts the file's imports with the symbol written in, it accepts them
            // with the import added too. a new import set apart from the code after it can't
            // be checked when the symbol is the first code after it, as the file the importer
            // sees has none there
            let mut with_symbol = self.cursor.source.to_string();
            with_symbol.insert_str(self.cursor.offset.to_usize(), resp.symbol_text());
            let follows_code =
                resp.import().is_none_or(|edit| {
                    self.cursor.parsed.suite().iter().any(|stmt| {
                        stmt.start() >= edit.start() && stmt.start() <= self.cursor.offset
                    })
                });
            if follows_code && self.unsorted_imports(&with_symbol) == 0 {
                assert_eq!(
                    self.unsorted_imports(&source),
                    0,
                    "buff reports the imports unsorted once one is added:\n{source}"
                );
            }
            source
        }

        /// The file with everything `requests` import added at once, as
        /// [`Importer::combined_edits`] adds them, with each request's symbol written on a line
        /// of its own at the cursor. A file buff accepts the imports of with the symbols written
        /// in still has imports it accepts once they are added.
        fn add_all(&self, requests: &[ImportRequest<'_>]) -> String {
            let (with_symbols, source) = self.add_all_with(&self.importer(), requests);
            if self.unsorted_imports(&with_symbols) == 0 {
                assert_eq!(
                    self.unsorted_imports(&source),
                    0,
                    "buff reports the imports unsorted once they are added:\n{source}"
                );
            }
            source
        }

        /// The file with the symbols `requests` are referred to by written at the cursor, and
        /// the file with `importer`'s imports of them added too.
        fn add_all_with(
            &self,
            importer: &Importer<'_>,
            requests: &[ImportRequest<'_>],
        ) -> (String, String) {
            let node = covering_node(
                self.cursor.parsed.syntax().into(),
                TextRange::empty(self.cursor.offset),
            )
            .node();
            let members = importer.members_in_scope_at(node, self.cursor.offset);
            let actions: Vec<_> = requests
                .iter()
                .map(|request| importer.import(*request, &members))
                .collect();
            let symbols: Vec<&str> = actions.iter().map(ImportAction::symbol_text).collect();
            let mut with_symbols = self.cursor.source.to_string();
            with_symbols.insert_str(self.cursor.offset.to_usize(), &symbols.join("\n"));
            let mut source = with_symbols.clone();
            let mut edits = importer.combined_edits(&actions);
            edits.sort_by_key(|edit| std::cmp::Reverse(edit.start()));
            for edit in edits {
                assert!(edit.range().end() <= self.cursor.offset);
                source.replace_range(edit.range().to_std_range(), edit.content().unwrap_or(""));
            }
            (with_symbols, source)
        }

        /// How many `I001` diagnostics buff's isort check reports for `source` as the contents
        /// of the cursor's file, with the configuration buff reads for it. buff sorts each cell
        /// of a notebook on its own, so of a notebook only the cursor's cell is checked, which
        /// is where an import is added
        fn unsorted_imports(&self, source: &str) -> usize {
            let Some(path) = self.cursor.file.path(&self.db).as_system_path() else {
                return 0;
            };
            let source =
                match self.cursor.source.as_notebook().and_then(|notebook| {
                    notebook.cell_offsets().containing_range(self.cursor.offset)
                }) {
                    Some(cell) => {
                        let grown = source.len() - self.cursor.source.as_str().len();
                        &source[cell.start().to_usize()..cell.end().to_usize() + grown]
                    }
                    None => source,
                };
            let root = self.db.project().root(&self.db);
            let config =
                IsortConfig::for_file(self.db.system(), path.as_std_path(), root.as_std_path());
            let (settings, package) = config.settings();
            let mut settings = settings.clone();
            settings.rules = RuleTable::from_iter([Rule::UnsortedImports]);
            lint_only(
                path.as_std_path(),
                package.map(PackageRoot::root),
                &settings,
                Noqa::Enabled,
                &SourceKind::Python {
                    code: source.to_string(),
                    is_stub: false,
                    is_basedpython: false,
                },
                PySourceType::Python,
                ParseSource::None,
            )
            .diagnostics
            .len()
        }

        fn importer(&self) -> Importer<'_> {
            import_order::importer(
                &self.db,
                ProgramFile::new(
                    &self.db,
                    self.cursor.file,
                    self.db.program_environment().program(&self.db),
                ),
                &self.cursor.parsed,
            )
        }
    }

    /// a new import goes where buff's isort would sort it: into its section of the block, in
    /// order among the section's imports, rather than at the top of the block
    #[test]
    fn sorted_into_its_section() {
        let source = r#"
"""Module docstring."""

from __future__ import annotations

import json
import sys
from collections import OrderedDict

import attr

<CURSOR>
"#;
        let test = cursor_test(source);
        assert_snapshot!(test.module("math"), @r#"
        """Module docstring."""

        from __future__ import annotations

        import json
        import math
        import sys
        from collections import OrderedDict

        import attr

        math
        "#);
        assert_snapshot!(test.import_from("pathlib", "Path"), @r#"
        """Module docstring."""

        from __future__ import annotations

        import json
        import sys
        from collections import OrderedDict
        from pathlib import Path

        import attr

        Path
        "#);
        assert_snapshot!(test.module("attrs"), @r#"
        """Module docstring."""

        from __future__ import annotations

        import json
        import sys
        from collections import OrderedDict

        import attr
        import attrs

        attrs
        "#);
    }

    /// a notebook's cells are sorted each on its own, so an import goes into the import block of
    /// the cell it is used in, sorted there, and the cells before it are left alone
    #[test]
    fn a_notebook_import_is_sorted_in_its_cell() {
        let test = CursorTest::builder()
            .notebook(
                "main.ipynb",
                &[
                    "import sys",
                    "import json\nimport os\n\nimport attr\n\n<CURSOR>",
                ],
            )
            .build();
        assert_snapshot!(test.module("math"), @"
        import sys
        import json
        import math
        import os

        import attr

        math
        ");
        assert_snapshot!(test.import_from("pathlib", "Path"), @"
        import sys
        import json
        import os
        from pathlib import Path

        import attr

        Path
        ");
    }

    /// a new block in a cell without imports goes at the top of the cell, set apart from the
    /// cell's code as the sorter sets a block apart, and a statement in the next cell is not
    /// code after it
    #[test]
    fn a_notebook_import_starts_a_block_in_its_cell() {
        let test = CursorTest::builder()
            .notebook(
                "main.ipynb",
                &["import sys", "def f():\n    return <CURSOR>", "x = 1"],
            )
            .build();
        assert_snapshot!(test.module("math"), @"
        import sys
        import math


        def f():
            return math
        x = 1
        ");
        let test = CursorTest::builder()
            .notebook(
                "main.ipynb",
                &["import sys", "<CURSOR>", "def g():\n    pass"],
            )
            .build();
        assert_snapshot!(test.module("math"), @"
        import sys
        import math
        math
        def g():
            pass
        ");
    }

    /// a heading above a cell's first import is that cell's, and a heading in the cell before
    /// is not taken into its block
    #[test]
    fn a_notebook_heading_search_stays_in_its_cell() {
        let test = CursorTest::builder()
            .source("ruff.toml", HEADINGS)
            .notebook(
                "main.ipynb",
                &[
                    "# Standard Library\nimport sys\n\n# Third Party",
                    "import attr\n\n<CURSOR>",
                ],
            )
            .build();
        assert_snapshot!(test.module("os"), @"
        # Standard Library
        import sys

        # Third Party
        # Standard Library
        import os

        # Third Party
        import attr

        os
        ");
    }

    /// a member added to a `from` import is listed where isort orders it: constants, then
    /// classes, then everything else, each alphabetically
    #[test]
    fn member_sorted_among_members() {
        let test = cursor_test(
            "\
from collections import OrderedDict, deque
<CURSOR>
",
        );
        assert_snapshot!(test.import("collections", "Counter"), @"
        from collections import Counter, OrderedDict, deque
        Counter
        ");
        assert_snapshot!(test.import("collections", "defaultdict"), @"
        from collections import OrderedDict, defaultdict, deque
        defaultdict
        ");
        assert_snapshot!(test.import("collections", "namedtuple"), @"
        from collections import OrderedDict, deque, namedtuple
        namedtuple
        ");
    }

    /// one per line in an import written one per line
    #[test]
    fn member_sorted_among_members_one_per_line() {
        let test = cursor_test(
            "\
from collections import (
    OrderedDict,
    deque,
)
<CURSOR>
",
        );
        assert_snapshot!(test.import("collections", "defaultdict"), @"
        from collections import (
            OrderedDict,
            defaultdict,
            deque,
        )
        defaultdict
        ");
    }

    /// an import of a section the block has none of yet is set apart by a blank line on each
    /// side, as isort separates sections
    #[test]
    fn new_section_is_set_apart() {
        let test = cursor_test(
            "\
import json

<CURSOR>
",
        );
        assert_snapshot!(test.module("attrs"), @"
        import json

        import attrs

        attrs
        ");

        let test = cursor_test(
            "\
import attr

<CURSOR>
",
        );
        assert_snapshot!(test.module("abc"), @"
        import abc

        import attr

        abc
        ");
    }

    /// comments above an import stay with it, as isort keeps them
    #[test]
    fn placed_above_the_comments_of_the_import_after() {
        let test = cursor_test(
            "\
import json
# the system
import sys

<CURSOR>
",
        );
        assert_snapshot!(test.module("os"), @"
        import json
        import os
        # the system
        import sys

        os
        ");
    }

    /// A test whose project configures buff with `ruff_toml`, a `ruff.toml`, and whose file is
    /// `main`.
    fn configured_test(ruff_toml: &str, main: &str) -> CursorTest {
        CursorTest::builder()
            .source("ruff.toml", ruff_toml)
            .source("main.py", main)
            .build()
    }

    const HEADINGS: &str = r#"
[lint.isort.import-heading]
standard-library = "Standard Library"
third-party = "Third Party"
"#;

    /// A statement of a section before the block's first goes above the heading of that first
    /// section, under a heading of its own: isort takes the headings directly above a block's
    /// first import into the block and writes them again.
    #[test]
    fn heading_of_a_new_first_section() {
        let test = configured_test(
            HEADINGS,
            "\
# Third Party
import attr

<CURSOR>
",
        );
        assert_snapshot!(test.module("os"), @"
        # Standard Library
        import os

        # Third Party
        import attr

        os
        ");
    }

    /// A statement that joins the block's first section goes under its heading.
    #[test]
    fn heading_of_the_first_section_stays_first() {
        let test = configured_test(
            HEADINGS,
            "\
# Standard Library
import sys

# Third Party
import attr

<CURSOR>
",
        );
        assert_snapshot!(test.module("os"), @"
        # Standard Library
        import os
        import sys

        # Third Party
        import attr

        os
        ");
    }

    /// A new section after the others is written with its heading.
    #[test]
    fn heading_of_a_new_last_section() {
        let test = configured_test(
            HEADINGS,
            "\
# Standard Library
import os

<CURSOR>
",
        );
        assert_snapshot!(test.module("attr"), @"
        # Standard Library
        import os

        # Third Party
        import attr

        attr
        ");
    }

    /// Two members added from a module the file doesn't import yet are one `from` import, with
    /// no import block to join.
    #[test]
    fn members_combined_without_an_import_block() {
        let test = cursor_test(
            "\
x = 1
<CURSOR>
",
        );
        assert_snapshot!(test.add_all(&[
            ImportRequest::import_from("pathlib", "PurePath"),
            ImportRequest::import_from("pathlib", "Path"),
        ]), @"
        from pathlib import Path, PurePath

        x = 1
        PurePath
        Path
        ");
    }

    /// And without an order to follow, as for a file buff has no configuration for.
    #[test]
    fn members_combined_without_an_order() {
        let test = cursor_test(
            "\
x = 1
<CURSOR>
",
        );
        let importer = Importer::new(
            &test.db,
            ProgramFile::new(
                &test.db,
                test.cursor.file,
                test.db.program_environment().program(&test.db),
            ),
            &test.cursor.parsed,
        );
        let (_, source) = test.add_all_with(
            &importer,
            &[
                ImportRequest::import_from("pathlib", "PurePath"),
                ImportRequest::import_from("pathlib", "Path"),
            ],
        );
        assert_snapshot!(source, @"
        from pathlib import PurePath, Path
        x = 1
        PurePath
        Path
        ");
    }

    /// A member that makes an import too long for its line wraps it, as isort writes it.
    #[test]
    fn member_wraps_an_import_past_the_line_length() {
        let test = configured_test(
            "line-length = 50\n",
            "\
from collections import OrderedDict, deque
<CURSOR>
",
        );
        assert_snapshot!(test.import("collections", "defaultdict"), @"
        from collections import (
            OrderedDict,
            defaultdict,
            deque,
        )
        defaultdict
        ");
    }

    /// A new statement too long for its line is written wrapped.
    #[test]
    fn new_statement_wrapped_to_the_line_length() {
        let test = configured_test(
            "line-length = 40\n",
            "\
import os

<CURSOR>
",
        );
        assert_snapshot!(test.add_all(&[
            ImportRequest::import_from("collections", "OrderedDict"),
            ImportRequest::import_from("collections", "defaultdict"),
        ]), @"
        import os
        from collections import (
            OrderedDict,
            defaultdict,
        )

        OrderedDict
        defaultdict
        ");
    }

    /// Between two imports of one line, a statement is written on that line, in order.
    #[test]
    fn between_imports_of_one_line() {
        let test = cursor_test(
            "\
import os; import sys

<CURSOR>
",
        );
        assert_snapshot!(test.module("re"), @"
        import os; import re; import sys

        re
        ");
    }

    /// Under `force-single-line`, a member is imported by a `from` import of its own.
    #[test]
    fn force_single_line() {
        let test = configured_test(
            "[lint.isort]\nforce-single-line = true\n",
            "\
from collections import OrderedDict
<CURSOR>
",
        );
        assert_snapshot!(test.import_from("collections", "deque"), @"
        from collections import OrderedDict
        from collections import deque
        deque
        ");
    }

    /// Without `combine-as-imports`, an aliased member is imported by a `from` import of its
    /// own.
    #[test]
    fn aliased_member_alone_without_combine_as_imports() {
        let test = cursor_test(
            "\
from pathlib import Path
<CURSOR>
",
        );
        assert_snapshot!(test.add(ImportRequest::import_from_as("pathlib", "PurePath", "PP")), @"
        from pathlib import Path
        from pathlib import PurePath as PP
        PP
        ");
    }

    /// With it, an aliased member joins the others.
    #[test]
    fn aliased_member_joins_with_combine_as_imports() {
        let test = configured_test(
            "[lint.isort]\ncombine-as-imports = true\n",
            "\
from pathlib import Path
<CURSOR>
",
        );
        assert_snapshot!(test.add(ImportRequest::import_from_as("pathlib", "PurePath", "PP")), @"
        from pathlib import Path, PurePath as PP
        PP
        ");
    }

    /// A section listed in `no-lines-before` follows the section before it without a blank line.
    #[test]
    fn no_lines_before() {
        let test = configured_test(
            "[lint.isort]\nno-lines-before = [\"third-party\"]\n",
            "\
import os

<CURSOR>
",
        );
        assert_snapshot!(test.module("attr"), @"
        import os
        import attr

        attr
        ");
    }

    /// `lines-between-types` sets the `from` imports of a section apart from its plain ones.
    #[test]
    fn lines_between_types() {
        let test = configured_test(
            "[lint.isort]\nlines-between-types = 1\n",
            "\
import os

<CURSOR>
",
        );
        assert_snapshot!(test.import_from("collections", "deque"), @"
        import os

        from collections import deque

        deque
        ");
    }

    /// `from-first` puts a section's `from` imports before its plain ones.
    #[test]
    fn from_first() {
        let test = configured_test(
            "[lint.isort]\nfrom-first = true\n",
            "\
import os

<CURSOR>
",
        );
        assert_snapshot!(test.import_from("collections", "deque"), @"
        from collections import deque
        import os

        deque
        ");
    }

    /// An eager import goes before a section's lazy ones, and a lazy one after its eager ones.
    #[test]
    fn eager_before_lazy() {
        let test = cursor_test(
            "\
import sys
lazy import json

<CURSOR>
",
        );
        assert_snapshot!(test.module("os"), @"
        import os
        import sys
        lazy import json

        os
        ");
    }

    /// Statements added at once that go in different places of the block are each placed.
    #[test]
    fn several_places_at_once() {
        let test = cursor_test(
            "\
import json
import sys

import attr

<CURSOR>
",
        );
        assert_snapshot!(test.add_all(&[
            ImportRequest::module("zlib"),
            ImportRequest::module("abc"),
            ImportRequest::module("attrs"),
            ImportRequest::module("os"),
        ]), @"
        import abc
        import json
        import os
        import sys
        import zlib

        import attr
        import attrs

        zlib
        abc
        attrs
        os
        ");
    }

    #[test]
    fn empty_source_qualified() {
        let test = cursor_test("<CURSOR>");
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections
        collections.defaultdict
        ");
    }

    #[test]
    fn empty_source_unqualified() {
        let test = cursor_test("<CURSOR>");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_exists_qualified() {
        let test = cursor_test(
            "\
import collections
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections
        collections.defaultdict
        ");
    }

    #[test]
    fn import_exists_unqualified() {
        let test = cursor_test(
            "\
from collections import defaultdict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_exists_glob() {
        let test = cursor_test(
            "\
from collections import *
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import *
        defaultdict
        ");
    }

    #[test]
    fn import_exists_qualified_aliased() {
        let test = cursor_test(
            "\
import collections as c
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections as c
        c.defaultdict
        ");
    }

    #[test]
    fn import_exists_unqualified_aliased() {
        let test = cursor_test(
            "\
from collections import defaultdict as ddict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict as ddict
        ddict
        ");
    }

    #[test]
    fn import_partially_exists_single() {
        let test = cursor_test(
            "\
from collections import Counter
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import Counter, defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_aliased_single() {
        let test = cursor_test(
            "\
from collections import Counter as C
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import Counter as C
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_multi() {
        let test = cursor_test(
            "\
from collections import Counter, OrderedDict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import Counter, OrderedDict, defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_aliased_multi() {
        let test = cursor_test(
            "\
from collections import Counter as C, OrderedDict as OD
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import Counter as C, OrderedDict as OD, defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_semi_colon() {
        let test = cursor_test(
            "\
from collections import Counter;
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import Counter, defaultdict;
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_continuation() {
        let test = cursor_test(
            "\
from collections import Counter, \\
  OrderedDict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @r"
        from collections import Counter, \
          OrderedDict, defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_parentheses_single() {
        let test = cursor_test(
            "\
from collections import (Counter)
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import (Counter, defaultdict)
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_parentheses_trailing_comma() {
        let test = cursor_test(
            "\
from collections import (Counter,)
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import (Counter, defaultdict,)
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_parentheses_multi_line_trailing_comma() {
        let test = cursor_test(
            "\
from collections import (
    Counter,
    OrderedDict,
)
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import (
            Counter,
            OrderedDict,
            defaultdict,
        )
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_parentheses_multi_line_no_trailing_comma() {
        let test = cursor_test(
            "\
from collections import (
    Counter,
    OrderedDict
)
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import (
            Counter,
            OrderedDict, defaultdict
        )
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_relative() {
        let test = CursorTest::builder()
            .source("package/__init__.py", "")
            .source("package/foo.py", "Foo = 1\nBar = 2\n")
            .source(
                "package/sub1/sub2/quux.py",
                "from ...foo import Foo\n<CURSOR>\n",
            )
            .build();
        assert_snapshot!(
            test.import("package.foo", "Bar"), @"
        from ...foo import Bar, Foo
        Bar
        ");
    }

    #[test]
    fn import_partially_exists_incomplete() {
        let test = cursor_test(
            "\
from collections import
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn import_partially_exists_incomplete_parentheses1() {
        let test = cursor_test(
            "\
from collections import ()
<CURSOR>
        ",
        );
        // In this case, because of the `()` being an
        // invalid AST, our importer gives up and just
        // adds a new line. We could add more heuristics
        // to make this case work, but I think there will
        // always be some cases like this that won't make
        // sense.
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import ()
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn priority_unqualified_over_unqualified() {
        let test = cursor_test(
            "\
from collections import defaultdict
import re
from collections import defaultdict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict
        import re
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn priority_unqualified_over_unqualified_between() {
        let test = cursor_test(
            "\
from collections import defaultdict
import re
<CURSOR>
from collections import defaultdict
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict
        import re
        defaultdict
        from collections import defaultdict
        ");
    }

    #[test]
    fn priority_unqualified_over_qualified() {
        let test = cursor_test(
            "\
import collections
from collections import defaultdict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn priority_unqualified_over_partial() {
        let test = cursor_test(
            "\
from collections import OrderedDict
from collections import defaultdict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import OrderedDict
        from collections import defaultdict
        defaultdict
        ");
    }

    #[test]
    fn priority_qualified_over_partial() {
        let test = cursor_test(
            "\
from collections import OrderedDict
import collections
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import OrderedDict, defaultdict
        import collections
        defaultdict
        ");
    }

    #[test]
    fn out_of_scope_ordering_top_level() {
        let test = cursor_test(
            "\
<CURSOR>
from collections import defaultdict
        ",
        );
        // Since the import came after the cursor,
        // we add another import at the top-level
        // of the module.
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections
        collections.defaultdict
        from collections import defaultdict
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict
        defaultdict
        from collections import defaultdict
        ");
    }

    #[test]
    fn out_of_scope_ordering_within_function_add_import() {
        let test = cursor_test(
            "\
def foo():
    <CURSOR>
from collections import defaultdict
        ",
        );
        // Since the import came after the cursor,
        // we add another import at the top-level
        // of the module.
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections


        def foo():
            collections.defaultdict
        from collections import defaultdict
        ");
    }

    #[test]
    fn in_scope_ordering_within_function() {
        let test = cursor_test(
            "\
from collections import defaultdict

def foo():
    <CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict

        def foo():
            defaultdict
        ");
    }

    #[test]
    fn existing_future_import() {
        let test = cursor_test(
            "\
from __future__ import annotations

<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("typing", "TypeVar"), @"
        from __future__ import annotations

        import typing

        typing.TypeVar
        ");
    }

    #[test]
    fn existing_future_import_after_docstring() {
        let test = cursor_test(
            r#"
"This is a module level docstring"
from __future__ import annotations

<CURSOR>
        "#,
        );
        assert_snapshot!(
            test.import("typing", "TypeVar"), @r#"

        "This is a module level docstring"
        from __future__ import annotations

        import typing

        typing.TypeVar
        "#);
    }

    #[test]
    fn lazy_future_import_is_not_special() {
        // Lazy `__future__` imports must not act like real future-import anchors for insertion.
        let test = cursor_test(
            "\
lazy from __future__ import annotations

<CURSOR>
        ",
        );
        assert_snapshot!(
            test.import("typing", "TypeVar"), @"
        lazy from __future__ import annotations

        import typing

        typing.TypeVar
        ");
    }

    #[test]
    fn qualify_symbol_to_avoid_overwriting_other_symbol_in_scope() {
        let test = cursor_test(
            "\
defaultdict = 1
(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        defaultdict = 1
        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections

        defaultdict = 1
        (collections.defaultdict)
        ");
    }

    #[test]
    fn unqualify_symbol_to_avoid_overwriting_other_symbol_in_scope() {
        let test = cursor_test(
            "\
collections = 1
(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict

        collections = 1
        (defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict

        collections = 1
        (defaultdict)
        ");
    }

    /// Tests a failure scenario where both the module
    /// name and the member name are in scope and defined
    /// as something other than a module. In this case,
    /// it's very difficult to auto-insert an import in a
    /// way that is correct.
    ///
    /// At time of writing (2025-09-15), we just insert a
    /// qualified import anyway, even though this will result
    /// in what is likely incorrect code. This seems better
    /// than some alternatives:
    ///
    /// 1. Silently do nothing.
    /// 2. Silently omit the symbol from completions.
    /// 3. Come up with an alias for the symbol.
    ///
    /// I think it would perhaps be ideal if we could somehow
    /// prompt the user for what they want to do. But I think
    /// this is okay for now. ---AG
    #[test]
    fn import_results_in_conflict() {
        let test = cursor_test(
            "\
collections = 1
defaultdict = 2
(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        collections = 1
        defaultdict = 2
        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections

        collections = 1
        defaultdict = 2
        (collections.defaultdict)
        ");
    }

    #[test]
    fn within_function_definition_simple() {
        let test = cursor_test(
            "\
def foo():
    (<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections


        def foo():
            (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict


        def foo():
            (defaultdict)
        ");
    }

    #[test]
    fn within_function_definition_member_conflict() {
        let test = cursor_test(
            "\
def defaultdict():
    (<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections


        def defaultdict():
            (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections


        def defaultdict():
            (collections.defaultdict)
        ");
    }

    #[test]
    fn within_function_definition_module_conflict() {
        let test = cursor_test(
            "\
def collections():
    (<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict


        def collections():
            (defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict


        def collections():
            (defaultdict)
        ");
    }

    #[test]
    fn member_conflict_with_other_import() {
        let test = cursor_test(
            "\
import defaultdict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        import defaultdict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections

        import defaultdict

        (collections.defaultdict)
        ");
    }

    #[test]
    fn module_conflict_with_other_import() {
        let test = cursor_test(
            "\
from foo import collections

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        from collections import defaultdict

        from foo import collections

        (defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict

        from foo import collections

        (defaultdict)
        ");
    }

    #[test]
    fn member_conflict_with_other_member_import() {
        let test = cursor_test(
            "\
from othermodule import defaultdict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        from othermodule import defaultdict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections

        from othermodule import defaultdict

        (collections.defaultdict)
        ");
    }

    #[test]
    fn member_conflict_with_other_module_import_alias() {
        let test = cursor_test(
            "\
import defaultdict as ddict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        import defaultdict as ddict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict

        import defaultdict as ddict

        (defaultdict)
        ");
    }

    #[test]
    fn member_conflict_with_other_member_import_alias() {
        let test = cursor_test(
            "\
from othermodule import something as defaultdict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        from othermodule import something as defaultdict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import collections

        from othermodule import something as defaultdict

        (collections.defaultdict)
        ");
    }

    #[test]
    fn no_conflict_alias_module() {
        let test = cursor_test(
            "\
import defaultdict as ddict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        import defaultdict as ddict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict

        import defaultdict as ddict

        (defaultdict)
        ");
    }

    #[test]
    fn no_conflict_alias_member() {
        let test = cursor_test(
            "\
from foo import defaultdict as ddict

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections

        from foo import defaultdict as ddict

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        from collections import defaultdict

        from foo import defaultdict as ddict

        (defaultdict)
        ");
    }

    #[test]
    fn multiple_import_blocks_std() {
        let test = cursor_test(
            "\
import json
import re

from whenever import ZonedDateTime
import numpy as np

(<CURSOR>)
        ",
        );

        assert_snapshot!(
            test.import("collections", "defaultdict"), @"
        import collections
        import json
        import re

        from whenever import ZonedDateTime
        import numpy as np

        (collections.defaultdict)
        ");
        assert_snapshot!(
            test.import_from("collections", "defaultdict"), @"
        import json
        import re
        from collections import defaultdict

        from whenever import ZonedDateTime
        import numpy as np

        (defaultdict)
        ");
    }

    #[test]
    fn multiple_import_blocks_other() {
        let test = CursorTest::builder()
            .source("foo.py", "Foo = 1\nBar = 2\n")
            .source(
                "main.py",
                "\
import json
import re

from whenever import ZonedDateTime
import numpy as np

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("foo", "Bar"), @"
        import json
        import re

        from whenever import ZonedDateTime
        import numpy as np

        import foo

        (foo.Bar)
        ");
        assert_snapshot!(
            test.import_from("foo", "Bar"), @"
        import json
        import re

        from whenever import ZonedDateTime
        import numpy as np

        from foo import Bar

        (Bar)
        ");
    }

    #[test]
    fn conditional_imports_new_import() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
if os.getenv(\"WHATEVER\"):
    from foo import MAGIC
else:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("quux", "MAGIC"), @r#"
        import quux

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (quux.MAGIC)
        "#);
        assert_snapshot!(
            test.import_from("quux", "MAGIC"), @r#"
        import quux

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (quux.MAGIC)
        "#);
    }

    // FIXME: This test (and the one below it) aren't
    // quite right. Namely, because we aren't handling
    // multiple binding sites correctly, we don't see the
    // existing `MAGIC` symbol.
    #[test]
    fn conditional_imports_existing_import1() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
if os.getenv(\"WHATEVER\"):
    from foo import MAGIC
else:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("foo", "MAGIC"), @r#"
        import foo

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (foo.MAGIC)
        "#);
        assert_snapshot!(
            test.import_from("foo", "MAGIC"), @r#"
        from foo import MAGIC

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (MAGIC)
        "#);
    }

    #[test]
    fn conditional_imports_existing_import2() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
if os.getenv(\"WHATEVER\"):
    from foo import MAGIC
else:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("bar", "MAGIC"), @r#"
        import bar

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (bar.MAGIC)
        "#);
        assert_snapshot!(
            test.import_from("bar", "MAGIC"), @r#"
        import bar

        if os.getenv("WHATEVER"):
            from foo import MAGIC
        else:
            from bar import MAGIC

        (bar.MAGIC)
        "#);
    }

    // FIXME: This test (and the one below it) aren't quite right. We
    // don't recognize the multiple declaration sites for `fubar`.
    //
    // In this case, it's not totally clear what we should do. Since we
    // are trying to import `MAGIC` from `foo`, we could add a `from
    // foo import MAGIC` within the first `if` block. Or we could try
    // and "infer" something about the code assuming that we know
    // `MAGIC` is in both `foo` and `bar`.
    #[test]
    fn conditional_imports_existing_module1() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
if os.getenv(\"WHATEVER\"):
    import foo as fubar
else:
    import bar as fubar

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("foo", "MAGIC"), @r#"
        import foo

        if os.getenv("WHATEVER"):
            import foo as fubar
        else:
            import bar as fubar

        (foo.MAGIC)
        "#);
        assert_snapshot!(
            test.import_from("foo", "MAGIC"), @r#"
        from foo import MAGIC

        if os.getenv("WHATEVER"):
            import foo as fubar
        else:
            import bar as fubar

        (MAGIC)
        "#);
    }

    #[test]
    fn conditional_imports_existing_module2() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
if os.getenv(\"WHATEVER\"):
    import foo as fubar
else:
    import bar as fubar

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("bar", "MAGIC"), @r#"
        import bar

        if os.getenv("WHATEVER"):
            import foo as fubar
        else:
            import bar as fubar

        (bar.MAGIC)
        "#);
        assert_snapshot!(
            test.import_from("bar", "MAGIC"), @r#"
        from bar import MAGIC

        if os.getenv("WHATEVER"):
            import foo as fubar
        else:
            import bar as fubar

        (MAGIC)
        "#);
    }

    #[test]
    fn try_imports_new_import() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
try:
    from foo import MAGIC
except ImportError:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("quux", "MAGIC"), @"
        import quux

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (quux.MAGIC)
        ");
        assert_snapshot!(
            test.import_from("quux", "MAGIC"), @"
        import quux

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (quux.MAGIC)
        ");
    }

    // FIXME: This test (and the one below it) aren't
    // quite right. Namely, because we aren't handling
    // multiple binding sites correctly, we don't see the
    // existing `MAGIC` symbol.
    #[test]
    fn try_imports_existing_import1() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
try:
    from foo import MAGIC
except ImportError:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("foo", "MAGIC"), @"
        import foo

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (foo.MAGIC)
        ");
        assert_snapshot!(
            test.import_from("foo", "MAGIC"), @"
        from foo import MAGIC

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (MAGIC)
        ");
    }

    #[test]
    fn try_imports_existing_import2() {
        let test = CursorTest::builder()
            .source("foo.py", "MAGIC = 1")
            .source("bar.py", "MAGIC = 2")
            .source("quux.py", "MAGIC = 3")
            .source(
                "main.py",
                "\
try:
    from foo import MAGIC
except ImportError:
    from bar import MAGIC

(<CURSOR>)
        ",
            )
            .build();

        assert_snapshot!(
            test.import("bar", "MAGIC"), @"
        import bar

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (bar.MAGIC)
        ");
        assert_snapshot!(
            test.import_from("bar", "MAGIC"), @"
        import bar

        try:
            from foo import MAGIC
        except ImportError:
            from bar import MAGIC

        (bar.MAGIC)
        ");
    }

    #[test]
    fn import_module_blank() {
        let test = cursor_test(
            "\
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.module("collections"), @"
        import collections
        collections
        ");
    }

    #[test]
    fn import_module_exists() {
        let test = cursor_test(
            "\
import collections
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.module("collections"), @"
        import collections
        collections
        ");
    }

    #[test]
    fn import_module_from_exists() {
        let test = cursor_test(
            "\
from collections import defaultdict
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.module("collections"), @"
        import collections
        from collections import defaultdict
        collections
        ");
    }

    // This test is working as intended. That is,
    // `abc` is already in scope, so requesting an
    // import for `collections.abc` could feasibly
    // reuse the import and rewrite the symbol text
    // to just `abc`. But for now it seems better
    // to respect what has been written and add the
    // `import collections.abc`. This behavior could
    // plausibly be changed.
    #[test]
    fn import_module_from_via_member_exists() {
        let test = cursor_test(
            "\
from collections import abc
<CURSOR>
        ",
        );
        assert_snapshot!(
            test.module("collections.abc"), @"
        import collections.abc
        from collections import abc
        collections.abc
        ");
    }
}
