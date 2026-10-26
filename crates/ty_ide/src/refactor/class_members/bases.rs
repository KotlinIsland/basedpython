//! Pulling a class's bases up into a superclass, as python's own pull members up offers a mixin:
//!
//! ```py
//! class Shape: ...                         class Shape(Mixin): ...
//! class Square(Shape, Mixin): ...   ──►    class Square(Shape): ...
//! ```
//!
//! Every class below the superclass then inherits the base, so the pull is refused unless every
//! class whose bases change — the superclass and every class of the project below it — finds each
//! name it found before in the class it found it in. A name only the base brings is gained, which
//! is what the pull is for. Python has to be able to order each class's bases afterwards, and the
//! superclass has to be able to name the base where it is defined.

use ruff_db::files::File;
use ruff_db::parsed::parsed_module;
use ruff_diagnostics::Edit;
use ruff_python_ast::name::Name;
use ruff_python_ast::visitor::source_order::{self, SourceOrderVisitor};
use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange, TextSize};
use rustc_hash::{FxHashMap, FxHashSet};
use ty_project::Db;
use ty_python_core::ProgramFile;
use ty_python_semantic::types::Type;
use ty_python_semantic::{
    ProgramEnvironment, class_bases, instance_member_names, type_hierarchy_mro,
    type_hierarchy_prepare,
};

use super::{ClassSite, all_classes};
use crate::code_action::FileEdit;

/// A base a class statement writes, by name, which a pull up can move.
pub(super) struct WrittenBase<'a> {
    /// The base as written, which is how a client names it.
    pub(super) text: &'a str,
    pub(super) expression: &'a Expr,
}

/// The bases `def` writes as a name or a dotted name, the kind a superclass can write too.
pub(super) fn written_bases<'a>(
    def: &'a ast::StmtClassDef,
    source: &'a str,
) -> Vec<WrittenBase<'a>> {
    def.bases()
        .iter()
        .filter(|base| root_name(base).is_some())
        .map(|expression| WrittenBase {
            text: &source[expression.range()],
            expression,
        })
        .filter(|base| base.text != "object")
        .collect()
}

/// The name a base's name or dotted name starts with.
fn root_name(expression: &Expr) -> Option<&Name> {
    match expression {
        Expr::Name(name) => Some(&name.id),
        Expr::Attribute(attribute) => root_name(&attribute.value),
        _ => None,
    }
}

/// What pulling bases up does: the edits, and the names the superclass gains with them.
pub(super) struct PulledBases {
    pub(super) edits: Vec<FileEdit>,
    pub(super) gained: FxHashSet<Name>,
}

/// A class, as the file and name range its statement has.
type ClassKey = (File, TextRange);

/// Pulls the bases `pulled`, as `class` writes them, up into `superclass`, after its own bases.
pub(super) fn pull_bases_up<'db>(
    db: &'db dyn Db,
    class: &ClassSite<'db>,
    superclass: &ClassSite<'db>,
    pulled: &[&str],
    source: &str,
) -> Result<PulledBases, String> {
    let def = class
        .def()
        .ok_or_else(|| "there is no class here".to_string())?;
    let super_def = superclass
        .def()
        .ok_or_else(|| format!("{} cannot be read", superclass.name))?;
    let env = ProgramEnvironment::from_file(class.file);
    let key_of = |ty: Type<'db>| {
        type_hierarchy_prepare(db, &env, ty)
            .map(|class| (class.file.file(db), class.selection_range))
    };
    let written = written_bases(def, source);
    let resolved = class_bases(db, &env, class.ty);
    if resolved.len() != def.bases().len() {
        return Err(format!(
            "a base of {} is not a class that can be told apart",
            class.name
        ));
    }

    // the bases pulled, each as written and as the class it is
    let mut bases: Vec<(&WrittenBase, Type<'db>, ClassKey)> = Vec::new();
    for name in pulled {
        let base = written
            .iter()
            .find(|base| base.text == *name)
            .ok_or_else(|| format!("{} has no base `{name}`", class.name))?;
        let index = def
            .bases()
            .iter()
            .position(|expression| expression.range() == base.expression.range())
            .ok_or_else(|| format!("{} has no base `{name}`", class.name))?;
        let ty = resolved[index];
        let key = key_of(ty).ok_or_else(|| format!("`{name}` is not a class"))?;
        bases.push((base, ty, key));
    }

    let super_key = (superclass.file_id, superclass.name_range);
    for (base, ty, key) in &bases {
        // the superclass is reached through the base, which the class would lose
        let below_base = mro_keys(db, &env, *ty);
        if *key == super_key || below_base.contains(&super_key) {
            return Err(format!(
                "{} inherits {} through `{}`, so it cannot lose it",
                class.name, superclass.name, base.text
            ));
        }
        if mro_keys(db, &env, superclass.ty).contains(key) {
            return Err(format!(
                "{} inherits `{}` already",
                superclass.name, base.text
            ));
        }
    }

    // the superclass names the base by the name the class does, so the name has to mean the
    // same where the superclass is defined: both at the top of one module, with the name bound
    // above the superclass and nowhere else
    if superclass.file_id != class.file_id {
        return Err(format!(
            "{} is in another module, which would have to import what it inherits",
            superclass.name
        ));
    }
    let suite = class.parsed.suite();
    let top_level = |def: &ast::StmtClassDef| {
        suite
            .iter()
            .position(|statement| statement.range() == def.range())
    };
    let (Some(super_index), Some(_)) = (top_level(super_def), top_level(def)) else {
        return Err(format!(
            "{} or {} is not defined at the top of its module",
            superclass.name, class.name
        ));
    };
    for (base, _, _) in &bases {
        let Some(root) = root_name(base.expression) else {
            continue;
        };
        let bindings: Vec<usize> = suite
            .iter()
            .enumerate()
            .filter(|(_, statement)| binds_at_top(statement, root))
            .map(|(index, _)| index)
            .collect();
        if bindings.is_empty() || bindings.iter().any(|&index| index >= super_index) {
            return Err(format!(
                "`{root}` is not bound above {}, where it would be written",
                superclass.name
            ));
        }
    }

    // every class whose bases change orders them as python would, and finds what it found
    let descendants = superclass.descendants(db);
    let affected: Vec<&ClassSite<'db>> = std::iter::once(superclass)
        .chain(descendants.iter())
        .collect();
    let pulled_keys: Vec<ClassKey> = bases.iter().map(|(_, _, key)| *key).collect();
    let class_key = (class.file_id, class.name_range);
    let mut new_bases: FxHashMap<ClassKey, Vec<ClassKey>> = FxHashMap::default();
    // the class each key is, for the classes above whose bases stay as they are
    let mut types: FxHashMap<ClassKey, Type<'db>> =
        bases.iter().map(|(_, ty, key)| (*key, *ty)).collect();
    for site in &affected {
        let Some(site_def) = site.def() else {
            return Err(format!("{} cannot be read", site.name));
        };
        let site_env = ProgramEnvironment::from_file(site.file);
        let site_bases = class_bases(db, &site_env, site.ty);
        if site_bases.len() != site_def.bases().len() {
            return Err(format!(
                "a base of {} is not a class that can be told apart",
                site.name
            ));
        }
        let mut keys: Vec<ClassKey> = Vec::new();
        for base in site_bases {
            let key = key_of(base).ok_or_else(|| {
                format!(
                    "a base of {} is not a class that can be told apart",
                    site.name
                )
            })?;
            types.insert(key, base);
            keys.push(key);
        }
        let key = (site.file_id, site.name_range);
        if key == super_key {
            keys.extend(pulled_keys.iter().copied());
        }
        if key == class_key {
            keys.retain(|base| !pulled_keys.contains(base));
        }
        new_bases.insert(key, keys);
    }
    let mut linearized: FxHashMap<ClassKey, Option<Vec<ClassKey>>> = FxHashMap::default();
    let mut names = OwnNames {
        db,
        program: class.file,
        cache: FxHashMap::default(),
    };
    for site in &affected {
        let key = (site.file_id, site.name_range);
        let before: Vec<ClassKey> = std::iter::once(key)
            .chain(mro_keys(db, &env, site.ty))
            .collect();
        let after =
            linearize(db, &env, key, &new_bases, &types, &mut linearized).ok_or_else(|| {
                format!(
                    "python could not order the bases of {} once it inherits {}",
                    site.name,
                    bases
                        .iter()
                        .map(|(base, _, _)| format!("`{}`", base.text))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
        let found_before = names.found_through(&before);
        let found_after = names.found_through(&after);
        let mut changed: Vec<&Name> = found_before
            .iter()
            .filter(|(name, owner)| found_after.get(*name) != Some(*owner))
            .map(|(name, _)| name)
            .collect();
        changed.sort();
        if let Some(name) = changed.first() {
            return Err(format!(
                "{} would find `{name}` in another class than it does",
                site.name
            ));
        }
    }

    let mut edits = remove_bases(class.file_id, def, &bases);
    edits.push(FileEdit {
        file: superclass.file_id,
        edit: add_bases(
            super_def,
            &bases
                .iter()
                .map(|(base, _, _)| base.text)
                .collect::<Vec<_>>()
                .join(", "),
        ),
    });
    let gained = bases
        .iter()
        .flat_map(|(_, ty, _)| instance_member_names(db, &env, *ty))
        .collect();
    Ok(PulledBases { edits, gained })
}

/// The classes `ty` inherits from in its method resolution order, itself not among them.
fn mro_keys<'db>(db: &'db dyn Db, env: &ProgramEnvironment<'db>, ty: Type<'db>) -> Vec<ClassKey> {
    type_hierarchy_mro(db, env, ty)
        .into_iter()
        .map(|class| (class.file.file(db), class.selection_range))
        .collect()
}

/// The method resolution order of `key` with the bases `new_bases` gives the classes whose bases
/// change, by python's C3 linearization; `None` where python could not order them. A class whose
/// bases stay as they are keeps the order python gives it, as `types` says which class it is.
fn linearize<'db>(
    db: &'db dyn Db,
    env: &ProgramEnvironment<'db>,
    key: ClassKey,
    new_bases: &FxHashMap<ClassKey, Vec<ClassKey>>,
    types: &FxHashMap<ClassKey, Type<'db>>,
    linearized: &mut FxHashMap<ClassKey, Option<Vec<ClassKey>>>,
) -> Option<Vec<ClassKey>> {
    if let Some(done) = linearized.get(&key) {
        return done.clone();
    }
    let Some(bases) = new_bases.get(&key) else {
        let order = types
            .get(&key)
            .map(|ty| std::iter::once(key).chain(mro_keys(db, env, *ty)).collect());
        linearized.insert(key, order.clone());
        return order;
    };
    // a cycle of classes has no order; marking the class first ends one
    linearized.insert(key, None);
    let mut sequences: Vec<Vec<ClassKey>> = Vec::new();
    for base in bases {
        sequences.push(linearize(db, env, *base, new_bases, types, linearized)?);
    }
    sequences.push(bases.clone());
    let order = merge(sequences).map(|merged| std::iter::once(key).chain(merged).collect());
    linearized.insert(key, order.clone());
    order
}

/// The C3 merge of `sequences`: python's order for a class's ancestors, given each base's order
/// and the bases' own.
fn merge(mut sequences: Vec<Vec<ClassKey>>) -> Option<Vec<ClassKey>> {
    let mut merged = Vec::new();
    loop {
        sequences.retain(|sequence| !sequence.is_empty());
        if sequences.is_empty() {
            return Some(merged);
        }
        let head = sequences.iter().map(|sequence| sequence[0]).find(|head| {
            !sequences
                .iter()
                .any(|sequence| sequence[1..].contains(head))
        })?;
        merged.push(head);
        for sequence in &mut sequences {
            if sequence[0] == head {
                sequence.remove(0);
            }
        }
    }
}

/// The names each class's own body binds, the ones a lookup through it finds there.
struct OwnNames<'db> {
    db: &'db dyn Db,
    program: ProgramFile<'db>,
    cache: FxHashMap<ClassKey, Vec<Name>>,
}

impl OwnNames<'_> {
    /// Where a lookup through `order`, a method resolution order, finds each name it finds.
    fn found_through(&mut self, order: &[ClassKey]) -> FxHashMap<Name, ClassKey> {
        let mut found = FxHashMap::default();
        for owner in order {
            for name in self.of(*owner) {
                found.entry(name).or_insert(*owner);
            }
        }
        found
    }

    fn of(&mut self, key: ClassKey) -> Vec<Name> {
        if let Some(names) = self.cache.get(&key) {
            return names.clone();
        }
        let program = ProgramFile::new(self.db, key.0, self.program.program(self.db));
        let parsed = parsed_module(self.db, program.python_file(self.db)).load(self.db);
        let mut binder = Binder::default();
        if let Some(def) = all_classes(&parsed)
            .into_iter()
            .find(|def| def.name.range() == key.1)
        {
            binder.visit_body(&def.body);
        }
        let mut names: Vec<Name> = binder.names.into_iter().collect();
        names.sort();
        self.cache.insert(key, names.clone());
        names
    }
}

/// The names statements bind in the scope they are in, not in the functions and classes they
/// define. Every name a store writes counts, and every name an import or a definition binds, so
/// that no name a class could be found to have is missed.
#[derive(Default)]
struct Binder {
    names: FxHashSet<Name>,
}

impl<'a> SourceOrderVisitor<'a> for Binder {
    fn visit_stmt(&mut self, statement: &'a Stmt) {
        match statement {
            Stmt::FunctionDef(function) => {
                self.names.insert(function.name.id.clone());
                for decorator in &function.decorator_list {
                    self.visit_decorator(decorator);
                }
            }
            Stmt::ClassDef(class) => {
                self.names.insert(class.name.id.clone());
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().map_or_else(
                        || alias.name.id.split('.').next().unwrap_or_default(),
                        |asname| asname.id.as_str(),
                    );
                    self.names.insert(Name::new(bound));
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    self.names.insert(bound.id.clone());
                }
            }
            _ => source_order::walk_stmt(self, statement),
        }
    }

    fn visit_expr(&mut self, expression: &'a Expr) {
        match expression {
            Expr::Name(name) if name.ctx.is_store() => {
                self.names.insert(name.id.clone());
            }
            // their own scopes
            Expr::Lambda(_)
            | Expr::ListComp(_)
            | Expr::SetComp(_)
            | Expr::DictComp(_)
            | Expr::Generator(_) => {}
            _ => source_order::walk_expr(self, expression),
        }
    }
}

/// Whether a top-level statement binds `name`.
fn binds_at_top(statement: &Stmt, name: &Name) -> bool {
    let mut binder = Binder::default();
    binder.visit_stmt(statement);
    binder.names.contains(name)
}

/// The edits that take `bases` out of the list `def` writes. A run of them is deleted up to the
/// argument after it, or, at the end of the list, from the argument before it.
fn remove_bases(
    file: File,
    def: &ast::StmtClassDef,
    bases: &[(&WrittenBase, Type, ClassKey)],
) -> Vec<FileEdit> {
    let Some(arguments) = &def.arguments else {
        return Vec::new();
    };
    let ranges: Vec<(TextRange, bool)> = arguments
        .iter_source_order()
        .map(|argument| {
            let range = argument.range();
            let removed = bases
                .iter()
                .any(|(base, _, _)| base.expression.range() == range);
            (range, removed)
        })
        .collect();
    let mut edits = Vec::new();
    let mut index = 0;
    while index < ranges.len() {
        if !ranges[index].1 {
            index += 1;
            continue;
        }
        let first = index;
        while index < ranges.len() && ranges[index].1 {
            index += 1;
        }
        let deleted = match (ranges.get(index), first.checked_sub(1)) {
            (Some((next, _)), _) => TextRange::new(ranges[first].0.start(), next.start()),
            (None, Some(previous)) => {
                TextRange::new(ranges[previous].0.end(), ranges[index - 1].0.end())
            }
            (None, None) => arguments.range(),
        };
        edits.push(FileEdit {
            file,
            edit: Edit::range_deletion(deleted),
        });
    }
    edits
}

/// The edit that writes `bases` after the bases `def` writes, before its keywords.
fn add_bases(def: &ast::StmtClassDef, bases: &str) -> Edit {
    let Some(arguments) = &def.arguments else {
        let after = def
            .type_params
            .as_ref()
            .map_or(def.name.end(), |params| params.end());
        return Edit::insertion(format!("({bases})"), after);
    };
    if let Some(last) = arguments.args.last() {
        Edit::insertion(format!(", {bases}"), last.end())
    } else if let Some(keyword) = arguments.keywords.first() {
        Edit::insertion(format!("{bases}, "), keyword.start())
    } else {
        Edit::insertion(bases.to_string(), arguments.start() + TextSize::new(1))
    }
}
