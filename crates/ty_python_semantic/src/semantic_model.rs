use compact_str::CompactString;
use ruff_db::PythonFile;
use ruff_db::files::{File, FilePath};
use ruff_db::parsed::{parsed_module, parsed_string_annotation};
use ruff_db::source::{line_index, source_text};
use ruff_python_ast::find_node::CoveringNode;
use ruff_python_ast::{self as ast, ExprStringLiteral, ModExpression};
use ruff_python_ast::{Expr, ExprRef, name::Name};
use ruff_python_parser::Parsed;
use ruff_source_file::LineIndex;
use ruff_text_size::{Ranged, TextRange};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_module_resolver::{
    ImportingFile, KnownModule, Module, ModuleName, list_modules, resolve_module,
};

use crate::Db;
use crate::place::implicit_globals::all_implicit_module_globals;
use crate::place::imported_symbol;
use crate::types::ide_support::{ImportAliasResolution, definition_for_name};
use crate::types::implicit_names::implicit_name;
use crate::types::list_members::{all_members, all_reachable_members};
use crate::types::{
    CycleDetector, ProgramEnvironment, SpecialFormType, Type, TypeQualifiers, binding_type,
    infer_complete_scope_types, inferred_declaration,
};
use ty_python_core::definition::{Definition, DefinitionKind};
use ty_python_core::node_key::NodeKey;
use ty_python_core::place_table;
use ty_python_core::scope::{FileScopeId, Scope};
use ty_python_core::semantic_index;
use ty_python_core::symbol::Symbol;
use ty_python_core::{Program, ProgramFile};

/// The primary interface the LSP should use for querying semantic information about a [`File`].
///
/// Although you can in principle freely construct this type given a `db` and `file`, you should
/// try to construct this at the start of your analysis and thread the same instance through
/// the full analysis.
///
/// The primary reason for this is that it manages traversing into the sub-ASTs of string
/// annotations (see [`Self::enter_string_annotation`]). When you do this you will be handling
/// AST nodes that don't belong to the file's AST (or *any* file's AST). These kinds of nodes
/// will result in panics and confusing results if handed to the wrong subsystem. `SemanticModel`
/// methods will automatically handle using the string literal's AST node when necessary.
pub struct SemanticModel<'db> {
    db: &'db dyn Db,
    file: ProgramFile<'db>,
    /// If `Some` then this `SemanticModel` is for analyzing the sub-AST of a string annotation.
    /// This expression will be used as a witness to the scope/location we're analyzing.
    in_string_annotation_expr: Option<Box<Expr>>,
}

impl<'db> SemanticModel<'db> {
    pub fn new(db: &'db dyn Db, file: ProgramFile<'db>) -> Self {
        Self {
            db,
            file,
            in_string_annotation_expr: None,
        }
    }

    pub fn db(&self) -> &'db dyn Db {
        self.db
    }

    pub fn file(&self) -> File {
        self.file.file(self.db)
    }

    pub fn python_file(&self) -> PythonFile<'db> {
        self.file.python_file(self.db)
    }

    pub fn program_file(&self) -> ProgramFile<'db> {
        self.file
    }

    pub fn program(&self) -> Program<'db> {
        self.file.program(self.db)
    }

    pub fn program_environment(&self) -> ProgramEnvironment<'db> {
        ProgramEnvironment::from_file(self.program_file())
    }

    pub fn file_path(&self) -> &FilePath {
        self.file().path(self.db)
    }

    /// basedpython: the name a `private` method is reached by in the emitted
    /// python, for an attribute access that resolves to one.
    ///
    /// The definition is lowered to `__helper`, which python name-mangles to
    /// `_A__helper` in class `A`'s body. A call site must reach the same
    /// attribute, and python's mangling is lexical — it would spell `__helper`
    /// as `_B__helper` in a subclass's body, and leave it alone outside a class
    /// altogether — so the mangled name is written out in full instead, which
    /// reads the same from everywhere.
    ///
    /// `None` when the attribute is not a private method.
    pub fn private_method_name(&self, attribute: &ast::ExprAttribute) -> Option<String> {
        crate::types::visibility::private_method_name(
            self.db,
            &self.program_environment(),
            attribute.value.inferred_type(self)?,
            attribute.inferred_type(self)?,
            attribute.attr.as_str(),
        )
    }

    /// basedpython: the source text of the specialization step the transpiler
    /// splices in after the callee of a bare reified-generic call (`f(1)` →
    /// `"[int]"`). `None` when the call is not a bare reified-generic call or
    /// no injectable spelling exists — the checker reports the latter as
    /// `unspecialized-reified-generic`
    pub fn reified_call_specialization(&self, call: &ast::ExprCall) -> Option<String> {
        let env = &self.program_environment();
        let db = self.db;
        let callee_ty = call.func.inferred_type(self)?;
        let function = match callee_ty {
            Type::FunctionLiteral(function) => function,
            Type::BoundMethod(method) => method.function(db),
            _ => return None,
        };
        if function.is_classmethod(db) || !function.is_unspecialized_reified(db) {
            return None;
        }
        let mut positional = Vec::with_capacity(call.arguments.args.len());
        for argument in &call.arguments.args {
            if argument.is_starred_expr() {
                return None;
            }
            positional.push(argument.inferred_type(self)?);
        }
        let mut keywords = Vec::with_capacity(call.arguments.keywords.len());
        for keyword in &call.arguments.keywords {
            let name = keyword.arg.as_ref()?;
            keywords.push((name.as_str(), keyword.value.inferred_type(self)?));
        }
        crate::types::reified_infer::injectable_call_specialization(
            db,
            env,
            self.file(),
            callee_ty,
            function,
            positional,
            keywords,
        )
    }

    /// basedpython: the runtime registrations a conformance extension
    /// (`extension str(A):`) emits — one per interface in its header.
    ///
    /// A registration maps each requirement an extension supplies to the backing
    /// function that answers it, so a call through an interface-typed receiver
    /// reaches the conformance's own member rather than an attribute the value
    /// does not have. A requirement the conforming type already carries is
    /// deliberately absent: the dispatcher falls back to the attribute
    pub fn conformance_registrations(
        &self,
        class_def: &ast::StmtClassDef,
    ) -> Vec<crate::types::conformance::ConformanceRegistration> {
        use crate::types::conformance;
        let env = &self.program_environment();

        let db = self.db;
        let Some(crate::types::ClassLiteral::Static(extension)) = class_def
            .inferred_type(self)
            .and_then(super::types::Type::as_class_literal)
        else {
            return Vec::new();
        };
        let mut registrations = Vec::new();
        // resolved per base, not by indexing a filtered list: a base that does
        // not resolve to a class is dropped from `declared_conformances`, which
        // would slide every later interface onto the wrong anchor
        for anchor in class_def.bases() {
            let Some(interface) = anchor
                .inferred_type(self)
                .and_then(|ty| ty.to_class_type(db))
            else {
                continue;
            };
            let Ok((spelling, import)) = crate::types::conversions::class_reference(
                db,
                env,
                self.file.file(db),
                self,
                anchor,
                interface,
            ) else {
                continue;
            };
            let table = conformance::witness_table(db, self.file.file(db), extension, interface);
            let mut imports = Vec::new();
            let mut entries = Vec::new();
            for entry in table {
                if let Some(module) = &entry.import_from {
                    imports.push(format!("from {module} import {}", entry.function));
                }
                entries.push((entry.member, entry.function));
            }
            registrations.push(conformance::ConformanceRegistration {
                interface: spelling,
                import,
                entries,
                imports,
            });
        }
        registrations
    }

    /// basedpython: the modules this file imports that must be imported
    /// *eagerly*, because they (or something they reach) declare a conformance.
    ///
    /// A conformance exists at runtime only because the declaring module's
    /// registration ran, so deferring that module's execution defers the
    /// conformance out of existence
    pub fn eagerly_imported_modules(&self) -> Vec<String> {
        let db = self.db;
        crate::types::conformance::eagerly_imported_modules(self.db, self.file.file(db))
    }

    /// basedpython: the names this file imports that must be bound to the real
    /// object rather than to a lazy proxy — the exception classes.
    ///
    /// `except` is the one place cpython refuses a stand-in: it checks that what
    /// it catches is a class inheriting `BaseException`, and never consults
    /// `__instancecheck__`. A proxy there raises `TypeError` from the handler —
    /// the line least likely to be covered by a happy-path test — so an
    /// exception class is imported eagerly and the proxy never stands where it
    /// cannot work.
    pub fn eagerly_imported_names(&self) -> Vec<String> {
        let db = self.db;
        let env = self.program_environment();
        let module = parsed_module(db, self.file.python_file(db)).load(db);
        let mut names = Vec::new();
        let mut collect = |import: &ast::StmtImportFrom| {
            for alias in &import.names {
                let Some(Type::ClassLiteral(class)) = alias.inferred_type(self) else {
                    continue;
                };
                if Type::instance(db, &env, class.default_specialization(db)).is_assignable_to(
                    db,
                    &env,
                    crate::types::KnownClass::BaseException.to_instance(db, &env),
                ) {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    names.push(bound.id.to_string());
                }
            }
        };
        for statement in module.suite() {
            if let ast::Stmt::ImportFrom(import) = statement {
                collect(import);
            }
        }
        names
    }

    /// basedpython: when an attribute access reads a *requirement* off an
    /// interface-typed receiver, how it dispatches through the witness table.
    ///
    /// Only a requirement needs this, and only while some conformance is
    /// visible: everything else on an interface-typed value is either an
    /// inherent extension member (statically resolved like any other) or an
    /// attribute the value provably carries
    pub fn witness_dispatch(
        &self,
        attribute: &ast::ExprAttribute,
    ) -> Option<crate::types::conformance::WitnessDispatch> {
        use crate::types::conformance;
        let env = self.program_environment();

        let db = self.db;
        if !self.file.file(db).source_type(db).is_basedpython() {
            return None;
        }
        let receiver_ty = attribute.value.inferred_type(self)?;
        // an optional-chain link resolves against the chain's *present* type —
        // the `None` it short-circuits with is not part of the receiver. without
        // this the whole lookup fails on a `Show?` receiver, and the transpiler's
        // "not through an optional chain" guard could never fire: it would emit a
        // plain attribute access instead of the error it promises
        let receiver_ty = if attribute.optional
            || crate::types::receivers::spine_has_optional(&attribute.value)
        {
            crate::types::receivers::strip_none(db, &env, receiver_ty)
        } else {
            receiver_ty
        };
        let interface = receiver_ty.erase_restriction(db).nominal_class(db, &env)?;
        let member = attribute.attr.as_str();
        if !conformance::requires_witness_dispatch(db, interface, member) {
            return None;
        }
        let (spelling, import) = crate::types::conversions::class_reference(
            db,
            &env,
            self.file.file(db),
            self,
            &attribute.value,
            interface,
        )
        .ok()?;
        // how the receiver reaches the witness: a `class def` takes the class,
        // a data member is read, anything else is fetched and called by the
        // parentheses that already follow the access
        let member_ty = Type::instance(db, &env, interface)
            .member(db, &env, member)
            .place
            .ignore_possibly_undefined();
        let kind = match member_ty {
            Some(Type::FunctionLiteral(function)) if function.is_classmethod(db) => {
                conformance::WitnessKind::ClassMethod
            }
            Some(Type::BoundMethod(method)) if method.function(db).is_classmethod(db) => {
                conformance::WitnessKind::ClassMethod
            }
            Some(Type::BoundMethod(_) | Type::FunctionLiteral(_) | Type::Callable(_)) => {
                conformance::WitnessKind::Method
            }
            _ => conformance::WitnessKind::Property,
        };
        Some(conformance::WitnessDispatch {
            interface: spelling,
            import,
            member: member.to_owned(),
            kind,
        })
    }

    /// basedpython: how `x is <interface>` is answered once conformances are in
    /// play. `None` when the target is not an interface anything conforms to
    /// here, where the ordinary `isinstance` lowering is still right
    pub fn conformance_test(
        &self,
        target: &ast::Expr,
    ) -> Option<crate::types::conformance::ConformanceTest> {
        use crate::types::conformance;

        let db = self.db;
        if !self.file.file(db).source_type(db).is_basedpython() {
            return None;
        }
        let interface = target.inferred_type(self)?.to_class_type(db)?;
        if !conformance::visible_conformances(db, self.file.file(db))
            .iter()
            .any(|(_, declared)| declared.class_literal(db) == interface.class_literal(db))
        {
            return None;
        }
        let members = Some(
            conformance::interface_requirements(db, interface)
                .iter()
                .map(ToString::to_string)
                .collect(),
        );
        Some(conformance::ConformanceTest { members })
    }

    /// basedpython: when an attribute access resolves to an `extension`
    /// member (this module's, or one from a module imported with a plain
    /// `import mod`), the backing-function rewrite the transpiler applies.
    /// `None` for ordinary attributes — extensions never shadow declared
    /// members, so this only answers when normal member lookup finds nothing
    pub fn extension_attribute_info(
        &self,
        attribute: &ast::ExprAttribute,
    ) -> Option<crate::types::extensions::ExtensionAttributeInfo> {
        let env = self.program_environment();
        let db = self.db;
        let receiver_ty = attribute.value.inferred_type(self)?;
        if !receiver_ty
            .member(db, &env, attribute.attr.as_str())
            .place
            .is_undefined()
        {
            return None;
        }
        let resolution = crate::types::extensions::resolve_extension_member(
            db,
            &env,
            self.file(),
            receiver_ty,
            attribute.attr.as_str(),
        )?;
        // a use-site modifier does not turn an instance into a class object:
        // `A()` is a `final A`, still a receiver a `class def` has to widen
        let receiver_is_class = receiver_ty
            .erase_restriction(db)
            .nominal_class(db, &env)
            .is_none();
        self.extension_rewrite(&resolution, attribute.attr.as_str(), receiver_is_class)
    }

    /// basedpython: when this attribute access resolves to a conversion dunder
    /// the *prelude* declares, how it was reached.
    ///
    /// A prelude member is type-only, and the prelude spells a conversion as
    /// construction — so `frozenset.__of__(x)` written out by hand means
    /// `frozenset(x)`, exactly what the conversion site emits. Without this the
    /// call would survive verbatim into the output and raise `AttributeError`,
    /// which is the one thing a member that does not exist at runtime must not
    /// be allowed to do
    pub fn prelude_conversion_dunder(
        &self,
        attribute: &ast::ExprAttribute,
    ) -> Option<PreludeDunderReceiver> {
        let env = self.program_environment();
        let db = self.db;
        let name = attribute.attr.as_str();
        if !crate::types::conversions::CONVERSION_DUNDERS.contains(&name) {
            return None;
        }
        let receiver_ty = attribute.value.inferred_type(self)?;
        if !receiver_ty.member(db, &env, name).place.is_undefined() {
            return None;
        }
        let resolution = crate::types::extensions::resolve_extension_member(
            db,
            &env,
            self.file(),
            receiver_ty,
            name,
        )?;
        if !crate::types::extensions::is_prelude_extension(
            db,
            self.file.file(db),
            resolution.extension,
        ) {
            return None;
        }
        // a use-site modifier does not turn an instance into a class object:
        // `A()` is a `final A`, still an instance
        Some(
            if receiver_ty
                .erase_restriction(db)
                .nominal_class(db, &env)
                .is_none()
            {
                PreludeDunderReceiver::Class
            } else {
                PreludeDunderReceiver::Instance
            },
        )
    }

    /// basedpython: how the transpiler rewrites an *operator* that resolved to
    /// an extension member — `+text` → `_by_ext__str____pos__(text)`. `None`
    /// for every operator the operand's own type supports, which is all of
    /// them outside a file that declares such an extension
    pub fn extension_operator_info(&self, expr: &Expr) -> Option<ExtensionOperatorRewrite> {
        let env = &self.program_environment();
        let db = self.db;
        let operator = match expr {
            Expr::UnaryOp(unary) => {
                let operand = unary.operand.inferred_type(self)?;
                crate::types::extensions::unary_extension_operator(
                    db,
                    env,
                    self.file(),
                    unary.op,
                    operand,
                )?
            }
            Expr::BinOp(binary) => {
                let left = binary.left.inferred_type(self)?;
                let right = binary.right.inferred_type(self)?;
                crate::types::extensions::binary_extension_operator(
                    db,
                    env,
                    self.file(),
                    left,
                    binary.op,
                    right,
                )?
            }
            // only a single comparison lowers: a chain (`a < b < c`) is two
            // calls joined by a short-circuit, which is a different rewrite
            Expr::Compare(compare) => {
                let [op] = compare.ops.as_ref() else {
                    return None;
                };
                let [right_expr] = compare.comparators.as_ref() else {
                    return None;
                };
                let left = compare.left.inferred_type(self)?;
                let right = right_expr.inferred_type(self)?;
                crate::types::extensions::comparison_extension_operator(
                    db,
                    env,
                    self.file(),
                    left,
                    *op,
                    right,
                )?
            }
            _ => return None,
        };
        Some(ExtensionOperatorRewrite {
            info: self.extension_rewrite(&operator.resolution, operator.member, false)?,
            reflected: operator.reflected,
        })
    }

    /// the transpiler's rewrite for a resolved extension member. `None` for a
    /// prelude member — the grapheme string surface has no backing function,
    /// and its own lowering handles it — or for one whose declaring module this
    /// file has no import spelling for
    fn extension_rewrite(
        &self,
        resolution: &crate::types::extensions::ExtensionMemberResolution<'db>,
        member: &str,
        receiver_is_class: bool,
    ) -> Option<crate::types::extensions::ExtensionAttributeInfo> {
        let db = self.db;
        if crate::types::extensions::is_prelude_extension(
            db,
            self.file.file(db),
            resolution.extension,
        ) || resolution.ambiguous_with.is_some_and(|other| {
            crate::types::extensions::is_prelude_extension(db, self.file.file(db), other)
        }) {
            return None;
        }
        let extension_file = resolution.extension.file(db);
        let import_from = if extension_file == self.file.file(db) {
            None
        } else {
            // spelled the way this file already imports the module: ty's absolute
            // module name can be one the interpreter cannot resolve (a file under a
            // directory that is not an importable package), and a relative import has
            // no absolute spelling at all
            Some(crate::types::conversions::imported_module_spelling(
                db,
                self.file.file(db),
                extension_file,
            )?)
        };
        Some(crate::types::extensions::ExtensionAttributeInfo {
            function: crate::types::extensions::backing_function_name(
                db,
                resolution.extension,
                member,
            ),
            kind: resolution.kind,
            import_from,
            receiver_is_class,
        })
    }

    /// basedpython: the conversions a call's arguments need, as
    /// `(argument range, conversion)` pairs.
    ///
    /// A conformance in scope, a `__from__` / `__of__` on the parameter type or
    /// an `__into__` on the argument's own type all make an argument acceptable
    /// where it otherwise is not; the transpiler wraps it in
    /// the call the checker resolved. The checker accepts exactly the same set —
    /// both sides ask `repair_conversion` with the argument's type and the
    /// parameter type of the single matching overload, and both decline when the
    /// callee is overloaded or a union, where no single parameter type is
    /// well-defined.
    ///
    /// Unlike a conformance, a conversion dunder travels with the type rather
    /// than with imports, so there is no registry of applicable ones to check
    /// first — that is the cost of the dunders being a property of the type they
    /// convert to. `call_may_convert` is what keeps it off the hot path instead.
    /// Measured on a synthetic file of 3000 calls and nothing else, the cost over
    /// skipping this entirely is ~9% of transpile time, against ~13% ungated.
    pub fn call_conversions(
        &self,
        call: &ast::ExprCall,
    ) -> Vec<(
        ruff_text_size::TextRange,
        crate::types::conversions::ConversionInfo,
    )> {
        let env = &self.program_environment();
        let db = self.db;
        if !self.file.file(db).source_type(db).is_basedpython() {
            return Vec::new();
        }
        let Some(callable_ty) = call.func.inferred_type(self) else {
            return Vec::new();
        };
        if !self.call_may_convert(call, callable_ty) {
            return Vec::new();
        }
        let arguments: Vec<ast::ArgOrKeyword> = call.arguments.iter_source_order().collect();
        let Some(parameter_types) =
            crate::types::conversions::call_parameter_types(self, callable_ty, call)
        else {
            return Vec::new();
        };
        let mut conversions = Vec::new();
        for (argument, parameter_type) in arguments.iter().zip(parameter_types) {
            let Some(parameter_type) = parameter_type else {
                continue;
            };
            let value = argument.value();
            let Some(argument_type) = value.inferred_type(self) else {
                continue;
            };
            let Some(repair) = crate::types::conversions::repair_conversion(
                db,
                env,
                self.file.file(db),
                argument_type,
                parameter_type,
                Some(value),
            ) else {
                continue;
            };
            conversions.push((
                value.range(),
                crate::types::conversions::conversion_info(
                    db,
                    env,
                    self.file.file(db),
                    self,
                    value,
                    &repair,
                ),
            ));
        }
        conversions
    }

    /// could any conversion apply at this call at all?
    ///
    /// Binding the call a second time to find out is the expensive part, and
    /// almost no call in almost any file converts anything. This answers off
    /// cached signatures instead: a conversion needs a parameter type that
    /// declares `__from__` / `__of__`, an argument whose own type declares
    /// `__into__`, or a conformance in scope. Anything it cannot read
    /// falls through to the full check, so the gate can only save work — it can
    /// never change an answer.
    fn call_may_convert(&self, call: &ast::ExprCall, callable_ty: Type<'db>) -> bool {
        let env = &self.program_environment();
        let db = self.db;
        if !crate::types::conformance::visible_conformances(db, self.file.file(db)).is_empty() {
            return true;
        }
        for argument in call.arguments.iter_source_order() {
            match argument.value().inferred_type(self) {
                Some(ty)
                    if crate::types::conversions::may_convert(db, env, self.file.file(db), ty) =>
                {
                    return true;
                }
                // an argument whose type is unknown here could be anything
                None => return true,
                Some(_) => {}
            }
        }
        let signature = match callable_ty {
            Type::FunctionLiteral(function) => function.signature(db),
            Type::BoundMethod(method) => method.function(db).signature(db),
            // a class, a callable instance, a union: the parameter types are not
            // one cached signature away, so do the full check
            _ => return true,
        };
        signature.iter().any(|overload| {
            overload.parameters().iter().any(|parameter| {
                let parameter_type = parameter.annotated_type();
                crate::types::conversions::may_convert(db, env, self.file.file(db), parameter_type)
                    // a callable that returns something the parameter does not
                    // want is repaired by an adapter rather than by a dunder, so
                    // there is no class for `may_convert` to read it off. asked
                    // of the parameter and not of the argument because only the
                    // *declared* side decides whether a return may be dropped
                    || crate::types::conversions::target_discards_return(db, env, parameter_type)
            })
        })
    }

    /// basedpython: the conversions a statement's value needs, as
    /// `(range to wrap, conversion)` pairs.
    ///
    /// Covers every non-call conversion site: an annotated or plain assignment
    /// (including to an attribute) and a `return`. For a collection literal the
    /// wraps are per element. This is the same `value_conversions` answer the
    /// checker used to accept the statement, so the emitted code converts exactly
    /// where the checker said it would.
    pub fn statement_conversions(
        &self,
        stmt: &ast::Stmt,
    ) -> Vec<(
        ruff_text_size::TextRange,
        crate::types::conversions::ConversionInfo,
    )> {
        let env = &self.program_environment();
        let db = self.db;
        if !self.file.file(db).source_type(db).is_basedpython() {
            return Vec::new();
        }
        let Some((value, declared)) = self.conversion_site_of(stmt) else {
            return Vec::new();
        };
        crate::types::conversions::value_conversions(
            db,
            env,
            self.file.file(db),
            self,
            value,
            declared,
        )
        .into_iter()
        .map(|(range, repair)| {
            // the anchor is the value being wrapped, which decides what the
            // emitted names have to resolve to — for an element-wise
            // conversion that is the element, not the whole literal
            let anchor = crate::types::conversions::expression_at(value, range).unwrap_or(value);
            let info = crate::types::conversions::conversion_info(
                db,
                env,
                self.file.file(db),
                self,
                anchor,
                &repair,
            );
            (range, info)
        })
        .collect()
    }

    /// the value expression and the type it is checked against, for a statement
    /// that is a conversion site
    fn conversion_site_of<'ast>(
        &self,
        stmt: &'ast ast::Stmt,
    ) -> Option<(&'ast ast::Expr, Type<'db>)> {
        let env = self.program_environment();
        let db = self.db;
        match stmt {
            ast::Stmt::AnnAssign(assignment) => {
                let value = assignment.value.as_deref()?;
                let definition = semantic_index(db, self.file).expect_single_definition(assignment);
                let declared = crate::types::inferred_declaration(db, definition)
                    .declared()?
                    .inner_type();
                Some((value, declared))
            }
            ast::Stmt::Assign(assignment) => {
                // one target only: with several, each could declare a different type
                // and there would be no single answer for the one value. the checker
                // declines the same shapes — see `is_conversion_site`
                match assignment.targets.as_slice() {
                    [ast::Expr::Attribute(attribute)] => {
                        let object_ty = attribute.value.inferred_type(self)?;
                        let declared = object_ty
                            .member(db, &env, attribute.attr.as_str())
                            .place
                            .ignore_possibly_undefined()?;
                        Some((&assignment.value, declared))
                    }
                    // a plain name's declaration lives in another statement: the one
                    // the binding was checked against, which is what reaches it here
                    [ast::Expr::Name(name)] => {
                        let index = semantic_index(db, self.file);
                        let binding = index.try_definition(name)?;
                        let declarations = index
                            .use_def_map(binding.file_scope(db))
                            .declarations_at_binding(binding);
                        let declared =
                            crate::place::place_from_declarations(db, &env, declarations)
                                .ignore_conflicting_declarations()
                                .place
                                .ignore_possibly_undefined()?;
                        Some((&assignment.value, declared))
                    }
                    _ => None,
                }
            }
            ast::Stmt::Return(ret) => {
                let value = ret.value.as_deref()?;
                // the scope has to be found from the *value*: `scope()` falls back to
                // the global scope for a statement, which names no function
                let declared = self.declared_return_type(ast::AnyNodeRef::from(value))?;
                Some((value, declared))
            }
            _ => None,
        }
    }

    /// the declared return type of the function enclosing `node`
    fn declared_return_type(&self, node: ast::AnyNodeRef) -> Option<Type<'db>> {
        let db = self.db;
        let index = semantic_index(db, self.file);
        let file_scope = self.scope(node)?;
        let function_ref = index.scope(file_scope).node().as_function()?;
        let module = parsed_module(db, self.file.python_file(db)).load(db);
        let function = function_ref.node(&module);
        // a generator's declared type describes the generator, not the returned
        // value; the checker checks those against the yield type instead
        if function.is_async || file_scope.is_generator_function(index) {
            return None;
        }
        crate::types::conversions::function_declared_return_type(db, self.file.file(db), function)
    }

    /// basedpython: whether subscripting `value` is a runtime `__getitem__`
    /// call rather than a type specialization, which decides what a keyword
    /// subscript on it lowers to. `false` for a value the checker could not
    /// resolve — the specialization reading is the one it also checks
    pub fn subscript_is_getitem_call(&self, value: &Expr) -> bool {
        let env = &self.program_environment();
        value
            .inferred_type(self)
            .is_some_and(|ty| crate::types::subscript::is_runtime_subscript(self.db, env, ty))
    }

    /// basedpython: whether an attribute access resolves through an *implicit
    /// receiver* — `x.fn` where `fn` names a receiver callable (`int.() -> str`)
    /// in scope rather than a member of `x`. The transpiler rewrites those to
    /// `fn(x)`. Like extensions, a receiver callable never shadows a declared
    /// member, and an extension member wins over it
    pub fn implicit_receiver_attribute(&self, attribute: &ast::ExprAttribute) -> bool {
        let env = &self.program_environment();
        let db = self.db;
        let Some(receiver_ty) = attribute.value.inferred_type(self) else {
            return false;
        };
        let Some(scope) = self.scope(ast::AnyNodeRef::from(attribute)) else {
            return false;
        };
        crate::types::receivers::is_implicit_receiver_attribute(
            db,
            env,
            self.file(),
            scope.to_scope_id(db, self.file),
            attribute,
            receiver_ty,
        )
    }

    /// basedpython: how a bare name resolves through the enclosing trailing
    /// lambda block's receiver, which the transpiler rewrites to the block's
    /// receiver parameter. `None` for every name that resolves any other way —
    /// the receiver and its members are the last fallback
    pub fn implicit_receiver_name(
        &self,
        name: &ast::ExprName,
    ) -> Option<ImplicitReceiverReference> {
        let env = &self.program_environment();
        let scope = self.scope(ast::AnyNodeRef::from(name))?;
        let resolved = crate::types::receivers::implicit_receiver_name(
            self.db,
            env,
            self.file(),
            scope.to_scope_id(self.db, self.file),
            name.id.as_str(),
            Some(name),
        )?;
        Some(match resolved {
            crate::types::receivers::ImplicitReceiverName::Receiver(_) => {
                ImplicitReceiverReference::Receiver
            }
            crate::types::receivers::ImplicitReceiverName::Member(_) => {
                ImplicitReceiverReference::Member
            }
            crate::types::receivers::ImplicitReceiverName::ExtensionMember {
                resolution, ..
            } => ImplicitReceiverReference::ExtensionMember(self.extension_rewrite(
                &resolution,
                name.id.as_str(),
                false,
            )?),
        })
    }

    /// basedpython: how each positional argument of a django lookup method that
    /// spells a `__` lookup as an expression lowers to a keyword —
    /// `filter(author.name == "x")` → `filter(author__name="x")`. Empty for
    /// every other call, and for any argument the checker did not read as a
    /// lookup, which the transpiler must then leave exactly as written
    pub fn django_lookup_arguments(&self, call: &ast::ExprCall) -> Vec<DjangoLookupArgument> {
        let env = &self.program_environment();
        let db = self.db;
        let Some(callee) = call.func.inferred_type(self) else {
            return Vec::new();
        };
        let Some(scope) = self.scope(ast::AnyNodeRef::from(call)) else {
            return Vec::new();
        };
        crate::types::dedicated::django::lookup_call_lowering(
            db,
            env,
            self.file(),
            scope.to_scope_id(db, self.file),
            callee,
            call,
        )
        .into_iter()
        .map(|lowering| DjangoLookupArgument {
            argument: lowering.argument,
            key: lowering.key,
            value: lowering.value,
        })
        .collect()
    }

    /// basedpython: the enum a *context-sensitively* resolved name must be
    /// qualified with in the emitted python — `Red` in a `Color` context lowers
    /// to `Color.Red`. `None` for every name that resolves the ordinary way
    pub fn context_sensitive_qualifier(&self, name: &ast::ExprName) -> Option<String> {
        let scope = self.scope(ast::AnyNodeRef::from(name))?;
        crate::types::context_sensitive::qualifier_for_unbound_name(
            self.db,
            &self.program_environment(),
            self.file.file(self.db),
            scope.to_scope_id(self.db, self.file),
            name.id.as_str(),
            || name.inferred_type(self),
        )
        .map(Name::to_string)
    }

    /// basedpython: the enum a bare `case A:` must be qualified with in the
    /// emitted python — `case Color.Red:`. `None` for a name that is the capture
    /// python spells it as, and for every pattern that is not a bare name
    pub fn case_name_qualifier(&self, identifier: &ast::Identifier) -> Option<String> {
        let index = semantic_index(self.db, self.file);
        let case_name = index.case_name(NodeKey::from_node(identifier))?;
        crate::types::context_sensitive::resolve_case_name(
            self.db,
            &self.program_environment(),
            case_name,
        )
        .map(|member| member.qualifier(self.db).to_string())
    }

    /// basedpython: the bracketed type-argument spelling the transpiler
    /// injects at a bare constructor call of a generic class (`A(1)` →
    /// `"int"`), read from the inferred type of the constructed instance.
    /// `None` when the callee is not a generic class literal or no runtime
    /// spelling exists — reification of constructors is best-effort, never
    /// an error
    pub fn reified_constructor_type_arguments(&self, call: &ast::ExprCall) -> Option<String> {
        let env = &self.program_environment();
        let callee_ty = call.func.inferred_type(self)?;
        let class_literal = callee_ty.as_class_literal()?;
        let constructed = call.inferred_type(self)?;
        crate::types::reified_infer::constructor_specialization_display(
            self.db,
            env,
            self.file(),
            class_literal,
            constructed,
        )
    }

    /// basedpython: the keyword the transpiler passes a trailing lambda block
    /// with — the name of the callee's last declared parameter. `None` means
    /// the lambda is appended as a positional argument instead (unknown
    /// callee signature, or a variadic / positional-only last parameter)
    pub fn trailing_lambda_keyword(&self, callee: &ast::Expr) -> Option<String> {
        let callee_ty = callee.inferred_type(self)?;
        crate::types::trailing_lambda::trailing_lambda_keyword(self.db, callee_ty)
            .map(|name| name.to_string())
    }

    /// basedpython: whether the trailing-lambda callee's callback declares an
    /// implicit receiver — the block then binds it as a leading parameter, which
    /// its body reads members off unqualified and spells `self`
    pub fn trailing_lambda_callback_has_receiver(&self, callee: &ast::Expr) -> bool {
        callee.inferred_type(self).is_some_and(|ty| {
            crate::types::trailing_lambda::trailing_lambda_receiver_type(self.db, ty).is_some()
        })
    }

    /// basedpython: whether the trailing-lambda callee's callback parameter is
    /// marked `once` — the block then runs exactly once (`with`-semantics), so
    /// its `return` may target the enclosing scope. `false` for anything the
    /// marker can't be read from (a non-function-literal callee).
    pub fn trailing_lambda_callee_is_once(&self, callee: &ast::Expr) -> bool {
        callee
            .inferred_type(self)
            .is_some_and(|ty| crate::types::trailing_lambda::callee_callback_is_once(self.db, ty))
    }

    /// basedpython: how the parametric type test `lhs is rhs` (keyword form)
    /// resolves, from the operands' inferred types. `rhs` may name the target
    /// specialization directly (`list[int]`) or through an alias — an implicit
    /// alias whose value is a specialization (`X = list[int]`) or a PEP 695
    /// `type` alias. `None` when `rhs` does not resolve to a specialization —
    /// the test is then an ordinary isinstance lowering
    pub fn parametric_is_plan(
        &self,
        lhs: &ast::Expr,
        rhs: &ast::Expr,
    ) -> Option<crate::types::reified_infer::ParametricIsPlan> {
        let env = &self.program_environment();
        let alias = crate::types::reified_infer::parametric_is_target(
            self.db,
            env,
            rhs.inferred_type(self)?,
        )?;
        let lhs_ty = lhs.inferred_type(self)?;
        Some(crate::types::reified_infer::classify_parametric_is(
            self.db,
            env,
            self.file(),
            lhs_ty,
            alias,
            rhs,
        ))
    }

    /// basedpython: [`Self::parametric_is_plan`] for a checked cast
    /// (`value cast T`). The same classification engine decides both — only the
    /// target's inference position differs, since a cast's target is a *type*
    /// expression while an `is`-rhs is a value expression.
    pub fn parametric_cast_plan(
        &self,
        value: &ast::Expr,
        target: &ast::Expr,
    ) -> Option<crate::types::reified_infer::ParametricIsPlan> {
        let env = &self.program_environment();
        let alias = crate::types::reified_infer::parametric_cast_target(
            self.db,
            env,
            target.inferred_type(self)?,
        )?;
        let value_ty = value.inferred_type(self)?;
        Some(crate::types::reified_infer::classify_parametric_is(
            self.db,
            env,
            self.file(),
            value_ty,
            alias,
            target,
        ))
    }

    /// basedpython: when `annotation` is a type expression denoting a union of
    /// specializations of one erased origin (`list[int] | list[str]`), how the
    /// arms differ. the transpiler uses this to give the parameter a reified
    /// type parameter, so the specialization travels with the call instead of
    /// being asked of a value that cannot answer
    pub fn erased_union(
        &self,
        annotation: &ast::Expr,
    ) -> Option<crate::types::reified_infer::ErasedUnion> {
        let env = &self.program_environment();
        crate::types::reified_infer::erased_union(
            self.db,
            env,
            self.file(),
            annotation.inferred_type(self)?,
        )
    }

    pub fn line_index(&self) -> LineIndex {
        line_index(self.db, self.file())
    }

    /// Returns a map from symbol name to that symbol's
    /// type and definition site (if available).
    ///
    /// The symbols are the symbols in scope at the given
    /// AST node.
    pub fn members_in_scope_at(
        &self,
        node: ast::AnyNodeRef<'_>,
    ) -> FxHashMap<Name, MemberDefinition<'db>> {
        let db = self.db;
        let mut members = FxHashMap::default();
        let program_file = self.program_file();
        let index = semantic_index(self.db, program_file);
        let Some(file_scope) = self.scope(node) else {
            return members;
        };
        for (file_scope, _) in index
            .visible_ancestor_scopes(file_scope)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            for memberdef in
                all_reachable_members(db, file_scope.to_scope_id(self.db, program_file))
            {
                members.insert(
                    memberdef.member.name,
                    MemberDefinition {
                        ty: memberdef.member.ty,
                        first_reachable_definition: memberdef.first_reachable_definition,
                    },
                );
            }
        }
        members
    }

    /// Resolve the given import made in this file to a Type
    pub fn resolve_module_type(&self, module: Option<&str>, level: u32) -> Option<Type<'db>> {
        let module = self.resolve_module(module, level)?;
        Some(Type::module_literal(self.db, self.program_file(), module))
    }

    /// Resolve the given import made in this file to a Module
    pub fn resolve_module(&self, module: Option<&str>, level: u32) -> Option<Module<'db>> {
        let importing_file = ImportingFile::File(
            self.file(),
            self.program_environment().resolver_environment(self.db),
        );
        let module_name =
            ModuleName::from_identifier_parts(self.db, importing_file, module, level).ok()?;
        resolve_module(self.db, importing_file, &module_name)
    }

    /// Returns completions for symbols available in a `import <CURSOR>` context.
    pub fn import_completions(&self) -> Vec<Completion<'db>> {
        let resolver_environment = self.program_environment().resolver_environment(self.db);
        list_modules(self.db, resolver_environment)
            .iter()
            .copied()
            .map(|module| {
                let builtin = module.is_known(self.db, KnownModule::Builtins);
                let ty = Type::module_literal(self.db, self.program_file(), module);
                Completion {
                    name: CompactString::new(module.name(self.db).as_str()),
                    ty: Some(ty),
                    builtin,
                    is_type_check_only: false,
                }
            })
            .collect()
    }

    /// Returns completions for symbols available in a `from module import <CURSOR>` context.
    pub fn from_import_completions(&self, import: &ast::StmtImportFrom) -> Vec<Completion<'db>> {
        let module_name = match ModuleName::from_import_statement(
            self.db,
            ImportingFile::File(
                self.file(),
                self.program_environment().resolver_environment(self.db),
            ),
            import,
        ) {
            Ok(module_name) => module_name,
            Err(err) => {
                tracing::debug!(
                    "Could not extract module name from `{module:?}` with level {level}: {err:?}",
                    module = import.module,
                    level = import.level,
                );
                return vec![];
            }
        };
        self.module_completions(&module_name)
    }

    /// Returns submodule-only completions for the given module.
    pub fn import_submodule_completions_for_name(
        &self,
        module_name: &ModuleName,
    ) -> Vec<Completion<'db>> {
        let Some(module) = resolve_module(
            self.db,
            ImportingFile::File(
                self.file(),
                self.program_environment().resolver_environment(self.db),
            ),
            module_name,
        ) else {
            tracing::debug!("Could not resolve module from `{module_name:?}`");
            return vec![];
        };
        self.submodule_completions(&module)
    }

    /// Resolves `module_name` as if it were imported by this model's `File`.
    pub fn resolve_module_name(&self, module_name: &ModuleName) -> Option<Module<'db>> {
        resolve_module(
            self.db,
            ImportingFile::File(
                self.file(),
                self.program_environment().resolver_environment(self.db),
            ),
            module_name,
        )
    }

    /// Returns completions for symbols available in the given module as if
    /// it were imported by this model's `File`.
    pub fn module_completions(&self, module_name: &ModuleName) -> Vec<Completion<'db>> {
        let db = self.db;
        let Some(module) = self.resolve_module_name(module_name) else {
            tracing::debug!("Could not resolve module from `{module_name:?}`");
            return vec![];
        };
        let ty = Type::module_literal(self.db, self.program_file(), module);
        let builtin = module.is_known(self.db, KnownModule::Builtins);
        let private = self.foreign_private_symbols(ty);

        let mut completions = vec![];
        #[expect(
            clippy::iter_over_hash_type,
            reason = "completion order is determined later by relevance ranking"
        )]
        for member in all_members(db, &self.program_environment(), ty) {
            if private.is_some_and(|names| names.contains(&member.name)) {
                continue;
            }
            completions.push(Completion {
                name: CompactString::new(member.name),
                ty: Some(member.ty),
                builtin,
                is_type_check_only: member.is_type_check_only,
            });
        }
        completions.extend(self.submodule_completions(&module));
        completions
    }

    /// Returns completions for submodules of the given module.
    fn submodule_completions(&self, module: &Module<'db>) -> Vec<Completion<'db>> {
        let builtin = module.is_known(self.db, KnownModule::Builtins);

        let mut completions = vec![];
        for submodule in module.all_submodules(self.db) {
            let ty = Type::module_literal(self.db, self.program_file(), *submodule);
            let base = submodule.name(self.db).last_component();
            completions.push(Completion {
                name: CompactString::new(base),
                ty: Some(ty),
                builtin,
                is_type_check_only: false,
            });
        }
        completions
    }

    /// basedpython: the `private` symbols of `ty`, when `ty` is another file's
    /// module. they are the module's implementation, not its interface, so an
    /// IDE must not offer them here.
    fn foreign_private_symbols(&self, ty: Type<'db>) -> Option<&'db FxHashSet<Name>> {
        let db = self.db;
        let Type::ModuleLiteral(module) = ty else {
            return None;
        };
        let file = module.module(self.db).file(self.db)?;
        if file == self.file.file(db) {
            return None;
        }
        Some(crate::types::visibility::private_symbols(self.db, file))
    }

    /// Returns completions for symbols available in a `object.<CURSOR>` context.
    pub fn attribute_completions(&self, node: &ast::ExprAttribute) -> Vec<Completion<'db>> {
        let db = self.db;
        let Some(ty) = node.value.inferred_type(self) else {
            return Vec::new();
        };
        let private = self.foreign_private_symbols(ty);

        all_members(db, &self.program_environment(), ty)
            .into_iter()
            .filter(|member| !private.is_some_and(|names| names.contains(&member.name)))
            .map(|member| Completion {
                name: CompactString::new(member.name),
                ty: Some(member.ty),
                builtin: false,
                is_type_check_only: member.is_type_check_only,
            })
            .collect()
    }

    /// Returns completions for symbols available in the scope containing the
    /// given expression.
    ///
    /// If a scope could not be determined, then completions for the global
    /// scope of this model's `File` are returned.
    pub fn scoped_completions(&self, node: ast::AnyNodeRef<'_>) -> Vec<Completion<'db>> {
        let db = self.db;
        let program_file = self.program_file();
        let index = semantic_index(self.db, program_file);
        let Some(file_scope) = self.scope(node) else {
            return vec![];
        };
        let mut completions = vec![];
        for (file_scope, _) in index.ancestor_scopes(file_scope) {
            completions.extend(
                all_reachable_members(db, file_scope.to_scope_id(self.db, program_file)).map(
                    |memberdef| Completion {
                        name: CompactString::new(memberdef.member.name),
                        ty: Some(memberdef.member.ty),
                        builtin: false,
                        is_type_check_only: memberdef.member.is_type_check_only,
                    },
                ),
            );
        }

        // Add implicit module globals (like `__file__`, `__name__`, etc.) with their
        // correct types. These are added before builtins so that the deduplication
        // keeps the correct types (e.g., `__file__` is `str` for the current module,
        // not `str | None`).
        completions.extend(
            all_implicit_module_globals(self.db, self.file).map(|(name, ty)| Completion {
                name: CompactString::new(name),
                ty: Some(ty),
                builtin: true,
                is_type_check_only: false,
            }),
        );

        // Project-level builtins take precedence over the standard builtins.
        let project_builtins = ModuleName::new_static("__builtins__").unwrap();
        let importing_file =
            ImportingFile::File(self.file(), self.file.resolver_environment(self.db));
        if resolve_module(self.db, importing_file, &project_builtins).is_some() {
            completions.extend(
                self.module_completions(&project_builtins)
                    .into_iter()
                    .filter(|completion| !completion.is_type_check_only)
                    .map(|mut completion| {
                        completion.builtin = true;
                        completion
                    }),
            );
        }

        // Builtins are available in all scopes.
        let builtins = KnownModule::Builtins.name();
        completions.extend(
            self.module_completions(&builtins)
                .into_iter()
                .filter(|completion| !completion.is_type_check_only),
        );

        // The above can sometimes result in duplicates. Get rid of them.
        completions.sort_by(|c1, c2| c1.name.cmp(&c2.name));
        completions.dedup_by(|c1, c2| c1.name == c2.name);

        completions
    }

    /// basedpython: the type the bare `name` has where it means a module member
    /// the file never imported — `Mapping` is `typing.Mapping` — or `None` when
    /// it means no such thing.
    ///
    /// `in_type_expression` says whether a type is being written at the
    /// position asked about, since some of these names only carry their implicit
    /// meaning there: a value-position `Character` is an ordinary identifier.
    ///
    /// A keyword spelling — `dynamic` for `Any` — is not one of these names. It
    /// is a word of the language, offered and highlighted as one, and this
    /// answers about what a *name* means.
    pub fn implicit_name_type(&self, name: &str, in_type_expression: bool) -> Option<Type<'db>> {
        if !self.file().source_type(self.db).is_basedpython() {
            return None;
        }
        let implicit = implicit_name(name).filter(|implicit| !implicit.is_keyword)?;
        if !implicit.position.admits(in_type_expression) {
            return None;
        }
        implicit
            .resolve(self.db, &self.program_environment())
            .ignore_possibly_undefined()
    }

    /// basedpython: whether importing `name` from `file` would bind exactly what
    /// the bare `name` already means here.
    ///
    /// An IDE offering to write that import is offering a second spelling of
    /// something the file has already: `Mapping` names `typing.Mapping` with no
    /// import, and `collections.abc` re-exports that very class.
    ///
    /// `in_type_expression` is as in [`SemanticModel::implicit_name_type`] — a
    /// name that means nothing where the import would be used is a name the
    /// import is the only way to reach.
    pub fn import_is_implicitly_available(
        &self,
        name: &str,
        file: File,
        in_type_expression: bool,
    ) -> bool {
        let db = self.db;
        let Some(implicit) = self.implicit_name_type(name, in_type_expression) else {
            return false;
        };
        let program_file = ProgramFile::new(db, file, self.program_environment().program(db));
        imported_symbol(
            db,
            &self.program_environment(),
            Some(program_file),
            name,
            None,
        )
        .ignore_possibly_undefined()
            == Some(implicit)
    }

    /// basedpython: the type a *read* of `name` on an instance of `class_def` answers
    /// with, where the lookup finds anything at all.
    ///
    /// The native compiler asks this to decide what a class's storage for an attribute
    /// has to be able to hold. One assignment's type is not that answer: `self.parent =
    /// None` in `__init__` says the attribute holds `None` on the way out of the
    /// constructor, and says nothing about what is written into it afterwards. The
    /// attribute's own type is the union over every assignment the checker can see, and
    /// where it cannot see them all — an implicit attribute whose only assignment is
    /// `None`, which is a partial inference rather than a settled one — it carries
    /// `Unknown` beside them to say so.
    ///
    /// The name is the one the class body writes, not the one python mangles a private
    /// attribute to: member lookup answers under the written name.
    pub fn instance_attribute_type(
        &self,
        class_def: &ast::StmtClassDef,
        name: &str,
    ) -> Option<Type<'db>> {
        let env = self.program_environment();
        let db = self.db;
        let instance = class_def
            .inferred_type(self)?
            .to_instance_approximation(db, &env)?;
        instance
            .member(db, &env, name)
            .place
            .ignore_possibly_undefined()
    }

    /// Returns `true` if the given class definition's name was previously
    /// bound in the same scope (i.e., the class definition is a re-assignment).
    pub fn is_class_name_reassigned(&self, class_def: &ast::StmtClassDef) -> bool {
        let index = semantic_index(self.db, self.program_file());
        let definition = index.expect_single_definition(class_def);
        let scope = definition.scope(self.db);
        let table = place_table(self.db, scope);
        let place = table.place(definition.place(self.db));
        place.as_symbol().is_some_and(Symbol::is_reassigned)
    }

    /// The type the place `expr` loads was declared with, when the load reads something narrower
    /// than the declaration allows.
    ///
    /// `loaded_ty` is the type the load itself produced. Returns `None` unless a declaration
    /// bounds the place with a wider type than that. The resolution itself is
    /// `crate::place::declared_type_at_load`, which is not public.
    pub fn declared_type_at_load(
        &self,
        expr: ast::ExprRef<'_>,
        loaded_ty: Type<'db>,
    ) -> Option<Type<'db>> {
        crate::place::declared_type_at_load(
            self.db,
            &self.program_environment(),
            self.program_file(),
            expr,
            loaded_ty,
        )
    }

    /// Returns the scope in which `node` is defined (handles string annotations).
    pub fn scope(&self, node: ast::AnyNodeRef<'_>) -> Option<FileScopeId> {
        let index = semantic_index(self.db, self.program_file());
        match self.node_in_ast(node) {
            ast::AnyNodeRef::Identifier(identifier) => index.try_expression_scope_id(identifier),

            // Nodes implementing `HasDefinition`
            ast::AnyNodeRef::StmtFunctionDef(function) => Some(
                function
                    .definition(self)
                    .scope(self.db)
                    .file_scope_id(self.db),
            ),
            ast::AnyNodeRef::StmtClassDef(class) => {
                Some(class.definition(self).scope(self.db).file_scope_id(self.db))
            }
            ast::AnyNodeRef::Parameter(parameter) => Some(
                parameter
                    .definition(self)
                    .scope(self.db)
                    .file_scope_id(self.db),
            ),
            ast::AnyNodeRef::ParameterWithDefault(parameter) => Some(
                parameter
                    .definition(self)
                    .scope(self.db)
                    .file_scope_id(self.db),
            ),
            ast::AnyNodeRef::ExceptHandlerExceptHandler(handler) => handler
                .optional_definition(self)
                .map(|definition| definition.scope(self.db).file_scope_id(self.db))
                .or_else(|| index.try_expression_scope_id(handler.type_.as_deref()?))
                .or(Some(FileScopeId::global())),
            ast::AnyNodeRef::TypeParamTypeVar(var) => {
                Some(var.definition(self).scope(self.db).file_scope_id(self.db))
            }

            // Fallback
            node => match node.as_expr_ref() {
                // If we couldn't identify a specific
                // expression that we're in, then just
                // fall back to the global scope.
                None => Some(FileScopeId::global()),
                Some(expr) => index.try_expression_scope_id(&expr),
            },
        }
    }

    /// Returns the scopes enclosing `node`, starting with the scope containing
    /// the node itself.
    ///
    /// Like [`Self::scope`], this handles nodes inside string annotations.
    pub fn ancestor_scopes(
        &self,
        node: ast::AnyNodeRef<'_>,
    ) -> impl Iterator<Item = (FileScopeId, &Scope)> + '_ {
        let index = semantic_index(self.db, self.program_file());
        self.scope(node)
            .into_iter()
            .flat_map(move |scope| index.ancestor_scopes(scope))
    }

    /// Returns the first local definition created by `covering_node`, if any.
    ///
    /// A local definition is a user-visible definition associated with `covering_node` itself, or
    /// one of its ancestors, whose focus range covers the queried node. This returns only the first
    /// match because one syntax node can represent multiple semantic definitions, for example
    /// `from module import *`. This helper is intended for classifying the local occurrence, such as
    /// deciding whether it is a binding or declaration, not for enumerating every symbol introduced
    /// by the syntax.
    pub fn first_local_definition(
        &self,
        covering_node: &CoveringNode<'_>,
    ) -> Option<Definition<'db>> {
        let index = semantic_index(self.db, self.program_file());
        let parsed = parsed_module(self.db, self.python_file()).load(self.db);
        let target_range = covering_node.node().range();

        for node in covering_node.ancestors() {
            let Some(definitions) = index.try_definitions(node) else {
                continue;
            };

            if let Some(definition) = definitions.iter().copied().find(|definition| {
                let kind = definition.kind(self.db);
                kind.is_user_visible()
                    && definition
                        .focus_range(self.db, &parsed)
                        .range()
                        .contains_range(target_range)
            }) {
                return Some(definition);
            }
        }

        None
    }

    /// Get a "safe" [`ast::AnyNodeRef`] to use for referring to the given (sub-)AST node.
    ///
    /// If we're analyzing a string annotation, it will return the string literal's node.
    /// Otherwise it will return the input.
    fn node_in_ast<'a>(&'a self, node: ast::AnyNodeRef<'a>) -> ast::AnyNodeRef<'a> {
        if let Some(string_annotation) = &self.in_string_annotation_expr {
            (&**string_annotation).into()
        } else {
            node
        }
    }

    /// Get a "safe" [`Expr`] to use for referring to the given (sub-)expression.
    ///
    /// If we're analyzing a string annotation, it will return the string literal's expression.
    /// Otherwise it will return the input.
    fn expr_in_ast<'a>(&'a self, expr: &'a Expr) -> &'a Expr {
        if let Some(string_annotation) = &self.in_string_annotation_expr {
            string_annotation
        } else {
            expr
        }
    }

    /// Get a "safe" [`ExprRef`] to use for referring to the given (sub-)expression.
    ///
    /// If we're analyzing a string annotation, it will return the string literal's expression.
    /// Otherwise it will return the input.
    fn expr_ref_in_ast<'a>(&'a self, expr: ExprRef<'a>) -> ExprRef<'a> {
        if let Some(string_annotation) = &self.in_string_annotation_expr {
            ExprRef::from(string_annotation)
        } else {
            expr
        }
    }

    /// Given a string expression, determine if it's a string annotation, and if it is,
    /// yield the parsed sub-AST and a sub-model that knows it's analyzing a sub-AST.
    ///
    /// Analysis of the sub-AST should only be done with the sub-model, or else things
    /// may return nonsense results or even panic!
    pub fn enter_string_annotation(
        &self,
        string_expr: &ExprStringLiteral,
    ) -> Option<(Parsed<ModExpression>, Self)> {
        // Ask the inference engine whether this is actually a string annotation
        let expr = ExprRef::StringLiteral(string_expr);
        let index = semantic_index(self.db, self.program_file());
        // When looking up scopes, use the expr in the top-level AST
        // (we might be trying to enter a sub-sub-AST, so this isn't silly)
        let file_scope = index.expression_scope_id(&self.expr_ref_in_ast(expr));
        let scope = file_scope.to_scope_id(self.db, self.program_file());
        // When querying whether the expr is a string annotation, we do however use the actual expr
        // (the inference engine should record this information even for sub-nodes)
        if !infer_complete_scope_types(self.db, scope).is_string_annotation(expr) {
            return None;
        }

        // Parse the sub-AST and create a semantic model that knows it's in a sub-AST
        //
        // The string_annotation will be used as the expr/node for any query that needs
        // to look up a node in the AST to prevent panics, because these sub-AST nodes
        // are not in the File's AST!
        let source = source_text(self.db, self.file());
        let string_literal = string_expr.as_single_part_string()?;
        let ast = parsed_string_annotation(source.as_str(), string_literal).ok()?;
        let model = Self {
            db: self.db,
            file: self.file,
            // Use expr_in_ast here because we might be entering a sub-sub-AST
            in_string_annotation_expr: Some(Box::new(
                self.expr_in_ast(&Expr::StringLiteral(string_expr.clone()))
                    .clone(),
            )),
        };
        Some((ast, model))
    }

    /// Returns whether `annotation` declares a PEP 613 type alias.
    pub fn is_type_alias_annotation(&self, annotation: &Expr) -> bool {
        matches!(
            annotation.inferred_type(self),
            Some(Type::SpecialForm(SpecialFormType::TypeAlias))
        )
    }

    /// Returns whether `definition` defines a PEP 613 or PEP 695 type alias.
    pub fn is_type_alias_definition(&self, definition: Definition<'db>) -> bool {
        match definition.kind(self.db) {
            DefinitionKind::TypeAlias(_) => true,
            DefinitionKind::AnnotatedAssignment(assignment) => {
                let parsed = parsed_module(self.db, definition.python_file(self.db));
                let model = Self::new(self.db, definition.program_file(self.db));
                model.is_type_alias_annotation(assignment.annotation(&parsed.load(self.db)))
            }
            _ => false,
        }
    }

    /// Returns the type qualifiers (e.g. `Final`, `ClassVar`) for a given expression,
    /// if the expression refers to a name or attribute with declared qualifiers.
    pub fn type_qualifiers(&self, expr: ExprRef<'_>) -> TypeQualifiers {
        let db = self.db;
        match expr {
            ExprRef::Name(name) => {
                let Some(definition) =
                    definition_for_name(self, name, ImportAliasResolution::ResolveAliases)
                else {
                    return TypeQualifiers::empty();
                };
                let definition_file = definition.file(self.db);
                let module = parsed_module(self.db, definition.python_file(self.db)).load(self.db);
                if !definition
                    .kind(self.db)
                    .category(definition_file.is_stub(self.db), &module)
                    .is_declaration()
                {
                    return TypeQualifiers::empty();
                }
                let Some(declared) = inferred_declaration(self.db(), definition).declared() else {
                    return TypeQualifiers::empty();
                };
                declared.qualifiers()
            }
            ExprRef::Attribute(attr) => {
                let Some(value_ty) = attr.value.inferred_type(self) else {
                    return TypeQualifiers::empty();
                };
                value_ty
                    .member_lookup_with_policy(
                        db,
                        &self.program_environment(),
                        &attr.attr.id,
                        crate::types::MemberLookupPolicy::default(),
                    )
                    .qualifiers
            }
            _ => TypeQualifiers::empty(),
        }
    }

    /// Returns completion candidates for a string-literal expression based on its expected type.
    pub fn expected_string_literal_completions(
        &self,
        string_expr: &ast::ExprStringLiteral,
    ) -> Vec<ExpectedStringLiteralCompletion<'db>> {
        struct StringLiteralCandidates;
        type StringLiteralCandidatesVisitor<'db> = CycleDetector<
            'db,
            StringLiteralCandidates,
            Type<'db>,
            Vec<ExpectedStringLiteralCompletion<'db>>,
            3,
        >;

        fn collect<'db>(
            db: &'db dyn Db,
            ty: Type<'db>,
            visitor: &StringLiteralCandidatesVisitor<'db>,
        ) -> Vec<ExpectedStringLiteralCompletion<'db>> {
            match ty {
                Type::LiteralValue(literal) => literal
                    .as_string()
                    .map(|string_literal| {
                        let value = string_literal.value(db).to_string();
                        vec![ExpectedStringLiteralCompletion {
                            ty: Type::string_literal(db, &*value),
                            value,
                        }]
                    })
                    .unwrap_or_default(),
                Type::Union(union) => union
                    .elements(db)
                    .iter()
                    .flat_map(|element| collect(db, *element, visitor))
                    .collect(),
                Type::Intersection(intersection) => intersection
                    .positive(db)
                    .iter()
                    .flat_map(|element| collect(db, *element, visitor))
                    .collect(),
                Type::TypeAlias(alias) => {
                    visitor.visit(db, ty, || collect(db, alias.value_type(db), visitor))
                }
                _ => Vec::new(),
            }
        }
        let db = self.db;

        let Some(expected_ty) = self.string_literal_completion_expected_type(string_expr) else {
            return Vec::new();
        };

        let mut candidates = collect(db, expected_ty, &StringLiteralCandidatesVisitor::default());
        candidates.sort_unstable_by(|left, right| left.value.cmp(&right.value));
        candidates.dedup_by(|left, right| left.value == right.value);
        candidates
    }

    fn string_literal_completion_expected_type(
        &self,
        string_expr: &ast::ExprStringLiteral,
    ) -> Option<Type<'db>> {
        let expr = ast::ExprRef::from(string_expr);
        let index = semantic_index(self.db, self.program_file());
        let file_scope = index.try_expression_scope_id(&self.expr_ref_in_ast(expr))?;
        let scope = file_scope.to_scope_id(self.db, self.program_file());

        infer_complete_scope_types(self.db, scope).try_expected_type(expr)
    }
}

/// The type and definition of a symbol.
#[derive(Clone, Debug)]
pub struct MemberDefinition<'db> {
    pub ty: Type<'db>,
    pub first_reachable_definition: Definition<'db>,
}

/// basedpython: how the transpiler rewrites an operator whose dunder an
/// `extension` supplies
#[derive(Clone, Debug)]
pub struct ExtensionOperatorRewrite {
    /// the backing function and the import that reaches it
    pub info: crate::types::extensions::ExtensionAttributeInfo,
    /// whether the *right* operand is the receiver — a reflected binary
    /// operator (`__radd__`), or a membership test (`a in b` calls
    /// `b.__contains__(a)`)
    pub reflected: bool,
}

/// basedpython: how a hand-written conversion dunder the prelude supplies was
/// reached, which decides whether it can be lowered
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PreludeDunderReceiver {
    /// `frozenset.__of__(x)` — the receiver already names the class the call has
    /// to reach, so dropping the `.__of__` is the whole lowering
    Class,
    /// `fs.__of__(x)` — python reaches a classmethod through an instance too,
    /// but the class it would construct is not written anywhere here, and
    /// recovering it would mean evaluating the receiver a second time
    Instance,
}

/// basedpython: one positional argument of a django lookup method that spells a
/// `__` lookup as an expression, and the keyword it lowers to
#[derive(Clone, Debug)]
pub struct DjangoLookupArgument {
    /// the argument to replace
    pub argument: TextRange,
    /// the keyword's name (`author__name`, `published__gt`)
    pub key: String,
    /// the value, re-emitted from source so lowerings inside it still apply
    pub value: TextRange,
}

/// basedpython: what a bare name inside a trailing lambda block resolves to
/// through the block's callback receiver
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImplicitReceiverReference {
    /// `self` — the receiver itself
    Receiver,
    /// a member read off the receiver
    Member,
    /// a member an applicable `extension` supplies for the receiver, which
    /// lowers to its backing function bound to the receiver rather than to an
    /// attribute read
    ExtensionMember(crate::types::extensions::ExtensionAttributeInfo),
}

/// A classification of symbol names.
///
/// The ordering here is used for sorting completions.
///
/// This sorts "normal" names first, then dunder names and finally
/// single-underscore names. This matches the order of the variants defined for
/// this enum, which is in turn picked up by the derived trait implementation
/// for `Ord`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum NameKind {
    Normal,
    Dunder,
    Sunder,
}

impl NameKind {
    pub fn classify(name: &str) -> NameKind {
        // Dunder needs a prefix and suffix double underscore.
        // When there's only a prefix double underscore, this
        // results in explicit name mangling. We let that be
        // classified as-if they were single underscore names.
        //
        // Ref: <https://docs.python.org/3/reference/lexical_analysis.html#reserved-classes-of-identifiers>
        if name.starts_with("__") && name.ends_with("__") {
            NameKind::Dunder
        } else if name.starts_with('_') {
            NameKind::Sunder
        } else {
            NameKind::Normal
        }
    }
}

/// A suggestion for code completion.
#[derive(Clone, Debug)]
pub struct Completion<'db> {
    /// The label shown to the user for this suggestion.
    pub name: CompactString,
    /// The type of this completion, if available.
    ///
    /// Generally speaking, this is always available
    /// *unless* this was a completion corresponding to
    /// an unimported symbol. In that case, computing the
    /// type of all such symbols could be quite expensive.
    pub ty: Option<Type<'db>>,
    /// Whether this suggestion came from builtins or not.
    ///
    /// At time of writing (2025-06-26), this information
    /// doesn't make it into the LSP response. Instead, we
    /// use it mainly in tests so that we can write less
    /// noisy tests.
    pub builtin: bool,
    /// Whether this symbol is known to exist only for type checking and should
    /// be ranked below runtime values.
    pub is_type_check_only: bool,
}

#[derive(Clone, Debug)]
pub struct ExpectedStringLiteralCompletion<'db> {
    pub value: String,
    pub ty: Type<'db>,
}

pub trait HasType {
    /// Returns the inferred type of `self`.
    ///
    /// ## Panics
    /// May panic if `self` is from another file than `model`.
    fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>>;
}

pub trait HasDefinition {
    /// Returns the definition of `self`.
    ///
    /// ## Panics
    /// May panic if `self` is from another file than `model`.
    fn definition<'db>(&self, model: &SemanticModel<'db>) -> Definition<'db>;
}

pub(crate) trait HasOptionalDefinition {
    /// Returns the definition of `self`, if it has one.
    ///
    /// ## Panics
    /// May panic if `self` is from another file than `model`.
    fn optional_definition<'db>(&self, model: &SemanticModel<'db>) -> Option<Definition<'db>>;
}

impl HasType for ast::ExprRef<'_> {
    fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
        let file = model.program_file();
        let index = semantic_index(model.db, file);
        // TODO(#1637): semantic tokens is making this crash even with
        // `try_expr_ref_in_ast` guarding this, for now just use `try_expression_scope_id`.
        // The problematic input is `x: "float` (with a dangling quote). I imagine the issue
        // is we're too eagerly setting `is_string_annotation` in inference.
        let file_scope = index.try_expression_scope_id(&model.expr_ref_in_ast(*self))?;
        let scope = file_scope.to_scope_id(model.db, file);

        infer_complete_scope_types(model.db, scope).try_expression_type(*self)
    }
}

macro_rules! impl_expression_has_type {
    ($ty: ty) => {
        impl HasType for $ty {
            #[inline]
            fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
                let expression_ref = ExprRef::from(self);
                expression_ref.inferred_type(model)
            }
        }
    };
}

impl_expression_has_type!(ast::ExprBoolOp);
impl_expression_has_type!(ast::ExprNamed);
impl_expression_has_type!(ast::ExprBinOp);
impl_expression_has_type!(ast::ExprUnaryOp);
impl_expression_has_type!(ast::ExprLambda);
impl_expression_has_type!(ast::ExprIf);
impl_expression_has_type!(ast::ExprDict);
impl_expression_has_type!(ast::ExprSet);
impl_expression_has_type!(ast::ExprListComp);
impl_expression_has_type!(ast::ExprSetComp);
impl_expression_has_type!(ast::ExprDictComp);
impl_expression_has_type!(ast::ExprGenerator);
impl_expression_has_type!(ast::ExprAwait);
impl_expression_has_type!(ast::ExprYield);
impl_expression_has_type!(ast::ExprYieldFrom);
impl_expression_has_type!(ast::ExprCompare);
impl_expression_has_type!(ast::ExprCall);
impl_expression_has_type!(ast::ExprFString);
impl_expression_has_type!(ast::ExprTString);
impl_expression_has_type!(ast::ExprStringLiteral);
impl_expression_has_type!(ast::ExprBytesLiteral);
impl_expression_has_type!(ast::ExprNumberLiteral);
impl_expression_has_type!(ast::ExprBooleanLiteral);
impl_expression_has_type!(ast::ExprNoneLiteral);
impl_expression_has_type!(ast::ExprEllipsisLiteral);
impl_expression_has_type!(ast::ExprAttribute);
impl_expression_has_type!(ast::ExprSubscript);
impl_expression_has_type!(ast::ExprStarred);
impl_expression_has_type!(ast::ExprName);
impl_expression_has_type!(ast::ExprList);
impl_expression_has_type!(ast::ExprTuple);
impl_expression_has_type!(ast::ExprSlice);
impl_expression_has_type!(ast::ExprIpyEscapeCommand);
impl_expression_has_type!(ast::ExprCallableType);
impl_expression_has_type!(ast::ExprProtocolType);
impl_expression_has_type!(ast::ExprProtocolMethod);
impl_expression_has_type!(ast::ExprStatement);

impl HasType for ast::Expr {
    fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
        match self {
            Expr::BoolOp(inner) => inner.inferred_type(model),
            Expr::Named(inner) => inner.inferred_type(model),
            Expr::BinOp(inner) => inner.inferred_type(model),
            Expr::UnaryOp(inner) => inner.inferred_type(model),
            Expr::Lambda(inner) => inner.inferred_type(model),
            Expr::If(inner) => inner.inferred_type(model),
            Expr::Dict(inner) => inner.inferred_type(model),
            Expr::Set(inner) => inner.inferred_type(model),
            Expr::ListComp(inner) => inner.inferred_type(model),
            Expr::SetComp(inner) => inner.inferred_type(model),
            Expr::DictComp(inner) => inner.inferred_type(model),
            Expr::Generator(inner) => inner.inferred_type(model),
            Expr::Await(inner) => inner.inferred_type(model),
            Expr::Yield(inner) => inner.inferred_type(model),
            Expr::YieldFrom(inner) => inner.inferred_type(model),
            Expr::Compare(inner) => inner.inferred_type(model),
            Expr::Call(inner) => inner.inferred_type(model),
            Expr::FString(inner) => inner.inferred_type(model),
            Expr::TString(inner) => inner.inferred_type(model),
            Expr::StringLiteral(inner) => inner.inferred_type(model),
            Expr::BytesLiteral(inner) => inner.inferred_type(model),
            Expr::NumberLiteral(inner) => inner.inferred_type(model),
            Expr::BooleanLiteral(inner) => inner.inferred_type(model),
            Expr::NoneLiteral(inner) => inner.inferred_type(model),
            Expr::EllipsisLiteral(inner) => inner.inferred_type(model),
            Expr::Attribute(inner) => inner.inferred_type(model),
            Expr::Subscript(inner) => inner.inferred_type(model),
            Expr::Starred(inner) => inner.inferred_type(model),
            Expr::Name(inner) => inner.inferred_type(model),
            Expr::List(inner) => inner.inferred_type(model),
            Expr::Tuple(inner) => inner.inferred_type(model),
            Expr::Slice(inner) => inner.inferred_type(model),
            Expr::IpyEscapeCommand(inner) => inner.inferred_type(model),
            Expr::CallableType(inner) => inner.inferred_type(model),
            Expr::ProtocolType(inner) => inner.inferred_type(model),
            Expr::ProtocolMethod(inner) => inner.inferred_type(model),
            Expr::Statement(inner) => inner.inferred_type(model),
        }
    }
}

macro_rules! impl_binding_has_ty_def {
    ($ty: ty) => {
        impl HasDefinition for $ty {
            #[inline]
            fn definition<'db>(&self, model: &SemanticModel<'db>) -> Definition<'db> {
                let index = semantic_index(model.db, model.program_file());
                index.expect_single_definition(self)
            }
        }

        impl HasType for $ty {
            #[inline]
            fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
                let binding = HasDefinition::definition(self, model);
                Some(binding_type(model.db(), binding))
            }
        }
    };
    // basedpython: a parameter the parser synthesized may be left undeclared —
    // a trailing lambda block's `it`, when its callback passes none — and then
    // has no type of its own, rather than a missing definition to panic on
    ($ty: ty, optional) => {
        impl HasDefinition for $ty {
            #[inline]
            fn definition<'db>(&self, model: &SemanticModel<'db>) -> Definition<'db> {
                let index = semantic_index(model.db, model.file);
                index.expect_single_definition(self)
            }
        }

        impl HasType for $ty {
            #[inline]
            fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
                let index = semantic_index(model.db, model.file);
                Some(binding_type(model.db, index.try_definition(self)?))
            }
        }
    };
}

impl_binding_has_ty_def!(ast::StmtFunctionDef);
impl_binding_has_ty_def!(ast::StmtClassDef);
impl_binding_has_ty_def!(ast::Parameter, optional);
impl_binding_has_ty_def!(ast::ParameterWithDefault, optional);
impl_binding_has_ty_def!(ast::TypeParamTypeVar);
impl_binding_has_ty_def!(ast::TypeParamParamSpec);
impl_binding_has_ty_def!(ast::TypeParamTypeVarTuple);
impl_binding_has_ty_def!(ast::StmtTypeAlias);

impl HasType for ast::Alias {
    fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
        if &self.name == "*" {
            return Some(Type::Never);
        }
        let index = semantic_index(model.db, model.program_file());
        Some(binding_type(
            model.db(),
            index.expect_single_definition(self),
        ))
    }
}

impl HasOptionalDefinition for ast::ExceptHandlerExceptHandler {
    fn optional_definition<'db>(&self, model: &SemanticModel<'db>) -> Option<Definition<'db>> {
        self.name.as_ref()?;

        let index = semantic_index(model.db, model.program_file());
        Some(index.expect_single_definition(self))
    }
}

impl HasType for ast::ExceptHandlerExceptHandler {
    fn inferred_type<'db>(&self, model: &SemanticModel<'db>) -> Option<Type<'db>> {
        let definition = self.optional_definition(model)?;
        Some(binding_type(model.db(), definition))
    }
}

#[cfg(test)]
mod tests {
    use crate::db::tests::TestDbBuilder;
    use crate::{HasType, SemanticModel};
    use ruff_db::files::system_path_to_file;
    use ruff_db::parsed::parsed_module;
    use ty_python_core::ProgramFile;

    #[test]
    fn function_type() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/foo.py", "def test(): pass")
            .build()?;

        let foo = system_path_to_file(&db, "/src/foo.py").unwrap();

        let foo = ProgramFile::new(&db, foo, db.program_environment().program(&db));
        let ast = parsed_module(&db, foo.python_file(&db)).load(&db);

        let function = ast.suite()[0].as_function_def_stmt().unwrap();
        let model = SemanticModel::new(&db, foo);
        let ty = function.inferred_type(&model).unwrap();

        assert!(ty.is_function_literal());

        Ok(())
    }

    #[test]
    fn class_type() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/foo.py", "class Test: pass")
            .build()?;

        let foo = system_path_to_file(&db, "/src/foo.py").unwrap();

        let foo = ProgramFile::new(&db, foo, db.program_environment().program(&db));
        let ast = parsed_module(&db, foo.python_file(&db)).load(&db);

        let class = ast.suite()[0].as_class_def_stmt().unwrap();
        let model = SemanticModel::new(&db, foo);
        let ty = class.inferred_type(&model).unwrap();

        assert!(ty.is_class_literal());

        Ok(())
    }

    #[test]
    fn alias_type() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_file("/src/foo.py", "class Test: pass")
            .with_file("/src/bar.py", "from foo import Test")
            .build()?;

        let bar = system_path_to_file(&db, "/src/bar.py").unwrap();

        let bar = ProgramFile::new(&db, bar, db.program_environment().program(&db));
        let ast = parsed_module(&db, bar.python_file(&db)).load(&db);

        let import = ast.suite()[0].as_import_from_stmt().unwrap();
        let alias = &import.names[0];
        let model = SemanticModel::new(&db, bar);
        let ty = alias.inferred_type(&model).unwrap();

        assert!(ty.is_class_literal());

        Ok(())
    }
}
