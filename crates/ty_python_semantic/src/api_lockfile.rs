//! api lockfile generator
//!
//! walks first-party modules and produces a single deterministic, line-oriented
//! summary of the public type-level api. the format is intentionally terse —
//! the lockfile is meant to be diffed, not parsed back into types. any
//! type-level change in a public symbol surfaces as a line-level diff
//!
//! four `#`-prefixed header lines come first: `#api-lock:v=1` (format
//! version), `#tool:by=<version>`, `#python:<target>` and `#modules:<count>`.
//! the records follow, sorted lexicographically, one per line:
//!
//! ```text
//! <qualified>:c[<bases>]                              # class
//! <qualified>:c<tv>[<bases>]                          # generic class
//! <qualified>:c[<bases>]{<flags>}                     # class with flags
//! <qualified>:d(<params>)-><ret>                      # def / method
//! <qualified>:d{<deco>}<tv>(<params>)-><ret>          # method with decorators / typevars
//! <qualified>:v=<type>                                # variable / class attribute
//! <qualified>:v[<quals>]=<type>                       # qualified variable
//! <qualified>:i=<type>                                # instance attribute (via self.x)
//! <qualified>:t=<type>                                # type alias
//! <qualified>:t<tv>=<type>                            # generic type alias
//! <qualified>:p=getter|setter|deleter,...             # property accessors present
//! <qualified>:r=<target>                              # another name for a class/function/alias
//! <qualified>:m=<module>                              # module re-export
//! ```
//!
//! `<tv>` is `<variance? name, ...>` with `+`/`-`/`*` variance prefixes
//! `<flags>` are class flags joined by `,`: `dataclass`, `enum`, `final`,
//!   `named_tuple`, `protocol`, `typed_dict`
//! `<deco>` are function decorators joined by `,`: `abstractmethod`, `async`,
//!   `classmethod`, `deprecated`, `final`, `no_type_check`, `overload`,
//!   `override`, `property`, `staticmethod`, `type_check_only`
//! `<quals>` are variable qualifiers joined by `,`: `classvar`, `final`,
//!   `initvar`, `notrequired`, `readonly`, `required`
//!
//! `<qualified>` is a path: a module, then names within it. a class, function or type alias is
//! described once, at the path of the name its definition binds (`m.C.f`, or `m.f.<locals>.C`
//! inside a function), where public or not, as long as the public api reaches it. every other
//! path that reaches it is an `r` record whose `<target>` is that path, followed by the type
//! arguments of a specialized class (`m.Box[builtins.int]`). something from outside the project
//! is named by the same path in its own module (`builtins.dict`), and is not described
use std::fmt::Write;

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_python_ast::name::Name;
use ruff_python_ast::{PythonVersion, PythonVersionDeserializationError};
use rustc_hash::FxHashSet;
use ty_module_resolver::{ImportingFile, file_to_module, resolve_module};
use ty_python_core::definition::Definition;
use ty_python_core::scope::{ScopeId, ScopeKind};
use ty_python_core::{
    BindingWithConstraintsIterator, DeclarationsIterator, attribute_scopes, global_scope,
    place_table, semantic_index, use_def_map,
};

use crate::dunder_all::dunder_all_names;
use crate::place::{place_from_bindings, place_from_declarations};
use crate::types::ProgramEnvironment;
use crate::types::enums::is_enum_class;
use crate::types::function::{FunctionDecorators, FunctionType};
use crate::types::type_alias::TypeAliasType;
use crate::types::{
    ClassLiteral, KnownClass, KnownInstanceType, NominalInstanceType, Parameter, ParameterKind,
    PropertyInstanceType, Signature, SpecialFormType, Type, TypeQualifiers, TypeVarVariance,
    UnionType, binding_type,
};
use crate::{Db, Program};

/// the version of the format this `by` writes, which the first line of a lockfile names
pub const FORMAT_VERSION: &str = "1";

/// the first line of a lockfile, which names the version of its format
const FORMAT_HEADER_PREFIX: &str = "#api-lock:v=";

/// the header line naming the `by` that wrote the file
const TOOL_HEADER_PREFIX: &str = "#tool:";

/// the header line naming the python version the file was generated for
const PYTHON_HEADER_PREFIX: &str = "#python:";

/// module-level dunders that are conventionally part of a module's public api
const PUBLIC_MODULE_DUNDERS: &[&str] = &["__all__", "__author__", "__doc__", "__version__"];

/// the python version a lockfile is generated for, which its `#python:` header names: many
/// typing constructs (`Self`, `Required`, `LiteralString`, …) resolve only on newer targets, so
/// the same source has a different lockfile for each
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, get_size2::GetSize)]
pub enum ApiLockTarget {
    /// `#python:default`: the version the project is checked against
    Project,
    /// `#python:3.10`
    Version(PythonVersion),
}

impl ApiLockTarget {
    /// the target the `#python:` header of `lockfile` names, `None` for a lockfile with none
    pub fn of_lockfile(lockfile: &str) -> Option<Result<Self, PythonVersionDeserializationError>> {
        let target = lockfile
            .lines()
            .nth(Self::header_line(lockfile)?)?
            .strip_prefix(PYTHON_HEADER_PREFIX)?;
        Some(match target.trim_end() {
            "default" => Ok(Self::Project),
            version => version.parse().map(Self::Version),
        })
    }

    /// the line, 0-based, of the `#python:` header of `lockfile`, `None` for a lockfile with none
    pub fn header_line(lockfile: &str) -> Option<usize> {
        lockfile
            .lines()
            .take_while(|line| line.starts_with('#'))
            .position(|line| line.starts_with(PYTHON_HEADER_PREFIX))
    }

    /// the program a lockfile for this target is generated under, from the project's own
    fn program<'db>(self, db: &'db dyn Db, project: Program<'db>) -> Program<'db> {
        match self {
            Self::Project => project,
            Self::Version(version) => project.at_python_version(db, version),
        }
    }
}

impl std::fmt::Display for ApiLockTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Project => f.write_str("default"),
            Self::Version(version) => version.fmt(f),
        }
    }
}

/// generate the api lockfile contents for `files`, read as modules of `program` at `target`
///
/// non-first-party files (stdlib, site-packages, vendored) are skipped — only
/// project code contributes to the lockfile.
///
/// `target` is recorded in the header so a downstream reader can see what
/// target the lockfile was generated against
///
/// the lockfile is a function of the set of `files`, not of the order they come in: what is
/// written at a path, and where a class, function or type alias is described, never depends on
/// which module was walked first
pub fn generate_api_lockfile<'db, I>(
    db: &'db dyn Db,
    program: Program<'db>,
    files: I,
    target: ApiLockTarget,
) -> ApiLockfile
where
    I: IntoIterator<Item = File>,
{
    use std::fmt::Write as _;

    let program = target.program(db, program);
    let env = &ProgramEnvironment::from_program(program);
    let modules: Vec<(String, ScopeId<'db>)> = files
        .into_iter()
        .filter_map(|file| {
            let program_file = program.program_file(db, file);
            let resolver_file = program_file.resolver_file(db);
            let module = file_to_module(db, resolver_file)?;
            // a module is read from the one file an import of it resolves to, a stub where there
            // is one beside the source, and the source it shadows is not a module of its own
            let imported = resolve_module(
                db,
                ImportingFile::ResolverFile(resolver_file),
                module.name(db),
            )?;
            if imported.file(db) != Some(file) {
                return None;
            }
            Some((
                module.name(db).as_str().to_string(),
                global_scope(db, program_file),
            ))
        })
        .collect();

    let mut writer = LockfileWriter {
        db,
        env,
        files: modules.iter().map(|(_, scope)| scope.file(db)).collect(),
        described: FxHashSet::default(),
        pending: Vec::new(),
        lines: Vec::new(),
    };
    for (module_name, scope) in &modules {
        writer.module(*scope, module_name);
    }
    writer.describe_pending();
    let mut lines = writer.lines;
    lines.sort();
    lines.dedup();

    let mut after_tool = String::new();
    let _ = writeln!(after_tool, "{PYTHON_HEADER_PREFIX}{target}");
    let _ = writeln!(after_tool, "#modules:{}", modules.len());
    after_tool.push_str(&lines.join("\n"));
    after_tool.push('\n');
    ApiLockfile {
        after_tool: after_tool.into_boxed_str(),
    }
}

/// a generated lockfile, short of the version of `by` its `#tool:` header names
///
/// the version is given when the lockfile is rendered, by the program that writes it, so that
/// what a lockfile records never depends on which version of `by` generated it
#[derive(Debug, Clone, PartialEq, Eq, get_size2::GetSize)]
pub struct ApiLockfile {
    /// every line after the `#tool:` header
    after_tool: Box<str>,
}

impl ApiLockfile {
    /// the text of the lockfile, its `#tool:` header naming `by` at `by_version`
    pub fn render(&self, by_version: &str) -> String {
        format!(
            "{FORMAT_HEADER_PREFIX}{FORMAT_VERSION}\n{TOOL_HEADER_PREFIX}by={by_version}\n{}",
            self.after_tool
        )
    }
}

/// the version of the format `lockfile` is written in, as its first line names it, `None` where
/// its first line names none
pub fn format_version(lockfile: &str) -> Option<&str> {
    lockfile
        .lines()
        .next()?
        .trim_end()
        .strip_prefix(FORMAT_HEADER_PREFIX)
}

/// the first line of `committed`, 0-based, at which it says something other than `generated`,
/// or `None` where the two say the same: the lockfile is up to date
///
/// the `#tool:` header line is not compared. it names the `by` that wrote the file, and two
/// versions of `by` that write the same records describe the same api: were it compared, a
/// lockfile would go stale with every upgrade, and flip between the versions of everyone
/// working on the project, without the api changing. line endings are not compared either,
/// since a checkout can turn them into `\r\n`
///
/// a `committed` that stops short of `generated` differs at the line after its last
pub fn first_stale_line(committed: &str, generated: &str) -> Option<usize> {
    fn compared(text: &str) -> impl Iterator<Item = (usize, &str)> {
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.starts_with(TOOL_HEADER_PREFIX))
    }
    let mut generated_lines = compared(generated).map(|(_, line)| line);
    for (index, line) in compared(committed) {
        if generated_lines.next() != Some(line) {
            return Some(index);
        }
    }
    generated_lines.next().map(|_| committed.lines().count())
}

/// the records of one lockfile as they are written
///
/// a class, function or type alias is described once, at the path of the name its definition
/// binds, and every other path that reaches it is an `r` record naming that path. one reached
/// only away from its own path, being private, local to a function or not the value its name
/// is declared as, is still described at its own path, so every `r` names a record of the
/// file. so what is written at a path depends on the path and the project, never on what was
/// walked before it
struct LockfileWriter<'db, 'env> {
    db: &'db dyn Db,
    env: &'env ProgramEnvironment<'db>,
    /// the files of the modules the lockfile describes: a definition in any other is named,
    /// never described
    files: FxHashSet<File>,
    /// the definitions described so far, each of which is described once
    described: FxHashSet<Definition<'db>>,
    /// the definitions reached away from their own path, to be described there
    pending: Vec<Definition<'db>>,
    lines: Vec<String>,
}

impl<'db> LockfileWriter<'db, '_> {
    fn module(&mut self, scope: ScopeId<'db>, module_name: &str) {
        let all_names = dunder_all_names(self.db, scope.program_file(self.db));
        let symbols = public_symbols(self.db, self.env, scope, |name| {
            is_public_module_symbol(name, all_names)
        });
        for (name, ty, qualifiers) in symbols {
            self.member(&format!("{module_name}.{name}"), ty, qualifiers);
        }
    }

    /// describes every definition reached away from its own path there, and what those reach
    fn describe_pending(&mut self) {
        while let Some(definition) = self.pending.pop() {
            if let Some(path) = definition_path(self.db, definition) {
                self.describe(&path, definition);
            }
        }
    }

    /// the records of a public symbol reached at `qualified`
    fn member(&mut self, qualified: &str, ty: Type<'db>, qualifiers: TypeQualifiers) {
        let db = self.db;
        let site = described_definition(db, ty)
            .and_then(|definition| Some((definition, definition_path(db, definition)?)));
        let Some((definition, path)) = site else {
            match ty {
                // a class a call makes without binding it to a name has no path of its own, so
                // it is described wherever it is reached, or named as python names it
                Type::ClassLiteral(class) if self.files.contains(&class.file(db)) => {
                    self.describe_class(qualified, class);
                }
                Type::ClassLiteral(class) => {
                    self.lines
                        .push(format!("{qualified}:r={}", qualified_class_name(db, class)));
                }
                _ => {
                    // each class, function or type alias a union holds is described at its own
                    // path
                    if let Type::Union(union) = ty {
                        for element in union.elements(db) {
                            if let Some(definition) = described_definition(db, *element) {
                                self.reach(qualified, definition);
                            }
                        }
                    }
                    emit_symbol(db, self.env, qualified, ty, qualifiers, &mut self.lines);
                }
            }
            return;
        };
        if path == qualified && self.files.contains(&definition.file(db)) {
            self.describe(qualified, definition);
            return;
        }
        let arguments = render_specialization(db, self.env, ty);
        self.lines.push(format!("{qualified}:r={path}{arguments}"));
        self.reach(qualified, definition);
    }

    /// `definition`, reached at `qualified`, is described at its own path
    fn reach(&mut self, qualified: &str, definition: Definition<'db>) {
        let db = self.db;
        if !self.files.contains(&definition.file(db)) || self.described.contains(&definition) {
            return;
        }
        match definition_path(db, definition) {
            Some(path) if path == qualified => self.describe(qualified, definition),
            Some(_) => self.pending.push(definition),
            None => {}
        }
    }

    /// the records of `definition` at `path`, the path of the name it binds, once
    fn describe(&mut self, path: &str, definition: Definition<'db>) {
        if !self.described.insert(definition) {
            return;
        }
        let db = self.db;
        match binding_type(db, definition) {
            Type::ClassLiteral(class) => self.describe_class(path, class),
            ty => emit_symbol(
                db,
                self.env,
                path,
                ty,
                TypeQualifiers::empty(),
                &mut self.lines,
            ),
        }
    }

    fn describe_class(&mut self, qualified: &str, class: ClassLiteral<'db>) {
        let db = self.db;
        // class header. bases render as bare class names; the lockfile reads
        // cleaner without surrounding `type[...]`
        let bases = match class {
            ClassLiteral::Static(static_class) => static_class
                .explicit_bases(db)
                .iter()
                .map(|base| render_class_base(db, self.env, *base))
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        };
        let typevars = render_class_typevars(db, class);
        let flags = render_class_flags(db, class);
        self.lines
            .push(format!("{qualified}:c{typevars}[{bases}]{flags}"));

        // walk the class body for own members. stays out of inherited members so a
        // base-class change only diffs the base class line, not every subclass
        if let ClassLiteral::Static(static_class) = class {
            let body_scope = static_class.body_scope(db);
            let name = class.name(db);
            let symbols = public_symbols(db, self.env, body_scope, |member| {
                is_public_class_member(member) && member != name.as_str()
            });
            for (member, ty, qualifiers) in symbols {
                self.member(&format!("{qualified}.{member}"), ty, qualifiers);
            }
            emit_instance_attributes(db, self.env, body_scope, qualified, &mut self.lines);
        }
    }
}

/// the definition of `ty` where it is a class, a specialization of one, a function or a type
/// alias: something the lockfile describes once, at the path of the name that definition
/// binds. `None` for any other type, and for a class a call makes without binding it to a name
fn described_definition<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Definition<'db>> {
    match ty {
        Type::ClassLiteral(class) => class.definition(db),
        Type::GenericAlias(alias) => Some(alias.origin(db).definition(db)),
        Type::FunctionLiteral(function) => Some(function.definition(db)),
        Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) | Type::TypeAlias(alias) => {
            Some(alias.definition(db))
        }
        _ => None,
    }
}

/// the path of the name `definition` binds: its module, then each class and function it is
/// nested in, the way python's `__qualname__` spells them (`m.f.<locals>.C`), then the name.
/// `None` where it binds no name, or where it is made inside a comprehension
fn definition_path<'db>(db: &'db dyn Db, definition: Definition<'db>) -> Option<String> {
    let file = definition.program_file(db);
    let scope = definition.scope(db);
    let name = place_table(db, scope)
        .place(definition.place(db))
        .as_symbol()?
        .name()
        .clone();
    let index = semantic_index(db, file);
    let module = parsed_module(db, file.python_file(db)).load(db);

    // innermost first
    let mut components: Vec<Name> = vec![name];
    for (_, ancestor) in index.ancestor_scopes(scope.file_scope_id(db)) {
        let node = ancestor.node();
        match ancestor.kind() {
            ScopeKind::Module | ScopeKind::TypeParams => {}
            ScopeKind::Class => components.push(node.as_class()?.node(&module).name.id.clone()),
            ScopeKind::Function => {
                components.push(Name::new_static("<locals>"));
                components.push(node.as_function()?.node(&module).name.id.clone());
            }
            ScopeKind::Lambda => {
                components.push(Name::new_static("<locals>"));
                components.push(Name::new_static("<lambda>"));
            }
            ScopeKind::Comprehension | ScopeKind::TypeAlias => return None,
        }
    }

    let mut path = file_to_module(db, file.resolver_file(db))?
        .name(db)
        .as_str()
        .to_string();
    for component in components.iter().rev() {
        path.push('.');
        path.push_str(component);
    }
    Some(path)
}

/// the type and qualifiers the lockfile describes a symbol with: what a module importing it
/// reads, which is what it is declared as at the end of its scope, or else what it is bound to
fn described_type<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    declarations: DeclarationsIterator<'_, 'db>,
    bindings: BindingWithConstraintsIterator<'_, 'db>,
) -> Option<(Type<'db>, TypeQualifiers)> {
    let place_and_qualifiers =
        place_from_declarations(db, env, declarations).ignore_conflicting_declarations();
    let declaration_ty = place_and_qualifiers.place.ignore_possibly_undefined();
    let binding_ty = place_from_bindings(db, env, bindings)
        .place
        .ignore_possibly_undefined();
    Some((
        declaration_ty.or(binding_ty)?,
        place_and_qualifiers.qualifiers,
    ))
}

/// the symbols of `scope` whose names `is_public` admits, with the type and qualifiers the
/// lockfile describes each with
fn public_symbols<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    scope: ScopeId<'db>,
    is_public: impl Fn(&str) -> bool,
) -> Vec<(&'db Name, Type<'db>, TypeQualifiers)> {
    let use_def_map = use_def_map(db, scope);
    let table = place_table(db, scope);
    use_def_map
        .all_reachable_symbols()
        .filter_map(|(symbol_id, declarations, bindings)| {
            let name = table.symbol(symbol_id).name();
            if !is_public(name.as_str()) {
                return None;
            }
            let (ty, qualifiers) = described_type(db, env, declarations, bindings)?;
            Some((name, ty, qualifiers))
        })
        .collect()
}

/// the record of a symbol that is not a class, or of a function or type alias at its own path
fn emit_symbol<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    qualified: &str,
    ty: Type<'db>,
    qualifiers: TypeQualifiers,
    lines: &mut Vec<String>,
) {
    match ty {
        Type::FunctionLiteral(function) => emit_function(db, env, qualified, function, lines),
        Type::KnownInstance(KnownInstanceType::TypeAliasType(alias)) | Type::TypeAlias(alias) => {
            emit_type_alias(db, env, qualified, alias, lines);
        }
        Type::PropertyInstance(property) => emit_property(db, env, qualified, property, lines),
        Type::Union(union)
            if union
                .elements(db)
                .iter()
                .all(|t| matches!(t, Type::PropertyInstance(_))) =>
        {
            let merged = merge_property_union(db, union);
            emit_property(db, env, qualified, merged, lines);
        }
        Type::ModuleLiteral(module_lit) => {
            let target = module_lit.module(db).name(db).as_str();
            lines.push(format!("{qualified}:m={target}"));
        }
        _ => {
            lines.push(format!(
                "{qualified}:v{}={}",
                render_qualifiers(qualifiers),
                render_type(db, env, ty)
            ));
        }
    }
}

/// emit `:i=<type>` lines for instance attributes assigned via `self.x = ...`
/// inside the class's methods. resolves the type from declarations first, then
/// bindings as fallback
fn emit_instance_attributes<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    body_scope: ScopeId<'db>,
    qualified_prefix: &str,
    lines: &mut Vec<String>,
) {
    let index = semantic_index(db, body_scope.program_file(db));

    let mut seen: FxHashSet<String> = FxHashSet::default();
    for function_scope_id in attribute_scopes(db, body_scope) {
        let attr_table = index.place_table(function_scope_id);
        for member in attr_table.members() {
            let Some(name) = member.as_instance_attribute() else {
                continue;
            };
            if !is_public_class_member(name) {
                continue;
            }
            if !seen.insert(name.to_string()) {
                continue;
            }

            let Some((ty, qualifiers)) = lookup_instance_attribute(db, env, body_scope, name)
            else {
                continue;
            };
            let qualified = format!("{qualified_prefix}.{name}");
            lines.push(format!(
                "{qualified}:i{}={}",
                render_qualifiers(qualifiers),
                render_type(db, env, ty)
            ));
        }
    }
}

fn lookup_instance_attribute<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    class_body_scope: ScopeId<'db>,
    name: &str,
) -> Option<(Type<'db>, TypeQualifiers)> {
    let index = semantic_index(db, class_body_scope.program_file(db));

    // try declarations first across all attribute scopes
    for function_scope_id in attribute_scopes(db, class_body_scope) {
        let attr_table = index.place_table(function_scope_id);
        let Some(member) = attr_table.member_id_by_instance_attribute_name(name) else {
            continue;
        };
        let use_def = index.use_def_map(function_scope_id);
        let place_and_qualifiers =
            place_from_declarations(db, env, use_def.reachable_member_declarations(member))
                .ignore_conflicting_declarations();
        if let Some(ty) = place_and_qualifiers.place.ignore_possibly_undefined() {
            return Some((ty, place_and_qualifiers.qualifiers));
        }
    }

    // fall back to bindings (bindings carry no qualifiers)
    for function_scope_id in attribute_scopes(db, class_body_scope) {
        let attr_table = index.place_table(function_scope_id);
        let Some(member) = attr_table.member_id_by_instance_attribute_name(name) else {
            continue;
        };
        let use_def = index.use_def_map(function_scope_id);
        let binding = place_from_bindings(db, env, use_def.reachable_member_bindings(member));
        if let Some(ty) = binding.place.ignore_possibly_undefined() {
            return Some((ty, TypeQualifiers::empty()));
        }
    }

    None
}

fn emit_function<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    qualified: &str,
    function: FunctionType<'db>,
    lines: &mut Vec<String>,
) {
    let decorators = render_function_decorators(db, function);
    for signature in function.signature(db) {
        let typevars = render_signature_typevars(db, signature);
        let body = render_signature_body(db, env, signature);
        lines.push(format!("{qualified}:d{decorators}{typevars}{body}"));
    }
}

fn emit_type_alias<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    qualified: &str,
    alias: TypeAliasType<'db>,
    lines: &mut Vec<String>,
) {
    let typevars = match alias {
        TypeAliasType::PEP695(pep695) => pep695
            .generic_context(db)
            .map(|ctx| render_generic_context(db, ctx))
            .unwrap_or_default(),
        TypeAliasType::ManualPEP695(_) => String::new(),
    };
    lines.push(format!(
        "{qualified}:t{typevars}={}",
        render_type(db, env, alias.value_type(db))
    ));
}

/// merge a union of `PropertyInstance` types into one property by taking the
/// first non-`None` accessor for each slot. ty produces such unions when a
/// property is split across `@x.setter` / `@x.deleter` declarations
fn merge_property_union<'db>(db: &'db dyn Db, union: UnionType<'db>) -> PropertyInstanceType<'db> {
    let mut getter: Option<Type<'db>> = None;
    let mut setter: Option<Type<'db>> = None;
    let mut deleter: Option<Type<'db>> = None;
    for element in union.elements(db) {
        if let Type::PropertyInstance(p) = element {
            if getter.is_none() {
                getter = p.getter(db);
            }
            if setter.is_none() {
                setter = p.setter(db);
            }
            if deleter.is_none() {
                deleter = p.deleter(db);
            }
        }
    }
    PropertyInstanceType::new(db, getter, setter, deleter)
}

fn emit_property<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    qualified: &str,
    property: PropertyInstanceType<'db>,
    lines: &mut Vec<String>,
) {
    let mut accessors: Vec<&'static str> = Vec::new();
    let mut return_ty: Option<Type<'db>> = None;
    if let Some(getter) = property.getter(db) {
        accessors.push("getter");
        if let Type::FunctionLiteral(function) = getter {
            return_ty = function
                .signature(db)
                .iter()
                .next()
                .map(|sig| sig.return_ty);
        }
    }
    if property.setter(db).is_some() {
        accessors.push("setter");
    }
    if property.deleter(db).is_some() {
        accessors.push("deleter");
    }
    let accessors_str = accessors.join(",");
    let type_str = return_ty
        .map(|ty| format!("={}", render_type(db, env, ty)))
        .unwrap_or_default();
    lines.push(format!("{qualified}:p[{accessors_str}]{type_str}"));
}

fn render_function_decorators<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> String {
    let mut tokens: Vec<&'static str> = Vec::new();
    if function.has_known_decorator(db, FunctionDecorators::ABSTRACT_METHOD) {
        tokens.push("abstractmethod");
    }
    if is_async_function(db, function) {
        tokens.push("async");
    }
    if function.is_classmethod(db) {
        tokens.push("classmethod");
    }
    if function.implementation_deprecated(db).is_some() {
        tokens.push("deprecated");
    }
    if function.has_known_decorator(db, FunctionDecorators::FINAL) {
        tokens.push("final");
    }
    if function.has_known_decorator(db, FunctionDecorators::NO_TYPE_CHECK) {
        tokens.push("no_type_check");
    }
    if function.has_known_decorator(db, FunctionDecorators::OVERLOAD) {
        tokens.push("overload");
    }
    if function.has_known_decorator(db, FunctionDecorators::OVERRIDE) {
        tokens.push("override");
    }
    if function.is_staticmethod(db) {
        tokens.push("staticmethod");
    }
    if function.has_known_decorator(db, FunctionDecorators::TYPE_CHECK_ONLY) {
        tokens.push("type_check_only");
    }
    if tokens.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", tokens.join(","))
    }
}

/// detect async functions by inspecting whether the signature's return type is
/// `CoroutineType[...]` — `FunctionType::signature` wraps async return types in
/// this form
fn is_async_function<'db>(db: &'db dyn Db, function: FunctionType<'db>) -> bool {
    function
        .signature(db)
        .iter()
        .any(|sig| match sig.return_ty {
            Type::NominalInstance(instance) => {
                instance.has_known_class(db, KnownClass::CoroutineType)
            }
            _ => false,
        })
}

fn render_signature_typevars<'db>(db: &'db dyn Db, signature: &Signature<'db>) -> String {
    signature
        .generic_context
        .map(|ctx| render_generic_context(db, ctx))
        .unwrap_or_default()
}

fn render_generic_context<'db>(
    db: &'db dyn Db,
    context: crate::types::GenericContext<'db>,
) -> String {
    let mut out = String::from("<");
    let mut first = true;
    for bound_typevar in context.variables(db) {
        // `Self` is an implicit method-bound typevar injected by ty for instance
        // methods. it is not part of the user-visible api surface and would
        // otherwise pollute every method line with a `<Self>` prefix
        if bound_typevar.name(db).as_str() == "Self" {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        let sigil = match bound_typevar.variance(db) {
            TypeVarVariance::Covariant => "+",
            TypeVarVariance::Contravariant => "-",
            TypeVarVariance::Invariant => "",
            TypeVarVariance::Bivariant => "*",
        };
        write!(out, "{sigil}{}", bound_typevar.name(db).as_str()).unwrap();
    }
    if first {
        return String::new();
    }
    out.push('>');
    out
}

fn render_generic_args<'db>(
    db: &'db dyn Db,
    context: Option<crate::types::GenericContext<'db>>,
) -> String {
    let Some(context) = context else {
        return String::new();
    };
    let names: Vec<String> = context
        .variables(db)
        .map(|tv| tv.name(db).as_str().to_string())
        .collect();
    if names.is_empty() {
        String::new()
    } else {
        format!("[{}]", names.join(","))
    }
}

fn render_signature_body<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    signature: &Signature<'db>,
) -> String {
    let parameters = signature.parameters();
    // ignore `self`/`cls`, which ty synthesises as positional-only — leaking
    // the `/` marker into the lockfile for every method is noise without
    // signal
    let is_real_pos_only = |p: &Parameter<'_>| match p.kind() {
        ParameterKind::PositionalOnly { name, .. } => match name {
            Some(n) => {
                let s = n.as_str();
                s != "self" && s != "cls"
            }
            None => true,
        },
        _ => false,
    };
    let any_positional_only = parameters.iter().any(is_real_pos_only);
    let any_explicit_variadic = parameters
        .iter()
        .any(|p| matches!(p.kind(), ParameterKind::Variadic { .. }));
    let any_keyword_only = parameters
        .iter()
        .any(|p| matches!(p.kind(), ParameterKind::KeywordOnly { .. }));

    let mut out = String::from("(");
    let mut tokens: Vec<String> = Vec::new();
    let mut emitted_pos_only_boundary = !any_positional_only;
    let mut emitted_kw_only_boundary = !any_keyword_only || any_explicit_variadic;
    let mut anon_index: usize = 0;

    for parameter in parameters {
        match parameter.kind() {
            ParameterKind::PositionalOnly { .. } if is_real_pos_only(parameter) => {}
            ParameterKind::PositionalOrKeyword { .. }
            | ParameterKind::Variadic { .. }
            | ParameterKind::KeywordOnly { .. }
            | ParameterKind::KeywordVariadic { .. }
                if !emitted_pos_only_boundary =>
            {
                tokens.push("/".to_string());
                emitted_pos_only_boundary = true;
            }
            _ => {}
        }

        if matches!(parameter.kind(), ParameterKind::KeywordOnly { .. })
            && !emitted_kw_only_boundary
        {
            tokens.push("*".to_string());
            emitted_kw_only_boundary = true;
        }

        let annotation = render_type(db, env, parameter.annotated_type());
        let token = match parameter.kind() {
            ParameterKind::PositionalOnly { name, default_type } => {
                let label = match name.as_ref() {
                    Some(n) => n.as_str().to_string(),
                    None => {
                        let label = format!("_{anon_index}");
                        anon_index += 1;
                        label
                    }
                };
                let mut s = format!("{label}:{annotation}");
                if default_type.is_some() {
                    s.push('=');
                }
                s
            }
            ParameterKind::PositionalOrKeyword { name, default_type }
            | ParameterKind::KeywordOnly { name, default_type } => {
                let mut s = format!("{name}:{annotation}");
                if default_type.is_some() {
                    s.push('=');
                }
                s
            }
            ParameterKind::Variadic { name } => {
                emitted_kw_only_boundary = true;
                format!("*{name}:{annotation}")
            }
            ParameterKind::KeywordVariadic { name } => {
                format!("**{name}:{annotation}")
            }
        };
        tokens.push(token);
    }

    out.push_str(&tokens.join(","));
    out.push(')');
    write!(out, "->{}", render_type(db, env, signature.return_ty)).unwrap();
    out
}

/// render a type to its lockfile form. nominal classes defined in user code
/// are prefixed with the defining module so the lockfile remains
/// disambiguated even if two modules export classes with the same short name.
/// unions and intersections are recursively walked so each member keeps its
/// module prefix
fn render_type<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> String {
    if ty.is_none(db) {
        return "None".to_string();
    }
    // surface ty's `Unknown` as an obvious sentinel so a downstream diff
    // can distinguish a real api change from a checker regression
    if matches!(ty, Type::Dynamic(crate::types::DynamicType::Unknown)) {
        return "<unresolved>".to_string();
    }
    match ty {
        Type::NominalInstance(instance) => {
            let class_name = instance.class_name(db, env).as_str().to_owned();
            // anonymous named-tuple types: render structurally so two
            // identically-shaped anon NTs share a lockfile representation
            // and the synthesized hash-suffixed class name doesn't leak
            if class_name.starts_with("_AnonNamedTuple_") {
                if let Some(structural) = render_anon_named_tuple(db, env, instance) {
                    return structural;
                }
            }
            let module = instance
                .class_module_name(db, env)
                .map(|m| format!("{}.", m.as_str()))
                .unwrap_or_default();
            let base = format!("{module}{class_name}");
            // surface generic args so `list[int]` and `list[str]` don't both
            // collapse to `builtins.list` in the lockfile
            if let Some(alias) = instance.class(db, env).into_generic_alias() {
                let args: Vec<String> = alias
                    .specialization(db)
                    .types(db)
                    .iter()
                    .map(|arg| render_type(db, env, *arg))
                    .collect();
                if !args.is_empty() {
                    return format!("{base}[{}]", args.join(","));
                }
            }
            base
        }
        Type::ClassLiteral(class) => format!("type[{}]", qualified_class_name(db, class)),
        Type::GenericAlias(alias) => {
            format!(
                "{}{}",
                qualified_class_name(db, ClassLiteral::Static(alias.origin(db))),
                render_specialization(db, env, ty)
            )
        }
        Type::Union(union) => {
            // group consecutive Literal members into a single `Literal[a, b, c]`
            // so unions of literal values stay compact in the lockfile
            let mut literal_parts: Vec<String> = Vec::new();
            let mut other_parts: Vec<String> = Vec::new();
            let mut has_none = false;
            for t in union.elements(db) {
                if t.is_none(db) {
                    has_none = true;
                } else if t.as_literal_value_kind().is_some() {
                    // peel the `Literal[...]` wrapper if the display added it
                    let rendered = render_type(db, env, *t);
                    let inner = rendered
                        .strip_prefix("Literal[")
                        .and_then(|s| s.strip_suffix(']'))
                        .map(ToOwned::to_owned)
                        .unwrap_or(rendered);
                    literal_parts.push(inner);
                } else {
                    other_parts.push(render_type(db, env, *t));
                }
            }
            literal_parts.sort();
            other_parts.sort();
            let mut elements: Vec<String> = Vec::new();
            if !literal_parts.is_empty() {
                elements.push(format!("Literal[{}]", literal_parts.join(", ")));
            }
            elements.extend(other_parts);
            // None is canonically last for readability of `T | None` style
            if has_none {
                elements.push("None".to_owned());
            }
            elements.join(" | ")
        }
        Type::TypeVar(bound) => bound.name(db).as_str().to_string(),
        Type::KnownInstance(KnownInstanceType::SubscriptedGeneric(ctx)) => {
            format!("typing.Generic{}", render_generic_args(db, ctx))
        }
        Type::KnownInstance(KnownInstanceType::SubscriptedProtocol(ctx)) => {
            format!("typing.Protocol{}", render_generic_args(db, ctx))
        }
        Type::SpecialForm(form) => form.to_string(),
        Type::Intersection(intersection) => {
            let mut positives: Vec<String> = intersection
                .iter_positive(db)
                .map(|t| render_type(db, env, t))
                .collect();
            let mut negatives: Vec<String> = intersection
                .iter_negative(db)
                .map(|t| render_type(db, env, t))
                .collect();
            positives.sort();
            negatives.sort();
            let mut parts: Vec<String> = Vec::new();
            parts.extend(positives);
            parts.extend(negatives.into_iter().map(|n| format!("~{n}")));
            parts.join(" & ")
        }
        _ => ty.display(db, env).to_string(),
    }
}

/// Render the structural shape of an anonymous named-tuple instance as
/// `(name: T, name: T, ...)`. Walks the class's body-scope declarations
/// in source order to recover field names. Falls back to a `tuple[T, …]`
/// shape if no named fields are visible (synthesised anon NTs from `.by`
/// source may not expose body declarations to ty).
fn render_anon_named_tuple<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    instance: NominalInstanceType<'db>,
) -> Option<String> {
    let class_literal = instance.class(db, env).class_literal(db);
    if let ClassLiteral::Static(static_class) = class_literal {
        let body_scope = static_class.body_scope(db);
        let table = place_table(db, body_scope);
        let use_def = use_def_map(db, body_scope);
        let mut fields: Vec<(String, String)> = Vec::new();
        for (symbol_id, declarations, _bindings) in use_def.all_reachable_symbols() {
            let symbol = table.symbol(symbol_id);
            let name = symbol.name().as_str().to_owned();
            if name.starts_with('_') {
                continue;
            }
            let place_and_qualifiers =
                place_from_declarations(db, env, declarations).ignore_conflicting_declarations();
            let Some(decl_ty) = place_and_qualifiers.place.ignore_possibly_undefined() else {
                continue;
            };
            fields.push((name, render_type(db, env, decl_ty)));
        }
        if !fields.is_empty() {
            let body = fields
                .iter()
                .map(|(n, t)| format!("{n}: {t}"))
                .collect::<Vec<_>>()
                .join(", ");
            return Some(format!("({body})"));
        }
    }
    // fall back to tuple-spec elements when body declarations aren't
    // available
    let spec = instance.tuple_spec(db, env)?;
    let elements: Vec<&Type<'db>> = spec.fixed_elements().collect();
    if elements.is_empty() {
        return None;
    }
    let body = elements
        .iter()
        .map(|t| render_type(db, env, **t))
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("({body})"))
}

/// the arguments a specialization of a class is written with, `[<args>]`, and nothing for any
/// other type
fn render_specialization<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    ty: Type<'db>,
) -> String {
    let Type::GenericAlias(alias) = ty else {
        return String::new();
    };
    let args: Vec<String> = alias
        .specialization(db)
        .types(db)
        .iter()
        .map(|arg| render_type(db, env, *arg))
        .collect();
    if args.is_empty() {
        String::new()
    } else {
        format!("[{}]", args.join(","))
    }
}

/// render the generic typevar list for a class, including variance.
/// returns empty string for non-generic classes
fn render_class_typevars<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let Some(generic_context) = class.generic_context(db) else {
        return String::new();
    };
    render_generic_context(db, generic_context)
}

fn render_class_flags<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let mut flags: Vec<&'static str> = Vec::new();
    if is_dataclass(db, class) {
        flags.push("dataclass");
    }
    if matches!(class, ClassLiteral::DynamicEnum(_)) || is_enum_class(db, Type::ClassLiteral(class))
    {
        flags.push("enum");
    }
    if class.is_final(db) {
        flags.push("final");
    }
    if is_named_tuple(db, class) {
        flags.push("named_tuple");
    }
    if class.is_protocol(db) {
        flags.push("protocol");
    }
    if class.is_typed_dict(db) {
        flags.push("typed_dict");
    }
    if flags.is_empty() {
        String::new()
    } else {
        format!("{{{}}}", flags.join(","))
    }
}

fn is_dataclass<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    match class {
        ClassLiteral::Static(static_class) => static_class.is_dataclass_like(db),
        ClassLiteral::Dynamic(_)
        | ClassLiteral::DynamicNamedTuple(_)
        | ClassLiteral::DynamicTypedDict(_)
        | ClassLiteral::DynamicEnum(_) => false,
    }
}

fn is_named_tuple<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    match class {
        ClassLiteral::DynamicNamedTuple(_) => true,
        ClassLiteral::Static(static_class) => static_class
            .explicit_bases(db)
            .iter()
            .any(|base| matches!(base, Type::SpecialForm(SpecialFormType::NamedTuple))),
        _ => false,
    }
}

fn render_class_base<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> String {
    match ty {
        Type::ClassLiteral(class) => qualified_class_name(db, class),
        Type::GenericAlias(alias) => {
            format!(
                "{}{}",
                qualified_class_name(db, ClassLiteral::Static(alias.origin(db))),
                render_specialization(db, env, ty)
            )
        }
        _ => render_type(db, env, ty),
    }
}

fn qualified_class_name<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> String {
    let class_name = class.name(db).as_str();
    if let Some(module) = file_to_module(db, class.program_file(db).resolver_file(db)) {
        format!("{}.{}", module.name(db).as_str(), class_name)
    } else {
        class_name.to_string()
    }
}

fn render_qualifiers(qualifiers: TypeQualifiers) -> String {
    if qualifiers.is_empty() {
        return String::new();
    }
    let mut tokens: Vec<&'static str> = Vec::new();
    if qualifiers.contains(TypeQualifiers::CLASS_VAR) {
        tokens.push("classvar");
    }
    if qualifiers.contains(TypeQualifiers::FINAL) {
        tokens.push("final");
    }
    if qualifiers.contains(TypeQualifiers::INIT_VAR) {
        tokens.push("initvar");
    }
    if qualifiers.contains(TypeQualifiers::NOT_REQUIRED) {
        tokens.push("notrequired");
    }
    if qualifiers.contains(TypeQualifiers::READ_ONLY) {
        tokens.push("readonly");
    }
    if qualifiers.contains(TypeQualifiers::REQUIRED) {
        tokens.push("required");
    }
    if tokens.is_empty() {
        String::new()
    } else {
        format!("[{}]", tokens.join(","))
    }
}

/// module-level symbols filter:
/// - if `__all__` is set, only names in it are public
/// - otherwise, names without leading underscore are public, plus an
///   allowlist of public-by-convention dunders (`__version__`, etc.)
fn is_public_module_symbol(name: &str, all_names: Option<&FxHashSet<Name>>) -> bool {
    if let Some(names) = all_names {
        return names.iter().any(|n| n.as_str() == name);
    }
    if PUBLIC_MODULE_DUNDERS.contains(&name) {
        return true;
    }
    !name.starts_with('_')
}

/// class members are public unless name-mangled (single underscore) or
/// strictly private (`__name` without trailing dunder). standard dunders like
/// `__init__` are part of the public api surface and stay
fn is_public_class_member(name: &str) -> bool {
    if name.starts_with("__") && name.ends_with("__") && name.len() >= 4 {
        return true;
    }
    !name.starts_with('_')
}

#[cfg(test)]
mod tests {
    use ruff_db::files::system_path_to_file;
    use ruff_python_ast::PythonVersion;

    use super::{
        ApiLockTarget, TOOL_HEADER_PREFIX, first_stale_line, format_version, generate_api_lockfile,
    };
    use crate::db::tests::TestDbBuilder;
    use ty_python_core::TestProgramDb as _;

    /// the records of the lockfile of `files`, each a path under the project's root and its
    /// source, walked in every order they can be walked in, which must all agree
    fn records_in_every_walk_order(files: &[(&str, &str)]) -> anyhow::Result<String> {
        let mut builder = TestDbBuilder::new();
        let paths: Vec<String> = files
            .iter()
            .map(|(name, _)| format!("/src/{name}"))
            .collect();
        for (path, (_, source)) in paths.iter().zip(files) {
            builder = builder.with_file(path, source);
        }
        let db = builder.build()?;
        let files = paths
            .iter()
            .map(|path| system_path_to_file(&db, path))
            .collect::<Result<Vec<_>, _>>()?;

        let lockfiles: Vec<String> = permutations(&files)
            .into_iter()
            .map(|order| {
                generate_api_lockfile(&db, db.program(), order, ApiLockTarget::Project)
                    .render("0.0.0")
            })
            .collect();
        for lockfile in &lockfiles[1..] {
            assert_eq!(lockfile, &lockfiles[0]);
        }
        Ok(without_tool_header(&lockfiles[0]))
    }

    /// `lockfile` without its `#tool:` header, which changes with every version of `by`
    fn without_tool_header(lockfile: &str) -> String {
        lockfile
            .lines()
            .filter(|line| !line.starts_with(TOOL_HEADER_PREFIX))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// every order `items` can be put in
    fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
        if items.is_empty() {
            return vec![Vec::new()];
        }
        let mut orders = Vec::new();
        for (index, first) in items.iter().enumerate() {
            let mut rest = items.to_vec();
            rest.remove(index);
            for mut order in permutations(&rest) {
                order.insert(0, first.clone());
                orders.push(order);
            }
        }
        orders
    }

    /// a class is described where it is defined, whichever module is walked first, and named
    /// from every other place it is reached
    #[test]
    fn a_class_is_described_in_the_module_that_defines_it() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[
            ("a.py", "import b\n\nclass A:\n    X = b.Foo\n"),
            ("b.py", "class Foo:\n    def m(self) -> int: ...\n"),
            (
                "c.py",
                "class Outer:\n    class Inner: ...\n\nInner = Outer.Inner\n",
            ),
        ])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:3
        a.A.X:r=b.Foo
        a.A:c[]
        a.b:m=b
        b.Foo.m:d(self:b.Foo)->builtins.int
        b.Foo:c[]
        c.Inner:r=c.Outer.Inner
        c.Outer.Inner:c[]
        c.Outer:c[]
        ");
        Ok(())
    }

    /// a class bound to a second public name, or specialized, is named there, as is one reached
    /// from inside itself
    #[test]
    fn a_class_reached_again_is_named() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[(
            "m.py",
            "\
from typing import NamedTuple

def node():
    return Node

class Node:
    me = node()

class Outer:
    class Box[T]: ...

Alias = Node
IntBox = Outer.Box[int]
Point = NamedTuple(\"Point\", [(\"x\", int)])
SamePoint = Point
",
        )])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:1
        m.Alias:r=m.Node
        m.IntBox:r=m.Outer.Box[builtins.int]
        m.NamedTuple:v=typing.NamedTuple
        m.Node.me:r=m.Node
        m.Node:c[]
        m.Outer.Box:c<+T>[]
        m.Outer:c[]
        m.Point:c[]{named_tuple}
        m.SamePoint:r=m.Point
        m.node:d()->type[m.Node]
        ");
        Ok(())
    }

    /// a class the public api reaches only under another name, being private or redefined, is
    /// still described at its own path, once, and named from every path that reaches it
    #[test]
    fn a_private_or_redefined_class_is_described_at_its_own_path() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[
            (
                "m.py",
                "\
def node():
    return _Node

class _Node:
    me = node()

class Redefined:
    x: int

First = Redefined

class Redefined:
    y: str

Public = _Node
Other = _Node
",
            ),
            (
                "p.py",
                "__all__ = [\"Exported\"]\n\nclass Hidden: ...\n\nExported = Hidden\n",
            ),
        ])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:2
        m.First:r=m.Redefined
        m.Other:r=m._Node
        m.Public:r=m._Node
        m.Redefined.x:v=builtins.int
        m.Redefined.y:v=builtins.str
        m.Redefined:c[]
        m.Redefined:v=type[m.Redefined] | type[m.Redefined]
        m._Node.me:r=m._Node
        m._Node:c[]
        m.node:d()->type[m._Node]
        p.Exported:r=p.Hidden
        p.Hidden:c[]
        ");
        Ok(())
    }

    /// a class from outside the project is named wherever the project reaches it, in a class as
    /// in a module
    #[test]
    fn a_class_from_outside_the_project_is_named() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[(
            "m.py",
            "Text = str\n\nclass Holder:\n    kind = str\n    mapping = dict[str, int]\n",
        )])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:1
        m.Holder.kind:r=builtins.str
        m.Holder.mapping:r=builtins.dict[builtins.str,builtins.int]
        m.Holder:c[]
        m.Text:r=builtins.str
        ");
        Ok(())
    }

    /// a function or type alias is described at the path of the name its definition binds, in
    /// a class as in a module, and named everywhere else, public name or not
    #[test]
    fn a_function_or_type_alias_is_described_at_its_own_path() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[
            (
                "a.py",
                "\
def _impl(x: int) -> str: ...

class C:
    def meth(self) -> int: ...
    alias = meth

type _Alias = int
",
            ),
            (
                "b.py",
                "\
from a import _impl as pub, C, _Alias

meth = C.meth
Al = _Alias

class H:
    f = pub
",
            ),
        ])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:2
        a.C.alias:r=a.C.meth
        a.C.meth:d(self:a.C)->builtins.int
        a.C:c[]
        a._Alias:t=builtins.int
        a._impl:d(x:builtins.int)->builtins.str
        b.Al:r=a._Alias
        b.C:r=a.C
        b.H.f:r=a._impl
        b.H:c[]
        b.meth:r=a.C.meth
        b.pub:r=a._impl
        ");
        Ok(())
    }

    /// a class a call makes is described at the name it is bound to, not the name it is given
    #[test]
    fn a_functional_class_is_described_at_the_name_it_is_bound_to() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[
            (
                "m.py",
                "\
from typing import NamedTuple

Q = NamedTuple(\"P\", [(\"x\", int)])

class Outer:
    Inner = NamedTuple(\"Inner\", [(\"y\", str)])
",
            ),
            (
                "n.py",
                "from m import Q, Outer\n\nclass H:\n    q = Q\n    i = Outer.Inner\n",
            ),
        ])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:2
        m.NamedTuple:v=typing.NamedTuple
        m.Outer.Inner:c[]{named_tuple}
        m.Outer:c[]
        m.Q:c[]{named_tuple}
        n.H.i:r=m.Outer.Inner
        n.H.q:r=m.Q
        n.H:c[]
        n.Outer:r=m.Outer
        n.Q:r=m.Q
        ");
        Ok(())
    }

    /// a module with a stub beside its source is read from the stub, as an import of it is, and
    /// is one module
    #[test]
    fn a_module_with_a_stub_is_read_from_the_stub() -> anyhow::Result<()> {
        let records = records_in_every_walk_order(&[
            ("st.py", "class P:\n    a: int\n"),
            ("st.pyi", "class P:\n    b: str\n\nQ = P\n"),
        ])?;
        insta::assert_snapshot!(records, @"
        #api-lock:v=1
        #python:default
        #modules:1
        st.P.b:v=builtins.str
        st.P:c[]
        st.Q:r=st.P
        ");
        Ok(())
    }

    /// a class from a dependency is named by the path of the name its definition binds there
    #[test]
    fn a_class_from_a_dependency_is_named_by_its_own_path() -> anyhow::Result<()> {
        let db = TestDbBuilder::new()
            .with_site_packages("/site-packages")
            .with_file(
                "/site-packages/dep/__init__.py",
                "class Outer:\n    class Inner: ...\n",
            )
            .with_file(
                "/src/m.py",
                "import dep\n\nclass H:\n    i = dep.Outer.Inner\n\nX = dep.Outer.Inner\n",
            )
            .build()?;
        let file = system_path_to_file(&db, "/src/m.py")?;
        let lockfile = generate_api_lockfile(&db, db.program(), [file], ApiLockTarget::Project)
            .render("0.0.0");
        insta::assert_snapshot!(without_tool_header(&lockfile), @"
        #api-lock:v=1
        #python:default
        #modules:1
        m.H.i:r=dep.Outer.Inner
        m.H:c[]
        m.X:r=dep.Outer.Inner
        m.dep:m=dep
        ");
        Ok(())
    }

    const LOCK: &str = "#api-lock:v=1\n#tool:by=0.0.5\n#python:3.10\n#modules:1\nm.X:v=int\n";

    #[test]
    fn the_target_is_what_the_python_header_names() {
        assert_eq!(
            ApiLockTarget::of_lockfile(LOCK),
            Some(Ok(ApiLockTarget::Version(PythonVersion::PY310)))
        );
        assert_eq!(
            ApiLockTarget::of_lockfile("#api-lock:v=1\n#python:default\n"),
            Some(Ok(ApiLockTarget::Project))
        );
        assert!(matches!(
            ApiLockTarget::of_lockfile("#python:three\n"),
            Some(Err(_))
        ));
        // a record is not a header, whatever it holds
        assert_eq!(
            ApiLockTarget::of_lockfile("m.X:v=int\n#python:3.10\n"),
            None
        );
    }

    #[test]
    fn the_format_is_what_the_first_line_names() {
        assert_eq!(format_version("#api-lock:v=1\r\n#python:3.10\n"), Some("1"));
        assert_eq!(format_version("#python:3.10\n#api-lock:v=1\n"), None);
        assert_eq!(format_version(""), None);
    }

    #[test]
    fn only_the_tool_that_wrote_a_lockfile_does_not_make_it_stale() {
        let other_tool = LOCK.replace("by=0.0.5", "by=0.0.6");
        assert_eq!(first_stale_line(LOCK, &other_tool), None);
        assert_eq!(first_stale_line(&LOCK.replace('\n', "\r\n"), LOCK), None);
    }

    #[test]
    fn a_lockfile_is_stale_from_the_first_line_that_differs() {
        assert_eq!(
            first_stale_line(LOCK, &LOCK.replace("m.X:v=int", "m.X:v=str")),
            Some(4)
        );
        assert_eq!(
            first_stale_line(LOCK, &LOCK.replace("3.10", "3.11")),
            Some(2)
        );
        // a record the lockfile does not have yet is missing after its last line
        assert_eq!(
            first_stale_line(LOCK, &format!("{LOCK}m.Y:v=int\n")),
            Some(5)
        );
        // and one it has that is gone is stale where it is
        assert_eq!(
            first_stale_line(&format!("{LOCK}m.Y:v=int\n"), LOCK),
            Some(5)
        );
    }
}
