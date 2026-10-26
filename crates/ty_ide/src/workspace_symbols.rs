use crate::django_template::django_workspace_symbols;
use crate::symbols::{QueryPattern, SymbolInfo, symbols_for_file};
use rayon::prelude::*;
use ruff_db::files::File;
use ty_module_resolver::file_to_module;
use ty_project::{Db, parallel::ParallelIteratorExt};

/// Get all workspace symbols matching the query string.
/// Returns symbols from all files in the workspace, filtered by the query.
///
/// a query with dots in it is a qualified name: its last part is matched against the symbol's
/// name, and the parts before it against what the symbol is declared in, the module's dotted
/// name and the classes around it. `Shape.describe` finds `describe` in the class `Shape`, and
/// `base.Sq` the class `Square` of the module `shapes.base`. each part matches as the whole query
/// would, as a subsequence of its letters; the last of the parts before the name matches what
/// the symbol is directly in, and the rest match, in order, what that is in. a name with dots of
/// its own, a django template's `base.html`, takes as many of the query's parts as it has
pub fn workspace_symbols(db: &dyn Db, query: &str) -> Vec<WorkspaceSymbolInfo> {
    // If the query is empty, return immediately to avoid expensive file scanning
    if query.is_empty() {
        return Vec::new();
    }

    let workspace_symbols_span = tracing::debug_span!("workspace_symbols");
    let _span = workspace_symbols_span.enter();

    let (containers_query, name_query) = query.rsplit_once('.').unwrap_or(("", query));
    let containers_query: Vec<QueryPattern> = containers_query
        .split('.')
        .filter(|part| !part.is_empty())
        .map(QueryPattern::fuzzy)
        .collect();
    let name_query = QueryPattern::fuzzy(name_query);

    let project = db.project();
    let files = project.files(db);
    let files: Vec<_> = files.iter().collect();

    let mut found: Vec<WorkspaceSymbolInfo> = files
        .into_par_iter()
        .map_with_db(db, |db, file| {
            let symbols_for_file_span = tracing::debug_span!(
                parent: &workspace_symbols_span,
                "symbols_for_file",
                ?file
            );
            let _entered = symbols_for_file_span.entered();

            let program_file = db.program_file(file);
            let module = file_to_module(db, program_file.resolver_file(db))
                .map(|module| module.name(db).to_string());
            let symbols = symbols_for_file(db, program_file);
            symbols
                .search(&name_query)
                .filter_map(|(id, symbol)| {
                    let mut containers: Vec<&str> = module
                        .as_deref()
                        .map(|module| module.split('.').collect())
                        .unwrap_or_default();
                    containers.extend(symbols.container_names(id));
                    if !containers_match(&containers_query, &containers) {
                        return None;
                    }
                    Some(WorkspaceSymbolInfo {
                        symbol: symbol.to_owned(),
                        file,
                        declared_in: (!containers.is_empty()).then(|| containers.join(".")),
                        container: None,
                    })
                })
                .collect::<Vec<_>>()
        })
        .flat_map_iter(|symbols| symbols)
        .collect();

    // a django project is made of things python has no symbol for — a route
    // name, a template — and of classes python offers without saying what django
    // does with them. both are added to the python answer rather than changing it.
    // what django names is declared in nothing a query can name, and its own name
    // can have dots in it, a template's `base.html`: the query's parts are the
    // name's, matched against the whole of it as an undotted query is against a
    // python name, while there are no more of them than the name has
    let query_parts = query.split('.').count();
    found.extend(
        django_workspace_symbols(db, &QueryPattern::fuzzy(query))
            .into_iter()
            .filter(|django| django.symbol.name.split('.').count() >= query_parts)
            .map(|django| WorkspaceSymbolInfo {
                symbol: django.symbol,
                file: django.file,
                declared_in: None,
                container: Some(django.container),
            }),
    );

    found
}

/// whether the parts of a qualified query before the name match `containers`, what a symbol is
/// declared in, outermost first: the last part matches the innermost container, and each part
/// before it a container further out than the one the part after it matched
fn containers_match(query: &[QueryPattern], containers: &[&str]) -> bool {
    let Some((innermost, outer)) = query.split_last() else {
        return true;
    };
    let Some((&direct, mut remaining)) = containers.split_last() else {
        return false;
    };
    if !innermost.is_match_symbol_name(direct) {
        return false;
    }
    for part in outer.iter().rev() {
        let Some(position) = remaining
            .iter()
            .rposition(|container| part.is_match_symbol_name(container))
        else {
            return false;
        };
        remaining = &remaining[..position];
    }
    true
}

/// A symbol found in the workspace, including the file it was found in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSymbolInfo {
    /// The symbol information
    pub symbol: SymbolInfo<'static>,
    /// The file containing the symbol
    pub file: File,
    /// what a python symbol is declared in: the module's dotted name and the classes around it,
    /// `shapes.base.Circle` for a method of `Circle`. `None` for a symbol of a file no module is
    /// imported from, and for a symbol django contributed
    pub declared_in: Option<String>,
    /// what kind of django thing this is, for a symbol django contributed
    ///
    /// `None` for everything python contributes, which is every symbol that was
    /// ever answered before django was.
    pub container: Option<&'static str>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::django_template::tests::{DJANGO_ADMIN, TemplateTest};
    use crate::tests::CursorTest;
    use crate::tests::IntoDiagnostic;
    use insta::assert_snapshot;
    use ruff_db::diagnostic::{
        Annotation, Diagnostic, DiagnosticId, LintName, Severity, Span, SubDiagnostic,
        SubDiagnosticSeverity,
    };

    #[test]
    fn workspace_symbols_multi_file() {
        let test = CursorTest::builder()
            .source(
                "utils.py",
                "
def utility_function():
    '''A helpful utility function'''
    pass
",
            )
            .source(
                "models.py",
                "
class DataModel:
    '''A data model class'''
    def __init__(self):
        pass
",
            )
            .source(
                "constants.py",
                "
API_BASE_URL = 'https://api.example.com'
<CURSOR>",
            )
            .build();

        assert_snapshot!(test.workspace_symbols("ufunc"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> utils.py:2:5
          |
        2 | def utility_function():
          |     ^^^^^^^^^^^^^^^^
        info: Function utility_function in utils
        ");

        assert_snapshot!(test.workspace_symbols("data"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> models.py:2:7
          |
        2 | class DataModel:
          |       ^^^^^^^^^
        info: Class DataModel in models
        ");

        assert_snapshot!(test.workspace_symbols("apibase"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> constants.py:2:1
          |
        2 | API_BASE_URL = 'https://api.example.com'
          | ^^^^^^^^^^^^
        info: Constant API_BASE_URL in constants
        ");
    }

    #[test]
    fn members() {
        let test = CursorTest::builder()
            .source(
                "utils.py",
                "
class Test:
    def from_path(): ...
<CURSOR>",
            )
            .build();

        assert_snapshot!(test.workspace_symbols("from"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> utils.py:3:9
          |
        3 |     def from_path(): ...
          |         ^^^^^^^^^
        info: Method from_path in utils.Test
        ");
    }

    #[test]
    fn ignore_all() {
        let test = CursorTest::builder()
            .source(
                "utils.py",
                "
__all__ = []
class Test:
    def from_path(): ...
<CURSOR>",
            )
            .build();

        assert_snapshot!(test.workspace_symbols("from"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> utils.py:4:9
          |
        4 |     def from_path(): ...
          |         ^^^^^^^^^
        info: Method from_path in utils.Test
        ");
    }

    #[test]
    fn ignore_imports() {
        let test = CursorTest::builder()
            .source(
                "utils.py",
                "
import re
import json as json
from collections import defaultdict
foo = 1
<CURSOR>",
            )
            .build();

        assert_snapshot!(test.workspace_symbols("foo"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> utils.py:5:1
          |
        5 | foo = 1
          | ^^^
        info: Variable foo in utils
        ");
        assert_snapshot!(test.workspace_symbols("re"), @"No symbols found");
        assert_snapshot!(test.workspace_symbols("json"), @"No symbols found");
        assert_snapshot!(test.workspace_symbols("default"), @"No symbols found");
    }

    /// a query with dots in it names what the symbol is declared in, and an attribute a method
    /// assigns on `self` is a member of the class
    #[test]
    fn qualified_queries_and_instance_attributes() {
        let test = CursorTest::builder()
            .source(
                "shapes/base.py",
                "
class Shape:
    def describe(self) -> str:
        return ''

class Circle(Shape):
    def __init__(self, radius: float) -> None:
        self.radius = radius

    def describe(self) -> str:
        return ''
",
            )
            .source("main.py", "<CURSOR>")
            .build();

        assert_snapshot!(test.workspace_symbols("Shape.describe"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> shapes/base.py:3:9
          |
        3 |     def describe(self) -> str:
          |         ^^^^^^^^
        info: Method describe in shapes.base.Shape
        ");
        assert_snapshot!(test.workspace_symbols("base.Circ"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> shapes/base.py:6:7
          |
        6 | class Circle(Shape):
          |       ^^^^^^
        info: Class Circle in shapes.base
        ");
        assert_snapshot!(test.workspace_symbols("radius"), @"
        info[workspace-symbols]: WorkspaceSymbolInfo
         --> shapes/base.py:8:14
          |
        8 |         self.radius = radius
          |              ^^^^^^
        info: Field radius in shapes.base.Circle
        ");
        assert_snapshot!(test.workspace_symbols("other.describe"), @"No symbols found");
    }

    /// a django template's name has dots of its own, so a dotted query is matched against the
    /// whole of it, as an undotted query is matched against a python name
    #[test]
    fn a_dotted_query_finds_a_django_template() {
        let test = TemplateTest::with_site_packages(
            &[
                (
                    "manage.py",
                    "
                    import os

                    os.environ.setdefault('DJANGO_SETTINGS_MODULE', 'project.settings')
                    ",
                ),
                ("project/__init__.py", ""),
                (
                    "project/settings.py",
                    "
                    INSTALLED_APPS = ['blog']

                    TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]
                    ",
                ),
                ("blog/__init__.py", ""),
                ("blog/templates/base.html", "a"),
                ("blog/templates/blog/post.html", "b"),
            ],
            DJANGO_ADMIN,
        );

        assert_eq!(
            test.workspace_symbols("base.html"),
            ["django template base.html [/src/blog/templates/base.html]"]
        );
        assert_eq!(
            test.workspace_symbols("post.ht"),
            ["django template blog/post.html [/src/blog/templates/blog/post.html]"]
        );
        // a part more than the name has would be what the template is declared in, and a
        // template is declared in nothing
        assert_eq!(
            test.workspace_symbols("templates.base.html"),
            Vec::<String>::new()
        );
    }

    impl CursorTest {
        fn workspace_symbols(&self, query: &str) -> String {
            let symbols = workspace_symbols(&self.db, query);

            if symbols.is_empty() {
                return "No symbols found".to_string();
            }

            self.render_diagnostics(symbols.into_iter().map(WorkspaceSymbolDiagnostic::new))
        }
    }

    struct WorkspaceSymbolDiagnostic {
        symbol_info: WorkspaceSymbolInfo,
    }

    impl WorkspaceSymbolDiagnostic {
        fn new(symbol_info: WorkspaceSymbolInfo) -> Self {
            Self { symbol_info }
        }
    }

    impl IntoDiagnostic for WorkspaceSymbolDiagnostic {
        fn into_diagnostic(self) -> Diagnostic {
            let symbol_kind_str = self.symbol_info.symbol.kind.to_string();

            let mut info_text = format!("{} {}", symbol_kind_str, self.symbol_info.symbol.name);
            if let Some(declared_in) = &self.symbol_info.declared_in {
                info_text.push_str(" in ");
                info_text.push_str(declared_in);
            }

            let sub = SubDiagnostic::new(SubDiagnosticSeverity::Info, info_text);

            let mut main = Diagnostic::new(
                DiagnosticId::Lint(LintName::of("workspace-symbols")),
                Severity::Info,
                "WorkspaceSymbolInfo".to_string(),
            );
            main.annotate(Annotation::primary(
                Span::from(self.symbol_info.file).with_range(self.symbol_info.symbol.name_range),
            ));
            main.sub(sub);

            main
        }
    }
}
