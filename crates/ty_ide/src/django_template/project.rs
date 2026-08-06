//! the django definitions a template needs from the project's python source
//!
//! everything a template can say that isn't a builtin comes from somewhere in
//! the project: `{% load %}`ed tags and filters from its `templatetags` modules,
//! `{% url %}` names from its url configuration, `{% extends %}` targets from its
//! template directories, and the variables in `{{ }}` from whichever view renders
//! the template. this module finds all of it.
//!
//! each source file is scanned by its own salsa query, and the project-wide
//! answers are the union of those. that keeps an edit to one view from re-walking
//! the whole project — and since the scans run off `parsed_module`, which the
//! type checker has already populated for every project file, the ast work is
//! shared rather than duplicated.

use std::sync::Mutex;

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::{File, FilePath, system_path_to_file};
use ruff_db::parsed::parsed_module;
use ruff_db::system::walk_directory::WalkState;
use ruff_db::system::{FileType, SystemPath, SystemPathBuf};
use ruff_python_ast::visitor::{Visitor, walk_expr, walk_stmt};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};
use ty_project::{Db, Project};

/// the directory name django's app-directories template loader looks in
const TEMPLATE_DIRECTORY: &str = "templates";

/// the directory name django's `staticfiles` app-directories finder looks in
const STATIC_DIRECTORY: &str = "static";

/// the package name a project's template tag libraries live in
const TEMPLATETAGS_PACKAGE: &str = "templatetags";

/// how deep below the project root a `templates`/`static` directory is looked for
///
/// django's own layout puts them at `<project>/<app>/templates`, and a
/// `<project>/src/<app>/templates` is about as deep as real projects go. bounding
/// the walk keeps a large repository's `node_modules` or virtualenv from being
/// crawled in full.
const DISCOVERY_DEPTH: usize = 5;

/// whether a tag or a filter was registered
#[derive(Debug, Clone, Copy, PartialEq, Eq, get_size2::GetSize)]
pub(crate) enum RegistrationKind {
    /// `@register.simple_tag` and friends. `block` is set for the
    /// `simple_block_tag` form, which takes a body and so needs an `{% end… %}`.
    Tag { block: bool },
    /// `@register.filter`
    Filter,
}

/// a tag or filter one of the project's `templatetags` modules registers
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct Registration {
    pub(crate) name: CompactString,
    pub(crate) kind: RegistrationKind,
    /// the name a `{% load %}` uses, i.e. the module's own file stem
    pub(crate) library: CompactString,
    pub(crate) file: File,
    /// the registered function's name, for navigation
    pub(crate) range: TextRange,
    pub(crate) documentation: Option<Box<str>>,
}

/// a name the project's url configuration gives a route
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct UrlName {
    /// the name a `{% url %}` reverses, namespace included
    pub(crate) name: CompactString,
    pub(crate) file: File,
    /// the `name=` literal, for navigation
    pub(crate) range: TextRange,
    /// the route pattern, shown alongside the completion
    pub(crate) route: Option<Box<str>>,
}

/// a name a view puts in a template's context
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct ContextVariable {
    pub(crate) name: CompactString,
    pub(crate) file: File,
    /// the key or attribute that declares the name, for navigation
    pub(crate) range: TextRange,
    /// the expression the view binds to the name
    ///
    /// this is what the completions infer the variable's type from, which is how
    /// a django model in the context brings its fields with it.
    pub(crate) value: Option<TextRange>,
}

/// the context one template is rendered with, from one place that renders it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct TemplateContext {
    /// the template the view names, as the loader sees it
    pub(crate) template: CompactString,
    pub(crate) variables: Box<[ContextVariable]>,
}

/// a file inside one of the project's template directories
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub(crate) struct DiscoveredFile {
    /// the path relative to the directory it was found under, which is the name
    /// `{% extends %}`, `{% include %}` and `{% static %}` all use
    pub(crate) name: CompactString,
    pub(crate) path: SystemPathBuf,
}

/// every file under a `templates` directory of the project
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(db, project, TEMPLATE_DIRECTORY)
}

/// every file under a `static` directory of the project
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn static_files(db: &dyn Db, project: Project) -> Box<[DiscoveredFile]> {
    discover(db, project, STATIC_DIRECTORY)
}

/// the file a template name resolves to
pub(crate) fn resolve_template(db: &dyn Db, name: &str) -> Option<File> {
    let path = template_files(db, db.project())
        .iter()
        .find(|candidate| candidate.name == name)
        .map(|candidate| candidate.path.clone())?;

    system_path_to_file(db, &path).ok()
}

/// every tag and filter the project registers
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn registrations(db: &dyn Db, project: Project) -> Box<[Registration]> {
    project_scan(db, project, |db, file| {
        // only a module inside a `templatetags` package is a tag library, so
        // there is no point parsing anything else
        is_templatetags_module(db, file)
            .then(|| registrations_in_file(db, file).iter().cloned())
            .into_iter()
            .flatten()
    })
}

/// every name the project's url configuration defines
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn url_names(db: &dyn Db, project: Project) -> Box<[UrlName]> {
    project_scan(db, project, |db, file| {
        url_names_in_file(db, file).iter().cloned()
    })
}

/// every template context the project's views build
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
pub(crate) fn template_contexts(db: &dyn Db, project: Project) -> Box<[TemplateContext]> {
    project_scan(db, project, |db, file| {
        template_contexts_in_file(db, file).iter().cloned()
    })
}

/// the names every view that renders `template` puts in its context
///
/// two views rendering the same template contribute both their contexts, since
/// either may be the one running. a name several views agree on appears once.
pub(crate) fn context_for_template<'db>(
    db: &'db dyn Db,
    template: &str,
) -> Vec<&'db ContextVariable> {
    let mut seen: Vec<&ContextVariable> = Vec::new();

    for context in template_contexts(db, db.project())
        .iter()
        .filter(|context| context.template == template)
    {
        for variable in &context.variables {
            if !seen.iter().any(|existing| existing.name == variable.name) {
                seen.push(variable);
            }
        }
    }

    seen
}

/// run `scan` over every first-party source file of the project
fn project_scan<'db, T, I>(
    db: &'db dyn Db,
    project: Project,
    scan: impl Fn(&'db dyn Db, File) -> I,
) -> Box<[T]>
where
    I: IntoIterator<Item = T>,
{
    let mut found = Vec::new();

    for file in &project.files(db) {
        // a stub declares types, never a url pattern or a rendered template
        if file.path(db).extension() == Some("pyi") {
            continue;
        }
        found.extend(scan(db, file));
    }

    found.into_boxed_slice()
}

/// find every file under a directory named `directory`, anywhere in the project
///
/// the walk is bounded (see [`DISCOVERY_DEPTH`]) and respects the project's
/// ignore-file settings, so a repository's dependencies are not crawled.
fn discover(db: &dyn Db, project: Project, directory: &str) -> Box<[DiscoveredFile]> {
    let root = project.root(db);
    let found = Mutex::new(Vec::new());

    db.system()
        .walk_directory(root)
        .standard_filters(project.settings(db).src().respect_ignore_files)
        .ignore_hidden(true)
        .run(|| {
            Box::new(|entry| {
                let Ok(entry) = entry else {
                    return WalkState::Continue;
                };

                let inside = entry
                    .path()
                    .ancestors()
                    .any(|ancestor| ancestor.file_name() == Some(directory));

                match entry.file_type() {
                    FileType::Directory => {
                        // stop descending once the entry is neither a candidate
                        // root nor already inside one
                        if !inside && entry.depth() >= DISCOVERY_DEPTH {
                            return WalkState::Skip;
                        }
                        WalkState::Continue
                    }
                    FileType::File => {
                        if inside && let Some(name) = relative_to_directory(entry.path(), directory)
                        {
                            found.lock().unwrap().push(DiscoveredFile {
                                name,
                                path: entry.path().to_path_buf(),
                            });
                        }
                        WalkState::Continue
                    }
                    FileType::Symlink => WalkState::Continue,
                }
            })
        });

    let mut found = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    found.sort_by(|left, right| left.name.cmp(&right.name).then(left.path.cmp(&right.path)));
    found.dedup_by(|left, right| left.name == right.name && left.path == right.path);
    found.into_boxed_slice()
}

/// the part of `path` below its innermost `directory` ancestor
///
/// `/app/templates/blog/post.html` under `templates` is `blog/post.html`, which
/// is exactly the name a `{% include %}` writes.
fn relative_to_directory(path: &SystemPath, directory: &str) -> Option<CompactString> {
    let root = path
        .ancestors()
        .find(|ancestor| ancestor.file_name() == Some(directory))?;

    Some(
        path.strip_prefix(root)
            .ok()?
            .as_str()
            .replace('\\', "/")
            .to_compact_string(),
    )
}

/// whether `file` is a module of a `templatetags` package
fn is_templatetags_module(db: &dyn Db, file: File) -> bool {
    let FilePath::System(path) = file.path(db) else {
        return false;
    };

    path.parent()
        .is_some_and(|parent| parent.file_name() == Some(TEMPLATETAGS_PACKAGE))
        && path.file_stem() != Some("__init__")
}

/// the tags and filters one `templatetags` module registers
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn registrations_in_file(db: &dyn Db, file: File) -> Box<[Registration]> {
    let Some(library) = file
        .path(db)
        .as_system_path()
        .and_then(SystemPath::file_stem)
    else {
        return Box::default();
    };
    let library = library.to_compact_string();

    let parsed = parsed_module(db, file).load(db);
    let mut visitor = RegistrationVisitor {
        file,
        library,
        registers: library_names(parsed.suite()),
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

/// the names the module binds a `template.Library()` to
///
/// django's own documentation always calls it `register`, and code that doesn't
/// is rare enough that the conventional name is the right fallback when no
/// `Library()` call is found at all.
fn library_names(body: &[Stmt]) -> Vec<CompactString> {
    let mut names: Vec<CompactString> = body
        .iter()
        .filter_map(|statement| {
            let Stmt::Assign(assign) = statement else {
                return None;
            };
            let Expr::Call(call) = &*assign.value else {
                return None;
            };
            (callee_name(&call.func)? == "Library").then_some(&assign.targets)
        })
        .flatten()
        .filter_map(|target| match target {
            Expr::Name(name) => Some(name.id.to_compact_string()),
            _ => None,
        })
        .collect();

    if names.is_empty() {
        names.push("register".to_compact_string());
    }
    names
}

struct RegistrationVisitor {
    file: File,
    library: CompactString,
    registers: Vec<CompactString>,
    found: Vec<Registration>,
}

impl RegistrationVisitor {
    /// the registration `decorator` makes of the function named `function`
    fn registration(
        &self,
        decorator: &ast::Decorator,
        function: &ast::Identifier,
        documentation: Option<Box<str>>,
    ) -> Option<Registration> {
        // `@register.filter` and `@register.filter(name="x")` both apply
        let (attribute, call) = match &decorator.expression {
            Expr::Attribute(attribute) => (attribute, None),
            Expr::Call(call) => match &*call.func {
                Expr::Attribute(attribute) => (attribute, Some(call)),
                _ => return None,
            },
            _ => return None,
        };

        let Expr::Name(object) = &*attribute.value else {
            return None;
        };
        if !self.registers.iter().any(|name| name == object.id.as_str()) {
            return None;
        }

        let kind = match attribute.attr.as_str() {
            "filter" | "filter_function" => RegistrationKind::Filter,
            "simple_block_tag" => RegistrationKind::Tag { block: true },
            "tag" | "simple_tag" | "inclusion_tag" => RegistrationKind::Tag { block: false },
            _ => return None,
        };

        Some(Registration {
            name: registered_name(attribute.attr.as_str(), call, function),
            kind,
            library: self.library.clone(),
            file: self.file,
            range: function.range(),
            documentation,
        })
    }
}

impl<'ast> Visitor<'ast> for RegistrationVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::FunctionDef(function) = stmt {
            let documentation = docstring_summary(&function.body);
            // one function is one tag even if it somehow carries two registering
            // decorators; the first is the one that names it
            let registration = function.decorator_list.iter().find_map(|decorator| {
                self.registration(decorator, &function.name, documentation.clone())
            });
            self.found.extend(registration);
            return;
        }

        walk_stmt(self, stmt);
    }
}

/// the name a registration goes by
///
/// `@register.inclusion_tag("card.html")` names a *template* with its first
/// argument, not the tag, which is the one case where the positional argument
/// must not be read as a name.
fn registered_name(
    attribute: &str,
    call: Option<&ast::ExprCall>,
    function: &ast::Identifier,
) -> CompactString {
    let Some(call) = call else {
        return function.id.to_compact_string();
    };

    if let Some(name) = call
        .arguments
        .find_keyword("name")
        .and_then(|keyword| string_literal(&keyword.value))
    {
        return name;
    }

    if attribute != "inclusion_tag"
        && let Some(name) = call.arguments.args.first().and_then(string_literal)
    {
        return name;
    }

    function.id.to_compact_string()
}

/// the url names one module defines
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn url_names_in_file(db: &dyn Db, file: File) -> Box<[UrlName]> {
    let parsed = parsed_module(db, file).load(db);

    // django namespaces an included module's names under its `app_name`
    let namespace = parsed.suite().iter().find_map(|statement| {
        let Stmt::Assign(assign) = statement else {
            return None;
        };
        let [Expr::Name(target)] = assign.targets.as_slice() else {
            return None;
        };
        (target.id == "app_name").then(|| string_literal(&assign.value))?
    });

    let mut visitor = UrlVisitor {
        file,
        namespace,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct UrlVisitor {
    file: File,
    namespace: Option<CompactString>,
    found: Vec<UrlName>,
}

impl<'ast> Visitor<'ast> for UrlVisitor {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr
            && matches!(
                callee_name(&call.func).as_deref(),
                Some("path" | "re_path" | "url")
            )
            && let Some(keyword) = call.arguments.find_keyword("name")
            && let Some(name) = string_literal(&keyword.value)
        {
            let name = match &self.namespace {
                Some(namespace) => format!("{namespace}:{name}").to_compact_string(),
                None => name,
            };

            self.found.push(UrlName {
                name,
                file: self.file,
                range: keyword.value.range(),
                route: call
                    .arguments
                    .args
                    .first()
                    .and_then(string_literal)
                    .map(|route| Box::from(route.as_str())),
            });
        }

        walk_expr(self, expr);
    }
}

/// the template contexts one module builds
#[salsa::tracked(returns(ref), heap_size=ruff_memory_usage::heap_size)]
fn template_contexts_in_file(db: &dyn Db, file: File) -> Box<[TemplateContext]> {
    let parsed = parsed_module(db, file).load(db);

    let mut visitor = ContextVisitor {
        file,
        found: Vec::new(),
    };
    visitor.visit_body(parsed.suite());

    visitor.found.into_boxed_slice()
}

struct ContextVisitor {
    file: File,
    found: Vec<TemplateContext>,
}

impl ContextVisitor {
    /// the context a `render()`/`TemplateResponse()` call passes
    ///
    /// both take the request first, the template second and the context third,
    /// and both accept those last two by keyword as well.
    fn render_call(&self, call: &ast::ExprCall) -> Option<TemplateContext> {
        let callee = callee_name(&call.func)?;
        let (template_index, context_index) = match callee.as_str() {
            "render" | "TemplateResponse" => (1, 2),
            _ => return None,
        };

        let template = call
            .arguments
            .find_keyword("template_name")
            .map(|keyword| &keyword.value)
            .or_else(|| call.arguments.args.get(template_index))
            .and_then(string_literal)?;

        let context = call
            .arguments
            .find_keyword("context")
            .map(|keyword| &keyword.value)
            .or_else(|| call.arguments.args.get(context_index));

        Some(TemplateContext {
            template,
            variables: context
                .map(|context| self.dict_variables(context))
                .unwrap_or_default(),
        })
    }

    /// the names a dict literal binds
    fn dict_variables(&self, expr: &Expr) -> Box<[ContextVariable]> {
        let Expr::Dict(dict) = expr else {
            return Box::default();
        };

        dict.items
            .iter()
            .filter_map(|item| {
                let key = item.key.as_ref()?;
                Some(ContextVariable {
                    name: string_literal(key)?,
                    file: self.file,
                    range: key.range(),
                    value: Some(item.value.range()),
                })
            })
            .collect()
    }

    /// the context a class-based view declares
    ///
    /// django's generic views name their template with `template_name` and their
    /// object with `context_object_name`, and add anything else through
    /// `extra_context` or by writing into the dict `get_context_data` returns.
    fn class_based_view(&self, class: &ast::StmtClassDef) -> Option<TemplateContext> {
        let template = class
            .body
            .iter()
            .find_map(|statement| class_attribute(statement, "template_name"))
            .and_then(|(value, _)| string_literal(value))?;

        let mut variables = Vec::new();

        for statement in &class.body {
            if let Some((value, range)) = class_attribute(statement, "context_object_name")
                && let Some(name) = string_literal(value)
            {
                variables.push(ContextVariable {
                    name,
                    file: self.file,
                    range,
                    // the object itself is what the view will bind; its type
                    // comes from the view's generics, which is beyond what a
                    // syntactic scan can follow
                    value: None,
                });
            }

            if let Some((value, _)) = class_attribute(statement, "extra_context") {
                variables.extend(self.dict_variables(value));
            }

            if let Stmt::FunctionDef(function) = statement
                && function.name.as_str() == "get_context_data"
            {
                variables.extend(self.context_assignments(&function.body));
            }
        }

        Some(TemplateContext {
            template,
            variables: variables.into_boxed_slice(),
        })
    }

    /// the names a `get_context_data` body writes into its context dict
    fn context_assignments(&self, body: &[Stmt]) -> Vec<ContextVariable> {
        let mut found = Vec::new();

        let mut visitor = ContextAssignmentVisitor {
            file: self.file,
            found: &mut found,
        };
        visitor.visit_body(body);

        found
    }
}

impl<'ast> Visitor<'ast> for ContextVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::ClassDef(class) = stmt {
            self.found.extend(self.class_based_view(class));
            return;
        }

        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'ast Expr) {
        if let Expr::Call(call) = expr {
            self.found.extend(self.render_call(call));
        }

        walk_expr(self, expr);
    }
}

/// collects `context["name"] = value` writes
struct ContextAssignmentVisitor<'a> {
    file: File,
    found: &'a mut Vec<ContextVariable>,
}

impl<'ast> Visitor<'ast> for ContextAssignmentVisitor<'_> {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        if let Stmt::Assign(assign) = stmt
            && let [Expr::Subscript(subscript)] = assign.targets.as_slice()
            && let Some(name) = string_literal(&subscript.slice)
        {
            self.found.push(ContextVariable {
                name,
                file: self.file,
                range: subscript.slice.range(),
                value: Some(assign.value.range()),
            });
        }

        walk_stmt(self, stmt);
    }
}

/// the value and name range of a class-body assignment to `attribute`
fn class_attribute<'ast>(
    statement: &'ast Stmt,
    attribute: &str,
) -> Option<(&'ast Expr, TextRange)> {
    match statement {
        Stmt::Assign(assign) => {
            let [Expr::Name(target)] = assign.targets.as_slice() else {
                return None;
            };
            (target.id == attribute).then(|| (&*assign.value, target.range()))
        }
        Stmt::AnnAssign(assign) => {
            let Expr::Name(target) = &*assign.target else {
                return None;
            };
            let value = assign.value.as_deref()?;
            (target.id == attribute).then(|| (value, target.range()))
        }
        _ => None,
    }
}

/// the final segment of a callee, so that `render` and `shortcuts.render` are one
fn callee_name(func: &Expr) -> Option<CompactString> {
    match func {
        Expr::Name(name) => Some(name.id.to_compact_string()),
        Expr::Attribute(attribute) => Some(attribute.attr.id.to_compact_string()),
        _ => None,
    }
}

/// the value of a plain string literal
fn string_literal(expr: &Expr) -> Option<CompactString> {
    match expr {
        Expr::StringLiteral(literal) => Some(literal.value.to_str().to_compact_string()),
        _ => None,
    }
}

/// the first paragraph of a body's docstring
fn docstring_summary(body: &[Stmt]) -> Option<Box<str>> {
    let Stmt::Expr(first) = body.first()? else {
        return None;
    };
    let Expr::StringLiteral(literal) = &*first.value else {
        return None;
    };

    let text = literal.value.to_str();
    let summary: String = text
        .lines()
        .map(str::trim)
        .take_while(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");

    (!summary.is_empty()).then(|| Box::from(summary.as_str()))
}
