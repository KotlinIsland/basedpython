//! `Type` → `RType`
//!
//! the only place the compiler reads ty. an `RType` answers "what do the bits
//! look like", so this is a lossy projection on purpose: `Literal[1]`, `int` and
//! `Literal[1] | Literal[2]` all land on the same tagged integer.
//!
//! the rule that governs every arm is the representation invariant: mapping to a
//! narrower `RType` is only allowed where the type *proves* the value has that
//! representation. anything gradual proves nothing, so it is declined rather than
//! guessed at.

use by_ir::function::FieldDecl;
use by_ir::rtype::RType;
use ruff_db::files::File;
use ty_python_semantic::ProgramEnvironment;
use ty_python_semantic::types::{KnownClass, Type};

/// why a construct could not be lowered natively
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decline {
    pub(crate) reason: String,
}

impl Decline {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

pub(crate) type Lowered<T> = Result<T, Decline>;

/// the classes whose layout this compilation emitted, by name
///
/// the whole declaration and not just the name and representation: a subclass's
/// layout *is* its base's plus its own, so anything the base's fields cost —
/// the presence byte an optional one carries — the subclass has to spend too, or
/// its instances would be smaller than the base python lays them out over
///
/// the *file* is carried beside them because a bare name is not an identity. every
/// name in here came out of this module's own source, so a lookup keyed on one is
/// safe wherever the name did too — but `map_type_with` starts from a ty type,
/// which may be a class of the same name from anywhere. `csv` declares a `Dialect`
/// and imports `_csv.Dialect` beside it, and asking the name alone gave the imported
/// class this module's layout: `_Dialect(self)` was narrowed to a struct its answer
/// is not, and `csv.excel()` raised where python built
#[derive(Clone)]
pub struct Layouts {
    file: File,
    classes: std::collections::HashMap<String, Vec<FieldDecl>>,
}

impl Layouts {
    /// the classes `file` writes, each with no fields worked out yet
    pub(crate) fn of(file: File, names: impl IntoIterator<Item = String>) -> Self {
        Self {
            file,
            classes: names.into_iter().map(|name| (name, Vec::new())).collect(),
        }
    }

    /// how many classes are in here, which bounds the rounds it takes to settle
    pub(crate) fn count(&self) -> usize {
        self.classes.len()
    }

    pub(crate) fn contains_key(&self, name: &str) -> bool {
        self.classes.contains_key(name)
    }

    pub(crate) fn get(&self, name: &str) -> Option<&Vec<FieldDecl>> {
        self.classes.get(name)
    }

    pub(crate) fn names(&self) -> impl Iterator<Item = &String> {
        self.classes.keys()
    }

    pub(crate) fn insert(&mut self, name: String, fields: Vec<FieldDecl>) {
        self.classes.insert(name, fields);
    }

    pub(crate) fn remove(&mut self, name: &str) {
        self.classes.remove(name);
    }

    pub(crate) fn clear(&mut self) {
        self.classes.clear();
    }
}

/// [`map_type`], plus knowledge of which classes have an emitted layout
///
/// a value whose class is one of them gets [`RType::Instance`], which is what
/// turns an attribute read into a field read at a compile-time offset
pub(crate) fn map_type_with(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
    layouts: &Layouts,
) -> Lowered<RType> {
    // a use-site modifier says something about the *place*, not about what the value
    // is: `A()` infers `final A`, and the thing being laid out is still an `A`. asking
    // `nominal_class_name` through the modifier is what stops a constructed value from
    // getting its own class's representation
    let bare = ty.erase_restriction(db);
    if !bare.is_dynamic()
        && let Some(class) = bare.nominal_class_name(db, env)
        && layouts.contains_key(class)
        // the class *this* module wrote under that name, and not another module's of
        // the same name — see [`Layouts`]
        && bare.nominal_class_file(db, env) == Some(layouts.file)
    {
        return Ok(RType::Instance {
            class: class.to_string(),
            // a `@final` or `sealed` class admits no subclass, so a value of it is
            // exactly it — which is what re-licenses the direct method call on a
            // class that is otherwise open
            exact: ty.nominal_class_is_exact(db, env),
        });
    }
    map_type(db, env, ty)
}

/// the representation a *local* gets, which may be an unboxed array where an
/// argument or a return could not be
///
/// a `list` of values that own nothing can live in a buffer of its own rather than
/// as a `PyObject *` each. it is only ever a local: a parameter arrives as a real
/// list and a return has to be one, and converting either way is a copy — which
/// would lose the list's *identity*, not just time
pub(crate) fn map_local_type(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
    layouts: &Layouts,
) -> Lowered<RType> {
    if let Some(element) = ty.list_element_type(db, env)
        && let Ok(element) = map_type_with(db, env, element, layouts)
        && element.is_unboxed()
        && !element.is_refcounted()
    {
        return Ok(RType::Array(Box::new(element)));
    }
    map_type_with(db, env, ty, layouts)
}

/// the representation a fixed-length tuple has when its elements are held in
/// registers rather than in a heap object
///
/// this is never what a *place* holds — only what a value on its way from one place
/// to another may be. a register tuple has no identity of its own, so anything that
/// keeps one has to build the real object first, and [`RType::Tuple`] is only
/// reached where the lowering can prove that happens at most once. `tuple[int, int]`
/// is `(tagged, tagged)`; `tuple[int, ...]` has no length and is not one of these
pub(crate) fn map_fixed_tuple(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
    layouts: &Layouts,
) -> Option<RType> {
    // gradual proves nothing about the length either, and `Never` is assignable to
    // every tuple type there is
    if ty.is_dynamic() || ty.is_never() || ty.has_gradual_member(db, env) {
        return None;
    }
    let elements = ty.fixed_tuple_element_types(db)?;
    // an empty tuple is `()`, which python hands back as one shared object — there is
    // nothing to hold in registers and a zero-field struct to hold it in
    if elements.is_empty() {
        return None;
    }
    let slots = elements
        .iter()
        .map(|element| map_type_with(db, env, *element, layouts).ok())
        .collect::<Option<Box<[RType]>>>()?;
    Some(RType::Tuple(slots))
}

/// the representation `ty` would have had, had python's numeric promotion not
/// widened it
///
/// nothing about this changes what compiles — a promoted place lands on the object
/// protocol and works — so it is reported rather than declined. `strict-float` is
/// what recovers it
pub(crate) fn missed_representation(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
    layouts: &Layouts,
) -> Option<RType> {
    if is_promoted_float(db, env, ty) {
        return Some(RType::FLOAT);
    }
    let element = ty.list_element_type(db, env)?;
    // the element's *strict* representation, which is what a buffer needs
    let strict = if is_promoted_float(db, env, element) {
        RType::FLOAT
    } else {
        map_type_with(db, env, element, layouts).ok()?
    };
    // a list whose element is already unboxed is a buffer, so nothing was missed
    let missed = strict.is_unboxed()
        && !strict.is_refcounted()
        && map_local_type(db, env, ty, layouts)
            .is_ok_and(|rtype| !matches!(rtype, RType::Array(_)));
    missed.then(|| RType::Array(Box::new(strict)))
}

/// whether `ty` is python's promoting `float` annotation, `int | float`
///
/// the typing spec makes an `int` acceptable wherever a `float` is asked for, and
/// ty models that by widening the *annotation*. so a `.py` parameter written
/// `float` is a union, and nothing about it proves a `double` representation —
/// only the boundary can, one call at a time. `.by` opts out of the promotion, so
/// this is never true there
pub(crate) fn is_promoted_float(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
) -> bool {
    let Some(union) = ty.as_union() else {
        return false;
    };
    let int = KnownClass::Int.to_instance(db, env);
    let float = KnownClass::Float.to_instance(db, env);
    if int.is_dynamic() || float.is_dynamic() {
        return false;
    }
    let same =
        |a: Type<'_>, b: Type<'_>| a.is_assignable_to(db, env, b) && b.is_assignable_to(db, env, a);
    // a *gradual* element is assignable both ways to everything, so one of them would
    // answer for both halves of this and any `Unknown | T` would read as the
    // promotion. gradual proves nothing, which is the rule the whole mapper rests on
    if ty.has_gradual_member(db, env) {
        return false;
    }
    let elements = union.elements(db);
    elements.len() == 2
        && elements.iter().any(|e| same(*e, int))
        && elements.iter().any(|e| same(*e, float))
}

/// the representation a value of type `ty` has
///
/// the order of the checks matters: `bool` is a subclass of `int` in python, so
/// it has to be recognized first or every `bool` would be given the tagged
/// integer representation
// `RType::OBJECT` is a representation for anything, so this one never declines today.
// it still answers in `Lowered` because that is the shape every entry point in this
// module has, and a caller threading `?` through the three of them should not have to
// know which of them can currently fail
#[expect(clippy::unnecessary_wraps)]
pub(crate) fn map_type(
    db: &dyn ty_python_semantic::Db,
    env: &ProgramEnvironment<'_>,
    ty: Type<'_>,
) -> Lowered<RType> {
    // a gradual type is not a proof of anything, so it lands on the widest
    // representation. `object` assumes nothing, which is exactly why it needs no
    // check: the representation invariant only bites when *narrowing*
    if ty.is_dynamic() {
        return Ok(RType::OBJECT);
    }
    // and a type with a gradual *member* proves nothing either, because that member is
    // assignable to whatever is asked about — so every test below would answer yes.
    // `def f(x=None)` is the common way to meet the union half: its type is
    // `Unknown | None`, which read as `None` and made storing anything else into `x`
    // impossible. narrowing produces the intersection half, `Unknown & None`
    if ty.has_gradual_member(db, env) {
        return Ok(RType::OBJECT);
    }

    // the bottom of the lattice proves nothing either, for the mirror image of the
    // reason the top does: `Never` is assignable to *everything*, so every test below
    // answers yes and whichever is written first wins. that was `None`, a representation
    // with no width at all — and ty gives every expression in unreachable code the type
    // `Never`, so
    //
    //     def f(line: str):
    //         return
    //         b = not line
    //
    // asked to store a `bit` into a place that cannot hold one, and declined a function
    // for code that never runs. the same two statements without the `return` compile
    if ty.is_never() {
        return Ok(RType::OBJECT);
    }

    let none = Type::none(db, env);
    if ty.is_assignable_to(db, env, none) {
        return Ok(RType::NONE);
    }
    for (known, rtype) in [
        (KnownClass::Bool, RType::BOOL),
        (KnownClass::Int, RType::INT),
        (KnownClass::Float, RType::FLOAT),
        (KnownClass::Str, RType::STR),
    ] {
        let instance = known.to_instance(db, env);
        if !instance.is_dynamic() && ty.is_assignable_to(db, env, instance) {
            return Ok(rtype);
        }
    }

    // anything else is a real type with no unboxed representation yet — a
    // container, a union, a user class. it is still a `PyObject *`, so it can be
    // passed, stored and returned; only operations on it decline
    Ok(RType::OBJECT)
}

/// whether a place of this representation carries no static information, so an
/// operation on it has to go through the abstract object protocol
#[cfg(test)]
fn is_boxed(rtype: &RType) -> bool {
    *rtype == RType::OBJECT
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::single_file::with_source;

    /// map the declared type of the single parameter of `def f(a: <annotation>)`
    fn param_repr(annotation: &str) -> Result<RType, String> {
        with_source(
            &format!(
                "from typing import Any, Literal, Never\n\
                 def f(a: {annotation}) -> None:\n    pass\n"
            ),
            |db, env, model, suite| {
                let ruff_python_ast::Stmt::FunctionDef(function) = &suite[1] else {
                    return Err("not a function".to_string());
                };
                let parameter = &function.parameters.args[0].parameter;
                let ty = ty_python_semantic::HasType::inferred_type(parameter, model)
                    .ok_or_else(|| "no inferred type".to_string())?;
                map_type(db, env, ty).map_err(|decline| decline.reason)
            },
        )
    }

    #[test]
    fn the_primitive_types_map_to_their_representations() {
        assert_eq!(param_repr("int"), Ok(RType::INT));
        assert_eq!(param_repr("float"), Ok(RType::FLOAT));
        assert_eq!(param_repr("str"), Ok(RType::STR));
        assert_eq!(param_repr("None"), Ok(RType::NONE));
    }

    /// whether the single parameter of `def f(a: <annotation>)` is python's
    /// promoting float, asked of a `.py` source — where the promotion applies
    fn param_is_promoted_float(annotation: &str) -> bool {
        crate::single_file::with_source_in(
            &format!("def f(a: {annotation}) -> None:\n    pass\n"),
            crate::Language::Python,
            |db, env, model, suite| {
                let ruff_python_ast::Stmt::FunctionDef(function) = &suite[0] else {
                    return false;
                };
                let parameter = &function.parameters.args[0].parameter;
                ty_python_semantic::HasType::inferred_type(parameter, model)
                    .is_some_and(|ty| is_promoted_float(db, env, ty))
            },
        )
    }

    #[test]
    fn python_float_is_promoted_and_basedpython_float_is_not() {
        assert!(param_is_promoted_float("float"));
        // `.by` opts out of the promotion, so its `float` is a plain instance
        assert_eq!(param_repr("float"), Ok(RType::FLOAT));
        for annotation in ["int", "bool", "complex", "int | str", "object"] {
            assert!(!param_is_promoted_float(annotation), "{annotation}");
        }
    }

    #[test]
    fn bool_is_recognized_before_int() {
        // `bool` is a subclass of `int`, so a naive order would give it the
        // tagged integer representation and lose `True` on the way out
        assert_eq!(param_repr("bool"), Ok(RType::BOOL));
    }

    #[test]
    fn a_literal_maps_to_the_representation_of_its_class() {
        assert_eq!(param_repr("Literal[1]"), Ok(RType::INT));
        assert_eq!(param_repr("Literal['x']"), Ok(RType::STR));
        assert_eq!(param_repr("Literal[True]"), Ok(RType::BOOL));
    }

    #[test]
    fn a_union_of_int_literals_is_still_an_int() {
        assert_eq!(param_repr("Literal[1, 2, 3]"), Ok(RType::INT));
    }

    #[test]
    fn a_gradual_type_is_the_widest_representation() {
        // `object` assumes nothing, so it needs no check — the representation
        // invariant only bites when narrowing
        assert_eq!(param_repr("Any"), Ok(RType::OBJECT));
    }

    #[test]
    fn the_bottom_of_the_lattice_is_the_widest_representation_too() {
        // `Never` is assignable to *everything*, so every test in `map_type` answers
        // yes and whichever is written first wins. that was `None`, which has no width
        // at all — and since ty types every expression in unreachable code as `Never`,
        // a dead `b = not line` after a `return` had a `bit` and nowhere to put it
        assert_eq!(param_repr("Never"), Ok(RType::OBJECT));
        // and a real `None` still gets the representation that says so
        assert_eq!(param_repr("None"), Ok(RType::NONE));
    }

    /// map the declared type of the single parameter of `def f(a: Alias)`, where
    /// `Alias` is a type alias standing for `value`
    fn alias_repr(value: &str) -> Result<RType, String> {
        with_source(
            &format!(
                "from typing import Any\n\
                 type Alias = {value}\n\
                 def f(a: Alias) -> None:\n    pass\n"
            ),
            |db, env, model, suite| {
                let ruff_python_ast::Stmt::FunctionDef(function) = &suite[2] else {
                    return Err("not a function".to_string());
                };
                let parameter = &function.parameters.args[0].parameter;
                let ty = ty_python_semantic::HasType::inferred_type(parameter, model)
                    .ok_or_else(|| "no inferred type".to_string())?;
                map_type(db, env, ty).map_err(|decline| decline.reason)
            },
        )
    }

    #[test]
    fn an_alias_is_as_gradual_as_what_it_stands_for() {
        // an alias is the same type under another spelling, so the gradual test has to
        // reach through it. `_socket` writes `type _RetAddress = dynamic` and
        // `socket.getsockname()` answers with it — read as a proof, that landed on
        // `None`, the first representation a gradual type is assignable to, and
        // `self._address = sock.getsockname()` then had nowhere to put a string
        assert_eq!(alias_repr("Any"), Ok(RType::OBJECT));
        // and out through a union around it, the way an ordinary gradual member is
        assert_eq!(alias_repr("int | Any"), Ok(RType::OBJECT));
        // a name over an ordinary type still stands for exactly that type
        assert_eq!(alias_repr("int"), Ok(RType::INT));
        assert_eq!(alias_repr("None"), Ok(RType::NONE));
    }

    #[test]
    fn a_type_with_no_unboxed_representation_is_still_an_object() {
        assert_eq!(param_repr("list[int]"), Ok(RType::OBJECT));
        assert_eq!(param_repr("dict[str, int]"), Ok(RType::OBJECT));
    }

    #[test]
    fn a_mixed_union_is_an_object() {
        assert_eq!(param_repr("int | str"), Ok(RType::OBJECT));
    }

    /// ask something of the inferred type of what `def f(a): return <expr>` returns,
    /// over an *unannotated* `a` — the gradual value narrowing acts on
    fn about_returned<R>(
        expr: &str,
        ask: impl FnOnce(&dyn ty_python_semantic::Db, &ProgramEnvironment<'_>, Type<'_>) -> R,
    ) -> Result<R, String> {
        crate::single_file::with_source_in(
            &format!("def f(a):\n    return {expr}\n"),
            crate::Language::Python,
            |db, env, model, suite| {
                let ruff_python_ast::Stmt::FunctionDef(function) = &suite[0] else {
                    return Err("not a function".to_string());
                };
                let ruff_python_ast::Stmt::Return(returned) = &function.body[0] else {
                    return Err("not a return".to_string());
                };
                let value = returned
                    .value
                    .as_deref()
                    .ok_or_else(|| "a bare return".to_string())?;
                let ty = ty_python_semantic::HasType::inferred_type(value, model)
                    .ok_or_else(|| "no inferred type".to_string())?;
                Ok(ask(db, env, ty))
            },
        )
    }

    #[test]
    fn a_gradual_value_narrowed_on_both_arms_still_proves_nothing() {
        // narrowing a gradual value gives an *intersection* holding it, and a
        // conditional over both arms unions two of those. the gradual part widens
        // whatever encloses it however deep it sits, so the whole type is assignable
        // to `None` — and that is not a proof that it holds one
        assert_eq!(
            about_returned("a if isinstance(a, int) else a", map_type),
            Ok(Ok(RType::OBJECT))
        );
        // the promotion asks the same question of the same shape, so it answers no here
        // for the same reason: `Unknown & int` is assignable both ways to `int`
        assert_eq!(
            about_returned("a if isinstance(a, int) else 1.0", is_promoted_float),
            Ok(false)
        );
    }

    #[test]
    fn only_the_widest_representation_counts_as_boxed() {
        assert!(is_boxed(&RType::OBJECT));
        assert!(!is_boxed(&RType::INT));
        // a `str` is a PyObject too, but its class is known, so operations on it
        // can be specialized rather than going through the abstract protocol
        assert!(!is_boxed(&RType::STR));
    }
}
