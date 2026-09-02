//! `.by` and `.py` AST + ty types → BIR
//!
//! the only crate that sees the checker. everything a later pass needs has to be
//! recorded into BIR here, because the optimizer cannot reach back into ty.
//!
//! a construct with no native lowering does not fail the build: the function
//! carrying it is **declined**, recorded with a reason, and left to the
//! interpreted definition. so coverage of the language is total from the first
//! milestone and only the speed varies.

mod closures;
mod generators;
pub mod mapper;
pub mod single_file;

pub use single_file::module_from_source;

/// which language a source is written in
///
/// one question, three answers: how the source parses, whether a loop's binding
/// is fresh on each iteration, and where a declined function's interpreted
/// definition comes from. the compiled half has to follow the source language on
/// every one of them, or the two halves of a module disagree
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Language {
    #[default]
    BasedPython,
    Python,
}

impl Language {
    /// the extension a source of this language is written in
    pub fn extension(self) -> &'static str {
        match self {
            Self::BasedPython => "by",
            Self::Python => "py",
        }
    }

    /// whether each iteration of a loop gets its own binding
    ///
    /// python shares one binding across a whole loop, so a closure made inside it
    /// sees the last value. basedpython gives each iteration its own
    pub fn unique_loop_bindings(self) -> bool {
        matches!(self, Self::BasedPython)
    }
}

use std::fmt::Write;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use by_ir::builder::FunctionBuilder;
use by_ir::function::{
    Binding, CallConvention, ClassBase, ClassKeyword, Declined, Decorator, Function, GradualUse,
    KeywordValue, ModuleIr, ModuleName, SlotAlias, qualify,
};
use by_ir::ops::{
    BinOp, BlockId, CmpOp, Conversion, Mutation, Op, RegisterId, StandardError, Terminator,
    UnaryOp, Value,
};
use by_ir::rtype::{Primitive, RType};
use mapper::{Decline, Layouts, Lowered, map_fixed_tuple, map_type, map_type_with};
use ruff_python_ast::{
    self as ast, CmpOp as AstCmpOp, Expr, ExprContext, Operator, Stmt, UnaryOp as AstUnaryOp,
};
use ruff_python_stdlib::identifiers::is_identifier;
use ruff_text_size::{Ranged, TextSize};
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::types::{KnownClass, TypeDefinition};
use ty_python_semantic::{HasType, SemanticModel};

/// lower every module-level function ty can represent natively
///
/// `module_name` is the **dotted** name python imports the module as, because
/// that is what a class in it has to answer for `__module__` — see
/// [`by_ir::ModuleName`]
pub fn build_module(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    module_name: impl Into<ModuleName>,
    unique_loop_bindings: bool,
) -> ModuleIr {
    let mut module = ModuleIr::new(module_name);

    // a call is only lowered natively when the callee is a module-level function
    // in this same unit, so the set has to be known before any body is lowered
    let native_callees: HashSet<String> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) => Some(function.name.to_string()),
            _ => None,
        })
        .collect();
    // …and only when the *name* still holds that function. a decorator replaces what
    // the module namespace binds with whatever it returned, which may be another
    // function, a class, a descriptor, or no callable at all — so a call through the
    // name has to go out through the namespace and find it. classes are in here for
    // the same reason: a construction is written against the name, not against the
    // type this module emitted under it.
    //
    // this is a separate set from `native_callees` because that one answers "does this
    // module declare the name at all", which is what says whether `len`, `range` and
    // `super` are still the builtins — and a decorator does not change that
    //
    // a *modifier* is not a decorator and rebinds nothing, so the question is which
    // decorators survive translation rather than which definitions carry one.
    //
    // a name a frame declares `global` is in here for the third form of the same
    // reason: the frame is going to rebind it, and the definition this module emitted
    // under that name is then not what the name holds. reaching it directly answered
    // with the old function and refused the new class outright
    let rebinds = |decorators: &[ast::Decorator], class: bool| {
        decorators.iter().any(|decorator| {
            let role = if class {
                class_modifier(db, model, decorator)
            } else {
                function_modifier(db, model, decorator)
            };
            // one that declines takes its definition with it, so either answer is safe
            !matches!(role, Ok(Modifier::Erased | Modifier::DataClass))
        })
    };
    let decorated: HashSet<String> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) if rebinds(&function.decorator_list, false) => {
                Some(function.name.to_string())
            }
            Stmt::ClassDef(class) if rebinds(&class.decorator_list, true) => {
                Some(class.name.to_string())
            }
            _ => None,
        })
        .chain(declared_global_anywhere(suite))
        .collect();

    // every name this module reads anywhere — see `names_read`
    let read = names_read(suite);
    // every attribute a `del` anywhere in this module names — see `deleted_attributes`
    let deleted = deleted_attributes(suite);

    // pass one: which classes get an emitted layout. a body cannot be lowered
    // until this is known, because whether `self.x` is a field read or a
    // `PyObject_GetAttr` depends on it.
    //
    // the names come first and the field types second, because a field may be
    // typed as another emitted class — including one declared later, or the
    // declaring class itself. mapping a field type only asks whether the class
    // has a layout, never what is in it, so knowing the names is enough
    let mut layouts: Layouts = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ClassDef(class) => Some((class.name.to_string(), Vec::new())),
            _ => None,
        })
        .collect();
    // a class that declines has no layout, and dropping it can only ever shrink
    // the set — so re-deriving until nothing more drops out terminates, and
    // leaves no field typed as a class that will not be emitted
    // a subclass's layout is its base's plus its own, so an entry *grows* as well as
    // disappearing. one round per class is enough for any chain; a base cycle would
    // never settle, and is not a class this can compile
    let mut settling = true;
    for _ in 0..=layouts.len() {
        if !settling {
            break;
        }
        settling = false;
        for stmt in suite {
            if let Stmt::ClassDef(class) = stmt
                && layouts.contains_key(class.name.as_str())
            {
                match class_fields(db, env, model, suite, class, &layouts, &deleted) {
                    Ok(fields) => {
                        if layouts.get(class.name.as_str()) != Some(&fields) {
                            layouts.insert(class.name.to_string(), fields);
                            settling = true;
                        }
                    }
                    Err(_) => {
                        layouts.remove(class.name.as_str());
                        settling = true;
                    }
                }
            }
        }
    }
    if settling {
        // still moving after a round per class: the bases form a cycle
        layouts.clear();
    }

    // the signature of every method of every emitted class. a direct call needs
    // the callee's representations *before* the callee is lowered, because a
    // method may call a sibling — or itself
    // a class emitted as a mutable *heap* type — one with a decorator, one another
    // extends, or one that extends another — gives up the direct method call: python
    // can rebind a method on it, or override it in a subclass, and a direct call
    // would see neither. a plain class is a static type that can be neither modified
    // nor subclassed, which is what licenses the direct call
    let declared: Vec<&ast::StmtClassDef> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ClassDef(class) => Some(class),
            _ => None,
        })
        .collect();
    let mut mutable: HashSet<&str> = HashSet::new();
    for class in &declared {
        if decorated.contains(class.name.as_str()) {
            mutable.insert(class.name.as_str());
        }
        if let Some(base) = base_class(db, env, model, suite, class, &layouts)
            .ok()
            .flatten()
        {
            mutable.insert(class.name.as_str());
            // a base of ours is made mutable too, whether it stands alone or beside a
            // name from outside: this class may override a method of it, and a direct
            // call on a receiver typed as the base would not see that. one out of this
            // module is somebody else's type and this module does not say how it is built
            for name in base.plain_names() {
                if let Some(owner) = declared
                    .iter()
                    .find(|candidate| candidate.name.as_str() == name)
                {
                    mutable.insert(owner.name.as_str());
                }
            }
        }
    }

    let mut methods: Methods = HashMap::new();
    for stmt in suite {
        if let Stmt::ClassDef(class) = stmt
            && layouts.contains_key(class.name.as_str())
        {
            let receiver = RType::Instance {
                class: class.name.to_string(),
                exact: false,
            };
            let table = class
                .body
                .iter()
                .filter_map(|member| match member {
                    Stmt::FunctionDef(method) if method.decorator_list.is_empty() => {
                        let mut signature = signature(
                            db,
                            env,
                            model,
                            method,
                            &layouts,
                            Some(Receiver::Explicit(&receiver)),
                            &[],
                        )
                        .ok()?;
                        resumable_return(method, &mut signature);
                        Some((mangled(Some(&class.name), &method.name), signature))
                    }
                    _ => None,
                })
                .collect();
            methods.insert(class.name.to_string(), table);
        }
    }

    // module-level signatures, so a call can coerce its arguments to what the callee
    // takes. a generator's constructor returns the state object, whatever its
    // annotation says
    let mut signatures: HashMap<String, Signature> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) => {
                let mut signature =
                    signature(db, env, model, function, &layouts, None, &[]).ok()?;
                resumable_return(function, &mut signature);
                Some((function.name.to_string(), signature))
            }
            _ => None,
        })
        .collect();

    // an unboxed edition, for the functions that can have one. it is registered
    // before any body is lowered, because whether a *caller* keeps its list in a
    // buffer depends on whether the callee it hands it to has this edition
    // handing a name to a callee's edition keeps it in a buffer, which is itself an
    // eligibility this is computing — so it is iterated. eligibility only ever grows,
    // and the positions are bounded by the parameters, so it settles
    let supplied = supplied_arrays(db, env, model, suite, &layouts);
    let mut arrays = ArrayEditions::new();
    loop {
        let next: ArrayEditions = suite
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::FunctionDef(function) if !generators::is_generator(&function.body) => {
                    let found =
                        array_editions(db, env, model, function, &layouts, &arrays, &supplied);
                    (!found.is_empty()).then(|| (function.name.to_string(), found))
                }
                _ => None,
            })
            .collect();
        let size =
            |editions: &ArrayEditions| -> usize { editions.values().flatten().map(Vec::len).sum() };
        let grew = size(&next) > size(&arrays);
        arrays = next;
        if !grew {
            break;
        }
    }
    let registered: Vec<(String, Signature)> = arrays
        .iter()
        .flat_map(|(name, editions)| editions.iter().map(move |positions| (name, positions)))
        .filter_map(|(name, positions)| {
            let mut edition = signatures.get(name)?.clone();
            for (index, rtype) in positions {
                if let Some((_, slot)) = edition.params.get_mut(*index) {
                    *slot = rtype.clone();
                }
                // a buffer is a representation the boundary cannot establish, and this
                // edition is the one the boundary never reaches
                edition.deferring.retain(|deferring| deferring != index);
            }
            Some((edition_name(name, positions), edition))
        })
        .collect();
    signatures.extend(registered);

    // a plain class's own `__init__`, so constructing one is an allocation and a
    // native call rather than a trip out through the module namespace. keyed the
    // way a method call is, on `Class.__init__` — which no module-level function
    // can be named, so the two share one map
    signatures.extend(
        suite
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::ClassDef(class) if layouts.contains_key(class.name.as_str()) => Some(class),
                _ => None,
            })
            .filter_map(|class| {
                let init = class.body.iter().find_map(|statement| match statement {
                    Stmt::FunctionDef(function) if function.name.as_str() == "__init__" => {
                        Some(function)
                    }
                    _ => None,
                })?;
                // the receiver is this exact class: `__init__` runs before anything
                // could have subclassed the instance
                let receiver = RType::Instance {
                    class: class.name.to_string(),
                    exact: true,
                };
                let signature = signature(
                    db,
                    env,
                    model,
                    init,
                    &layouts,
                    Some(Receiver::Explicit(&receiver)),
                    &[],
                )
                .ok()?;
                Some((qualify(Some(class.name.as_str()), "__init__"), signature))
            }),
    );

    // an `async def` that never suspends gets a second edition holding the same body,
    // which `await` of it calls instead of building a coroutine that one `send` would
    // finish. its return is the body's own rather than the state object, so this is the
    // signature *before* `resumable_return` widened it
    //
    // only a name a call would already have reached natively: a decorator, or a `global`
    // that rebinds the name, means the name does not hold this definition, and the
    // interpreted one has to answer for the `await` the same as for the call
    signatures.extend(
        suite
            .iter()
            .filter_map(|stmt| match stmt {
                Stmt::FunctionDef(function)
                    if generators::never_suspends(function)
                        && !decorated.contains(function.name.as_str())
                        && defined_once(suite, function).is_ok() =>
                {
                    Some(function)
                }
                _ => None,
            })
            .filter_map(|function| {
                let signature = signature(db, env, model, function, &layouts, None, &[]).ok()?;
                Some((generators::direct_name(&function.name), signature))
            }),
    );

    // each emitted class's base, so an upcast to it is recognised as free
    let owned_mutable: HashSet<String> = mutable.iter().map(|name| (*name).to_string()).collect();

    let slotted: HashSet<String> = declared
        .iter()
        .filter(|class| declared_slots(class).is_some())
        .map(|class| class.name.to_string())
        .collect();

    let bases: HashMap<String, String> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ClassDef(class) if layouts.contains_key(class.name.as_str()) => Some(class),
            _ => None,
        })
        .filter_map(|class| {
            // the map is the *layout* chain, which only an in-module base extends
            base_class(db, env, model, suite, class, &layouts)
                .ok()
                .flatten()
                .and_then(|base| base.in_module().map(str::to_owned))
                .map(|base| (class.name.to_string(), base))
        })
        .collect();

    let properties = published_properties(db, model, suite, &layouts);

    // read off the source rather than off the lowered methods: a `__new__` python never
    // reaches the method table for — one carrying a decorator, one whose class declined —
    // is still the constructor a `C(...)` in this module has to run
    let constructs: HashSet<String> = suite
        .iter()
        .filter_map(|stmt| {
            match stmt {
            Stmt::ClassDef(class) => class
                .body
                .iter()
                .any(|member| {
                    matches!(member, Stmt::FunctionDef(method) if method.name.as_str() == "__new__")
                })
                .then(|| class.name.to_string()),
            _ => None,
        }
        })
        .collect();

    let no_directs: HashSet<String> = HashSet::new();
    let unit = Unit {
        mutable: &owned_mutable,
        slotted: &slotted,
        constructs: &constructs,
        bases: &bases,
        properties: &properties,
        unique_loop_bindings,
        db,
        env,
        model,
        native_callees: &native_callees,
        decorated: &decorated,
        read: &read,
        deleted: &deleted,
        suite,
        layouts: &layouts,
        methods: &methods,
        signatures: &signatures,
        arrays: &arrays,
        // filled in below: the editions themselves are lowered against a unit where
        // nothing redirects, because a direct edition never contains an `await`
        directs: &no_directs,
        // a module-level frame is in no class body, so nothing it names is mangled
        owner: None,
    };

    // pass one and a half: the direct editions, before any caller, because an `await`
    // only redirects to one that is there. one that declines leaves the `await` on the
    // coroutine path, which is what it had before
    let mut direct_editions: Vec<Function> = Vec::new();
    let mut directs: HashSet<String> = HashSet::new();
    for stmt in suite {
        let Stmt::FunctionDef(function) = stmt else {
            continue;
        };
        if !signatures.contains_key(&generators::direct_name(&function.name)) {
            continue;
        }
        if let Ok(edition) = lower_direct_edition(unit, function)
            .and_then(|mut edition| verify_one(&mut edition).map(|()| edition))
        {
            directs.insert(function.name.to_string());
            direct_editions.push(edition);
        }
    }
    let unit = Unit {
        directs: &directs,
        ..unit
    };
    module.functions.extend(direct_editions);

    // pass two: bodies, with the layouts available
    for stmt in suite {
        if let Stmt::ClassDef(class) = stmt {
            match lower_class(unit, class).and_then(verified_class) {
                Ok((lowered, environments)) => {
                    module.classes.push(lowered);
                    // an environment a method needed is a sibling class, and the layout
                    // its methods reference has to be emitted too
                    module.classes.extend(environments);
                }
                Err(decline) => module.declined.push(Declined {
                    name: class.name.to_string(),
                    reason: decline.reason,
                    range: Some(span(class.range)),
                }),
            }
        }
        if let Stmt::FunctionDef(function) = stmt {
            module
                .gradual
                .extend(gradual_signature_places(db, env, model, function));
            module
                .promoted
                .extend(promoted_places(db, env, model, function, &layouts));
            match defined_once(suite, function)
                .and_then(|()| lower_function(unit, function))
                .and_then(verified)
            {
                Ok((lowered, environments)) => {
                    module.functions.push(lowered);
                    // an environment is a real emitted class, just not a named one
                    module.classes.extend(environments);
                    // the unboxed edition is an optimisation: if it declines, the
                    // boxed one still stands and every caller reaches that
                    for positions in arrays.get(function.name.as_str()).into_iter().flatten() {
                        if let Ok(edition) = lower_array_edition(unit, function, positions)
                            .and_then(|mut edition| verify_one(&mut edition).map(|()| edition))
                        {
                            module.functions.push(edition);
                        }
                    }
                }
                Err(decline) => module.declined.push(Declined {
                    name: function.name.to_string(),
                    reason: decline.reason,
                    range: Some(span(function.range)),
                }),
            }
        }
    }

    // a direct edition is emitted for every coroutine that could have one, because
    // whether an `await` reaches it is only known once the callers are lowered. one
    // nothing reached is a second copy of a body under a name the namespace never
    // binds, so it is dropped rather than emitted for nobody
    //
    // an edition holds no `await` of its own, so it can never be the caller here and
    // this settles in one pass
    let called: HashSet<String> = module
        .all_functions()
        .flat_map(|function| function.blocks.iter())
        .flat_map(|block| block.ops.iter())
        .filter_map(|op| match op {
            Op::CallNative { owner, callee, .. } => Some(qualify(owner.as_deref(), callee)),
            _ => None,
        })
        .collect();
    module
        .functions
        .retain(|function| function.coroutine_body.is_none() || called.contains(&function.name));

    // the pruner drops things by name, and a dropped definition still deserves to
    // point at its source
    let ranges: HashMap<String, (u32, u32)> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) => Some((function.name.to_string(), span(function.range))),
            Stmt::ClassDef(class) => Some((class.name.to_string(), span(class.range))),
            _ => None,
        })
        .collect();
    // every class the module writes, and the names its header extends. syntactic
    // on purpose: what the interpreted definition builds on is whatever the name
    // resolves to in the module namespace when the `class` statement runs, which
    // is this module's compiled type wherever it emitted one. a name the module
    // aliases stands for the class it was bound to, the same as in a base
    let extends: Vec<(String, Vec<String>)> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ClassDef(class) => Some((
                class.name.to_string(),
                class
                    .bases()
                    .iter()
                    .filter_map(|base| match base {
                        Expr::Name(name) => {
                            // a name this cannot settle stands for itself here, which is
                            // what the interpreted definition looked up
                            Some(
                                base_stands_for(suite, name.id.as_str())
                                    .unwrap_or(name.id.as_str())
                                    .to_string(),
                            )
                        }
                        _ => None,
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect();
    prune_unbuildable(
        &mut module,
        &ranges,
        &extends,
        &disturbed_definitions(suite),
    );
    module
}

/// what the module body does to its own definitions after making them
struct Disturbed {
    /// every `def` or `class` whose name the body binds again
    rebound: HashSet<String>,
    /// every class the body writes a dunder attribute onto, and one such name
    dunder: HashMap<String, String>,
}

/// every module-level `def` or `class` the module body disturbs after defining it
///
/// module init installs the native definition into the namespace over whatever the
/// fallback source left there, which is the definition it replaces only while nothing
/// rebound the name. `_not_given = _not_given()` leaves an *instance* there, and
/// installing the class over it is a silent wrong answer
///
/// a *second* definition of the same name is a different shape and is not covered:
/// nothing in the stdlib does it, and two emitted functions of one name would
/// collide in the C long before the namespace did
///
/// a binding *before* the definition is the ordinary forward declaration —
/// `Enum = Flag = ReprEnum = None` ahead of the classes themselves — which the
/// definition then overwrites, so the two are compared by position
///
/// a dunder written onto a *class* is the other half, and it is not about the name at
/// all: what an emitted type takes from its twin is the twin's own dict minus the
/// dunders, because a dunder is what a type slot answers and a second answer in the dict
/// would disagree with it. so a dunder the body wrote there has nowhere to land, and
/// where it is compared by position the ordinary forward declaration is
fn disturbed_definitions(suite: &[Stmt]) -> Disturbed {
    let defined: HashMap<&str, TextSize> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::FunctionDef(function) => Some((function.name.as_str(), function.range.start())),
            Stmt::ClassDef(class) => Some((class.name.as_str(), class.range.start())),
            _ => None,
        })
        .collect();
    let mut bindings = ModuleBindings {
        found: Vec::new(),
        unbinds: false,
        dunders: Vec::new(),
    };
    ast::visitor::walk_body(&mut bindings, suite);
    let classes: HashSet<&str> = suite
        .iter()
        .filter_map(|stmt| match stmt {
            Stmt::ClassDef(class) => Some(class.name.as_str()),
            _ => None,
        })
        .collect();
    let dunder = bindings
        .dunders
        .into_iter()
        .filter(|(owner, _)| classes.contains(owner))
        .map(|(owner, attribute)| (owner.to_string(), attribute.to_string()))
        .collect();
    // a name taken back out of the namespace object is a `del` whose target this cannot
    // read — `ast` pops five of its own classes out through a comprehension — so every
    // definition is treated as one it could have been
    if bindings.unbinds {
        return Disturbed {
            rebound: defined.into_keys().map(str::to_string).collect(),
            dunder,
        };
    }
    Disturbed {
        rebound: bindings
            .found
            .into_iter()
            .filter(|(name, at)| defined.get(name).is_some_and(|defined| at > defined))
            .map(|(name, _)| name.to_string())
            .collect(),
        dunder,
    }
}

/// whether a name is one python spells with two underscores at each end
///
/// the same test `By_IsDunder` makes, which is what decides at runtime whether an
/// attribute is carried from a twin
fn is_a_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// whether an expression is the module namespace itself
///
/// only the call, not a name it was stored in first: what a namespace held somewhere else
/// can be made to do is a wider question than this one, and the answer to it is not
/// syntactic
fn module_namespace(expr: &Expr) -> bool {
    matches!(expr, Expr::Call(call)
        if call.arguments.is_empty()
            && matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "globals"))
}

/// whether an expression takes a binding back out of the module namespace
fn unbinds_through_the_namespace(expr: &Expr) -> bool {
    match expr {
        // `del globals()[name]`, which the store/delete context is the whole of
        Expr::Subscript(subscript) => {
            subscript.ctx == ExprContext::Del && module_namespace(&subscript.value)
        }
        Expr::Call(call) => match call.func.as_ref() {
            Expr::Attribute(attribute) => {
                module_namespace(&attribute.value)
                    && matches!(attribute.attr.as_str(), "pop" | "popitem" | "clear")
            }
            _ => false,
        },
        _ => false,
    }
}

/// every name the module body binds, and where
///
/// the binding forms are asked about rather than enumerated: a store context covers
/// assignment, unpacking, `for`, `with ... as` and the walrus alike, and the three
/// that bind an identifier rather than an expression are the remaining cases
struct ModuleBindings<'a> {
    found: Vec<(&'a str, TextSize)>,
    /// whether the body took a name back out of its own namespace, without saying which
    unbinds: bool,
    /// the dunder attributes the body writes onto a name, as `(owner, attribute)`
    dunders: Vec<(&'a str, &'a str)>,
}

impl<'a> ast::visitor::Visitor<'a> for ModuleBindings<'a> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        // a nested body is a scope of its own: what it binds is a local or a class
        // attribute. a `global` write from one is a rebind module init never races,
        // because nothing has called the function yet
        match stmt {
            Stmt::FunctionDef(_) | Stmt::ClassDef(_) => {}
            _ => ast::visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        if let Expr::Name(name) = expr
            && matches!(name.ctx, ExprContext::Store | ExprContext::Del)
        {
            self.found.push((name.id.as_str(), name.range.start()));
        }
        if let Expr::Attribute(attribute) = expr
            && matches!(attribute.ctx, ExprContext::Store | ExprContext::Del)
            && let Expr::Name(owner) = attribute.value.as_ref()
            && is_a_dunder(attribute.attr.as_str())
        {
            self.dunders
                .push((owner.id.as_str(), attribute.attr.as_str()));
        }
        self.unbinds |= unbinds_through_the_namespace(expr);
        ast::visitor::walk_expr(self, expr);
    }

    fn visit_alias(&mut self, alias: &'a ast::Alias) {
        let bound = match &alias.asname {
            Some(asname) => asname.as_str(),
            // `import a.b` binds `a`, which is what a later `a = ...` would replace
            None => alias.name.split('.').next().unwrap_or_default(),
        };
        self.found.push((bound, alias.range.start()));
    }

    fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(bound) = handler;
        if let Some(name) = &bound.name {
            self.found.push((name.as_str(), name.range.start()));
        }
        ast::visitor::walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a ast::Pattern) {
        let capture = match pattern {
            ast::Pattern::MatchAs(node) => node.name.as_ref(),
            ast::Pattern::MatchStar(node) => node.name.as_ref(),
            ast::Pattern::MatchMapping(node) => node.rest.as_ref(),
            _ => None,
        };
        if let Some(name) = capture {
            self.found.push((name.as_str(), name.range.start()));
        }
        ast::visitor::walk_pattern(self, pattern);
    }
}

/// the places in `function`'s signature where a gradual type enters
///
/// a gradual type compiles — it lands on `object` — so nothing here stops the
/// build. it is recorded because `--no-any` is a question about types, and after
/// the boxed representation landed it can no longer be answered by looking at
/// the byte offsets of a node, as a plain pair — `by_ir` does not depend on the ast
fn span(range: ruff_text_size::TextRange) -> (u32, u32) {
    (range.start().to_u32(), range.end().to_u32())
}

/// lower a generator: a constructor, and a state class whose `$resume` method is
/// the body as a state machine
fn lower_generator(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
    decorators: Vec<Decorator>,
    receiver: Option<Receiver<'_>>,
    captures: Option<&closures::Nested>,
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    let Unit {
        env,
        db,
        model,
        layouts,
        ..
    } = unit;
    generators::check(function)?;

    // two classes may each have a `values` method, and each needs its own state
    // class — so the receiver's class namespaces the name the way it does a method's
    let class = match receiver.and_then(Receiver::owner) {
        Some(owner) => generators::state_name(&format!("{owner}.{}", function.name)),
        None => generators::state_name(&function.name),
    };
    // every parameter and local lives in a field: it has to survive the suspension.
    // a field is a *cell* — `object`, with an unset check on every read — unless the
    // name is definitely assigned, in which case it takes the local's own
    // representation and the read is an infallible `GetField`
    // a declared `global` is not one of them: it lives in the module namespace, which
    // already outlives every suspension
    let declared_global = declared_globals(&function.body);
    let mut representations =
        local_representations(db, env, model, &function.body, layouts, unit.arrays);
    representations.retain(|(name, _)| !declared_global.contains(name));
    let locals: Vec<String> = representations
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    let captured: Vec<String> = captures
        .map(|nested| nested.captures.clone())
        .unwrap_or_default();
    // the constructor seeds every one of them, so they are as assigned as a parameter
    let mut assigned = generators::definitely_assigned(function);
    assigned.extend(captured.iter().cloned());
    let parameters = signature(db, env, model, function, layouts, receiver, &[])?.params;
    let representation = |name: &str| {
        parameters
            .iter()
            .chain(representations.iter())
            .find(|(candidate, _)| candidate == name)
            .map(|(_, rtype)| rtype.clone())
    };
    let names = {
        let mut names = generators::state_names(function, &locals);
        let extra: Vec<String> = captured
            .iter()
            .filter(|name| !names.contains(name))
            .cloned()
            .collect();
        names.extend(extra);
        names
    };
    let fields: Vec<by_ir::function::FieldDecl> = names
        .into_iter()
        .map(|name| by_ir::function::FieldDecl {
            optional: false,
            // the state number is never unset — the constructor writes 0 — so it is
            // an unboxed int, and the dispatch reads it without a narrowing
            ty: if name == generators::STATE_FIELD || name == generators::KIND_FIELD {
                RType::INT
            } else if assigned.contains(&name) {
                representation(&name).unwrap_or(RType::OBJECT)
            } else {
                RType::OBJECT
            },
            name,
            default: None,
        })
        .collect();

    let mut layouts_with_state = layouts.clone();
    layouts_with_state.insert(class.clone(), fields.clone());
    let (resume, parked) = lower_resume(
        Unit {
            layouts: &layouts_with_state,
            ..unit
        },
        function,
        &class,
        &fields,
        &assigned,
    )?;

    // which registers had to be parked is only known once the body is lowered, so the
    // slots they took join the layout here — before the constructor, which seeds one
    // value per field and would otherwise leave the last of them off
    let mut fields = fields;
    fields.extend(parked);
    layouts_with_state.insert(class.clone(), fields.clone());
    let constructor = lower_generator_constructor(
        Unit {
            layouts: &layouts_with_state,
            ..unit
        },
        function,
        &class,
        &fields,
        decorators,
        receiver,
        &captured,
    )?;

    Ok((
        constructor,
        vec![by_ir::function::ClassIr {
            exported: false,
            declares_slots: false,
            name: class,
            // the machine is the same; only the surface differs. a coroutine answers
            // `__await__` and is deliberately *not* iterable
            resume: Some(by_ir::function::Resumption {
                method: generators::RESUME_METHOD.to_string(),
                surface: match (function.is_async, generators::is_generator(&function.body)) {
                    (true, true) => by_ir::function::Surface::AsyncGenerator,
                    (true, false) => by_ir::function::Surface::Coroutine,
                    _ => by_ir::function::Surface::Generator,
                },
            }),
            fields,
            decorators: Vec::new(),
            constants: Vec::new(),
            slot_aliases: Vec::new(),
            generic: false,
            properties: Vec::new(),
            methods: vec![resume],
            base: None,
            inherited_init: false,
            immutable: false,
            keywords: Vec::new(),
        }],
    ))
}

/// the function the *call* runs: allocate the state object, seed the parameters
fn lower_generator_constructor(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
    class: &str,
    fields: &[by_ir::function::FieldDecl],
    decorators: Vec<Decorator>,
    receiver: Option<Receiver<'_>>,
    captured: &[String],
) -> Lowered<Function> {
    let Unit {
        env,
        db,
        model,
        layouts,
        ..
    } = unit;
    let Signature {
        params,
        defaults,
        vararg,
        kwarg,
        posonly,
        kwonly,
        deferring,
        computed_defaults,
        ..
    } = signature(db, env, model, function, layouts, receiver, &[])?;

    let mut builder = FunctionBuilder::new(function.name.to_string(), RType::OBJECT);
    builder.at(span(function.range));
    builder.decorators(decorators);
    builder.defaults(defaults);
    builder.variadic(vararg, kwarg);
    builder.binding_kinds(posonly, kwonly);
    builder.deferring(deferring);
    builder.computed_defaults(computed_defaults);
    let mut registers: HashMap<String, RegisterId> = HashMap::new();
    for (name, rtype) in &params {
        registers.insert(name.clone(), builder.param(name.clone(), rtype.clone()));
    }

    let state = builder.local(
        "$gen".to_string(),
        RType::Instance {
            class: class.to_string(),
            exact: false,
        },
    );
    // a capture is read out of the environment *here*, in the frame that has one —
    // the resumable frame outlives this call and cannot reach back for it
    let environment = registers.get("$env").copied();
    let env_class = match receiver.map(Receiver::rtype) {
        Some(RType::Instance { class, .. }) => class.clone(),
        _ => String::new(),
    };
    let mut seeded: HashMap<String, RegisterId> = HashMap::new();
    for name in captured {
        let Some(env) = environment else { continue };
        let ty = layouts
            .get(&env_class)
            .and_then(|env_fields| {
                env_fields
                    .iter()
                    .find(|field| field.name == *name)
                    .map(|field| field.ty.clone())
            })
            .unwrap_or(RType::OBJECT);
        let dest = builder.temp(ty);
        builder.push(Op::GetField {
            dest,
            receiver: Value::Register(env),
            class: env_class.clone(),
            field: name.clone(),
        });
        seeded.insert(name.clone(), dest);
    }

    let mut values = Vec::with_capacity(fields.len());
    for field in fields {
        values.push(match field.name.as_str() {
            // the machine starts at resumption point 0, and nothing has been sent
            generators::STATE_FIELD | generators::KIND_FIELD => Some(Value::Int(0)),
            generators::SENT_FIELD => Some(Value::None),
            name => registers.get(name).or(seeded.get(name)).map(|&id| {
                // the field only needs boxing when it *is* an object — a
                // definitely-assigned parameter keeps its own representation, and
                // boxing into it would be a type error
                let unboxed = builder.register_type(id) != Some(&RType::OBJECT);
                if field.ty != RType::OBJECT || !unboxed {
                    return Value::Register(id);
                }
                let boxed = builder.temp(RType::OBJECT);
                builder.push(Op::Box {
                    dest: boxed,
                    src: Value::Register(id),
                });
                Value::Register(boxed)
            }),
        });
    }
    builder.push(Op::NewInstance {
        dest: state,
        class: class.to_string(),
        fields: values,
    });
    let boxed = builder.temp(RType::OBJECT);
    builder.push(Op::Box {
        dest: boxed,
        src: Value::Register(state),
    });
    builder.terminate(Terminator::Return(Value::Register(boxed)));
    Ok(builder.finish())
}

/// the body, as a method that resumes at `$state` and returns the next yielded value
///
/// also reports the fields the parked registers took, which only the lowered body says
fn lower_resume(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
    class: &str,
    fields: &[by_ir::function::FieldDecl],
    assigned: &HashSet<String>,
) -> Lowered<(Function, Vec<by_ir::function::FieldDecl>)> {
    let Unit {
        db,
        model,
        native_callees,
        layouts,
        methods,
        signatures,
        ..
    } = unit;

    let mut builder = FunctionBuilder::new(generators::RESUME_METHOD.to_string(), RType::OBJECT);
    builder.at(span(function.range));
    let receiver = builder.param(
        "$gen".to_string(),
        RType::Instance {
            class: class.to_string(),
            exact: false,
        },
    );
    debug_assert_eq!(receiver, RegisterId(0));

    // block 0 dispatches, so the body starts in its own block and the chain is
    // filled in afterwards — the resumption points are only known once it is lowered
    let entry = builder.new_block();
    let exhausted = builder.new_block();
    builder.switch_to(entry);

    let mut lowering = Lowering {
        arrays: unit.arrays,
        directs: unit.directs,
        in_range: Vec::new(),
        mutable: unit.mutable,
        slotted: unit.slotted,
        constructs: unit.constructs,
        bases: unit.bases,
        properties: unit.properties,
        db,
        model,
        builder,
        locals: HashMap::new(),
        globals: declared_globals(&function.body),
        native_callees,
        decorated: unit.decorated,
        layouts,
        methods,
        signatures,
        ret: RType::OBJECT,
        loops: Vec::new(),
        handling: Vec::new(),
        owner: unit.owner.map(str::to_string),
        zero_super: Err(
            "a `super()` with no arguments reads slot zero, which a generator's resume frame fills with its state",
        ),
        comprehensions: 0,
        environment: None,
        captures: Some(Captured {
            class: class.to_string(),
            receiver,
            // a definitely-assigned name is a plain field: no unset check, and the
            // local's own representation
            names: fields
                .iter()
                .map(|field| field.name.clone())
                .filter(|name| assigned.contains(name))
                .collect(),
            cells: fields
                .iter()
                .map(|field| field.name.clone())
                .filter(|name| name != generators::STATE_FIELD && !assigned.contains(name))
                .collect(),
            // a generator's state field is the frame's *own* local, parked
            free: false,
        }),
        generator: Some(Generator {
            class: class.to_string(),
            resumptions: Vec::new(),
            iterators: 0,
        }),
        delegations: 0,
        contexts: 0,
        cleanups: Vec::new(),
    };
    lowering.block(&function.body)?;

    // falling off the end exhausts the generator
    if !lowering.builder.is_sealed(lowering.builder.current_block()) {
        lowering.builder.terminate(Terminator::Goto(exhausted));
    }

    let resumptions = lowering
        .generator
        .take()
        .map(|generator| generator.resumptions)
        .unwrap_or_default();

    // the property a *direct* edition rests on, checked against the machine that was
    // actually built rather than trusted from the syntax it was read off. an `await`
    // has already been lowered to a plain call by the time this runs, so a coroutine
    // that turns out to have a resumption point after all has to take its edition down
    // with it — declining here is what makes the pruner do that
    if generators::never_suspends(function) && !resumptions.is_empty() {
        return Err(Decline::new(format!(
            "`{}` was read as a coroutine that never suspends and lowered to {} suspension point(s)",
            function.name,
            resumptions.len()
        )));
    }

    // the dispatch: `$state` against each resumption point, falling through to
    // exhausted. a chain of branches *is* a jump table, and the C compiler builds it
    lowering.builder.switch_to(Function::entry());
    let narrowed = lowering.builder.temp(RType::INT);
    lowering.builder.push(Op::GetField {
        dest: narrowed,
        receiver: Value::Register(receiver),
        class: class.to_string(),
        field: generators::STATE_FIELD.to_string(),
    });
    let mut targets = vec![(0i64, entry)];
    targets.extend(resumptions.iter().map(|point| (point.state, point.resume)));
    for (index, (value, block)) in targets.iter().enumerate() {
        let matched = lowering.builder.temp(RType::BIT);
        lowering.builder.push(Op::IntCompare {
            dest: matched,
            op: CmpOp::Eq,
            lhs: Value::Register(narrowed),
            rhs: Value::Int(*value),
        });
        let next = if index + 1 == targets.len() {
            exhausted
        } else {
            lowering.builder.new_block()
        };
        lowering.builder.terminate(Terminator::Branch {
            cond: Value::Register(matched),
            then_block: *block,
            else_block: next,
        });
        lowering.builder.switch_to(next);
    }

    // exhausted: mark it so, and finish handing back `None`.
    //
    // both ways a frame ends without naming a value arrive here — running off the end
    // of the body, and being resumed again once it already has — and python answers
    // both with `StopIteration(None)`. it is a *finish* rather than a raise, so the
    // send slot can report it without an exception being built at all
    lowering.builder.switch_to(exhausted);
    lowering.builder.push(Op::SetField {
        receiver: Value::Register(receiver),
        class: class.to_string(),
        field: generators::STATE_FIELD.to_string(),
        value: Value::Int(-1),
    });
    let nothing = lowering.builder.temp(RType::OBJECT);
    lowering.builder.push(Op::Box {
        dest: nothing,
        src: Value::None,
    });
    lowering.builder.push(Op::FinishFrame {
        value: Value::Register(nothing),
    });
    lowering.builder.terminate(Terminator::Unreachable);

    let mut lowered = lowering.builder.finish();
    let parked = generators::park_live_registers(&mut lowered, class, &resumptions)?;
    lowered.owner = Some(class.to_string());
    lowered.exported = false;
    Ok((lowered, parked))
}

/// a lowering that does not verify is a *decline*, not a build failure
///
/// codegen is only correct for well-formed BIR, and a frontend bug that produces
/// ill-formed BIR should cost one function's speed rather than the whole module.
/// the optimization passes are held to a stricter standard — a pass bug fails the
/// build, because it is not user code that provoked it
fn verified(
    lowered: (Function, Vec<by_ir::function::ClassIr>),
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    let (mut function, mut environments) = lowered;
    verify_one(&mut function)?;
    for method in environments
        .iter_mut()
        .flat_map(|environment| environment.methods.iter_mut())
    {
        verify_one(method)?;
    }
    Ok((function, environments))
}

fn verified_class(
    lowered: (by_ir::function::ClassIr, Vec<by_ir::function::ClassIr>),
) -> Lowered<(by_ir::function::ClassIr, Vec<by_ir::function::ClassIr>)> {
    let (mut class, mut environments) = lowered;
    for method in class.methods.iter_mut().chain(
        environments
            .iter_mut()
            .flat_map(|environment| environment.methods.iter_mut()),
    ) {
        verify_one(method)?;
    }
    Ok((class, environments))
}

fn verify_one(function: &mut Function) -> Lowered<()> {
    // a local some path reads before writing is compiled with a byte saying whether it
    // was written, so the flag has to be set before the verifier objects to the read
    by_ir::unbound_locals::mark(function);
    by_ir::verify::verify(function).map_err(|errors| {
        let detail = errors
            .iter()
            .map(|error| error.message.clone())
            .collect::<Vec<_>>()
            .join("; ");
        // a finding about the *source* is why this function cannot compile, not
        // evidence that the compiler mislowered it — saying "not well-formed" there
        // sends someone looking for a bug that is not theirs and is not ours either
        if errors.iter().all(|error| error.about_the_source) {
            return Decline::new(detail);
        }
        Decline::new(format!("the lowering is not well-formed: {detail}"))
    })
}

/// drop anything that cannot be built, and keep dropping until it settles
///
/// a decline is not local. a function that calls a declined function has no
/// symbol to call, and one whose representations name a declined class has no
/// struct to point at — both are C compile errors, which would take the whole
/// module down. that is exactly what declining exists to prevent, so the decline
/// has to propagate to every dependent instead.
///
/// each round can only shrink the emitted set, so this terminates
///
/// `extends` is every class the module writes and the names its header extends,
/// including the ones nothing here will emit — a class left to the interpreted
/// definition still extends what it says it extends. `disturbed` is what the module body
/// does to its own definitions after making them, from [`disturbed_definitions`]
fn prune_unbuildable(
    module: &mut ModuleIr,
    ranges: &HashMap<String, (u32, u32)>,
    extends: &[(String, Vec<String>)],
    disturbed: &Disturbed,
) {
    // the name is the whole of what module init has to install under, and only an
    // exported definition is installed at all
    let rebound = |name: &str, exported: bool| {
        (exported && disturbed.rebound.contains(name)).then(|| {
            format!("`{name}` is rebound at module level, so installing this over it would replace what the rebind produced")
        })
    };
    // and what the body hangs on the definition afterwards has to survive the swap too.
    // `ctypes` writes `c_byte.__ctype_le__` there, and the adoption that carries a twin's
    // attributes leaves every dunder behind
    let hung_on = |name: &str, exported: bool| {
        exported
            .then(|| disturbed.dunder.get(name))
            .flatten()
            .map(|attribute| {
                format!(
                    "the module body writes `{attribute}` onto `{name}`, which the emitted type does not carry"
                )
            })
    };
    loop {
        let classes: HashSet<String> = module
            .classes
            .iter()
            .map(|class| class.name.clone())
            .collect();
        // a target is owner-qualified, so a method and a module-level function of
        // the same name resolve separately
        let targets: HashSet<String> = module
            .all_functions()
            .map(Function::qualified_name)
            .collect();
        let anchors = storage_anchors(module);

        let unbuildable = |function: &Function| -> Option<String> {
            // an edition is only ever reached from a call written against the name its
            // definition is installed under, so it is only the right call while that
            // definition is still what the module holds
            if let Some(coroutine_body) = &function.coroutine_body
                && !targets.contains(coroutine_body)
            {
                return Some(format!(
                    "`{coroutine_body}` declined, so the direct edition of it stands for nothing"
                ));
            }
            let representations = function
                .registers
                .iter()
                .map(|register| &register.ty)
                .chain(std::iter::once(&function.ret));
            for class in representations.flat_map(RType::instance_classes) {
                if !classes.contains(class) {
                    return Some(format!("`{class}` declined, so it has no layout"));
                }
            }
            for op in function.blocks.iter().flat_map(|block| &block.ops) {
                match op {
                    Op::CallNative { owner, callee, .. } => {
                        let target = qualify(owner.as_deref(), callee);
                        if !targets.contains(&target) {
                            return Some(format!("`{target}` declined, so a call has no target"));
                        }
                    }
                    Op::GetField { class, .. } | Op::SetField { class, .. }
                        if !classes.contains(class.as_str()) =>
                    {
                        return Some(format!("`{class}` declined, so it has no layout"));
                    }
                    _ => {}
                }
            }
            None
        };

        let mut fresh: Vec<Declined> = Vec::new();
        // a class goes as a unit: the native type object replaces the interpreted
        // class whole, so keeping it with one method missing would drop that
        // method from the module's surface
        module.classes.retain_mut(|class| {
            // a base this module meant to emit and did not is not a base at all, and
            // building on nothing in its place would quietly drop everything it brought
            //
            // unless this class brought no storage, and nothing under it did either. what
            // stands under the base's name at import is a class either way — the
            // interpreted definition, where the base declined — and building on the
            // *name* is what every class over a base out of this module already does.
            //
            // a class with fields has no such answer: its struct begins with the base's,
            // at offsets only the emitted base has. neither has one whose *subclass*
            // stores something, because rebuilding this class moves the whole chain's
            // layout outside the module, and that subclass's fields would go from inside
            // an instance to past one — which is a construction it has no answer for
            // either, and one that refuses the whole module at import rather than the
            // class. `urllib.request` lost every compiled definition it had that way
            let declined_base = class
                .base
                .as_ref()
                .and_then(ClassBase::in_module)
                .filter(|base| !classes.contains(*base))
                .map(str::to_owned)
                .and_then(|base| {
                    if class.fields.is_empty() && !anchors.contains(&class.name) {
                        class.base = Some(ClassBase::External(vec![base]));
                        return None;
                    }
                    Some(format!("`{base}` declined, so it is not a base to build on"))
                });
            // and the other way round. a class this module does not emit is still
            // built — by the interpreted definition, on whatever its base name
            // resolves to, which is the type emitted here. that is a subclass an
            // emitted type cannot have: a static type object refuses to be a base at
            // all, and the direct method call takes it that no override exists
            let interpreted_subclass = || {
                extends.iter().find_map(|(name, bases)| {
                    (!classes.contains(name) && bases.contains(&class.name)).then(|| {
                        format!(
                            "`{name}` declined, so it extends the interpreted definition rather than this type"
                        )
                    })
                })
            };
            match rebound(&class.name, class.exported)
                .or_else(|| hung_on(&class.name, class.exported))
                .or(declined_base)
                .or_else(interpreted_subclass)
                .or_else(|| class.methods.iter().find_map(&unbuildable))
            {
                Some(reason) => {
                    fresh.push(Declined {
                        name: class.name.clone(),
                        reason,
                        range: ranges.get(&class.name).copied(),
                    });
                    false
                }
                None => true,
            }
        });
        // an edition dropping is not a decline anyone wrote: the definition it is an
        // edition of reports one of its own, and a second entry under a name the source
        // never held would inflate every count taken off this list. it still has to keep
        // the loop going, because a caller of it is now unbuildable too
        let mut dropped_an_edition = false;
        module.functions.retain(|function| {
            match rebound(&function.name, function.exported).or_else(|| unbuildable(function)) {
                Some(reason) => {
                    if function.coroutine_body.is_some() {
                        dropped_an_edition = true;
                    } else {
                        fresh.push(Declined {
                            name: function.name.clone(),
                            reason,
                            range: ranges.get(&function.name).copied(),
                        });
                    }
                    false
                }
                None => true,
            }
        });

        if fresh.is_empty() && !dropped_an_edition {
            return;
        }
        module.declined.extend(fresh);
    }
}

/// every class an emitted one keeps storage inside an instance of
///
/// a class's struct begins with the fields of every class this module writes above it, so
/// where any of those stops being a layout of ours the storage stops being *inside* the
/// instance and starts sitting past one. these are the classes that cannot be moved
fn storage_anchors(module: &ModuleIr) -> HashSet<String> {
    let mut anchors = HashSet::new();
    for class in &module.classes {
        if class.fields.is_empty() {
            continue;
        }
        let mut current = class;
        // bounded by the class count: a base chain cannot visit one twice without being a
        // cycle, and a cycle would otherwise spin here rather than settle
        for _ in 0..=module.classes.len() {
            let Some(base) = current.base.as_ref().and_then(ClassBase::in_module) else {
                break;
            };
            anchors.insert(base.to_string());
            match module.classes.iter().find(|other| other.name == base) {
                Some(next) => current = next,
                None => break,
            }
        }
    }
    anchors
}

/// what declined
/// the places in `function` a representation was available at but for the promotion
///
/// the parameters and the locals: those are where a buffer or a `double` would have
/// been chosen, and where the report can name something a user wrote
fn promoted_places(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
    layouts: &Layouts,
) -> Vec<by_ir::function::PromotedPlace> {
    let mut places: Vec<by_ir::function::PromotedPlace> = Vec::new();
    let record = |places: &mut Vec<_>, place: String, ty| {
        if let Some(missed) = mapper::missed_representation(db, env, ty, layouts) {
            places.push(by_ir::function::PromotedPlace {
                function: function.name.to_string(),
                place,
                missed,
            });
        }
    };
    for parameter in function
        .parameters
        .posonlyargs
        .iter()
        .chain(function.parameters.args.iter())
        .chain(function.parameters.kwonlyargs.iter())
    {
        if let Some(ty) = parameter.parameter.inferred_type(model) {
            record(&mut places, parameter.parameter.name.to_string(), ty);
        }
    }
    for stmt in walk(&function.body) {
        let Stmt::Assign(node) = stmt else { continue };
        let [Expr::Name(name)] = node.targets.as_slice() else {
            continue;
        };
        if places.iter().any(|place| place.place == name.id.as_str()) {
            continue;
        }
        if let Some(ty) = name.inferred_type(model) {
            record(&mut places, name.id.to_string(), ty);
        }
    }
    places
}

fn gradual_signature_places(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
) -> Vec<GradualUse> {
    // the same "proves nothing" test the mapper decides a representation with: a gradual
    // member answers for the whole type, and so does the gradual bound of the hole an
    // unannotated parameter opens
    let is_gradual =
        |ty: ty_python_semantic::types::Type<'_>| ty.is_dynamic() || ty.has_gradual_member(db, env);
    let mut places = Vec::new();
    for parameter in &function.parameters.args {
        if parameter
            .parameter
            .inferred_type(model)
            .is_some_and(is_gradual)
        {
            places.push(GradualUse {
                function: function.name.to_string(),
                place: parameter.parameter.name.to_string(),
            });
        }
    }
    // the return is gradual when any `return` produces a gradual value
    for stmt in walk(&function.body) {
        if let Stmt::Return(ret) = stmt
            && let Some(value) = &ret.value
            && value.inferred_type(model).is_some_and(is_gradual)
        {
            places.push(GradualUse {
                function: function.name.to_string(),
                place: "return".to_string(),
            });
            break;
        }
    }
    places
}

/// a class with a fixed instance layout
///
/// every attribute is declared in the class body with a type, and the generated
/// `__init__` writes all of them — which is what makes them *always defined*, so
/// no per-read check and no definedness bitfield.
///
/// returns the class *and* the closure environments its methods needed: an environment
/// is a sibling class, not something nested in the one whose method made it, so the
/// module has to collect both or the layout those methods reference is never emitted
fn lower_class<'a>(
    unit: Unit<'a>,
    class: &'a ast::StmtClassDef,
) -> Lowered<(by_ir::function::ClassIr, Vec<by_ir::function::ClassIr>)> {
    let Unit {
        env,
        db,
        model,
        suite,
        layouts,
        deleted,
        ..
    } = unit;
    // every frame written inside this body mangles a private name against this class,
    // including one nested inside a method
    let unit = Unit {
        owner: Some(&class.name),
        ..unit
    };
    // `data class` reaches the AST as a marker decorator, and it is the only
    // class form with a field-initializing constructor — which is exactly what a
    // fixed layout needs. a plain class with bare annotations has no constructor
    // in the interpreted build either, so compiling one would invent behaviour
    let (is_data, class_decorators) = class_modifiers(db, model, class)?;
    let base = base_class(db, env, model, suite, class, layouts)?;

    let fields = class_fields(db, env, model, suite, class, layouts, deleted)?;
    // the `@property` pairs, worked out before any method is lowered: the two halves of
    // one are both written `def value`, so a pass that took them one at a time would see
    // a name defined twice rather than the single attribute python builds out of them
    let properties = property_groups(db, model, class)?;
    let immutable = class.decorator_list.iter().any(|decorator| {
        matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == "frozen_data_class")
    });
    // a `frozen data class` publishes no setter at all, the way `@dataclass(frozen=True)`
    // has none — so a property that writes would be the one way an attribute of one could
    // still change
    if immutable
        && properties
            .iter()
            .any(|group| group.setter.is_some() || group.deleter.is_some())
    {
        return Err(Decline::new(
            "a property that writes stands in a frozen class, which publishes no setters",
        ));
    }
    let mut lowered = Vec::new();
    let mut environments = Vec::new();
    let mut constants = Vec::new();
    let mut slot_aliases = Vec::new();
    let mut published = Vec::new();
    for group in &properties {
        let mut published_group = by_ir::function::PropertyIr {
            name: mangled(Some(&class.name), group.name),
            getter: None,
            setter: None,
            deleter: None,
        };
        for (half, accessor) in group.halves() {
            let (method, produced) = lower_accessor(unit, accessor, &class.name, half)?;
            *match half {
                Half::Get => &mut published_group.getter,
                Half::Set => &mut published_group.setter,
                Half::Delete => &mut published_group.deleter,
            } = Some(method.name.clone());
            lowered.push(method);
            environments.extend(produced);
        }
        published.push(published_group);
    }
    for statement in &class.body {
        match statement {
            // an annotation is not a binding, but an annotated *assignment* is the
            // same one a plain assignment makes — the annotation only adds an entry
            // to `__annotations__`, and the value lands in the class dict either way.
            // in a `data class` the annotations are the fields instead, and the
            // layout has taken them already
            Stmt::AnnAssign(node) if !is_data && node.value.is_some() => {
                match (node.target.as_ref(), node.value.as_deref()) {
                    (Expr::Name(name), Some(value)) => {
                        slot_aliases.extend(assigned_slot(name.id.as_str(), value)?);
                        constants.push(mangled(Some(&class.name), name.id.as_str()));
                    }
                    _ => return Err(Decline::new("only a plain class-level name is lowered yet")),
                }
            }
            Stmt::AnnAssign(_) => {}
            // a property's halves were lowered above, as the one attribute they are
            Stmt::FunctionDef(method)
                if properties
                    .iter()
                    .any(|group| group.halves().any(|(_, half)| std::ptr::eq(half, method))) => {}
            Stmt::FunctionDef(method) => {
                // a `data class` has its `__init__` generated from the fields, so a
                // hand-written one would disagree with it. a plain class *is* its
                // `__init__`, and every other dunder fills a type slot the method
                // table does not reach
                let own_init = !is_data && method.name.as_str() == "__init__";
                let slotted = has_a_slot_adapter(method.name.as_str());
                if method.name.as_str() == "__getattr__" {
                    getattr_hook_stands_alone(base.as_ref())?;
                }
                if method.name.as_str() == "__new__" {
                    new_slot_is_adaptable(db, env, model, suite, base.as_ref(), layouts)?;
                }
                if method.name.starts_with("__")
                    && !own_init
                    && !slotted
                    && fills_a_type_slot(method.name.as_str())
                {
                    return Err(Decline::new(format!(
                        "`{}` fills a type slot with no adapter yet",
                        method.name
                    )));
                }
                // every decorator is resolved out of the module namespace at init, so
                // one rooted at a name the *class body* bound is looked up somewhere it
                // does not exist. `@fieldnames.setter` is the shape this turns down, and
                // it turns down the whole class: two `def`s of one name would otherwise
                // become two entries in one method table
                if let Some(rooted) = method.decorator_list.iter().find_map(|decorator| {
                    let path = function_modifier(db, model, decorator).ok()?;
                    let Modifier::Apply(path) = path else {
                        return None;
                    };
                    class_body_binds(&class.body, path.root()).then(|| path.root().to_string())
                }) {
                    return Err(Decline::new(format!(
                        "`{rooted}` is bound by the class body, and a decorator is resolved out of the module namespace at init"
                    )));
                }
                defined_once(&class.body, method)?;
                let (method, produced) = lower_method(unit, method, &class.name)?;
                if method.name == "__new__" {
                    new_answers_its_own_class(&method, &class.name)?;
                }
                lowered.push(method);
                environments.extend(produced);
            }
            // a class-level constant: whatever it is, the interpreted definition
            // evaluated it already and module init copies it across
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    match target {
                        // a dunder that fills a type slot needs one emitted alongside the
                        // copy. the copy writes into `tp_dict`, and a name there does not
                        // fill a slot — so `__repr__ = _repr` on its own leaves `repr(x)`
                        // going to the slot python inherited while `x.__repr__()` answers
                        // the assignment, which is two answers where the interpreted class
                        // has one.
                        //
                        // `a = b = value` writes the one value under both names, so each
                        // is asked the same question the single-target form asks
                        Expr::Name(name) => {
                            slot_aliases.extend(assigned_slot(name.id.as_str(), &assign.value)?);
                            constants.push(mangled(Some(&class.name), name.id.as_str()));
                        }
                        // `dispatch[PROTO[0]] = load_proto` binds no name in the class
                        // namespace — it writes into an object the body already built. the
                        // interpreted definition made that write before module init reads
                        // the body dict, so the object copied across is already finished
                        Expr::Subscript(_) => {}
                        _ => {
                            return Err(Decline::new(
                                "only a plain class-level name is lowered yet",
                            ));
                        }
                    }
                }
            }
            // python runs a class body as an ordinary block, so a conditional or a loop
            // in one is an ordinary statement and what it binds lands in the class
            // namespace like anything else:
            //
            //     class _Pickler:
            //         if _HAVE_PICKLE_BUFFER:
            //             def save_picklebuffer(self, obj): ...
            //
            // nothing here evaluates the condition. the interpreted definition ran the
            // block already — once, where python runs it, with whatever the condition
            // read and whatever it did on the way — and the namespace it left is the
            // answer. so every name the block *could* bind becomes a class-level
            // constant, taken off that namespace by the copy every other constant takes.
            // where the condition was false the body never wrote the name, nothing is
            // copied, and the emitted class has no such attribute either — which is what
            // `hasattr` has to say about it
            Stmt::If(_) | Stmt::For(_) | Stmt::While(_) => {
                let mut names = Vec::new();
                nested_bindings(std::slice::from_ref(statement), &mut names)?;
                for name in names {
                    nested_binding_stands_alone(&class.body, name)?;
                    let name = mangled(Some(&class.name), name);
                    if !constants.contains(&name) {
                        constants.push(name);
                    }
                }
            }
            Stmt::Pass(_) => {}
            // a docstring
            Stmt::Expr(node) if matches!(node.value.as_ref(), Expr::StringLiteral(_)) => {}
            _ => return Err(Decline::new("only fields and methods are lowered yet")),
        }
    }

    // the two questions a metaclass construction raises are asked while the layouts
    // settle, in `metaclass_carries_the_body`, so that a class they turn down leaves the
    // layout set rather than sitting in it as a base nothing emits
    let keywords = class_keywords(class)?;
    // a constant is copied into the type's dict, and a field's descriptor is already
    // sitting there under its own name — so a name that is both would answer with the
    // class-level value for an instance that has one of its own
    if let Some(clash) = constants
        .iter()
        .find(|name| fields.iter().any(|field| field.name == **name))
    {
        return Err(Decline::new(format!(
            "`{clash}` is both a class-level constant and a field"
        )));
    }
    // the same question the `def` above asks, for a body that assigned the hook instead
    if slot_aliases.iter().any(|alias| alias.name == "__getattr__") {
        getattr_hook_stands_alone(base.as_ref())?;
    }
    // a slot has one filler. a name written both ways would put the assignment's value
    // over the method's descriptor in the type's dict while the slot still called the
    // method, which is the two answers this whole path exists to prevent
    if let Some(clash) = slot_aliases
        .iter()
        .find(|alias| lowered.iter().any(|method| method.name == alias.name))
    {
        return Err(Decline::new(format!(
            "`{}` is both defined and assigned, and its type slot has room for one",
            clash.name
        )));
    }
    // a property publishes its attribute through `tp_getset`, and a field's descriptor is
    // already sitting under its own name there — the same collision a class-level
    // constant has with one, and the same answer
    if let Some(clash) = published.iter().find(|property| {
        fields.iter().any(|field| field.name == property.name)
            || constants.contains(&property.name)
            || slot_aliases.iter().any(|alias| alias.name == property.name)
    }) {
        return Err(Decline::new(format!(
            "`{}` is both a property and a name the class body binds directly",
            clash.name
        )));
    }
    // a property's halves are named with a `$`, which no source can write — but the
    // symbol they mangle to replaces it with an underscore, and a method a source *did*
    // write may already hold that symbol. two bodies under one symbol is a module that
    // does not build at all, which is worse than one that declines
    if let Some(clash) = published
        .iter()
        .flat_map(|property| [&property.getter, &property.setter, &property.deleter])
        .flatten()
        .find(|accessor| {
            lowered
                .iter()
                .any(|method| method.name != **accessor && mangles_alike(&method.name, accessor))
        })
    {
        return Err(Decline::new(format!(
            "a property half named `{clash}` reaches the same symbol as a method of this class"
        )));
    }
    // this class's own decorators are applied at init and taken out of the twin's source,
    // so the module body must not reach the class in the window between the twin's
    // `class` statement and init — everything it bound in that window keeps the
    // definition nothing had decorated yet
    decorator_stays_unread(unit.read, class.name.as_str(), &class_decorators)?;
    Ok((
        by_ir::function::ClassIr {
            resume: None,
            exported: true,
            name: class.name.to_string(),
            immutable,
            base,
            // neither written nor generated: a `data class` always gets one, and a
            // written `__init__` is a method of its own
            inherited_init: !is_data
                && !class.body.iter().any(|statement| {
                    matches!(statement, Stmt::FunctionDef(method) if method.name.as_str() == "__init__")
                }),
            fields,
            decorators: class_decorators,
            generic: class.type_params.is_some(),
            declares_slots: declared_slots(class).is_some(),
            constants,
            slot_aliases,
            methods: lowered,
            properties: published,
            keywords,
        },
        environments,
    ))
}

/// whether two python names reach one C identifier
///
/// a symbol is the name with every character that is not alphanumeric replaced, so
/// `value$set` and `value_set` arrive at the same one. a property's halves are named
/// with a `$` no source can write, and this is what keeps that from being the only thing
/// standing between them and a method a source did write
fn mangles_alike(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left.chars().zip(right.chars()).all(|(left, right)| {
            left == right || (!left.is_ascii_alphanumeric() && !right.is_ascii_alphanumeric())
        })
}

/// `__getattr__` stands *behind* the ordinary lookup rather than replacing it
///
/// the hook runs the ordinary lookup first and falls back to the method only where that
/// raised. what the ordinary one is, is the base's answer — so this is lowered where the
/// base is `object`, whose answer is the generic lookup the adapter runs
fn getattr_hook_stands_alone(base: Option<&ClassBase>) -> Lowered<()> {
    if base.is_some() {
        return Err(Decline::new(
            "`__getattr__` falls back from the lookup a base may have replaced, and this class extends one",
        ));
    }
    Ok(())
}

/// whether the emitter can publish this class's written `__new__`
///
/// the method itself needs nothing special: it is bound onto the finished type as the
/// static method python makes of it, and python's own construction runs it and decides
/// from what it answers whether `__init__` runs at all. that is what makes an interning
/// `__new__`, one answering with a different class, and one answering a cached object all
/// come out right without the emitter knowing which it is looking at.
///
/// what it needs is for the class to allocate its own instances. a class standing on a
/// base outside this module has no layout of its own — `str`, `tuple` and `int` each
/// allocate an instance whose size only that base knows — and the written `__new__` reaches
/// that by calling the base's, through a subclass check that reads the emitted type as the
/// allocator it is not
fn new_slot_is_adaptable(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    base: Option<&ClassBase>,
    layouts: &Layouts,
) -> Lowered<()> {
    let outside = match base {
        None => false,
        Some(ClassBase::External(_)) => true,
        Some(ClassBase::InModule(name)) => {
            laid_out_from_outside(db, env, model, suite, layouts, name)
        }
    };
    if outside {
        return Err(Decline::new(
            "a `__new__` allocates, and this class takes its instance layout from a base outside this module — only that base knows how big one is",
        ));
    }
    Ok(())
}

/// whether a written `__new__` is one whose answer a construction can be compiled against
///
/// python lets `__new__` answer with anything, and the checker does not follow it: every
/// `C(...)` in this module is compiled believing the answer is a `C`. so a `__new__` whose
/// answer is *known* to be some other class of this module's would hand every construction
/// an object of a shape it was compiled not to expect, and the boundary check that catches
/// that raises where the interpreted class simply hands the object over.
///
/// an answer nothing is known about is a different case and stays: it is the ordinary
/// gradual one, and the same boundary check stands behind it
fn new_answers_its_own_class(new: &Function, class: &str) -> Lowered<()> {
    if let RType::Instance {
        class: answered, ..
    } = &new.ret
        && answered != class
    {
        return Err(Decline::new(format!(
            "`__new__` answers a `{answered}`, and a construction of `{class}` is compiled against the `{class}` the checker reports"
        )));
    }
    Ok(())
}

/// the type slot a class-level assignment to `name` has to fill, where it fills one
///
/// python reads `tp_repr` for `repr(x)` and never consults the name, so a class writing
/// `__repr__ = _repr` would otherwise answer twice: the inherited slot for `repr(x)` and
/// the assignment for `x.__repr__()`. `optparse.Option` and `http.cookies.BaseCookie` are
/// the shape, and both were silently answering `object.__repr__`. the emitter fills the
/// slot from the assigned value where it has an adapter, and this declines where it does
/// not — the same question [`has_a_slot_adapter`] settles for a `def`
fn assigned_slot(name: &str, value: &Expr) -> Lowered<Option<SlotAlias>> {
    if !fills_a_type_slot(name) {
        return Ok(None);
    }
    // `__hash__ = None` names nothing to call: it is how python says an instance cannot
    // be hashed at all, and `tp_hash` has a standing value for exactly that. no other
    // slot does, so `None` anywhere else is turning an operation off in a way the
    // emitter cannot reproduce
    if value.is_none_literal_expr() {
        if name == "__hash__" {
            return Ok(Some(SlotAlias {
                name: name.to_string(),
                unsupported: true,
            }));
        }
        return Err(Decline::new(format!(
            "`{name} = None` turns a type slot off, and only `__hash__` has a standing value for that"
        )));
    }
    // a `def __new__` is reached through the slot alone, so the emitter never has to say
    // what its own name is bound to. an *assignment* is the other way round: python wraps
    // whatever was assigned in a `staticmethod` and the class dict keeps it, so the name
    // and the slot have to agree — and the cell a slot alias reads is filled from a dict
    // entry `PyType_Ready` has already replaced with the wrapper around `tp_new`
    if name == "__new__" {
        return Err(Decline::new(
            "`__new__ = ...` binds the name python fills from `tp_new`, so the assignment and the slot would answer differently",
        ));
    }
    if !has_a_slot_adapter(name) {
        return Err(Decline::new(format!(
            "`{name}` fills a type slot with no adapter yet"
        )));
    }
    Ok(Some(SlotAlias {
        name: name.to_string(),
        unsupported: false,
    }))
}

/// the parameter a call leaves in slot zero, which for a method is the receiver
///
/// a `/` makes the receiver positional-only along with everything before it, and
/// python keeps those in a list of their own — so the first *ordinary* parameter is
/// not slot zero, it is whatever the marker was written after
fn slot_zero(parameters: &ast::Parameters) -> Option<&ast::ParameterWithDefault> {
    parameters
        .posonlyargs
        .first()
        .or_else(|| parameters.args.first())
}

/// the instance methods of a class, each with the name it calls its receiver
///
/// `staticmethod` and `classmethod` are left out because slot zero is not an instance
/// for either: a `classmethod` writing `cls.x` is giving the *class* an attribute, and
/// recording that as a field would give every instance storage the source never asked
/// for. only a bare name says which convention, exactly as [`method_binding`] reads it —
/// `abc.abstractmethod` is not one however its last segment reads
fn instance_methods(class: &ast::StmtClassDef) -> Vec<(&ast::StmtFunctionDef, &str)> {
    class
        .body
        .iter()
        .filter_map(|statement| {
            let Stmt::FunctionDef(method) = statement else {
                return None;
            };
            let convention = method.decorator_list.iter().any(|decorator| {
                matches!(&decorator.expression, Expr::Name(name)
                    if matches!(name.id.as_str(), "staticmethod" | "classmethod"))
            });
            if convention {
                return None;
            }
            Some((
                method,
                slot_zero(&method.parameters)?.parameter.name.as_str(),
            ))
        })
        .collect()
}

/// the attributes of `receiver` one assignment *target* writes, with the sub-expression
/// each is written through
///
/// a target is a tree rather than a name: `self.a, (self.b, *self.rest) = xs` writes
/// three attributes, and every one of them has to reach the layout. an attribute the
/// layout never hears about is not simply missed — the write falls through to the dynamic
/// form, and an emitted instance has no `__dict__` for that to land in, so it raises where
/// the interpreted class stored a value
fn target_attributes<'a>(
    target: &'a Expr,
    receiver: &str,
    owner: Option<&str>,
) -> Lowered<Vec<(String, &'a Expr)>> {
    match target {
        // `self.a[i] = v` and `self.a.b = v` write through the attribute rather than to
        // it, so the layout hears nothing new — but the read they begin with does have to
        // find it, which is the same field the assignment that put it there declared
        Expr::Name(_) | Expr::Subscript(_) => Ok(Vec::new()),
        Expr::Attribute(_) => match attribute_of(target, receiver, owner) {
            Some(name) => Ok(vec![(name, target)]),
            None => Ok(Vec::new()),
        },
        Expr::Starred(starred) => target_attributes(&starred.value, receiver, owner),
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            let mut out = Vec::new();
            for element in elts {
                out.extend(target_attributes(element, receiver, owner)?);
            }
            Ok(out)
        }
        // python has no other target form, so nothing reaches here — and a form that
        // did would be one the layout had not been shown, which is the shape of the
        // bug this whole walk exists to rule out
        other => Err(Decline::new(format!(
            "{other:?} is not an assignment target the layout can read"
        ))),
    }
}

/// the attributes of `receiver` one *statement* writes
///
/// every statement that binds a name binds an attribute the same way, so all six forms
/// are asked here rather than only the two an assignment uses. `for self.item in xs` and
/// `with open(p) as self.file` each give the instance an attribute exactly as
/// `self.item = x` does, and each was silently invisible to the layout
///
/// this does not descend into a nested body of its own: the caller walks those, and
/// which of them are *definite* is a separate question — see [`completing_assignments`]
fn receiver_writes<'a>(
    statement: &'a Stmt,
    receiver: &str,
    owner: Option<&str>,
) -> Lowered<Vec<(String, &'a Expr)>> {
    let mut out = Vec::new();
    match statement {
        // a chained assignment binds every target to the one value
        Stmt::Assign(node) => {
            for target in &node.targets {
                out.extend(target_attributes(target, receiver, owner)?);
            }
        }
        // an annotation with no value declares rather than assigns
        Stmt::AnnAssign(node) if node.value.is_some() => {
            out.extend(target_attributes(&node.target, receiver, owner)?);
        }
        // an augmented assignment reads the attribute and writes it back, so the
        // layout has to hold it for either half to work
        Stmt::AugAssign(node) => out.extend(target_attributes(&node.target, receiver, owner)?),
        Stmt::For(node) => out.extend(target_attributes(&node.target, receiver, owner)?),
        Stmt::With(node) => {
            for item in &node.items {
                if let Some(bound) = &item.optional_vars {
                    out.extend(target_attributes(bound, receiver, owner)?);
                }
            }
        }
        _ => {}
    }
    // an emitted instance *is* its layout: `__dict__` is not a field it could be given,
    // because the namespace it stands for is the thing an emitted class does not have.
    // `__weakref__` is the same answer — a type spec adds neither, which is what
    // `slot_fields` already tells a `__slots__` that asks for one
    if let Some((name, _)) = out
        .iter()
        .find(|(name, _)| matches!(name.as_str(), "__dict__" | "__weakref__"))
    {
        return Err(Decline::new(format!(
            "`{name}` is written on the receiver, and an emitted instance is its layout with nothing behind it"
        )));
    }
    Ok(out)
}

/// every statement a body contains, however deeply — the bodies of nested `def`s and
/// `class`es included
///
/// [`walk`] and [`walk_with_cases`] deliberately stop at a nested definition, because
/// what a nested frame binds is not what this one binds. a question about the *values* a
/// frame reaches has no such boundary: a closure reads the enclosing frame's names, so a
/// write it makes is a write this frame's receiver sees
fn every_statement(body: &[Stmt]) -> Vec<&Stmt> {
    struct Collect<'a>(Vec<&'a Stmt>);
    impl<'a> ruff_python_ast::visitor::Visitor<'a> for Collect<'a> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            self.0.push(stmt);
            ruff_python_ast::visitor::walk_stmt(self, stmt);
        }
    }
    let mut collect = Collect(Vec::new());
    for stmt in body {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut collect, stmt);
    }
    collect.0
}

/// a `classmethod` that binds an attribute on the class it is handed
///
/// slot zero of a `classmethod` holds the emitted *type*, and an emitted type is sealed:
/// immutable to `setattr`, so `cls.x = 1` raises `TypeError: cannot set attribute of
/// immutable type` where python binds a class attribute. unsealing it is not the answer —
/// a write into `tp_dict` would replace the descriptor a field publishes there, and every
/// instance would read the class's value instead of its own.
///
/// so the class declines, and declining is enough: a method's decline takes its class
/// with it, and the interpreted class python is then left with takes the write exactly as
/// the source meant it.
///
/// a function nested inside the method counts, because it captures the same class
/// object — so the walk reaches every body, not only this one. a nested `def` that binds
/// the name itself is a name of its own and not the class at all, but declining it too
/// costs a class python could have had rather than giving one an answer it never gives
fn class_object_is_not_written(
    function: &ast::StmtFunctionDef,
    owner: Option<&str>,
) -> Lowered<()> {
    let Some(parameter) = slot_zero(&function.parameters) else {
        return Ok(());
    };
    let receiver = parameter.parameter.name.as_str();
    let refuse = |name: &str| {
        Err(Decline::new(format!(
            "`{receiver}.{name}` binds an attribute on the class, and the type this \
             module emits for it is sealed"
        )))
    };
    for statement in every_statement(&function.body) {
        if let Some((name, _)) = receiver_writes(statement, receiver, owner)?.first() {
            return refuse(name);
        }
        // `del cls.x` reaches the same sealed type, and unbinds a class attribute where
        // python unbinds one
        if let Stmt::Delete(node) = statement {
            for target in &node.targets {
                if let Expr::Attribute(attribute) = target
                    && matches!(attribute.value.as_ref(), Expr::Name(name) if name.id == *receiver)
                {
                    return refuse(&attribute.attr);
                }
            }
        }
    }
    Ok(())
}

/// as [`receiver_writes`], keeping only the writes that are *certain* to have happened
/// once the statement is past
///
/// a `for` target is bound once per iteration, so an empty iterable binds it never. that
/// leaves the loop saying only that the attribute exists somewhere in the class, which is
/// the optional field with a presence byte beside it that the width pass gives it. every
/// other form here runs exactly once — a `with` binds what `__enter__` handed back before
/// its body starts, and the body of a `with` is not a body that may be skipped
fn certain_writes<'a>(
    statement: &'a Stmt,
    receiver: &str,
    owner: Option<&str>,
) -> Lowered<Vec<(String, &'a Expr)>> {
    if matches!(statement, Stmt::For(_)) {
        return Ok(Vec::new());
    }
    receiver_writes(statement, receiver, owner)
}

/// the fields of a plain class: the attributes its body gives the instance
///
/// a fixed layout needs every field to exist by the time anything can read one,
/// which for a plain class means `__init__` assigns it *unconditionally*. an
/// assignment inside a branch or a loop leaves the question open, so the field takes a
/// presence byte beside it rather than the class inventing an answer — python raises
/// `AttributeError` there, and a struct field has no other way to be absent.
///
/// every write reaches here, whatever statement made it, because the layout is the only
/// place an attribute can go: one it never heard about is lowered as the dynamic form and
/// lands nowhere at all
fn init_fields(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
    layouts: &Layouts,
    mut fields: Vec<by_ir::function::FieldDecl>,
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    let methods = instance_methods(class);
    if class.body.iter().any(|statement| {
        matches!(statement, Stmt::FunctionDef(function)
            if function.name.as_str() == "__init__"
                && slot_zero(&function.parameters).is_none())
    }) {
        return Err(Decline::new("`__init__` takes no receiver"));
    }

    // `self.value = x` where the body binds `value` with `@property` is not a write to
    // the instance at all: it goes through the descriptor, which is what runs the
    // setter's body. a field of that name would sit beside the property, take the write
    // the descriptor was meant to have, and leave the setter never running —
    // `logging.Manager.__init__` writes `self.disable = 0` and that is where `_checkLevel`
    // would have been skipped
    let properties = property_names(db, model, class);

    // the representation a field needs has to cover *every* write to it, not only
    // the one in `__init__`: a method assigning a wider value would otherwise be
    // storing something the struct cannot hold
    let mut widths: Vec<(String, RType)> = Vec::new();
    for (method, receiver) in &methods {
        for statement in walk_with_cases(&method.body) {
            for (name, target) in receiver_writes(statement, receiver, Some(&class.name))? {
                if properties.contains(&name) {
                    continue;
                }
                let ty = target
                    .inferred_type(model)
                    .ok_or_else(|| Decline::new("an attribute assignment has no inferred type"))?;
                let rtype = map_type_with(db, env, ty, layouts)?;
                match widths.iter_mut().find(|(written, _)| *written == name) {
                    Some((_, existing)) => {
                        if *existing != rtype {
                            *existing = RType::OBJECT;
                        }
                    }
                    None => widths.push((name, rtype)),
                }
            }
        }
    }

    // a class with no `__init__` gives the instance nothing at construction, so every
    // attribute it has is one some later method wrote — which is the optional field the
    // last pass below declares. it is not the same as having no *layout*: a class of
    // methods has an empty one, which is as representable as any other
    let Some((init, receiver)) = methods
        .iter()
        .find(|(method, _)| method.name.as_str() == "__init__")
    else {
        for (name, ty) in &widths {
            if fields.iter().any(|field| field.name == *name) {
                continue;
            }
            fields.push(by_ir::function::FieldDecl {
                name: name.clone(),
                ty: ty.clone(),
                default: None,
                optional: true,
            });
        }
        return Ok(fields);
    };

    // an attribute every path through `__init__` assigns is as much a field as one
    // assigned at the top: the layout only needs to know it is always there
    let definite = definitely_assigned_attributes(&init.body, receiver, Some(&class.name));
    for statement in &init.body {
        for (name, _) in certain_writes(statement, receiver, Some(&class.name))? {
            if properties.contains(&name) || fields.iter().any(|field| field.name == name) {
                continue;
            }
            let ty = widths
                .iter()
                .find(|(written, _)| *written == name)
                .map(|(_, rtype)| rtype.clone())
                .ok_or_else(|| Decline::new("an attribute assignment has no representation"))?;
            fields.push(by_ir::function::FieldDecl {
                name,
                ty,
                default: None,
                optional: false,
            });
        }
    }

    // the ones assigned on every path but not at the top come next, in the order the
    // widths found them, so the layout is deterministic
    for (name, ty) in &widths {
        if fields.iter().any(|field| field.name == *name) || !definite.contains(name) {
            continue;
        }
        fields.push(by_ir::function::FieldDecl {
            name: name.clone(),
            ty: ty.clone(),
            default: None,
            optional: false,
        });
    }

    // an attribute only *some* paths through `__init__` assign still has a place in
    // the layout — it just needs one more byte saying whether it was written, because
    // reading it on a path that skipped it is an `AttributeError` rather than a value
    for (name, ty) in &widths {
        if fields.iter().any(|field| field.name == *name) {
            continue;
        }
        fields.push(by_ir::function::FieldDecl {
            name: name.clone(),
            ty: ty.clone(),
            default: None,
            optional: true,
        });
    }
    Ok(fields)
}

/// the value a class body writes `__slots__` as, where it writes one
///
/// an annotated assignment binds the name exactly as a plain one does, so both count.
/// the body is read backwards because the declaration is the *namespace entry* the class
/// statement ends with, which a second binding would have replaced
fn declared_slots(class: &ast::StmtClassDef) -> Option<&Expr> {
    class
        .body
        .iter()
        .rev()
        .find_map(|statement| match statement {
            Stmt::Assign(node) => match node.targets.as_slice() {
                [Expr::Name(name)] if name.id.as_str() == "__slots__" => Some(node.value.as_ref()),
                _ => None,
            },
            Stmt::AnnAssign(node) => match node.target.as_ref() {
                Expr::Name(name) if name.id.as_str() == "__slots__" => node.value.as_deref(),
                _ => None,
            },
            _ => None,
        })
}

/// the attribute names a `__slots__` value declares
///
/// a bare string declares one, and an iterable of strings declares each. python accepts
/// any iterable, including one nothing here can read — a generator, a name from
/// elsewhere — and the names are what the layout is, so anything else declines
fn slot_names(value: &Expr) -> Lowered<Vec<&str>> {
    let entries = match value {
        Expr::StringLiteral(literal) => return Ok(vec![literal.value.to_str()]),
        Expr::Tuple(tuple) => &tuple.elts,
        Expr::List(list) => &list.elts,
        _ => {
            return Err(Decline::new("`__slots__` names its attributes at runtime"));
        }
    };
    entries
        .iter()
        .map(|entry| match entry {
            Expr::StringLiteral(literal) => Ok(literal.value.to_str()),
            _ => Err(Decline::new("a `__slots__` entry is not a literal name")),
        })
        .collect()
}

/// the fields a `__slots__` declares that nothing in the class body assigns
///
/// `__slots__` is copied onto the emitted type like every other class-level constant, so
/// the type advertises storage python would have made descriptors for. what python makes
/// is an attribute the instance always has room for and may not have written yet — which
/// is exactly the field an assignment on only some paths already gets: a byte beside it
/// saying whether it was written, a read that answers `AttributeError` while it says no,
/// and no way to reach any name the declaration left out
fn slot_fields(
    class: &ast::StmtClassDef,
    mut fields: Vec<by_ir::function::FieldDecl>,
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    let Some(value) = declared_slots(class) else {
        return Ok(fields);
    };
    for name in slot_names(value)? {
        // neither is storage of the instance's own: they ask the *type* for a dict and
        // for weakref support, and a spec adds neither
        if matches!(name, "__dict__" | "__weakref__") {
            return Err(Decline::new(format!(
                "`__slots__` asks for `{name}`, which a type spec cannot add"
            )));
        }
        if !is_identifier(name) {
            return Err(Decline::new("a `__slots__` entry is not an identifier"));
        }
        // a slot name is mangled against the class body that wrote it, like every other
        // private name — `__x` in `__slots__` is what `self.__x` reaches
        let name = mangled(Some(&class.name), name);
        // an assignment already gave it storage, and one derived from a write knows the
        // representation the value has rather than erasing it
        if fields.iter().any(|field| field.name == name) {
            continue;
        }
        fields.push(by_ir::function::FieldDecl {
            name,
            ty: RType::OBJECT,
            default: None,
            optional: true,
        });
    }
    Ok(fields)
}

/// the name python binds an identifier under, given the class body it was written in
///
/// this is `_Py_Mangle`: an identifier of two leading underscores and not two trailing
/// ones, written anywhere in the body of `class C`, is bound and read as `_C__spam`,
/// with the class's own leading underscores stripped. reading the written name instead
/// publishes an attribute under a name python never uses — `symtable.Function` lost
/// `_Function__params` that way, and every method whose name starts `__` with it.
///
/// `owner` is `None` outside a class body, where nothing is mangled
fn mangled(owner: Option<&str>, written: &str) -> String {
    let Some(owner) = owner else {
        return written.to_string();
    };
    if !written.starts_with("__") || written.ends_with("__") {
        return written.to_string();
    }
    let stripped = owner.trim_start_matches('_');
    if stripped.is_empty() {
        return written.to_string();
    }
    format!("_{stripped}{written}")
}

/// the attribute name in `<receiver>.<name>`, when the target is one, under the name
/// the class body binds it as
fn attribute_of(target: &Expr, receiver: &str, owner: Option<&str>) -> Option<String> {
    let Expr::Attribute(attribute) = target else {
        return None;
    };
    let Expr::Name(name) = attribute.value.as_ref() else {
        return None;
    };
    (name.id.as_str() == receiver).then(|| mangled(owner, attribute.attr.as_str()))
}

/// whether a class carries the `data class` marker, and which decorators it applies
///
/// `data_class` and `frozen_data_class` are how the transpiler spells a language
/// *modifier*: they are read here and never applied, which is right for a marker and
/// would be wrong for a decorator. every other modifier in the family had been read as
/// a decorator, which is the bug this split exists for
fn class_modifiers(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
) -> Lowered<(bool, Vec<Decorator>)> {
    let mut is_data = false;
    let mut applied = Vec::new();
    for decorator in &class.decorator_list {
        match class_modifier(db, model, decorator)? {
            Modifier::DataClass => is_data = true,
            Modifier::Erased => {}
            Modifier::Apply(decorator) => applied.push(decorator),
        }
    }
    if !applied.is_empty()
        && let Some(unwritten) = published_beyond_the_body(class)
    {
        return Err(Decline::new(format!(
            "an emitted type publishes `{unwritten}` alongside a method this class writes, and a decorator reads the class it is handed"
        )));
    }
    Ok((is_data, applied))
}

/// the dunders an emitted type publishes alongside this one, because they share a slot
///
/// python reaches these through a *slot* rather than by name, and one slot backs several
/// names: `tp_richcompare` backs all six comparisons, every binary numeric slot backs an
/// operator and its reflection, and `mp_ass_subscript` backs `__setitem__` along with
/// `__delitem__`. the type publishes a wrapper for each name a filled slot backs, so a
/// class that writes one of a group gets the whole group and the rest answer
/// `NotImplemented`.
///
/// the groups are `COMPARISONS`, `ARITHMETIC`, `POWER` and `slot_companion` in
/// `by_codegen_c`, which sits downstream of this crate and cannot be asked from here
fn shares_a_slot(name: &str) -> &'static [&'static str] {
    const GROUPS: &[&[&str]] = &[
        &["__lt__", "__le__", "__eq__", "__ne__", "__gt__", "__ge__"],
        &["__add__", "__radd__"],
        &["__sub__", "__rsub__"],
        &["__mul__", "__rmul__"],
        &["__truediv__", "__rtruediv__"],
        &["__floordiv__", "__rfloordiv__"],
        &["__mod__", "__rmod__"],
        &["__divmod__", "__rdivmod__"],
        &["__lshift__", "__rlshift__"],
        &["__rshift__", "__rrshift__"],
        &["__and__", "__rand__"],
        &["__xor__", "__rxor__"],
        &["__or__", "__ror__"],
        &["__matmul__", "__rmatmul__"],
        &["__pow__", "__rpow__"],
        &["__setitem__", "__delitem__"],
    ];
    GROUPS
        .iter()
        .find(|group| group.contains(&name))
        .copied()
        .unwrap_or(&[])
}

/// a name the emitted type would publish that the class body never wrote, where the
/// class writes any method at all that shares its slot with one
///
/// this is what stops a decorator being handed a class the `class` statement did not
/// write. `@functools.total_ordering` is the shape that proves it matters: it fills in
/// the comparisons a class left out, saw `__le__` already published, added nothing — and
/// `a <= b` then raised where the interpreted class answered `True`
fn published_beyond_the_body(class: &ast::StmtClassDef) -> Option<&'static str> {
    let written = |name: &str| {
        class
            .body
            .iter()
            .any(|statement| matches!(statement, Stmt::FunctionDef(method) if method.name.as_str() == name))
    };
    class.body.iter().find_map(|statement| {
        let Stmt::FunctionDef(method) = statement else {
            return None;
        };
        shares_a_slot(method.name.as_str())
            .iter()
            .copied()
            .find(|name| !written(name))
    })
}

/// what a modifier keyword means to the native build
enum Modifier {
    /// the `data class` marker, which is a layout rather than a decorator
    DataClass,
    /// no runtime effect at all: the transpiler erases it, so the interpreted twin
    /// has nothing there either
    Erased,
    /// a real python decorator, applied to the finished definition at module init —
    /// which is what the transpiler emits in a modifier's place
    Apply(Decorator),
}

/// the decorator expression this is, or why it is one the native build cannot evaluate
///
/// python evaluates a decorator where the definition stands. a module-level definition
/// stands in the interpreted twin's body, which has already run by the time module init
/// installs the native one — so init is the only moment left, and the expression has to
/// be one that means the same thing there. a chain of attribute reads off a name does; a
/// call does not, because calling it at init calls it a *second* time and at the end of
/// the module rather than where it was written
fn decorator_path(expression: &Expr) -> Lowered<Decorator> {
    let mut attributes = Vec::new();
    let mut cursor = expression;
    loop {
        match cursor {
            Expr::Name(name) => {
                attributes.reverse();
                return Ok(Decorator::Path {
                    root: name.id.to_string(),
                    attributes,
                });
            }
            Expr::Attribute(attribute) => {
                attributes.push(attribute.attr.to_string());
                cursor = &attribute.value;
            }
            Expr::Call(_) => {
                return Err(Decline::new(
                    "a decorator that is a call is evaluated where the definition stands, and module-level code is not compiled — calling it at init would run it a second time",
                ));
            }
            _ => {
                return Err(Decline::new(
                    "only a name, or a chain of attributes read off one, is lowered as a decorator",
                ));
            }
        }
    }
}

/// whether a module-level definition can have its decorators moved to module init
///
/// python runs a decorator where the definition stands, and the twin's body is what
/// stands there — so init running it again is one evaluation too many, and the decorator
/// comes out of the twin's source to make it one. that leaves a window, from the twin's
/// `def` to the end of module init, in which the name holds a definition nothing has
/// decorated yet. it is invisible unless something reads the name, and the module's own
/// body is the only thing that can: everything else runs after init.
///
/// so this is what decides whether the move is safe, and a definition it turns down
/// declines rather than being compiled and decorated twice
fn decorator_stays_unread(
    read: &BTreeSet<&str>,
    name: &str,
    decorators: &[Decorator],
) -> Lowered<()> {
    if decorators.is_empty() || !read.contains(name) {
        return Ok(());
    }
    Err(Decline::new(format!(
        "this module reads `{name}`, and its decorator cannot run where the definition stands and again over the compiled one"
    )))
}

/// every name this module's own body can read before module init has finished
///
/// a decorator module init applies is taken out of the source the twin runs — see
/// [`without_init_decorators`] — so from the twin's `def` until init reaches it, the
/// definition stands in the namespace undecorated. anything that reads the name in that
/// window keeps what it read: `TABLE = f()` in the module body straightforwardly, and
/// `def g(): return f()` called from that body just the same, because `g` reads the
/// global when it runs and not when it was written.
///
/// a load inside a `def` is a different matter: it happens when that definition *runs*,
/// and everything that runs after import sees the decorated name. so the body reaches one
/// only by naming the definition it is in — which is itself a load the body makes, and is
/// followed from there. a module that defines helpers and calls none of them at import
/// reads nothing at all, which is the common shape and the one that keeps compiling.
///
/// an annotation is a read only where python evaluates one. under
/// `from __future__ import annotations` it never does — `def f(x: Held)` stores the
/// *string* `"Held"` — so a module written that way names a class in a signature without
/// ever holding what the name meant at that moment
fn names_read(suite: &[Stmt]) -> BTreeSet<&str> {
    /// loads the module body makes as it runs, and the loads each definition holds
    /// behind its own name
    #[derive(Default)]
    struct Reads<'a> {
        now: BTreeSet<&'a str>,
        held: HashMap<&'a str, BTreeSet<&'a str>>,
    }
    /// a walk that files every load under one heading
    struct Into<'a, 'r>(&'r mut BTreeSet<&'a str>);
    impl<'a> ruff_python_ast::visitor::Visitor<'a> for Into<'a, '_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Name(name) = expr
                && name.ctx.is_load()
            {
                self.0.insert(name.id.as_str());
            }
            ruff_python_ast::visitor::walk_expr(self, expr);
        }
    }
    fn statements<'a>(into: &mut BTreeSet<&'a str>, body: &'a [Stmt]) {
        for statement in body {
            ruff_python_ast::visitor::walk_stmt(&mut Into(into), statement);
        }
    }
    /// everything a `def` evaluates where it stands: its decorators, its defaults, and
    /// its annotations where this module evaluates those. only the body waits to be called
    fn header<'a>(
        into: &mut BTreeSet<&'a str>,
        function: &'a ast::StmtFunctionDef,
        evaluated: bool,
    ) {
        let mut visit = Into(into);
        for decorator in &function.decorator_list {
            ruff_python_ast::visitor::walk_expr(&mut visit, &decorator.expression);
        }
        for parameter in &function.parameters {
            if let Some(annotation) = parameter.annotation()
                && evaluated
            {
                ruff_python_ast::visitor::walk_expr(&mut visit, annotation);
            }
            if let Some(default) = parameter.default() {
                ruff_python_ast::visitor::walk_expr(&mut visit, default);
            }
        }
        if let Some(returns) = &function.returns
            && evaluated
        {
            ruff_python_ast::visitor::walk_expr(&mut visit, returns);
        }
    }

    let evaluated = annotations_are_evaluated(suite);
    let mut reads = Reads::default();
    for statement in suite {
        match statement {
            Stmt::FunctionDef(function) => {
                header(&mut reads.now, function, evaluated);
                let held = reads.held.entry(function.name.as_str()).or_default();
                statements(held, &function.body);
            }
            // a class body runs with the module, so what it reads the module reads. its
            // methods' bodies do not, and they wait behind the class's own name
            Stmt::ClassDef(class) => {
                let mut visit = Into(&mut reads.now);
                for decorator in &class.decorator_list {
                    ruff_python_ast::visitor::walk_expr(&mut visit, &decorator.expression);
                }
                if let Some(arguments) = &class.arguments {
                    ruff_python_ast::visitor::walk_arguments(&mut visit, arguments);
                }
                for member in &class.body {
                    match member {
                        Stmt::FunctionDef(method) => {
                            header(&mut reads.now, method, evaluated);
                            let held = reads.held.entry(class.name.as_str()).or_default();
                            statements(held, &method.body);
                        }
                        other => statements(&mut reads.now, std::slice::from_ref(other)),
                    }
                }
            }
            other => statements(&mut reads.now, std::slice::from_ref(other)),
        }
    }

    // naming a definition is enough to have reached what it holds: the body may call it,
    // hand it to something that calls it, or store it where a later statement will
    let mut read = reads.now;
    let mut pending: Vec<&str> = read.iter().copied().collect();
    while let Some(name) = pending.pop() {
        let Some(held) = reads.held.get(name) else {
            continue;
        };
        for inner in held {
            if read.insert(inner) {
                pending.push(inner);
            }
        }
    }
    read
}

/// the interpreted twin's source with every decorator module init re-applies blanked out
///
/// a decorator is evaluated once in python, where the definition stands. the twin's `def`
/// is what stands there, and module init evaluates the same decorator a *second* time over
/// the native definition that replaces the twin's — so `@register` puts two entries in its
/// registry and `@count_them` counts one function twice. the binding the namespace ends up
/// with is right either way, which is what makes this a silent one.
///
/// only the decorators [`ModuleIr::decorated_at_init`] names are removed, and each is
/// matched by the path it was written as, so a decorator init does *not* re-apply — the
/// `@dataclass` a `data class` becomes, the `@staticmethod` the method table honours
/// instead, a *method's* which init applies to the finished type — is left where it is
/// and still runs once.
///
/// they are blanked rather than cut, because a traceback through the twin quotes its
/// source by line: taking the line out would move every definition below it.
///
/// this is deliberately keyed off the *twin's* text rather than the original source's,
/// because the twin is what runs. for a `.by` module the two are not the same file — the
/// twin is the transpiler's output, where a modifier has already become the decorator it
/// stands for
pub fn without_init_decorators(source: &str, module: &ModuleIr) -> Result<String, String> {
    let mut wanted: HashMap<&str, Vec<&Decorator>> = HashMap::new();
    for decoration in module.decorated_at_init() {
        wanted
            .entry(decoration.name)
            .or_default()
            .extend(decoration.decorators);
    }
    if wanted.is_empty() {
        return Ok(source.to_string());
    }
    let parsed = ruff_python_parser::parse_module(source)
        .map_err(|error| format!("the interpreted fallback does not parse: {error}"))?;

    let mut blank: Vec<(usize, usize)> = Vec::new();
    let mut mark = |paths: Option<&Vec<&Decorator>>, written: &[ast::Decorator]| {
        let Some(paths) = paths else {
            return;
        };
        let mut taken = vec![false; written.len()];
        for path in paths {
            // the first unclaimed one that was written as this path. a definition may
            // carry the same decorator twice, and then init re-applies it twice too —
            // so each application claims one occurrence rather than all of them
            let found = written.iter().enumerate().position(|(index, decorator)| {
                !taken[index]
                    && decorator_path(&decorator.expression).is_ok_and(|found| found == **path)
            });
            if let Some(index) = found {
                taken[index] = true;
                let range = written[index].range();
                blank.push((range.start().to_usize(), range.end().to_usize()));
            }
        }
    };
    // only a module-level definition is ever named here, so descending further could
    // only blank a decorator on an unrelated definition that happens to share a name
    for statement in parsed.suite() {
        match statement {
            Stmt::FunctionDef(function) => {
                mark(wanted.get(function.name.as_str()), &function.decorator_list);
            }
            Stmt::ClassDef(class) => {
                mark(wanted.get(class.name.as_str()), &class.decorator_list);
            }
            _ => {}
        }
    }

    let mut out = source.as_bytes().to_vec();
    for (start, end) in blank {
        for byte in &mut out[start..end] {
            // a decorator may be written over several lines, and the line breaks inside
            // it are what keep everything below on the line it was on
            if *byte != b'\n' && *byte != b'\r' {
                *byte = b' ';
            }
        }
    }
    String::from_utf8(out)
        .map_err(|error| format!("blanking a decorator split a character: {error}"))
}

/// the attributes each emitted class publishes as a `property`
///
/// every one is a *data* descriptor, so a write on an instance reaches it rather than
/// looking for somewhere on the instance to land — which is what makes it the one
/// class-level binding an emitted instance can still be written through. a method or a
/// class-level constant is not: writing over one puts a value in the instance dict an
/// emitted instance does not have.
///
/// a lone `@property` with no setter is here too. the write reaches the descriptor and
/// the descriptor refuses it, in python's own wording — which is the interpreted answer
fn published_properties(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
) -> HashMap<String, HashSet<String>> {
    suite
        .iter()
        .filter_map(|statement| match statement {
            Stmt::ClassDef(class) if layouts.contains_key(class.name.as_str()) => {
                Some((class.name.to_string(), property_names(db, model, class)))
            }
            _ => None,
        })
        .collect()
}

/// the attributes this class body binds with `@property`, under the names the body binds
/// them as
fn property_names(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
) -> HashSet<String> {
    // a class body that binds `property` itself is the nearer scope the decorator is
    // resolved out of, so nothing under it is one of these
    if class_body_binds(&class.body, "property") {
        return HashSet::new();
    }
    class
        .body
        .iter()
        .filter_map(|statement| match statement {
            Stmt::FunctionDef(function) if is_property_getter(db, model, function) => {
                Some(mangled(Some(&class.name), &function.name))
            }
            _ => None,
        })
        .collect()
}

/// which half of a property an accessor is
#[derive(Clone, Copy, PartialEq, Eq)]
enum Half {
    Get,
    Set,
    Delete,
}

impl Half {
    /// the suffix the half's body is held under
    ///
    /// two halves are both written `def value`, and one symbol per name is not enough
    /// for two bodies. `$` cannot appear in a python identifier, so a source name never
    /// arrives here already carrying one
    fn suffix(self) -> &'static str {
        match self {
            Self::Get => "$get",
            Self::Set => "$set",
            Self::Delete => "$del",
        }
    }

    /// how many parameters the half's `def` takes, receiver included
    fn arity(self) -> usize {
        match self {
            Self::Set => 2,
            Self::Get | Self::Delete => 1,
        }
    }

    /// the attribute python's `property` reads this half out of, which is also the word
    /// it uses in the `AttributeError` a missing one raises
    fn written_as(self) -> &'static str {
        match self {
            Self::Get => "getter",
            Self::Set => "setter",
            Self::Delete => "deleter",
        }
    }
}

/// a `@property` getter and the halves written under it
///
/// python folds all of them into one `property` object bound once, under the name every
/// `def` in the group was written as.
///
/// the attribute it becomes is published by the *type spec*, so a class built through its
/// metaclass instead — which is built out of a namespace and never consults the spec —
/// would simply not have it. nothing here asks that question, because every half carries a
/// decorator and [`metaclass_carries_the_body`] already turns such a class down for
/// exactly that; relaxing that gate has to answer this one
struct PropertyGroup<'a> {
    /// the name every half was written as, before private mangling
    name: &'a str,
    getter: Option<&'a ast::StmtFunctionDef>,
    setter: Option<&'a ast::StmtFunctionDef>,
    deleter: Option<&'a ast::StmtFunctionDef>,
}

impl<'a> PropertyGroup<'a> {
    fn halves(&self) -> impl Iterator<Item = (Half, &'a ast::StmtFunctionDef)> + use<'a> {
        [
            (Half::Get, self.getter),
            (Half::Set, self.setter),
            (Half::Delete, self.deleter),
        ]
        .into_iter()
        .filter_map(|(half, written)| written.map(|written| (half, written)))
    }
}

/// the `@property` pairs this class body writes
///
/// the shape is exact, because anything looser would be lowering something else: every
/// `def` of the name carries a single written decorator, the first of them is
/// `@property`, and each of the others is `@<name>.setter` or `@<name>.deleter` on that
/// same name. a different decorator, a rebound root, `property(fget, fset)` called
/// directly — none of those is this construct, and a group that is *nearly* one declines
/// with what stopped it rather than falling through to a message about a name defined
/// twice
fn property_groups<'a>(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    class: &'a ast::StmtClassDef,
) -> Lowered<Vec<PropertyGroup<'a>>> {
    // a class body that binds `property` itself is the nearer scope, and the decorator
    // is resolved out of it — so `@property` there is whatever the body bound
    if class_body_binds(&class.body, "property") {
        return Ok(Vec::new());
    }
    let definitions = |name: &str| {
        class
            .body
            .iter()
            .filter_map(|statement| match statement {
                Stmt::FunctionDef(function) if function.name.as_str() == name => Some(function),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let mut groups: Vec<PropertyGroup<'a>> = Vec::new();
    for statement in &class.body {
        let Stmt::FunctionDef(function) = statement else {
            continue;
        };
        let name = function.name.as_str();
        if groups.iter().any(|group| group.name == name) {
            continue;
        }
        let written = definitions(name);
        // a lone `@property` getter is left alone: module init copies the `property` the
        // interpreted body already built, which keeps `fget` and `fset` answering, and
        // taking it over here would trade that for a getset and recover no decline
        if written.len() < 2
            || !written[1..]
                .iter()
                .any(|later| accessor_half(db, model, later).is_ok())
        {
            continue;
        }
        // something below is a half of this name, so this *is* the construct — and what
        // stands above it has to be the plain `@property` python folds them onto. a
        // getter carrying a second decorator is a different object being folded
        if !is_property_getter(db, model, written[0]) {
            return Err(Decline::new(format!(
                "`{name}` has a half written under it, and what stands above them is not a plain `@property`"
            )));
        }
        let mut group = PropertyGroup {
            name,
            getter: Some(written[0]),
            setter: None,
            deleter: None,
        };
        for accessor in &written[1..] {
            let half = accessor_half(db, model, accessor)?;
            let slot = match half {
                Half::Get => &mut group.getter,
                Half::Set => &mut group.setter,
                Half::Delete => &mut group.deleter,
            };
            // `@value.getter` replaces the getter above it, and a second `@value.setter`
            // replaces the first — python keeps only the last, so the ones before it are
            // written and never reached. lowering the group as written would keep them
            if half == Half::Get || slot.is_some() {
                return Err(Decline::new(format!(
                    "`{name}` writes a second `{}`, which replaces the one above it rather than adding to it",
                    half.written_as()
                )));
            }
            *slot = Some(accessor);
        }
        // a dunder is read out of the *slot* rather than off the name, and a getset entry
        // fills no slot — so `@property def __len__` would answer `x.__len__` and leave
        // `len(x)` going wherever the base's slot went, which is two answers
        if fills_a_type_slot(name) {
            return Err(Decline::new(format!(
                "`{name}` is a property and fills a type slot, which a getset entry does not"
            )));
        }
        for (half, accessor) in group.halves() {
            accessor_is_plain(accessor, half)?;
        }
        groups.push(group);
    }
    Ok(groups)
}

/// whether this definition is the `@property` a group starts with
///
/// matched by the bare name, the way `@staticmethod` and `@classmethod` are — and with
/// the class body already ruled out as having bound it
fn is_property_getter(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
) -> bool {
    let [decorator] = function.decorator_list.as_slice() else {
        return false;
    };
    matches!(&decorator.expression, Expr::Name(name) if name.id.as_str() == "property")
        && is_written_decorator(db, model, decorator)
}

/// which half `@value.setter` and its siblings write
fn accessor_half(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
) -> Lowered<Half> {
    let unrecognised = || {
        Decline::new(format!(
            "`{}` is written more than once, and the second is not a half of the property above it",
            function.name
        ))
    };
    let [decorator] = function.decorator_list.as_slice() else {
        return Err(unrecognised());
    };
    if !is_written_decorator(db, model, decorator) {
        return Err(unrecognised());
    }
    let Expr::Attribute(attribute) = &decorator.expression else {
        return Err(unrecognised());
    };
    // the root has to be the name this very `def` binds: `@other.setter` folds this body
    // into a *different* property, which is not the one-attribute construct lowered here
    let Expr::Name(root) = attribute.value.as_ref() else {
        return Err(unrecognised());
    };
    if root.id.as_str() != function.name.as_str() {
        return Err(unrecognised());
    }
    match attribute.attr.as_str() {
        "getter" => Ok(Half::Get),
        "setter" => Ok(Half::Set),
        "deleter" => Ok(Half::Delete),
        _ => Err(unrecognised()),
    }
}

/// whether a half is the plain accessor a getset entry can stand for
///
/// `tp_getset` calls a getter with the receiver and nothing else, and a setter with the
/// receiver and the one value. a half that takes anything more can still be *called* as
/// python calls it — through the `property` the interpreted body built — but not through
/// the two function pointers this lowers it to
fn accessor_is_plain(function: &ast::StmtFunctionDef, half: Half) -> Lowered<()> {
    let parameters = &function.parameters;
    let arity = parameters.posonlyargs.len() + parameters.args.len();
    if arity != half.arity()
        || !parameters.kwonlyargs.is_empty()
        || parameters.vararg.is_some()
        || parameters.kwarg.is_some()
        || parameters
            .iter_non_variadic_params()
            .any(|parameter| parameter.default.is_some())
    {
        return Err(Decline::new(format!(
            "a property `{}` reached through a getset takes exactly {} argument(s), and `{}` does not",
            half.written_as(),
            half.arity(),
            function.name
        )));
    }
    // a suspending accessor hands back a generator rather than running, and the state
    // class it would need is named after the `def` — which both halves share
    if function.is_async || generators::is_generator(&function.body) {
        return Err(Decline::new(format!(
            "a property `{}` that suspends is not lowered yet",
            half.written_as()
        )));
    }
    Ok(())
}

/// one half of a property, as the method holding its body
///
/// the decorator that made it *is* the construct, so it comes off: applying it at init
/// would build a second `property` out of the native function and put it exactly where
/// the getset entry has to be
fn lower_accessor<'a>(
    unit: Unit<'a>,
    accessor: &'a ast::StmtFunctionDef,
    class: &str,
    half: Half,
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    let (mut method, produced) = lower_method(unit, accessor, class)?;
    method.decorators.clear();
    // a boundary that hands the call on takes the twin off the interpreted class by
    // name, and the name a property is under there holds the `property` object rather
    // than either half of it — so there is nothing for it to call
    if method.defers() {
        return Err(Decline::new(format!(
            "a property `{}` whose boundary can hand the call on would reach the `property` object rather than the half it wants",
            half.written_as()
        )));
    }
    // an environment class is named after the `def`, which both halves share
    if !produced.is_empty() {
        return Err(Decline::new(format!(
            "a property `{}` that makes closures is not lowered yet",
            half.written_as()
        )));
    }
    method.name.push_str(half.suffix());
    Ok((method, produced))
}

/// whether this definition is the only one of its name in the scope it stands in
///
/// two `def`s of one name bind whichever one *ran*, so a direct call cannot know which
/// function it is calling — and they mangle to one C symbol besides, which makes the
/// whole module fail to build rather than answer wrongly. `closures::plan` asks this of
/// every nested scope; the module scope had nobody asking, and
/// `importlib/resources/_common.py` has three module-level `def _`s
fn defined_once(scope: &[Stmt], function: &ast::StmtFunctionDef) -> Lowered<()> {
    let named = scope
        .iter()
        .filter(
            |statement| matches!(statement, Stmt::FunctionDef(other) if other.name.as_str() == function.name.as_str()),
        )
        .count();
    if named > 1 {
        return Err(Decline::new(format!(
            "`{}` is defined more than once in this scope, so a call to it has no single target",
            function.name
        )));
    }
    Ok(())
}

/// whether a class body binds this name
///
/// a decorator is resolved out of the *module* namespace at module init, and a class
/// body is not that namespace. `@fieldnames.setter` reads the property the body bound
/// two statements up, which is nowhere init can look — so the method keeps its
/// interpreted definition, and with it the whole class
fn class_body_binds(body: &[Stmt], name: &str) -> bool {
    body.iter().any(|statement| match statement {
        Stmt::FunctionDef(node) => node.name.as_str() == name,
        Stmt::ClassDef(node) => node.name.as_str() == name,
        Stmt::Assign(node) => node.targets.iter().any(|target| binds_name(target, name)),
        Stmt::AnnAssign(node) => binds_name(&node.target, name),
        Stmt::AugAssign(node) => binds_name(&node.target, name),
        // a block nested in the body is part of the body, and what it binds is bound in
        // the same namespace — so a decorator written below one reads what the block
        // left there rather than the module's. the other block shapes never reach a
        // lowered class at all: the body walk turns them down before this is asked
        Stmt::If(node) => {
            class_body_binds(&node.body, name)
                || node
                    .elif_else_clauses
                    .iter()
                    .any(|clause| class_body_binds(&clause.body, name))
        }
        Stmt::For(node) => {
            binds_name(&node.target, name)
                || class_body_binds(&node.body, name)
                || class_body_binds(&node.orelse, name)
        }
        Stmt::While(node) => {
            class_body_binds(&node.body, name) || class_body_binds(&node.orelse, name)
        }
        _ => false,
    })
}

/// the names a block nested in a class body could bind into the class namespace
///
/// this is deliberately an over-approximation, and the direction it errs in is the whole
/// design. a name collected here that the block never reached costs nothing:
/// `By_ConstantValue` reads the interpreted definition's namespace by name and answers
/// "the body did not write that", which is exactly what the emitted class should say
/// about an attribute a false condition never defined. a name *missed* is an attribute
/// the interpreted class has and the emitted one does not — so a statement whose
/// bindings are not modelled here declines rather than being walked past
fn nested_bindings<'a>(body: &'a [Stmt], names: &mut Vec<&'a str>) -> Lowered<()> {
    for statement in body {
        match statement {
            Stmt::FunctionDef(node) => names.push(node.name.as_str()),
            Stmt::Assign(node) => {
                for target in &node.targets {
                    bound_by_a_target(target, names)?;
                }
            }
            // an annotation with no value binds nothing, the same as one written at the
            // top of the body — where a class that is not a `data class` ignores it too
            Stmt::AnnAssign(node) if node.value.is_none() => {}
            Stmt::AnnAssign(node) => bound_by_a_target(&node.target, names)?,
            Stmt::AugAssign(node) => bound_by_a_target(&node.target, names)?,
            // `if let P := subject:` binds the pattern's captures, which are not names
            // written anywhere this can read them off
            Stmt::If(node) if node.pattern.is_some() => {
                return Err(Decline::new(
                    "a pattern's captures in a class body are not lowered yet",
                ));
            }
            Stmt::If(node) => {
                nested_bindings(&node.body, names)?;
                for clause in &node.elif_else_clauses {
                    nested_bindings(&clause.body, names)?;
                }
            }
            // the loop variable is left behind in the namespace when the loop ends, so
            // it is one of the names the block binds
            Stmt::For(node) => {
                bound_by_a_target(&node.target, names)?;
                nested_bindings(&node.body, names)?;
                nested_bindings(&node.orelse, names)?;
            }
            Stmt::While(node) => {
                nested_bindings(&node.body, names)?;
                nested_bindings(&node.orelse, names)?;
            }
            // an expression evaluated for its effect binds no name: whatever it did to
            // whatever it reached, the interpreted definition did it before init reads
            // the namespace
            Stmt::Expr(_) | Stmt::Pass(_) | Stmt::Break(_) | Stmt::Continue(_) => {}
            _ => {
                return Err(Decline::new(format!(
                    "{} nested in a class body is not lowered yet",
                    statement_word(statement)
                )));
            }
        }
    }
    Ok(())
}

/// the names an assignment target binds, for [`nested_bindings`]
fn bound_by_a_target<'a>(target: &'a Expr, names: &mut Vec<&'a str>) -> Lowered<()> {
    match target {
        Expr::Name(name) => names.push(name.id.as_str()),
        // a write into an object rather than a name the namespace takes —
        // `dispatch[PickleBuffer] = save_picklebuffer` is the shape. the interpreted
        // definition made the write before init reads the namespace, so the object
        // copied across is already finished
        Expr::Subscript(_) | Expr::Attribute(_) => {}
        Expr::Tuple(tuple) => {
            for element in tuple {
                bound_by_a_target(element, names)?;
            }
        }
        Expr::List(list) => {
            for element in list {
                bound_by_a_target(element, names)?;
            }
        }
        Expr::Starred(starred) => bound_by_a_target(&starred.value, names)?,
        _ => return Err(Decline::new("only a plain class-level name is lowered yet")),
    }
    Ok(())
}

/// whether a name a nested block binds is the only definition of that attribute
///
/// two ways it is not. a dunder decides a type slot, an instance layout, or what the
/// class publishes about itself, and all three are settled from the body text while a
/// nested block's binding is only known once the interpreter has run the block — so
/// `__slots__` or `__init__` under a conditional would have the emitted class laid out
/// for one answer and carrying the other.
///
/// and a `def` in the same body binds the same attribute twice: the `def` is lowered
/// into the method table while the block's binding is copied off the interpreted
/// definition, so the type would answer with whichever landed in its dict last while a
/// compiled call site still reached the method
fn nested_binding_stands_alone(body: &[Stmt], name: &str) -> Lowered<()> {
    if name.starts_with("__") && name.ends_with("__") {
        return Err(Decline::new(format!(
            "`{name}` is bound by a block nested in the class body, and a dunder is settled before one runs"
        )));
    }
    if body
        .iter()
        .any(|statement| matches!(statement, Stmt::FunctionDef(node) if node.name.as_str() == name))
    {
        return Err(Decline::new(format!(
            "`{name}` is both defined by this class body and bound by a block nested in it"
        )));
    }
    Ok(())
}

/// what to call a statement in a decline, in the word python spells it with
fn statement_word(statement: &Stmt) -> &'static str {
    match statement {
        // the top of a class body does not lower one either, and a block is no place to
        // settle a question the plain form has not been asked yet
        Stmt::ClassDef(_) => "a class",
        Stmt::Try(_) => "`try`",
        Stmt::With(_) => "`with`",
        Stmt::Match(_) => "`match`",
        Stmt::Delete(_) => "`del`",
        Stmt::Import(_) | Stmt::ImportFrom(_) => "an import",
        Stmt::Global(_) => "`global`",
        Stmt::Nonlocal(_) => "`nonlocal`",
        Stmt::Raise(_) => "`raise`",
        Stmt::Assert(_) => "`assert`",
        Stmt::Return(_) => "`return`",
        Stmt::TypeAlias(_) => "a type alias",
        Stmt::Let(_) => "`let`",
        _ => "a statement",
    }
}

/// whether this module evaluates the annotations it writes
///
/// `from __future__ import annotations` makes every one of them a string that nothing
/// evaluates until something asks — so naming a class in a signature is not a *read* of
/// it, and a module written that way keeps compiling classes a module without it would
/// have to turn down
fn annotations_are_evaluated(suite: &[Stmt]) -> bool {
    !suite.iter().any(|statement| {
        matches!(statement, Stmt::ImportFrom(import)
            if import.module.as_ref().is_some_and(|module| module.as_str() == "__future__")
                && import.names.iter().any(|alias| alias.name.as_str() == "annotations"))
    })
}

/// whether this decorator was written with an `@`
///
/// basedpython's class and function *modifiers* — `sealed`, `static`, `data class` and
/// the rest — reach the ast as decorators with no `@` in front of them, which is the
/// only thing that tells the two apart. the transpiler settles them by the same test.
///
/// this matters more here than anywhere: a decorator becomes a **name looked up in the
/// module namespace at init**, and a modifier has no such name. `static def m` compiled
/// to `By_ApplyDecorator(dict, "m", "static")` and the whole extension then failed to
/// import with `NameError: name 'static' is not defined`
fn is_written_decorator(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    decorator: &ast::Decorator,
) -> bool {
    let source = ruff_db::source::source_text(db, model.file());
    source
        .as_bytes()
        .get(usize::from(decorator.range().start()))
        .copied()
        == Some(b'@')
}

/// what a class decorator means to the native build
///
/// a written `@` decorator is applied to the *namespace entry* after the type is
/// installed, exactly as the class statement would have; [`decorator_path`] says which
/// expressions still mean there what they meant where the `class` stood. a *modifier* is
/// a bare name and never anything else, which is why the two questions are asked in this
/// order
fn class_modifier(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    decorator: &ast::Decorator,
) -> Lowered<Modifier> {
    if is_written_decorator(db, model, decorator) {
        return decorator_path(&decorator.expression).map(Modifier::Apply);
    }
    let Expr::Name(name) = &decorator.expression else {
        return Err(Decline::new(
            "only a plain-name class modifier is lowered yet",
        ));
    };
    match name.id.as_str() {
        "data_class" | "frozen_data_class" => Ok(Modifier::DataClass),
        // erased by the transpiler, so the interpreted twin carries nothing either.
        // `sealed` grows a `__sealed_members__` tuple, but the module body the
        // fallback runs is what writes it
        "abstract" | "open" | "sealed" | "export" => Ok(Modifier::Erased),
        // `final` becomes `@final` from `typing`, which returns its argument — the
        // one class decorator whose result is provably the class it was handed
        "final" => Ok(Modifier::Apply(Decorator::name("final"))),
        // `private` renames the class and `protocol` rewrites its bases: neither is a
        // decorator at all, and the emitted type would answer to the wrong name or
        // stand outside the protocol it was declared to be
        _ => Err(Decline::new(
            "this class modifier changes what the class is, which an emitted type cannot follow",
        )),
    }
}

/// the same for a function or a method
///
/// the modifier mapping is the transpiler's: each of these becomes the named python
/// decorator in the interpreted twin, and the fallback preamble is what binds the name —
/// so looking it up in the module namespace at init finds exactly what the twin used
fn function_modifier(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    decorator: &ast::Decorator,
) -> Lowered<Modifier> {
    if is_written_decorator(db, model, decorator) {
        return decorator_path(&decorator.expression).map(Modifier::Apply);
    }
    let Expr::Name(name) = &decorator.expression else {
        return Err(Decline::new("only a plain-name modifier is lowered yet"));
    };
    match name.id.as_str() {
        "static" => Ok(Modifier::Apply(Decorator::name("staticmethod"))),
        "classmethod" => Ok(Modifier::Apply(Decorator::name("classmethod"))),
        "abstract" => Ok(Modifier::Apply(Decorator::name("abstractmethod"))),
        "final" => Ok(Modifier::Apply(Decorator::name("final"))),
        "override" => Ok(Modifier::Apply(Decorator::name("override"))),
        // neither reaches the interpreted twin as a decorator
        "open" | "export" => Ok(Modifier::Erased),
        // `private` mangles the name the definition is bound under, which is a
        // rename rather than a decorator
        _ => Err(Decline::new(
            "this modifier changes what the definition is bound as, not what it is",
        )),
    }
}

/// the class a class extends
///
/// a name this module does not emit is still lowerable, however many of them: the type
/// is built on whatever they resolve to at module init, python works out the mro and
/// which of them owns the layout, and this class declares none of its own.
///
/// two things are not lowerable. a base that is not a name at all — a subscript like
/// `Generic[T]` is not a value to look up. and one of *ours* that lays out fields
/// standing beside one that is not, because the layout would have to be inherited from
/// the outside and laid out here at the same time
fn base_class(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    class: &ast::StmtClassDef,
    layouts: &Layouts,
) -> Lowered<Option<ClassBase>> {
    let Some(arguments) = &class.arguments else {
        return Ok(None);
    };
    // a keyword goes to the metaclass, which only the construction *through* one can
    // reach — and that construction takes a base tuple however short. so a keyword makes
    // the bases external even when none were written: python supplies `(object,)` for an
    // empty one itself, the same as `type("C", (), ns)` does
    let keyed = !arguments.keywords.is_empty();
    // what each base written as a plain name stands for — see `base_stands_for`
    let named: Vec<Option<&str>> = arguments
        .args
        .iter()
        .map(|base| match base {
            Expr::Name(name) => base_stands_for(suite, name.id.as_str()).map(Some),
            _ => Ok(None),
        })
        .collect::<Lowered<Vec<_>>>()?;
    match (arguments.args.as_ref(), named.as_slice()) {
        ([], _) if keyed => Ok(Some(ClassBase::External(Vec::new()))),
        ([], _) => Ok(None),
        // `class C(object)` is what `class C:` already is — the base adds no storage
        // and no members, so there is nothing to lay out and nothing to inherit.
        // resolved rather than matched by name, because a module may bind `object`
        // to something else entirely
        ([base], _) if !keyed && is_builtin_object(db, env, model, base, layouts) => Ok(None),
        ([Expr::Name(_)], [Some(name)]) if layouts.contains_key(*name) => {
            if keyed {
                // the layout would have to be ours, which only the type spec lays out,
                // and a spec has nowhere to put a keyword
                return Err(Decline::new(
                    "a class keyword on a base this module emits is not lowered yet",
                ));
            }
            Ok(Some(ClassBase::InModule((*name).to_string())))
        }
        // more than one base: python works out the mro and which of them owns the
        // layout, and this class declares none of its own. one of *ours* may stand
        // among them so long as it lays nothing out — it is in the module namespace by
        // the time this class is built, so it resolves like any other name, and having
        // no fields it asks for no room this class does not control
        (bases, named) => {
            let mut paths = Vec::with_capacity(bases.len());
            for (base, name) in bases.iter().zip(named) {
                if let Some(name) = name
                    && layouts.get(*name).is_some_and(|fields| !fields.is_empty())
                {
                    return Err(Decline::new(
                        "a base this module lays out cannot stand beside one it does not",
                    ));
                }
                let path = match name {
                    Some(name) => (*name).to_string(),
                    None => match dotted_path(base) {
                        Some(path) => path,
                        None => {
                            return Err(Decline::new(
                                "only a name or a dotted name is lowered as a base class yet",
                            ));
                        }
                    },
                };
                if !external_base_resolves(model, base) {
                    return Err(Decline::new(
                        "a base out of this module needs to resolve to a class",
                    ));
                }
                if base_is_special_form(model, base) {
                    return Err(Decline::new(
                        "a typing special form builds the class itself, and what it builds is not a layout",
                    ));
                }
                paths.push(path);
            }
            Ok(Some(ClassBase::External(paths)))
        }
    }
}

/// the name a base written as a plain name stands for in this module's own namespace
///
/// every question asked about a base is asked of the *name*: whether this module lays it
/// out, whether it stands beside one that does, and what the emitted module looks up at
/// import. a module-level alias answers all three about a name that is not the class.
///
/// an emitted type is put in the namespace under the class's **own** name as it is
/// built, and an alias is carried over to it only once every class has been built — so
/// `Alias = Root` left `class C(Alias)` built on the interpreted definition while
/// `m.Root` was the emitted type, and `isinstance(C(), Root)` answered `False` where the
/// interpreter says `True`. the layout gates missed it for the same reason:
/// `class C(Alias, ABC)` compiled where `class C(Root, ABC)` is refused.
///
/// only a name the module body binds exactly once, to another plain name, is followed,
/// and only as far as a class this module writes. a chain that leaves the module is left
/// where it was written: swapping one name the body may rebind for another buys nothing,
/// and both stand for the same object at import.
///
/// a name bound twice stands for whichever binding ran last rather than the one the
/// class statement saw, which is a question about order that a name cannot answer — so
/// where any of those bindings is a class this module writes, the class declines
fn base_stands_for<'a>(suite: &'a [Stmt], written: &'a str) -> Lowered<&'a str> {
    let mut current = written;
    // bounded the way the base walks are: an alias chain cannot reach a name twice
    // without being a cycle, and a cycle would otherwise spin here rather than settle
    for _ in 0..=suite.len() {
        if class_written(suite, current).is_some() {
            return Ok(current);
        }
        match module_binding(suite, current) {
            Bound::Loose => return Ok(written),
            Bound::Alias(next) => current = next,
            Bound::Contested => {
                return Err(Decline::new(
                    "a base the module binds more than once stands for the class bound last, not the one it was built on",
                ));
            }
        }
    }
    Ok(written)
}

/// what this module's own body binds a base name to
enum Bound<'a> {
    /// nothing here says, so the name stands for itself — an import, or a value no
    /// class of this module's is behind
    Loose,
    /// the one module-level `name = other`
    Alias(&'a str),
    /// bound more than once, and a class this module writes is one of them
    Contested,
}

fn module_binding<'a>(suite: &'a [Stmt], name: &str) -> Bound<'a> {
    let mut aliases = Vec::new();
    let mut bindings = 0usize;
    for statement in suite {
        let value = match statement {
            Stmt::Assign(assign) => match assign.targets.as_slice() {
                [Expr::Name(target)] if target.id.as_str() == name => Some(assign.value.as_ref()),
                // a name among several targets is bound here too, so it counts as a
                // binding even though this one does not say what to
                targets if targets.iter().any(|target| binds_name(target, name)) => None,
                _ => continue,
            },
            // an annotation with no value binds nothing at all
            Stmt::AnnAssign(assign) => match assign.target.as_ref() {
                Expr::Name(target) if target.id.as_str() == name => match &assign.value {
                    Some(value) => Some(value.as_ref()),
                    None => continue,
                },
                _ => continue,
            },
            _ => continue,
        };
        bindings += 1;
        if let Some(Expr::Name(value)) = value {
            aliases.push(value.id.as_str());
        }
    }
    match (bindings, aliases.as_slice()) {
        (1, [alias]) => Bound::Alias(alias),
        _ if aliases
            .iter()
            .any(|alias| class_written(suite, alias).is_some()) =>
        {
            Bound::Contested
        }
        _ => Bound::Loose,
    }
}

/// whether an assignment target binds this name anywhere inside it
fn binds_name(target: &Expr, name: &str) -> bool {
    match target {
        Expr::Name(target) => target.id.as_str() == name,
        Expr::Tuple(tuple) => tuple.iter().any(|element| binds_name(element, name)),
        Expr::List(list) => list.iter().any(|element| binds_name(element, name)),
        Expr::Starred(starred) => binds_name(&starred.value, name),
        _ => false,
    }
}

/// the keyword arguments a class header carries, as the emitter needs them
///
/// the values are evaluated in the module scope at class-definition time, which at
/// import is what the module namespace holds — so a name, or a chain of attributes on
/// one, is resolved exactly the way a base is. a literal is emitted where it stands.
/// anything else would need the expression itself lowered into module init
fn class_keywords(class: &ast::StmtClassDef) -> Lowered<Vec<ClassKeyword>> {
    let Some(arguments) = &class.arguments else {
        return Ok(Vec::new());
    };
    let mut keywords = Vec::with_capacity(arguments.keywords.len());
    for keyword in &arguments.keywords {
        let Some(name) = &keyword.arg else {
            return Err(Decline::new("`**` in a class header is not lowered yet"));
        };
        let value = match &keyword.value {
            Expr::BooleanLiteral(literal) => KeywordValue::Bool(literal.value),
            Expr::NoneLiteral(_) => KeywordValue::None,
            Expr::StringLiteral(literal) => KeywordValue::Str(literal.value.to_string()),
            Expr::NumberLiteral(literal) => match &literal.value {
                ast::Number::Int(value) => match value.as_i64() {
                    Some(value) => KeywordValue::Int(value),
                    None => {
                        return Err(Decline::new(
                            "only a machine integer class keyword is lowered yet",
                        ));
                    }
                },
                _ => {
                    return Err(Decline::new(
                        "only an integer number class keyword is lowered yet",
                    ));
                }
            },
            value => match dotted_path(value) {
                Some(path) => KeywordValue::Path(path),
                None => {
                    return Err(Decline::new(
                        "only a name, a dotted name or a literal is lowered as a class keyword yet",
                    ));
                }
            },
        };
        keywords.push(ClassKeyword {
            name: name.to_string(),
            value,
        });
    }
    Ok(keywords)
}

/// whether this base expression is the builtin `object`
///
/// the name has to be resolved rather than matched, because a module may bind
/// `object` to a class of its own — and taking that for the builtin would give the
/// subclass the wrong base entirely
fn is_builtin_object(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    base: &Expr,
    layouts: &Layouts,
) -> bool {
    let Expr::Name(name) = base else {
        return false;
    };
    if name.id.as_str() != "object" || layouts.contains_key("object") {
        return false;
    }
    // and it has to *be* a class: an unresolved name is gradual, and a gradual base
    // says nothing about what it brings
    base.inferred_type(model)
        .is_some_and(|ty| !ty.is_dynamic() && mapper::map_type(db, env, ty).is_ok())
}

/// whether `expr` is the builtin function called `name`
///
/// python resolves a name local → enclosing → global → builtins, and only the last of
/// those steps reaches a builtin. so the name is *resolved* rather than matched: a
/// module that writes its own `def globals()` has bound the name itself, and
/// `from builtins import len as globals` binds a builtins function under a name that
/// is not its own. what settles both is the definition — the file it is written in,
/// and the identifier it is written under — so both are asked
fn is_builtin_function(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    expr: &Expr,
    name: &str,
) -> bool {
    defined_as(db, env, model, expr, name).is_some_and(|file| builtins_file(db, env) == Some(file))
}

/// whether `expr` is a function written under the identifier `name`, and the file it
/// is written in
///
/// resolving rather than matching is the whole point: what a *call site* spells says
/// nothing, because `from sys import _getframe as f` reaches the same function under
/// another name and a module of one's own can bind `globals` to anything. the
/// definition's own identifier is what the question is really about, so that is what
/// is read
fn defined_as(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    expr: &Expr,
    name: &str,
) -> Option<ruff_db::files::File> {
    let definition = expr
        .inferred_type(model)
        .and_then(|ty| ty.definition(db, env))?;
    if !matches!(definition, TypeDefinition::Function(_)) {
        return None;
    }
    // the *focus* range, which is the identifier a `def` was written under
    let defined = definition.focus_range(db)?;
    let source = ruff_db::source::source_text(db, defined.file());
    let range = defined.range();
    (source
        .as_str()
        .get(usize::from(range.start())..usize::from(range.end()))
        == Some(name))
    .then(|| defined.file())
}

/// the file `builtins` is written in
///
/// asked for through `object` rather than by path, because a path would have to know
/// which extension the stub carries and which typeshed it came from, while `object`
/// is a builtin whose home ty already resolves
fn builtins_file(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
) -> Option<ruff_db::files::File> {
    Some(
        KnownClass::Object
            .to_instance(db, env)
            .definition(db, env)?
            .focus_range(db)?
            .file(),
    )
}

/// a base written as a name, or as a chain of attributes on one, as a dotted path
///
/// `Exception` and `os.PathLike` are both reachable from the module namespace at
/// import; a subscript like `Generic[T]` is not a value to look up at all
fn dotted_path(base: &Expr) -> Option<String> {
    match base {
        Expr::Name(name) => Some(name.id.to_string()),
        Expr::Attribute(attribute) => Some(format!(
            "{}.{}",
            dotted_path(&attribute.value)?,
            attribute.attr
        )),
        _ => None,
    }
}

/// whether a base this module does not emit resolves to a class at all
///
/// a gradual base says nothing about what it brings — not its layout and not its
/// metaclass — so a type cannot be built on one
fn external_base_resolves(model: &SemanticModel<'_>, base: &Expr) -> bool {
    base.inferred_type(model).is_some_and(|ty| !ty.is_dynamic())
}

/// whether a base is one of typing's special forms rather than a class
///
/// `class Point(NamedTuple)` does not mean "a class deriving from `NamedTuple`". the
/// special form is machinery: python reads the annotations in the body and builds a
/// `tuple` subclass with `_fields`, a generated `__new__` and a fixed arity, none of which
/// a layout can describe. and the result is an ordinary `type` over `tuple`, so nothing
/// downstream refuses it the way a `TypedDict`'s own metaclass is refused — it was emitted
/// as a class with **no fields at all**, which built, imported, and answered
/// `Point(1, "x")` with a `TypeError` while `Point()` succeeded
fn base_is_special_form(model: &SemanticModel<'_>, base: &Expr) -> bool {
    matches!(
        base.inferred_type(model),
        Some(ty_python_semantic::types::Type::SpecialForm(_))
    )
}

/// the first base whose metaclass is not `type`, said as the tail of a decline —
/// which base it is and what it has instead
///
/// a class built from a type spec gets `type` as its own metaclass, so a base with
/// another one is a conflict python rejects. only a class **appending storage** to its
/// base needs the spec — one that adds no fields is built through its metaclass
/// instead — so this is what decides whether such a class can be compiled at all.
/// typeshed does not always record a metaclass, and an unrecorded one reads as `type`
/// here; the emitted module tests the bases again at import for that reason
///
/// what it found is named rather than only what it wanted, because the two are not the
/// same question and the answer decides what to do next. `abc.ABCMeta` is nearly the
/// whole of this over the standard library — an `io` abstract base under a class that
/// keeps a buffer of its own — and cpython refuses that outright from 3.14, where
/// `PyType_FromSpecWithBases` over such a base raises `TypeError: Metaclasses with
/// custom tp_new are not supported`. so a report saying `ABCMeta` is a report saying
/// "not without a construction other than a type spec", which the fixed wording was not
///
/// the answer is the *stub's*, and for the `io` family the stub and the interpreter
/// disagree: typeshed writes `TextIOWrapper(TextIOBase, ...)`, so its metaclass reads
/// as `ABCMeta`, while `io.py` only calls `TextIOBase.register(TextIOWrapper)` and the
/// runtime type keeps plain `type`. naming the metaclass is what makes that visible in
/// the report at all
///
/// a base that is not a class gets said so rather than given a metaclass it never had.
/// a module ending its own import on the platforms it does not serve — `raise
/// ImportError('win32 only')` — leaves everything below unreachable, and a base named
/// there settles on nothing; `asyncio`'s windows modules are the whole of that here
fn base_with_another_metaclass(
    db: &dyn ty_python_semantic::Db,
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
) -> Option<String> {
    class
        .arguments
        .iter()
        .flat_map(|arguments| arguments.args.iter())
        .find_map(|base| {
            let named =
                dotted_path(base).map_or_else(|| "a base".to_string(), |path| format!("`{path}`"));
            let Some(ty) = base.inferred_type(model) else {
                return Some(format!("{named} has no inferred type to read one off"));
            };
            if ty.has_default_metaclass(db) {
                return None;
            }
            if !ty.is_class_object(db) {
                return Some(format!("{named} is not a class the types settle on"));
            }
            Some(match ty.metaclass_name(db) {
                Some(metaclass) => format!("{named} has `{metaclass}`"),
                None => format!("{named} has one that does not settle on a class"),
            })
        })
}

/// whether a method of `class` stores an attribute on its own receiver through
/// `setattr`
///
/// the layout is the whole of an emitted instance — there is no `__dict__` behind it —
/// so an attribute has to be one of the fields, and the fields are the attributes the
/// body is seen to assign. `setattr` names its attribute as a *value*, which no layout
/// can record, so a class doing it would be emitted without somewhere to put what it
/// stores. leaving the class interpreted is the only answer that keeps the invariant.
///
/// the name is matched rather than resolved because the answer is a decline either way:
/// a module that binds `setattr` to something else loses a layout it might have had,
/// and never gains an instance that cannot hold its own attributes
fn stores_through_setattr(class: &ast::StmtClassDef) -> bool {
    struct Scan<'a> {
        receiver: &'a str,
        found: bool,
    }

    impl<'a> ast::visitor::Visitor<'a> for Scan<'_> {
        fn visit_expr(&mut self, expr: &'a Expr) {
            if let Expr::Call(call) = expr
                && let Expr::Name(callee) = call.func.as_ref()
                && callee.id.as_str() == "setattr"
                && let Some(Expr::Name(target)) = call.arguments.args.first()
                && target.id.as_str() == self.receiver
            {
                self.found = true;
            }
            ast::visitor::walk_expr(self, expr);
        }
    }

    class.body.iter().any(|statement| {
        let Stmt::FunctionDef(method) = statement else {
            return false;
        };
        let Some(receiver) = slot_zero(&method.parameters) else {
            return false;
        };
        let mut scan = Scan {
            receiver: receiver.parameter.name.as_str(),
            found: false,
        };
        ast::visitor::walk_body(&mut scan, &method.body);
        scan.found
    })
}

/// the declared fields of a class, or a decline explaining why it has no layout
fn class_fields(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    class: &ast::StmtClassDef,
    layouts: &Layouts,
    deleted: &HashSet<String>,
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    if stores_through_setattr(class) {
        return Err(Decline::new(
            "a `setattr` on the receiver names its attribute at runtime",
        ));
    }
    let (is_data, _) = class_modifiers(db, model, class)?;
    let base = base_class(db, env, model, suite, class, layouts)?;

    // a subclass's struct *begins* with its base's fields, in the same order and
    // unchanged, so a pointer to one is a valid pointer to the other — which is what
    // python's own single-inheritance layout rule requires. an inherited field keeps
    // whether it is optional along with its representation, because the presence byte
    // that answers it is part of what the base laid out
    let inherited: Vec<by_ir::function::FieldDecl> = base
        .as_ref()
        .and_then(ClassBase::in_module)
        .and_then(|base| layouts.get(base))
        .into_iter()
        .flatten()
        .cloned()
        .collect();
    // the inherited ones come first and nothing after that removes or reorders one, so
    // what this class adds of its own is whatever the field passes left past them
    let taken = inherited.len();
    let fields = if is_data {
        data_fields(db, env, model, class, layouts, inherited)?
    } else {
        // a plain class *is* its `__init__`: the fields are the attributes it gives
        // the instance, in the order it gives them, and a `__slots__` declares the
        // ones no assignment reached
        slot_fields(
            class,
            init_fields(db, env, model, class, layouts, inherited)?,
        )?
    };
    // a class that adds no field of its own keeps what its base keeps, at the offsets
    // the base laid them out, reached through the descriptors the base published — so
    // there is no region past the base's instance for it to own, and none of the three
    // slots that would reach one. it is built the way any other class with no storage of
    // its own is, and what it declares here is the same nothing
    let fields = if fields.len() == taken
        && appends_past_a_base_of_ours(db, env, model, suite, base.as_ref(), layouts)
    {
        Vec::new()
    } else {
        spec_built_where_needed(db, env, model, suite, class, base.as_ref(), layouts, fields)?
    };
    metaclass_carries_the_body(class, base.as_ref(), layouts)?;
    finalizer_reaches_a_dealloc_of_ours(class, base.as_ref())?;
    let mut fields =
        presence_where_a_finalizer_reads(db, env, model, suite, layouts, class, fields);
    for field in &mut fields {
        if deleted.contains(&field.name) {
            field.optional = true;
        }
    }
    Ok(fields)
}

/// whether a `__del__` this class writes is one the deallocation would ever reach
///
/// `tp_finalize` is reached from `tp_dealloc`, and the dealloc that would reach it
/// belongs to whichever class owns the instance layout. a class that extends a base is
/// freed through *that* base's, which may or may not call a finalizer at all — so the
/// cleanups would run by accident.
///
/// asked while the layouts are still settling, for the reason `metaclass_carries_the_body`
/// gives: a class turned down here leaves the layout set, so a subclass of one takes the
/// external base every declining class's subclass takes rather than being laid out on a
/// base that is never emitted and cascading behind it. `asyncio.selector_events` lost
/// every compiled definition it had that way — `_SelectorTransport` writes a `__del__`,
/// and the transport built on it dragged the event loop and ten of its generators down
fn finalizer_reaches_a_dealloc_of_ours(
    class: &ast::StmtClassDef,
    base: Option<&ClassBase>,
) -> Lowered<()> {
    if base.is_none() {
        return Ok(());
    }
    let finalizes = class.body.iter().any(|statement| {
        matches!(statement, Stmt::FunctionDef(method) if method.name.as_str() == "__del__")
    });
    if finalizes {
        return Err(Decline::new(
            "`__del__` is reached from the dealloc of the class that owns the layout, and this one extends a base",
        ));
    }
    Ok(())
}

/// whether a class only its metaclass can build carries what only the finished type
/// takes
///
/// a keyword can only reach the metaclass, and a base of ours beside one from outside
/// leaves nothing but the metaclass either. the metaclass decides what the class defines
/// from the *namespace* — which is settled before a method decorator, applied to the
/// finished type, could have run. an `abstractmethod` there also raises, since a compiled
/// method is a descriptor and takes no attributes.
///
/// a class-level constant is not in the same position, though it reads like it: it goes
/// into the namespace with the methods, and the class is asked afterwards whether it kept
/// the value — see `By_ConstantsHeldUp`. a decorator has no such answer, because what it
/// produces is only knowable by running it on a class that already exists.
///
/// this is asked while the layouts are still settling rather than while the body is
/// lowered, and where it turns a class down that class leaves the layout set — so a
/// subclass of one takes the same external base every other declining class's subclass
/// takes, rather than being laid out on a base that is never emitted and cascading behind
/// it. what it asks does not depend on the fields, so it sits at the end of the settling
/// where the more specific reasons the field passes give are reported first
fn metaclass_carries_the_body(
    class: &ast::StmtClassDef,
    base: Option<&ClassBase>,
    layouts: &Layouts,
) -> Lowered<()> {
    if class_keywords(class)?.is_empty() && !stands_on_an_emitted_base(base, layouts) {
        return Ok(());
    }
    let decorated = class.body.iter().any(|statement| {
        matches!(statement, Stmt::FunctionDef(method) if !method.decorator_list.is_empty())
    });
    if decorated {
        return Err(Decline::new(
            "a decorated method on a class built through its metaclass is not lowered yet",
        ));
    }
    Ok(())
}

/// the fields of a `data class`: the annotations its body writes, after its base's
fn data_fields(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    class: &ast::StmtClassDef,
    layouts: &Layouts,
    mut fields: Vec<by_ir::function::FieldDecl>,
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    for statement in &class.body {
        match statement {
            Stmt::AnnAssign(node) => {
                let Expr::Name(name) = node.target.as_ref() else {
                    return Err(Decline::new("only a plain attribute name is lowered yet"));
                };
                // the annotation key a class body writes is mangled like any other
                // private name, and a `data class` takes its fields from those keys
                let name = mangled(Some(&class.name), name.id.as_str());
                // only a *literal* default, for the reason a parameter default is:
                // it is evaluated once at class definition time and cannot change.
                // a mutable one is an error in a dataclass anyway
                let default = match &node.value {
                    None => None,
                    Some(value) => Some(literal_value(value).ok_or_else(|| {
                        Decline::new("only a literal field default is lowered yet")
                    })?),
                };
                let ty = node
                    .target
                    .inferred_type(model)
                    .ok_or_else(|| Decline::new("a field has no inferred type"))?;
                fields.push(by_ir::function::FieldDecl {
                    name,
                    ty: map_type_with(db, env, ty, layouts)?,
                    default,
                    optional: false,
                });
            }
            Stmt::FunctionDef(method) if method.name.starts_with("__") => {
                return Err(Decline::new(format!(
                    "`{}` on a data class is not lowered yet",
                    method.name
                )));
            }
            Stmt::FunctionDef(_) | Stmt::Pass(_) => {}
            Stmt::Expr(node) if matches!(node.value.as_ref(), Expr::StringLiteral(_)) => {}
            _ => return Err(Decline::new("only fields and methods are lowered yet")),
        }
    }
    Ok(fields)
}

/// the fields, unless the class needs a construction that cannot carry them or a
/// deallocation that cannot reach them
///
/// a class with fields of its own appends storage past its base's instance, and a type
/// spec is the only construction that can say where — so such a class needs `type` for
/// every base's metaclass and no class keyword to place. one that adds no fields is
/// built through its metaclass instead, and neither restriction reaches it
#[expect(clippy::too_many_arguments)]
fn spec_built_where_needed(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    class: &ast::StmtClassDef,
    base: Option<&ClassBase>,
    layouts: &Layouts,
    fields: Vec<by_ir::function::FieldDecl>,
) -> Lowered<Vec<by_ir::function::FieldDecl>> {
    if fields.is_empty() {
        return Ok(fields);
    }
    if base.and_then(ClassBase::external).is_some() && stands_on_an_emitted_base(base, layouts) {
        return Err(Decline::new(
            "a class with fields of its own cannot have a base this module emits beside one it does not",
        ));
    }
    if appends_past_a_base_of_ours(db, env, model, suite, base, layouts) {
        // the one base of ours this class *can* keep its storage past is one this module
        // builds from a spec of its own: such a base carries the three slots we emitted,
        // each reading the base to chain to from the type that declared it, so the chain
        // walks down to the outside base and stops. every other base of ours is a `class`
        // statement's type at import — the interpreted definition where the class
        // declined, or the one its metaclass built — and python's own three resolve from
        // `Py_TYPE(self)`, find this class's back, and recur until the stack runs out
        if !stands_on_a_spec_built_base(db, env, model, suite, class, base, layouts) {
            return Err(Decline::new(
                "a class whose fields sit past a base's instance needs a base python frees itself, and one this module builds from a spec is the only one of ours that is",
            ));
        }
        // the keyword such a class cannot carry is turned down earlier, where the base is
        // resolved: a base of ours beside a keyword has no construction whatever the
        // fields are, because the layout would have to be one only a spec lays out
        return Ok(fields);
    }
    if base.and_then(ClassBase::external).is_none() {
        return Ok(fields);
    }
    if !class_keywords(class)?.is_empty() {
        return Err(Decline::new(
            "a class keyword on a class with fields of its own is not lowered yet",
        ));
    }
    if let Some(found) = base_with_another_metaclass(db, model, class) {
        return Err(Decline::new(format!(
            "a class with fields of its own needs `type` for every base's metaclass, and {found}"
        )));
    }
    Ok(fields)
}

/// whether a class with storage of its own would keep it past an instance of a base this
/// module writes
///
/// reaching such storage takes three type slots of this class's own that call the base's.
/// python's own three resolve which base to chain to from the instance's type rather than
/// from the type that declared them, so they find this class's back and call it — a
/// recursion that ends as a stack overflow. a class this module *writes* carries exactly
/// those whichever way it is built: emitted from a spec, or left to the interpreted
/// definition where it declined.
///
/// a class that adds no field of its own asks nothing of this: there is no region past
/// the base's instance for it to own, so none of the three slots is its to supply
fn appends_past_a_base_of_ours(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    base: Option<&ClassBase>,
    layouts: &Layouts,
) -> bool {
    let appended = match base {
        None => false,
        Some(ClassBase::External(_)) => true,
        Some(ClassBase::InModule(name)) => {
            laid_out_from_outside(db, env, model, suite, layouts, name)
        }
    };
    appended
        && base.is_some_and(|base| {
            base.plain_names()
                .any(|name| class_written(suite, name).is_some())
        })
}

/// whether the base this class appends its storage to is one this module builds from a
/// type spec of its own — the one base of ours a spec can stand on
///
/// what makes that base different from every other class of ours is that its
/// `tp_dealloc`, `tp_traverse` and `tp_clear` are ones we emitted. each of the three
/// reads the base to chain to from the type that *declared* it, so a chain of them walks
/// down to the outside base and stops. `subtype_dealloc` — which is what a `class`
/// statement's type carries — reads it from `Py_TYPE(self)` instead, finds this class's
/// own deallocator, and calls it back until the stack runs out.
///
/// what the base *holds* is beside the point: a base with no fields of its own is no
/// safer, because a type spec that asks for none of the three slots is handed
/// `subtype_dealloc` just the same. so the backend gives such a base the three with
/// nothing in them, and the whole chain under this class has to be asked about rather
/// than only the rung immediately below
fn stands_on_a_spec_built_base(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    class: &ast::StmtClassDef,
    base: Option<&ClassBase>,
    layouts: &Layouts,
) -> bool {
    let Some(name) = base.and_then(ClassBase::in_module) else {
        // a base from outside is python's own to free, and this class's storage sits past
        // an instance it allocated — which is the shape a spec was made for
        return false;
    };
    frees_its_instances(
        db,
        env,
        model,
        suite,
        layouts,
        name,
        position_of(suite, class.name.as_str()),
    )
}

/// whether module init builds the class written under `name` from a type spec that frees
/// its own instances, and does so before the class written at `before`
///
/// the questions are exactly the ones the backend asks before it builds a base from a
/// spec, because the two have to name the same set: a class lowered here whose base the
/// backend then builds some other way has no construction left at all. so the base must
/// be one of ours, take its layout from outside, carry no class keyword, stand on no base
/// of ours beside one from outside, and be one a spec can be built on at all — which a
/// base whose metaclass is not `type` is not.
///
/// and it has to be written *before* the class standing on it, because module init builds
/// them in that order and that class's spec stands on the finished type.
///
/// then the same of the rung below, all the way down: each rung frees an instance by
/// calling the one under it, so a rung python built instead is where the walk turns back
fn frees_its_instances(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
    name: &str,
    before: usize,
) -> bool {
    let mut wanted = name.to_string();
    let mut before = before;
    // bounded the way [`laid_out_from_outside`] is: a chain that visits a class twice is
    // a cycle, and a cycle would otherwise spin here rather than settle
    for _ in 0..=suite.len() {
        let Some(written) = class_written(suite, &wanted) else {
            return false;
        };
        let at = position_of(suite, &wanted);
        if at >= before {
            return false;
        }
        // a class that declines has no layout, and its type is its interpreted
        // definition's — a `class` statement's, with `subtype_dealloc` on it
        if !layouts.contains_key(wanted.as_str()) {
            return false;
        }
        if !laid_out_from_outside(db, env, model, suite, layouts, &wanted) {
            return false;
        }
        // an empty list that was read, rather than one that could not be: a keyword this
        // module cannot lower is a keyword all the same, and it has nowhere to go in a
        // spec either way
        if !class_keywords(written).is_ok_and(|keywords| keywords.is_empty()) {
            return false;
        }
        if base_with_another_metaclass(db, model, written).is_some() {
            return false;
        }
        let under = base_class(db, env, model, suite, written, layouts)
            .ok()
            .flatten();
        // a class with no base at all owns its layout, which the walk above already
        // turned down — but saying so here rather than leaning on that keeps the two
        // answers from having to agree at a distance
        let Some(under) = under else {
            return false;
        };
        // a base list that reads as external but names a class this module *writes* is a
        // `class` statement's type at import whichever way that class went — the type we
        // built, or its interpreted definition where it declined. either is a heap type,
        // and a spec cannot be built on one: `By_SpecClass` reads the twin's own base and
        // turns a heap type down, which would leave the whole module interpreted. asked
        // of the source for the reason [`class_written`] gives
        if under.external().is_some()
            && under
                .plain_names()
                .any(|name| class_written(suite, name).is_some())
        {
            return false;
        }
        // a base from outside is one python allocates and frees, which is where the
        // chain of deallocators was walking to
        let Some(next) = under.in_module() else {
            return true;
        };
        before = at;
        wanted = next.to_string();
    }
    false
}

/// where in the module body the class statement under a name is written
///
/// a base written after the class that names it cannot be built first, and module init
/// builds the two in the order the source declares them
fn position_of(suite: &[Stmt], name: &str) -> usize {
    suite
        .iter()
        .position(
            |statement| matches!(statement, Stmt::ClassDef(class) if class.name.as_str() == name),
        )
        .unwrap_or(usize::MAX)
}

/// the class statement this module writes under a name, where it writes one
///
/// asked of the *source* rather than of the emitted set, because what a base name stands
/// for at import is a class either way — the type this module built, or the one its
/// interpreted definition did where the class declined
fn class_written<'a>(suite: &'a [Stmt], name: &str) -> Option<&'a ast::StmtClassDef> {
    suite.iter().find_map(|statement| match statement {
        Stmt::ClassDef(class) if class.name.as_str() == name => Some(class),
        _ => None,
    })
}

/// every field written down as one that may be absent, where a finalizer can read it
/// before `__init__` wrote it
///
/// a field is read as always written because `__init__` writes it, and `__del__` is the
/// one thing that runs on an instance whose `__init__` did not finish: python releases
/// the half-built object when the constructor raises, and releasing it is what calls the
/// finalizer. the fields it reaches are then the zeroes `tp_alloc` left, which a read of
/// ours handed straight on — `wave` and `tarfile` segfaulted there. saying they may be
/// absent costs a byte and a branch and answers the `AttributeError` python answers.
///
/// it is the whole *layout tree* that is marked, not the one class: a subclass's fields
/// begin with its base's, so a base marked one way and a subclass the other would put
/// the shared ones at two different offsets
fn presence_where_a_finalizer_reads(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
    class: &ast::StmtClassDef,
    mut fields: Vec<by_ir::function::FieldDecl>,
) -> Vec<by_ir::function::FieldDecl> {
    if fields.is_empty() || !layout_tree_finalizes(db, env, model, suite, layouts, class) {
        return fields;
    }
    for field in &mut fields {
        field.optional = true;
    }
    fields
}

/// every field a `del` in this module names, written down as one that may be absent
///
/// `del self.buffer` has somewhere to go only where the field can record that it is
/// gone, and that is the presence byte an optional one carries: the delete clears it,
/// a read while it is clear raises `AttributeError` the way python does, and a later
/// write sets it again. a field with no byte has no absent state at all, so a delete
/// on one can only refuse — which is the `cannot delete an attribute` an emitted class
/// used to answer where the interpreted twin deleted.
///
/// the names are collected module-wide rather than per class, for two reasons. a
/// delete need not be written in a method at all — `del handle.buffer` in a plain
/// function reaches the same field — and a subclass's layout begins with its base's,
/// so marking on the *name* keeps a base and a subclass agreeing about what the shared
/// fields cost without either having to work the other out
fn deleted_attributes(suite: &[Stmt]) -> HashSet<String> {
    /// the walk, carrying the class body each name is written in so that a private
    /// one is mangled the way python mangles it — `del self.__held` in `class C`
    /// names the field `_C__held`
    struct Deleted<'a> {
        owner: Option<&'a str>,
        names: HashSet<String>,
    }
    impl<'a> ruff_python_ast::visitor::Visitor<'a> for Deleted<'a> {
        fn visit_stmt(&mut self, stmt: &'a Stmt) {
            match stmt {
                Stmt::ClassDef(class) => {
                    let outer = self.owner.replace(class.name.as_str());
                    ruff_python_ast::visitor::walk_stmt(self, stmt);
                    self.owner = outer;
                }
                Stmt::Delete(node) => {
                    for target in &node.targets {
                        if let Expr::Attribute(attribute) = target {
                            self.names.insert(mangled(self.owner, &attribute.attr));
                        }
                    }
                    ruff_python_ast::visitor::walk_stmt(self, stmt);
                }
                _ => ruff_python_ast::visitor::walk_stmt(self, stmt),
            }
        }
    }
    let mut deleted = Deleted {
        owner: None,
        names: HashSet::new(),
    };
    for stmt in suite {
        ruff_python_ast::visitor::Visitor::visit_stmt(&mut deleted, stmt);
    }
    deleted.names
}

/// whether any class sharing this one's layout writes a `__del__`
///
/// the finalizers are collected first because most modules write none at all, and
/// working a layout out is a walk up the base chain through ty
fn layout_tree_finalizes(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
    class: &ast::StmtClassDef,
) -> bool {
    let finalizers: Vec<&ast::StmtClassDef> = suite
        .iter()
        .filter_map(|statement| {
            match statement {
            Stmt::ClassDef(candidate) => candidate
                .body
                .iter()
                .any(|member| {
                    matches!(member, Stmt::FunctionDef(method) if method.name.as_str() == "__del__")
                })
                .then_some(candidate),
            _ => None,
        }
        })
        .collect();
    if finalizers.is_empty() {
        return false;
    }
    let root = layout_root(db, env, model, suite, layouts, class);
    finalizers
        .into_iter()
        .any(|candidate| layout_root(db, env, model, suite, layouts, candidate) == root)
}

/// the topmost class of this module's own that this one's layout extends
///
/// two classes share a layout — one's struct beginning with the other's — exactly when
/// they share this
fn layout_root(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
    class: &ast::StmtClassDef,
) -> String {
    let mut current = class;
    // bounded the way [`laid_out_from_outside`] is
    for _ in 0..=suite.len() {
        let next = base_class(db, env, model, suite, current, layouts)
            .ok()
            .flatten()
            .and_then(|base| base.in_module().map(str::to_owned))
            .and_then(|name| class_written(suite, &name));
        match next {
            Some(next) => current = next,
            None => break,
        }
    }
    current.name.to_string()
}

/// whether the instances of the class written under this name are laid out by something
/// outside this module
///
/// the base chain, through the classes this module emits: where it ends on a name that is
/// not one of ours, that name owns the instance and every class under it keeps its own
/// fields past one rather than inside it. a chain that ends on `object` instead is laid
/// out here, and a subclass on it extends a struct rather than appending to one
fn laid_out_from_outside(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
    name: &str,
) -> bool {
    let Some(mut current) = class_written(suite, name) else {
        // not a class this module writes at all, so nothing here lays it out
        return true;
    };
    // bounded by the class count: a base chain cannot visit one twice without being a
    // cycle, and a cycle would otherwise spin here rather than settle
    for _ in 0..=suite.len() {
        match base_class(db, env, model, suite, current, layouts)
            .ok()
            .flatten()
        {
            None => return false,
            Some(ClassBase::External(_)) => return true,
            Some(ClassBase::InModule(base)) => match class_written(suite, &base) {
                Some(next) => current = next,
                None => return true,
            },
        }
    }
    true
}

/// whether a base this module emits stands in this class's list beside one it does not
///
/// such a class is built by calling its metaclass, and nothing else. a type spec takes
/// its whole instance shape from the one base python picks out of the list, so where
/// that is one of ours the `__dict__` an outside base needs is simply dropped — the type
/// then claims a managed dict it has no room for, and the first attribute read on an
/// instance walks off the object. `type` works the shape out from every base at once
fn stands_on_an_emitted_base(base: Option<&ClassBase>, layouts: &Layouts) -> bool {
    base.is_some_and(|base| {
        base.external().is_some() && base.plain_names().any(|name| layouts.contains_key(name))
    })
}

/// a resumable frame's *call* hands back the state object, whatever the annotation
/// says the body produces — the `await`, or the iteration, is what turns it back
///
/// there are three signature maps (module-level, methods, nested) and this has to be
/// true of all of them: a caller that believes the annotation instead assigns a
/// `PyObject *` into whatever representation that annotation had
fn resumable_return(function: &ast::StmtFunctionDef, signature: &mut Signature) {
    if generators::is_generator(&function.body) || function.is_async {
        signature.ret = RType::OBJECT;
    }
}

/// the `(index, array)` a `while` loop counts over, when it counts over one
///
/// the guard has to be exactly `i < len(A)`, `i` has to be a counter the body only
/// ever advances by a positive literal, and `A` has to be left alone — a rebind or an
/// `append` could move what the guard measured
fn counted_over(node: &ast::StmtWhile) -> Option<(String, String)> {
    let Expr::Compare(guard) = node.test.as_ref() else {
        return None;
    };
    let ([by_ir::ops::CmpOp::Lt], [bound]) = (
        guard
            .ops
            .iter()
            .map(|op| match op {
                ast::CmpOp::Lt => by_ir::ops::CmpOp::Lt,
                _ => by_ir::ops::CmpOp::Ne,
            })
            .collect::<Vec<_>>()
            .as_slice(),
        guard.comparators.as_ref(),
    ) else {
        return None;
    };
    let Expr::Name(index) = guard.left.as_ref() else {
        return None;
    };
    let Expr::Call(call) = bound else { return None };
    let Expr::Name(callee) = call.func.as_ref() else {
        return None;
    };
    if callee.id.as_str() != "len" || !call.arguments.keywords.is_empty() {
        return None;
    }
    let [Expr::Name(array)] = call.arguments.args.as_ref() else {
        return None;
    };
    let (index, array) = (index.id.to_string(), array.id.to_string());
    counting_body(&node.body, &index, &array).then_some((index, array))
}

/// whether the body advances `index` only by a positive literal and leaves `array` be
fn counting_body(body: &[Stmt], index: &str, array: &str) -> bool {
    for statement in walk(body) {
        let rebinds = |target: &Expr| {
            matches!(target, Expr::Name(name)
                if name.id.as_str() == array || name.id.as_str() == index)
        };
        match statement {
            Stmt::Assign(node) => {
                let [target] = node.targets.as_slice() else {
                    return false;
                };
                let Expr::Name(name) = target else { continue };
                if name.id.as_str() == array {
                    return false;
                }
                if name.id.as_str() == index && !advances(&node.value, index) {
                    return false;
                }
            }
            // `i += 1` is the same advance written the other way
            Stmt::AugAssign(node) => {
                let Expr::Name(name) = node.target.as_ref() else {
                    continue;
                };
                if name.id.as_str() == array {
                    return false;
                }
                if name.id.as_str() == index
                    && !(matches!(node.op, ast::Operator::Add) && positive_literal(&node.value))
                {
                    return false;
                }
            }
            Stmt::AnnAssign(node) if rebinds(&node.target) => return false,
            Stmt::For(node) if rebinds(&node.target) => return false,
            _ => {}
        }
    }
    // an `append` grows the array, and anything else it is handed could move it
    !walk(body).into_iter().any(|statement| {
        crate::closures::statement_expressions(statement)
            .into_iter()
            .any(|expr| escapes(expr, array))
    })
}

/// whether `value` is `<index> + <positive literal>`
fn advances(value: &Expr, index: &str) -> bool {
    let Expr::BinOp(node) = value else {
        return false;
    };
    matches!(node.op, ast::Operator::Add)
        && matches!(node.left.as_ref(), Expr::Name(name) if name.id.as_str() == index)
        && positive_literal(&node.right)
}

fn positive_literal(value: &Expr) -> bool {
    matches!(value, Expr::NumberLiteral(node)
        if matches!(&node.value, ast::Number::Int(int)
            if int.as_u64().is_some_and(|value| value > 0)))
}

/// whether `expr` hands `array` somewhere its length could change
///
/// deliberately blunt: anything but `len(A)` and `A[i]` is treated as reaching it,
/// because proving otherwise means proving what every callee does
fn escapes(expr: &Expr, array: &str) -> bool {
    let is_array =
        |candidate: &Expr| matches!(candidate, Expr::Name(name) if name.id.as_str() == array);
    let mut out = false;
    crate::closures::visit_expressions(expr, &mut |child| match child {
        Expr::Call(call) => {
            let bare_len =
                matches!(call.func.as_ref(), Expr::Name(name) if name.id.as_str() == "len");
            if !bare_len && call.arguments.args.iter().any(&is_array) {
                out = true;
            }
            if let Expr::Attribute(attribute) = call.func.as_ref()
                && is_array(&attribute.value)
            {
                out = true;
            }
        }
        // `A` handed to anything that builds a container keeps a reference to it
        Expr::List(node) => out = out || node.elts.iter().any(&is_array),
        Expr::Tuple(node) => out = out || node.elts.iter().any(&is_array),
        _ => {}
    });
    out
}

/// the attributes every path through `__init__` assigns
///
/// a field is always present, where python raises `AttributeError` for one that was
/// never written — so a layout may only hold what *every* path fills. an `if` with no
/// `else` contributes nothing, and neither does a loop or a `try` body: each has a
/// path through it that assigns nothing
fn definitely_assigned_attributes(
    body: &[Stmt],
    receiver: &str,
    owner: Option<&str>,
) -> HashSet<String> {
    completing_assignments(body, receiver, owner).unwrap_or_default()
}

/// as [`definitely_assigned_attributes`], with `None` where the path never completes
///
/// a branch that *raises* produces no object at all, so it cannot leave a field
/// unfilled and has nothing to say about the layout — which is what makes the
/// validate-or-raise shape compile. a `return` is different: it completes, with
/// whatever was assigned by then
fn completing_assignments(
    body: &[Stmt],
    receiver: &str,
    owner: Option<&str>,
) -> Option<HashSet<String>> {
    let mut out = HashSet::new();
    for statement in body {
        // the writes this statement makes in its own right, whatever form it is. a
        // declining shape is not reported here: the width pass walks the same statements
        // and has already turned the class down over it
        out.extend(
            certain_writes(statement, receiver, owner)
                .unwrap_or_default()
                .into_iter()
                .map(|(name, _)| name),
        );
        match statement {
            // the body runs, so what it assigns is assigned — and if it never
            // completes, neither does this
            Stmt::With(node) => {
                out.extend(completing_assignments(&node.body, receiver, owner)?);
            }
            Stmt::If(node) => {
                // only a chain that ends in `else` covers every case; without one
                // there is a path through it that assigns nothing
                let covered = node
                    .elif_else_clauses
                    .last()
                    .is_some_and(|last| last.test.is_none());
                let branches = std::iter::once(completing_assignments(&node.body, receiver, owner))
                    .chain(
                        node.elif_else_clauses
                            .iter()
                            .map(|clause| completing_assignments(&clause.body, receiver, owner)),
                    );
                // a branch that never completes has nothing to say about the layout
                let mut completing = branches.flatten();
                let Some(mut shared) = completing.next() else {
                    // every branch raised, so this `if` never completes either — but
                    // only when it covered every case
                    if covered {
                        return None;
                    }
                    continue;
                };
                for branch in completing {
                    shared.retain(|name| branch.contains(name));
                }
                if covered {
                    out.extend(shared);
                }
            }
            // `return` completes, with what was assigned by now; `raise` does not
            Stmt::Return(_) => break,
            Stmt::Raise(_) => return None,
            _ => {}
        }
    }
    Some(out)
}

/// what a zero-argument `super()` written in this method stands for, if anything
///
/// python's compiler fills the two arguments in from the frame: `__class__` is the
/// class the `def` is written in, and the receiver is whatever is in slot zero — the
/// *slot*, not the argument, so a method that assigns to its own receiver moves what
/// `super()` sees.
///
/// so the method has to have a slot zero at all: `def m(*args)` and `def m(*, k)`
/// both leave `co_argcount` at nought and python raises there. and what is in it has
/// to be the receiver, which `classmethod` and `staticmethod` both say it is not —
/// as do the dunders python makes implicitly static or class methods
fn zero_argument_super(
    function: &ast::StmtFunctionDef,
    receiver: Option<Receiver<'_>>,
) -> Result<ZeroSuper, &'static str> {
    let owner = match receiver {
        Some(Receiver::Explicit(RType::Instance { class, .. })) => class,
        // the environment a nested function or a lambda reads through. python gives
        // one a frame of its own, whose slot zero is its own first argument — and it
        // raises outright where the nested function takes none
        Some(Receiver::Implicit(_)) => {
            return Err(
                "a `super()` with no arguments reads the nested function's own slot zero, not the method's receiver",
            );
        }
        _ => {
            return Err(
                "a `super()` with no arguments is sugar for the method it is written in, and this is not one",
            );
        }
    };
    if matches!(
        function.name.as_str(),
        "__new__" | "__init_subclass__" | "__class_getitem__"
    ) {
        return Err(
            "python makes this method implicitly static or class, so slot zero holds the class rather than a receiver",
        );
    }
    // `static` and `classmethod` are how basedpython spells the first two, so the
    // marker forms have to be read here as well as the `@` forms
    let rebinds_slot_zero = function.decorator_list.iter().any(|decorator| {
        matches!(&decorator.expression, Expr::Name(name)
            if matches!(name.id.as_str(), "classmethod" | "staticmethod" | "static"))
    });
    if rebinds_slot_zero {
        return Err(
            "`classmethod` and `staticmethod` both leave something other than the receiver in slot zero",
        );
    }
    let receiver = slot_zero(&function.parameters).ok_or(
        "a `super()` with no arguments reads slot zero, and this method has no positional parameter to fill one",
    )?;
    Ok(ZeroSuper {
        owner: owner.clone(),
        receiver: receiver.parameter.name.to_string(),
    })
}

/// which of python's three method conventions this definition asks for, with the
/// decorator that asked for it taken off the list
///
/// `staticmethod` and `classmethod` are not applied at module init like every other
/// decorator: the method table entry carries `METH_STATIC` or `METH_CLASS`, and the
/// type builds the descriptor python would have built. so honouring one means dropping
/// it, and the two must not both happen.
///
/// that only holds where it is the **only** decorator. the runtime folds the rest onto
/// the attribute it reads back off the finished type — and reading a static method back
/// hands over the plain function, which would then be written back as an ordinary
/// method. a second decorator keeps the decline
fn method_binding(
    function: &ast::StmtFunctionDef,
    receiver: Option<Receiver<'_>>,
    decorators: &mut Vec<Decorator>,
) -> Lowered<Binding> {
    if !matches!(receiver, Some(Receiver::Explicit(_))) {
        return Ok(Binding::Instance);
    }
    // only a bare name says which convention: `abc.abstractmethod` is not one however
    // its last segment reads, and neither is any other attribute off something else
    let convention = |decorator: &Decorator| match decorator.as_name()? {
        "staticmethod" => Some(Binding::Static),
        "classmethod" => Some(Binding::Class),
        _ => None,
    };
    // python makes `__new__` a static method however it is written, and hands it the
    // *class* as its first argument rather than as a receiver — so this is the one method
    // whose convention the source does not choose. writing `@staticmethod` over it says
    // the same thing a second time, so it comes off the list; anything else would be
    // applied at init to a name that is bound by the assignment publishing the method,
    // and the decorated value would be the one that assignment then overwrote
    if function.name.as_str() == "__new__" {
        decorators.retain(|decorator| decorator.as_name() != Some("staticmethod"));
        if !decorators.is_empty() {
            return Err(Decline::new(
                "a decorator over `__new__` is applied at init to the name the published method binds, so the construction would not reach the decorated value",
            ));
        }
        // a `__new__` that suspends hands back a generator rather than an instance, and
        // its state object would be namespaced by a receiver this convention has none of
        if generators::is_generator(&function.body) || function.is_async {
            return Err(Decline::new("a `__new__` that suspends is not lowered yet"));
        }
        return Ok(Binding::Static);
    }
    let Some(binding) = decorators.iter().find_map(convention) else {
        return Ok(Binding::Instance);
    };
    if decorators.len() > 1 {
        return Err(Decline::new(
            "a second decorator over `classmethod` or `staticmethod` is folded onto the attribute read back off the type, which is no longer the one it was",
        ));
    }
    // python already makes these implicitly static or class, and the emitted type
    // publishes its own `__class_getitem__` for a generic class — so a table entry of
    // our own would either double the convention or collide with that one
    if matches!(
        function.name.as_str(),
        "__init_subclass__" | "__class_getitem__"
    ) {
        return Err(Decline::new(
            "python gives this method a convention of its own, which a method table entry would duplicate",
        ));
    }
    // a generator's state object is namespaced by the receiver's class, and neither of
    // these has one — so two classes with a static `values` would want one state class
    // between them
    if generators::is_generator(&function.body) || function.is_async {
        return Err(Decline::new(
            "a `classmethod` or `staticmethod` that suspends is not lowered yet",
        ));
    }
    decorators.clear();
    Ok(binding)
}

/// a method: an ordinary function whose exported name is namespaced by the class
fn lower_method(
    unit: Unit<'_>,
    method: &ast::StmtFunctionDef,
    class: &str,
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    let receiver = RType::Instance {
        class: class.to_string(),
        exact: false,
    };
    let (mut lowered, environments) = lower_function_with_receiver(
        unit,
        method,
        Some(Receiver::Explicit(&receiver)),
        None,
        &[],
        Frame::AsWritten,
    )?;
    // a `def __helper` in a class body is bound as `_C__helper`, in the method table
    // as everywhere else
    lowered.name = mangled(Some(class), &lowered.name);
    // the symbol is class-qualified so a method and a module-level function may
    // share a python-visible name
    lowered.owner = Some(class.to_string());
    // a method is reached through the type object, not the module namespace
    lowered.exported = false;
    Ok((lowered, environments))
}

fn lower_function(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    lower_function_with_receiver(unit, function, None, None, &[], Frame::AsWritten)
}

/// which frame a body is being given
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    /// the one the `def` asks for: a generator or an `async def` gets a state class,
    /// and the call allocates it rather than running anything
    AsWritten,
    /// no state class at all — the body runs straight through, which is what a
    /// coroutine that [never suspends](generators::never_suspends) does on its only
    /// `send`
    Straight,
}

/// the same body as the ordinary call an `await` of it makes
///
/// a coroutine that never suspends has one entry and one exit, so the object it builds
/// carries nothing between them. this is that body with no object at all: an `await` of
/// the name reaches it directly, and everything else still gets the real coroutine
fn lower_direct_edition(unit: Unit<'_>, function: &ast::StmtFunctionDef) -> Lowered<Function> {
    let (mut lowered, environments) =
        lower_function_with_receiver(unit, function, None, None, &[], Frame::Straight)?;
    if !environments.is_empty() {
        return Err(Decline::new(
            "a direct edition of a coroutine that makes closures is not lowered yet",
        ));
    }
    lowered.name = generators::direct_name(&function.name);
    lowered.exported = false;
    // the decorators belong to the name, which this edition does not hold — and
    // applying them at init would rebind a name the namespace never had
    lowered.decorators = Vec::new();
    lowered.coroutine_body = Some(function.name.to_string());
    Ok(lowered)
}

/// the same source lowered a second time, with its buffer-shaped parameters taken
/// unboxed
///
/// one body cannot have two representations, so an in-unit caller holding a buffer
/// and a caller from python holding a list need two — this is the one the boundary
/// never reaches
fn lower_array_edition(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
    arrays: &[(usize, RType)],
) -> Lowered<Function> {
    let (mut lowered, environments) =
        lower_function_with_receiver(unit, function, None, None, arrays, Frame::AsWritten)?;
    if !environments.is_empty() {
        return Err(Decline::new(
            "an unboxed edition of a function that makes closures is not lowered yet",
        ));
    }
    lowered.name = edition_name(&function.name, arrays);
    lowered.exported = false;
    Ok(lowered)
}

/// as [`lower_function`], with the first parameter's representation forced
///
/// a method's `self` *is* an instance of its class, which ty may spell as `Self`
/// — a type whose class name the mapper cannot read. forcing it here is what turns
/// `self.x` into a field read
fn lower_function_with_receiver(
    unit: Unit<'_>,
    function: &ast::StmtFunctionDef,
    receiver: Option<Receiver<'_>>,
    captures: Option<&closures::Nested>,
    arrays: &[(usize, RType)],
    frame: Frame,
) -> Lowered<(Function, Vec<by_ir::function::ClassIr>)> {
    let Unit {
        env,
        db,
        model,
        native_callees,
        layouts,
        methods,
        signatures,
        ..
    } = unit;
    // a name this frame declares `global` gets no register and no environment field:
    // both halves of it are the module namespace, reached through `Place::Global`.
    // keeping it out of the locals here is what makes that true — a register declared
    // for it would be what a nested function captured, and the two would disagree
    let declared_global = declared_globals(&function.body);
    // a decorator is applied at module init to the installed native function, so the
    // body still compiles. `decorator_path` says which expressions mean the same thing
    // evaluated there as they did where the `def` stood.
    //
    // a *modifier* is not a name at all — it is spelled without an `@` and the
    // transpiler rewrites it — so it is translated to whatever the interpreted twin
    // ended up with, or dropped where the twin has nothing
    // a *nested* function's decorators belong to the frame the `def` stands in, which
    // applies them to the closure it just made — see `nested_def`. carrying them here
    // as well would apply them a second time, to the environment class's method
    let nested = matches!(receiver, Some(Receiver::Implicit(_)));
    let mut decorators = Vec::with_capacity(function.decorator_list.len());
    for decorator in function.decorator_list.iter().filter(|_| !nested) {
        match function_modifier(db, model, decorator)? {
            Modifier::Apply(name) => decorators.push(name),
            Modifier::Erased | Modifier::DataClass => {}
        }
    }
    // a method's first parameter is forced to the receiver, because that is what python
    // puts in slot zero — and `staticmethod` and `classmethod` are exactly the two that
    // say it is not. the method table entry says which, so the decorator is honoured by
    // the emitted type rather than applied to it, and comes off the list here
    let binding = method_binding(function, receiver, &mut decorators)?;
    if receiver.is_none() {
        decorator_stays_unread(unit.read, function.name.as_str(), &decorators)?;
    }
    // a class method's slot zero holds the *class*: an ordinary object, and pointedly
    // not an instance of the layout, so nothing reads a field off it. a static method
    // has no slot zero at all, and its first written parameter keeps its own type
    let class_object = RType::OBJECT;
    // what the `def` was *written* as a method of, which a zero-argument `super()` is
    // asked about — it has its own account of why neither of these fills slot zero with
    // a receiver, and the effective one no longer says which class the method is on
    let declared_receiver = receiver;
    // `__new__` binds every parameter out of the argument vector the way any other static
    // method does — the dispatcher python installs for it puts the class in front — while
    // that first parameter is still the *class*, so it is typed as the plain object a
    // class is. reading a field off it would otherwise treat a type as an instance
    let takes_the_class = binding == Binding::Class || function.name.as_str() == "__new__";
    let receiver = match binding {
        Binding::Instance => receiver,
        Binding::Static if takes_the_class => Some(Receiver::Explicit(&class_object)),
        Binding::Static => None,
        Binding::Class => Some(Receiver::Explicit(&class_object)),
    };
    if takes_the_class {
        class_object_is_not_written(function, unit.owner)?;
    }

    // a generator and a coroutine do not run their body when called: they allocate a
    // state object and hand it back. the body becomes a method of that object
    if frame == Frame::AsWritten && (generators::is_generator(&function.body) || function.is_async)
    {
        // a nested one keeps its captures in the *state* object rather than reaching
        // back through the environment: the frame outlives the call that made it, and
        // a copy is what a capture already is. a *shared* one is a cell both frames
        // write, which a copy cannot be
        if let Some(captured) = captures
            && !captured.shared.is_empty()
        {
            return Err(Decline::new(
                "a generator that shares a cell with the frame around it is not lowered yet",
            ));
        }
        return lower_generator(unit, function, decorators, receiver, captures);
    }

    let Signature {
        params,
        defaults,
        vararg,
        kwarg,
        posonly,
        kwonly,
        ret,
        deferring,
        computed_defaults,
    } = signature(db, env, model, function, layouts, receiver, arrays)?;

    // a boundary that hands the call on takes the twin off the interpreted class, and
    // for a class method that is already *bound* — to the interpreted class, not to the
    // one in slot zero. handing it the class as well would give the body two of them
    if binding == Binding::Class && !computed_defaults.is_empty() {
        return Err(Decline::new(
            "a `classmethod` whose default is not an immediate would reach a twin already bound to the interpreted class",
        ));
    }

    // a nested function lives on a generated environment class, whose fields are
    // the captures. it has to exist before the body is lowered, because the `def`
    // statement allocates it
    let mut locals_here =
        local_representations(db, env, model, &function.body, layouts, unit.arrays);
    locals_here.retain(|(name, _)| !declared_global.contains(name));
    let bound: HashSet<String> = params
        .iter()
        .map(|(name, _)| name.clone())
        .chain(locals_here.iter().map(|(name, _)| name.clone()))
        .collect();
    // a name this frame *captures* is bound here too, as far as anything nested inside
    // is concerned — otherwise a function two deep would resolve it as a global
    let bound: HashSet<String> = bound
        .into_iter()
        .chain(
            captures
                .into_iter()
                .flat_map(|captured| captured.captures.iter().cloned()),
        )
        .collect();
    // only a name *this* frame never writes may be captured by copy. a capture is never
    // written here by definition — whether the frame it came from writes it is a
    // separate question, answered where the environment is seeded
    let written: HashSet<String> = locals_here.iter().map(|(name, _)| name.clone()).collect();
    let never_written: HashSet<String> = bound.difference(&written).cloned().collect();
    // with per-iteration bindings a captured loop target is a *copy* rather than a
    // shared cell, and the environment holding it is re-allocated at each closure
    let per_iteration = if unit.unique_loop_bindings {
        closures::loop_targets(&function.body)
    } else {
        HashSet::new()
    };
    let nested =
        closures::nested_functions(&function.body, &bound, &never_written, &per_iteration)?;
    // a *captured* name has no register here, so its representation comes from the
    // layout of the environment this frame reads it out of
    let outer_layout = receiver
        .and_then(|receiver| match receiver.rtype() {
            RType::Instance { class, .. } => layouts.get(class),
            _ => None,
        })
        .map(Vec::as_slice)
        .unwrap_or_default();
    let representation = |name: &str| {
        params
            .iter()
            .chain(locals_here.iter())
            .map(|(candidate, rtype)| (candidate, rtype))
            .chain(outer_layout.iter().map(|field| (&field.name, &field.ty)))
            .find(|(candidate, _)| *candidate == name)
            .map(|(_, rtype)| rtype.clone())
    };
    let enclosing = receiver.and_then(|receiver| match receiver.rtype() {
        RType::Instance { class, .. } => Some(class.as_str()),
        _ => None,
    });
    let owned: HashSet<String> = params
        .iter()
        .map(|(name, _)| name.clone())
        .chain(locals_here.iter().map(|(name, _)| name.clone()))
        .collect();
    // a cell has to outlive every closure that shares it; a loop binding has to be
    // fresh for each one. when a frame needs both, they live in *two* environments —
    // the closure's holds the bindings and reaches the cells through `$outer`, which
    // is the same chain a function nested two deep already walks
    let cells_here: HashSet<&str> = nested
        .iter()
        .flat_map(|entry| entry.shared.iter().map(String::as_str))
        .collect();
    let bindings_here: HashSet<String> = nested
        .iter()
        .flat_map(|entry| entry.captures.iter())
        .filter(|name| per_iteration.contains(name.as_str()))
        .cloned()
        .collect();
    let split = !cells_here.is_empty() && !bindings_here.is_empty();

    // the *name* is qualified by the class the `def` was written in even where the
    // frame has no receiver of that class, or a static `parse` and a module-level
    // `parse` would ask for one environment class between them. the chain is not:
    // `enclosing` is what says whether there is an outer frame to reach through, and
    // neither of these has one
    let frame_name = closures::environment_name(
        enclosing.or_else(|| unit.owner.filter(|_| binding != Binding::Instance)),
        &function.name,
    );
    let frame_owned: HashSet<String> = if split {
        owned.difference(&bindings_here).cloned().collect()
    } else {
        owned
    };
    let outer_environment = closures::environment(
        &frame_name,
        enclosing,
        &nested,
        &representation,
        &frame_owned,
    )?;
    let (environment, outer_environment) = if split {
        let closure = closures::environment(
            &format!("{frame_name}$closure"),
            Some(&frame_name),
            &nested,
            &representation,
            &bindings_here,
        )?;
        (closure, outer_environment)
    } else {
        (outer_environment, None)
    };

    // the environment's own layout and method signatures, so the enclosing body can
    // both allocate it and call straight into its methods
    let mut layouts_with_env = layouts.clone();
    let mut methods_with_env = methods.clone();
    if let Some(outer) = &outer_environment {
        layouts_with_env.insert(outer.name.clone(), outer.fields.clone());
    }
    if let Some(environment) = &environment {
        layouts_with_env.insert(environment.name.clone(), environment.fields.clone());
        let receiver = RType::Instance {
            class: environment.name.clone(),
            exact: false,
        };
        let table = nested
            .iter()
            .filter_map(|entry| {
                let mut signature = signature(
                    db,
                    env,
                    model,
                    &entry.def,
                    &layouts_with_env,
                    Some(Receiver::Implicit(&receiver)),
                    &[],
                )
                .ok()?;
                resumable_return(&entry.def, &mut signature);
                Some((entry.def.name.to_string(), signature))
            })
            .collect();
        methods_with_env.insert(environment.name.clone(), table);
    }
    let layouts = &layouts_with_env;
    let methods = &methods_with_env;

    let mut builder = FunctionBuilder::new(function.name.to_string(), ret.clone());
    builder.at(span(function.range));
    builder.decorators(decorators);
    builder.defaults(defaults);
    builder.variadic(vararg, kwarg);
    builder.binding_kinds(posonly, kwonly);
    builder.deferring(deferring);
    builder.computed_defaults(computed_defaults);
    let mut locals: HashMap<String, RegisterId> = HashMap::new();

    for (name, rtype) in &params {
        let id = builder.param(name.clone(), rtype.clone());
        locals.insert(name.clone(), id);
    }

    // a local's representation has to cover *every* value assigned to it, not
    // just the first. `acc = 0` followed by `acc = acc + <object>` makes `acc` an
    // object, and deciding that from the first assignment alone would decline the
    // function
    // a name either frame writes is one cell, so neither frame gives it a register:
    // this frame reads and writes the *field*, and so does the nested one
    let shared: HashSet<String> = nested
        .iter()
        .flat_map(|entry| entry.shared.iter().cloned())
        .chain(
            captures
                .into_iter()
                .flat_map(|captured| captured.shared.iter().cloned()),
        )
        .collect();

    for (name, rtype) in &locals_here {
        if shared.contains(name) {
            continue;
        }
        // a parameter of the same name is already declared and wins
        if let std::collections::hash_map::Entry::Vacant(slot) = locals.entry(name.clone()) {
            slot.insert(builder.local(name.clone(), rtype.clone()));
        }
    }

    // the frame environment's register, when the cells live in one of their own
    let outer_register = outer_environment.as_ref().map(|outer| {
        builder.local(
            format!("${}", outer.name),
            RType::Instance {
                class: outer.name.clone(),
                exact: false,
            },
        )
    });
    // the environment register, if this function makes closures
    let env_register = environment.as_ref().map(|environment| {
        builder.local(
            format!("${}", environment.name),
            RType::Instance {
                class: environment.name.clone(),
                exact: false,
            },
        )
    });

    // the frame's own environment holds the cells, and is allocated once — that is
    // what makes a cell shared. the closures reach it through `$outer`
    if let Some((outer, register)) = outer_environment.as_ref().zip(outer_register) {
        let mut values = Vec::with_capacity(outer.fields.len());
        for field in &outer.fields {
            if field.name == closures::OUTER_FIELD {
                values.push(Some(Value::Register(RegisterId(0))));
                continue;
            }
            let is_parameter = locals
                .get(&field.name)
                .is_some_and(|id| id.index() < params.len());
            values.push(match locals.get(&field.name) {
                Some(&id) if is_parameter && shared.contains(&field.name) => {
                    Some(boxed_object(&mut builder, id))
                }
                Some(&id) if !shared.contains(&field.name) => Some(Value::Register(id)),
                _ => None,
            });
        }
        builder.push(Op::NewInstance {
            dest: register,
            class: outer.name.clone(),
            fields: values,
        });
    }

    // the environment is allocated *before the body*, not at the first `def`: a
    // shared cell can be read or written before the `def` that closes over it
    let mut per_closure: Option<Vec<String>> = None;
    if let Some((environment, register)) = environment.as_ref().zip(env_register) {
        let mut values = Vec::with_capacity(environment.fields.len());
        for field in &environment.fields {
            // the chain: a nested environment holds this frame's own receiver, so a
            // name further up is reached by walking rather than copying — which is the
            // only way a shared cell up there stays one cell
            if field.name == closures::OUTER_FIELD {
                values.push(Some(Value::Register(RegisterId(0))));
                continue;
            }
            let is_parameter = locals
                .get(&field.name)
                .is_some_and(|id| id.index() < params.len());
            values.push(match locals.get(&field.name) {
                // only a *parameter* seeds its cell — it is assigned on entry. every
                // other cell starts unset, so reading it before a write is
                // `UnboundLocalError`, exactly as python reports it
                Some(&id) if is_parameter && shared.contains(&field.name) => {
                    Some(boxed_object(&mut builder, id))
                }
                Some(&id) if !shared.contains(&field.name) => Some(Value::Register(id)),
                _ => None,
            });
        }
        // an environment with no cells *of its own* is re-allocated at each closure
        // instead, so a loop's values are the ones the closure was written with
        if (shared.is_empty() || split) && !per_iteration.is_empty() {
            per_closure = Some(
                environment
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect(),
            );
        } else {
            builder.push(Op::NewInstance {
                dest: register,
                class: environment.name.clone(),
                fields: values,
            });
        }
    }

    // a *parameter* that is a shared cell seeded the field above; from here on the
    // name resolves to the cell, so the register goes
    locals.retain(|name, _| !shared.contains(name));

    // the fields the receiver actually has. a capture the receiver does *not* hold
    // lives further up, and resolving it to a field here would read an unset one
    let held: HashSet<&str> = enclosing
        .and_then(|class| layouts.get(class))
        .into_iter()
        .flatten()
        .map(|field| field.name.as_str())
        .collect();

    // this frame's own field access: either the captures it reads as a *nested*
    // function, or the shared cells it owns as an *enclosing* one
    let own_captures = match (captures, environment.as_ref()) {
        (Some(captured), _) => Some(Captured {
            class: receiver
                .and_then(|receiver| match receiver.rtype() {
                    RType::Instance { class, .. } => Some(class.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            // the receiver of a nested function is its own first parameter
            receiver: RegisterId(0),
            names: captured
                .captures
                .iter()
                .filter(|name| !captured.shared.contains(name))
                .filter(|name| held.contains(name.as_str()))
                .cloned()
                .collect(),
            cells: captured
                .shared
                .iter()
                .filter(|name| held.contains(name.as_str()))
                .cloned()
                .collect(),
            free: true,
        }),
        (None, Some(environment)) if !shared.is_empty() => Some(Captured {
            class: outer_environment
                .as_ref()
                .map_or_else(|| environment.name.clone(), |outer| outer.name.clone()),
            receiver: outer_register.or(env_register).unwrap_or(RegisterId(0)),
            names: HashSet::new(),
            cells: shared.clone(),
            free: false,
        }),
        (None, _) => None,
    };

    let zero_super = zero_argument_super(function, declared_receiver);

    let mut lowering = Lowering {
        arrays: unit.arrays,
        directs: unit.directs,
        in_range: Vec::new(),
        mutable: unit.mutable,
        slotted: unit.slotted,
        constructs: unit.constructs,
        bases: unit.bases,
        properties: unit.properties,
        db,
        model,
        builder,
        locals,
        globals: declared_global,
        native_callees,
        decorated: unit.decorated,
        layouts,
        methods,
        signatures,
        ret,
        loops: Vec::new(),
        handling: Vec::new(),
        owner: unit.owner.map(str::to_string),
        zero_super,
        comprehensions: 0,
        generator: None,
        delegations: 0,
        contexts: 0,
        cleanups: Vec::new(),
        captures: own_captures,
        environment: environment.as_ref().map(|environment| Closures {
            class: environment.name.clone(),
            register: env_register.unwrap_or(RegisterId(0)),
            lambdas: nested
                .iter()
                .filter_map(|entry| {
                    entry
                        .lambda
                        .map(|range| (span(range), entry.def.name.to_string()))
                })
                .collect(),
            ready: HashSet::new(),
            outer: outer_register,
            per_closure,
        }),
    };
    lowering.block(&function.body)?;

    // a body that falls off the end returns `None` — python's implicit return. what it
    // needs is a representation the declared return type can hold: the plain fit for
    // `None`, a widening for `object`. anything narrower is a type error the checker has
    // already reported, and there is genuinely nothing to return there
    if !lowering.builder.is_sealed(lowering.builder.current_block()) {
        let ret = lowering.ret.clone();
        let value = lowering
            .coerce(Value::None, &RType::NONE, &ret)
            .map_err(|_| {
                Decline::new("control reaches the end of a function that must return a value")
            })?;
        lowering.builder.terminate(Terminator::Return(value));
    }

    let mut lowered = lowering.builder.finish();
    lowered.binding = binding;
    // the environment's methods are the nested bodies, lowered with the environment
    // as the receiver — so a captured read is a field read like any other
    let environments = match environment {
        None => Vec::new(),
        Some(environment) => {
            let receiver = RType::Instance {
                class: environment.name.clone(),
                exact: false,
            };
            // the environment is a class like any other, and its methods have to be
            // able to see its layout — otherwise a captured read has no field type
            let inner_unit = Unit {
                layouts,
                methods,
                ..unit
            };
            let mut lowered_methods = Vec::with_capacity(nested.len());
            // an environment a *nested* function needed is another sibling: a function
            // two levels deep has one of its own
            let mut inner_environments = Vec::new();
            for entry in &nested {
                let (mut method, inner) = lower_function_with_receiver(
                    inner_unit,
                    &entry.def,
                    Some(Receiver::Implicit(&receiver)),
                    Some(entry),
                    &[],
                    Frame::AsWritten,
                )?;
                inner_environments.extend(inner);
                method.owner = Some(environment.name.clone());
                method.exported = false;
                lowered_methods.push(method);
            }
            // the frame's own environment is a sibling with no methods: it exists to
            // hold the cells the closures reach through `$outer`. it comes *first*
            // because the closure's struct names it
            let mut all: Vec<by_ir::function::ClassIr> = outer_environment
                .into_iter()
                .map(|outer| by_ir::function::ClassIr {
                    resume: None,
                    keywords: Vec::new(),
                    exported: false,
                    declares_slots: false,
                    decorators: Vec::new(),
                    constants: Vec::new(),
                    slot_aliases: Vec::new(),
                    generic: false,
                    properties: Vec::new(),
                    name: outer.name,
                    fields: outer.fields,
                    methods: Vec::new(),
                    base: None,
                    inherited_init: false,
                    immutable: false,
                })
                .collect();
            all.push(by_ir::function::ClassIr {
                resume: None,
                keywords: Vec::new(),
                exported: false,
                declares_slots: false,
                decorators: Vec::new(),
                constants: Vec::new(),
                slot_aliases: Vec::new(),
                generic: false,
                properties: Vec::new(),
                name: environment.name.clone(),
                fields: environment.fields,
                methods: lowered_methods,
                base: None,
                inherited_init: false,
                immutable: false,
            });
            all.extend(inner_environments);
            all
        }
    };
    Ok((lowered, environments))
}

/// the signature of every method of every emitted class, by class then method
type Methods = HashMap<String, HashMap<String, Signature>>;

/// the parts of a module's lowering that do not change from function to function
///
/// they all travel together to every entry point, so they travel as one thing.
/// `layouts` is the exception that has to be *replaced* rather than shared: an
/// environment's methods need to see the environment's own layout
#[derive(Clone, Copy)]
struct Unit<'a> {
    /// the environment the module is being checked in, which every type query needs
    env: &'a ProgramEnvironment<'a>,
    /// whether a closure made in a loop binds *that* iteration's values, which is
    /// the language's default and the transpiler's. it decides one thing here:
    /// whether a captured loop target is a shared cell or a per-closure copy
    unique_loop_bindings: bool,
    db: &'a dyn ty_python_semantic::Db,
    model: &'a SemanticModel<'a>,
    native_callees: &'a HashSet<String>,
    /// the module-level functions whose name a decorator rebinds, so a call through
    /// that name has to resolve it rather than reach the native entry
    decorated: &'a HashSet<String>,
    /// every name the module reads anywhere — see [`names_read`]
    read: &'a BTreeSet<&'a str>,
    /// every attribute a `del` in the module names — see [`deleted_attributes`]
    deleted: &'a HashSet<String>,
    /// the module body, so a class can be asked about a base's base — which is what
    /// says whether its own fields sit inside a base's instance or past one
    suite: &'a [Stmt],
    layouts: &'a Layouts,
    methods: &'a Methods,
    /// the signature of each module-level function, so a call can coerce its
    /// arguments to what the callee actually takes
    signatures: &'a HashMap<String, Signature>,
    /// which module-level functions have an unboxed edition, and where
    arrays: &'a ArrayEditions,
    /// the module-level coroutines a *direct* edition was emitted for, so an `await`
    /// of one calls the body instead of building a coroutine — see
    /// [`lower_direct_edition`]
    directs: &'a HashSet<String>,
    /// each emitted class's base, so an upcast can be recognised as free
    bases: &'a HashMap<String, String>,
    /// the classes emitted as *mutable* heap types. python can rebind a method on one
    /// or subclass it, so a call on one goes through a test rather than straight to a
    /// body — see [`Lowering::dispatch_candidates`]
    mutable: &'a HashSet<String>,
    /// the classes whose body declares `__slots__`, and whose instances therefore have
    /// no dict to hold a value shadowing a method — see
    /// [`Lowering::keeps_instance_dict`]
    slotted: &'a HashSet<String>,
    /// the classes whose body writes a `__new__`, which is what a construction of one
    /// has to reach — see [`Lowering::construct`]
    constructs: &'a HashSet<String>,
    /// the attributes each emitted class publishes as a `property` — see
    /// [`published_properties`]
    properties: &'a HashMap<String, HashSet<String>>,
    /// the class whose body the frame being lowered is written in, which is what
    /// decides how python mangles a private name — see [`mangled`]. a nested frame
    /// inherits it, because the mangling follows the source and not the receiver
    owner: Option<&'a str>,
}

/// what a nested function reads through its environment
struct Captured {
    class: String,
    /// the register holding the environment
    receiver: RegisterId,
    /// captures read by copy — the value cannot change, so a register is faster
    names: HashSet<String>,
    /// captures that are shared cells: both frames read and write one field
    cells: HashSet<String>,
    /// whether this frame reads these through a *capture* rather than owning them
    free: bool,
}

/// where a name's value lives
///
/// a register is the fast case and the common one. a *field* is what makes a value
/// outlive the frame that wrote it, which is what a closure over a mutable name and
/// a generator's state across a suspension both need — so the two are the same
/// mechanism, reached through the same two methods
#[derive(Clone)]
enum Place {
    Register(RegisterId),
    /// a name this frame declares `global`: it lives in the module namespace, and
    /// neither half of it is a register
    ///
    /// the declaration is not a hint the write can ignore. python's binding is the
    /// module's, so a write is visible at once to every other reader — and *this*
    /// frame's own later reads have to come back out of the namespace too, or the
    /// two halves stop agreeing with each other rather than with the module
    Global {
        name: String,
    },
    /// a field of a receiver register: a capture neither frame writes, copied in
    /// where the `def` runs
    Field {
        receiver: RegisterId,
        class: String,
        name: String,
        ty: RType,
    },
    /// a *shared* cell — a field both frames write, so both see one value.
    ///
    /// it starts unset, and reading it before a write is `UnboundLocalError`. that is
    /// why a cell is always `object`: NULL has to be distinguishable from every value
    /// it could hold, and an unboxed zero is not
    Cell {
        receiver: RegisterId,
        class: String,
        name: String,
        /// whether this frame *closes over* the name rather than owning it, which
        /// decides whether an unset read is `NameError` or `UnboundLocalError`
        free: bool,
    },
    /// a field of an environment further up the chain
    ///
    /// reading it walks `$outer` from this frame's receiver, which is why a place
    /// cannot just be a receiver register: the receiver has to be *derived*
    Chained {
        /// the classes to walk, this frame's receiver first and the one holding the
        /// field last. always at least two long
        path: Vec<String>,
        name: String,
        ty: RType,
    },
}

/// how a function's receiver relates to its written parameters
///
/// a method's `self` is the first parameter in the source; a nested function has no
/// such parameter at all, so its environment has to be *prepended*. getting this
/// wrong silently binds the receiver to the first real argument
#[derive(Clone, Copy)]
enum Receiver<'a> {
    /// the ast's first parameter *is* the receiver
    Explicit(&'a RType),
    /// the receiver is synthetic and comes before every written parameter
    Implicit(&'a RType),
}

impl<'a> Receiver<'a> {
    fn rtype(self) -> &'a RType {
        match self {
            Self::Explicit(rtype) | Self::Implicit(rtype) => rtype,
        }
    }

    /// the class a method's receiver is an instance of, when it is one
    fn owner(self) -> Option<String> {
        match self.rtype() {
            RType::Instance { class, .. } => Some(class.clone()),
            _ => None,
        }
    }
}

/// which of the two things a suspension was
///
/// only an *async generator* distinguishes them, but every generator records it: the
/// field costs one `Py_ssize_t` and having it always written is what lets one resume
/// method serve all three surfaces
#[derive(Clone, Copy)]
enum Suspension {
    /// the frame is waiting on something else, and the wait has to reach whatever is
    /// driving it
    Awaited = 0,
    /// the frame produced an item
    Yielded = 1,
}

/// something an early exit has to run before it transfers control
#[derive(Clone)]
enum Cleanup {
    /// a `finally` body, inlined at each exit
    Finally(Vec<Stmt>),
    /// a context manager's `__exit__`, on the normal path.
    ///
    /// a *place* rather than a register, because inside a generator the manager
    /// has to survive every suspension the body makes — and a register does not
    /// come back from one. the flag says which protocol: leaving an `async with`
    /// by `return` still has to *await* the exit
    Context(Place, bool),
    /// an `except` block's handled exception, put back on the way out
    Handled(RegisterId),
}

/// an assignment target with its location parts already evaluated
///
/// a plain assignment could re-derive them, but an augmented one must not: it reads
/// and writes *one* location, so `xs[f()] += 1` has to call `f` exactly once
#[derive(Clone)]
enum Location {
    Place(Place),
    Attribute {
        receiver: Value,
        receiver_ty: RType,
        name: String,
    },
    Item {
        container: Value,
        index: Value,
    },
    /// one slot of an unboxed array, at its own offset
    Element {
        array: Value,
        index: Value,
        element: RType,
    },
}

/// which display is being built, and how a `*` in it is merged
#[derive(Clone, Copy, PartialEq, Eq)]
enum Display {
    List,
    Set,
    Tuple,
    Dict,
}

impl Display {
    fn rtype(self) -> RType {
        match self {
            Self::List => RType::LIST,
            Self::Set | Self::Tuple | Self::Dict => RType::OBJECT,
        }
    }

    fn build(self, dest: RegisterId, items: Vec<Value>) -> Op {
        match self {
            Self::List => Op::BuildList { dest, items },
            Self::Set => Op::BuildSet { dest, items },
            Self::Tuple => Op::BuildTuple { dest, items },
            Self::Dict => Op::BuildDict { dest, pairs: items },
        }
    }
}

/// the state machine a generator's `$resume` method is
struct Generator {
    class: String,
    /// where each `yield` lowered so far leaves the frame and comes back
    resumptions: Vec<generators::Resumption>,
    /// how many `for` loops have taken an iterator field
    iterators: usize,
}

/// the closure environment a frame allocates
struct Closures {
    class: String,
    /// the register holding the instance
    register: RegisterId,
    /// `lambda range -> generated method name`, so the *expression* can find the
    /// method the closure analysis made for it
    lambdas: HashMap<(u32, u32), String>,
    /// the nested functions whose `def` has been lowered, so the environment is
    /// live and a call to one can go straight to its native entry point
    ready: HashSet<String>,
    /// the register holding the environment this one's `$outer` points at, when
    /// that is a *sibling* of this frame rather than the frame's own receiver
    outer: Option<RegisterId>,
    /// the field names to re-seed at each closure, when the environment is
    /// allocated *there* rather than once per frame.
    ///
    /// that is what makes a loop's binding per-iteration: the closure holds the
    /// values as they were where it was written. only an environment with no cells
    /// can work this way — a cell exists to be shared, and a fresh one is not
    per_closure: Option<Vec<String>>,
}

/// a function's representations, derived without lowering its body
///
/// the direct-call fast path needs a callee's signature *before* the callee is
/// lowered, because a method may call a sibling — or itself. deriving it here
/// keeps one definition of what a signature is
#[derive(Clone)]
struct Signature {
    params: Vec<(String, RType)>,
    /// the default for each parameter, `None` where it has none
    defaults: Vec<Option<Value>>,
    /// whether the trailing parameters are `*args` and `**kwargs`. both hold a real
    /// python object the wrapper builds, so the body sees a `tuple` and a `dict`
    vararg: bool,
    kwarg: bool,
    /// how many of `params` are positional-only, and how many keyword-only. the two
    /// bound the run a caller may fill positionally
    posonly: usize,
    kwonly: usize,
    ret: RType,
    /// the indices of parameters the boundary can only *sometimes* establish
    ///
    /// see [`by_ir::function::Function::deferring`]
    deferring: Vec<usize>,
    /// the indices of parameters whose default is not an immediate
    ///
    /// see [`by_ir::function::Function::computed_defaults`]
    computed_defaults: Vec<usize>,
}

/// the chain a call site writes out in place of asking the object protocol
struct Dispatch {
    /// the emitted classes tested, in the order the tests are written
    candidates: Vec<String>,
    /// the representations every candidate takes its arguments in
    params: Vec<RType>,
    /// the representation every candidate's body answers with
    produced: RType,
    /// what the *site* hands on, which the arm the tests fall through to has to meet too
    site: RType,
}

/// how many emitted classes one call site tests before it settles for the protocol
///
/// each candidate costs a compare, and a whole method body the c compiler is then free
/// to inline behind it, so the chain earns its place only while it stays well under what
/// a lookup on the type costs. a hierarchy wider than this still runs — every class of it
/// through the arm the tests fall through to
const DISPATCH_CANDIDATES: usize = 4;

/// the suffix distinguishing a function's unboxed edition from its boxed one
const ARRAY_EDITION: &str = "$arr";

/// which parameters of each module-level function its unboxed edition takes as
/// buffers, keyed by the function's name
/// which parameters one edition takes as buffers, and of what, in index order
type ArraySignature = Vec<(usize, RType)>;

/// the editions each module-level function has, keyed by its name
type ArrayEditions = BTreeMap<String, Vec<ArraySignature>>;

/// the buffer signatures each function's parameters are handed by calls in this
/// unit — see [`supplied_arrays`]
type SuppliedArrays = HashMap<String, BTreeSet<ArraySignature>>;

/// the symbol one edition is emitted under
///
/// the element types are in the name because a callee handed a `list[float]` by one
/// caller and a `list[int]` by another needs one edition each — one body cannot have
/// two representations, and neither can one symbol
fn edition_name(function: &str, signature: &[(usize, RType)]) -> String {
    let mut out = format!("{function}{ARRAY_EDITION}");
    for (index, rtype) in signature {
        let _ = write!(
            out,
            "{index}{}",
            by_ir::rtype::tuple_mangle(std::slice::from_ref(rtype))
        );
    }
    out
}

/// which parameters a second, unboxed edition of this function would take as buffers
///
/// a `list[T]` of unboxed elements *can* be a buffer; whether it may be is the same
/// question [`buffer_safe`] asks of a list built here, asked of one handed in. the
/// answer depends only on this function, so it is settled before any body is lowered
fn array_editions(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
    layouts: &Layouts,
    arrays: &ArrayEditions,
    supplied: &SuppliedArrays,
) -> Vec<ArraySignature> {
    // a variadic parameter holds a tuple or a dict the boundary built, never a buffer
    let named = || {
        function
            .parameters
            .posonlyargs
            .iter()
            .chain(function.parameters.args.iter())
            .chain(function.parameters.kwonlyargs.iter())
    };
    // a parameter whose *annotation* pins an element representation gives one edition
    // on its own; a generic `list[T]` pins none, so the unit's own call sites are what
    // say which to build
    let declared: ArraySignature = named()
        .enumerate()
        .filter_map(|(index, parameter)| {
            let rtype = parameter
                .parameter
                .inferred_type(model)
                .and_then(|ty| mapper::map_local_type(db, env, ty, layouts).ok())
                .filter(|rtype| matches!(rtype, RType::Array(_)))?;
            Some((index, rtype))
        })
        .collect();

    let mut found: BTreeSet<ArraySignature> = BTreeSet::new();
    if !declared.is_empty() {
        found.insert(declared);
    }
    if let Some(signatures) = supplied.get(function.name.as_str()) {
        found.extend(signatures.iter().cloned());
    }
    let names: Vec<&str> = named()
        .map(|parameter| parameter.parameter.name.as_str())
        .collect();
    found
        .into_iter()
        .filter(|signature| {
            signature.iter().all(|(index, _)| {
                names
                    .get(*index)
                    .is_some_and(|name| buffer_safe(&function.body, name, arrays))
            })
        })
        .collect()
}

/// the buffer representation each module-level function's parameters are *handed*
/// by calls within this unit
///
/// only where every call agrees: two call sites supplying different element types
/// would need an edition each, and picking between them at a call would need the
/// argument's representation, which is what this is computing
fn supplied_arrays(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    suite: &[Stmt],
    layouts: &Layouts,
) -> SuppliedArrays {
    let mut seen: SuppliedArrays = HashMap::new();
    for stmt in suite {
        let Stmt::FunctionDef(function) = stmt else {
            continue;
        };
        for statement in walk(&function.body) {
            for expr in crate::closures::statement_expressions(statement) {
                crate::closures::visit_expressions(expr, &mut |child| {
                    let Expr::Call(call) = child else { return };
                    let Expr::Name(callee) = call.func.as_ref() else {
                        return;
                    };
                    let signature: ArraySignature = call
                        .arguments
                        .args
                        .iter()
                        .enumerate()
                        .filter_map(|(index, argument)| {
                            let rtype = argument
                                .inferred_type(model)
                                .and_then(|ty| mapper::map_local_type(db, env, ty, layouts).ok())
                                .filter(|rtype| matches!(rtype, RType::Array(_)))?;
                            Some((index, rtype))
                        })
                        .collect();
                    if !signature.is_empty() {
                        seen.entry(callee.id.to_string())
                            .or_default()
                            .insert(signature);
                    }
                });
            }
        }
    }
    seen
}

fn signature(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
    layouts: &Layouts,
    receiver: Option<Receiver<'_>>,
    arrays: &[(usize, RType)],
) -> Lowered<Signature> {
    let parameters = &function.parameters;

    let mut params = Vec::with_capacity(parameters.args.len() + 1);
    let mut defaults: Vec<Option<Value>> = Vec::with_capacity(parameters.args.len() + 1);
    let mut deferring = Vec::new();
    let mut computed_defaults = Vec::new();
    // a synthetic receiver comes before every written parameter, and its name
    // cannot collide with one because `$` is not an identifier character
    if let Some(Receiver::Implicit(rtype)) = receiver {
        params.push(("$env".to_string(), rtype.clone()));
        defaults.push(None);
    }
    // the binding order is the source order: positional-only, then ordinary, then
    // keyword-only. `*args` and `**kwargs` come last because nothing binds them by
    // name — the wrapper builds them out of what is left
    let named = parameters
        .posonlyargs
        .iter()
        .chain(parameters.args.iter())
        .chain(parameters.kwonlyargs.iter())
        .collect::<Vec<_>>();

    // a parameter's register has to cover every value written into it, and its own
    // body is one of the writers. an *unannotated* parameter is declared by its
    // default — ty reads `def quote_from_bytes(bs, safe='/')` as taking a `str` — and
    // python is perfectly happy for the body to rebind the name, which
    // `safe = safe.encode('ascii', 'ignore')` does. a register shaped for the default
    // then either refuses that store or narrows it with a check, and the check raises
    // on a call the interpreter answers without complaint
    //
    // this lives here rather than where the body is lowered because a parameter's
    // representation is part of the calling convention: a caller coerces its arguments
    // to it and the boundary unboxes to it, so every reader of the signature has to
    // see the same widening the body will
    //
    // the editions are deliberately empty rather than the real ones. which lists live
    // in an unboxed buffer is still being settled when the signature tables are built,
    // so consulting them here would make a signature depend on *when* it was computed.
    // a parameter that already is a buffer is left alone below for the same reason
    let rebound: HashMap<String, RType> = local_representations(
        db,
        env,
        model,
        &function.body,
        layouts,
        &ArrayEditions::new(),
    )
    .into_iter()
    .collect();

    for (index, parameter) in named.iter().enumerate() {
        // a *literal* default is evaluated once in python and cannot change, so
        // inlining it is the same thing.
        //
        // anything computed is evaluated once too — at definition time, which is
        // before this module's init has finished and is where the mutable-default
        // behaviour comes from. the interpreted definition already did that, and
        // holds the one object every call has to share, so a call that omits such
        // a parameter is handed to it rather than given a second one
        let default = match &parameter.default {
            None => None,
            Some(expr) => match literal_value(expr) {
                Some(value) => Some(value),
                // a module-level function's twin is in the module dict and a method's is
                // an attribute of the interpreted class the fallback left there. a
                // *nested* function was never defined under a name of its own, so there
                // is nothing to hand the call to
                None if !matches!(receiver, Some(Receiver::Implicit(_))) => {
                    computed_defaults.push(params.len());
                    None
                }
                None => {
                    return Err(Decline::new(
                        "only a literal parameter default is lowered yet",
                    ));
                }
            },
        };
        defaults.push(default);
        let rtype = match (index, receiver) {
            (0, Some(Receiver::Explicit(receiver))) => receiver.clone(),
            // the unboxed edition takes the buffer itself. it is never reached from
            // the boundary, so nothing here has to be established at a call
            _ if arrays.iter().any(|(at, _)| *at == index) => arrays
                .iter()
                .find(|(at, _)| *at == index)
                .map(|(_, rtype)| rtype.clone())
                .unwrap_or(RType::OBJECT),
            _ => {
                let ty = parameter
                    .parameter
                    .inferred_type(model)
                    .ok_or_else(|| Decline::new("a parameter has no inferred type"))?;
                // python's `float` annotation admits an `int`, so the body compiles
                // against a `double` and the boundary tests each call rather than
                // assuming one — see [`Function::deferring`].
                //
                // only a module-level function: deferring means calling the
                // interpreted twin, and a method's twin belongs to a *different*
                // class than the compiled receiver, while a nested function has no
                // python-visible name to have been defined under
                if receiver.is_none() && mapper::is_promoted_float(db, env, ty) {
                    deferring.push(params.len());
                    RType::FLOAT
                } else {
                    map_type_with(db, env, ty, layouts)?
                }
            }
        };
        let name = parameter.parameter.name.as_str();
        let rtype = match rebound.get(name) {
            // an unboxed edition's parameter *is* the caller's buffer, and a buffer is
            // not a value that widens: handing one out means copying it, and a copy is
            // a different list. the store in the body declines instead, as it already
            // did before a parameter's writes were counted at all
            Some(_) if matches!(rtype, RType::Array(_)) => rtype,
            Some(written) => {
                let covered = covering(&rtype, written);
                // slot zero is the receiver, which every field read in the body is
                // written against — a frame whose `self` has become an object no
                // longer has one to read a field off
                if covered != rtype && index == 0 && matches!(receiver, Some(Receiver::Explicit(_)))
                {
                    return Err(Decline::new(
                        "a method whose body rebinds its receiver is not lowered yet",
                    ));
                }
                covered
            }
            None => rtype,
        };
        params.push((name.to_string(), rtype));
    }

    // the return type comes from the returns themselves rather than from the
    // annotation: ty has already checked the two agree, and the inferred type is
    // the one that says what representation the value actually has. with no
    // returns at all there is nothing to derive it from, and the annotation is
    // what a caller will read back — see [`return_type`]
    // `*args` and `**kwargs` are ordinary parameters holding an ordinary tuple and
    // dict — packing them is the boundary's job, not the body's
    if let Some(vararg) = &parameters.vararg {
        params.push((vararg.name.to_string(), RType::OBJECT));
        defaults.push(None);
    }
    if let Some(kwarg) = &parameters.kwarg {
        params.push((kwarg.name.to_string(), RType::OBJECT));
        defaults.push(None);
    }

    Ok(Signature {
        params,
        defaults,
        vararg: parameters.vararg.is_some(),
        kwarg: parameters.kwarg.is_some(),
        // counted from `params[0]`, which for a synthetic receiver is a slot no
        // keyword can reach either — a boundary reads this against the parameters it
        // has, not against the ones that were written
        posonly: parameters.posonlyargs.len()
            + usize::from(matches!(receiver, Some(Receiver::Implicit(_)))),
        kwonly: parameters.kwonlyargs.len(),
        ret: return_type(db, env, model, function, layouts)?,
        deferring,
        computed_defaults,
    })
}

/// the representation every `return` in a body agrees on
/// whether control can fall off the end of this body
///
/// deliberately shallow: a body ending in `return` or `raise` cannot, an `if` covering
/// every case whose branches all end that way cannot, and anything else is assumed to.
/// assuming wrongly costs a wider representation for the return, never a wrong one
fn reaches_the_end(body: &[Stmt]) -> bool {
    match body.last() {
        None => true,
        Some(Stmt::Return(_) | Stmt::Raise(_)) => false,
        Some(Stmt::If(node)) => {
            let covered = node
                .elif_else_clauses
                .iter()
                .any(|clause| clause.test.is_none());
            !covered
                || reaches_the_end(&node.body)
                || node
                    .elif_else_clauses
                    .iter()
                    .any(|clause| reaches_the_end(&clause.body))
        }
        Some(_) => true,
    }
}

/// whether the emitter has a slot adapter for this dunder
///
/// [`fills_a_type_slot`] is the whole of CPython's `slotdefs`; this is the part of
/// it the emitter can write a slot for. a dunder in neither is ordinary and the
/// method table answers it, and one in the first but not this is a decline —
/// whether the class body wrote it as a `def` or as an assignment
fn has_a_slot_adapter(name: &str) -> bool {
    matches!(
        name,
        "__new__"
            | "__repr__"
            | "__str__"
            | "__len__"
            | "__bool__"
            | "__hash__"
            | "__eq__"
            | "__ne__"
            | "__lt__"
            | "__le__"
            | "__gt__"
            | "__ge__"
            | "__add__"
            | "__radd__"
            | "__sub__"
            | "__rsub__"
            | "__mul__"
            | "__rmul__"
            | "__truediv__"
            | "__rtruediv__"
            | "__aiter__"
            | "__anext__"
            | "__await__"
            | "__iter__"
            | "__next__"
            | "__getitem__"
            | "__setitem__"
            | "__delitem__"
            | "__int__"
            | "__float__"
            | "__index__"
            | "__contains__"
            | "__neg__"
            | "__pos__"
            | "__abs__"
            | "__invert__"
            | "__call__"
            | "__get__"
            | "__iadd__"
            | "__isub__"
            | "__imul__"
            | "__itruediv__"
            | "__floordiv__"
            | "__rfloordiv__"
            | "__mod__"
            | "__rmod__"
            | "__divmod__"
            | "__rdivmod__"
            | "__lshift__"
            | "__rlshift__"
            | "__rshift__"
            | "__rrshift__"
            | "__and__"
            | "__rand__"
            | "__xor__"
            | "__rxor__"
            | "__or__"
            | "__ror__"
            | "__matmul__"
            | "__rmatmul__"
            | "__pow__"
            | "__rpow__"
            | "__ifloordiv__"
            | "__imod__"
            | "__ilshift__"
            | "__irshift__"
            | "__iand__"
            | "__ixor__"
            | "__ior__"
            | "__imatmul__"
            | "__del__"
            | "__getattr__"
    )
}

/// whether python reads this name out of a *type slot* rather than looking it up
///
/// that is the whole of what makes a dunder special to the emitter. `repr(x)` reads
/// `tp_repr` and never consults the name, so a method table entry would never be found
/// — such a method needs a slot adapter, and one we cannot write is a decline.
///
/// every *other* dunder is ordinary: `__reduce__`, `__enter__`, `__set_name__`,
/// `__format__` and the rest are found by name, which is exactly what the method table
/// provides. the list is CPython's `slotdefs`, which is finite and does not move
fn fills_a_type_slot(name: &str) -> bool {
    matches!(
        name,
        "__new__"
            | "__init__"
            | "__del__"
            | "__repr__"
            | "__str__"
            | "__hash__"
            | "__call__"
            | "__getattr__"
            | "__getattribute__"
            | "__setattr__"
            | "__delattr__"
            | "__lt__"
            | "__le__"
            | "__eq__"
            | "__ne__"
            | "__gt__"
            | "__ge__"
            | "__iter__"
            | "__next__"
            | "__get__"
            | "__set__"
            | "__delete__"
            | "__len__"
            | "__getitem__"
            | "__setitem__"
            | "__delitem__"
            | "__contains__"
            | "__await__"
            | "__aiter__"
            | "__anext__"
            | "__bool__"
            | "__index__"
            | "__int__"
            | "__float__"
            // `__complex__` is deliberately absent: `PyNumberMethods` has `nb_int`,
            // `nb_float` and `nb_index` but no complex field at all, so `complex(x)`
            // looks the name up on the type — which the method table answers
            | "__neg__"
            | "__pos__"
            | "__abs__"
            | "__invert__"
            | "__add__"
            | "__sub__"
            | "__mul__"
            | "__matmul__"
            | "__truediv__"
            | "__floordiv__"
            | "__mod__"
            | "__divmod__"
            | "__pow__"
            | "__lshift__"
            | "__rshift__"
            | "__and__"
            | "__xor__"
            | "__or__"
    ) || is_reflected_or_augmented_operator(name)
}

/// the `__r*__` and `__i*__` forms, which fill the same numeric slots
fn is_reflected_or_augmented_operator(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("__r")
        .or_else(|| name.strip_prefix("__i"))
    else {
        return false;
    };
    let Some(stem) = stem.strip_suffix("__") else {
        return false;
    };
    matches!(
        stem,
        "add"
            | "sub"
            | "mul"
            | "matmul"
            | "truediv"
            | "floordiv"
            | "mod"
            | "divmod"
            | "pow"
            | "lshift"
            | "shift"
            | "and"
            | "xor"
            | "or"
    )
}

/// a register's value in the object representation, boxing only what is not one
///
/// a cell holds objects, and a parameter that is already one is already what the cell
/// wants — boxing it again is ill-formed, and the verifier says so
fn boxed_object(builder: &mut by_ir::builder::FunctionBuilder, id: RegisterId) -> Value {
    if builder.register_type(id) == Some(&RType::OBJECT) {
        return Value::Register(id);
    }
    let boxed = builder.temp(RType::OBJECT);
    builder.push(Op::Box {
        dest: boxed,
        src: Value::Register(id),
    });
    Value::Register(boxed)
}

/// the representation of a body that hands its pair back in registers, where it does
///
/// python builds a *fresh* tuple at every tuple display, so `f() is f()` is already
/// false for a body whose every `return` is one — which is what licenses handing the
/// elements back and building the object at the boundary instead. a body that passes
/// a tuple *through*,
///
/// ```python
/// def first(pair: tuple[int, int]) -> tuple[int, int]:
///     return pair
/// ```
///
/// is not one of those: `first(p) is p` is true, and a rebuilt tuple would answer
/// false. so the display is the whole gate, and it is syntactic on purpose — the
/// question is where the object came from, not what its type is
fn tuple_return_type(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
    layouts: &Layouts,
) -> Option<RType> {
    let body = &function.body;
    // falling off the end hands back `None`, which no tuple representation holds
    if reaches_the_end(body) {
        return None;
    }
    let mut found: Option<RType> = None;
    // every `return` has to be seen, a `case` body's included — one this missed would
    // be lowered against a representation nothing proved it had. reaching further than
    // [`return_type`] does can only make this refuse more often, never accept more
    for stmt in walk_with_cases(body) {
        let Stmt::Return(node) = stmt else { continue };
        // a bare `return` is `None`, and so is a body with no `return` at all
        let value = node.value.as_deref()?;
        let Expr::Tuple(display) = value else {
            return None;
        };
        // `return *rest, x` has no length until it runs
        if display
            .elts
            .iter()
            .any(|elt| matches!(elt, Expr::Starred(_)))
        {
            return None;
        }
        let rtype = map_fixed_tuple(db, env, value.inferred_type(model)?, layouts)?;
        // the checker's length is what the struct is laid out from and the display's
        // is what fills it, so a body only compiles this way where they agree
        let RType::Tuple(slots) = &rtype else {
            return None;
        };
        if slots.len() != display.elts.len() {
            return None;
        }
        match &found {
            Some(existing) if *existing != rtype => return None,
            _ => found = Some(rtype),
        }
    }
    found
}

fn return_type(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    function: &ast::StmtFunctionDef,
    layouts: &Layouts,
) -> Lowered<RType> {
    if let Some(tuple) = tuple_return_type(db, env, model, function, layouts) {
        return Ok(tuple);
    }
    let body = &function.body;
    let mut found: Option<RType> = None;
    for stmt in walk(body) {
        let Stmt::Return(ret) = stmt else { continue };
        let rtype = match &ret.value {
            None => RType::NONE,
            Some(value) => {
                let ty = value
                    .inferred_type(model)
                    .ok_or_else(|| Decline::new("a returned expression has no inferred type"))?;
                map_type_with(db, env, ty, layouts)?
            }
        };
        found = Some(match found {
            Some(existing) if existing == rtype => rtype,
            // a `bit` and a `bool` are the same byte
            Some(RType::Primitive(Primitive::Bit | Primitive::Bool))
                if matches!(rtype, RType::Primitive(Primitive::Bit | Primitive::Bool)) =>
            {
                RType::BOOL
            }
            // anything else unifies at the widest representation rather than
            // declining: `object` holds either of them
            Some(_) => RType::OBJECT,
            None => rtype,
        });
    }
    // falling off the end returns `None` too, and it is as much a return as a written
    // one — a function whose only `return` is inside an `if` hands a caller back `None`
    // on the other path
    if let Some(existing) = found.clone()
        && existing != RType::NONE
        && reaches_the_end(body)
    {
        found = Some(RType::OBJECT);
    }
    if let Some(found) = found {
        return Ok(found);
    }
    // no `return` at all: either the body falls off the end, which is `None`, or it
    // always raises — and then the annotation is the only thing that says what a
    // caller reads back. getting this wrong is not a decline: the caller would take
    // the C function's error sentinel for a value
    match &function.returns {
        Some(annotation) => {
            let ty = annotation
                .inferred_type(model)
                .ok_or_else(|| Decline::new("a return annotation has no inferred type"))?;
            map_type_with(db, env, ty, layouts)
        }
        None => Ok(RType::NONE),
    }
}

/// the one representation that covers two writes to the same place
///
/// there is no union representation, so two that do not already agree meet at the
/// object at the top of the lattice. a `bit` and a `bool` are the same byte, and so
/// meet at `bool` rather than falling all the way up
fn covering(left: &RType, right: &RType) -> RType {
    if left == right {
        return left.clone();
    }
    if matches!(left, RType::Primitive(Primitive::Bit | Primitive::Bool))
        && matches!(right, RType::Primitive(Primitive::Bit | Primitive::Bool))
    {
        return RType::BOOL;
    }
    RType::OBJECT
}

/// the representation each local needs, covering every value assigned to it
///
/// computed before any lowering, because a register is declared once and every
/// write to it has to fit
fn local_representations(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    model: &SemanticModel<'_>,
    body: &[Stmt],
    layouts: &Layouts,
    arrays: &ArrayEditions,
) -> Vec<(String, RType)> {
    let mut order: Vec<String> = Vec::new();
    let mut found: HashMap<String, RType> = HashMap::new();

    let mut record =
        |name: &str, rtype: RType, found: &mut HashMap<String, RType>| match found.get(name) {
            None => {
                order.push(name.to_string());
                found.insert(name.to_string(), rtype);
            }
            Some(existing) => {
                let covered = covering(existing, &rtype);
                if covered != *existing {
                    found.insert(name.to_string(), covered);
                }
            }
        };

    // deliberately *not* `map_local_type`: that one may answer with an unboxed
    // array, and the array decision belongs to the two sites below that ask
    // `buffer_safe` first. a buffer handed out ungated is a list that escapes
    let peek = |expr: &Expr| -> RType {
        expr.inferred_type(model)
            .and_then(|ty| map_type_with(db, env, ty, layouts).ok())
            .unwrap_or(RType::OBJECT)
    };

    // every plain name a target *list* binds, each with its own checker type: a
    // slot holds an object, and the name narrows back to what it is declared as
    let target_names = |target: &Expr| -> Vec<(String, RType)> {
        // only the names an assignment *binds*. `xs[i] = v` and `o.a = v` mutate
        // something that already exists — walking into them records the base as though
        // it had been assigned, which merges its representation with the plain one and
        // costs a buffer its whole reason for existing
        fn bound<'a>(target: &'a Expr, out: &mut Vec<&'a Expr>) {
            match target {
                Expr::Name(_) => out.push(target),
                Expr::Tuple(tuple) => tuple.elts.iter().for_each(|item| bound(item, out)),
                Expr::List(list) => list.elts.iter().for_each(|item| bound(item, out)),
                Expr::Starred(starred) => bound(&starred.value, out),
                _ => {}
            }
        }
        let mut names = Vec::new();
        bound(target, &mut names);
        names
            .into_iter()
            .filter_map(|child| match child {
                Expr::Name(name) => Some((name.id.to_string(), peek(child))),
                _ => None,
            })
            .collect()
    };

    // a `list` display whose elements all own nothing can live in a buffer of its
    // own. the *elements* decide it rather than the list's inferred type: a display
    // is where the representation is chosen, and the elements are right there —
    // *and* the name must never escape, or the function would decline where it used
    // to compile
    let display_array = |expr: &Expr| -> Option<RType> {
        // a comprehension is the usual way one of these is built, and its element
        // expression says the representation as directly as a display's items do
        if let Expr::ListComp(comprehension) = expr {
            let ty = peek(&comprehension.elt);
            return (ty.is_unboxed() && !ty.is_refcounted()).then(|| RType::Array(Box::new(ty)));
        }
        let Expr::List(display) = expr else {
            return None;
        };
        if display.elts.iter().any(Expr::is_starred_expr) {
            return None;
        }
        // an empty display says nothing about its elements, so the *checker* does:
        // a list built by appending is the usual way one of these is made
        if display.elts.is_empty() {
            return expr
                .inferred_type(model)
                .and_then(|ty| mapper::map_local_type(db, env, ty, layouts).ok())
                .filter(|rtype| matches!(rtype, RType::Array(_)));
        }
        let mut element: Option<RType> = None;
        for item in &display.elts {
            let ty = peek(item);
            if !ty.is_unboxed() || ty.is_refcounted() {
                return None;
            }
            match &element {
                Some(seen) if *seen != ty => return None,
                _ => element = Some(ty),
            }
        }
        element.map(|element| RType::Array(Box::new(element)))
    };

    for stmt in walk(body) {
        // a comprehension's target is a local of this frame — the comprehension is
        // desugared into it — so a closure inside one captures it like any other
        let mut targets: Vec<(String, RType)> = Vec::new();
        for expr in crate::closures::statement_expressions(stmt) {
            crate::closures::visit_expressions(expr, &mut |child| {
                // `x := v` binds wherever it stands, so it is a write like any other —
                // and one that hides inside an expression rather than standing as a
                // statement, which is why it is looked for here
                if let Expr::Named(node) = child
                    && let Expr::Name(name) = node.target.as_ref()
                {
                    targets.push((name.id.to_string(), peek(&node.value)));
                    return;
                }
                let generators = match child {
                    Expr::ListComp(node) => &node.generators,
                    Expr::SetComp(node) => &node.generators,
                    Expr::DictComp(node) => &node.generators,
                    Expr::Generator(node) => &node.generators,
                    _ => return,
                };
                for generator in generators {
                    targets.extend(target_names(&generator.target));
                }
            });
        }
        for (name, rtype) in targets {
            record(&name, rtype, &mut found);
        }
        match stmt {
            Stmt::Assign(node) => match node.targets.as_slice() {
                // the *name's* type where that earns an unboxed array: the display's
                // own type may be narrower than what the binding is declared as
                [Expr::Name(name)] => {
                    let rtype = display_array(&node.value)
                        .filter(|_| buffer_safe(body, name.id.as_str(), arrays))
                        .unwrap_or_else(|| peek(&node.value));
                    record(name.id.as_str(), rtype, &mut found);
                }
                targets => {
                    for (name, rtype) in targets.iter().flat_map(&target_names) {
                        record(&name, rtype, &mut found);
                    }
                }
            },
            Stmt::AnnAssign(node) => {
                if let (Expr::Name(name), Some(value)) = (node.target.as_ref(), &node.value) {
                    let rtype = display_array(value)
                        .filter(|_| buffer_safe(body, name.id.as_str(), arrays))
                        .unwrap_or_else(|| peek(value));
                    record(name.id.as_str(), rtype, &mut found);
                }
            }
            // `with a() as x:` binds `x` — and inside a generator that has to be a
            // field, because the body suspends while the block is still open
            Stmt::With(node) => {
                for item in &node.items {
                    let Some(target) = &item.optional_vars else {
                        continue;
                    };
                    for (name, rtype) in target_names(target) {
                        record(&name, rtype, &mut found);
                    }
                }
            }
            Stmt::AugAssign(node) => {
                if let Expr::Name(name) = node.target.as_ref() {
                    record(name.id.as_str(), peek(&node.value), &mut found);
                }
            }
            Stmt::For(node) => {
                // iterating a buffer binds the *element*, not a boxed copy of it —
                // and the binding has to say so, or the body does object arithmetic
                // on values that were already unboxed
                if let (Expr::Name(target), Expr::Name(source)) =
                    (node.target.as_ref(), node.iter.as_ref())
                    && let Some(RType::Array(element)) = found.get(source.id.as_str())
                {
                    let element = (**element).clone();
                    record(target.id.as_str(), element, &mut found);
                    continue;
                }
                // the checker's type for each name the target binds, which the
                // lowering narrows to with a checked unbox
                for (name, rtype) in target_names(&node.target) {
                    record(&name, rtype, &mut found);
                }
            }
            // `except E as e` binds the caught exception, which the lowering fetches
            // as a plain object — so this is the one write whose representation is
            // known without asking the checker anything
            Stmt::Try(node) => {
                for handler in &node.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    if let Some(bound) = &handler.name {
                        record(bound.as_str(), RType::OBJECT, &mut found);
                    }
                }
            }
            // a nested `def` binds its name to a callable
            Stmt::FunctionDef(node) => {
                record(node.name.as_str(), RType::OBJECT, &mut found);
            }
            _ => {}
        }
    }

    order
        .into_iter()
        .filter_map(|name| found.get(&name).map(|rtype| (name.clone(), rtype.clone())))
        .collect()
}

/// whether `name` is only ever used in ways an unboxed buffer can serve
///
/// the safe uses are the ones with a native form: `name[i]` either way, `len(name)`,
/// and `for x in name`. anything else — an argument, a return, a container, a
/// capture — needs a real `list`, so the name keeps one.
///
/// deciding it *here* rather than declining later is the difference between a buffer
/// being an optimization and being a coverage regression
fn buffer_safe(body: &[Stmt], name: &str, arrays: &ArrayEditions) -> bool {
    let mut safe: HashSet<(u32, u32)> = HashSet::new();
    let mut mentions: Vec<(u32, u32)> = Vec::new();

    let note = |expr: &Expr, safe: &mut HashSet<(u32, u32)>| {
        if let Expr::Name(candidate) = expr
            && candidate.id.as_str() == name
        {
            safe.insert(span(candidate.range));
        }
    };

    for stmt in walk(body) {
        match stmt {
            // the binding itself, and a write through a subscript
            Stmt::Assign(node) => {
                for target in &node.targets {
                    note(target, &mut safe);
                    // `xs[i] = v` writes *into* the buffer, which is as native a form as
                    // reading one. the name here is the thing indexed, not the thing
                    // assigned, and only the target side of an assignment is walked for
                    // names — so without this a single indexed write costs the buffer
                    if let Expr::Subscript(subscript) = target {
                        note(&subscript.value, &mut safe);
                    }
                }
            }
            Stmt::AnnAssign(node) => note(&node.target, &mut safe),
            Stmt::For(node) => note(&node.iter, &mut safe),
            _ => {}
        }
        for expr in crate::closures::statement_expressions(stmt) {
            crate::closures::visit_expressions(expr, &mut |child| match child {
                Expr::Subscript(subscript) => note(&subscript.value, &mut safe),
                // `xs.append(v)` pushes onto the buffer rather than escaping it
                Expr::Call(call)
                    if matches!(call.func.as_ref(), Expr::Attribute(attribute)
                        if attribute.attr.as_str() == "append")
                        && call.arguments.args.len() == 1
                        && call.arguments.keywords.is_empty() =>
                {
                    if let Expr::Attribute(attribute) = call.func.as_ref() {
                        note(&attribute.value, &mut safe);
                    }
                }
                Expr::Call(call)
                    if matches!(call.func.as_ref(), Expr::Name(callee)
                        if callee.id.as_str() == "len")
                        && call.arguments.args.len() == 1 =>
                {
                    note(&call.arguments.args[0], &mut safe);
                }
                // a name handed to a callee's unboxed edition has not escaped: that
                // edition exists only because the callee's own body keeps it
                Expr::Call(call) => {
                    if let Expr::Name(callee) = call.func.as_ref()
                        && let Some(editions) = arrays.get(callee.id.as_str())
                    {
                        for (index, _) in editions.iter().flatten() {
                            if let Some(argument) = call.arguments.args.get(*index) {
                                note(argument, &mut safe);
                            }
                        }
                    }
                }
                Expr::Name(candidate) if candidate.id.as_str() == name => {
                    mentions.push(span(candidate.range));
                }
                _ => {}
            });
        }
    }
    // a target is not visited as an expression on every path, so collect those too
    for stmt in walk(body) {
        if let Stmt::AugAssign(node) = stmt
            && let Expr::Subscript(subscript) = node.target.as_ref()
        {
            note(&subscript.value, &mut safe);
            if let Expr::Name(candidate) = subscript.value.as_ref()
                && candidate.id.as_str() == name
            {
                mentions.push(span(candidate.range));
            }
        }
    }
    mentions.iter().all(|mention| safe.contains(mention))
}

/// every name any frame in this module declares `global`, at any depth
///
/// a `global` declaration is written in order to rebind: the name stops holding what
/// the module bound at import, and a call or a construction through it has to find
/// what is really there. that is exactly what a decorator does to a name, so the two
/// share a set — see `decorated` in [`build_module`].
///
/// a declaration with nothing assigned under it would be harmless, and is also
/// pointless, so this does not try to tell the two apart: naming one name too many
/// costs the direct call and nothing else, where naming one too few is a call that
/// reaches a definition the namespace no longer holds
fn declared_global_anywhere(body: &[Stmt]) -> HashSet<String> {
    let mut out = declared_globals(body);
    for stmt in walk(body) {
        match stmt {
            Stmt::FunctionDef(node) => out.extend(declared_global_anywhere(&node.body)),
            Stmt::ClassDef(node) => out.extend(declared_global_anywhere(&node.body)),
            _ => {}
        }
    }
    out
}

/// the names a frame declares `global`
///
/// [`walk`] stops at a nested `def` or `class`, which is what makes this per-scope:
/// a declaration inside one is that scope's, and python does not pass it outwards
fn declared_globals(body: &[Stmt]) -> HashSet<String> {
    walk(body)
        .into_iter()
        .filter_map(|stmt| match stmt {
            Stmt::Global(node) => Some(node.names.iter().map(ast::Identifier::to_string)),
            _ => None,
        })
        .flatten()
        .collect()
}

/// every statement in `body`, including nested ones
fn walk(body: &[Stmt]) -> Vec<&Stmt> {
    let mut out = Vec::new();
    for stmt in body {
        out.push(stmt);
        match stmt {
            Stmt::If(node) => {
                out.extend(walk(&node.body));
                for clause in &node.elif_else_clauses {
                    out.extend(walk(&clause.body));
                }
            }
            Stmt::While(node) => {
                out.extend(walk(&node.body));
                out.extend(walk(&node.orelse));
            }
            Stmt::For(node) => {
                out.extend(walk(&node.body));
                out.extend(walk(&node.orelse));
            }
            Stmt::Try(node) => {
                out.extend(walk(&node.body));
                for handler in &node.handlers {
                    let ast::ExceptHandler::ExceptHandler(handler) = handler;
                    out.extend(walk(&handler.body));
                }
                out.extend(walk(&node.orelse));
                out.extend(walk(&node.finalbody));
            }
            Stmt::With(node) => out.extend(walk(&node.body)),
            _ => {}
        }
    }
    out
}

/// as [`walk`], reaching the body of every `case` as well
///
/// a `case` body is a body like any other and what it writes is a field like any other,
/// so the field passes have to see into one. this is a step of their own rather than a
/// widening of [`walk`] because that walk is shared with the passes that choose a
/// register's representation, and a `match` arm arriving there changes what they choose
fn walk_with_cases(body: &[Stmt]) -> Vec<&Stmt> {
    let mut out = Vec::new();
    for stmt in walk(body) {
        out.push(stmt);
        if let Stmt::Match(node) = stmt {
            for case in &node.cases {
                out.extend(walk_with_cases(&case.body));
            }
        }
    }
    out
}

struct Lowering<'a, 'db> {
    db: &'db dyn ty_python_semantic::Db,
    model: &'a SemanticModel<'db>,
    builder: FunctionBuilder,
    locals: HashMap<String, RegisterId>,
    /// the names this frame declares `global`, which live in the module namespace
    /// rather than in any register of this frame
    globals: HashSet<String>,
    native_callees: &'a HashSet<String>,
    /// the module-level functions whose name a decorator rebinds
    decorated: &'a HashSet<String>,
    layouts: &'a Layouts,
    methods: &'a Methods,
    /// the signature of each module-level function, so a call coerces its arguments
    signatures: &'a HashMap<String, Signature>,
    /// which module-level functions have an unboxed edition, and where
    arrays: &'a ArrayEditions,
    /// the module-level coroutines a *direct* edition was emitted for
    directs: &'a HashSet<String>,
    bases: &'a HashMap<String, String>,
    mutable: &'a HashSet<String>,
    slotted: &'a HashSet<String>,
    /// the classes whose body writes a `__new__` — see [`Lowering::construct`]
    constructs: &'a HashSet<String>,
    /// the attributes each emitted class publishes as a `property` — see
    /// [`published_properties`]
    properties: &'a HashMap<String, HashSet<String>>,
    /// the closure environment this frame allocates, when it makes closures
    environment: Option<Closures>,
    /// the `(index, array)` pairs a counting loop has proved in range, innermost last
    in_range: Vec<(String, String)>,
    /// the state machine this frame *is*, when it is a generator's `$resume`
    generator: Option<Generator>,
    /// how many delegations this frame has lowered, so each gets its own result local
    delegations: usize,
    /// how many `with` blocks, so each manager gets its own register
    contexts: usize,
    /// the cleanups an early exit has to run, outermost first.
    ///
    /// a `return` inside `try` / `finally` or inside a `with` has to run the handler
    /// before it leaves — python does, and skipping it is a silent wrong answer rather
    /// than a missing optimization
    cleanups: Vec<Cleanup>,
    /// the captures this frame reads through its receiver, when it *is* a closure
    captures: Option<Captured>,
    ret: RType,
    /// `(continue target, break target, cleanup depth)` for each enclosing loop,
    /// innermost last. the depth is what `break` unwinds to
    loops: Vec<(BlockId, BlockId, usize)>,
    /// the exception each enclosing `except` block caught, innermost last — which is
    /// what a bare `raise` re-raises
    handling: Vec<RegisterId>,
    /// what a zero-argument `super()` in this frame stands for — the class the `def`
    /// is written in and the parameter python reads out of slot zero — or why this
    /// frame stands for nothing. see [`zero_argument_super`]
    zero_super: Result<ZeroSuper, &'static str>,
    /// the class whose body this frame is written in — see [`Unit::owner`]
    owner: Option<String>,
    /// how many comprehensions enclose the expression being lowered.
    ///
    /// all four forms are lowered inline, into this frame. python gives each a frame
    /// of its own whose slot zero is the iterator — except that 3.12 folds the three
    /// container forms back into the method's. so only the depth being zero says
    /// slot zero is the receiver whatever the interpreter turns out to be
    comprehensions: usize,
}

/// the two halves of the `super(C, self)` a zero-argument `super()` is sugar for
struct ZeroSuper {
    /// the class the method is written in — python's `__class__` cell
    owner: String,
    /// the parameter python reads out of slot zero, by name because it reads the
    /// *slot*: a method that assigns to its own receiver moves what `super()` sees
    receiver: String,
}

impl Lowering<'_, '_> {
    /// the name an attribute written `receiver.<written>` in this frame is bound and
    /// read under — mangled where this frame sits in a class body, see [`mangled`].
    ///
    /// every attribute name the lowering takes off the ast goes through here, so a
    /// field's published name, a `GetAttr` and the method table all agree on one
    /// spelling
    fn attribute_name(&self, written: &str) -> String {
        mangled(self.owner.as_deref(), written)
    }

    /// whether an instance whose type is `class` — or, where the receiver is not exact,
    /// any emitted class under it — has somewhere to keep an attribute called `name`
    ///
    /// an emitted instance **is** its layout: there is no `__dict__` behind it, so a name
    /// nothing declares cannot be stored at all. two things widen what counts as declaring
    /// it, and missing either turns a working write into a decline:
    ///
    /// - a class that adds no field of its own declares an *empty* layout even though its
    ///   instances carry every one of its base's, reached through the descriptors the base
    ///   published. so the answer is the chain's, not this one class's
    /// - a receiver typed as a base may hold a subclass this module also emitted, and the
    ///   descriptor the dynamic form finds is then that subclass's
    fn holds_attribute(&self, class: &str, exact: bool, name: &str) -> bool {
        let declares = |candidate: &str| {
            self.layouts
                .get(candidate)
                .is_some_and(|fields| fields.iter().any(|field| field.name == name))
                // a `property` is a *data* descriptor, so the write reaches it rather
                // than looking for somewhere on the instance to land — and one with no
                // setter raises there, which is what python does too
                || self
                    .properties
                    .get(candidate)
                    .is_some_and(|published| published.contains(name))
        };
        let mut current = class;
        // bounded by the class count, the way every base walk here is: a chain that
        // visits a class twice is a cycle, and the layouts never settle on one
        for _ in 0..=self.bases.len() {
            if declares(current) {
                return true;
            }
            match self.bases.get(current) {
                Some(base) => current = base,
                None => break,
            }
        }
        if exact {
            return false;
        }
        self.layouts
            .keys()
            .any(|candidate| declares(candidate) && self.descends_from(candidate, class))
    }

    /// whether `candidate` stands on `ancestor` through this module's own layout chain
    fn descends_from(&self, candidate: &str, ancestor: &str) -> bool {
        let mut current = candidate;
        for _ in 0..=self.bases.len() {
            match self.bases.get(current) {
                Some(base) if base == ancestor => return true,
                Some(base) => current = base,
                None => return false,
            }
        }
        false
    }

    fn block(&mut self, body: &[Stmt]) -> Lowered<()> {
        for stmt in body {
            self.statement(stmt)?;
        }
        Ok(())
    }

    fn statement(&mut self, stmt: &Stmt) -> Lowered<()> {
        // the first statement lowered into a block decides that block's `#line`
        self.builder.block_at(span(stmt.range()));
        match stmt {
            Stmt::Pass(_) => Ok(()),
            // a declaration, not an action: the capture analysis already read it, and
            // the name resolves to the shared cell from here on
            // a declaration binds nothing: it says where a name lives, and the
            // lowering already knows
            Stmt::Nonlocal(_) | Stmt::Global(_) => Ok(()),
            // `import x` inside a body binds the module to a local, exactly as the
            // statement does — the interpreter's own import machinery does the work
            Stmt::Import(node) => {
                for alias in &node.names {
                    let bound = alias.asname.as_ref().map_or_else(
                        || alias.name.split('.').next().unwrap_or(""),
                        |name| name.as_str(),
                    );
                    let module = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::CallPython {
                        dest: module,
                        callee: "__import__".to_string(),
                        args: vec![Value::Str(alias.name.to_string())],
                    });
                    // `__import__("a.b")` hands back `a`, which is what a plain
                    // `import a.b` binds; an `as` name wants the submodule instead
                    let value = match &alias.asname {
                        None => Value::Register(module),
                        Some(_) => {
                            let mut current = Value::Register(module);
                            for part in alias.name.split('.').skip(1) {
                                let dest = self.builder.temp(RType::OBJECT);
                                self.builder.push(Op::GetAttr {
                                    dest,
                                    receiver: current,
                                    name: part.to_string(),
                                });
                                current = Value::Register(dest);
                            }
                            current
                        }
                    };
                    let place = self.binding(bound, &RType::OBJECT);
                    self.write_place(&place, value, &RType::OBJECT)?;
                }
                Ok(())
            }
            // `from x import a, b`: one import of the module, then a name off it per
            // alias. the fromlist carries every name, because that is what makes the
            // importer resolve a submodule rather than only an attribute
            Stmt::ImportFrom(node) => {
                // python rejects a star import in a body outright, so this is only
                // reachable through a recovered parse — where binding a place called
                // `*` would emit C nobody could compile
                if node.names.iter().any(|alias| alias.name.as_str() == "*") {
                    return Err(Decline::new(
                        "`from x import *` binds names this frame does not know",
                    ));
                }
                let fromlist: Box<[String]> = node
                    .names
                    .iter()
                    .map(|alias| alias.name.to_string())
                    .collect();
                let module = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::ImportModule {
                    dest: module,
                    name: node
                        .module
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_default(),
                    fromlist,
                    level: node.level,
                });
                for alias in &node.names {
                    let dest = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::ImportFrom {
                        dest,
                        module: Value::Register(module),
                        name: alias.name.to_string(),
                    });
                    let bound = alias.asname.as_ref().unwrap_or(&alias.name);
                    let place = self.binding(bound.as_str(), &RType::OBJECT);
                    self.write_place(&place, Value::Register(dest), &RType::OBJECT)?;
                }
                Ok(())
            }
            // `del` on a subscript or an attribute is the protocol's own operation; on a
            // plain name it unbinds, which a register expresses with the same byte a
            // local some path may read before writing already carries
            Stmt::Delete(node) => {
                for target in &node.targets {
                    self.delete_target(target)?;
                }
                Ok(())
            }
            // a nested `def` binds its name to a method of the environment, bound to
            // the environment instance. the instance is allocated on first use, from
            // the captures' *current* values — which is only sound because a capture
            // that could still change was declined
            Stmt::FunctionDef(node) => self.nested_def(node),
            // `return` in a generator is exhaustion, not a value: the iterator protocol
            // reports it as `StopIteration`
            Stmt::Return(node) if self.generator.is_some() => {
                let value = match &node.value {
                    None => None,
                    Some(expr) => {
                        let (value, ty) = self.expression(expr)?;
                        let value = self.widen_to_object(value, &ty);
                        // the cleanups run before the frame ends and may reassign
                        // whatever the value came from, exactly as in a plain
                        // function — and an async one suspends, so it goes to a field
                        Some(self.hold_across_cleanups(value)?)
                    }
                };
                self.unwind(0)?;
                let class = self
                    .generator
                    .as_ref()
                    .map(|generator| generator.class.clone())
                    .unwrap_or_default();
                // the exception *is* the return, so it must leave without meeting the
                // cleanups it has already run — which it would if it were raised
                // under their error target
                let leaving = self.builder.new_block();
                self.builder.terminate(Terminator::Goto(leaving));
                self.builder.switch_to(leaving);
                let previous = self.builder.set_error_target(None);
                self.builder.push(Op::SetField {
                    receiver: Value::Register(RegisterId(0)),
                    class,
                    field: generators::STATE_FIELD.to_string(),
                    value: Value::Int(-1),
                });
                // the frame is finished, and the value goes with the finish — which is
                // how it reaches whatever was driving the iteration, and how `await`
                // gets a result. a bare `return` finishes with `None`, exactly as a
                // bare `return` from a plain function does
                let value = match value {
                    Some(place) => self.read_place(&place)?.0,
                    None => {
                        let nothing = self.builder.temp(RType::OBJECT);
                        self.builder.push(Op::Box {
                            dest: nothing,
                            src: Value::None,
                        });
                        Value::Register(nothing)
                    }
                };
                self.builder.push(Op::FinishFrame { value });
                self.builder.terminate(Terminator::Unreachable);
                self.builder.set_error_target(previous);
                Ok(())
            }
            Stmt::Return(node) => {
                let value = match &node.value {
                    // a bare `return` returns `None`, and the declared return type may
                    // be wider — a caller of an `object`-returning function gets the
                    // *object* `None` back, not the unboxed byte
                    None => {
                        let ret = self.ret.clone();
                        self.coerce(Value::None, &RType::NONE, &ret)?
                    }
                    // the elements go straight into the struct the caller reads back:
                    // the real `tuple` is built once, at the boundary a python caller
                    // comes through, rather than on every call
                    Some(expr) if let RType::Tuple(slots) = self.ret.clone() => {
                        self.return_tuple_display(expr, &slots)?
                    }
                    Some(expr) => {
                        let (value, ty) = self.expression(expr)?;
                        let ret = self.ret.clone();
                        self.coerce(value, &ty, &ret)?
                    }
                };
                // the value is captured *before* the cleanups run, because python
                // evaluates the return expression first and a `finally` may reassign
                // whatever it came from
                let value = self.hold(value)?;
                self.unwind(0)?;
                self.builder.terminate(Terminator::Return(value));
                Ok(())
            }
            Stmt::Assign(node) => {
                // a `list` display bound to a local whose representation is an
                // unboxed array builds the buffer directly — the elements never
                // become `PyObject *` at all
                if let [Expr::Name(name)] = node.targets.as_slice()
                    && let Some(Place::Register(dest)) = self.place(name.id.as_str())
                    && let RType::Array(element) = self.register_type(dest)?
                    && let Expr::List(display) = node.value.as_ref()
                    && !display.elts.iter().any(Expr::is_starred_expr)
                {
                    let mut items = Vec::with_capacity(display.elts.len());
                    for item in &display.elts {
                        let (value, ty) = self.expression(item)?;
                        items.push(self.coerce(value, &ty, &element)?);
                    }
                    self.builder.push(Op::ArrayNew { dest, items });
                    return Ok(());
                }
                if let [Expr::Name(name)] = node.targets.as_slice()
                    && let Some(Place::Register(dest)) = self.place(name.id.as_str())
                    && let RType::Array(element) = self.register_type(dest)?
                    && let Expr::ListComp(comprehension) = node.value.as_ref()
                {
                    let (value, ty) = self.comprehension(
                        &comprehension.generators,
                        &Comprehension::Array(&comprehension.elt, (*element).clone()),
                    )?;
                    return self.write_place(&Place::Register(dest), value, &ty);
                }
                // the value is evaluated first, then each target in turn — which is
                // python's order, and what makes a chained assignment bind them all
                // to *one* value
                let (value, ty) = self.expression(&node.value)?;
                for target in &node.targets {
                    self.assign_to(target, value.clone(), &ty)?;
                }
                Ok(())
            }
            Stmt::AnnAssign(node) => {
                // a declaration with no value binds nothing at all: it is an
                // annotation, and the name stays unbound until something assigns it
                let Some(value) = &node.value else {
                    return Ok(());
                };
                let (value, ty) = self.expression(value)?;
                self.assign_to(&node.target, value, &ty)
            }
            Stmt::AugAssign(node) => {
                if let Expr::Name(name) = node.target.as_ref()
                    && self.place(name.id.as_str()).is_none()
                {
                    return Err(Decline::new(format!(
                        "`{}` is augmented before it is assigned",
                        name.id
                    )));
                }
                let location = self.location(&node.target)?;
                let (target, target_ty) = self.read_location(&location)?;
                let (rhs, rhs_ty) = self.expression(&node.value)?;
                let op = binary_op(node.op)?;
                match &location {
                    // a register destination is also an operand, and codegen already
                    // computes into a temporary before releasing it
                    Location::Place(Place::Register(dest)) => {
                        let dest = *dest;
                        let declared = self.register_type(dest)?;
                        let result_ty = binary_result(op, &target_ty, &rhs_ty);
                        // writing the result straight back is only sound when the
                        // operation produces what the register holds. `s += "%s" % x`
                        // on a `str` does not: `%` goes through the object protocol, so
                        // the sum is an `object` and the register is a `str`
                        if result_ty == declared {
                            self.emit_binary(
                                dest,
                                op,
                                (target, &target_ty),
                                (rhs, &rhs_ty),
                                Mutation::InPlace,
                            );
                            return Ok(());
                        }
                        let temp = self.builder.temp(result_ty.clone());
                        self.emit_binary(
                            temp,
                            op,
                            (target, &target_ty),
                            (rhs, &rhs_ty),
                            Mutation::InPlace,
                        );
                        self.store(dest, Value::Register(temp), &result_ty)
                    }
                    // anything else is read, then written: a temporary keeps the read
                    // from being released by the write
                    other => {
                        let result_ty = binary_result(op, &target_ty, &rhs_ty);
                        let temp = self.builder.temp(result_ty.clone());
                        self.emit_binary(
                            temp,
                            op,
                            (target, &target_ty),
                            (rhs, &rhs_ty),
                            Mutation::InPlace,
                        );
                        let other = other.clone();
                        self.write_location(&other, Value::Register(temp), &result_ty)
                    }
                }
            }
            Stmt::If(node) => self.if_statement(node),
            Stmt::Match(node) => self.match_statement(node),
            Stmt::While(node) => self.while_statement(node),
            Stmt::For(node) => self.for_statement(node),
            // a jump out of a loop runs the cleanups *inside* it, and no more
            Stmt::Break(_) => match self.loops.last().copied() {
                Some((_, break_target, depth)) => {
                    self.unwind(depth)?;
                    self.builder.terminate(Terminator::Goto(break_target));
                    Ok(())
                }
                None => Err(Decline::new("`break` outside a loop")),
            },
            Stmt::Continue(_) => match self.loops.last().copied() {
                Some((continue_target, _, depth)) => {
                    self.unwind(depth)?;
                    self.builder.terminate(Terminator::Goto(continue_target));
                    Ok(())
                }
                None => Err(Decline::new("`continue` outside a loop")),
            },
            Stmt::With(node) => self.with_statement(node),
            Stmt::Try(node) => self.try_statement(node),
            Stmt::Assert(node) => self.assert_statement(node),
            Stmt::Raise(node) => self.raise_statement(node),
            Stmt::Expr(node) => {
                // an expression statement is only worth lowering for its effect
                self.expression(&node.value).map(|_| ())
            }
            other => Err(Decline::new(format!(
                "`{}` is not lowered yet",
                statement_kind(other)
            ))),
        }
    }

    /// one target of a `del`
    ///
    /// the statement stops where it raises, so the targets are lowered in source
    /// order and each one's failure edge leaves the rest of them alone: after
    /// `del a, b` raises on `a`, python has not touched `b`
    fn delete_target(&mut self, target: &Expr) -> Lowered<()> {
        match target {
            Expr::Subscript(subscript) => {
                let (container, container_ty) = self.expression(&subscript.value)?;
                let container = self.widen_to_object(container, &container_ty);
                let (index, index_ty) = self.expression(&subscript.slice)?;
                let index = self.widen_to_object(index, &index_ty);
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::DeleteItem {
                    dest: status,
                    container,
                    index,
                });
                Ok(())
            }
            Expr::Attribute(attribute) => {
                let (receiver, receiver_ty) = self.expression(&attribute.value)?;
                let receiver = self.widen_to_object(receiver, &receiver_ty);
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::DeleteAttr {
                    dest: status,
                    receiver,
                    name: self.attribute_name(&attribute.attr),
                });
                Ok(())
            }
            // `del (a, b)` is `del a, b` written with brackets: python flattens a
            // display target into its elements rather than deleting a tuple
            Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
                for element in elts {
                    self.delete_target(element)?;
                }
                Ok(())
            }
            Expr::Name(name) => match self.place(name.id.as_str()) {
                // a name in the module namespace unbinds by leaving the dict, and an
                // unbound read there is `NameError` rather than `UnboundLocalError` —
                // which is why it is its own operation
                Some(Place::Global { .. }) => {
                    let status = self.builder.temp(RType::BIT);
                    self.builder.push(Op::DeleteGlobal {
                        dest: status,
                        name: name.id.to_string(),
                    });
                    Ok(())
                }
                // a local unbinds by clearing the byte that says whether it was
                // written — the one the unbound-locals pass gives it — so a later read
                // raises `UnboundLocalError`, a second `del` raises the same, and a
                // later write binds the name again
                Some(Place::Register(id)) => {
                    self.builder.push(Op::DeleteLocal { dest: id });
                    Ok(())
                }
                // a name that lives in an environment object belongs to a frame this
                // one does not own: unbinding it means clearing the field, and every
                // *reader* of it — here and in every frame that shares it — would then
                // have to test for NULL where today it just reads
                Some(Place::Cell { .. } | Place::Field { .. } | Place::Chained { .. }) => {
                    Err(Decline::new(format!(
                        "`del {}` unbinds a name another frame shares, which is a \
                         cell rather than a register",
                        name.id
                    )))
                }
                // python makes a name local for the whole function as soon as anything
                // in it binds or deletes the name, and `del` alone is such a statement.
                // this lowering decides what is local from the *writes*, so a name only
                // ever deleted resolved as a global everywhere else in the body — and
                // the reads would have gone to the wrong place
                None => Err(Decline::new(format!(
                    "`del {}` is the only statement binding `{}` in this function, and a \
                     name deleted but never assigned is local for the whole of it",
                    name.id, name.id
                ))),
            },
            other => Err(Decline::new(format!(
                "`del` on {} is not lowered yet",
                expression_kind(other)
            ))),
        }
    }

    /// `match` — the subject once, then each case in order
    ///
    /// a case is a *test* and a set of *bindings*, and the two are separate
    /// because python binds before it evaluates a guard, and leaves the binding
    /// behind when the guard then fails
    fn match_statement(&mut self, node: &ast::StmtMatch) -> Lowered<()> {
        let (subject, subject_ty) = self.expression(&node.subject)?;
        let subject = self.widen_to_object(subject, &subject_ty);
        // held in a register, because every case reads the same value and the
        // subject expression is evaluated once
        let held = self.builder.temp(RType::OBJECT);
        self.builder.assign(held, subject);
        let subject = Value::Register(held);

        let join = self.builder.new_block();
        let mut join_reachable = false;
        let mut fell_through = true;

        for case in &node.cases {
            let body_block = self.builder.new_block();
            let next_block = self.builder.new_block();
            self.pattern_branch(&case.pattern, &subject, body_block, next_block)?;

            self.builder.switch_to(body_block);
            if let Some(guard) = &case.guard {
                let (cond, cond_ty) = self.expression(guard)?;
                let cond = self.truthy(cond, &cond_ty);
                let guarded = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond,
                    then_block: guarded,
                    else_block: next_block,
                });
                self.builder.switch_to(guarded);
            }
            self.block(&case.body)?;
            join_reachable |= !self.builder.is_sealed(self.builder.current_block());
            self.builder.terminate(Terminator::Goto(join));

            self.builder.switch_to(next_block);
            // an unguarded pattern that matches everything is the last one that can
            // run: nothing reaches the block after it, and saying so is what lets a
            // function whose every case returns not look like it falls off the end
            if case.guard.is_none() && irrefutable(&case.pattern) {
                self.builder.terminate(Terminator::Unreachable);
                fell_through = false;
                break;
            }
        }

        // no case matched, which python treats as no statement at all
        if fell_through {
            join_reachable = true;
            self.builder.terminate(Terminator::Goto(join));
        }
        self.builder.switch_to(join);
        if !join_reachable {
            self.builder.terminate(Terminator::Unreachable);
        }
        Ok(())
    }

    /// emit `pattern`'s test against `subject`, branching to one block or the other
    ///
    /// the branch rather than a value, because alternatives in `P | Q` have to
    /// short-circuit: a `__eq__` that ran for `Q` after `P` already matched would
    /// be a call python never makes.
    ///
    /// bindings happen *here*, as the test goes, rather than in a pass of their
    /// own — a sequence element has to be read once, and reading it again to bind
    /// it would run `__getitem__` twice. a pattern that fails partway leaves what
    /// it had already bound, which is what the interpreter does too
    fn pattern_branch(
        &mut self,
        pattern: &ast::Pattern,
        subject: &Value,
        matched: by_ir::ops::BlockId,
        unmatched: by_ir::ops::BlockId,
    ) -> Lowered<()> {
        match pattern {
            // `case P as x:` — `P`'s test, then the binding on the way through
            ast::Pattern::MatchAs(node) => {
                let bound = match &node.pattern {
                    Some(inner) => {
                        let inner_matched = self.builder.new_block();
                        self.pattern_branch(inner, subject, inner_matched, unmatched)?;
                        self.builder.switch_to(inner_matched);
                        inner_matched
                    }
                    None => self.builder.current_block(),
                };
                let _ = bound;
                self.bind_pattern_name(node.name.as_ref(), subject)?;
                self.builder.terminate(Terminator::Goto(matched));
                Ok(())
            }
            ast::Pattern::MatchValue(node) => {
                let (value, value_ty) = self.expression(&node.value)?;
                let cond = self.emit_compare(
                    AstCmpOp::Eq,
                    (subject.clone(), RType::OBJECT),
                    (value, value_ty),
                )?;
                self.builder.terminate(Terminator::Branch {
                    cond,
                    then_block: matched,
                    else_block: unmatched,
                });
                Ok(())
            }
            // `case None:` / `case True:` / `case False:` are identity tests, so a
            // type with its own `__eq__` cannot answer for them
            ast::Pattern::MatchSingleton(node) => {
                let literal = match node.value {
                    ast::Singleton::None => Value::None,
                    ast::Singleton::True => Value::Bool(true),
                    ast::Singleton::False => Value::Bool(false),
                };
                let rtype = match node.value {
                    ast::Singleton::None => RType::NONE,
                    _ => RType::BOOL,
                };
                let literal = self.widen_to_object(literal, &rtype);
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::Identity {
                    dest,
                    lhs: subject.clone(),
                    rhs: literal,
                    negated: false,
                });
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(dest),
                    then_block: matched,
                    else_block: unmatched,
                });
                Ok(())
            }
            // `case [a, b]:` — the shape the interpreter's own `MATCH_SEQUENCE`
            // accepts, which `str` and `bytes` are deliberately not part of
            ast::Pattern::MatchSequence(node) => {
                // a star swallows whatever is left over, so the fixed patterns
                // *after* it are counted from the end rather than the front
                let star = node
                    .patterns
                    .iter()
                    .position(|p| matches!(p, ast::Pattern::MatchStar(_)));
                if node
                    .patterns
                    .iter()
                    .skip(star.map_or(0, |index| index + 1))
                    .any(|p| matches!(p, ast::Pattern::MatchStar(_)))
                {
                    return Err(Decline::new("a sequence pattern with two stars"));
                }
                let shaped = self.builder.temp(RType::BIT);
                self.builder.push(Op::IsSequence {
                    dest: shaped,
                    src: subject.clone(),
                });
                let sized = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(shaped),
                    then_block: sized,
                    else_block: unmatched,
                });
                self.builder.switch_to(sized);

                let length = self.builder.temp(RType::INT);
                self.builder.push(Op::Len {
                    dest: length,
                    src: subject.clone(),
                });
                let fixed = node.patterns.len() - usize::from(star.is_some());
                let count = i64::try_from(fixed)
                    .map_err(|_| Decline::new("a sequence pattern with too many elements"))?;
                let right_length = self.builder.temp(RType::BIT);
                // a star makes the length a minimum rather than an exact count
                self.builder.push(Op::IntCompare {
                    dest: right_length,
                    op: if star.is_some() { CmpOp::Ge } else { CmpOp::Eq },
                    lhs: Value::Register(length),
                    rhs: Value::Int(count),
                });
                let elements = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(right_length),
                    then_block: elements,
                    else_block: unmatched,
                });
                self.builder.switch_to(elements);

                let total = node.patterns.len();
                for (index, element) in node.patterns.iter().enumerate() {
                    let read = self.builder.temp(RType::OBJECT);
                    // before the star an element has a fixed index; after it, only
                    // a fixed distance from the end
                    let after = i64::try_from(total - index - 1)
                        .map_err(|_| Decline::new("a sequence pattern with too many elements"))?;
                    let position = i64::try_from(index)
                        .map_err(|_| Decline::new("a sequence pattern with too many elements"))?;
                    match star {
                        Some(at) if index == at => self.builder.push(Op::MatchSlice {
                            dest: read,
                            sequence: subject.clone(),
                            start: position,
                            after,
                            rest: true,
                        }),
                        Some(at) if index > at => self.builder.push(Op::MatchSlice {
                            dest: read,
                            sequence: subject.clone(),
                            start: position,
                            after: after + 1,
                            rest: false,
                        }),
                        _ => self.builder.push(Op::GetItem {
                            dest: read,
                            container: subject.clone(),
                            index: Value::Int(position),
                        }),
                    }
                    let next = if index + 1 == total {
                        matched
                    } else {
                        self.builder.new_block()
                    };
                    // a star's own pattern is the name it binds, if it has one
                    match element {
                        ast::Pattern::MatchStar(starred) => {
                            self.bind_pattern_name(starred.name.as_ref(), &Value::Register(read))?;
                            self.builder.terminate(Terminator::Goto(next));
                        }
                        _ => {
                            self.pattern_branch(element, &Value::Register(read), next, unmatched)?;
                        }
                    }
                    if next != matched {
                        self.builder.switch_to(next);
                    }
                }
                if node.patterns.is_empty() {
                    self.builder.terminate(Terminator::Goto(matched));
                }
                Ok(())
            }
            // `case {'k': v, **rest}:` — the shape, then each key it names
            ast::Pattern::MatchMapping(node) => {
                // a key is a value, so two of them are only known to be equal once
                // evaluated — which is why python's duplicate check is a runtime
                // `ValueError`. declining hands that to the definition that raises it
                let mut literals = Vec::with_capacity(node.keys.len());
                for key in &node.keys {
                    let literal = literal_value(key).ok_or_else(|| {
                        Decline::new("only a literal mapping-pattern key is lowered yet")
                    })?;
                    if literals.contains(&literal) {
                        return Err(Decline::new(
                            "a mapping pattern with a duplicate key is a runtime error",
                        ));
                    }
                    literals.push(literal);
                }

                let shaped = self.builder.temp(RType::BIT);
                self.builder.push(Op::IsMapping {
                    dest: shaped,
                    src: subject.clone(),
                });
                let keyed = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(shaped),
                    then_block: keyed,
                    else_block: unmatched,
                });
                self.builder.switch_to(keyed);

                let mut keys = Vec::with_capacity(node.keys.len());
                let steps = node.keys.len() + usize::from(node.rest.is_some());
                for (index, (key, value)) in node.keys.iter().zip(&node.patterns).enumerate() {
                    let (key, key_ty) = self.expression(key)?;
                    let key = self.widen_to_object(key, &key_ty);
                    keys.push(key.clone());
                    let read = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::MatchKey {
                        dest: read,
                        map: subject.clone(),
                        key,
                    });
                    let missing = self.builder.temp(RType::BIT);
                    self.builder.push(Op::IsMissing {
                        dest: missing,
                        src: Value::Register(read),
                    });
                    let present = self.builder.new_block();
                    self.builder.terminate(Terminator::Branch {
                        cond: Value::Register(missing),
                        then_block: unmatched,
                        else_block: present,
                    });
                    self.builder.switch_to(present);

                    let next = if index + 1 == steps {
                        matched
                    } else {
                        self.builder.new_block()
                    };
                    self.pattern_branch(value, &Value::Register(read), next, unmatched)?;
                    if next != matched {
                        self.builder.switch_to(next);
                    }
                }
                // `**rest` is everything the pattern did not name, as a plain dict
                if let Some(rest) = &node.rest {
                    let named = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::BuildTuple {
                        dest: named,
                        items: keys,
                    });
                    let read = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::MatchRest {
                        dest: read,
                        map: subject.clone(),
                        keys: Value::Register(named),
                    });
                    self.bind_pattern_name(Some(rest), &Value::Register(read))?;
                    self.builder.terminate(Terminator::Goto(matched));
                } else if node.keys.is_empty() {
                    self.builder.terminate(Terminator::Goto(matched));
                }
                Ok(())
            }
            // `case Point(x=1):` — the class, then each attribute it names
            ast::Pattern::MatchClass(node) => {
                let (class, class_ty) = self.expression(&node.cls)?;
                let class = self.widen_to_object(class, &class_ty);
                let is_instance = self.builder.temp(RType::BIT);
                self.builder.push(Op::IsInstance {
                    dest: is_instance,
                    src: subject.clone(),
                    class: class.clone(),
                });
                let attributes = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(is_instance),
                    then_block: attributes,
                    else_block: unmatched,
                });
                self.builder.switch_to(attributes);

                // a positional sub-pattern names its attribute through the
                // class's `__match_args__`, which only exists at runtime
                let positional = &node.arguments.patterns;
                let keywords = &node.arguments.keywords;
                let count = i64::try_from(positional.len())
                    .map_err(|_| Decline::new("a class pattern with too many sub-patterns"))?;
                let total = positional.len() + keywords.len();
                let mut done = 0;
                let branch_to_sub = |lowering: &mut Self,
                                     read: RegisterId,
                                     sub: &ast::Pattern,
                                     done: &mut usize| {
                    *done += 1;
                    let next = if *done == total {
                        matched
                    } else {
                        lowering.builder.new_block()
                    };
                    lowering.pattern_branch(sub, &Value::Register(read), next, unmatched)?;
                    if next != matched {
                        lowering.builder.switch_to(next);
                    }
                    Ok::<(), Decline>(())
                };
                for (index, sub) in positional.iter().enumerate() {
                    let read = self.read_match_attr(
                        &subject.clone(),
                        None,
                        Some(class.clone()),
                        i64::try_from(index).unwrap_or(0),
                        count,
                        unmatched,
                    );
                    branch_to_sub(self, read, sub, &mut done)?;
                }
                for keyword in keywords {
                    let read = self.read_match_attr(
                        &subject.clone(),
                        Some(self.attribute_name(&keyword.attr)),
                        None,
                        0,
                        0,
                        unmatched,
                    );
                    branch_to_sub(self, read, &keyword.pattern, &mut done)?;
                }
                if total == 0 {
                    self.builder.terminate(Terminator::Goto(matched));
                }
                Ok(())
            }
            // basedpython: `case P and Q:` — every one has to match the *same*
            // subject, which is the mirror of `P | Q` and needs no restriction on
            // what they bind, because they all run
            ast::Pattern::MatchAnd(node) => {
                let Some((last, rest)) = node.patterns.split_last() else {
                    return Err(Decline::new("a conjunction pattern with no patterns"));
                };
                for pattern in rest {
                    let next = self.builder.new_block();
                    self.pattern_branch(pattern, subject, next, unmatched)?;
                    self.builder.switch_to(next);
                }
                self.pattern_branch(last, subject, matched, unmatched)
            }
            ast::Pattern::MatchOr(node) => {
                let Some((last, rest)) = node.patterns.split_last() else {
                    return Err(Decline::new("an alternative pattern with no alternatives"));
                };
                for alternative in rest {
                    // an alternative that binds would have to agree with every
                    // other on what it bound, which is a analysis of its own
                    if binds_a_name(alternative) {
                        return Err(Decline::new(
                            "an alternative pattern that binds a name is not lowered yet",
                        ));
                    }
                    let try_next = self.builder.new_block();
                    self.pattern_branch(alternative, subject, matched, try_next)?;
                    self.builder.switch_to(try_next);
                }
                if binds_a_name(last) {
                    return Err(Decline::new(
                        "an alternative pattern that binds a name is not lowered yet",
                    ));
                }
                self.pattern_branch(last, subject, matched, unmatched)
            }
            ast::Pattern::MatchStar(_) => {
                Err(Decline::new("a star pattern outside a sequence pattern"))
            }
        }
    }

    /// read the attribute a class pattern names, jumping to `unmatched` when the
    /// subject simply does not have it
    ///
    /// absent is an answer rather than a failure: `case Point(z=1):` against a
    /// point with no `z` falls through to the next case rather than raising
    fn read_match_attr(
        &mut self,
        subject: &Value,
        name: Option<String>,
        class: Option<Value>,
        index: i64,
        count: i64,
        unmatched: by_ir::ops::BlockId,
    ) -> RegisterId {
        let read = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::MatchAttr {
            dest: read,
            subject: subject.clone(),
            name,
            class,
            index,
            count,
        });
        let missing = self.builder.temp(RType::BIT);
        self.builder.push(Op::IsMissing {
            dest: missing,
            src: Value::Register(read),
        });
        let present = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(missing),
            then_block: unmatched,
            else_block: present,
        });
        self.builder.switch_to(present);
        read
    }

    /// bind a capture's name to what it matched, when it has one
    fn bind_pattern_name(
        &mut self,
        name: Option<&ast::Identifier>,
        subject: &Value,
    ) -> Lowered<()> {
        let Some(name) = name else { return Ok(()) };
        let place = self.binding(name.as_str(), &RType::OBJECT);
        self.write_place(&place, subject.clone(), &RType::OBJECT)
    }

    fn if_statement(&mut self, node: &ast::StmtIf) -> Lowered<()> {
        let (cond, cond_ty) = self.expression(&node.test)?;
        let cond = self.truthy(cond, &cond_ty);

        let then_block = self.builder.new_block();
        let else_block = self.builder.new_block();
        let join = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond,
            then_block,
            else_block,
        });

        // a join nothing reaches — every arm returned — must close as
        // unreachable, or the function looks like it falls off the end
        let mut join_reachable = false;

        self.builder.switch_to(then_block);
        self.block(&node.body)?;
        join_reachable |= !self.builder.is_sealed(self.builder.current_block());
        self.builder.terminate(Terminator::Goto(join));

        self.builder.switch_to(else_block);
        let mut has_else = false;
        for clause in &node.elif_else_clauses {
            match &clause.test {
                // `else:` — the last clause, lowered in the current else block
                None => {
                    has_else = true;
                    self.block(&clause.body)?;
                }
                // `elif:` — a fresh branch inside the else block
                Some(test) => {
                    let (cond, cond_ty) = self.expression(test)?;
                    let cond = self.truthy(cond, &cond_ty);
                    let elif_then = self.builder.new_block();
                    let elif_else = self.builder.new_block();
                    self.builder.terminate(Terminator::Branch {
                        cond,
                        then_block: elif_then,
                        else_block: elif_else,
                    });
                    self.builder.switch_to(elif_then);
                    self.block(&clause.body)?;
                    join_reachable |= !self.builder.is_sealed(self.builder.current_block());
                    self.builder.terminate(Terminator::Goto(join));
                    self.builder.switch_to(elif_else);
                }
            }
        }
        // with no `else`, the implicit empty one always falls through
        join_reachable |= !has_else || !self.builder.is_sealed(self.builder.current_block());
        self.builder.terminate(Terminator::Goto(join));

        self.builder.switch_to(join);
        if !join_reachable {
            self.builder.terminate(Terminator::Unreachable);
        }
        Ok(())
    }

    /// whether this exact `A[i]` is a pair an enclosing counting loop proved
    fn proven_in_range(&self, array: &Expr, index: &Expr) -> bool {
        let (Expr::Name(array), Expr::Name(index)) = (array, index) else {
            return false;
        };
        self.in_range
            .iter()
            .any(|(counter, over)| counter == index.id.as_str() && over == array.id.as_str())
    }

    fn while_statement(&mut self, node: &ast::StmtWhile) -> Lowered<()> {
        // `while i < len(A)` with a counting `i` and an `A` the body leaves alone puts
        // every `A[i]` inside it in range, which is the one thing the checked read tests
        let proven = counted_over(node);
        let header = self.builder.new_block();
        let body = self.builder.new_block();
        // the natural exit and the `break` exit are different blocks: `else` runs
        // only on the first, which is the whole meaning of a loop `else`
        let natural_exit = self.builder.new_block();
        let after = self.builder.new_block();

        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let (cond, cond_ty) = self.expression(&node.test)?;
        let cond = self.truthy(cond, &cond_ty);
        self.builder.terminate(Terminator::Branch {
            cond,
            then_block: body,
            else_block: natural_exit,
        });

        self.builder.switch_to(body);
        self.loops.push((header, after, self.cleanups.len()));
        if let Some(pair) = proven.clone() {
            self.in_range.push(pair);
        }
        let result = self.block(&node.body);
        if proven.is_some() {
            self.in_range.pop();
        }
        self.loops.pop();
        result?;
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(natural_exit);
        self.block(&node.orelse)?;
        self.builder.terminate(Terminator::Goto(after));

        self.builder.switch_to(after);
        Ok(())
    }

    /// `try` / `except` / `else` / `finally`
    ///
    /// the handler is a real block, and every failing operation in the body has
    /// its `error_target` pointing at it — so an exception edge is a CFG edge
    /// rather than an implicit jump to one place.
    ///
    /// `finally` is *duplicated* along each exit path rather than run through a
    /// saved-state trampoline, which is what lets the C compiler optimize the
    /// normal path without the exceptional one weighing on it
    /// copy a value into a temporary, so a later cleanup cannot change it
    ///
    /// only where there *is* a cleanup: with nothing to run, nothing can change it,
    /// and the copy would be noise in the IR
    fn hold(&mut self, value: Value) -> Lowered<Value> {
        if self.cleanups.is_empty() {
            return Ok(value);
        }
        let ty = match &value {
            Value::Register(id) => self.register_type(*id)?,
            other => match other.immediate_type() {
                Some(ty) => ty,
                None => return Ok(value),
            },
        };
        let held = self.builder.temp(ty);
        self.builder.assign(held, value);
        Ok(Value::Register(held))
    }

    /// keep a value alive across the cleanups that are about to run
    ///
    /// awaiting `__aexit__` suspends the frame, and a register does not survive a
    /// suspension — so when a cleanup can suspend the value goes into a field
    fn hold_across_cleanups(&mut self, value: Value) -> Lowered<Place> {
        let suspends = self
            .cleanups
            .iter()
            .any(|cleanup| matches!(cleanup, Cleanup::Context(_, true)));
        let held = self.builder.temp(RType::OBJECT);
        self.builder.assign(held, value);
        if suspends {
            self.park_iterator(held)
        } else {
            Ok(Place::Register(held))
        }
    }

    /// run every cleanup above `depth`, innermost first, without popping them
    ///
    /// the stack stays intact because the *fall-through* path still has to run them
    /// too — an early exit is an extra path, not a replacement for the normal one
    fn unwind(&mut self, depth: usize) -> Lowered<()> {
        let pending: Vec<Cleanup> = self.cleanups[depth..].iter().rev().cloned().collect();
        for cleanup in pending {
            match cleanup {
                Cleanup::Finally(body) => self.block(&body)?,
                Cleanup::Handled(handled) => self.builder.push(Op::PopHandled {
                    value: Value::Register(handled),
                }),
                Cleanup::Context(manager, is_async) => {
                    let (manager, _) = self.read_place(&manager)?;
                    let ignored = self.builder.temp(RType::BIT);
                    let none = self.widen_to_object(Value::None, &RType::NONE);
                    if is_async {
                        self.await_exit(manager, none, ignored)?;
                    } else {
                        self.builder.push(Op::ExitContext {
                            dest: ignored,
                            manager,
                            exception: none,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// `with EXPR as VAR: BLOCK`
    ///
    /// the exceptional path is the whole point: `__exit__` runs either way, and it
    /// decides whether the exception continues. so the body gets an error target the
    /// way a `try` body does, and the handler asks `__exit__` before re-raising
    fn with_statement(&mut self, node: &ast::StmtWith) -> Lowered<()> {
        // `with a() as x, b() as y:` is `with a() as x:` around `with b() as y:`,
        // which is what python's own desugaring says it is
        let Some((item, rest)) = node.items.split_first() else {
            return Ok(());
        };

        let (manager, manager_ty) = self.expression(&item.context_expr)?;
        let manager = self.widen_to_object(manager, &manager_ty);
        // the manager is read again on both exits, so it lives in a register of its
        // own rather than being re-evaluated
        let held = self
            .builder
            .local(format!("$manager{}", self.contexts), RType::OBJECT);
        self.contexts += 1;
        self.builder.assign(held, manager);
        // inside a generator the manager outlives the frame: the body suspends and
        // `__exit__` still has to run, whether the resumption returns or raises
        let held = self.park_iterator(held)?;

        let entered = self.builder.temp(RType::OBJECT);
        let (live, _) = self.read_place(&held)?;
        if node.is_async {
            // `__aenter__` hands back an awaitable, so the value the block binds is
            // what awaiting *that* produced
            let awaitable = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::AsyncContext {
                dest: awaitable,
                manager: live,
                exception: None,
            });
            let (value, ty) = self.delegate_value(Value::Register(awaitable), true)?;
            let value = self.widen_to_object(value, &ty);
            self.builder.assign(entered, value);
        } else {
            self.builder.push(Op::Enter {
                dest: entered,
                manager: live,
            });
        }
        if let Some(target) = &item.optional_vars {
            match target.as_ref() {
                Expr::Name(name) => {
                    let ty = self.peek_type(target)?;
                    let value = self.coerce(Value::Register(entered), &RType::OBJECT, &ty)?;
                    let place = self.binding(name.id.as_str(), &ty);
                    self.write_place(&place, value, &ty)?;
                }
                other => self.assign_to(other, Value::Register(entered), &RType::OBJECT)?,
            }
        }

        let handler = self.builder.new_block();
        let body_block = self.builder.new_block();
        let success = self.builder.new_block();
        let after = self.builder.new_block();

        self.builder.terminate(Terminator::Goto(body_block));
        self.builder.switch_to(body_block);
        let previous = self.builder.set_error_target(Some(handler));
        self.cleanups
            .push(Cleanup::Context(held.clone(), node.is_async));
        let body_result = if rest.is_empty() {
            self.block(&node.body)
        } else {
            let inner = ast::StmtWith {
                items: rest.to_vec(),
                ..node.clone()
            };
            self.with_statement(&inner)
        };
        self.cleanups.pop();
        let mut success_reached = false;
        if body_result.is_ok() && !self.builder.is_sealed(self.builder.current_block()) {
            success_reached = true;
            self.builder.terminate(Terminator::Goto(success));
        }
        self.builder.set_error_target(previous);
        body_result?;

        // the normal exit: `__exit__(None, None, None)`, and its answer is ignored
        self.builder.switch_to(success);
        let ignored = self.builder.temp(RType::BIT);
        let no_exception = self.widen_to_object(Value::None, &RType::NONE);
        let (live, _) = self.read_place(&held)?;
        if node.is_async {
            self.await_exit(live, no_exception, ignored)?;
        } else {
            self.builder.push(Op::ExitContext {
                dest: ignored,
                manager: live,
                exception: no_exception,
            });
        }
        self.builder.terminate(Terminator::Goto(after));

        // the exceptional exit: `__exit__` decides whether it continues
        self.builder.switch_to(handler);
        let exception = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::FetchException { dest: exception });
        let suppressed = self.builder.temp(RType::BIT);
        // its own read: this block is reached after a suspension the other never
        // saw, and the register the normal exit used is stale by then
        let (raising, _) = self.read_place(&held)?;
        // awaiting the exit suspends, and the reraise below still needs the
        // exception afterwards — so it goes into a field too
        let parked_exception = if node.is_async {
            Some(self.park_iterator(exception)?)
        } else {
            None
        };
        if node.is_async {
            self.await_exit(raising, Value::Register(exception), suppressed)?;
        } else {
            self.builder.push(Op::ExitContext {
                dest: suppressed,
                manager: raising,
                exception: Value::Register(exception),
            });
        }
        let reraise = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(suppressed),
            then_block: after,
            else_block: reraise,
        });
        self.builder.switch_to(reraise);
        let exception = match &parked_exception {
            Some(parked) => {
                let (live, _) = self.read_place(parked)?;
                match live {
                    Value::Register(id) => id,
                    other => {
                        let dest = self.builder.temp(RType::OBJECT);
                        self.builder.assign(dest, other);
                        dest
                    }
                }
            }
            None => exception,
        };
        self.builder.push(Op::Reraise {
            value: Value::Register(exception),
        });
        self.builder.terminate(Terminator::Unreachable);

        self.builder.switch_to(after);
        if !success_reached {
            // every path through the body raised, so `after` is only reachable from
            // the suppressing handler
        }
        Ok(())
    }

    fn try_statement(&mut self, node: &ast::StmtTry) -> Lowered<()> {
        if node.is_star {
            return Err(Decline::new("`except*` is not lowered yet"));
        }

        let handler_entry = self.builder.new_block();
        let body_block = self.builder.new_block();
        let success = self.builder.new_block();
        let after = self.builder.new_block();

        // the body gets its own block, because a block records its error target
        // when it is *sealed* — leaving the body in the enclosing block would seal
        // it after the target had been restored
        self.builder.terminate(Terminator::Goto(body_block));
        self.builder.switch_to(body_block);

        let previous = self.builder.set_error_target(Some(handler_entry));
        if !node.finalbody.is_empty() {
            self.cleanups
                .push(Cleanup::Finally(node.finalbody.to_vec()));
        }
        let body_result = self.block(&node.body);
        if !node.finalbody.is_empty() {
            self.cleanups.pop();
        }
        // whether the body can fall out of the bottom, rather than every path
        // returning or raising
        let mut success_reached = false;
        if body_result.is_ok() && !self.builder.is_sealed(self.builder.current_block()) {
            success_reached = true;
            self.builder.terminate(Terminator::Goto(success));
        }
        self.builder.set_error_target(previous);
        body_result?;

        // `after` is only reachable if something actually jumps to it. when every
        // path returns it has to close as unreachable, or the function looks like
        // it runs off the end
        let mut after_reached = false;

        // the success path runs `else`, then `finally`
        self.builder.switch_to(success);
        self.block(&node.orelse)?;
        self.block(&node.finalbody)?;
        if success_reached && !self.builder.is_sealed(self.builder.current_block()) {
            after_reached = true;
        }
        self.builder.terminate(Terminator::Goto(after));

        self.builder.switch_to(handler_entry);
        let exception = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::FetchException { dest: exception });

        for handler in &node.handlers {
            let ast::ExceptHandler::ExceptHandler(handler) = handler;
            let matched = self.builder.new_block();
            let next = self.builder.new_block();

            match &handler.type_ {
                // a bare `except:` catches everything
                None => self.builder.terminate(Terminator::Goto(matched)),
                Some(class) => {
                    // an ordinary expression, so a user-defined class, a tuple of
                    // them and a *shadowed* builtin all take one path
                    let (value, ty) = self.expression(class)?;
                    let class = self.widen_to_object(value, &ty);
                    let test = self.builder.temp(RType::BIT);
                    self.builder.push(Op::ExceptionMatches {
                        dest: test,
                        value: Value::Register(exception),
                        class,
                    });
                    self.builder.terminate(Terminator::Branch {
                        cond: Value::Register(test),
                        then_block: matched,
                        else_block: next,
                    });
                }
            }

            self.builder.switch_to(matched);
            if let Some(bound) = &handler.name {
                let target = self.binding(bound.as_str(), &RType::OBJECT);
                self.write_place(&target, Value::Register(exception), &RType::OBJECT)?;
            }
            // from here the exception is being *handled*, which is what makes a raise
            // inside the block — or inside anything it calls — chain onto it
            let handled = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::PushHandled {
                dest: handled,
                value: Value::Register(exception),
            });

            // the body gets its own block for the same reason the `try` body does,
            // and its own error target so leaving by raising still unwinds
            let raised = self.builder.new_block();
            let normal = self.builder.new_block();
            let handler_body = self.builder.new_block();
            self.builder.terminate(Terminator::Goto(handler_body));
            self.builder.switch_to(handler_body);
            let outer = self.builder.set_error_target(Some(raised));
            // reversed on the way out, so `finally` runs *after* the handled
            // exception is put back — which is the order python leaves the block in
            if !node.finalbody.is_empty() {
                self.cleanups
                    .push(Cleanup::Finally(node.finalbody.to_vec()));
            }
            self.cleanups.push(Cleanup::Handled(handled));
            self.handling.push(exception);
            let body = self.block(&handler.body);
            self.handling.pop();
            self.cleanups.pop();
            if !node.finalbody.is_empty() {
                self.cleanups.pop();
            }
            let fell_through =
                body.is_ok() && !self.builder.is_sealed(self.builder.current_block());
            if fell_through {
                self.builder.terminate(Terminator::Goto(normal));
            }
            self.builder.set_error_target(outer);
            body?;

            // both exits put the handled exception back and run `finally`; they
            // differ only in what happens after
            self.builder.switch_to(normal);
            self.builder.push(Op::PopHandled {
                value: Value::Register(handled),
            });
            self.block(&node.finalbody)?;
            if fell_through && !self.builder.is_sealed(self.builder.current_block()) {
                after_reached = true;
            }
            self.builder.terminate(Terminator::Goto(after));

            self.builder.switch_to(raised);
            let pending = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::FetchException { dest: pending });
            self.builder.push(Op::PopHandled {
                value: Value::Register(handled),
            });
            self.block(&node.finalbody)?;
            self.builder.push(Op::Reraise {
                value: Value::Register(pending),
            });
            self.builder.terminate(Terminator::Unreachable);

            self.builder.switch_to(next);
        }

        // nothing matched: run `finally`, then let the exception continue
        self.block(&node.finalbody)?;
        self.builder.push(Op::Reraise {
            value: Value::Register(exception),
        });
        self.builder.terminate(Terminator::Unreachable);

        self.builder.switch_to(after);
        if !after_reached {
            self.builder.terminate(Terminator::Unreachable);
        }
        Ok(())
    }

    /// `assert cond` / `assert cond, "message"`
    fn assert_statement(&mut self, node: &ast::StmtAssert) -> Lowered<()> {
        let (cond, cond_ty) = self.expression(&node.test)?;
        let cond = self.truthy(cond, &cond_ty);
        let fail = self.builder.new_block();
        let ok = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond,
            then_block: ok,
            else_block: fail,
        });
        // the message is evaluated *inside* the failing block, because python only
        // evaluates it when the assertion fails
        self.builder.switch_to(fail);
        match &node.msg {
            None => self.builder.push(Op::RaiseStandard {
                error: StandardError::AssertionError,
                message: String::new(),
            }),
            Some(expr) => {
                let (value, ty) = self.expression(expr)?;
                let value = self.widen_to_object(value, &ty);
                self.builder.push(Op::RaiseWith {
                    error: StandardError::AssertionError,
                    value,
                });
            }
        }
        self.builder.terminate(Terminator::Unreachable);
        self.builder.switch_to(ok);
        Ok(())
    }

    /// `raise <exception>`, optionally `from <cause>`
    ///
    /// a raise of a builtin with a literal message goes straight to `PyErr_SetString`
    /// and never builds the instance; anything else evaluates the expression and
    /// raises the object, which is what the statement itself does
    fn raise_statement(&mut self, node: &ast::StmtRaise) -> Lowered<()> {
        let Some(exception) = &node.exc else {
            // a bare `raise` re-raises what the enclosing handler caught. outside one
            // it would need the *interpreter's* handled exception, which we never set
            let Some(&active) = self.handling.last() else {
                return Err(Decline::new(
                    "a bare `raise` outside an `except` block is not lowered yet",
                ));
            };
            self.builder.push(Op::Reraise {
                value: Value::Register(active),
            });
            self.builder.terminate(Terminator::Unreachable);
            return Ok(());
        };
        if node.cause.is_none()
            && let Ok(()) = self.raise_standard(exception)
        {
            return Ok(());
        }
        let (value, ty) = self.expression(exception)?;
        let exception = self.widen_to_object(value, &ty);
        let cause = match &node.cause {
            Some(cause) => {
                let (value, ty) = self.expression(cause)?;
                Some(self.widen_to_object(value, &ty))
            }
            None => None,
        };
        self.builder.push(Op::RaiseObject { exception, cause });
        self.builder.terminate(Terminator::Unreachable);
        Ok(())
    }

    /// the direct form: a builtin error class with at most a literal message
    ///
    /// declining here is not a decline of the statement — [`Self::raise_statement`]
    /// falls back to evaluating the expression
    fn raise_standard(&mut self, exception: &Expr) -> Lowered<()> {
        let (name, message) = match exception {
            Expr::Name(name) => (name.id.as_str(), String::new()),
            Expr::Call(call) => {
                let Expr::Name(name) = call.func.as_ref() else {
                    return Err(Decline::new("only `raise Cls(...)` is lowered yet"));
                };
                if !call.arguments.keywords.is_empty() {
                    return Err(Decline::new(
                        "keyword arguments to `raise` are not lowered yet",
                    ));
                }
                let message = match call.arguments.args.as_ref() {
                    [] => String::new(),
                    [Expr::StringLiteral(literal)] => literal.value.to_str().to_string(),
                    _ => {
                        return Err(Decline::new(
                            "only a literal string argument to `raise` is lowered yet",
                        ));
                    }
                };
                (name.id.as_str(), message)
            }
            _ => return Err(Decline::new("not the direct form")),
        };
        let Some(error) = StandardError::from_name(name) else {
            return Err(Decline::new("not a builtin error class"));
        };
        // a shadowed name is not the builtin, and python raises what the name is
        // bound to
        if self.binds(name) || self.native_callees.contains(name) {
            return Err(Decline::new("the name is shadowed"));
        }
        self.builder.push(Op::RaiseStandard { error, message });
        self.builder.terminate(Terminator::Unreachable);
        Ok(())
    }

    /// `for <name> in range(...)`, desugared to the equivalent counting loop
    ///
    /// only `range` is lowered: every other iterable needs the iteration
    /// protocol, which needs a boxed representation
    fn for_statement(&mut self, node: &ast::StmtFor) -> Lowered<()> {
        if node.is_async {
            return self.async_for(node);
        }
        // `range` gets a counting loop; anything else goes through the iteration
        // protocol, which is what the interpreter would do. a target *list* is
        // always the protocol path: `range` yields ints, which do not unpack
        let range_call = match (node.target.as_ref(), node.iter.as_ref()) {
            (Expr::Name(_), Expr::Call(call)) => match call.func.as_ref() {
                Expr::Name(callee)
                    if callee.id.as_str() == "range"
                        && !self.native_callees.contains("range")
                        && call.arguments.keywords.is_empty() =>
                {
                    Some(call)
                }
                _ => None,
            },
            _ => None,
        };
        // a `for` over an unboxed array is a counting loop over the buffer: no
        // iterator object, no null test per step, an `i64` counter, and no bounds
        // check — the counter is the lowering's own, so it is in range by
        // construction. this is the shape the whole representation exists for
        if let Expr::Name(target) = node.target.as_ref()
            && let Expr::Name(source) = node.iter.as_ref()
            // the *register's* representation, not the checker's type: `list[float]`
            // says nothing about whether this local earned a buffer
            && let Some(place) = self.place(source.id.as_str())
            && let Place::Register(id) = place
            && matches!(self.register_type(id)?, RType::Array(_))
        {
            return self.for_over_array(target, node);
        }
        let Some(call) = range_call else {
            return self.for_over_iterable(node);
        };
        let Expr::Name(target) = node.target.as_ref() else {
            return self.for_over_iterable(node);
        };
        // the counting loop increments its variable *in place*, which a shared cell
        // cannot be — a generator's locals are fields, so `for i in range(n)` inside
        // one takes the protocol path instead. slower, and it compiles
        if !matches!(
            self.place(target.id.as_str()),
            None | Some(Place::Register(_))
        ) {
            return self.for_over_iterable(node);
        }

        // the bounds are evaluated once, before the loop, exactly as `range` does.
        // every check from here on falls back to the protocol path rather than
        // declining: the counting loop is an optimisation, and one that cannot
        // apply must not cost the whole function
        let (start, stop, step) = match call.arguments.args.as_ref() {
            [stop] => (None, stop, None),
            [start, stop] => (Some(start), stop, None),
            [start, stop, step] => (Some(start), stop, Some(step)),
            // wrong arity is a `TypeError` `range` itself raises, which the
            // protocol path reaches by calling it
            _ => return self.for_over_iterable(node),
        };

        // a non-literal step would decide the comparison direction at runtime,
        // which needs a second loop shape
        let step_value = match step {
            None => Some(1),
            Some(expr) => match literal_step(expr) {
                // a step of zero is a `ValueError` `range` itself raises
                Some(0) | None => None,
                found => found,
            },
        };
        let Some(step_value) = step_value else {
            return self.for_over_iterable(node);
        };

        // the bounds come first, because whether this is a counting loop at all
        // depends on their representations — and they are evaluated in this order
        // either way, so nothing is computed twice
        let (start_value, start_ty) = match start {
            None => (Value::Int(0), RType::INT),
            Some(expr) => self.expression(expr)?,
        };
        let (stop_value, stop_ty) = self.expression(stop)?;
        if start_ty != RType::INT || stop_ty != RType::INT {
            // the bounds are already evaluated, so the protocol path takes the
            // `range` object built from them rather than the expression again
            let start_value = self.widen_to_object(start_value, &start_ty);
            let stop_value = self.widen_to_object(stop_value, &stop_ty);
            let step_value = self.widen_to_object(Value::Int(step_value), &RType::INT);
            let iterable = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::CallPython {
                dest: iterable,
                callee: "range".to_string(),
                args: vec![start_value, stop_value, step_value],
            });
            return self.for_over_value(node, Value::Register(iterable), &RType::OBJECT);
        }

        let index = self.binding_register(target.id.as_str(), &RType::INT)?;
        self.builder.assign(index, start_value);

        // the stop bound is read once into its own register, so a call or a
        // mutated local cannot change the trip count mid-loop
        let limit = self.builder.temp(RType::INT);
        self.builder.assign(limit, stop_value);

        let header = self.builder.new_block();
        let body = self.builder.new_block();
        let step_block = self.builder.new_block();
        let natural_exit = self.builder.new_block();
        let after = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let cond = self.builder.temp(RType::BIT);
        self.builder.push(Op::IntCompare {
            dest: cond,
            op: if step_value > 0 { CmpOp::Lt } else { CmpOp::Gt },
            lhs: Value::Register(index),
            rhs: Value::Register(limit),
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(cond),
            then_block: body,
            else_block: natural_exit,
        });

        self.builder.switch_to(body);
        // `continue` jumps to the step, not the header, or the index never moves
        self.loops.push((step_block, after, self.cleanups.len()));
        let result = self.block(&node.body);
        self.loops.pop();
        result?;
        self.builder.terminate(Terminator::Goto(step_block));

        self.builder.switch_to(step_block);
        self.builder.push(Op::IntBinary {
            dest: index,
            op: BinOp::Add,
            lhs: Value::Register(index),
            rhs: Value::Int(step_value),
        });
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(natural_exit);
        self.block(&node.orelse)?;
        self.builder.terminate(Terminator::Goto(after));

        self.builder.switch_to(after);
        Ok(())
    }

    /// `for <name> in <iterable>`, through the iteration protocol
    /// `for <name> in <array>`, as a counting loop over the buffer
    fn for_over_array(&mut self, target: &ast::ExprName, node: &ast::StmtFor) -> Lowered<()> {
        let (array, array_ty) = self.expression(&node.iter)?;
        let RType::Array(element) = &array_ty else {
            return Err(Decline::new("a for over an array needs an array"));
        };
        let element = (**element).clone();
        let width = RType::fixed(by_ir::rtype::IntWidth::I64);

        // the array is read again every trip, so it lives in a register of its own
        let held = self
            .builder
            .local(format!("$array{}", self.contexts), array_ty.clone());
        self.contexts += 1;
        self.builder.assign(held, array);
        let length = self
            .builder
            .local(format!("$len{}", self.contexts), width.clone());
        self.builder.push(Op::ArrayLen {
            dest: length,
            array: Value::Register(held),
        });
        let counter = self
            .builder
            .local(format!("$i{}", self.contexts), width.clone());
        self.builder.assign(counter, Value::Fixed(0));

        let item_place = self.binding(target.id.as_str(), &element);
        let header = self.builder.new_block();
        let body = self.builder.new_block();
        // `continue` steps the counter and goes round again — jumping straight to
        // the header would skip the step, and the loop would never end
        let step = self.builder.new_block();
        let natural_exit = self.builder.new_block();
        let after = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let more = self.builder.temp(RType::BIT);
        self.builder.push(Op::IntCompare {
            dest: more,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(length),
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: natural_exit,
        });

        self.builder.switch_to(body);
        let item = self.builder.temp(element.clone());
        self.builder.push(Op::ArrayRead {
            dest: item,
            array: Value::Register(held),
            index: Value::Register(counter),
        });
        self.write_place(&item_place, Value::Register(item), &element)?;
        self.loops.push((step, after, self.cleanups.len()));
        let result = self.block(&node.body);
        self.loops.pop();
        result?;
        if !self.builder.is_sealed(self.builder.current_block()) {
            self.builder.terminate(Terminator::Goto(step));
        }

        self.builder.switch_to(step);
        let stepped = self.builder.temp(width);
        self.builder.push(Op::IntBinary {
            dest: stepped,
            op: BinOp::Add,
            lhs: Value::Register(counter),
            rhs: Value::Fixed(1),
        });
        self.builder.assign(counter, Value::Register(stepped));
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(natural_exit);
        self.block(&node.orelse)?;
        self.builder.terminate(Terminator::Goto(after));

        self.builder.switch_to(after);
        Ok(())
    }

    /// `async for x in it:` — the asynchronous iteration protocol
    ///
    /// `__aiter__` hands back the iterator without awaiting, and each step is
    /// `await it.__anext__()`. the loop ends when that raises
    /// `StopAsyncIteration`, which is why the step runs under an error target:
    /// unlike `__next__`, there is no sentinel to test — the end *is* an exception,
    /// and it surfaces after a suspension rather than before one
    fn async_for(&mut self, node: &ast::StmtFor) -> Lowered<()> {
        if self.generator.is_none() {
            return Err(Decline::new("`async for` outside an async function"));
        }
        let (iterable, iterable_ty) = self.expression(&node.iter)?;
        let iterable = self.widen_to_object(iterable, &iterable_ty);
        let iterator = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::AsyncIter {
            dest: iterator,
            src: iterable,
            next: false,
        });
        // the iterator has to outlive every suspension the awaits make, and a
        // register does not come back from one
        let parked = self.park_iterator(iterator)?;

        let header = self.builder.new_block();
        let stepping = self.builder.new_block();
        let body = self.builder.new_block();
        let stopped = self.builder.new_block();
        let natural_exit = self.builder.new_block();
        let after = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let (live, _) = self.read_place(&parked)?;
        let awaitable = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::AsyncIter {
            dest: awaitable,
            src: live,
            next: true,
        });
        // the step gets a block of its own, because a block records its error
        // target when it is *sealed* — leaving it in the header would seal that one
        // after the target had been restored
        self.builder.terminate(Terminator::Goto(stepping));
        self.builder.switch_to(stepping);
        let previous = self.builder.set_error_target(Some(stopped));
        let stepped = self.delegate_value(Value::Register(awaitable), true);
        self.builder.set_error_target(previous);
        let (stepped, stepped_ty) = stepped?;
        let stepped = self.widen_to_object(stepped, &stepped_ty);

        // the target binds what the await produced, checked against the type the
        // checker gave it — the same narrowing the synchronous loop does
        match node.target.as_ref() {
            Expr::Name(name) => {
                let element_ty = self.peek_type(node.target.as_ref())?;
                let value = self.coerce(stepped, &RType::OBJECT, &element_ty)?;
                let place = self.binding(name.id.as_str(), &element_ty);
                self.write_place(&place, value, &element_ty)?;
            }
            target => self.assign_to(target, stepped, &RType::OBJECT)?,
        }
        self.builder.terminate(Terminator::Goto(body));

        self.builder.switch_to(body);
        self.loops.push((header, after, self.cleanups.len()));
        let result = self.block(&node.body);
        self.loops.pop();
        result?;
        self.builder.terminate(Terminator::Goto(header));

        // the end of the iteration is an exception, and only `StopAsyncIteration`
        // is one: anything else the step raised goes on out
        self.builder.switch_to(stopped);
        let exception = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::FetchException { dest: exception });
        let class = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::LoadGlobal {
            dest: class,
            name: "StopAsyncIteration".to_string(),
        });
        let matched = self.builder.temp(RType::BIT);
        self.builder.push(Op::ExceptionMatches {
            dest: matched,
            value: Value::Register(exception),
            class: Value::Register(class),
        });
        let propagate = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(matched),
            then_block: natural_exit,
            else_block: propagate,
        });

        self.builder.switch_to(propagate);
        self.builder.push(Op::Reraise {
            value: Value::Register(exception),
        });
        self.builder.terminate(Terminator::Unreachable);

        self.builder.switch_to(natural_exit);
        let orelse = self.block(&node.orelse);
        orelse?;
        self.builder.terminate(Terminator::Goto(after));
        self.builder.switch_to(after);
        Ok(())
    }

    fn for_over_iterable(&mut self, node: &ast::StmtFor) -> Lowered<()> {
        let (iterable, iterable_ty) = self.expression(&node.iter)?;
        self.for_over_value(node, iterable, &iterable_ty)
    }

    /// the protocol path over an iterable that is already lowered
    ///
    /// the counting loop evaluates `range`'s bounds before it can tell whether it
    /// applies, so its fallback has an iterable in hand rather than an expression
    fn for_over_value(
        &mut self,
        node: &ast::StmtFor,
        iterable: Value,
        iterable_ty: &RType,
    ) -> Lowered<()> {
        let target = node.target.as_ref();
        let boxed = self.widen_to_object(iterable, iterable_ty);
        let iterator = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::GetIter {
            dest: iterator,
            src: boxed,
        });
        // in a generator the iterator has to outlive the frame: a `yield` in the body
        // returns, and a register does not come back. it has no source name, so the
        // state object reserves one field per loop
        let parked = self.park_iterator(iterator)?;

        // the checker knows the element type; the protocol hands back an object,
        // so narrowing to that type is a *checked* unbox. this is the
        // `iterations` soundness position. a target *list* takes the object as it
        // comes and unpacks it, which is where its own element types come from
        let element_ty = match target {
            Expr::Name(_) => self.peek_type(target)?,
            _ => RType::OBJECT,
        };
        // the loop variable is only *assigned* each iteration, never incremented in
        // place — so it can be a cell as well as a register
        let item_place = match target {
            Expr::Name(name) => Some(self.binding(name.id.as_str(), &element_ty)),
            _ => None,
        };
        let item = self.builder.temp(element_ty.clone());
        let raw = self.builder.temp(RType::OBJECT);

        let header = self.builder.new_block();
        let body = self.builder.new_block();
        let natural_exit = self.builder.new_block();
        let after = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        // read the iterator back from wherever it was parked, every trip
        let (live_iterator, _) = self.read_place(&parked)?;
        self.builder.push(Op::IterNext {
            dest: raw,
            iter: live_iterator,
        });
        let exhausted = self.builder.temp(RType::BIT);
        self.builder.push(Op::IsNull {
            dest: exhausted,
            src: Value::Register(raw),
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(exhausted),
            then_block: natural_exit,
            else_block: body,
        });

        self.builder.switch_to(body);
        // the null test has run, so the item is a real object by here. narrowing
        // to the element type is a *checked* unbox even when the target is itself
        // boxed — a `str` element still has to be proven a str
        match &item_place {
            Some(place) => {
                if self.narrowable_here(&element_ty) {
                    self.builder.push(Op::Unbox {
                        dest: item,
                        src: Value::Register(raw),
                        to: element_ty.clone(),
                    });
                } else {
                    self.builder.assign(item, Value::Register(raw));
                }
                let place = place.clone();
                self.write_place(&place, Value::Register(item), &element_ty)?;
            }
            None => self.assign_to(target, Value::Register(raw), &RType::OBJECT)?,
        }
        self.loops.push((header, after, self.cleanups.len()));
        let result = self.block(&node.body);
        self.loops.pop();
        result?;
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(natural_exit);
        self.block(&node.orelse)?;
        self.builder.terminate(Terminator::Goto(after));

        self.builder.switch_to(after);
        Ok(())
    }

    /// where `name` lives, if this frame has it at all
    ///
    /// a register wins over a field: a generator's *parameters* are registers even
    /// where its locals are fields, and a closure's own parameters shadow a capture
    fn place(&self, name: &str) -> Option<Place> {
        // asked first, and it has to be: a `global` declaration says this name is not
        // this frame's to bind, so nothing else may answer for it
        if self.globals.contains(name) {
            return Some(Place::Global {
                name: name.to_string(),
            });
        }
        if let Some(&id) = self.locals.get(name) {
            return Some(Place::Register(id));
        }
        let captured = self.captures.as_ref()?;
        if captured.cells.contains(name) {
            return Some(Place::Cell {
                receiver: captured.receiver,
                class: captured.class.clone(),
                name: name.to_string(),
                free: captured.free,
            });
        }
        if !captured.names.contains(name) {
            // not ours: follow `$outer` until an environment holds it
            let mut path = vec![captured.class.clone()];
            while let Some(fields) = self.layouts.get(path.last()?) {
                if let Some(held) = fields.iter().find(|field| field.name == name)
                    && path.len() > 1
                {
                    return Some(Place::Chained {
                        path,
                        name: name.to_string(),
                        ty: held.ty.clone(),
                    });
                }
                let outer = fields
                    .iter()
                    .find(|field| field.name == closures::OUTER_FIELD)?;
                match &outer.ty {
                    RType::Instance { class, .. } => path.push(class.clone()),
                    _ => return None,
                }
            }
            return None;
        }
        let held = self
            .layouts
            .get(&captured.class)?
            .iter()
            .find(|field| field.name == name)?;
        Some(Place::Field {
            receiver: captured.receiver,
            class: captured.class.clone(),
            name: name.to_string(),
            ty: held.ty.clone(),
        })
    }

    /// walk `$outer` from this frame's receiver to the environment `path` ends at
    fn enclosing_environment(&mut self, path: &[String]) -> RegisterId {
        let mut receiver = RegisterId(0);
        for step in path.windows(2) {
            let [via, outer] = step else { continue };
            let dest = self.builder.temp(RType::Instance {
                class: outer.clone(),
                exact: false,
            });
            self.builder.push(Op::GetField {
                dest,
                receiver: Value::Register(receiver),
                class: via.clone(),
                field: closures::OUTER_FIELD.to_string(),
            });
            receiver = dest;
        }
        receiver
    }

    /// read a place, emitting a field load where it is a field
    fn read_place(&mut self, place: &Place) -> Lowered<(Value, RType)> {
        match place {
            Place::Register(id) => {
                let ty = self.register_type(*id)?;
                Ok((Value::Register(*id), ty))
            }
            // resolved out of the namespace exactly as an undeclared name is, which is
            // what keeps a read after a write in the same frame seeing the write
            Place::Global { name } => {
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::LoadGlobal {
                    dest,
                    name: name.clone(),
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
            Place::Field {
                receiver,
                class,
                name,
                ty,
            } => {
                let dest = self.builder.temp(ty.clone());
                self.builder.push(Op::GetField {
                    dest,
                    receiver: Value::Register(*receiver),
                    class: class.clone(),
                    field: name.clone(),
                });
                Ok((Value::Register(dest), ty.clone()))
            }
            Place::Cell {
                receiver,
                class,
                name,
                free,
            } => {
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::GetCell {
                    dest,
                    receiver: Value::Register(*receiver),
                    class: class.clone(),
                    field: name.clone(),
                    free: *free,
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
            Place::Chained { path, name, ty } => {
                let (path, name, ty) = (path.clone(), name.clone(), ty.clone());
                let outer = path.last().cloned().unwrap_or_default();
                let receiver = self.enclosing_environment(&path);
                let dest = self.builder.temp(ty.clone());
                // an `object` field up the chain may be a cell, and the checked read is
                // sound either way — a field that is never unset simply never fails it
                if ty == RType::OBJECT {
                    self.builder.push(Op::GetCell {
                        dest,
                        receiver: Value::Register(receiver),
                        class: outer,
                        field: name,
                        // a chained read is always from a frame that closes over it
                        free: true,
                    });
                } else {
                    self.builder.push(Op::GetField {
                        dest,
                        receiver: Value::Register(receiver),
                        class: outer,
                        field: name,
                    });
                }
                Ok((Value::Register(dest), ty))
            }
        }
    }

    /// write `value` into a place, coercing to the place's representation
    fn write_place(&mut self, place: &Place, value: Value, ty: &RType) -> Lowered<()> {
        match place {
            Place::Register(id) => self.store(*id, value, ty),
            // the namespace holds objects, so an unboxed value is boxed on the way in
            Place::Global { name } => {
                let value = self.widen_to_object(value, ty);
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::StoreGlobal {
                    dest: status,
                    name: name.clone(),
                    value,
                });
                Ok(())
            }
            Place::Field {
                receiver,
                class,
                name,
                ty: field_ty,
            } => {
                let value = self.coerce(value, ty, field_ty)?;
                self.builder.push(Op::SetField {
                    receiver: Value::Register(*receiver),
                    class: class.clone(),
                    field: name.clone(),
                    value,
                });
                Ok(())
            }
            // a cell holds an `object`, always — see [`Place::Cell`]
            Place::Cell {
                receiver,
                class,
                name,
                ..
            } => {
                let value = self.coerce(value, ty, &RType::OBJECT)?;
                self.builder.push(Op::SetField {
                    receiver: Value::Register(*receiver),
                    class: class.clone(),
                    field: name.clone(),
                    value,
                });
                Ok(())
            }
            Place::Chained {
                path,
                name,
                ty: field_ty,
            } => {
                let (path, name, field_ty) = (path.clone(), name.clone(), field_ty.clone());
                let outer = path.last().cloned().unwrap_or_default();
                let value = self.coerce(value, ty, &field_ty)?;
                let receiver = self.enclosing_environment(&path);
                self.builder.push(Op::SetField {
                    receiver: Value::Register(receiver),
                    class: outer,
                    field: name,
                    value,
                });
                Ok(())
            }
        }
    }

    /// bind `value` to one assignment target
    ///
    /// every target form arrives here — a name, an attribute, a subscript, a nested
    /// target list — so a chained assignment, a loop variable and an unpacking are
    /// one piece of code rather than three
    fn assign_to(&mut self, target: &Expr, value: Value, ty: &RType) -> Lowered<()> {
        match target {
            Expr::Name(name) => {
                let place = self.binding(name.id.as_str(), ty);
                self.write_place(&place, value, ty)
            }
            Expr::Attribute(_) | Expr::Subscript(_) => {
                let location = self.location(target)?;
                self.write_location(&location, value, ty)
            }
            Expr::Tuple(tuple) => self.unpack_into(&tuple.elts, value, ty),
            Expr::List(list) => self.unpack_into(&list.elts, value, ty),
            other => Err(Decline::new(format!(
                "{other:?} is not an assignment target the compiler lowers yet"
            ))),
        }
    }

    /// a target with its location parts already evaluated
    ///
    /// an augmented assignment reads and writes *one* location, so `xs[f()] += 1`
    /// has to call `f` once — which means the parts cannot be evaluated twice
    fn location(&mut self, target: &Expr) -> Lowered<Location> {
        match target {
            Expr::Attribute(attribute) => {
                let (receiver, receiver_ty) = self.expression(&attribute.value)?;
                Ok(Location::Attribute {
                    receiver,
                    receiver_ty,
                    name: self.attribute_name(&attribute.attr),
                })
            }
            Expr::Subscript(subscript) => {
                let (container, container_ty) = self.expression(&subscript.value)?;
                // an unboxed array is written at its own offset, with the same
                // bounds check a `list` index does
                if let RType::Array(element) = &container_ty {
                    let element = (**element).clone();
                    let index = self.array_index(&subscript.slice)?;
                    return Ok(Location::Element {
                        array: container,
                        index,
                        element,
                    });
                }
                let container = self.widen_to_object(container, &container_ty);
                let (index, index_ty) = self.expression(&subscript.slice)?;
                // an integer index is handed over as one — see the read side
                let index = match index_ty {
                    RType::INT => index,
                    _ => self.widen_to_object(index, &index_ty),
                };
                Ok(Location::Item { container, index })
            }
            Expr::Name(name) => match self.place(name.id.as_str()) {
                Some(place) => Ok(Location::Place(place)),
                None => Err(Decline::new(format!(
                    "`{}` is used before it is assigned",
                    name.id
                ))),
            },
            other => Err(Decline::new(format!(
                "{other:?} is not an assignment target the compiler lowers yet"
            ))),
        }
    }

    /// read whatever a location holds
    fn read_location(&mut self, location: &Location) -> Lowered<(Value, RType)> {
        match location {
            Location::Place(place) => self.read_place(place),
            Location::Attribute {
                receiver,
                receiver_ty,
                name,
            } => {
                if let RType::Instance { class, .. } = receiver_ty
                    && let Some(fields) = self.layouts.get(class)
                    && let Some(held) = fields.iter().find(|field| field.name == *name)
                {
                    let (class, field_ty) = (class.clone(), held.ty.clone());
                    let dest = self.builder.temp(field_ty.clone());
                    self.builder.push(Op::GetField {
                        dest,
                        receiver: receiver.clone(),
                        class,
                        field: name.clone(),
                    });
                    return Ok((Value::Register(dest), field_ty));
                }
                let receiver = self.widen_to_object(receiver.clone(), receiver_ty);
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::GetAttr {
                    dest,
                    receiver,
                    name: name.clone(),
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
            Location::Element {
                array,
                index,
                element,
            } => {
                let dest = self.builder.temp(element.clone());
                self.builder.push(Op::ArrayGet {
                    dest,
                    array: array.clone(),
                    index: index.clone(),
                });
                Ok((Value::Register(dest), element.clone()))
            }
            Location::Item { container, index } => {
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::GetItem {
                    dest,
                    container: container.clone(),
                    index: index.clone(),
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
        }
    }

    /// write `value` into a location, coercing to whatever it holds
    fn write_location(&mut self, location: &Location, value: Value, ty: &RType) -> Lowered<()> {
        match location {
            Location::Place(place) => self.write_place(place, value, ty),
            Location::Attribute {
                receiver,
                receiver_ty,
                name,
            } => {
                if let RType::Instance { class, .. } = receiver_ty
                    && let Some(fields) = self.layouts.get(class)
                    && let Some(held) = fields.iter().find(|field| field.name == *name)
                {
                    let (class, field_ty) = (class.clone(), held.ty.clone());
                    let value = self.coerce(value, ty, &field_ty)?;
                    self.builder.push(Op::SetField {
                        receiver: receiver.clone(),
                        class,
                        field: name.clone(),
                        value,
                    });
                    return Ok(());
                }
                // the dynamic form is where a write goes when the compiler does not know
                // the receiver's layout. it is the wrong answer when it *does*: an
                // emitted instance is its layout and there is no `__dict__` behind it, so
                // `PyObject_SetAttr` for a name no field holds raises where the
                // interpreted class stored a value. the field passes are meant to have
                // seen every write, and this is the invariant that says so
                if let RType::Instance { class, exact } = receiver_ty
                    && self.layouts.contains_key(class)
                    && !self.holds_attribute(class, *exact, name)
                {
                    return Err(Decline::new(format!(
                        "`{name}` is written on a `{class}`, whose layout has nowhere to keep it"
                    )));
                }
                let receiver = self.widen_to_object(receiver.clone(), receiver_ty);
                let value = self.widen_to_object(value, ty);
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::SetAttr {
                    dest: status,
                    receiver,
                    name: name.clone(),
                    value,
                });
                Ok(())
            }
            Location::Element {
                array,
                index,
                element,
            } => {
                let value = self.coerce(value, ty, element)?;
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::ArraySet {
                    dest: status,
                    array: array.clone(),
                    index: index.clone(),
                    value,
                });
                Ok(())
            }
            Location::Item { container, index } => {
                let value = self.widen_to_object(value, ty);
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::SetItem {
                    dest: status,
                    container: container.clone(),
                    index: index.clone(),
                    value,
                });
                Ok(())
            }
        }
    }

    /// `return a, b` from a body that hands its pair back in registers
    ///
    /// [`tuple_return_type`] has already proved every `return` here is a display of
    /// this arity. it is proved again rather than assumed, because the two walks
    /// disagreeing has to be a decline and not a struct filled from the wrong
    /// expressions
    fn return_tuple_display(&mut self, expr: &Expr, slots: &[RType]) -> Lowered<Value> {
        let Expr::Tuple(display) = expr else {
            return Err(Decline::new(
                "a tuple returned in registers has to be written as a display",
            ));
        };
        if display.elts.len() != slots.len() {
            return Err(Decline::new(
                "a tuple display of a different length from the return it fills",
            ));
        }
        let mut items = Vec::with_capacity(slots.len());
        for (element, slot) in display.elts.iter().zip(slots) {
            let (value, ty) = self.expression(element)?;
            items.push(self.coerce(value, &ty, slot)?);
        }
        let dest = self.builder.temp(RType::Tuple(slots.into()));
        self.builder.push(Op::TupleBuild { dest, items });
        Ok(Value::Register(dest))
    }

    /// bind `value` to a target *list*, the way `a, b = xs` does
    fn unpack_into(&mut self, targets: &[Expr], value: Value, ty: &RType) -> Lowered<()> {
        let mut starred = None;
        for (index, target) in targets.iter().enumerate() {
            if matches!(target, Expr::Starred(_)) {
                if starred.is_some() {
                    return Err(Decline::new(
                        "two starred targets in one assignment is a syntax error",
                    ));
                }
                starred = Some(index);
            }
        }
        // a value already held as a fixed-length tuple has the slots the targets want
        // right there — `whole, part = split(i)` never builds the object at all. a
        // star collects a *list*, which the runtime unpack is what builds, so it takes
        // the general path
        let (unpacked, slots) = match ty {
            RType::Tuple(slots) if slots.len() == targets.len() && starred.is_none() => {
                (value, slots.clone())
            }
            _ => {
                let src = self.widen_to_object(value, ty);
                let slots: Box<[RType]> = vec![RType::OBJECT; targets.len()].into();
                let unpacked = self.builder.temp(RType::Tuple(slots.clone()));
                self.builder.push(Op::Unpack {
                    dest: unpacked,
                    src,
                    starred,
                });
                (Value::Register(unpacked), slots)
            }
        };
        for (index, target) in targets.iter().enumerate() {
            let Some(slot) = slots.get(index) else {
                return Err(Decline::new("an unpack target with no slot to read"));
            };
            let item = self.builder.temp(slot.clone());
            self.builder.push(Op::TupleGet {
                dest: item,
                src: unpacked.clone(),
                index,
            });
            let target = match target {
                Expr::Starred(starred) => starred.value.as_ref(),
                other => other,
            };
            // a name narrows back to its own representation, with a check. anything
            // else stays whatever the slot holds
            let (value, ty) = match target {
                Expr::Name(_) => match self.peek_type(target) {
                    Ok(want) if want != *slot => {
                        (self.coerce(Value::Register(item), slot, &want)?, want)
                    }
                    _ => (Value::Register(item), slot.clone()),
                },
                _ => (Value::Register(item), slot.clone()),
            };
            self.assign_to(target, value, &ty)?;
        }
        Ok(())
    }

    /// an array index, as the fixed-width integer the buffer is addressed by
    fn array_index(&mut self, index: &Expr) -> Lowered<Value> {
        let (value, ty) = self.expression(index)?;
        self.coerce(value, &ty, &RType::INT)
    }

    /// as [`Self::binding`], where the lowering needs a *register* — a loop counter
    /// it increments in place, say. a shared cell cannot be one
    fn binding_register(&mut self, name: &str, ty: &RType) -> Lowered<RegisterId> {
        match self.binding(name, ty) {
            Place::Register(id) => Ok(id),
            _ => Err(Decline::new(format!(
                "`{name}` is a shared closure cell, which cannot be a loop counter yet"
            ))),
        }
    }

    /// the place a name binds to, declaring a register on first assignment
    ///
    /// a name that already has a place keeps it — in particular a shared cell, which
    /// must not be shadowed by a fresh register or the two frames stop agreeing
    fn binding(&mut self, name: &str, ty: &RType) -> Place {
        // no guard on the representation: `write_place` coerces, and `coerce` both
        // widens and *narrows* (with a check), so it is the one place that decides
        // whether a write fits and says why when it does not
        if let Some(place) = self.place(name) {
            return place;
        }
        let id = self.builder.local(name.to_string(), ty.clone());
        self.locals.insert(name.to_string(), id);
        Place::Register(id)
    }

    /// write `value` into `dest`, widening to the destination's representation
    fn store(&mut self, dest: RegisterId, value: Value, ty: &RType) -> Lowered<()> {
        let declared = self.register_type(dest)?;
        let value = self.coerce(value, ty, &declared)?;
        self.builder.assign(dest, value);
        Ok(())
    }

    /// build an instance of a class this module emits, when `name` is one
    ///
    /// the interpreted path resolves the class through the module namespace, boxes
    /// every argument into a call, runs the whole `tp_new`/`tp_init` argument
    /// binding and unboxes the result back to an instance pointer. none of that
    /// says anything a direct allocation and a native `__init__` does not.
    ///
    /// a **decorated** class is the one this does not answer for. the decorator is
    /// applied to the namespace entry, and a construction is written against that name
    /// — so allocating the emitted layout skips the decorator entirely, and a decorator
    /// that returns another class had every construction in the module building the
    /// wrong object. that one has to go out through the namespace and find what is
    /// really there.
    ///
    /// a class writing its own `__new__` is the other. python runs that *before* the
    /// allocation and lets it answer with anything at all — an object it had already, an
    /// instance of some other class, one it filled in itself — and only where the answer
    /// is an instance of the class asked for does `__init__` run. allocating here would
    /// skip all of it: a cache would hand back a second object, and an `__init__` written
    /// to be unreachable would run
    ///
    /// only a plain positional call: a default or a keyword needs the binding the
    /// signature describes, and falling back to the interpreted path for those is
    /// correct — just slower
    fn construct(&mut self, name: &str, node: &ast::ExprCall) -> Lowered<Option<(Value, RType)>> {
        if !self.layouts.contains_key(name)
            || self.decorated.contains(name)
            || self.runs_a_written_new(name)
            || !node.arguments.keywords.is_empty()
        {
            return Ok(None);
        }
        let Some(signature) = self.signatures.get(&qualify(Some(name), "__init__")) else {
            return Ok(None);
        };
        // the receiver is not an argument, and every other parameter has to be
        // supplied for this to be the same binding
        let params: Vec<RType> = signature
            .params
            .iter()
            .skip(1)
            .map(|(_, rtype)| rtype.clone())
            .collect();
        if signature.vararg
            || signature.kwarg
            || signature.kwonly > 0
            || params.len() != node.arguments.args.len()
        {
            return Ok(None);
        }

        let returned = signature.ret.clone();
        let mut args = Vec::with_capacity(params.len() + 1);
        args.push(Value::Int(0));
        for (argument, param) in node.arguments.args.iter().zip(&params) {
            let (value, ty) = self.expression(argument)?;
            args.push(self.coerce(value, &ty, param)?);
        }

        let rtype = RType::Instance {
            class: name.to_string(),
            exact: false,
        };
        let instance = self.builder.temp(rtype.clone());
        // every field starts at its undefined value, which is what `tp_new` leaves
        // behind too — `__init__` is what fills them
        let fields = self.layouts.get(name).map_or(0, Vec::len);
        self.builder.push(Op::NewInstance {
            dest: instance,
            class: name.to_string(),
            fields: vec![None; fields],
        });
        args[0] = Value::Register(instance);
        // the result is `None`, but it is also where failure is reported: without a
        // destination there is nothing for the error check to test, and a
        // constructor that raised would return an instance with the exception still set
        let outcome = self.builder.temp(returned);
        self.builder.push(Op::CallNative {
            dest: Some(outcome),
            owner: Some(name.to_string()),
            callee: "__init__".to_string(),
            args,
        });
        Ok(Some((Value::Register(instance), rtype)))
    }

    /// whether a call to `name` has to go through python rather than straight to
    /// the native entry
    ///
    /// a deferring parameter is a `double` the *boundary* proves, one call at a
    /// time. an argument that is not already a float has to reach that boundary to
    /// be proved — or, when it cannot be, to be handed to the interpreted
    /// definition. calling the native entry directly would skip the test and unbox
    /// the argument, which raises where python does not
    fn defers_call(&self, name: &str, node: &ast::ExprCall) -> bool {
        let env = &self.model.program_environment();
        let Some(signature) = self.signatures.get(name) else {
            return false;
        };
        // a parameter whose default only the interpreted definition holds has to
        // reach it, and a direct call would have nothing to fill the gap with
        let omitted = signature.computed_defaults.iter().any(|index| {
            node.arguments.args.get(*index).is_none()
                && signature.params.get(*index).is_some_and(|(parameter, _)| {
                    !node.arguments.keywords.iter().any(|keyword| {
                        keyword
                            .arg
                            .as_ref()
                            .is_some_and(|arg| arg == parameter.as_str())
                    })
                })
        });
        omitted
            || signature.deferring.iter().any(|index| {
                let supplied = node.arguments.args.get(*index).or_else(|| {
                    let (parameter, _) = signature.params.get(*index)?;
                    node.arguments
                        .keywords
                        .iter()
                        .find(|keyword| {
                            keyword
                                .arg
                                .as_ref()
                                .is_some_and(|arg| arg == parameter.as_str())
                        })
                        .map(|keyword| &keyword.value)
                });
                supplied.is_none_or(|argument| {
                    argument
                        .inferred_type(self.model)
                        .and_then(|ty| map_type(self.db, env, ty).ok())
                        != Some(RType::FLOAT)
                })
            })
    }

    /// convert `value` from `from` to `to`, or decline if there is no free
    /// conversion between the two representations
    fn coerce(&mut self, value: Value, from: &RType, to: &RType) -> Lowered<Value> {
        if from == to {
            return Ok(value);
        }
        // a comparison result is already a valid bool byte
        if matches!(from, RType::Primitive(Primitive::Bit))
            && matches!(to, RType::Primitive(Primitive::Bool))
        {
            return Ok(value);
        }
        // a machine integer is an `int` that has been proven to fit in a register, so
        // it widens to one — and to whatever an `int` widens to, by going through it
        if matches!(from, RType::Primitive(Primitive::Fixed(_))) {
            let boxed = self.builder.temp(RType::INT);
            self.builder.push(Op::Box {
                dest: boxed,
                src: value,
            });
            return self.coerce(Value::Register(boxed), &RType::INT, to);
        }
        // an unboxed array is a local's private buffer. handing one out means
        // building a real `list` from it, which is a *copy* — and a copy is a
        // different list, so a mutation through it would go unseen
        if matches!(from, RType::Array(_)) {
            return Err(Decline::new(
                "an unboxed list cannot leave the function that built it — it would \
                 have to be copied, and a copy is a different list",
            ));
        }
        if *to == RType::OBJECT {
            return Ok(self.widen_to_object(value, from));
        }
        // a subclass's struct begins with its base's, so a pointer to one *is* a
        // pointer to the other: an upcast costs nothing. the other direction goes
        // through the checked unbox below, which tests the type
        if let (
            RType::Instance {
                class: from_class, ..
            },
            RType::Instance {
                class: to_class, ..
            },
        ) = (from, to)
            && self.extends(from_class, to_class)
        {
            return Ok(value);
        }
        // the other direction is a *narrowing*, and a checked one — so it is sound
        // wherever it appears. it comes up because our lowering is coarser than the
        // checker's: `str * int` lands on `object` where ty says `str`
        if *from == RType::OBJECT && self.narrowable_here(to) {
            let dest = self.builder.temp(to.clone());
            self.builder.push(Op::Unbox {
                dest,
                src: value,
                to: to.clone(),
            });
            return Ok(Value::Register(dest));
        }
        Err(Decline::new(format!(
            "a {from} cannot be stored in a {to} place"
        )))
    }

    /// whether this frame has a value for `name`: a register, or a capture
    ///
    /// the two are the same question for every purpose but *how* the value is read,
    /// and the lowering of a plain name already answers that
    fn binds(&self, name: &str) -> bool {
        self.place(name).is_some()
    }

    /// `await manager.__aexit__(...)`, whose answer decides suppression
    fn await_exit(
        &mut self,
        manager: Value,
        exception: Value,
        suppressed: RegisterId,
    ) -> Lowered<()> {
        let awaitable = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::AsyncContext {
            dest: awaitable,
            manager,
            exception: Some(exception),
        });
        let (answer, ty) = self.delegate_value(Value::Register(awaitable), true)?;
        let answer = self.widen_to_object(answer, &ty);
        let truth = self.truthy(answer, &RType::OBJECT);
        self.builder.assign(suppressed, truth);
        Ok(())
    }

    /// `yield from x` and `await x`, which are the same machine
    ///
    /// both drive an inner iterator: every value it yields is yielded on, and its
    /// return value is the expression's own. the only difference is how the iterator
    /// is obtained — `iter(x)` versus `x.__await__()` — because awaiting an ordinary
    /// iterable has to be an error rather than silently working
    /// `await f(...)` of a coroutine of ours that never suspends, which is the call
    ///
    /// the coroutine object such an await would build is one nothing else can reach: it
    /// is made by this very expression, awaited once, and dropped. the one `send` the
    /// await makes runs the body from its entry to its end, because there is no
    /// suspension in between — so the value the await produces is the value the body
    /// returns, and calling the body is the same program without the object.
    ///
    /// only the *syntactic* form. `c = f(i)` puts the coroutine in a name, and what a
    /// name can be made to do — handed to `asyncio`, awaited twice, closed, or never
    /// awaited at all so the `RuntimeWarning` fires — is a wider question than this one
    ///
    /// `None` where the shape does not qualify, and the ordinary delegation stands
    fn direct_await(&mut self, node: &ast::ExprAwait) -> Lowered<Option<(Value, RType)>> {
        let Expr::Call(call) = node.value.as_ref() else {
            return Ok(None);
        };
        let Expr::Name(callee) = call.func.as_ref() else {
            return Ok(None);
        };
        let name = callee.id.as_str();
        // a `*` or a `**` in the arguments means python binds them at runtime, against
        // the coroutine's own entry — which is a different signature from this one's
        if call.arguments.args.iter().any(Expr::is_starred_expr)
            || call.arguments.keywords.iter().any(|kw| kw.arg.is_none())
        {
            return Ok(None);
        }
        // the name has to still be the definition. a local, a parameter or a capture
        // holding a callable is a value and not this module's `def` at all, and a call
        // the boundary would have deferred to the interpreted twin has no twin here
        if !self.directs.contains(name)
            || self.binds(name)
            || self.decorated.contains(name)
            || self.defers_call(name, call)
        {
            return Ok(None);
        }
        let direct = generators::direct_name(name);
        self.native_call(call, &direct).map(Some)
    }

    fn delegate(&mut self, source: &Expr, awaitable: bool) -> Lowered<(Value, RType)> {
        let (value, ty) = self.expression(source)?;
        let boxed = self.widen_to_object(value, &ty);
        self.delegate_value(boxed, awaitable)
    }

    /// as [`Self::delegate`], for a value the caller already has — `async for`
    /// awaits what `__anext__` handed it, which was never an expression
    fn delegate_value(&mut self, boxed: Value, awaitable: bool) -> Lowered<(Value, RType)> {
        if self.generator.is_none() {
            return Err(Decline::new("a delegation outside a generator"));
        }
        let iterator = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::DelegateIter {
            dest: iterator,
            src: boxed,
            awaitable,
        });
        // the inner iterator has to survive every suspension the delegation makes
        let parked = self.park_iterator(iterator)?;

        let header = self.builder.new_block();
        let forward = self.builder.new_block();
        let finished = self.builder.new_block();
        let result = self
            .builder
            .local(format!("$delegated{}", self.delegations), RType::OBJECT);
        self.delegations += 1;
        self.builder.terminate(Terminator::Goto(header));

        // each trip: read the inner iterator back, send in whatever `send` gave us,
        // and either forward a value or take the result
        self.builder.switch_to(header);
        let (inner, _) = self.read_place(&parked)?;
        let sent = self.read_sent()?;
        let outcome = self
            .builder
            .temp(RType::Tuple(Box::from([RType::OBJECT, RType::BIT])));
        self.builder.push(Op::DelegateStep {
            dest: outcome,
            inner,
            sent,
        });
        let step = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::TupleGet {
            dest: step,
            src: Value::Register(outcome),
            index: 0,
        });
        let done = self.builder.temp(RType::BIT);
        self.builder.push(Op::TupleGet {
            dest: done,
            src: Value::Register(outcome),
            index: 1,
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(done),
            then_block: finished,
            else_block: forward,
        });

        self.builder.switch_to(forward);
        self.suspend(Value::Register(step), Suspension::Awaited)?;
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(finished);
        self.builder.assign(result, Value::Register(step));
        Ok((Value::Register(result), RType::OBJECT))
    }

    /// what `send` last passed in
    fn read_sent(&mut self) -> Lowered<Value> {
        let Some(generator) = &self.generator else {
            return Err(Decline::new("`$sent` outside a generator"));
        };
        let class = generator.class.clone();
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::GetField {
            dest,
            receiver: Value::Register(RegisterId(0)),
            class,
            field: generators::SENT_FIELD.to_string(),
        });
        Ok(Value::Register(dest))
    }

    /// suspend with `value`, and continue in a fresh resumption block
    fn suspend(&mut self, value: Value, kind: Suspension) -> Lowered<()> {
        let Some(generator) = &self.generator else {
            return Err(Decline::new("a suspension outside a generator"));
        };
        let class = generator.class.clone();
        self.builder.push(Op::SetField {
            receiver: Value::Register(RegisterId(0)),
            class: class.clone(),
            field: generators::KIND_FIELD.to_string(),
            value: Value::Int(kind as i64),
        });
        let state = i64::try_from(generator.resumptions.len()).unwrap_or(i64::MAX - 1) + 1;
        self.builder.push(Op::SetField {
            receiver: Value::Register(RegisterId(0)),
            class: class.clone(),
            field: generators::STATE_FIELD.to_string(),
            value: Value::Int(state),
        });
        let suspend_at = self.builder.current_block();
        self.builder.terminate(Terminator::Return(value));
        let resume_at = self.builder.new_block();
        self.builder.switch_to(resume_at);
        if let Some(generator) = &mut self.generator {
            generator.resumptions.push(generators::Resumption {
                state,
                suspend: suspend_at,
                resume: resume_at,
            });
        }

        // `throw` and `close` resume *by raising*, and the raise has to happen here —
        // at the suspension — so a `yield` inside `try` enters its own handler
        let thrown = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::GetField {
            dest: thrown,
            receiver: Value::Register(RegisterId(0)),
            class: class.clone(),
            field: generators::THROWN_FIELD.to_string(),
        });
        let quiet = self.builder.temp(RType::BIT);
        self.builder.push(Op::IsNull {
            dest: quiet,
            src: Value::Register(thrown),
        });
        let settled = self.builder.new_block();
        let raising = self.builder.new_block();
        let continuing = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(quiet),
            then_block: continuing,
            else_block: settled,
        });

        // the field starts null and is *emptied* by writing `None` into it, because no
        // operation stores a null — so both spell nothing thrown, and a resumption
        // after a handled `throw` reads the second rather than the first
        self.builder.switch_to(settled);
        let cleared = self.widen_to_object(Value::None, &RType::NONE);
        let handled = self.builder.temp(RType::BIT);
        self.builder.push(Op::Identity {
            dest: handled,
            lhs: Value::Register(thrown),
            rhs: cleared.clone(),
            negated: false,
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(handled),
            then_block: continuing,
            else_block: raising,
        });

        self.builder.switch_to(raising);
        // cleared first: the handler may `yield` again, and a stale exception would
        // raise a second time
        self.builder.push(Op::SetField {
            receiver: Value::Register(RegisterId(0)),
            class,
            field: generators::THROWN_FIELD.to_string(),
            value: cleared,
        });
        // the block's error target is the enclosing handler, which is exactly where a
        // `throw` at a suspension has to land
        self.builder.push(Op::Reraise {
            value: Value::Register(thrown),
        });
        self.builder.terminate(Terminator::Unreachable);

        self.builder.switch_to(continuing);
        Ok(())
    }

    /// move a loop iterator into a state field, in a generator
    ///
    /// returns the place to read it from on each trip round the loop — a register
    /// outside a generator, a field inside one
    fn park_iterator(&mut self, iterator: RegisterId) -> Lowered<Place> {
        let Some(generator) = &self.generator else {
            return Ok(Place::Register(iterator));
        };
        let class = generator.class.clone();
        let index = generator.iterators;
        let field = generators::iterator_field(index);
        if !self
            .layouts
            .get(&class)
            .is_some_and(|fields| fields.iter().any(|held| held.name == field))
        {
            return Err(Decline::new(
                "a generator has more `for` loops than reserved iterator fields",
            ));
        }
        if let Some(generator) = &mut self.generator {
            generator.iterators = index + 1;
        }
        self.builder.push(Op::SetField {
            receiver: Value::Register(RegisterId(0)),
            class: class.clone(),
            field: field.clone(),
            value: Value::Register(iterator),
        });
        Ok(Place::Field {
            receiver: RegisterId(0),
            class,
            name: field,
            ty: RType::OBJECT,
        })
    }

    /// suspend: record where to resume, store the state, and return the value
    ///
    /// the expression's own value is what `send` passed in, read back out of the
    /// state object — which is why `$sent` is a field rather than an argument
    fn yield_expression(&mut self, value: Option<&Expr>) -> Lowered<(Value, RType)> {
        let value = match value {
            // a bare `yield` yields `None` — the *object*, like any other yielded value.
            // the unboxed one is a byte, and what the surface hands a caller back is a
            // `PyObject *`
            None => self.widen_to_object(Value::None, &RType::NONE),
            Some(expr) => {
                let (value, ty) = self.expression(expr)?;
                self.widen_to_object(value, &ty)
            }
        };
        self.suspend(value, Suspension::Yielded)?;
        // the expression's own value is what `send` passed in, or `None` for plain
        // iteration
        let sent = self.read_sent()?;
        Ok((sent, RType::OBJECT))
    }

    /// a lambda: the closure over this frame's environment, by the generated name
    fn lambda(&mut self, node: &ast::ExprLambda) -> Lowered<(Value, RType)> {
        self.refresh_environment()?;
        let Some(environment) = &self.environment else {
            return Err(Decline::new("a lambda is not lowered yet"));
        };
        let Some(method) = environment.lambdas.get(&span(node.range)).cloned() else {
            return Err(Decline::new("a lambda with no generated method"));
        };
        let class = environment.class.clone();
        let register = environment.register;
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::MakeClosure {
            dest,
            class,
            method,
            env: Value::Register(register),
        });
        Ok((Value::Register(dest), RType::OBJECT))
    }

    /// bind a nested function's name to a closure over this frame's environment
    fn nested_def(&mut self, node: &ast::StmtFunctionDef) -> Lowered<()> {
        self.refresh_environment()?;
        let Some(environment) = &self.environment else {
            return Err(Decline::new("a nested function is not lowered yet"));
        };
        let class = environment.class.clone();
        let register = environment.register;

        let dest = self
            .locals
            .get(node.name.as_str())
            .copied()
            .ok_or_else(|| Decline::new("a nested function has no local to bind to"))?;
        let closure = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::MakeClosure {
            dest: closure,
            class,
            method: node.name.to_string(),
            env: Value::Register(register),
        });
        // each decorator wraps what the one inside it produced, so the outermost
        // is applied last — the same order the `def` statement itself applies them
        let mut made = Value::Register(closure);
        for decorator in node.decorator_list.iter().rev() {
            let wrapped = self.builder.temp(RType::OBJECT);
            // the decorator expression is evaluated *here*, where the `def` stands, in
            // this frame — which is what python does and what makes an arbitrary
            // expression safe to take: `@functools.wraps(func)` reads `func` out of
            // this frame's own registers, at the moment the closure is made
            if let Expr::Name(name) = &decorator.expression
                && !self.binds(name.id.as_str())
            {
                self.builder.push(Op::CallPython {
                    dest: wrapped,
                    callee: name.id.to_string(),
                    args: vec![made],
                });
            } else {
                let callee = self.callable(&decorator.expression)?;
                self.builder.push(Op::CallValue {
                    dest: wrapped,
                    callee,
                    args: vec![made],
                });
            }
            made = Value::Register(wrapped);
        }
        let dest_ty = self.register_type(dest)?;
        let value = self.coerce(made, &RType::OBJECT, &dest_ty)?;
        self.builder.assign(dest, value);
        // `ready` means the name holds the closure *this frame made*, which is what
        // licenses calling it at its native entry. a decorator makes that false: the
        // name holds whatever the decorator returned, and the entry point would skip it
        if node.decorator_list.is_empty()
            && let Some(environment) = &mut self.environment
        {
            environment.ready.insert(node.name.to_string());
        }
        Ok(())
    }

    /// allocate a fresh environment for the closure being made here
    ///
    /// a frame whose environment holds a loop binding re-allocates it at each
    /// closure, seeded from the values as they stand *there* — which is what gives
    /// each iteration its own binding. an environment with cells is allocated once
    /// per frame instead, because a cell exists to be shared
    fn refresh_environment(&mut self) -> Lowered<()> {
        let Some(environment) = &self.environment else {
            return Ok(());
        };
        let Some(fields) = environment.per_closure.clone() else {
            return Ok(());
        };
        let (class, register) = (environment.class.clone(), environment.register);
        let outer = environment.outer.unwrap_or(RegisterId(0));
        let mut values = Vec::with_capacity(fields.len());
        for name in &fields {
            if name == closures::OUTER_FIELD {
                values.push(Some(Value::Register(outer)));
                continue;
            }
            values.push(match self.place(name) {
                Some(place) => Some(self.read_place(&place)?.0),
                None => None,
            });
        }
        self.builder.push(Op::NewInstance {
            dest: register,
            class,
            fields: values,
        });
        Ok(())
    }

    /// whether constructing `class` runs a written `__new__`
    ///
    /// inherited as much as written: a subclass with no `__new__` of its own still
    /// constructs through its base's, and python hands that one the subclass
    fn runs_a_written_new(&self, class: &str) -> bool {
        let mut current = Some(class.to_string());
        // bounded by the base count, because a chain that visits a class twice is a
        // cycle — which would otherwise spin here rather than settle
        for _ in 0..=self.bases.len() {
            let Some(name) = current else { return false };
            if self.constructs.contains(&name) {
                return true;
            }
            current = self.bases.get(&name).cloned();
        }
        false
    }

    /// whether `class` is `base`, or reaches it through its bases
    fn extends(&self, class: &str, base: &str) -> bool {
        let mut current = Some(class.to_string());
        while let Some(name) = current {
            if name == base {
                return true;
            }
            current = self.bases.get(&name).cloned();
        }
        false
    }

    /// whether an instance of `class` has somewhere of its own to hold a value that
    /// shadows a method name
    ///
    /// python gives an instance a `__dict__` unless every class contributing to its
    /// layout declared `__slots__`, and an emitted class follows it. only a class with
    /// no base at all is asked here — one standing on another is a mutable heap type and
    /// never reaches the direct call — so the class's own declaration is the whole of
    /// the chain. `instance_dict` in the C emitter is what actually decides, and asks the
    /// question again for a class this cannot see
    fn keeps_instance_dict(&self, class: &str) -> bool {
        !self.slotted.contains(class)
    }

    /// whether an `object` may be narrowed to `rtype` in this module
    ///
    /// a native class is narrowable exactly when its layout was emitted here: the
    /// narrowing is a `PyObject_TypeCheck` and a pointer cast, so a class with no
    /// struct has nothing to cast to
    fn narrowable_here(&self, rtype: &RType) -> bool {
        match rtype {
            RType::Instance { class, .. } => self.layouts.contains_key(class),
            other => narrowable(other),
        }
    }

    /// widen `value` to `object`, emitting a `Box` unless it is one already
    fn widen_to_object(&mut self, value: Value, ty: &RType) -> Value {
        // what the register was *declared* as wins over what the caller believes it
        // holds. the two disagree wherever a narrower static type was inferred for a
        // value the lowering had already widened, and boxing something already boxed is
        // ill-formed — the verifier says so, and the function declines for it
        let held = match &value {
            Value::Register(id) => self.builder.register_type(*id).cloned(),
            other => other.immediate_type(),
        };
        if *ty == RType::OBJECT || held.as_ref() == Some(&RType::OBJECT) {
            return value;
        }
        // a fixed-length tuple held in registers becomes a real one by building it from
        // its slots, which is a *fresh* object — exactly what the display it came from
        // would have built. so this is only reached where the lowering has already
        // proved the value has no identity of its own to lose
        if let RType::Tuple(slots) = held.as_ref().unwrap_or(ty).clone() {
            let mut items = Vec::with_capacity(slots.len());
            for (index, slot) in slots.iter().enumerate() {
                let item = self.builder.temp(slot.clone());
                self.builder.push(Op::TupleGet {
                    dest: item,
                    src: value.clone(),
                    index,
                });
                items.push(self.widen_to_object(Value::Register(item), slot));
            }
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::BuildTuple { dest, items });
            return Value::Register(dest);
        }
        // a machine integer's *object* representation is the tagged `int`, which is one
        // widening further on. going straight to `object` would declare a register the
        // box does not fill
        if matches!(
            held.as_ref().unwrap_or(ty),
            RType::Primitive(Primitive::Fixed(_))
        ) {
            let tagged = self.builder.temp(RType::INT);
            self.builder.push(Op::Box {
                dest: tagged,
                src: value,
            });
            return self.widen_to_object(Value::Register(tagged), &RType::INT);
        }
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::Box { dest, src: value });
        Value::Register(dest)
    }

    fn register_type(&self, id: RegisterId) -> Lowered<RType> {
        self.builder
            .register_type(id)
            .cloned()
            .ok_or_else(|| Decline::new("a register was used before it was declared"))
    }

    /// a value usable as a branch condition. only a `bit` is accepted directly;
    /// a `bool` converts, and anything else would need python truthiness, which
    /// is not lowered yet
    /// a value usable as a branch condition
    ///
    /// every representation has a truthiness: a comparison against zero for the
    /// unboxed numbers, and python's `__bool__` protocol for anything boxed
    fn truthy(&mut self, value: Value, ty: &RType) -> Value {
        match ty {
            RType::Primitive(Primitive::Bit) => value,
            RType::Primitive(Primitive::Bool) => {
                // a bool is already 0 or 1, so `not not x` is the whole conversion
                let flipped = self.builder.temp(RType::BIT);
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::Unary {
                    dest: flipped,
                    op: UnaryOp::Not,
                    operand: value,
                });
                self.builder.push(Op::Unary {
                    dest,
                    op: UnaryOp::Not,
                    operand: Value::Register(flipped),
                });
                Value::Register(dest)
            }
            RType::Primitive(Primitive::Int) => {
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::IntCompare {
                    dest,
                    op: CmpOp::Ne,
                    lhs: value,
                    rhs: Value::Int(0),
                });
                Value::Register(dest)
            }
            RType::Primitive(Primitive::Float) => {
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::FloatCompare {
                    dest,
                    op: CmpOp::Ne,
                    lhs: value,
                    rhs: Value::Float(0.0),
                });
                Value::Register(dest)
            }
            // `None` is always falsy, and nothing else is statically decidable
            RType::Primitive(Primitive::None) => Value::Bit(false),
            other => {
                // anything else goes through python's truthiness protocol
                let boxed = self.widen_to_object(value, other);
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::Truthy { dest, src: boxed });
                Value::Register(dest)
            }
        }
    }

    /// the representation an expression will produce, without lowering it
    fn peek_type(&self, expr: &Expr) -> Lowered<RType> {
        let env = &self.model.program_environment();
        let ty = expr
            .inferred_type(self.model)
            .ok_or_else(|| Decline::new("an expression has no inferred type"))?;
        map_type_with(self.db, env, ty, self.layouts)
    }

    /// the representation that holds every one of `exprs`
    fn unified_type(&self, exprs: &[&Expr]) -> Lowered<RType> {
        let mut found: Option<RType> = None;
        for expr in exprs {
            let ty = self.peek_type(expr)?;
            found = Some(match found {
                Some(existing) if existing != ty => RType::OBJECT,
                _ => ty,
            });
        }
        found.ok_or_else(|| Decline::new("an empty operand list"))
    }

    /// `a and b`, `a or b` — n-ary and short-circuiting, and the result is one of
    /// the operands rather than a bool
    fn bool_op(&mut self, node: &ast::ExprBoolOp) -> Lowered<(Value, RType)> {
        let operands: Vec<&Expr> = node.values.iter().collect();
        let result_ty = self.unified_type(&operands)?;
        let result = self.builder.temp(result_ty.clone());

        let (first, ty) = self.expression(operands[0])?;
        self.store(result, first, &ty)?;

        let join = self.builder.new_block();
        for operand in &operands[1..] {
            let cond = self.truthy(Value::Register(result), &result_ty);
            let next = self.builder.new_block();
            let (then_block, else_block) = match node.op {
                // `and` continues while the accumulated value is truthy
                ast::BoolOp::And => (next, join),
                ast::BoolOp::Or => (join, next),
            };
            self.builder.terminate(Terminator::Branch {
                cond,
                then_block,
                else_block,
            });
            self.builder.switch_to(next);
            let (value, ty) = self.expression(operand)?;
            self.store(result, value, &ty)?;
        }
        self.builder.terminate(Terminator::Goto(join));
        self.builder.switch_to(join);
        Ok((Value::Register(result), result_ty))
    }

    /// `a if c else b`
    fn conditional(&mut self, node: &ast::ExprIf) -> Lowered<(Value, RType)> {
        let result_ty = self.unified_type(&[&node.body, &node.orelse])?;
        let result = self.builder.temp(result_ty.clone());

        let (cond, cond_ty) = self.expression(&node.test)?;
        let cond = self.truthy(cond, &cond_ty);
        let then_block = self.builder.new_block();
        let else_block = self.builder.new_block();
        let join = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond,
            then_block,
            else_block,
        });

        for (block, arm) in [(then_block, &node.body), (else_block, &node.orelse)] {
            self.builder.switch_to(block);
            let (value, ty) = self.expression(arm)?;
            self.store(result, value, &ty)?;
            self.builder.terminate(Terminator::Goto(join));
        }

        self.builder.switch_to(join);
        Ok((Value::Register(result), result_ty))
    }

    /// lower an expression, returning its operand and representation
    fn expression(&mut self, expr: &Expr) -> Lowered<(Value, RType)> {
        match expr {
            Expr::NumberLiteral(node) => match &node.value {
                ast::Number::Int(value) => {
                    let value = value
                        .as_i64()
                        .ok_or_else(|| Decline::new("an integer literal is too large to inline"))?;
                    Ok((Value::Int(value), RType::INT))
                }
                ast::Number::Float(value) => Ok((Value::Float(*value), RType::FLOAT)),
                ast::Number::Complex { .. } => {
                    Err(Decline::new("complex literals are not lowered yet"))
                }
            },
            Expr::BooleanLiteral(node) => Ok((Value::Bool(node.value), RType::BOOL)),
            Expr::NoneLiteral(_) => Ok((Value::None, RType::NONE)),
            // `...` is a singleton the module namespace already has
            // a slice is an ordinary `slice` object, and subscripting with one is
            // the same `GetItem` as any other index
            Expr::Slice(node) => {
                // an absent bound is `None` the *object*, which the call wants
                // boxed like every other argument
                let mut part = |bound: &Option<Box<Expr>>| -> Lowered<Value> {
                    let (value, ty) = match bound {
                        None => (Value::None, RType::NONE),
                        Some(expr) => self.expression(expr)?,
                    };
                    Ok(self.widen_to_object(value, &ty))
                };
                let lower = part(&node.lower)?;
                let upper = part(&node.upper)?;
                let step = part(&node.step)?;
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::CallPython {
                    dest,
                    callee: "slice".to_string(),
                    args: vec![lower, upper, step],
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
            // `x := v` binds and evaluates to the value it bound
            Expr::Named(node) => {
                let (value, ty) = self.expression(&node.value)?;
                let Expr::Name(name) = node.target.as_ref() else {
                    return Err(Decline::new("a walrus binds a plain name"));
                };
                let place = self.binding(name.id.as_str(), &ty);
                self.write_place(&place, value, &ty)?;
                self.read_place(&place)
            }
            Expr::EllipsisLiteral(_) => {
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::LoadGlobal {
                    dest,
                    name: "Ellipsis".to_string(),
                });
                Ok((Value::Register(dest), RType::OBJECT))
            }
            Expr::StringLiteral(node) => {
                Ok((Value::Str(node.value.to_str().to_string()), RType::STR))
            }
            Expr::BytesLiteral(node) => {
                Ok((Value::Bytes(node.value.bytes().collect()), RType::OBJECT))
            }
            Expr::Name(node) => {
                let name = node.id.as_str();
                match self.place(name) {
                    Some(Place::Global { .. }) | None => {
                        // a name this frame does not have is a global, resolved the way
                        // `LOAD_GLOBAL` resolves it — and a name it *declares* `global`
                        // is the same read, which is what makes the declaration mean
                        // anything. the result is an `object`, so the checker's type for
                        // the expression decides any narrowing
                        let dest = self.builder.temp(RType::OBJECT);
                        self.builder.push(Op::LoadGlobal {
                            dest,
                            name: name.to_string(),
                        });
                        self.narrow_call_result(dest, expr)
                    }
                    Some(place) => self.read_place(&place),
                }
            }
            // a `yield` is a field write and a return. the code after it becomes a new
            // resumption point, which the dispatch chain picks up
            Expr::Yield(node) => self.yield_expression(node.value.as_deref()),
            // delegation: drive the inner iterator, forwarding every value it yields
            // and taking its return value as this expression's own
            Expr::YieldFrom(node) => self.delegate(&node.value, false),
            Expr::Await(node) => match self.direct_await(node)? {
                Some(direct) => Ok(direct),
                None => self.delegate(&node.value, true),
            },
            // the closure analysis already made a method for this lambda; the expression
            // is just the binding of it
            Expr::Lambda(node) => self.lambda(node),
            Expr::UnaryOp(node) => self.unary(node),
            Expr::BinOp(node) => self.binary(node),
            Expr::Compare(node) => self.compare(node),
            Expr::Call(node) => self.call(node),
            Expr::BoolOp(node) => self.bool_op(node),
            Expr::Attribute(node) => self.attribute(node),
            Expr::List(node) => self.display(&node.elts, Display::List),
            Expr::Set(node) => self.display(&node.elts, Display::Set),
            Expr::Tuple(node) => self.display(&node.elts, Display::Tuple),
            Expr::Dict(node) => self.dict_display(&node.items),
            Expr::Subscript(node) => {
                let (container, container_ty) = self.expression(&node.value)?;
                // an unboxed array is indexed directly: no boxed index, no protocol,
                // and the element comes back in its own representation
                if let RType::Array(element) = &container_ty {
                    let element = (**element).clone();
                    let index = self.array_index(&node.slice)?;
                    let dest = self.builder.temp(element.clone());
                    // a proven counter is in range by the loop's own guard, so the
                    // read needs no test — the same op a `for` over an array emits
                    let op = if self.proven_in_range(&node.value, &node.slice) {
                        Op::ArrayRead {
                            dest,
                            array: container,
                            index,
                        }
                    } else {
                        Op::ArrayGet {
                            dest,
                            array: container,
                            index,
                        }
                    };
                    self.builder.push(op);
                    return Ok((Value::Register(dest), element));
                }
                let (index, index_ty) = self.expression(&node.slice)?;
                // a character of a `str` is a `str`, so the read writes its own
                // representation rather than an object the frontend then has to
                // check. a slice is a subscript too, and is not this one — the
                // index's type is what tells them apart
                if container_ty == RType::STR && index_ty == RType::INT {
                    let dest = self.builder.temp(RType::STR);
                    self.builder.push(Op::StrGetItem {
                        dest,
                        container,
                        index,
                    });
                    return Ok((Value::Register(dest), RType::STR));
                }
                let container = self.widen_to_object(container, &container_ty);
                // an integer index is handed over as one: boxing it to look up a
                // list element allocates a `PyLongObject` nothing ever sees
                let index = match index_ty {
                    RType::INT => index,
                    _ => self.widen_to_object(index, &index_ty),
                };
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::GetItem {
                    dest,
                    container,
                    index,
                });
                self.narrow_call_result(dest, &Expr::Subscript(node.clone()))
            }
            Expr::FString(node) => self.fstring(node),
            Expr::ListComp(node) => {
                self.comprehension(&node.generators, &Comprehension::List(&node.elt))
            }
            // a generator expression is lazy, and the consumers that matter — `sum`,
            // `any`, `max` — drain it immediately. building the list is the same
            // answer for those, so it is only a decline where the laziness is
            // observable: an infinite one, or a side effect ordered against the
            // consumer. neither is expressible without a `yield`, which a genexp has
            // no way to write
            Expr::Generator(node) => {
                self.comprehension(&node.generators, &Comprehension::List(&node.elt))
            }
            Expr::SetComp(node) => {
                self.comprehension(&node.generators, &Comprehension::Set(&node.elt))
            }
            Expr::DictComp(node) => {
                // a `None` key is basedpython's set-like dict comprehension form
                let Some(key) = &node.key else {
                    return Err(Decline::new(
                        "a dict comprehension with no key is not lowered yet",
                    ));
                };
                self.comprehension(&node.generators, &Comprehension::Dict(key, &node.value))
            }
            Expr::If(node) => self.conditional(node),
            other => Err(Decline::new(format!(
                "`{}` is not lowered yet",
                expression_kind(other)
            ))),
        }
    }

    fn unary(&mut self, node: &ast::ExprUnaryOp) -> Lowered<(Value, RType)> {
        let (operand, ty) = self.expression(&node.operand)?;
        match node.op {
            AstUnaryOp::USub => {
                // anything the direct form does not cover goes through the
                // protocol, which is what the interpreter would do anyway
                if !matches!(
                    ty,
                    RType::Primitive(Primitive::Int | Primitive::Float | Primitive::Object)
                ) {
                    let operand = self.widen_to_object(operand, &ty);
                    let dest = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::Unary {
                        dest,
                        op: UnaryOp::Neg,
                        operand,
                    });
                    return Ok((Value::Register(dest), RType::OBJECT));
                }
                let dest = self.builder.temp(ty.clone());
                self.builder.push(Op::Unary {
                    dest,
                    op: UnaryOp::Neg,
                    operand,
                });
                Ok((Value::Register(dest), ty))
            }
            AstUnaryOp::Not => {
                let operand = self.truthy(operand, &ty);
                let dest = self.builder.temp(RType::BIT);
                self.builder.push(Op::Unary {
                    dest,
                    op: UnaryOp::Not,
                    operand,
                });
                Ok((Value::Register(dest), RType::BIT))
            }
            AstUnaryOp::Invert => {
                if !matches!(ty, RType::Primitive(Primitive::Int | Primitive::Object)) {
                    let operand = self.widen_to_object(operand, &ty);
                    let dest = self.builder.temp(RType::OBJECT);
                    self.builder.push(Op::Unary {
                        dest,
                        op: UnaryOp::Invert,
                        operand,
                    });
                    return Ok((Value::Register(dest), RType::OBJECT));
                }
                let dest = self.builder.temp(ty.clone());
                self.builder.push(Op::Unary {
                    dest,
                    op: UnaryOp::Invert,
                    operand,
                });
                Ok((Value::Register(dest), ty))
            }
            AstUnaryOp::UAdd => Ok((operand, ty)),
            // `~`, and the basedpython postfix operators `?` / `!` / `!!`
            other => Err(Decline::new(format!(
                "unary `{}` is not lowered yet",
                other.as_str()
            ))),
        }
    }

    fn binary(&mut self, node: &ast::ExprBinOp) -> Lowered<(Value, RType)> {
        let (lhs, lhs_ty) = self.expression(&node.left)?;
        let (rhs, rhs_ty) = self.expression(&node.right)?;
        let op = binary_op(node.op)?;
        let mut result_ty = binary_result(op, &lhs_ty, &rhs_ty);
        // a double meeting an object is an `object` result as far as the
        // representations go, but the pair may still be provably a float — and
        // then the object can be tested rather than the double boxed to reach it
        if result_ty == RType::OBJECT
            && matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::TrueDiv)
            && self.float_by_proof(&node.left, &lhs_ty, &node.right, &rhs_ty)
        {
            result_ty = RType::FLOAT;
        }
        let dest = self.builder.temp(result_ty.clone());
        self.emit_binary(dest, op, (lhs, &lhs_ty), (rhs, &rhs_ty), Mutation::Fresh);
        Ok((Value::Register(dest), result_ty))
    }

    /// `mutation` only reaches the object protocol: an unboxed pair has no method to
    /// offer, so `x += 1` on an `int` register *is* `x = x + 1`
    fn emit_binary(
        &mut self,
        dest: RegisterId,
        op: BinOp,
        lhs: (Value, &RType),
        rhs: (Value, &RType),
        mutation: Mutation,
    ) {
        let ((lhs, lhs_ty), (rhs, rhs_ty)) = (lhs, rhs);
        match (lhs_ty, rhs_ty) {
            (RType::Primitive(Primitive::Int), RType::Primitive(Primitive::Int)) => {
                self.builder.push(Op::IntBinary { dest, op, lhs, rhs });
            }
            (RType::Primitive(Primitive::Str), RType::Primitive(Primitive::Str))
                if matches!(op, BinOp::Add) =>
            {
                self.builder.push(Op::StrConcat {
                    dest,
                    lhs,
                    rhs,
                    consumes_lhs: false,
                });
            }
            (RType::Primitive(Primitive::Float), RType::Primitive(Primitive::Float))
                if !matches!(
                    op,
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
                ) =>
            {
                self.builder.push(Op::FloatBinary { dest, op, lhs, rhs });
            }
            // python's numeric tower converts the `int` side of a mixed pair to a
            // float and then operates — `float.__add__` calls the same rounding
            // conversion this emits. so the double operation *is* the operation,
            // not an approximation of one, right down to the `OverflowError` an
            // integer with no float at all raises
            (RType::Primitive(Primitive::Float), RType::Primitive(Primitive::Int))
            | (RType::Primitive(Primitive::Int), RType::Primitive(Primitive::Float))
                if !matches!(
                    op,
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
                ) =>
            {
                let lhs = self.widen_to_float(lhs, lhs_ty);
                let rhs = self.widen_to_float(rhs, rhs_ty);
                self.builder.push(Op::FloatBinary { dest, op, lhs, rhs });
            }
            // a double meeting an object: the *object* is tested rather than the
            // double boxed to reach it. the checker has said the result is a
            // float, so the only question is the object's runtime type — and an
            // exact float is the one whose value the register already holds
            (RType::Primitive(Primitive::Float), RType::Primitive(Primitive::Object))
            | (RType::Primitive(Primitive::Object), RType::Primitive(Primitive::Float))
                if matches!(op, BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::TrueDiv)
                    && self.register_type(dest).ok() == Some(RType::FLOAT) =>
            {
                self.builder
                    .push(Op::FloatObjectBinary { dest, op, lhs, rhs });
            }
            // anything else goes through the abstract object protocol, widening
            // whichever side is not already an object
            _ => {
                let lhs = self.widen_to_object(lhs, lhs_ty);
                let rhs = self.widen_to_object(rhs, rhs_ty);
                self.builder.push(Op::ObjectBinary {
                    dest,
                    op,
                    lhs,
                    rhs,
                    mutation,
                });
            }
        }
    }

    /// whether this pair operates as doubles because one side is *proven* to be one
    ///
    /// python's tower makes `int op float` a float, so a proven double on either
    /// side forces a float result. the checker cannot always say so itself: a
    /// parameter written `float` in a `.py` file keeps the type `int | float`
    /// throughout the body, even where the boundary has established that its
    /// register holds a double — so the representation knows more than the
    /// annotation, and this is the one place that matters
    fn float_by_proof(&self, left: &Expr, lhs_ty: &RType, right: &Expr, rhs_ty: &RType) -> bool {
        let env = &self.model.program_environment();
        if *lhs_ty != RType::FLOAT && *rhs_ty != RType::FLOAT {
            return false;
        }
        let numeric = |expr: &Expr, rtype: &RType| {
            if *rtype == RType::FLOAT {
                return true;
            }
            *rtype == RType::OBJECT
                && expr
                    .inferred_type(self.model)
                    .is_some_and(|ty| mapper::is_promoted_float(self.db, env, ty))
        };
        numeric(left, lhs_ty) && numeric(right, rhs_ty)
    }

    /// the float a mixed numeric pair operates on, converting an `int` operand
    fn widen_to_float(&mut self, value: Value, ty: &RType) -> Value {
        if *ty != RType::INT {
            return value;
        }
        let dest = self.builder.temp(RType::FLOAT);
        self.builder.push(Op::IntToFloat { dest, src: value });
        Value::Register(dest)
    }

    fn compare(&mut self, node: &ast::ExprCompare) -> Lowered<(Value, RType)> {
        if node.ops.len() > 1 {
            return self.chained_compare(node);
        }
        let ([op], [right]) = (node.ops.as_ref(), node.comparators.as_ref()) else {
            return Err(Decline::new("a comparison with no operator"));
        };
        let left_value = self.expression(&node.left)?;
        let right_value = self.expression(right)?;
        let bit =
            self.emit_compare_of(*op, left_value, right_value, Some(&node.left), Some(right))?;
        Ok((bit, RType::BIT))
    }

    /// one comparison, choosing the representation from the operand pair
    fn emit_compare(
        &mut self,
        op: AstCmpOp,
        (lhs, lhs_ty): (Value, RType),
        (rhs, rhs_ty): (Value, RType),
    ) -> Lowered<Value> {
        self.emit_compare_of(op, (lhs, lhs_ty), (rhs, rhs_ty), None, None)
    }

    /// as [`Self::emit_compare`], with the operand expressions when the caller has
    /// them — which is what lets a proven double be recognised
    fn emit_compare_of(
        &mut self,
        op: AstCmpOp,
        (lhs, lhs_ty): (Value, RType),
        (rhs, rhs_ty): (Value, RType),
        left: Option<&Expr>,
        right: Option<&Expr>,
    ) -> Lowered<Value> {
        // containment is the container's own protocol rather than a comparison:
        // `__contains__` where the type has one, and a scan of the iterator
        // otherwise. so it reads the operands in the opposite order to everything
        // else here — `value in container`
        if let AstCmpOp::In | AstCmpOp::NotIn = op {
            let negated = matches!(op, AstCmpOp::NotIn);
            let value = self.widen_to_object(lhs, &lhs_ty);
            let container = self.widen_to_object(rhs, &rhs_ty);
            let dest = self.builder.temp(RType::BIT);
            self.builder.push(Op::Contains {
                dest,
                value,
                container,
                negated,
            });
            return Ok(Value::Register(dest));
        }
        // identity asks nothing of either object, so it needs no representation
        // agreement between the two sides — only that both are objects to compare
        if let AstCmpOp::Is | AstCmpOp::IsNot = op {
            let negated = matches!(op, AstCmpOp::IsNot);
            let lhs = self.widen_to_object(lhs, &lhs_ty);
            let rhs = self.widen_to_object(rhs, &rhs_ty);
            let dest = self.builder.temp(RType::BIT);
            self.builder.push(Op::Identity {
                dest,
                lhs,
                rhs,
                negated,
            });
            return Ok(Value::Register(dest));
        }
        let op = compare_op(op)?;
        let dest = self.builder.temp(RType::BIT);
        // a proven double against an int-or-float object: the object is tested
        // rather than the double boxed to reach it
        if let (Some(left), Some(right)) = (left, right)
            && lhs_ty != rhs_ty
            && self.float_by_proof(left, &lhs_ty, right, &rhs_ty)
        {
            self.builder
                .push(Op::FloatObjectCompare { dest, op, lhs, rhs });
            return Ok(Value::Register(dest));
        }
        match (&lhs_ty, &rhs_ty) {
            (RType::Primitive(Primitive::Int), RType::Primitive(Primitive::Int)) => {
                self.builder.push(Op::IntCompare { dest, op, lhs, rhs });
            }
            (RType::Primitive(Primitive::Float), RType::Primitive(Primitive::Float)) => {
                self.builder.push(Op::FloatCompare { dest, op, lhs, rhs });
            }
            (RType::Primitive(Primitive::Str), RType::Primitive(Primitive::Str)) => {
                self.builder.push(Op::StrCompare { dest, op, lhs, rhs });
            }
            _ => {
                let lhs = self.widen_to_object(lhs, &lhs_ty);
                let rhs = self.widen_to_object(rhs, &rhs_ty);
                self.builder.push(Op::ObjectCompare { dest, op, lhs, rhs });
            }
        }
        Ok(Value::Register(dest))
    }

    /// `receiver.name(args)` through the object protocol
    fn method_call(
        &mut self,
        node: &ast::ExprCall,
        attribute: &ast::ExprAttribute,
    ) -> Lowered<(Value, RType)> {
        let (receiver, receiver_ty) = self.expression(&attribute.value)?;
        let name = self.attribute_name(&attribute.attr);

        // a receiver whose class the compiler emitted is called directly. an
        // emitted class cannot be subclassed — the static type object does not
        // set `Py_TPFLAGS_BASETYPE`, and a base class declines — so no override
        // can exist and there is nothing for a vtable to dispatch on. it is also an
        // immutable type, so `C.m = f` raises rather than rebinding the method
        //
        // what neither of those rules out is a value on the *instance*: a method is a
        // non-data descriptor, so `o.m = f` stored in the instance's own dict wins over
        // the class's entry, and `o.m()` then calls it. an emitted class keeps a dict
        // beside its layout so that `o.extra = 3` works the way the interpreted twin
        // does, and that dict is exactly where such a value goes. so the call asks
        // first, and the protocol call is the arm the answer sends it to
        //
        // the method may be declared on a *base*: the symbol lives there, and the
        // receiver's struct begins with the base's, so the pointer is already valid
        let owner_of = |class: &str| {
            let mut current = Some(class.to_string());
            while let Some(candidate) = current {
                if self
                    .methods
                    .get(&candidate)
                    .is_some_and(|table| table.contains_key(name.as_str()))
                {
                    return Some(candidate);
                }
                current = self.bases.get(&candidate).cloned();
            }
            None
        };
        if let RType::Instance { class, .. } = &receiver_ty
            && !self.mutable.contains(class)
            && let Some(owner) = owner_of(class)
            && let Some(signature) = self
                .methods
                .get(&owner)
                .and_then(|table| table.get(name.as_str()))
            && signature.params.len() == node.arguments.args.len() + 1
            // the shadow answers with whatever was stored, which reaches the same
            // register as the compiled body's answer. a return the protocol call
            // cannot be narrowed to has nowhere to land, and the whole site takes
            // the ordinary protocol call below instead
            && (!self.keeps_instance_dict(class)
                || signature.ret == RType::OBJECT
                || self.narrowable_here(&signature.ret))
        {
            let shadowable = self.keeps_instance_dict(class);
            let class = class.clone();
            let params: Vec<RType> = signature
                .params
                .iter()
                .skip(1)
                .map(|(_, rtype)| rtype.clone())
                .collect();
            let ret = signature.ret.clone();
            // once, before the test, where the ordinary call would have evaluated them
            let mut args = Vec::with_capacity(params.len());
            for (argument, param) in node.arguments.args.iter().zip(&params) {
                let (value, ty) = self.expression(argument)?;
                args.push(self.coerce(value, &ty, param)?);
            }
            let mut direct = Vec::with_capacity(args.len() + 1);
            direct.push(receiver.clone());
            direct.extend(args.iter().cloned());
            if !shadowable {
                let dest = self.builder.temp(ret.clone());
                self.builder.push(Op::CallNative {
                    dest: Some(dest),
                    owner: Some(owner),
                    callee: name,
                    args: direct,
                });
                return Ok((Value::Register(dest), ret));
            }

            // the test takes the receiver as it stands: it reads the instance and stores
            // nothing, so widening it — which is a reference taken and released again —
            // would be the fast path paying for the slow one
            let shadowed = self.builder.temp(RType::BIT);
            self.builder.push(Op::DictShadows {
                dest: shadowed,
                src: receiver.clone(),
                class,
                method: name.clone(),
            });
            let dest = self.builder.temp(ret.clone());
            let stored = self.builder.new_block();
            let compiled = self.builder.new_block();
            let join = self.builder.new_block();
            self.builder.terminate(Terminator::Branch {
                cond: Value::Register(shadowed),
                then_block: stored,
                else_block: compiled,
            });

            self.builder.switch_to(compiled);
            self.builder.push(Op::CallNative {
                dest: Some(dest),
                owner: Some(owner),
                callee: name.clone(),
                args: direct,
            });
            self.builder.terminate(Terminator::Goto(join));

            self.builder.switch_to(stored);
            let object = self.widen_to_object(receiver, &receiver_ty);
            let mut boxed = Vec::with_capacity(args.len());
            for (value, ty) in args.into_iter().zip(&params) {
                boxed.push(self.widen_to_object(value, ty));
            }
            let answer = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::CallMethod {
                dest: answer,
                receiver: object,
                name,
                args: boxed,
            });
            let narrowed = self.coerce(Value::Register(answer), &RType::OBJECT, &ret)?;
            self.builder.assign(dest, narrowed);
            self.builder.terminate(Terminator::Goto(join));

            self.builder.switch_to(join);
            return Ok((Value::Register(dest), ret));
        }

        // an override reached through a base-typed name — `shapes[i].area()`, where the
        // element is declared a `Shape` and may be a `Square`. the call site cannot name
        // one body, so it asks the object protocol, and asking is most of what the call
        // costs: a lookup on the type, then a boxed round trip through the python-facing
        // entry point for what the compiled bodies would have passed in registers
        //
        // so the question is asked here instead, where the emitted classes under the
        // receiver's static class are all known. each gets a test of its own and its body
        // called directly; the protocol call stays as the last arm, and is what a
        // receiver none of them describe still takes — a subclass written in the
        // interpreter, or the method rebound on the class after import
        if let RType::Instance { class, .. } = &receiver_ty
            && node.arguments.keywords.is_empty()
            && let Ok(site) = self.call_result_type(&Expr::Call(node.clone()))
            && let Some(dispatch) =
                self.dispatch_candidates(class, name.as_str(), node.arguments.args.len(), &site)
        {
            return self.dispatched_call(node, &receiver, &receiver_ty, name, &dispatch);
        }

        // `xs.append(v)` on an unboxed buffer pushes the element itself. this is the
        // *statement* form; the comprehension has always built one directly, and
        // without this a list built by appending could never earn a buffer at all
        if let RType::Array(element) = &receiver_ty
            && attribute.attr.as_str() == "append"
            && node.arguments.args.len() == 1
            && node.arguments.keywords.is_empty()
        {
            let element = (**element).clone();
            let (value, value_ty) = self.expression(&node.arguments.args[0])?;
            let value = self.coerce(value, &value_ty, &element)?;
            let status = self.builder.temp(RType::BIT);
            self.builder.push(Op::ArrayPush {
                dest: status,
                array: receiver,
                value,
            });
            // `list.append` answers `None`, and a statement discards it
            return Ok((Value::None, RType::NONE));
        }

        let receiver = self.widen_to_object(receiver, &receiver_ty);
        let mut args = Vec::with_capacity(node.arguments.args.len());
        for argument in &node.arguments.args {
            let (value, ty) = self.expression(argument)?;
            args.push(self.widen_to_object(value, &ty));
        }
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::CallMethod {
            dest,
            receiver,
            name,
            args,
        });
        self.narrow_call_result(dest, &Expr::Call(node.clone()))
    }

    /// the emitted classes a call on a receiver statically typed `class` can reach
    /// directly, and the signature every one of them answers with
    ///
    /// a candidate has to *declare* the method itself. one that only inherits it is left
    /// to the protocol, because what a class's body binds is not knowable from the method
    /// table alone — a `property` and a decorated override are both absent from it for
    /// reasons that have nothing to do with inheritance — and reading past such a class
    /// to a base's entry would call a body its instances do not have
    ///
    /// they also have to agree on everything the call site depends on: what it coerces
    /// its arguments to, and what representation it takes back. one that differs is
    /// dropped on its own rather than sinking the whole site, since a dropped candidate
    /// still resolves through the protocol arm
    fn dispatch_candidates(
        &self,
        class: &str,
        name: &str,
        argc: usize,
        site: &RType,
    ) -> Option<Dispatch> {
        // the direct call is already licensed for these, and takes no test to reach
        if !self.mutable.contains(class) {
            return None;
        }
        let answered = |candidate: &str| -> Option<&Signature> {
            let signature = self.methods.get(candidate)?.get(name)?;
            (signature.params.len() == argc + 1
                && !signature.vararg
                && !signature.kwarg
                && signature.kwonly == 0)
                .then_some(signature)
        };
        let dispatched = answered(class)?.clone();
        // what the site hands back is what the *call* was typed as, and that is not always
        // what one body produces. a receiver whose mro takes the name somewhere else
        // entirely — a base outside this module, listed first — answers with something a
        // compiled body's representation has no room for, and only the site's own type
        // has ever described both. so a body narrower than the site is widened to it, and
        // anything else declines
        if dispatched.ret != *site && *site != RType::OBJECT {
            return None;
        }
        // sorted, because the chain is emitted in this order and two runs of the compiler
        // over one source have to write the same C
        let mut under: Vec<&String> = self
            .layouts
            .keys()
            .filter(|other| other.as_str() != class && self.descends_from(other, class))
            .collect();
        under.sort();
        let mut candidates = vec![class.to_string()];
        for other in under {
            if candidates.len() == DISPATCH_CANDIDATES {
                break;
            }
            if let Some(signature) = answered(other)
                && signature.ret == dispatched.ret
                && signature
                    .params
                    .iter()
                    .skip(1)
                    .map(|(_, ty)| ty)
                    .eq(dispatched.params.iter().skip(1).map(|(_, ty)| ty))
            {
                candidates.push(other.clone());
            }
        }
        Some(Dispatch {
            candidates,
            params: dispatched
                .params
                .iter()
                .skip(1)
                .map(|(_, ty)| ty.clone())
                .collect(),
            produced: dispatched.ret,
            site: site.clone(),
        })
    }

    /// `receiver.name(args)` as a test per candidate, with the protocol call left as the
    /// arm nothing else took
    fn dispatched_call(
        &mut self,
        node: &ast::ExprCall,
        receiver: &Value,
        receiver_ty: &RType,
        name: String,
        dispatch: &Dispatch,
    ) -> Lowered<(Value, RType)> {
        let Dispatch {
            candidates,
            params,
            produced,
            site,
        } = dispatch;
        let produced = produced.clone();
        let ret = site.clone();
        // once, before any test, where the ordinary call would have evaluated them
        let mut args = Vec::with_capacity(params.len());
        for (argument, param) in node.arguments.args.iter().zip(params) {
            let (value, ty) = self.expression(argument)?;
            args.push(self.coerce(value, &ty, param)?);
        }
        let held = match receiver {
            Value::Register(id) => self.builder.register_type(*id).cloned(),
            other => other.immediate_type(),
        };
        let object = self.widen_to_object(receiver.clone(), receiver_ty);
        let dest = self.builder.temp(ret.clone());
        let join = self.builder.new_block();
        for candidate in candidates {
            let hit = self.builder.new_block();
            let next = self.builder.new_block();
            let stands = self.builder.temp(RType::BIT);
            self.builder.push(Op::MethodStands {
                dest: stands,
                src: object.clone(),
                class: candidate.clone(),
                method: name.clone(),
            });
            self.builder.terminate(Terminator::Branch {
                cond: Value::Register(stands),
                then_block: hit,
                else_block: next,
            });
            self.builder.switch_to(hit);
            // the receiver reaches a *subclass*'s body, which is a narrowing the test has
            // just proved and nothing downstream can see it prove. so it is written out
            // as the ordinary checked one — the pointer arithmetic differs between a
            // class that owns its layout and one that appends to a base, and this is what
            // knows which
            let mut direct = Vec::with_capacity(args.len() + 1);
            direct.push(match &held {
                Some(RType::Instance { class, .. })
                    if class == candidate || self.descends_from(class, candidate) =>
                {
                    receiver.clone()
                }
                _ => {
                    let narrowed = self.builder.temp(RType::Instance {
                        class: candidate.clone(),
                        exact: false,
                    });
                    self.builder.push(Op::Unbox {
                        dest: narrowed,
                        src: object.clone(),
                        to: RType::Instance {
                            class: candidate.clone(),
                            exact: false,
                        },
                    });
                    Value::Register(narrowed)
                }
            });
            direct.extend(args.iter().cloned());
            // the body may answer with something narrower than the site was typed as,
            // which is a widening rather than a different value
            let answered = if produced == ret {
                dest
            } else {
                self.builder.temp(produced.clone())
            };
            self.builder.push(Op::CallNative {
                dest: Some(answered),
                owner: Some(candidate.clone()),
                callee: name.clone(),
                args: direct,
            });
            if answered != dest {
                let widened = self.widen_to_object(Value::Register(answered), &produced);
                self.builder.assign(dest, widened);
            }
            self.builder.terminate(Terminator::Goto(join));
            self.builder.switch_to(next);
        }
        let mut boxed = Vec::with_capacity(args.len());
        for (value, ty) in args.iter().zip(params) {
            let value = value.clone();
            boxed.push(self.widen_to_object(value, ty));
        }
        let answer = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::CallMethod {
            dest: answer,
            receiver: object,
            name,
            args: boxed,
        });
        if ret == RType::OBJECT {
            self.builder.assign(dest, Value::Register(answer));
        } else {
            self.builder.push(Op::Unbox {
                dest,
                src: Value::Register(answer),
                to: ret.clone(),
            });
        }
        self.builder.terminate(Terminator::Goto(join));
        self.builder.switch_to(join);
        Ok((Value::Register(dest), ret))
    }

    /// the representation a call site hands on, whatever the callee turns out to be
    ///
    /// the checker's type for the call, where an object can be narrowed to it, and
    /// `object` otherwise
    fn call_result_type(&mut self, call: &Expr) -> Lowered<RType> {
        let declared = self.peek_type(call)?;
        Ok(if self.narrowable_here(&declared) {
            declared
        } else {
            RType::OBJECT
        })
    }

    /// narrow a boxed call result to the representation the checker says it has
    ///
    /// the call proves nothing, so this is a *checked* unbox — the `arguments`
    /// soundness position, arriving for free
    fn narrow_call_result(&mut self, boxed: RegisterId, call: &Expr) -> Lowered<(Value, RType)> {
        let declared = self.call_result_type(call)?;
        if declared != RType::OBJECT {
            let narrowed = self.builder.temp(declared.clone());
            self.builder.push(Op::Unbox {
                dest: narrowed,
                src: Value::Register(boxed),
                to: declared.clone(),
            });
            return Ok((Value::Register(narrowed), declared));
        }
        Ok((Value::Register(boxed), RType::OBJECT))
    }

    /// a call with a `*` or a `**` in its arguments
    ///
    /// the argument list *is* a tuple display and the keywords *are* a dict display,
    /// so both reuse what the display lowering already does
    fn call_unpacked(&mut self, node: &ast::ExprCall) -> Lowered<(Value, RType)> {
        let callee = self.callable(&node.func)?;
        let (args, args_ty) = self.display(&node.arguments.args, Display::Tuple)?;
        let args = self.widen_to_object(args, &args_ty);
        let kwargs = self.keyword_dict(&node.arguments.keywords)?;
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::CallUnpacked {
            dest,
            callee,
            args,
            kwargs,
        });
        self.narrow_call_result(dest, &Expr::Call(node.clone()))
    }

    /// the callable a call names, as a value
    ///
    /// a name this frame does not bind is resolved through the module namespace on
    /// every call, which is where a native function's own wrapper lives
    fn callable(&mut self, func: &Expr) -> Lowered<Value> {
        if let Expr::Name(name) = func
            && !self.binds(name.id.as_str())
        {
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::LoadGlobal {
                dest,
                name: name.id.to_string(),
            });
            return Ok(Value::Register(dest));
        }
        let (value, ty) = self.expression(func)?;
        Ok(self.widen_to_object(value, &ty))
    }

    /// a call's keywords as a dict, with `**` merging another mapping in
    fn keyword_dict(&mut self, keywords: &[ast::Keyword]) -> Lowered<Option<Value>> {
        if keywords.is_empty() {
            return Ok(None);
        }
        let accumulator = self.builder.temp(RType::OBJECT);
        let mut pairs: Vec<Value> = Vec::new();
        let mut started = false;
        for keyword in keywords {
            match &keyword.arg {
                Some(name) => {
                    pairs.push(Value::Str(name.to_string()));
                    let (value, ty) = self.expression(&keyword.value)?;
                    pairs.push(self.widen_to_object(value, &ty));
                }
                None => {
                    let target = if started {
                        self.builder.temp(RType::OBJECT)
                    } else {
                        accumulator
                    };
                    self.builder.push(Op::BuildDict {
                        dest: target,
                        pairs: std::mem::take(&mut pairs),
                    });
                    if started {
                        self.extend(accumulator, Value::Register(target), Display::Dict);
                    }
                    started = true;
                    let (value, ty) = self.expression(&keyword.value)?;
                    let source = self.widen_to_object(value, &ty);
                    self.extend(accumulator, source, Display::Dict);
                }
            }
        }
        if started {
            if !pairs.is_empty() {
                let tail = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::BuildDict { dest: tail, pairs });
                self.extend(accumulator, Value::Register(tail), Display::Dict);
            }
        } else {
            self.builder.push(Op::BuildDict {
                dest: accumulator,
                pairs,
            });
        }
        Ok(Some(Value::Register(accumulator)))
    }

    /// a comprehension, desugared to an empty container plus a loop that fills it
    ///
    /// the accumulator's method is called through the object protocol, which is
    /// what the interpreter does — so the shape is right and only the speed is
    /// left on the table
    fn comprehension(
        &mut self,
        generators: &[ast::Comprehension],
        kind: &Comprehension<'_>,
    ) -> Lowered<(Value, RType)> {
        // the one place all four forms pass through, which is what makes it the one
        // place that has to record python giving each a frame this does not
        self.comprehensions += 1;
        let lowered = self.comprehension_inner(generators, kind);
        self.comprehensions -= 1;
        lowered
    }

    fn comprehension_inner(
        &mut self,
        generators: &[ast::Comprehension],
        kind: &Comprehension<'_>,
    ) -> Lowered<(Value, RType)> {
        // the accumulator starts empty, with the representation its builder
        // produces
        let accumulator_ty = match kind {
            Comprehension::List(_) => RType::LIST,
            Comprehension::Array(_, element) => RType::Array(Box::new(element.clone())),
            Comprehension::Set(_) | Comprehension::Dict(..) => RType::OBJECT,
        };
        let accumulator = self.builder.temp(accumulator_ty.clone());
        match kind {
            Comprehension::List(_) => self.builder.push(Op::BuildList {
                dest: accumulator,
                items: Vec::new(),
            }),
            Comprehension::Set(_) => self.builder.push(Op::BuildSet {
                dest: accumulator,
                items: Vec::new(),
            }),
            Comprehension::Dict(..) => self.builder.push(Op::BuildDict {
                dest: accumulator,
                pairs: Vec::new(),
            }),
            Comprehension::Array(..) => self.builder.push(Op::ArrayNew {
                dest: accumulator,
                items: Vec::new(),
            }),
        }

        self.comprehension_loop(generators, kind, accumulator, &accumulator_ty)?;
        Ok((Value::Register(accumulator), accumulator_ty))
    }

    /// one `for` of a comprehension, and everything nested inside it
    ///
    /// each level gets its own header, so an `if` guard skips to the *next value of
    /// its own loop* rather than to the outermost one
    /// one generator of an `async for` comprehension clause
    ///
    /// the same machine [`Self::async_for`] is, driving the rest of the
    /// comprehension where the statement form drives a body — including the
    /// parked iterator, because every step of this loop suspends
    fn comprehension_loop_async(
        &mut self,
        generator: &ast::Comprehension,
        rest: &[ast::Comprehension],
        kind: &Comprehension<'_>,
        accumulator: RegisterId,
        accumulator_ty: &RType,
    ) -> Lowered<()> {
        if self.generator.is_none() {
            return Err(Decline::new(
                "an async comprehension outside an async function",
            ));
        }
        // the accumulator has to survive every suspension, which means a field —
        // and a field hands back an `object`, so the register is narrowed back from
        // one on the way out. an unboxed array has no object form at all
        if matches!(accumulator_ty, RType::Array(_)) {
            return Err(Decline::new(
                "an async comprehension cannot build an unboxed array — the accumulator has to \
                 live in a field, and a buffer has no object form",
            ));
        }
        let target = &generator.target;
        let (iterable, iterable_ty) = self.expression(&generator.iter)?;
        let iterable = self.widen_to_object(iterable, &iterable_ty);
        let iterator = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::AsyncIter {
            dest: iterator,
            src: iterable,
            next: false,
        });
        let parked = self.park_iterator(iterator)?;
        // the accumulator is a register, and every step of this loop suspends — so
        // it goes into a field too. `append` mutates in place, so the field keeps
        // pointing at the same object and only the *register* has to be re-read
        let parked_accumulator = self.park_iterator(accumulator)?;

        let header = self.builder.new_block();
        let stepping = self.builder.new_block();
        let stopped = self.builder.new_block();
        let exit = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let (live, _) = self.read_place(&parked)?;
        let awaitable = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::AsyncIter {
            dest: awaitable,
            src: live,
            next: true,
        });
        self.builder.terminate(Terminator::Goto(stepping));
        self.builder.switch_to(stepping);
        let previous = self.builder.set_error_target(Some(stopped));
        let stepped = self.delegate_value(Value::Register(awaitable), true);
        self.builder.set_error_target(previous);
        let (stepped, stepped_ty) = stepped?;
        let stepped = self.widen_to_object(stepped, &stepped_ty);
        self.restore_accumulator(&parked_accumulator, accumulator, accumulator_ty)?;

        match target {
            Expr::Name(name) => {
                let element_ty = self.peek_type(target)?;
                let value = self.coerce(stepped, &RType::OBJECT, &element_ty)?;
                let place = self.binding(name.id.as_str(), &element_ty);
                self.write_place(&place, value, &element_ty)?;
            }
            target => self.assign_to(target, stepped, &RType::OBJECT)?,
        }

        // each `if` guard skips straight back to this loop's header
        for condition in &generator.ifs {
            let (cond, cond_ty) = self.expression(condition)?;
            let cond = self.truthy(cond, &cond_ty);
            let kept = self.builder.new_block();
            self.builder.terminate(Terminator::Branch {
                cond,
                then_block: kept,
                else_block: header,
            });
            self.builder.switch_to(kept);
        }

        self.comprehension_loop(rest, kind, accumulator, accumulator_ty)?;
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(stopped);
        let exception = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::FetchException { dest: exception });
        let class = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::LoadGlobal {
            dest: class,
            name: "StopAsyncIteration".to_string(),
        });
        let matched = self.builder.temp(RType::BIT);
        self.builder.push(Op::ExceptionMatches {
            dest: matched,
            value: Value::Register(exception),
            class: Value::Register(class),
        });
        let propagate = self.builder.new_block();
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(matched),
            then_block: exit,
            else_block: propagate,
        });

        self.builder.switch_to(propagate);
        self.builder.push(Op::Reraise {
            value: Value::Register(exception),
        });
        self.builder.terminate(Terminator::Unreachable);

        self.builder.switch_to(exit);
        self.restore_accumulator(&parked_accumulator, accumulator, accumulator_ty)?;
        Ok(())
    }

    /// read a parked accumulator back into its register, which a suspension left
    /// holding nothing
    fn restore_accumulator(
        &mut self,
        parked: &Place,
        accumulator: RegisterId,
        accumulator_ty: &RType,
    ) -> Lowered<()> {
        let (live, live_ty) = self.read_place(parked)?;
        let value = self.coerce(live, &live_ty, accumulator_ty)?;
        self.builder.assign(accumulator, value);
        Ok(())
    }

    fn comprehension_loop(
        &mut self,
        generators: &[ast::Comprehension],
        kind: &Comprehension<'_>,
        accumulator: RegisterId,
        accumulator_ty: &RType,
    ) -> Lowered<()> {
        let Some((generator, rest)) = generators.split_first() else {
            return self.comprehension_emit(kind, accumulator, accumulator_ty);
        };
        if generator.is_async {
            return self.comprehension_loop_async(
                generator,
                rest,
                kind,
                accumulator,
                accumulator_ty,
            );
        }
        let target = &generator.target;

        // `for i in range(n)` gets the same counting loop the statement form gets.
        // driving the iteration protocol here cost a `range` object, an iterator, a
        // `next` and an unbox *per element* — for a loop whose bounds are right there
        if let Expr::Name(name) = target
            && let Some(count) = self.counting_range(&generator.iter)?
        {
            return self.comprehension_counted(
                name,
                count,
                generator,
                rest,
                kind,
                accumulator,
                accumulator_ty,
            );
        }

        let (iterable, iterable_ty) = self.expression(&generator.iter)?;
        let boxed = self.widen_to_object(iterable, &iterable_ty);
        let iterator = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::GetIter {
            dest: iterator,
            src: boxed,
        });

        let element_ty = match target {
            Expr::Name(_) => self.peek_type(target)?,
            _ => RType::OBJECT,
        };
        // the loop variable is only *assigned* each iteration, never incremented in
        // place — so it can be a cell as well as a register
        let item_place = match target {
            Expr::Name(name) => Some(self.binding(name.id.as_str(), &element_ty)),
            _ => None,
        };
        let item = self.builder.temp(element_ty.clone());
        let raw = self.builder.temp(RType::OBJECT);

        let header = self.builder.new_block();
        let body = self.builder.new_block();
        let exit = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        self.builder.push(Op::IterNext {
            dest: raw,
            iter: Value::Register(iterator),
        });
        let exhausted = self.builder.temp(RType::BIT);
        self.builder.push(Op::IsNull {
            dest: exhausted,
            src: Value::Register(raw),
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(exhausted),
            then_block: exit,
            else_block: body,
        });

        self.builder.switch_to(body);
        match &item_place {
            Some(place) => {
                if self.narrowable_here(&element_ty) {
                    self.builder.push(Op::Unbox {
                        dest: item,
                        src: Value::Register(raw),
                        to: element_ty.clone(),
                    });
                } else {
                    self.builder.assign(item, Value::Register(raw));
                }
                let place = place.clone();
                self.write_place(&place, Value::Register(item), &element_ty)?;
            }
            None => self.assign_to(target, Value::Register(raw), &RType::OBJECT)?,
        }

        // each `if` guard skips straight back to this loop's header
        for condition in &generator.ifs {
            let (cond, cond_ty) = self.expression(condition)?;
            let cond = self.truthy(cond, &cond_ty);
            let kept = self.builder.new_block();
            self.builder.terminate(Terminator::Branch {
                cond,
                then_block: kept,
                else_block: header,
            });
            self.builder.switch_to(kept);
        }

        self.comprehension_loop(rest, kind, accumulator, accumulator_ty)?;
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(exit);
        Ok(())
    }

    /// the `(start, stop)` of a `range` call this frame can count over, when the
    /// expression is one — a computed step is left to the protocol
    fn counting_range(&mut self, iter: &Expr) -> Lowered<Option<(Value, Value)>> {
        let Expr::Call(call) = iter else {
            return Ok(None);
        };
        let Expr::Name(callee) = call.func.as_ref() else {
            return Ok(None);
        };
        if callee.id.as_str() != "range"
            || self.binds("range")
            || self.native_callees.contains("range")
            || !call.arguments.keywords.is_empty()
        {
            return Ok(None);
        }
        let (start, stop) = match call.arguments.args.as_ref() {
            [stop] => (None, stop),
            [start, stop] => (Some(start), stop),
            _ => return Ok(None),
        };
        let stop = {
            let (value, ty) = self.expression(stop)?;
            self.coerce(value, &ty, &RType::INT)?
        };
        let start = match start {
            None => Value::Int(0),
            Some(start) => {
                let (value, ty) = self.expression(start)?;
                self.coerce(value, &ty, &RType::INT)?
            }
        };
        Ok(Some((start, stop)))
    }

    /// a comprehension clause over `range`, as a counting loop
    #[expect(
        clippy::too_many_arguments,
        reason = "one clause of a comprehension needs all of its context"
    )]
    fn comprehension_counted(
        &mut self,
        name: &ast::ExprName,
        (start, stop): (Value, Value),
        generator: &ast::Comprehension,
        rest: &[ast::Comprehension],
        kind: &Comprehension<'_>,
        accumulator: RegisterId,
        accumulator_ty: &RType,
    ) -> Lowered<()> {
        let counter = self.binding_register(name.id.as_str(), &RType::INT)?;
        self.store(counter, start, &RType::INT)?;
        let bound = self
            .builder
            .local(format!("$stop{}", self.contexts), RType::INT);
        self.contexts += 1;
        self.store(bound, stop, &RType::INT)?;

        let header = self.builder.new_block();
        let body = self.builder.new_block();
        let step = self.builder.new_block();
        let exit = self.builder.new_block();
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(header);
        let more = self.builder.temp(RType::BIT);
        self.builder.push(Op::IntCompare {
            dest: more,
            op: by_ir::ops::CmpOp::Lt,
            lhs: Value::Register(counter),
            rhs: Value::Register(bound),
        });
        self.builder.terminate(Terminator::Branch {
            cond: Value::Register(more),
            then_block: body,
            else_block: exit,
        });

        self.builder.switch_to(body);
        let mut inner = body;
        for condition in &generator.ifs {
            let (cond, cond_ty) = self.expression(condition)?;
            let cond = self.truthy(cond, &cond_ty);
            let kept = self.builder.new_block();
            self.builder.terminate(Terminator::Branch {
                cond,
                then_block: kept,
                else_block: step,
            });
            self.builder.switch_to(kept);
            inner = kept;
        }
        let _ = inner;
        self.comprehension_loop(rest, kind, accumulator, accumulator_ty)?;
        self.builder.terminate(Terminator::Goto(step));

        self.builder.switch_to(step);
        let stepped = self.builder.temp(RType::INT);
        self.builder.push(Op::IntBinary {
            dest: stepped,
            op: BinOp::Add,
            lhs: Value::Register(counter),
            rhs: Value::Int(1),
        });
        self.builder.assign(counter, Value::Register(stepped));
        self.builder.terminate(Terminator::Goto(header));

        self.builder.switch_to(exit);
        Ok(())
    }

    /// the innermost body of a comprehension: one element into the accumulator
    fn comprehension_emit(
        &mut self,
        kind: &Comprehension<'_>,
        accumulator: RegisterId,
        accumulator_ty: &RType,
    ) -> Lowered<()> {
        match kind {
            Comprehension::Array(element, wanted) => {
                let (value, ty) = self.expression(element)?;
                let value = self.coerce(value, &ty, wanted)?;
                let status = self.builder.temp(RType::BIT);
                self.builder.push(Op::ArrayPush {
                    dest: status,
                    array: Value::Register(accumulator),
                    value,
                });
            }
            Comprehension::List(element) | Comprehension::Set(element) => {
                let (value, ty) = self.expression(element)?;
                let value = self.widen_to_object(value, &ty);
                let receiver = self.widen_to_object(Value::Register(accumulator), accumulator_ty);
                let discard = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::CallMethod {
                    dest: discard,
                    receiver,
                    name: match kind {
                        Comprehension::Set(_) => "add".to_string(),
                        _ => "append".to_string(),
                    },
                    args: vec![value],
                });
            }
            Comprehension::Dict(key, element) => {
                let (key, key_ty) = self.expression(key)?;
                let key = self.widen_to_object(key, &key_ty);
                let (value, value_ty) = self.expression(element)?;
                let value = self.widen_to_object(value, &value_ty);
                let discard = self.builder.temp(RType::BIT);
                self.builder.push(Op::SetItem {
                    dest: discard,
                    container: Value::Register(accumulator),
                    index: key,
                    value,
                });
            }
        }
        Ok(())
    }

    /// every element of a display, boxed
    fn boxed_elements(&mut self, elements: &[Expr]) -> Lowered<Vec<Value>> {
        let mut items = Vec::with_capacity(elements.len());
        for element in elements {
            let (value, ty) = self.expression(element)?;
            items.push(self.widen_to_object(value, &ty));
        }
        Ok(items)
    }

    /// a list, set or tuple display
    ///
    /// with no `*` this is one build op. with one, the display is built in runs and
    /// each star *extends* it — which is the method the container already has, so
    /// nothing here needs an op of its own
    fn display(&mut self, elements: &[Expr], kind: Display) -> Lowered<(Value, RType)> {
        if !elements.iter().any(Expr::is_starred_expr) {
            let items = self.boxed_elements(elements)?;
            let dest = self.builder.temp(kind.rtype());
            self.builder.push(kind.build(dest, items));
            return Ok((Value::Register(dest), kind.rtype()));
        }

        // a tuple is built as a list and converted, because a tuple cannot be
        // extended in place
        let building = match kind {
            Display::Tuple => Display::List,
            other => other,
        };
        let accumulator = self.builder.temp(building.rtype());
        let mut run: Vec<&Expr> = Vec::new();
        let mut started = false;
        for element in elements {
            match element {
                Expr::Starred(starred) => {
                    self.flush_run(accumulator, &run, building, &mut started)?;
                    run.clear();
                    let (value, ty) = self.expression(&starred.value)?;
                    let source = self.widen_to_object(value, &ty);
                    if started {
                        self.extend(accumulator, source, building);
                    } else {
                        // the first piece *is* the accumulator, once it is a fresh
                        // container of the right kind rather than the source itself
                        self.builder.push(building.build(accumulator, Vec::new()));
                        started = true;
                        self.extend(accumulator, source, building);
                    }
                }
                other => run.push(other),
            }
        }
        self.flush_run(accumulator, &run, building, &mut started)?;

        if matches!(kind, Display::Tuple) {
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::ToTuple {
                dest,
                src: Value::Register(accumulator),
            });
            return Ok((Value::Register(dest), RType::OBJECT));
        }
        Ok((Value::Register(accumulator), kind.rtype()))
    }

    /// a run of plain elements between two stars, appended in one go
    fn flush_run(
        &mut self,
        accumulator: RegisterId,
        run: &[&Expr],
        kind: Display,
        started: &mut bool,
    ) -> Lowered<()> {
        if !*started {
            let items = run
                .iter()
                .map(|element| {
                    let (value, ty) = self.expression(element)?;
                    Ok(self.widen_to_object(value, &ty))
                })
                .collect::<Lowered<Vec<Value>>>()?;
            self.builder.push(kind.build(accumulator, items));
            *started = true;
            return Ok(());
        }
        if run.is_empty() {
            return Ok(());
        }
        let items = run
            .iter()
            .map(|element| {
                let (value, ty) = self.expression(element)?;
                Ok(self.widen_to_object(value, &ty))
            })
            .collect::<Lowered<Vec<Value>>>()?;
        let piece = self.builder.temp(kind.rtype());
        self.builder.push(kind.build(piece, items));
        self.extend(accumulator, Value::Register(piece), kind);
        Ok(())
    }

    /// merge `source` into a display under construction
    fn extend(&mut self, container: RegisterId, source: Value, kind: Display) {
        let status = self.builder.temp(RType::BIT);
        let container = self.widen_to_object(Value::Register(container), &kind.rtype());
        self.builder.push(Op::Extend {
            dest: status,
            container,
            source,
            mapping: matches!(kind, Display::Dict),
        });
    }

    /// a dict display, where `**` merges another mapping in
    fn dict_display(&mut self, items: &[ast::DictItem]) -> Lowered<(Value, RType)> {
        let accumulator = self.builder.temp(RType::OBJECT);
        let mut pairs: Vec<Value> = Vec::new();
        let mut started = false;
        for item in items {
            match &item.key {
                Some(key) => {
                    let (key, key_ty) = self.expression(key)?;
                    pairs.push(self.widen_to_object(key, &key_ty));
                    let (value, value_ty) = self.expression(&item.value)?;
                    pairs.push(self.widen_to_object(value, &value_ty));
                }
                None => {
                    let target = if started {
                        self.builder.temp(RType::OBJECT)
                    } else {
                        accumulator
                    };
                    self.builder.push(Op::BuildDict {
                        dest: target,
                        pairs: std::mem::take(&mut pairs),
                    });
                    if started {
                        self.extend(accumulator, Value::Register(target), Display::Dict);
                    }
                    started = true;
                    let (value, ty) = self.expression(&item.value)?;
                    let source = self.widen_to_object(value, &ty);
                    self.extend(accumulator, source, Display::Dict);
                }
            }
        }
        if started {
            if !pairs.is_empty() {
                let tail = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::BuildDict { dest: tail, pairs });
                self.extend(accumulator, Value::Register(tail), Display::Dict);
            }
        } else {
            self.builder.push(Op::BuildDict {
                dest: accumulator,
                pairs,
            });
        }
        Ok((Value::Register(accumulator), RType::OBJECT))
    }

    /// an f-string: each literal part and each interpolation, concatenated
    fn fstring(&mut self, node: &ast::ExprFString) -> Lowered<(Value, RType)> {
        let mut accumulated: Option<Value> = None;

        let append = |lowering: &mut Self, part: Value, acc: &mut Option<Value>| {
            *acc = Some(match acc.take() {
                None => part,
                Some(previous) => {
                    let dest = lowering.builder.temp(RType::STR);
                    lowering.builder.push(Op::StrConcat {
                        dest,
                        lhs: previous,
                        rhs: part,
                        consumes_lhs: false,
                    });
                    Value::Register(dest)
                }
            });
        };

        for part in &node.value {
            match part {
                ast::FStringPart::Literal(literal) => {
                    let text = Value::Str(literal.value.to_string());
                    append(self, text, &mut accumulated);
                }
                ast::FStringPart::FString(fstring) => {
                    for element in &fstring.elements {
                        match element {
                            ast::InterpolatedStringElement::Literal(literal) => {
                                let text = Value::Str(literal.value.to_string());
                                append(self, text, &mut accumulated);
                            }
                            ast::InterpolatedStringElement::Interpolation(interpolation) => {
                                let rendered = self.interpolation(interpolation)?;
                                append(self, rendered, &mut accumulated);
                            }
                        }
                    }
                }
            }
        }

        // an empty f-string is the empty string
        let value = accumulated.unwrap_or_else(|| Value::Str(String::new()));
        // the result must be a register, because the caller may store it
        match value {
            Value::Register(_) => Ok((value, RType::STR)),
            immediate => {
                let dest = self.builder.temp(RType::STR);
                self.builder.assign(dest, immediate);
                Ok((Value::Register(dest), RType::STR))
            }
        }
    }

    /// one `{...}` inside an f-string
    fn interpolation(&mut self, node: &ast::InterpolatedElement) -> Lowered<Value> {
        let (value, ty) = self.expression(&node.expression)?;
        let value = self.widen_to_object(value, &ty);

        // a spec is itself an f-string, so a `{width}` inside one is an ordinary
        // interpolation — the same lowering, one level down
        let spec = match &node.format_spec {
            None => None,
            Some(spec) => {
                let mut accumulated: Option<Value> = None;
                for element in &spec.elements {
                    let piece = match element {
                        ast::InterpolatedStringElement::Literal(literal) => {
                            Value::Str(literal.value.to_string())
                        }
                        ast::InterpolatedStringElement::Interpolation(inner) => {
                            self.interpolation(inner)?
                        }
                    };
                    accumulated = Some(match accumulated {
                        None => piece,
                        Some(prefix) => {
                            let dest = self.builder.temp(RType::STR);
                            self.builder.push(Op::StrConcat {
                                dest,
                                lhs: prefix,
                                rhs: piece,
                                consumes_lhs: false,
                            });
                            Value::Register(dest)
                        }
                    });
                }
                Some(accumulated.unwrap_or_else(|| Value::Str(String::new())))
            }
        };
        let conversion = match node.conversion {
            // `{x=}` renders the *repr*, unless a conversion or a format spec says
            // otherwise — which is what makes it a debugging form
            ast::ConversionFlag::None if node.debug_text.is_some() && spec.is_none() => {
                Conversion::Repr
            }
            ast::ConversionFlag::None => Conversion::None,
            ast::ConversionFlag::Str => Conversion::Str,
            ast::ConversionFlag::Repr => Conversion::Repr,
            ast::ConversionFlag::Ascii => Conversion::Ascii,
        };

        let dest = self.builder.temp(RType::STR);
        self.builder.push(Op::Format {
            dest,
            value,
            spec,
            conversion,
        });
        // `{x=}` emits the source text before the value, spacing and all
        let Some(debug) = &node.debug_text else {
            return Ok(Value::Register(dest));
        };
        let joined = self.builder.temp(RType::STR);
        self.builder.push(Op::StrConcat {
            dest: joined,
            lhs: Value::Str(debug.as_str().to_string()),
            rhs: Value::Register(dest),
            consumes_lhs: false,
        });
        Ok(Value::Register(joined))
    }

    /// `receiver.name`
    fn attribute(&mut self, node: &ast::ExprAttribute) -> Lowered<(Value, RType)> {
        let (receiver, receiver_ty) = self.expression(&node.value)?;

        // a receiver whose class the compiler emitted reads the field directly:
        // one load at a compile-time offset, no hash lookup and no descriptor
        let name = self.attribute_name(&node.attr);
        if let RType::Instance { class, .. } = &receiver_ty
            && let Some(fields) = self.layouts.get(class)
            && let Some(held) = fields.iter().find(|field| field.name == name)
        {
            let field_ty = held.ty.clone();
            let dest = self.builder.temp(field_ty.clone());
            self.builder.push(Op::GetField {
                dest,
                receiver,
                class: class.clone(),
                field: name,
            });
            return Ok((Value::Register(dest), field_ty));
        }

        // every other name on an emitted instance still goes out through the dynamic
        // form, because the type is where a method and a class-level constant live and
        // the lookup finds them there. these two are the exception: `__dict__` stands for
        // a namespace an emitted instance does not have, and `__weakref__` for support a
        // type spec does not add, so neither is anywhere to be found — the read raises
        // where the interpreted class answered. `multiprocessing.dummy.Namespace` reads
        // its own `__dict__`, and `tkinter.Event` reads one in `__repr__`
        if let RType::Instance { class, .. } = &receiver_ty
            && self.layouts.contains_key(class)
            && matches!(name.as_str(), "__dict__" | "__weakref__")
        {
            return Err(Decline::new(format!(
                "`{name}` is read off a `{class}`, and an emitted instance is its layout with nothing behind it"
            )));
        }

        let receiver = self.widen_to_object(receiver, &receiver_ty);
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::GetAttr {
            dest,
            receiver,
            name,
        });
        self.narrow_call_result(dest, &Expr::Attribute(node.clone()))
    }

    /// `super()`, as the `super(__class__, <slot zero>)` python's compiler writes
    ///
    /// the class is named by identity rather than through the namespace: a class
    /// decorator replaces the namespace entry, and python's `__class__` cell holds
    /// the class the `class` statement made either way
    fn zero_argument_super(&mut self) -> Lowered<(Value, RType)> {
        if self.comprehensions > 0 {
            return Err(Decline::new(
                "a `super()` in a comprehension reads that comprehension's own frame, which only python 3.12 and later fold into the method's",
            ));
        }
        let zero = match &self.zero_super {
            Ok(zero) => zero,
            Err(reason) => return Err(Decline::new(*reason)),
        };
        let owner = zero.owner.clone();
        let Some(place) = self.place(&zero.receiver) else {
            return Err(Decline::new(
                "a `super()` with no arguments needs a receiver this frame still holds",
            ));
        };
        let (receiver, receiver_ty) = self.read_place(&place)?;
        let receiver = self.widen_to_object(receiver, &receiver_ty);
        let class = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::LoadClass {
            dest: class,
            class: owner,
        });
        let dest = self.builder.temp(RType::OBJECT);
        self.builder.push(Op::CallPython {
            dest,
            callee: "super".to_string(),
            args: vec![Value::Register(class), receiver],
        });
        Ok((Value::Register(dest), RType::OBJECT))
    }

    /// `a < b < c` — each operand evaluated once, and short-circuiting
    fn chained_compare(&mut self, node: &ast::ExprCompare) -> Lowered<(Value, RType)> {
        let result = self.builder.temp(RType::BIT);
        let join = self.builder.new_block();

        let mut left = self.expression(&node.left)?;
        for (index, (op, right)) in node.ops.iter().zip(node.comparators.iter()).enumerate() {
            let right = self.expression(right)?;
            let bit = self.emit_compare(*op, left.clone(), right.clone())?;
            self.builder.assign(result, bit);

            // every link but the last short-circuits on a false result
            if index + 1 < node.ops.len() {
                let next = self.builder.new_block();
                self.builder.terminate(Terminator::Branch {
                    cond: Value::Register(result),
                    then_block: next,
                    else_block: join,
                });
                self.builder.switch_to(next);
            }
            left = right;
        }
        self.builder.terminate(Terminator::Goto(join));
        self.builder.switch_to(join);
        Ok((Value::Register(result), RType::BIT))
    }

    /// the builtins whose answer is about the frame that called them
    ///
    /// a compiled function pushes no python frame, so each of these reads whatever
    /// frame happens to be underneath — which is the caller's, in another module
    /// entirely. left as ordinary calls they are silent wrong answers rather than
    /// errors: `globals().get("marker")` answered `None` for a name the module plainly
    /// binds, `globals()["x"] = 1` bound `x` in the caller instead, and `locals()`
    /// handed back the caller's whole namespace.
    ///
    /// `globals()` is the one with a compiled answer, because the namespace it asks
    /// for is this module's own and the extension already holds it — see
    /// [`Op::ModuleDict`]. the rest have none: a compiled frame's locals are
    /// registers, several of them not even objects, so there is no dict to build and
    /// the function is left to its interpreted definition.
    ///
    /// `Ok(None)` where the call is an ordinary one after all, and the lowering below
    /// carries on with it
    fn frame_reading(
        &mut self,
        node: &ast::ExprCall,
        name: &str,
    ) -> Lowered<Option<(Value, RType)>> {
        if !matches!(
            name,
            "globals" | "locals" | "vars" | "dir" | "eval" | "exec"
        ) {
            return Ok(None);
        }
        // a name this frame binds is a value of its own and never a builtin. that is
        // the cheap half of the question, so it is asked before the resolution
        if self.binds(name) {
            return Ok(None);
        }
        let env = &self.model.program_environment();
        if !is_builtin_function(self.db, env, self.model, &node.func, name) {
            return Ok(None);
        }
        match name {
            // `globals()` and `locals()` take nothing at all, so a call carrying
            // arguments is a `TypeError` python has to raise rather than a namespace
            // question — and the interpreted definition raises it with python's wording
            "globals" if node.arguments.is_empty() => {
                let dest = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::ModuleDict { dest });
                Ok(Some((Value::Register(dest), RType::OBJECT)))
            }
            // `vars(x)` is `x.__dict__` and `dir(x)` is that object's names; only the
            // argumentless spellings are about a frame
            "vars" | "dir" if !node.arguments.is_empty() => Ok(None),
            "eval" | "exec" if self.given_a_namespace(node) => Ok(None),
            "eval" | "exec" => Err(Decline::new(format!(
                "`{name}` with no namespace of its own runs in the calling frame's, and a compiled function pushes none"
            ))),
            _ => Err(Decline::new(format!(
                "`{name}()` answers about the calling frame, and a compiled function pushes none"
            ))),
        }
    }

    /// whether `eval`/`exec` was handed a namespace to run the code in
    ///
    /// python defaults the `globals` argument to the *calling* frame's namespace, and
    /// writing `None` there means the same thing — so nothing short of a value that
    /// cannot be `None` settles it, and a `dict` is what the checker proves
    fn given_a_namespace(&self, node: &ast::ExprCall) -> bool {
        // a `*` earlier in the call moves every position after it, so which argument
        // fills `globals` is not knowable here and the answer has to be no
        if node.arguments.args.iter().any(Expr::is_starred_expr) {
            return false;
        }
        let env = &self.model.program_environment();
        node.arguments
            .find_argument_value("globals", 1)
            .and_then(|argument| argument.inferred_type(self.model))
            .and_then(|ty| ty.nominal_class_name(self.db, env))
            == Some("dict")
    }

    /// `sys._getframe()`, which hands back the frame of whoever called it
    ///
    /// the same defect [`Self::frame_reading`] covers, one step further out: a
    /// compiled function pushes no frame, so the frame this answers with is its
    /// caller's — and `sys._getframe().f_globals` then reads another module's
    /// namespace while looking exactly like it read this one's.
    ///
    /// only the call written here. a stdlib function that walks frames *itself* —
    /// `inspect.stack`, `warnings.warn`'s `stacklevel`, `namedtuple` reading
    /// `__module__` off its caller — lands one frame short in the same way, and no
    /// predicate over this function's own body can see that
    fn refuse_a_frame_walk(&self, node: &ast::ExprCall) -> Lowered<()> {
        let written = match node.func.as_ref() {
            Expr::Attribute(attribute) => attribute.attr.as_str(),
            Expr::Name(name) => name.id.as_str(),
            _ => return Ok(()),
        };
        // a syntactic filter first: resolving a definition parses the module it is in,
        // and that is not worth doing for every call in the unit
        if written != "_getframe" {
            return Ok(());
        }
        let env = &self.model.program_environment();
        if defined_as(self.db, env, self.model, &node.func, "_getframe").is_none() {
            return Ok(());
        }
        Err(Decline::new(
            "`_getframe()` answers with the calling frame, and a compiled function pushes none",
        ))
    }

    fn call(&mut self, node: &ast::ExprCall) -> Lowered<(Value, RType)> {
        // `super()` with no arguments is not an ordinary call: python's own compiler
        // fills the two arguments in from the frame. a compiled method has no frame,
        // but the compiler knows both — so it fills them in here instead. a shadowed
        // `super` is left alone, because python resolves the name like any other and
        // whatever it finds is called with the nought arguments written
        if let Expr::Name(callee) = node.func.as_ref()
            && callee.id.as_str() == "super"
            && node.arguments.is_empty()
            && !self.native_callees.contains("super")
            && !self.binds("super")
        {
            return self.zero_argument_super();
        }
        // the builtins that answer about the *calling* frame, asked ahead of every
        // path below so that `exec(*argv)` reaches it too — see [`Self::frame_reading`]
        if let Expr::Name(callee) = node.func.as_ref()
            && let Some(handled) = self.frame_reading(node, callee.id.as_str())?
        {
            return Ok(handled);
        }
        self.refuse_a_frame_walk(node)?;
        // a `*` or a `**` in the arguments means the binding happens at runtime, so
        // the arguments become a tuple and a dict and python does the binding
        if node.arguments.args.iter().any(Expr::is_starred_expr)
            || node.arguments.keywords.iter().any(|kw| kw.arg.is_none())
        {
            return self.call_unpacked(node);
        }
        // keywords the compiler cannot bind here — a method, a name the unit does not
        // own, or one a decorator rebound to a signature this unit never saw — are
        // bound by python, from a tuple and a dict
        if !node.arguments.keywords.is_empty()
            && !matches!(node.func.as_ref(), Expr::Name(name)
                if self.native_callees.contains(name.id.as_str())
                    && !self.decorated.contains(name.id.as_str()))
        {
            return self.call_unpacked(node);
        }
        if let Expr::Attribute(attribute) = node.func.as_ref() {
            return self.method_call(node, attribute);
        }
        // a callee that is not a name is just a value: evaluate it and call through it
        let Expr::Name(callee) = node.func.as_ref() else {
            let (callee, callee_ty) = self.expression(&node.func)?;
            let callee = self.widen_to_object(callee, &callee_ty);
            let mut args = Vec::with_capacity(node.arguments.args.len());
            for argument in &node.arguments.args {
                let (value, ty) = self.expression(argument)?;
                args.push(self.widen_to_object(value, &ty));
            }
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::CallValue { dest, callee, args });
            return self.narrow_call_result(dest, &Expr::Call(node.clone()));
        };
        let name = callee.id.as_str();

        // `len` has a direct lowering, so it is not a call at all — unless the name
        // is shadowed, by a module-level definition *or* by anything this frame
        // binds. a parameter called `len` is not the builtin
        if name == "len"
            && !self.native_callees.contains("len")
            && !self.binds("len")
            && node.arguments.keywords.is_empty()
        {
            let [argument] = node.arguments.args.as_ref() else {
                return Err(Decline::new("`len` takes exactly one argument"));
            };
            let (value, ty) = self.expression(argument)?;
            // an array knows its own length, and it is a field read rather than a
            // call into the object protocol
            if matches!(ty, RType::Array(_)) {
                let dest = self.builder.temp(RType::INT);
                self.builder.push(Op::ArrayLen { dest, array: value });
                return Ok((Value::Register(dest), RType::INT));
            }
            let boxed = self.widen_to_object(value, &ty);
            let dest = self.builder.temp(RType::INT);
            self.builder.push(Op::Len { dest, src: boxed });
            return Ok((Value::Register(dest), RType::INT));
        }

        // a closure this frame made itself is called at its native entry point: the
        // environment is in a register right here, so there is nothing to look up
        // and nothing to box
        if let Some(environment) = &self.environment
            && environment.ready.contains(name)
            && let Some(signature) = self
                .methods
                .get(&environment.class)
                .and_then(|table| table.get(name))
            && signature.params.len() == node.arguments.args.len() + 1
        {
            let owner = environment.class.clone();
            let env = environment.register;
            let params: Vec<RType> = signature
                .params
                .iter()
                .skip(1)
                .map(|(_, rtype)| rtype.clone())
                .collect();
            let ret = signature.ret.clone();
            let mut args = Vec::with_capacity(params.len() + 1);
            args.push(Value::Register(env));
            for (argument, param) in node.arguments.args.iter().zip(&params) {
                let (value, ty) = self.expression(argument)?;
                args.push(self.coerce(value, &ty, param)?);
            }
            let dest = self.builder.temp(ret.clone());
            self.builder.push(Op::CallNative {
                dest: Some(dest),
                owner: Some(owner),
                callee: name.to_string(),
                args,
            });
            return Ok((Value::Register(dest), ret));
        }

        // a name this frame *has* — a parameter, a local, or a capture — is a value,
        // and calling it must read that value. resolving it as a global instead
        // raised `NameError` for every callable held in one
        if self.binds(name) {
            let (callee, callee_ty) = self.expression(&node.func)?;
            let callee = self.widen_to_object(callee, &callee_ty);
            let mut args = Vec::with_capacity(node.arguments.args.len());
            for argument in &node.arguments.args {
                let (value, ty) = self.expression(argument)?;
                args.push(self.widen_to_object(value, &ty));
            }
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::CallValue { dest, callee, args });
            return self.narrow_call_result(dest, &Expr::Call(node.clone()));
        }

        // constructing a class this module emits is an allocation and a call to its
        // own `__init__`, not a trip out through the module namespace
        if let Some(constructed) = self.construct(name, node)? {
            return Ok(constructed);
        }

        // a name the unit does not own is resolved and called the way the
        // interpreter would, with everything boxed on both sides. a call the native
        // entry cannot take goes the same way, and reaches the deferring boundary.
        //
        // a *decorated* one goes the same way for a different reason: the name holds
        // what the decorator returned, and the native entry is what it was handed.
        // reaching it directly would skip the decorator entirely — which is a wrong
        // answer rather than a missed optimization
        if !self.native_callees.contains(name)
            || self.decorated.contains(name)
            || self.defers_call(name, node)
        {
            let mut args = Vec::with_capacity(node.arguments.args.len());
            for argument in &node.arguments.args {
                let (value, ty) = self.expression(argument)?;
                args.push(self.widen_to_object(value, &ty));
            }
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::CallPython {
                dest,
                callee: name.to_string(),
                args,
            });

            return self.narrow_call_result(dest, &Expr::Call(node.clone()));
        }

        self.native_call(node, name)
    }

    /// a call that reaches a definition in this same unit at its native entry point
    ///
    /// the arguments are bound here rather than at runtime: the callee's signature says
    /// which parameter each one fills, what representation it has to arrive in, and
    /// which of the ones left over have a default to stand in.
    ///
    /// `name` is the entry to reach, which is not always the name that was written —
    /// an [unboxed edition](lower_array_edition) and the [direct edition](
    /// lower_direct_edition) of a coroutine are both other spellings of the same
    /// definition
    fn native_call(&mut self, node: &ast::ExprCall, name: &str) -> Lowered<(Value, RType)> {
        let env = &self.model.program_environment();
        // a caller already holding buffers reaches the callee's unboxed edition, which
        // takes them as they are. anything else — a list from python, a name that is
        // not a buffer here — goes to the boxed one, where the coercion below would
        // otherwise have to build a buffer per call
        // the widest edition every one of whose buffers this caller already holds. by
        // width, because a callee may have an edition per element type and one whose
        // signature is a subset would leave a buffer boxed for no reason
        let holds = |signature: &ArraySignature| -> bool {
            signature.iter().all(|(index, rtype)| {
                node.arguments.args.get(*index).is_some_and(|argument| {
                    matches!(argument, Expr::Name(argument)
                    if self.locals.get(argument.id.as_str()).is_some_and(|id| {
                        self.builder.register_type(*id) == Some(rtype)
                    }))
                })
            })
        };
        let chosen = self
            .arrays
            .get(name)
            .into_iter()
            .flatten()
            .filter(|signature| holds(signature))
            .max_by_key(|signature| signature.len())
            .map(|signature| edition_name(name, signature))
            .filter(|edition| self.signatures.contains_key(edition));
        let name = chosen.as_deref().unwrap_or(name);

        let ty = node
            .inferred_type(self.model)
            .ok_or_else(|| Decline::new("a call has no inferred type"))?;
        // the *callee's* return representation, not the checker's type of the call:
        // a native call reads back exactly what the C function returns, and the two
        // part company for a function that never returns at all
        let result_ty = match self.signatures.get(name) {
            Some(signature) => signature.ret.clone(),
            None => map_type(self.db, env, ty)?,
        };

        // the callee's parameter representations, so each argument is coerced rather
        // than assumed to match — a cell read hands back an `object` where the callee
        // may want an unboxed int
        let params: Vec<RType> = self
            .signatures
            .get(name)
            .map(|signature| {
                signature
                    .params
                    .iter()
                    .map(|(_, rtype)| rtype.clone())
                    .collect()
            })
            .unwrap_or_default();
        // a keyword argument goes to the position its *name* has in the callee's
        // signature, and an unsupplied parameter takes its default. the signature is
        // right here, so this is a lookup rather than a runtime bind
        let (names, defaults) = match self.signatures.get(name) {
            Some(signature) => (
                signature
                    .params
                    .iter()
                    .map(|(name, _)| name.clone())
                    .collect::<Vec<_>>(),
                signature.defaults.clone(),
            ),
            None => (Vec::new(), Vec::new()),
        };

        // a variadic callee takes its extra positionals as a tuple and its unmatched
        // keywords as a dict, both built here — so its body sees the same ordinary
        // objects it sees when python calls it
        let (vararg, kwarg) = self
            .signatures
            .get(name)
            .map_or((false, false), |signature| {
                (signature.vararg, signature.kwarg)
            });
        let (posonly, kwonly) = self
            .signatures
            .get(name)
            .map_or((0, 0), |signature| (signature.posonly, signature.kwonly));
        let packed_count = usize::from(vararg) + usize::from(kwarg);
        let named = names.len().saturating_sub(packed_count);
        // a keyword-only parameter is one nothing positional reaches
        let positional_limit = named.saturating_sub(kwonly);

        // a callee whose signature is unknown *declined*, and the pruner will decline
        // this caller too — with a reason that names the callee, which is more use
        // than an arity complaint about a signature nobody has
        if self.signatures.get(name).is_none() {
            let mut args = Vec::with_capacity(node.arguments.args.len());
            for arg in &node.arguments.args {
                let (value, _) = self.expression(arg)?;
                args.push(value);
            }
            let dest = self.builder.temp(result_ty.clone());
            self.builder.push(Op::CallNative {
                dest: Some(dest),
                owner: None,
                callee: name.to_string(),
                args,
            });
            return Ok((Value::Register(dest), result_ty));
        }

        let mut slots: Vec<Option<Value>> = vec![None; params.len().max(names.len())];
        let mut extra_positional: Vec<Value> = Vec::new();
        let mut extra_keywords: Vec<Value> = Vec::new();
        for (index, arg) in node.arguments.args.iter().enumerate() {
            if index >= positional_limit {
                if !vararg {
                    return Err(Decline::new("too many arguments for the callee"));
                }
                let (value, ty) = self.expression(arg)?;
                extra_positional.push(self.widen_to_object(value, &ty));
                continue;
            }
            let (value, ty) = self.expression(arg)?;
            let value = match params.get(index) {
                Some(param) => self.coerce(value, &ty, param)?,
                None => value,
            };
            match slots.get_mut(index) {
                Some(slot) => *slot = Some(value),
                None => return Err(Decline::new("too many arguments for the callee")),
            }
        }
        for keyword in &node.arguments.keywords {
            // a `**` keyword never reaches here: the whole call goes through
            // [`Self::call_unpacked`] instead
            let Some(keyword_name) = &keyword.arg else {
                continue;
            };
            // a positional-only parameter is not reachable by name
            let position = names
                .iter()
                .take(named)
                .position(|param| param == keyword_name.as_str())
                .filter(|index| *index >= posonly);
            let Some(index) = position else {
                // a `**kwargs` parameter takes it; without one it is an error
                if !kwarg {
                    return Err(Decline::new(format!(
                        "`{name}` has no parameter `{keyword_name}`"
                    )));
                }
                let key = self.builder.temp(RType::OBJECT);
                self.builder.push(Op::Assign {
                    dest: key,
                    src: Value::Str(keyword_name.to_string()),
                });
                let (value, ty) = self.expression(&keyword.value)?;
                let value = self.widen_to_object(value, &ty);
                extra_keywords.push(Value::Register(key));
                extra_keywords.push(value);
                continue;
            };
            if slots.get(index).is_some_and(Option::is_some) {
                return Err(Decline::new(format!(
                    "`{name}` got two values for `{keyword_name}`"
                )));
            }
            let (value, ty) = self.expression(&keyword.value)?;
            let value = match params.get(index) {
                Some(param) => self.coerce(value, &ty, param)?,
                None => value,
            };
            slots[index] = Some(value);
        }

        let mut args = Vec::with_capacity(slots.len());
        for (index, slot) in slots.into_iter().enumerate().take(named) {
            match slot.or_else(|| defaults.get(index).cloned().flatten()) {
                Some(value) => {
                    // a supplied argument is coerced above; an *unsupplied* one is the
                    // same argument, and the parameter's representation is the callee's
                    // either way. a bare `length=0` reaching an unannotated parameter is
                    // a tagged integer arriving where a `PyObject *` is declared
                    let from = match &value {
                        Value::Register(id) => Some(self.register_type(*id)?),
                        other => other.immediate_type(),
                    };
                    let value = match (params.get(index), from) {
                        (Some(param), Some(ty)) => self.coerce(value, &ty, param)?,
                        _ => value,
                    };
                    args.push(value);
                }
                None => {
                    let missing = names.get(index).cloned().unwrap_or_default();
                    return Err(Decline::new(format!(
                        "`{name}` needs an argument for `{missing}`"
                    )));
                }
            }
        }
        if vararg {
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::BuildTuple {
                dest,
                items: extra_positional,
            });
            args.push(Value::Register(dest));
        }
        if kwarg {
            let dest = self.builder.temp(RType::OBJECT);
            self.builder.push(Op::BuildDict {
                dest,
                pairs: extra_keywords,
            });
            args.push(Value::Register(dest));
        }

        let dest = self.builder.temp(result_ty.clone());
        self.builder.push(Op::CallNative {
            dest: Some(dest),
            owner: None,
            callee: name.to_string(),
            args,
        });
        Ok((Value::Register(dest), result_ty))
    }
}

fn binary_op(op: Operator) -> Lowered<BinOp> {
    Ok(match op {
        Operator::Add => BinOp::Add,
        Operator::Sub => BinOp::Sub,
        Operator::Mult => BinOp::Mul,
        Operator::FloorDiv => BinOp::FloorDiv,
        Operator::Mod => BinOp::Mod,
        Operator::Div => BinOp::TrueDiv,
        Operator::Pow => BinOp::Pow,
        Operator::BitAnd => BinOp::BitAnd,
        Operator::BitOr => BinOp::BitOr,
        Operator::BitXor => BinOp::BitXor,
        Operator::LShift => BinOp::Shl,
        Operator::RShift => BinOp::Shr,
        other => return Err(Decline::new(format!("`{other:?}` is not lowered yet"))),
    })
}

/// which flavour of comprehension is being built
#[derive(Clone)]
enum Comprehension<'a> {
    List(&'a Expr),
    Set(&'a Expr),
    Dict(&'a Expr, &'a Expr),
    /// filling an unboxed buffer rather than a `list` — the element never becomes a
    /// `PyObject *` at all
    Array(&'a Expr, RType),
}

/// whether the compiler can narrow a boxed value to this representation
///
/// exactly the set `Op::Unbox` has a checked conversion for. `list` and friends
/// are boxed too, but nothing yet reads their elements, so narrowing to them
/// would buy nothing
/// a `range` step written as a literal, which is what settles the comparison
/// direction at compile time
///
/// `None` for anything computed, too large to inline, or not an integer — the
/// caller takes the protocol path, where `range` decides all three itself
fn literal_step(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::NumberLiteral(literal) => match &literal.value {
            ast::Number::Int(value) => value.as_i64(),
            _ => None,
        },
        Expr::UnaryOp(unary) if matches!(unary.op, AstUnaryOp::USub) => {
            match unary.operand.as_ref() {
                Expr::NumberLiteral(literal) => match &literal.value {
                    ast::Number::Int(value) => value.as_i64().map(|value| -value),
                    _ => None,
                },
                _ => None,
            }
        }
        _ => None,
    }
}

/// an expression that is a literal, as an immediate
fn literal_value(expr: &Expr) -> Option<Value> {
    Some(match expr {
        Expr::NumberLiteral(node) => match &node.value {
            ast::Number::Int(value) => Value::Int(value.as_i64()?),
            ast::Number::Float(value) => Value::Float(*value),
            ast::Number::Complex { .. } => return None,
        },
        Expr::BooleanLiteral(node) => Value::Bool(node.value),
        Expr::NoneLiteral(_) => Value::None,
        Expr::StringLiteral(node) => Value::Str(node.value.to_str().to_string()),
        Expr::BytesLiteral(node) => Value::Bytes(node.value.bytes().collect()),
        _ => return None,
    })
}

fn narrowable(rtype: &RType) -> bool {
    matches!(
        rtype,
        RType::Primitive(
            Primitive::Int
                | Primitive::Float
                | Primitive::Bool
                | Primitive::None
                | Primitive::Str
                | Primitive::List
        )
    )
}

/// the representation a binary operation produces
///
/// `/` between two ints is a float, and anything that is not a matched pair of
/// unboxed numbers goes through the object protocol and yields an object
fn binary_result(op: BinOp, lhs: &RType, rhs: &RType) -> RType {
    let bitwise = matches!(
        op,
        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
    );
    // concatenating two strings is still a string
    if lhs == rhs && matches!(lhs, RType::Primitive(Primitive::Str)) && matches!(op, BinOp::Add) {
        return RType::STR;
    }
    let int = |ty: &RType| matches!(ty, RType::Primitive(Primitive::Int));
    let float = |ty: &RType| matches!(ty, RType::Primitive(Primitive::Float));
    if int(lhs) && int(rhs) {
        // `/` is python's one operator that leaves the integers behind
        return if matches!(op, BinOp::TrueDiv) {
            RType::FLOAT
        } else {
            RType::INT
        };
    }
    // python's numeric tower converts the `int` side of a mixed pair to a float
    // and operates on doubles. there is no bitwise operation on one
    if !bitwise
        && (float(lhs) || float(rhs))
        && (int(lhs) || float(lhs))
        && (int(rhs) || float(rhs))
    {
        return RType::FLOAT;
    }
    RType::OBJECT
}

fn compare_op(op: AstCmpOp) -> Lowered<CmpOp> {
    Ok(match op {
        AstCmpOp::Eq => CmpOp::Eq,
        AstCmpOp::NotEq => CmpOp::Ne,
        AstCmpOp::Lt => CmpOp::Lt,
        AstCmpOp::LtE => CmpOp::Le,
        AstCmpOp::Gt => CmpOp::Gt,
        AstCmpOp::GtE => CmpOp::Ge,
        other => return Err(Decline::new(format!("`{other:?}` is not lowered yet"))),
    })
}

/// whether a pattern matches every subject, so nothing after it can run
fn irrefutable(pattern: &ast::Pattern) -> bool {
    match pattern {
        ast::Pattern::MatchAs(node) => node.pattern.as_deref().is_none_or(irrefutable),
        ast::Pattern::MatchOr(node) => node.patterns.iter().any(irrefutable),
        // every one has to match, so the conjunction is only irrefutable if all are
        ast::Pattern::MatchAnd(node) => node.patterns.iter().all(irrefutable),
        _ => false,
    }
}

/// whether a pattern captures any name, which decides if it may stand as an
/// alternative in `P | Q`
fn binds_a_name(pattern: &ast::Pattern) -> bool {
    match pattern {
        ast::Pattern::MatchAs(node) => {
            node.name.is_some() || node.pattern.as_deref().is_some_and(binds_a_name)
        }
        ast::Pattern::MatchOr(node) => node.patterns.iter().any(binds_a_name),
        ast::Pattern::MatchValue(_) | ast::Pattern::MatchSingleton(_) => false,
        // anything else binds by construction, or is declined before this is asked
        _ => true,
    }
}

fn statement_kind(stmt: &Stmt) -> &'static str {
    match stmt {
        Stmt::For(_) => "for",
        Stmt::With(_) => "with",
        Stmt::Match(_) => "match",
        Stmt::ClassDef(_) => "class",
        Stmt::FunctionDef(_) => "a nested function",
        Stmt::Import(_) | Stmt::ImportFrom(_) => "import",
        Stmt::Global(_) | Stmt::Nonlocal(_) => "global/nonlocal",
        Stmt::Delete(_) => "del",
        Stmt::Break(_) => "break",
        Stmt::Continue(_) => "continue",
        _ => "this statement",
    }
}

/// what to call an expression in a decline
///
/// a decline that does not name what declined cannot be counted, so every kind the
/// dispatch can fall through to has a name here
fn expression_kind(expr: &Expr) -> &'static str {
    match expr {
        Expr::Subscript(_) => "subscript",
        Expr::Dict(_) => "a dict display",
        Expr::Set(_) => "a set display",
        Expr::Tuple(_) => "a tuple display",
        Expr::Generator(_) => "a generator expression",
        Expr::Lambda(_) => "a lambda",
        Expr::Await(_) => "await",
        Expr::TString(_) => "a template string",
        Expr::Starred(_) => "a starred expression here",
        Expr::IpyEscapeCommand(_) => "an ipython escape",
        Expr::CallableType(_) | Expr::ProtocolType(_) | Expr::ProtocolMethod(_) => {
            "a type expression here"
        }
        Expr::Statement(_) => "a statement expression",
        _ => "this expression",
    }
}

/// unused today, kept because a function with an empty exception set is what
/// selects the infallible convention once the `raises` analysis is wired in
pub fn infallible(function: &mut Function) {
    function.convention = CallConvention::NativeInfallible;
}

/// the entry block, exposed for tests that assert on block structure
pub const ENTRY: BlockId = BlockId(0);

#[cfg(test)]
mod tests;
