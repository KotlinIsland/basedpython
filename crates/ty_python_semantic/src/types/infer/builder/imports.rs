use ruff_python_ast as ast;
use ruff_text_size::{Ranged, TextRange};
use ty_module_resolver::{
    ImportingFile, KnownModule, Module, ModuleName, ModuleNameResolutionError, ModuleResolveMode,
    resolve_module, search_paths,
};

use crate::{
    TypeQualifiers, add_inferred_python_version_hint_to_diagnostic,
    dependencies::{self, GroupName, ImportStanding},
    place::{
        DefinedPlace, Definedness, Place, PlaceAndQualifiers, TypeOrigin,
        basedpython_typing_added_in, basedpython_warnings_added_in, typing_extensions_symbol,
    },
    types::{
        ModuleLiteralType, Type, TypeAndQualifiers,
        dedicated::EXTERNALLY_STUBBED_FRAMEWORKS,
        diagnostic::{
            MISPLACED_DEPENDENCY, MISSING_FRAMEWORK_STUBS, POSSIBLY_MISSING_IMPORT, PRIVATE_IMPORT,
            UNDECLARED_DEPENDENCY, UNRESOLVED_IMPORT,
            hint_if_stdlib_attribute_exists_on_other_versions,
            hint_if_stdlib_submodule_exists_on_other_versions,
        },
        infer::{TypeInferenceBuilder, builder::DeclaredAndInferredType},
        infer_definition_types,
        visibility::private_symbols,
    },
};
use ruff_python_ast::name::Name;
use ty_python_core::definition::Definition;

impl<'db, 'ast> TypeInferenceBuilder<'db, 'ast> {
    pub(super) fn infer_import_statement(&mut self, import: &ast::StmtImport) {
        let ast::StmtImport {
            names,
            is_lazy: _,
            range: _,
            node_index: _,
        } = import;

        for alias in names {
            self.infer_definition(alias);
        }
    }

    fn report_unresolved_import(
        &self,
        range: TextRange,
        level: u32,
        module: Option<&str>,
        module_name: Option<&ModuleName>,
    ) {
        let db = self.db();

        if let Some(module_name) = &module_name
            && (self
                .settings()
                .allowed_unresolved_imports
                .matches(module_name)
                .is_include()
                || self
                    .settings()
                    .replace_imports_with_any
                    .matches(module_name)
                    .is_include())
        {
            return;
        }

        let Some(builder) = self.context.report_lint(&UNRESOLVED_IMPORT, range) else {
            return;
        };

        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Cannot resolve imported module `{}`",
            format_import_from_module(level, module)
        ));

        if level == 0 {
            if let Some(module_name) = module_name {
                let resolver_environment = self.program_environment().program(db);
                let typeshed_versions = resolver_environment.search_paths(db).typeshed_versions();

                // Loop over ancestors in case we have info on the parent module but not submodule
                for module_name in module_name.ancestors() {
                    if let Some(version_range) = typeshed_versions.exact(&module_name) {
                        // We know it is a stdlib module on *some* Python versions...
                        let python_version = self.program_environment().python_version(db);
                        if !version_range.contains(python_version) {
                            // ...But not on *this* Python version.
                            diagnostic.info(format_args!(
                                "The stdlib module `{module_name}` is only available on Python {version_range}",
                                version_range = version_range.diagnostic_display(),
                            ));
                            add_inferred_python_version_hint_to_diagnostic(
                                db,
                                self.file(),
                                &mut diagnostic,
                                "resolving modules",
                            );
                            return;
                        }
                        // We found the most precise answer we could, stop searching
                        break;
                    }
                }
            }
        } else {
            let importing_file = ImportingFile::File(
                self.file(),
                self.program_environment().resolver_environment(db),
            );
            if let Some(better_level) = (0..level).rev().find(|reduced_level| {
                let Ok(module_name) =
                    ModuleName::from_identifier_parts(db, importing_file, module, *reduced_level)
                else {
                    return false;
                };
                resolve_module(db, importing_file, &module_name).is_some()
            }) {
                diagnostic
                    .help("The module can be resolved if the number of leading dots is reduced");
                diagnostic.help(format_args!(
                    "Did you mean `{}`?",
                    format_import_from_module(better_level, module)
                ));
                diagnostic.set_concise_message(format_args!(
                    "Cannot resolve imported module `{}` - did you mean `{}`?",
                    format_import_from_module(level, module),
                    format_import_from_module(better_level, module)
                ));
            }
        }

        // Add search paths information to the diagnostic
        // Use the same search paths function that is used in actual module resolution
        let verbose = db.verbose();
        let search_paths = search_paths(
            db,
            self.program_environment().resolver_environment(db),
            ModuleResolveMode::Typing,
        );

        diagnostic.info(format_args!(
            "Searched in the following paths during module resolution:"
        ));

        let mut search_paths = search_paths.enumerate().peekable();

        while let Some((index, path)) = search_paths.next() {
            if index > 4 && !verbose && search_paths.peek().is_some() {
                let more = search_paths.count() + 1;
                diagnostic.info(format_args!(
                    "  ... and {more} more paths. Run with `-v` to see all paths."
                ));
                break;
            }
            diagnostic.info(format_args!(
                "  {}. {} ({})",
                index + 1,
                path,
                path.describe_kind()
            ));
        }

        diagnostic.info(
            "make sure your Python environment is properly configured: \
                https://docs.astral.sh/ty/modules/#python-environment",
        );
    }

    pub(super) fn infer_import_definition(
        &mut self,
        alias: &ast::Alias,
        definition: Definition<'db>,
    ) {
        let ast::Alias {
            range: _,
            node_index: _,
            name,
            asname,
        } = alias;

        // The name of the module being imported
        let Some(full_module_name) = ModuleName::new(name) else {
            tracing::debug!("Failed to resolve import due to invalid syntax");
            self.add_unknown_declaration_with_binding(alias.into(), definition);
            return;
        };

        if self
            .settings()
            .replace_imports_with_any
            .matches(&full_module_name)
            .is_include()
        {
            self.add_declaration_with_binding(
                alias.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(Type::any()),
            );
            return;
        }

        // Resolve the module being imported.
        let Some(full_module_ty) = self.module_type_from_name(&full_module_name) else {
            self.report_unresolved_import(alias.range(), 0, Some(name), Some(&full_module_name));
            self.add_unknown_declaration_with_binding(alias.into(), definition);
            return;
        };

        self.check_framework_stubs(alias.range(), &full_module_name);
        self.check_dependency_declaration(alias.range(), &full_module_name);

        let binding_ty = if asname.is_some() {
            // If we are renaming the imported module via an `as` clause, then we bind the resolved
            // module's type to that name, even if that module is nested.
            full_module_ty
        } else if full_module_name.contains('.') {
            // If there's no `as` clause and the imported module is nested, we're not going to bind
            // the resolved module itself into the current scope; we're going to bind the top-most
            // parent package of that module.
            let topmost_parent_name =
                ModuleName::new(full_module_name.components().next().unwrap()).unwrap();
            let Some(topmost_parent_ty) = self.module_type_from_name(&topmost_parent_name) else {
                self.add_unknown_declaration_with_binding(alias.into(), definition);
                return;
            };
            topmost_parent_ty
        } else {
            // If there's no `as` clause and the imported module isn't nested, then the imported
            // module _is_ what we bind into the current scope.
            full_module_ty
        };

        self.add_declaration_with_binding(
            alias.into(),
            definition,
            &DeclaredAndInferredType::are_the_same_type(binding_ty),
        );
    }

    pub(super) fn infer_import_from_statement(&mut self, import: &ast::StmtImportFrom) {
        let ast::StmtImportFrom {
            module: _,
            names,
            level: _,
            is_lazy: _,
            is_export: _,
            range: _,
            node_index: _,
        } = import;

        let db = self.db();

        self.check_import_from_module_is_resolvable(import);

        for alias in names {
            for definition in self.index.definitions(alias) {
                let inferred = infer_definition_types(self.db(), *definition);
                // Check non-star imports for deprecations
                if definition.kind(db).as_star_import().is_none() {
                    // In the initial cycle, `declaration_types()` is empty, so no deprecation check is performed.
                    for ty in inferred.declaration_types() {
                        self.check_deprecated(alias, ty.inner);
                    }
                }
                self.extend_definition(*definition, inferred);
            }
        }
    }

    /// Resolve the [`ModuleName`], and the type of the module, being referred to by an
    /// [`ast::StmtImportFrom`] node. Emit a diagnostic if the module cannot be resolved.
    fn check_import_from_module_is_resolvable(&mut self, import_from: &ast::StmtImportFrom) {
        let ast::StmtImportFrom { module, level, .. } = import_from;

        let db = self.db();

        // For diagnostics, we want to highlight the unresolvable
        // module and not the entire `from ... import ...` statement.
        let module_ref = module
            .as_ref()
            .map(ast::AnyNodeRef::from)
            .unwrap_or_else(|| ast::AnyNodeRef::from(import_from));
        let module = module.as_deref();

        tracing::trace!(
            "Resolving import statement from module `{}` into file `{}`",
            format_import_from_module(*level, module),
            self.file().path(db),
        );
        let importing_file = ImportingFile::File(
            self.file(),
            self.program_environment().resolver_environment(db),
        );
        let module_name = ModuleName::from_import_statement(db, importing_file, import_from);

        let module_name = match module_name {
            Ok(module_name) => module_name,
            Err(ModuleNameResolutionError::InvalidSyntax) => {
                tracing::debug!("Failed to resolve import due to invalid syntax");
                // Invalid syntax diagnostics are emitted elsewhere.
                return;
            }
            Err(ModuleNameResolutionError::TooManyDots) => {
                tracing::debug!(
                    "Relative module resolution `{}` failed: too many leading dots",
                    format_import_from_module(*level, module),
                );
                self.report_unresolved_import(module_ref.range(), *level, module, None);
                return;
            }
            Err(ModuleNameResolutionError::UnknownCurrentModule) => {
                tracing::debug!(
                    "Relative module resolution `{}` failed: could not resolve file `{}` to a module \
                    (try adjusting configured search paths?)",
                    format_import_from_module(*level, module),
                    self.file().path(db)
                );
                self.report_unresolved_import(module_ref.range(), *level, module, None);
                return;
            }
        };

        if resolve_module(db, importing_file, &module_name).is_none() {
            self.report_unresolved_import(module_ref.range(), *level, module, Some(&module_name));
        } else {
            self.check_framework_stubs(module_ref.range(), &module_name);
            self.check_dependency_declaration(module_ref.range(), &module_name);
        }
    }

    /// basedpython: resolve a version-gated `typing`/`warnings` member from
    /// `typing_extensions` when it is absent from the module at the target
    /// Python version. Returns `None` for any other module or name, so ordinary
    /// unresolved imports still report as usual.
    fn basedpython_typing_import_redirect(
        &self,
        module: Module<'db>,
        name: &str,
    ) -> Option<PlaceAndQualifiers<'db>> {
        let env = self.program_environment();
        let db = self.db();
        let redirected = if module.is_known(db, KnownModule::Typing) {
            basedpython_typing_added_in(name).is_some()
        } else if module.is_known(db, KnownModule::Warnings) {
            basedpython_warnings_added_in(name).is_some()
        } else {
            false
        };
        if !redirected {
            return None;
        }
        let place = typing_extensions_symbol(db, env, name);
        (!place.place.is_undefined()).then_some(place)
    }

    /// Warn when a framework that needs an external PEP 561 stubs package
    /// resolves from its untyped runtime package instead of the stubs.
    /// Report an import of a distribution this file is not entitled to import.
    ///
    /// Only ever reports about a resolved import of installed third-party code
    /// whose distribution the project's manifest has something to say about.
    /// Everything else — first-party code, the standard library, an editable
    /// install, a project that declares nothing — is silence.
    fn check_dependency_declaration(&self, range: TextRange, module_name: &ModuleName) {
        let db = self.db();

        let Some(module) = resolve_module(
            db,
            ImportingFile::File(
                self.file(),
                db.program_file(self.file()).resolver_environment(db),
            ),
            module_name,
        ) else {
            return;
        };

        match dependencies::import_standing(db, self.file(), module) {
            ImportStanding::Unknown | ImportStanding::Available => {}

            ImportStanding::Undeclared { distribution } => {
                let Some(builder) = self.context.report_lint(&UNDECLARED_DEPENDENCY, range) else {
                    return;
                };
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{root}` comes from `{distribution}`, which this project does not depend on",
                    root = module_name.first_component(),
                ));
                diagnostic.info(format_args!(
                    "It is only installed because something else depends on it; \
                     add `{distribution}` to the project's dependencies"
                ));
            }

            ImportStanding::Misplaced {
                distribution,
                declared_in,
            } => {
                let Some(builder) = self.context.report_lint(&MISPLACED_DEPENDENCY, range) else {
                    return;
                };
                let mut diagnostic = builder.into_diagnostic(format_args!(
                    "`{root}` comes from `{distribution}`, which is declared in {groups}",
                    root = module_name.first_component(),
                    groups = format_groups(&declared_in),
                ));
                diagnostic.info(
                    "Nothing installs a dependency group alongside the project, \
                     so this import fails for everyone who installs it",
                );
            }
        }
    }

    fn check_framework_stubs(&self, range: TextRange, module_name: &ModuleName) {
        let db = self.db();

        let Some(framework) = EXTERNALLY_STUBBED_FRAMEWORKS
            .iter()
            .find(|framework| module_name.components().next() == Some(framework.package))
        else {
            return;
        };

        let Some(package_name) = ModuleName::new(framework.package) else {
            return;
        };
        let Some(module) = resolve_module(
            db,
            ImportingFile::File(
                self.file(),
                db.program_file(self.file()).resolver_environment(db),
            ),
            &package_name,
        ) else {
            return;
        };
        // A module that happens to share the framework's name is not the framework unless it
        // was installed as one — this diagnostic asks the user to install a stubs distribution,
        // which only makes sense for a package they installed in the first place.
        if !module
            .search_path(db)
            .is_some_and(ty_module_resolver::SearchPath::is_installed_distribution)
        {
            return;
        }
        let Some(file) = module.file(db) else {
            return;
        };
        // The stubs, when installed, take resolution priority; resolving to a
        // file outside the `<package>-stubs` directory means they are absent.
        let stubs_resolved = file.path(db).as_system_path().is_some_and(|path| {
            path.components()
                .any(|component| component.as_str() == framework.stubs_directory)
        });
        if stubs_resolved {
            return;
        }

        let Some(builder) = self.context.report_lint(&MISSING_FRAMEWORK_STUBS, range) else {
            return;
        };
        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Types for `{}` are incomplete without the `{}` package",
            framework.package, framework.stubs_distribution,
        ));
        diagnostic.info(format_args!(
            "Install it with `uv add {} --dev`",
            framework.stubs_distribution,
        ));
    }

    /// basedpython: report `from other import X` where `other` declared `X`
    /// `private`.
    ///
    /// A star import is not reported: it names no symbol, so the diagnostic
    /// would have nothing actionable to point at.
    fn check_private_import(
        &self,
        module: Module<'db>,
        module_name: &ModuleName,
        alias: &ast::Alias,
        name: &Name,
    ) {
        let db = self.db();
        let Some(defining_file) = module.file(db) else {
            return;
        };
        if defining_file == self.file() || &alias.name == "*" {
            return;
        }
        if !private_symbols(db, defining_file).contains(name) {
            return;
        }
        if let Some(builder) = self
            .context
            .report_lint(&PRIVATE_IMPORT, ast::AnyNodeRef::Alias(alias))
        {
            builder.into_diagnostic(format_args!("`{name}` is private to `{module_name}`"));
        }
    }

    pub(super) fn infer_import_from_definition(
        &mut self,
        import_from: &ast::StmtImportFrom,
        alias: &ast::Alias,
        definition: Definition<'db>,
    ) {
        let env = self.program_environment();
        let db = self.db();

        let importing_file = ImportingFile::File(
            self.file(),
            self.program_environment().resolver_environment(db),
        );
        let Ok(module_name) = ModuleName::from_import_statement(db, importing_file, import_from)
        else {
            self.add_unknown_declaration_with_binding(alias.into(), definition);
            return;
        };

        if self
            .settings()
            .replace_imports_with_any
            .matches(&module_name)
            .is_include()
        {
            self.add_declaration_with_binding(
                alias.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(Type::any()),
            );
            return;
        }

        let Some(module) = resolve_module(db, importing_file, &module_name) else {
            self.add_unknown_declaration_with_binding(alias.into(), definition);
            return;
        };

        let module_literal = ModuleLiteralType::new(
            db,
            module,
            module.kind(db).is_package().then_some(self.program_file()),
        );
        let module_ty = Type::ModuleLiteral(module_literal);

        let name = if let Some(star_import) = definition.kind(db).as_star_import() {
            self.index
                .place_table(self.scope().file_scope_id(db))
                .symbol(star_import.symbol_id())
                .name()
        } else {
            &alias.name.id
        };

        self.check_private_import(module, &module_name, alias, name);

        // Avoid looking up attributes on a module if a module imports from itself
        // at the module-global scope, where the import definition itself is one of the
        // bindings for the symbol being looked up, which would cause a query cycle.
        //
        // In nested scopes (e.g. function bodies), the module's global-scope definitions
        // are resolved independently, so there is no cycle risk and the lookup is safe.
        let skip_self_referential_member_lookup = Some(self.file())
            == module_literal.module(db).file(db)
            && self.scope().file_scope_id(db).is_global();

        // Although it isn't the runtime semantics, we go to some trouble to prioritize a submodule
        // over module `__getattr__`, because that's what other type checkers do.
        let mut from_module_getattr = None;

        // First try loading the requested attribute from the module.
        if !skip_self_referential_member_lookup {
            let mut member = module_literal.static_member(db, env, name);
            // basedpython: version-gated `typing`/`warnings` members are always
            // available. When the member is missing at the target Python version,
            // fall back to `typing_extensions`, mirroring the transpiler's import
            // redirect so the checker resolves what the emitted code will import.
            if member.place.is_undefined()
                && self.is_basedpython_file()
                && let Some(redirected) = self.basedpython_typing_import_redirect(module, name)
            {
                member = redirected;
            }
            if let PlaceAndQualifiers {
                place:
                    Place::Defined(DefinedPlace {
                        ty,
                        definedness: boundness,
                        provenance: source_provenance,
                        ..
                    }),
                qualifiers,
            } = member
            {
                if &alias.name != "*" && boundness == Definedness::PossiblyUndefined {
                    // TODO: Consider loading _both_ the attribute and any submodule and unioning them
                    // together if the attribute exists but is possibly-unbound.
                    if let Some(builder) = self
                        .context
                        .report_lint(&POSSIBLY_MISSING_IMPORT, ast::AnyNodeRef::Alias(alias))
                    {
                        builder.into_diagnostic(format_args!(
                            "Member `{name}` of module `{module_name}` may be missing",
                        ));
                    }
                }
                if qualifiers.contains(TypeQualifiers::FROM_MODULE_GETATTR) {
                    from_module_getattr = Some((ty, qualifiers, source_provenance));
                } else {
                    self.add_declaration_with_binding(
                        alias.into(),
                        definition,
                        &DeclaredAndInferredType::MightBeDifferent {
                            declared_ty: TypeAndQualifiers {
                                inner: ty,
                                origin: TypeOrigin::Declared,
                                qualifiers,
                                provenance: source_provenance,
                            },
                            inferred_ty: ty,
                        },
                    );
                    return;
                }
            }
        }

        // Evaluate whether `X.Y` would constitute a valid submodule name,
        // given a `from X import Y` statement. If it is valid, this will be `Some()`;
        // else, it will be `None`.
        let full_submodule_name = ModuleName::new(name).map(|final_part| {
            let mut ret = module_name.clone();
            ret.extend(&final_part);
            ret
        });

        // If the module doesn't bind the symbol, check if it's a submodule.  This won't get
        // handled by the `Type::member` call because it relies on the semantic index's
        // `imported_modules` set.  The semantic index does not include information about
        // `from...import` statements because there are two things it cannot determine while only
        // inspecting the content of the current file:
        //
        //   - whether the imported symbol is an attribute or submodule
        //   - whether the containing file is in a module or a package (needed to correctly resolve
        //     relative imports)
        //
        // The first would be solvable by making it a _potentially_ imported modules set.  The
        // second is not.
        //
        // Regardless, for now, we sidestep all of that by repeating the submodule-or-attribute
        // check here when inferring types for a `from...import` statement.
        if let Some(submodule_type) = full_submodule_name
            .as_ref()
            .and_then(|submodule_name| self.module_type_from_name(submodule_name))
        {
            self.add_declaration_with_binding(
                alias.into(),
                definition,
                &DeclaredAndInferredType::are_the_same_type(submodule_type),
            );
            return;
        }

        // We've checked for a submodule, so now we can go ahead and use a type from module
        // `__getattr__`.
        if let Some((ty, qualifiers, source_provenance)) = from_module_getattr {
            self.add_declaration_with_binding(
                alias.into(),
                definition,
                &DeclaredAndInferredType::MightBeDifferent {
                    declared_ty: TypeAndQualifiers {
                        inner: ty,
                        origin: TypeOrigin::Declared,
                        qualifiers,
                        provenance: source_provenance,
                    },
                    inferred_ty: ty,
                },
            );
            return;
        }

        self.add_unknown_declaration_with_binding(alias.into(), definition);

        if &alias.name == "*" {
            return;
        }

        if self
            .settings()
            .allowed_unresolved_imports
            .matches(full_submodule_name.as_ref().unwrap_or(&module_name))
            .is_include()
        {
            return;
        }

        let Some(builder) = self
            .context
            .report_lint(&UNRESOLVED_IMPORT, ast::AnyNodeRef::Alias(alias))
        else {
            return;
        };

        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Module `{module_name}` has no member `{name}`"
        ));

        let mut submodule_hint_added = false;

        if let Some(full_submodule_name) = full_submodule_name {
            submodule_hint_added = hint_if_stdlib_submodule_exists_on_other_versions(
                db,
                self.file(),
                self.program_environment(),
                &mut diagnostic,
                &full_submodule_name,
                module,
            );
        }

        if !submodule_hint_added {
            hint_if_stdlib_attribute_exists_on_other_versions(
                db,
                self.program_file(),
                diagnostic,
                module_ty,
                name,
                "resolving imports",
            );
        }
    }

    /// Infer the implicit local definition `x = <module 'whatever.thispackage.x'>` that
    /// `from .x.y import z` or `from whatever.thispackage.x.y` can introduce in `__init__.py(i)`.
    ///
    /// For the definition `z`, see [`TypeInferenceBuilder::infer_import_from_definition`].
    ///
    /// The runtime semantic of this kind of statement is to introduce a variable in the global
    /// scope of this module *the first time it's imported in the entire program*. This
    /// implementation just blindly introduces a local variable wherever the `from..import` is
    /// (if the imports actually resolve).
    ///
    /// That gap between the semantics and implementation are currently the responsibility of the
    /// code that actually creates these kinds of Definitions (so blindly introducing a local
    /// is all we need to be doing here).
    pub(super) fn infer_import_from_submodule_definition(
        &mut self,
        import_from: &'ast ast::StmtImportFrom,
        definition: Definition<'db>,
    ) {
        let db = self.db();
        let importing_file = ImportingFile::File(
            self.file(),
            self.program_environment().resolver_environment(db),
        );

        // Get this package's absolute module name by resolving `.`, and make sure it exists
        let Ok(thispackage_name) = ModuleName::package_for_file(db, importing_file) else {
            self.add_binding(import_from.into(), definition)
                .insert(self, Type::unknown());
            return;
        };

        let Some(module) = resolve_module(db, importing_file, &thispackage_name) else {
            self.add_binding(import_from.into(), definition)
                .insert(self, Type::unknown());
            return;
        };

        // We have `from whatever.thispackage.x.y ...` or `from .x.y ...`
        // and we want to extract `x` (to ultimately construct `whatever.thispackage.x`):

        // First we normalize to `whatever.thispackage.x.y`
        let Some(final_part) = ModuleName::from_identifier_parts(
            db,
            importing_file,
            import_from.module.as_deref(),
            import_from.level,
        )
        .ok()
        // `whatever.thispackage.x.y` => `x.y`
        .and_then(|submodule_name| submodule_name.relative_to(&thispackage_name))
        // `x.y` => `x`
        .and_then(|relative_submodule_name| {
            relative_submodule_name
                .components()
                .next()
                .and_then(ModuleName::new)
        }) else {
            self.add_binding(import_from.into(), definition)
                .insert(self, Type::unknown());
            return;
        };

        // `x` => `whatever.thispackage.x`
        let mut full_submodule_name = thispackage_name.clone();
        full_submodule_name.extend(&final_part);

        // Try to actually resolve the import `whatever.thispackage.x`
        if let Some(submodule_type) = self.module_type_from_name(&full_submodule_name) {
            // Success, introduce a binding!
            //
            // We explicitly don't introduce a *declaration* because it's actual ok
            // (and fairly common) to overwrite this import with a function or class
            // and we don't want it to be a type error to do so.
            self.add_binding(import_from.into(), definition)
                .insert(self, submodule_type);
            return;
        }

        // That didn't work, try to produce diagnostics
        self.add_binding(import_from.into(), definition)
            .insert(self, Type::unknown());

        if self
            .settings()
            .allowed_unresolved_imports
            .matches(&full_submodule_name)
            .is_include()
        {
            return;
        }

        let Some(builder) = self.context.report_lint(
            &UNRESOLVED_IMPORT,
            ast::AnyNodeRef::StmtImportFrom(import_from),
        ) else {
            return;
        };

        let mut diagnostic = builder.into_diagnostic(format_args!(
            "Module `{thispackage_name}` has no submodule `{final_part}`"
        ));

        hint_if_stdlib_submodule_exists_on_other_versions(
            self.db(),
            self.file(),
            self.program_environment(),
            &mut diagnostic,
            &full_submodule_name,
            module,
        );
    }
}

/// The groups a distribution is declared in, as a diagnostic reads them.
fn format_groups(groups: &[&GroupName]) -> String {
    let mut names = groups.iter().map(ToString::to_string);
    let mut formatted = names.next().unwrap_or_default();
    for name in names {
        formatted.push_str(" and ");
        formatted.push_str(&name);
    }
    formatted
}

fn format_import_from_module(level: u32, module: Option<&str>) -> String {
    format!(
        "{}{}",
        ".".repeat(level as usize),
        module.unwrap_or_default()
    )
}
