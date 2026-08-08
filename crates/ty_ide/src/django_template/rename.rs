//! renaming the django names that are written in two languages
//!
//! a `{% block %}` name is written in the template that declares it and in every
//! template that overrides it; a route name in the `path()` that declares it and
//! in every `{% url %}` and `reverse()` that reaches it; a template name in every
//! `{% extends %}`, `{% include %}` and `render()` that loads it, and in the name
//! of the file itself. renaming any of them by hand means finding all the others
//! by hand, which is what makes this worth doing — and what makes it dangerous.
//! a rename that misses one occurrence leaves a project that no longer renders,
//! and nothing about the result looks wrong.
//!
//! so every path through this module ends in one of two places: a complete set of
//! edits, or a refusal that says why. an index that does not answer for the whole
//! project, an inheritance tree with a gap in it, a name written in a form
//! nothing here could rewrite — each of those refuses the whole rename rather
//! than applying the visible part of it.
//!
//! finding the occurrences is [`super::uses`]' job, which references asks the same
//! question of. this module is what turns each of them into an edit or into the
//! refusal it is evidence for; the limit that leaves is a name assembled at run
//! time out of pieces — `f"blog:{kind}"`, `"blog/" + page` — which spells nothing
//! anything can see.

use compact_str::{CompactString, ToCompactString};
use ruff_db::files::{File, FileRange};
use ruff_db::source::source_text;
use ruff_db::system::SystemPathBuf;
use ruff_text_size::{Ranged, TextRange, TextSize};
use ty_project::Db;

use super::project::{self, NAMESPACE_SEPARATOR};
use super::uses::{self, Anchor, FRAGMENT_SEPARATOR, Named, Use, Written};

/// what renaming a django name would do
#[derive(Debug, PartialEq, Eq)]
pub struct TemplateRename {
    /// every range whose text the new name replaces
    pub edits: Vec<FileRange>,
    /// the template file to move, and the path it moves to
    pub file_rename: Option<(SystemPathBuf, SystemPathBuf)>,
}

/// what the editor should offer for a rename at a position
#[derive(Debug, PartialEq, Eq)]
pub enum PreparedTemplateRename {
    /// the range the editor offers, and the text it starts as
    Ready {
        range: TextRange,
        placeholder: String,
    },
    /// why the rename cannot be done
    Refused(String),
}

/// the answer to a rename request
#[derive(Debug, PartialEq, Eq)]
pub enum TemplateRenameOutcome {
    Edits(TemplateRename),
    /// why the rename cannot be done
    Refused(String),
}

/// whether the name at `offset` can be renamed, and what the editor should offer
///
/// `None` is a position that names nothing django knows, which is what leaves the
/// python services to answer for it.
pub(super) fn prepare(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    template: bool,
) -> Option<PreparedTemplateRename> {
    Some(match plan(db, file, offset, template)? {
        Ok(plan) => PreparedTemplateRename::Ready {
            range: plan.range,
            placeholder: plan.placeholder.to_string(),
        },
        Err(refusal) => PreparedTemplateRename::Refused(refusal),
    })
}

/// every edit renaming the name at `offset` to `new_name` would make
pub(super) fn rename(
    db: &dyn Db,
    file: File,
    offset: TextSize,
    new_name: &str,
    template: bool,
) -> Option<TemplateRenameOutcome> {
    Some(match plan(db, file, offset, template)? {
        Ok(plan) => match plan.finish(db, new_name) {
            Ok(rename) => TemplateRenameOutcome::Edits(rename),
            Err(refusal) => TemplateRenameOutcome::Refused(refusal),
        },
        Err(refusal) => TemplateRenameOutcome::Refused(refusal),
    })
}

/// what a rename at a position would touch, worked out before a new name is known
///
/// everything that decides whether the rename is possible at all is settled here,
/// so that `prepareRename` refuses exactly what `rename` would.
struct Plan {
    /// the range the editor offers, in the file the request came from
    range: TextRange,
    placeholder: CompactString,
    edits: Vec<FileRange>,
    kind: Kind,
}

/// what is being renamed, and what settling on a new name still needs
enum Kind {
    /// the templates that declare the block, which is where a name already taken
    /// would collide
    Block {
        declaring: Vec<File>,
    },
    Route,
    /// the file to move, and the directory its name is relative to
    Template {
        path: SystemPathBuf,
        root: SystemPathBuf,
    },
}

fn plan(db: &dyn Db, file: File, offset: TextSize, template: bool) -> Option<Result<Plan, String>> {
    let (named, range) = uses::named_at(db, file, offset, template)?;

    Some(match named {
        Named::Block(name) => block_plan(db, file, &name, range),
        Named::Route(name) => route_plan(db, &Anchor::Use { name }, range),
        Named::RouteDeclaration => route_plan(db, &Anchor::Declaration { file, range }, range),
        Named::Template(name) => template_plan(db, &name, range),
        // a tag and a filter are named by the python that registers them, which is
        // the python rename's to carry out rather than this one's
        Named::Tag(_) | Named::Filter(_) => return None,
    })
}

// ---------------------------------------------------------------------------
// a `{% block %}` name
// ---------------------------------------------------------------------------

fn block_plan(db: &dyn Db, file: File, name: &str, range: TextRange) -> Result<Plan, String> {
    let found = uses::block(db, file, name);
    if let Some(refusal) = found.incomplete {
        return Err(refusal);
    }

    // an installed app's template overriding this block would have to be rewritten
    // with the rest, and it is not the project's to rewrite
    if let Some(installed) = found.found.iter().find(|used| !used.own) {
        return Err(format!(
            "`{}` declares this block too, and an installed app's template is not the project's \
             to rewrite",
            uses::path_of(db, installed.file)
        ));
    }

    let mut declaring: Vec<File> = found.found.iter().map(|used| used.file).collect();
    declaring.dedup();
    // a clash the request's own template already holds is the one to say so about
    declaring.sort_by_key(|member| *member != file);

    let edits = found
        .found
        .iter()
        .map(|used| FileRange::new(used.file, used.range))
        .collect();

    Ok(Plan {
        range,
        placeholder: name.to_compact_string(),
        edits,
        kind: Kind::Block { declaring },
    })
}

// ---------------------------------------------------------------------------
// a route name
// ---------------------------------------------------------------------------

fn route_plan(db: &dyn Db, anchor: &Anchor, range: TextRange) -> Result<Plan, String> {
    let route = uses::route(db, anchor);
    if let Some(refusal) = route.uses.incomplete {
        return Err(refusal);
    }
    // every way a route can fail to be pinned down to one declaration is one
    // `incomplete` has already refused over
    let Some(declared) = route.declared else {
        return Err("this route has no one declaration of its own".to_string());
    };

    if !declared.exact {
        return Err(
            "this route's name is generated from a rest framework router's basename rather than \
             written out, so renaming it would mean renaming the basename and every route \
             generated from it"
                .to_string(),
        );
    }

    let Some(bare) = declared.name else {
        return Err(
            "the route's name is not written as one plain string literal, so it cannot be \
             rewritten"
                .to_string(),
        );
    };
    if !fits(declared.range, &bare) {
        return Err(
            "the route's name is written with an escape, so the name inside the literal cannot \
             be located"
                .to_string(),
        );
    }

    let qualifier = format!("{NAMESPACE_SEPARATOR}{bare}");
    if !route
        .names
        .iter()
        .all(|name| *name == bare || name.ends_with(&qualifier))
    {
        return Err(format!(
            "`{bare}` is reversed under a name that does not end in it, so the two cannot be \
             renamed together"
        ));
    }

    let mut edits = Vec::new();
    let names: Vec<&str> = route.names.iter().map(CompactString::as_str).collect();

    for used in &route.uses.found {
        match &used.written {
            Written::Whole if used.declaration => edits.push(FileRange::new(used.file, used.range)),
            Written::Whole if used.own => {
                edits.push(FileRange::new(
                    used.file,
                    replaced(db, used, &names, &bare)?,
                ));
            }
            Written::Whole => {
                return Err(format!(
                    "{} reverses this route, and an installed app's template is not the \
                     project's to rewrite",
                    position(db, used.file, used.range)
                ));
            }
            Written::Pieces => {
                return Err(format!(
                    "{} reverses this route under a name written in two pieces, which no one \
                     range could replace",
                    position(db, used.file, used.range)
                ));
            }
            Written::Unknown if used.template => {
                return Err(format!(
                    "{} reverses a route whose name is a variable, so nothing here can tell \
                     whether it is this one",
                    position(db, used.file, used.range)
                ));
            }
            Written::Unknown => {
                return Err(format!(
                    "{} reverses a route whose name is worked out at run time, so nothing here \
                     can tell whether it is this one",
                    position(db, used.file, used.range)
                ));
            }
            Written::Stray => {
                return Err(format!(
                    "{} writes this route's name somewhere no rename would rewrite it",
                    position(db, used.file, used.range)
                ));
            }
            Written::Bound(bound_to) => {
                if let Some(refusal) = bound_elsewhere(db, used, bound_to, &route.uses.found) {
                    return Err(refusal);
                }
            }
            Written::Itself => {}
        }
    }

    sort(db, &mut edits);

    Ok(Plan {
        range: match anchor {
            Anchor::Declaration { .. } => declared.range,
            Anchor::Use { name } => tail(range, name, &bare)?,
        },
        placeholder: bare,
        edits,
        kind: Kind::Route,
    })
}

/// the range of `used` a rename replaces
///
/// a use whose source text does not spell one of `names` outright is one written
/// with an escape, and the arithmetic that finds a name inside a literal would
/// then land in the wrong place.
fn replaced(db: &dyn Db, used: &Use, names: &[&str], bare: &str) -> Result<TextRange, String> {
    let source = source_text(db, used.file);
    let written = &source[used.range];

    if !names.contains(&written) {
        return Err(
            "a route name is written with an escape in it, so the name inside the literal cannot \
             be located"
                .to_string(),
        );
    }

    tail(used.range, written, bare)
}

/// the part of the literal spanning `range` that writes the bare name
///
/// every use writes the name django reverses it by, which is qualified by the
/// namespaces it is mounted under, while the declaration writes only the bare
/// name — so a rename that reads the same at both ends replaces the tail.
fn tail(range: TextRange, name: &str, bare: &str) -> Result<TextRange, String> {
    if !fits(range, name) {
        return Err(
            "a route name is written with an escape in it, so the name inside the literal cannot \
             be located"
                .to_string(),
        );
    }

    let bare = TextSize::try_from(bare.len()).map_err(|_| "the name is too long".to_string())?;
    Ok(TextRange::new(range.end() - bare, range.end()))
}

/// whether a literal's contents are written exactly as its value reads
///
/// an escape makes the source longer than the value, and the arithmetic that
/// finds a name inside the literal would then land in the wrong place.
fn fits(range: TextRange, value: &str) -> bool {
    usize::from(range.len()) == value.len()
}

// ---------------------------------------------------------------------------
// a template name
// ---------------------------------------------------------------------------

fn template_plan(db: &dyn Db, name: &str, range: TextRange) -> Result<Plan, String> {
    let loaded = uses::template(db, name);
    if let Some(refusal) = loaded.uses.incomplete {
        return Err(refusal);
    }
    let Some(discovered) = loaded.discovered else {
        return Err(format!(
            "no template of the project is loadable as `{name}`"
        ));
    };
    let Some(root) = discovered.root() else {
        return Err(format!(
            "`{name}` is not below the template directory it was found under"
        ));
    };

    let mut edits = Vec::new();

    for used in &loaded.uses.found {
        match &used.written {
            Written::Whole if used.own => edits.push(FileRange::new(used.file, used.range)),
            Written::Whole => {
                return Err(format!(
                    "{} loads this template, and an installed app's template is not the \
                     project's to rewrite",
                    position(db, used.file, used.range)
                ));
            }
            Written::Pieces => {
                return Err(format!(
                    "{} names this template in two pieces, which no one range could replace",
                    position(db, used.file, used.range)
                ));
            }
            Written::Unknown if used.template => {
                return Err(format!(
                    "{} extends or includes a template whose name is a variable, so nothing here \
                     can tell whether it is this one",
                    position(db, used.file, used.range)
                ));
            }
            Written::Unknown => {
                return Err(format!(
                    "{} renders a template whose name is worked out at run time, so nothing here \
                     can tell whether it is this one",
                    position(db, used.file, used.range)
                ));
            }
            Written::Stray => {
                return Err(format!(
                    "{} writes this template's name somewhere no rename would rewrite it",
                    position(db, used.file, used.range)
                ));
            }
            Written::Bound(bound_to) => {
                if let Some(refusal) = bound_elsewhere(db, used, bound_to, &loaded.uses.found) {
                    return Err(refusal);
                }
            }
            // the file the name loads is moved rather than edited
            Written::Itself => {}
        }
    }

    sort(db, &mut edits);

    Ok(Plan {
        range,
        placeholder: name.to_compact_string(),
        edits,
        kind: Kind::Template {
            path: discovered.path.clone(),
            root: root.to_path_buf(),
        },
    })
}

// ---------------------------------------------------------------------------
// settling on a new name
// ---------------------------------------------------------------------------

impl Plan {
    fn finish(self, db: &dyn Db, new_name: &str) -> Result<TemplateRename, String> {
        if new_name.is_empty() || new_name.chars().any(char::is_whitespace) {
            return Err("a django name cannot be empty or hold a space".to_string());
        }
        if new_name.contains(['{', '}', '%', '"', '\'', '\\']) {
            return Err(
                "a django name cannot hold a quote, a backslash or a template delimiter"
                    .to_string(),
            );
        }

        let file_rename = match &self.kind {
            // one template declaring a block twice is one django refuses to
            // render, so a name already taken in a template this would edit is a
            // rename that cannot be completed
            Kind::Block { declaring } => {
                if let Some(clash) = declaring
                    .iter()
                    .find(|member| uses::defines_block(db, **member, new_name))
                {
                    return Err(format!(
                        "`{}` already declares a block named `{new_name}`, and django renders no \
                         template that declares one twice",
                        uses::path_of(db, *clash)
                    ));
                }
                None
            }
            Kind::Route => {
                if new_name.contains(NAMESPACE_SEPARATOR) {
                    return Err(format!(
                        "a route's own name holds no `{NAMESPACE_SEPARATOR}` — the namespaces in \
                         front of it come from where its url configuration is included"
                    ));
                }
                None
            }
            Kind::Template { path, root } => {
                if new_name.contains(FRAGMENT_SEPARATOR) {
                    return Err(format!(
                        "a template's name holds no `{FRAGMENT_SEPARATOR}`, which addresses a \
                         partial inside one"
                    ));
                }
                if new_name.starts_with('/')
                    || new_name.ends_with('/')
                    || new_name
                        .split('/')
                        .any(|segment| segment.is_empty() || segment == "..")
                {
                    return Err(
                        "a template's name is a path relative to its template directory"
                            .to_string(),
                    );
                }
                if project::template_files(db, db.project())
                    .iter()
                    .any(|discovered| discovered.name == new_name)
                {
                    return Err(format!("a template is already loadable as `{new_name}`"));
                }

                Some((path.clone(), root.join(new_name)))
            }
        };

        Ok(TemplateRename {
            edits: self.edits,
            file_rename,
        })
    }
}

/// why the rename cannot go ahead, when a constant binds the name somewhere no
/// other occurrence already covers
///
/// a constant carries the name somewhere this cannot follow — into a `render()`
/// as a variable, into a helper, into a setting — so the rename stops at the
/// binding and says which one. a `template_name = "…"` is a binding *and* a
/// position the rename already rewrites, which is no reason to refuse.
fn bound_elsewhere(db: &dyn Db, binding: &Use, bound_to: &str, found: &[Use]) -> Option<String> {
    let rewritten = found.iter().any(|used| {
        matches!(used.written, Written::Whole)
            && used.file == binding.file
            && binding.range.contains_range(used.range)
    });

    (!rewritten).then(|| {
        format!(
            "{} binds this name to `{bound_to}`, and a rename cannot follow where a constant \
             carries it",
            position(db, binding.file, binding.range)
        )
    })
}

/// how a refusal names the place it is refusing over
fn position(db: &dyn Db, file: File, range: TextRange) -> String {
    let line = source_text(db, file)[..usize::from(range.start())]
        .matches('\n')
        .count()
        + 1;

    format!("`{}:{line}`", uses::path_of(db, file))
}

/// put the edits in a stable order, so that two runs answer the same
fn sort(db: &dyn Db, edits: &mut [FileRange]) {
    edits.sort_by(|left, right| {
        uses::path_of(db, left.file())
            .cmp(&uses::path_of(db, right.file()))
            .then(left.range().start().cmp(&right.range().start()))
    });
}

#[cfg(test)]
mod tests {
    use crate::django_template::tests::TemplateTest;

    /// a whole django project, with `sources` written into it
    ///
    /// the settings module the convention finds, and every app it names being
    /// resolvable, is what makes the indexes authoritative — and every rename
    /// here depends on that, since a rename that cannot see the whole project is
    /// one that refuses.
    fn project(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
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

                ROOT_URLCONF = 'project.urls'
                ",
            ),
            (
                "project/urls.py",
                "
                from django.urls import include, path

                urlpatterns = [path('blog/', include('blog.urls'))]
                ",
            ),
            ("blog/__init__.py", ""),
        ];
        all.extend_from_slice(sources);

        TemplateTest::with_site_packages(&all, &[])
    }

    /// the same project with a url configuration, a view and a base template
    fn blog(sources: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
            (
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [
                    path('', index, name='index'),
                    path('<int:pk>/', detail, name='detail'),
                ]
                ",
            ),
            (
                "blog/templates/blog/base.html",
                "{% block content %}{% endblock content %}{% block footer %}{% endblock %}",
            ),
        ];
        all.extend_from_slice(sources);

        project(&all)
    }

    // -----------------------------------------------------------------------
    // a `{% block %}` name
    // -----------------------------------------------------------------------

    #[test]
    fn renaming_a_block_in_a_base_renames_every_child_that_overrides_it() {
        let test = blog(&[
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% block content %}a{% endblock %}",
            ),
            (
                "blog/templates/blog/list.html",
                "{% extends 'blog/base.html' %}{% block content %}b{% endblock content %}",
            ),
            (
                "blog/templates/blog/edit.html",
                "{% extends 'blog/base.html' %}{% block co<CURSOR>ntent %}c{% endblock %}",
            ),
        ]);

        assert_eq!(
            test.rename("body"),
            [
                "/src/blog/templates/blog/base.html:1 content -> body",
                "/src/blog/templates/blog/base.html:1 content -> body",
                "/src/blog/templates/blog/edit.html:1 content -> body",
                "/src/blog/templates/blog/list.html:1 content -> body",
                "/src/blog/templates/blog/list.html:1 content -> body",
                "/src/blog/templates/blog/post.html:1 content -> body",
            ]
        );
    }

    #[test]
    fn renaming_a_block_from_the_base_reaches_the_same_templates() {
        let test = blog(&[
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% block content %}a{% endblock %}",
            ),
            (
                "blog/templates/blog/grandchild.html",
                "{% extends 'blog/post.html' %}{% block content %}b{% endblock %}",
            ),
            (
                "blog/templates/blog/unrelated.html",
                "{% block content %}{% endblock %}",
            ),
            (
                "blog/templates/blog/seed.html",
                "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}{% endblock %}",
            ),
        ]);

        let renamed = test.rename("body");
        assert!(
            renamed.contains(
                &"/src/blog/templates/blog/grandchild.html:1 content -> body".to_string()
            ),
            "got {renamed:?}"
        );
        assert!(
            !renamed.iter().any(|edit| edit.contains("unrelated.html")),
            "a template joined to no chain of this one's is not in the family: got {renamed:?}"
        );
    }

    #[test]
    fn a_named_closing_tag_is_renamed_with_the_block_it_closes() {
        let test = blog(&[(
            "blog/templates/blog/post.html",
            "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}a{% endblock content %}",
        )]);

        assert_eq!(
            test.rename("body"),
            [
                "/src/blog/templates/blog/base.html:1 content -> body",
                "/src/blog/templates/blog/base.html:1 content -> body",
                "/src/blog/templates/blog/post.html:1 content -> body",
                "/src/blog/templates/blog/post.html:1 content -> body",
            ]
        );
    }

    #[test]
    fn a_block_rename_is_refused_when_a_template_picks_its_parent_at_render_time() {
        let test = blog(&[
            (
                "blog/templates/blog/dynamic.html",
                "{% extends parent %}{% block content %}a{% endblock %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}b{% endblock %}",
            ),
        ]);

        let refusal = test.prepare_rename();
        assert!(
            refusal.starts_with("refused:") && refusal.contains("dynamic.html"),
            "got {refusal}"
        );
        assert_eq!(test.rename("body"), [refusal]);
    }

    #[test]
    fn a_block_rename_is_refused_when_the_settings_cannot_be_read() {
        // no settings module means no `INSTALLED_APPS`, and so no way to know
        // that an installed app holds no template overriding this block
        let test = TemplateTest::new(&[
            (
                "blog/templates/blog/base.html",
                "{% block content %}{% endblock %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}{% endblock %}",
            ),
        ]);

        assert!(
            test.prepare_rename().contains("settings could not be read"),
            "got {}",
            test.prepare_rename()
        );
    }

    #[test]
    fn a_block_rename_is_refused_when_the_new_name_is_already_declared() {
        let test = blog(&[(
            "blog/templates/blog/post.html",
            "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}{% endblock %}\
             {% block footer %}{% endblock %}",
        )]);

        assert_eq!(
            test.rename("footer"),
            [
                "refused: `/src/blog/templates/blog/post.html` already declares a block named \
                 `footer`, and django renders no template that declares one twice"
            ]
        );
    }

    /// the same project with a `widgets` app installed beside it, holding
    /// `installed`
    ///
    /// an installed app's templates are django's to load and nobody's to
    /// rewrite, which is what makes them the interesting half of every scan.
    fn with_installed_app(sources: &[(&str, &str)], installed: &[(&str, &str)]) -> TemplateTest {
        let mut all: Vec<(&str, &str)> = vec![
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
                INSTALLED_APPS = ['blog', 'widgets']

                TEMPLATES = [{'DIRS': [], 'APP_DIRS': True, 'OPTIONS': {}}]

                ROOT_URLCONF = 'project.urls'
                ",
            ),
            (
                "project/urls.py",
                "
                from django.urls import include, path

                urlpatterns = [path('blog/', include('blog.urls'))]
                ",
            ),
            ("blog/__init__.py", ""),
            (
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', detail, name='detail')]
                ",
            ),
        ];
        all.extend_from_slice(sources);

        let mut packaged: Vec<(&str, &str)> = vec![("widgets/__init__.py", "")];
        packaged.extend_from_slice(installed);

        TemplateTest::with_site_packages(&all, &packaged)
    }

    #[test]
    fn a_block_rename_is_refused_when_an_installed_app_overrides_it() {
        let test = with_installed_app(
            &[
                (
                    "blog/templates/blog/base.html",
                    "{% block content %}{% endblock %}",
                ),
                (
                    "blog/templates/blog/post.html",
                    "{% extends 'blog/base.html' %}{% block cont<CURSOR>ent %}{% endblock %}",
                ),
            ],
            &[(
                "widgets/templates/widgets/panel.html",
                "{% extends 'blog/base.html' %}{% block content %}x{% endblock %}",
            )],
        );

        let refusal = test.prepare_rename();
        assert!(
            refusal.contains("panel.html") && refusal.contains("not the project's to rewrite"),
            "got {refusal}"
        );
    }

    #[test]
    fn an_installed_apps_own_url_variable_does_not_refuse_a_route_rename() {
        // a reusable app reverses its *own* routes at render time, and it is code
        // this could not have rewritten whatever it said
        let test = with_installed_app(
            &[(
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            )],
            &[(
                "widgets/templates/widgets/panel.html",
                "{% url chosen %}{% url 'widgets:index' %}",
            )],
        );

        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:6 detail -> entry",
            ]
        );
    }

    #[test]
    fn an_installed_apps_template_spelling_the_route_does_refuse_it() {
        // this one is no speculation: the name is written out, and it is written
        // somewhere a rename cannot reach
        let test = with_installed_app(
            &[(
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            )],
            &[(
                "widgets/templates/widgets/panel.html",
                "{% url 'blog:detail' %}",
            )],
        );

        let refusal = test.prepare_rename();
        assert!(
            refusal.contains("panel.html") && refusal.contains("not the project's to rewrite"),
            "got {refusal}"
        );
    }

    #[test]
    fn a_tag_name_is_not_a_block_name() {
        let test = blog(&[(
            "blog/templates/blog/post.html",
            "{% bl<CURSOR>ock content %}{% endblock %}",
        )]);

        assert_eq!(test.prepare_rename(), "no rename");
    }

    // -----------------------------------------------------------------------
    // a route name
    // -----------------------------------------------------------------------

    #[test]
    fn renaming_a_route_from_a_template_reaches_the_declaration_and_the_python() {
        let test = blog(&[
            (
                "blog/views.py",
                "
                from django.urls import reverse, reverse_lazy


                def go(request):
                    reverse('blog:detail')
                    reverse_lazy('blog:detail')
                    return redirect('blog:detail')
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' pk=1 %}{% url 'blog:index' %}",
            ),
        ]);

        assert_eq!(test.prepare_rename(), "rename `detail`, replacing `detail`");
        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:8 detail -> entry",
                "/src/blog/views.py:6 detail -> entry",
                "/src/blog/views.py:7 detail -> entry",
                "/src/blog/views.py:8 detail -> entry",
            ]
        );
    }

    #[test]
    fn renaming_a_route_from_its_declaration_keeps_the_namespace_on_every_use() {
        let test = blog(&[
            (
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', detail, name='de<CURSOR>tail')]
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:detail' pk=1 %}",
            ),
        ]);

        assert_eq!(test.prepare_rename(), "rename `detail`, replacing `detail`");
        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:6 detail -> entry",
            ]
        );
    }

    #[test]
    fn a_route_can_be_renamed_from_a_reverse_in_python() {
        let test = blog(&[
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:detail' pk=1 %}",
            ),
            (
                "blog/views.py",
                "
                from django.urls import reverse


                def go(request):
                    return reverse('blog:de<CURSOR>tail')
                ",
            ),
        ]);

        assert_eq!(test.prepare_rename(), "rename `detail`, replacing `detail`");
        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:8 detail -> entry",
                "/src/blog/views.py:6 detail -> entry",
            ]
        );
    }

    #[test]
    fn one_bare_name_under_two_namespaces_is_two_routes() {
        let test = project(&[
            (
                "project/urls.py",
                "
                from django.urls import include, path

                urlpatterns = [
                    path('blog/', include('blog.urls')),
                    path('shop/', include('shop.urls')),
                ]
                ",
            ),
            ("shop/__init__.py", ""),
            (
                "shop/urls.py",
                "
                from django.urls import path

                app_name = 'shop'

                urlpatterns = [path('<int:pk>/', detail, name='detail')]
                ",
            ),
            (
                "blog/urls.py",
                "
                from django.urls import path

                app_name = 'blog'

                urlpatterns = [path('<int:pk>/', detail, name='detail')]
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}{% url 'shop:detail' %}",
            ),
        ]);

        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:6 detail -> entry",
            ]
        );
    }

    #[test]
    fn a_route_rename_is_refused_for_a_name_a_router_generates() {
        let test = project(&[
            (
                "project/urls.py",
                "
                from django.urls import include, path
                from rest_framework.routers import DefaultRouter

                from blog.views import BookViewSet

                router = DefaultRouter()
                router.register('books', BookViewSet, basename='book')

                urlpatterns = [path('api/', include(router.urls))]
                ",
            ),
            (
                "blog/views.py",
                "
                class BookViewSet:
                    pass
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'book-<CURSOR>list' %}",
            ),
        ]);

        assert!(
            test.prepare_rename().contains("basename"),
            "got {}",
            test.prepare_rename()
        );
    }

    #[test]
    fn a_route_rename_is_refused_when_a_reverse_names_the_route_at_run_time() {
        let test = blog(&[
            (
                "blog/views.py",
                "
                from django.urls import reverse


                def go(request, wanted):
                    return reverse(wanted)
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        let refusal = test.prepare_rename();
        assert!(
            refusal.contains("worked out at run time") && refusal.contains("views.py:6"),
            "got {refusal}"
        );
    }

    #[test]
    fn a_route_rename_reaches_a_rest_framework_hyperlinked_field() {
        let test = blog(&[
            (
                "blog/serializers.py",
                "
                class BookSerializer:
                    url = HyperlinkedIdentityField(view_name='blog:detail')
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/serializers.py:3 detail -> entry",
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:8 detail -> entry",
            ]
        );
    }

    #[test]
    fn a_literal_spelling_the_route_somewhere_unrelated_blocks_nothing() {
        // `detail` is among the commonest strings in any codebase. a dict lookup
        // in code that has nothing to do with django is not a use of the route,
        // and refusing over one would make the commonest route names unrenameable
        let test = blog(&[
            (
                "blog/report.py",
                "
                def summarise(item):
                    seen = ['detail', 'index']
                    return item.get('detail'), {'detail': 1}, summarise('detail')
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:8 detail -> entry",
            ]
        );
    }

    #[test]
    fn a_constant_holding_the_route_name_refuses_the_rename() {
        // a constant carries the name into a `reverse()` this cannot see, so the
        // rename stops at the binding and says which one
        let test = blog(&[
            (
                "blog/links.py",
                "
                BOOK_ROUTE = 'blog:detail'


                def link():
                    return reverse(BOOK_ROUTE)
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert_eq!(
            test.rename("entry"),
            [
                "refused: `/src/blog/links.py:2` binds this name to `BOOK_ROUTE`, and a rename \
                 cannot follow where a constant carries it"
            ]
        );
    }

    #[test]
    fn a_constant_in_a_class_body_refuses_it_too() {
        let test = blog(&[
            (
                "blog/links.py",
                "
                class Links:
                    BOOK_ROUTE = 'blog:detail'
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert!(
            test.prepare_rename()
                .contains("binds this name to `BOOK_ROUTE`"),
            "got {}",
            test.prepare_rename()
        );
    }

    #[test]
    fn a_name_bound_inside_a_function_blocks_nothing_on_its_own() {
        // it is visible only in that body, so the only way out is a call this
        // already reads — and reads as an argument it cannot follow, which
        // refuses on its own
        let test = blog(&[
            (
                "blog/report.py",
                "
                def summarise():
                    wanted = 'blog:detail'
                    return wanted
                ",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        assert_eq!(
            test.rename("entry"),
            [
                "/src/blog/templates/blog/post.html:1 detail -> entry",
                "/src/blog/urls.py:8 detail -> entry",
            ]
        );
    }

    #[test]
    fn a_route_rename_is_refused_when_a_template_reverses_a_variable() {
        let test = blog(&[
            ("blog/templates/blog/nav.html", "{% url target %}"),
            (
                "blog/templates/blog/post.html",
                "{% url 'blog:de<CURSOR>tail' %}",
            ),
        ]);

        let refusal = test.prepare_rename();
        assert!(refusal.contains("nav.html:1"), "got {refusal}");
    }

    #[test]
    fn a_route_cannot_be_renamed_to_a_qualified_name() {
        let test = blog(&[(
            "blog/templates/blog/post.html",
            "{% url 'blog:de<CURSOR>tail' %}",
        )]);

        assert_eq!(
            test.rename("shop:detail"),
            [
                "refused: a route's own name holds no `:` — the namespaces in front of it come \
                 from where its url configuration is included"
            ]
        );
    }

    #[test]
    fn a_route_rename_is_refused_when_the_url_tree_cannot_be_read() {
        // no settings module means no `ROOT_URLCONF`, and so no way to know which
        // namespace a name is reversed under
        let test = TemplateTest::new(&[
            (
                "blog/urls.py",
                "urlpatterns = [path('', detail, name='detail')]",
            ),
            (
                "blog/templates/blog/post.html",
                "{% url 'de<CURSOR>tail' %}",
            ),
        ]);

        assert!(
            test.prepare_rename().contains("url configuration"),
            "got {}",
            test.prepare_rename()
        );
    }

    // -----------------------------------------------------------------------
    // a template name
    // -----------------------------------------------------------------------

    #[test]
    fn renaming_a_template_rewrites_every_reference_and_moves_the_file() {
        let test = blog(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render


                def post(request):
                    return render(request, 'blog/post.html', {})


                class PostView:
                    template_name = 'blog/post.html'
                ",
            ),
            (
                "blog/templates/blog/list.html",
                "{% include 'blog/post.html' %}{% include 'blog/post.html#card' %}",
            ),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% extends 'blog/base.html' %}{% partialdef card %}x{% endpartialdef %}",
            ),
        ]);

        assert_eq!(
            test.prepare_rename(),
            "rename `blog/post.html`, replacing `blog/post.html`"
        );
        assert_eq!(
            test.rename("blog/entry.html"),
            [
                "/src/blog/templates/blog/child.html:1 blog/post.html -> blog/entry.html",
                "/src/blog/templates/blog/list.html:1 blog/post.html -> blog/entry.html",
                "/src/blog/templates/blog/list.html:1 blog/post.html -> blog/entry.html",
                "/src/blog/views.py:6 blog/post.html -> blog/entry.html",
                "/src/blog/views.py:10 blog/post.html -> blog/entry.html",
                "move /src/blog/templates/blog/post.html -> /src/blog/templates/blog/entry.html",
            ]
        );
    }

    #[test]
    fn a_template_can_be_renamed_from_a_render_in_python() {
        let test = blog(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render


                def post(request):
                    return render(request, 'blog/p<CURSOR>ost.html', {})
                ",
            ),
            (
                "blog/templates/blog/list.html",
                "{% include 'blog/post.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        assert_eq!(
            test.rename("blog/entry.html"),
            [
                "/src/blog/templates/blog/list.html:1 blog/post.html -> blog/entry.html",
                "/src/blog/views.py:6 blog/post.html -> blog/entry.html",
                "move /src/blog/templates/blog/post.html -> /src/blog/templates/blog/entry.html",
            ]
        );
    }

    #[test]
    fn a_template_rename_leaves_the_partial_a_reference_addresses_alone() {
        let test = blog(&[
            (
                "blog/templates/blog/list.html",
                "{% include 'blog/po<CURSOR>st.html#card' %}",
            ),
            (
                "blog/templates/blog/post.html",
                "{% partialdef card %}x{% endpartialdef %}",
            ),
        ]);

        assert_eq!(
            test.prepare_rename(),
            "rename `blog/post.html`, replacing `blog/post.html`"
        );
    }

    #[test]
    fn a_template_rename_is_refused_when_a_render_names_its_template_at_run_time() {
        let test = blog(&[
            (
                "blog/views.py",
                "
                from django.shortcuts import render


                def post(request, chosen):
                    return render(request, chosen, {})
                ",
            ),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        let refusal = test.prepare_rename();
        assert!(
            refusal.contains("worked out at run time") && refusal.contains("views.py:6"),
            "got {refusal}"
        );
    }

    #[test]
    fn a_constant_holding_the_template_name_refuses_the_rename() {
        let test = blog(&[
            (
                "blog/tasks.py",
                "
                WELCOME = 'blog/post.html'


                def send(request):
                    return render(request, WELCOME, {})
                ",
            ),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        assert_eq!(
            test.rename("blog/entry.html"),
            [
                "refused: `/src/blog/tasks.py:2` binds this name to `WELCOME`, and a rename \
                 cannot follow where a constant carries it"
            ]
        );
    }

    #[test]
    fn the_template_name_of_a_view_class_is_rewritten_rather_than_refused() {
        // it is a class-level binding *and* a position the rename already
        // rewrites, so it is no reason to refuse
        let test = blog(&[
            (
                "blog/views.py",
                "
                class PostView:
                    template_name = 'blog/post.html'
                ",
            ),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
            ("blog/templates/blog/post.html", "x"),
        ]);

        assert_eq!(
            test.rename("blog/entry.html"),
            [
                "/src/blog/templates/blog/child.html:1 blog/post.html -> blog/entry.html",
                "/src/blog/views.py:3 blog/post.html -> blog/entry.html",
                "move /src/blog/templates/blog/post.html -> /src/blog/templates/blog/entry.html",
            ]
        );
    }

    #[test]
    fn a_template_rename_is_refused_when_two_directories_hold_the_name() {
        let test = blog(&[
            ("blog/templates/blog/post.html", "x"),
            ("shop/templates/blog/post.html", "y"),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
        ]);

        assert!(
            test.prepare_rename()
                .contains("two of the project's template directories"),
            "got {}",
            test.prepare_rename()
        );
    }

    #[test]
    fn a_template_cannot_be_renamed_over_one_that_is_already_there() {
        let test = blog(&[
            ("blog/templates/blog/post.html", "x"),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
        ]);

        assert_eq!(
            test.rename("blog/base.html"),
            ["refused: a template is already loadable as `blog/base.html`"]
        );
    }

    #[test]
    fn a_template_name_is_a_path_below_its_template_directory() {
        let test = blog(&[
            ("blog/templates/blog/post.html", "x"),
            (
                "blog/templates/blog/child.html",
                "{% extends 'blog/p<CURSOR>ost.html' %}",
            ),
        ]);

        assert_eq!(
            test.rename("../escape.html"),
            ["refused: a template's name is a path relative to its template directory"]
        );
    }
}
