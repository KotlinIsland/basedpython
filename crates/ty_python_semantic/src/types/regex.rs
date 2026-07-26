//! statically-known regular-expression capture groups
//!
//! when a pattern reaches `re.compile` / `re.match` / … as a literal, we know
//! exactly which capture groups it has, what they are named, and which of them
//! must have participated in any successful match. that turns the deliberately
//! vague stub signatures — `Match.group` returns `AnyStr | None`, `findall`
//! returns `list[Any]` — into precise types
//!
//! the groups ride along on the `re.Match` / `re.Pattern` instance type itself
//! (see [`Type::regex_instance`]), so they survive assignment, narrowing and a
//! round trip through `re.compile`

mod parse;

use std::fmt;

use ruff_python_ast::name::Name;
use ty_module_resolver::{KnownModule, file_to_module};

use crate::Db;
use crate::types::instance::NominalInstanceType;
use crate::types::typed_dict::{TypedDictFieldBuilder, TypedDictOpenness, TypedDictSchema};
use crate::types::{
    ClassLiteral, KnownClass, LiteralValueTypeKind, Type, TypeContext, TypeMapping, TypedDictType,
    UnionType,
};

pub(crate) use parse::{PatternAnalysis, analyze};

/// one capture group of a statically-known pattern
#[derive(Debug, Clone, PartialEq, Eq, Hash, get_size2::GetSize, salsa::SalsaValue)]
pub struct RegexGroup {
    /// the `(?P<name>…)` name, if the group has one
    pub(crate) name: Option<Name>,
    /// whether the group must have participated in *every* successful match
    pub(crate) definitely_set: bool,
}

/// the capture groups of a statically-known pattern, in group-number order
#[salsa::interned(debug, heap_size=ruff_memory_usage::heap_size)]
pub struct RegexGroups<'db> {
    #[returns(deref)]
    pub(crate) groups: Box<[RegexGroup]>,
}

// The Salsa heap is tracked separately.
impl get_size2::GetSize for RegexGroups<'_> {}

impl<'db> RegexGroups<'db> {
    /// build the groups of a pattern we parsed
    pub(crate) fn from_parsed(db: &'db dyn Db, parsed: &[parse::ParsedGroup]) -> Self {
        Self::new(
            db,
            parsed
                .iter()
                .map(|group| RegexGroup {
                    name: group.name.clone(),
                    definitely_set: group.definitely_set,
                })
                .collect::<Box<[_]>>(),
        )
    }

    /// resolve `key` to the type its group has when the match succeeded
    ///
    /// `unset` is what an unmatched group evaluates to — `None`, or the default
    /// passed to `groups()` / `groupdict()`. `Err` means the pattern has no such
    /// group, which `re` raises `IndexError` for.
    fn resolve(
        self,
        db: &'db dyn Db,
        any_str: Type<'db>,
        unset: Type<'db>,
        key: GroupKey<'_>,
    ) -> Result<Type<'db>, NoSuchGroup> {
        let groups = self.groups(db);
        let group = match key {
            // group 0 is the whole match, which by definition participated
            GroupKey::Number(0) => return Ok(any_str),
            GroupKey::Number(number) => usize::try_from(number)
                .ok()
                .and_then(|number| groups.get(number - 1)),
            GroupKey::Name(name) => groups
                .iter()
                .find(|group| group.name.as_ref().is_some_and(|it| it == name)),
        };
        let group = group.ok_or(NoSuchGroup)?;
        Ok(if group.definitely_set {
            any_str
        } else {
            UnionType::from_two_elements(db, any_str, unset)
        })
    }
}

/// how a `Match` member named the group it wants
#[derive(Debug, Clone, Copy)]
pub(crate) enum GroupKey<'a> {
    Number(u32),
    Name(&'a str),
}

impl fmt::Display for GroupKey<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(number) => write!(f, "{number}"),
            Self::Name(name) => write!(f, "'{name}'"),
        }
    }
}

/// the pattern has no group by that name or number
#[derive(Debug, Clone, Copy)]
pub(crate) struct NoSuchGroup;

/// the `re` module functions and `re.Pattern` methods whose result depends on
/// the pattern's capture groups
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegexCall {
    /// `re.compile` — the groups travel on the returned `Pattern`
    Compile,
    /// `match`, `fullmatch`, `search`, `finditer` — on the returned `Match`
    Match,
    Split,
    FindAll,
    /// `sub` / `subn`, whose result is unaffected: only the `Match` handed to a
    /// callable replacement carries the groups
    Substitute,
}

impl RegexCall {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            // `template` is a pre-3.13 `compile` with a fixed flag
            "compile" | "template" => Self::Compile,
            // `prefixmatch` is the 3.15 name for `match`
            "match" | "prefixmatch" | "fullmatch" | "search" | "finditer" => Self::Match,
            "split" => Self::Split,
            "findall" => Self::FindAll,
            "sub" | "subn" => Self::Substitute,
            _ => return None,
        })
    }
}

/// the `re.Match` members whose result depends on the pattern's capture groups
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchMember {
    /// `group` and `__getitem__`, which select one group (or several)
    Group,
    Groups,
    GroupDict,
    /// `start`, `end` and `span`, which name a group without taking its value —
    /// so only the "no such group" check applies
    Position,
}

impl MatchMember {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "group" | "__getitem__" => Self::Group,
            "groups" => Self::Groups,
            "groupdict" => Self::GroupDict,
            "start" | "end" | "span" => Self::Position,
            _ => return None,
        })
    }
}

/// refine the return type of `call` now that the pattern's groups are known
///
/// `any_str` is the `str`/`bytes` the pattern is over, and `default` the type
/// the stubs gave the call.
pub(crate) fn refined_return<'db>(
    db: &'db dyn Db,
    call: RegexCall,
    groups: RegexGroups<'db>,
    any_str: Type<'db>,
    default: Type<'db>,
) -> Type<'db> {
    match call {
        RegexCall::Compile | RegexCall::Match => attach_groups(db, default, groups),
        RegexCall::Split => {
            // a split yields the text between matches plus every group; a group
            // that did not participate comes back as `None`
            let element = if groups.groups(db).iter().all(|group| group.definitely_set) {
                any_str
            } else {
                UnionType::from_two_elements(db, any_str, Type::none(db))
            };
            KnownClass::List.to_specialized_instance(db, &[element])
        }
        RegexCall::FindAll => {
            // unlike everywhere else, `findall` reports a group that did not
            // participate as the empty string rather than `None`
            let element = match groups.groups(db).len() {
                0 | 1 => any_str,
                count => Type::heterogeneous_tuple(db, std::iter::repeat_n(any_str, count)),
            };
            KnownClass::List.to_specialized_instance(db, &[element])
        }
        RegexCall::Substitute => default,
    }
}

/// the type of `m.group(key)` / `m[key]`
pub(crate) fn group_type<'db>(
    db: &'db dyn Db,
    groups: RegexGroups<'db>,
    any_str: Type<'db>,
    key: GroupKey<'_>,
) -> Result<Type<'db>, NoSuchGroup> {
    groups.resolve(db, any_str, Type::none(db), key)
}

/// the type of `m.groups()`, or of `m.groups(default)` when `unset` is given
pub(crate) fn groups_type<'db>(
    db: &'db dyn Db,
    groups: RegexGroups<'db>,
    any_str: Type<'db>,
    unset: Option<Type<'db>>,
) -> Type<'db> {
    let unset = unset.unwrap_or_else(|| Type::none(db));
    Type::heterogeneous_tuple(
        db,
        groups.groups(db).iter().map(|group| {
            if group.definitely_set {
                any_str
            } else {
                UnionType::from_two_elements(db, any_str, unset)
            }
        }),
    )
}

/// the type of `m.groupdict()`, a `TypedDict` over the pattern's named groups
///
/// a pattern with no named groups keeps the stub's plain `dict`: an empty
/// `TypedDict` would say nothing extra about a dict that is always empty, while
/// costing the caller everything a `dict` can be passed to
pub(crate) fn group_dict_type<'db>(
    db: &'db dyn Db,
    groups: RegexGroups<'db>,
    any_str: Type<'db>,
    unset: Option<Type<'db>>,
) -> Option<Type<'db>> {
    if groups.groups(db).iter().all(|group| group.name.is_none()) {
        return None;
    }
    let unset = unset.unwrap_or_else(|| Type::none(db));
    let items: TypedDictSchema<'db> = groups
        .groups(db)
        .iter()
        .filter_map(|group| {
            let name = group.name.clone()?;
            let declared = if group.definitely_set {
                any_str
            } else {
                UnionType::from_two_elements(db, any_str, unset)
            };
            Some((
                name,
                TypedDictFieldBuilder::new(declared)
                    .required(true)
                    .read_only(false)
                    .build(),
            ))
        })
        .collect();
    // the pattern fixes the key set exactly, so nothing else can be in there
    Some(Type::TypedDict(
        TypedDictType::from_schema_items_with_openness(db, items, TypedDictOpenness::Closed),
    ))
}

/// rewrite every `re.Match` / `re.Pattern` instance inside `ty` to carry `groups`
///
/// this rides the ordinary inductive type mapping, so it reaches the match
/// wherever a stub happens to put it — directly (`Pattern[str]`), under a union
/// (`Match[str] | None`), or nested in another generic (`Iterator[Match[str]]`)
pub(crate) fn attach_groups<'db>(
    db: &'db dyn Db,
    ty: Type<'db>,
    groups: RegexGroups<'db>,
) -> Type<'db> {
    ty.apply_type_mapping(
        db,
        &TypeMapping::AttachRegexGroups(groups),
        TypeContext::default(),
    )
}

/// the `re.Match` / `re.Pattern` instance a value denotes
///
/// narrowing wraps the instance in an intersection (`if m:` gives
/// `Match[str] & ~AlwaysFalsy`), which is exactly the shape a caller reaches
/// these members through, so look through one
fn regex_instance<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<NominalInstanceType<'db>> {
    let candidate = |ty: Type<'db>| {
        let instance = ty.as_nominal_instance()?;
        is_regex_class(instance.known_class(db)).then_some(instance)
    };
    match ty {
        Type::Intersection(intersection) => intersection
            .positive(db)
            .iter()
            .copied()
            .find_map(candidate),
        _ => candidate(ty),
    }
}

/// the statically-known capture groups of a `re.Match` / `re.Pattern` value
pub(crate) fn groups_of<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<RegexGroups<'db>> {
    regex_instance(db, ty)?.regex_groups(db)
}

/// whether a value is a `re.Pattern` rather than a `re.Match`
///
/// this sees through narrowing the same way [`groups_of`] does, so a receiver
/// reached through `if p:` is still recognized as a pattern
pub(crate) fn is_pattern<'db>(db: &'db dyn Db, ty: Type<'db>) -> bool {
    regex_instance(db, ty)
        .is_some_and(|instance| instance.known_class(db) == Some(KnownClass::RePattern))
}

/// the `str`/`bytes` a `re.Match` / `re.Pattern` instance is specialized over
pub(crate) fn any_str_of<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<Type<'db>> {
    regex_instance(db, ty)?
        .class(db)
        .into_generic_alias()?
        .specialization(db)
        .types(db)
        .first()
        .copied()
}

/// whether `class` is one of the two `re` classes that can carry groups
pub(crate) fn is_regex_class(known: Option<KnownClass>) -> bool {
    matches!(known, Some(KnownClass::ReMatch | KnownClass::RePattern))
}

/// the pattern text of a literal `re` pattern argument, with the `str`/`bytes`
/// type the resulting match is over
pub(crate) fn pattern_source<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<(String, Type<'db>)> {
    match ty.as_literal_value_kind()? {
        LiteralValueTypeKind::String(literal) => Some((
            literal.value(db).to_string(),
            KnownClass::Str.to_instance(db),
        )),
        LiteralValueTypeKind::Bytes(literal) => {
            // read the bytes as latin-1 so one byte stays one character, which
            // keeps the offsets in python's own error messages right
            let text = literal.value(db).iter().copied().map(char::from).collect();
            Some((text, KnownClass::Bytes.to_instance(db)))
        }
        _ => None,
    }
}

/// the `re.RegexFlag` members that turn verbose mode on
const VERBOSE_FLAG_NAMES: [&str; 2] = ["X", "VERBOSE"];

/// `re.VERBOSE`'s bit, for a `flags` argument spelled as a plain integer
const VERBOSE_FLAG_BIT: i64 = 64;

/// whether one `flags` operand turns on verbose mode, where we can tell
pub(crate) fn flag_is_verbose<'db>(db: &'db dyn Db, ty: Type<'db>) -> Option<bool> {
    if let Some(literal) = ty.as_enum_literal() {
        let class = literal.enum_class(db);
        return is_regex_flag_class(db, class)
            .then(|| VERBOSE_FLAG_NAMES.contains(&literal.name(db).as_str()));
    }
    ty.as_int_literal()
        .map(|value| value & VERBOSE_FLAG_BIT != 0)
}

fn is_regex_flag_class<'db>(db: &'db dyn Db, class: ClassLiteral<'db>) -> bool {
    class.name(db) == "RegexFlag"
        && file_to_module(db, class.file(db)).and_then(|module| module.known(db))
            == Some(KnownModule::Re)
}

/// the unrefined instance behind two `re` instances of the same class that
/// disagree about their capture groups
///
/// `re.match("(a)", s) if c else re.match("(a)?", s)` describes one set of
/// objects, and neither pattern's groups are true of the union, so it has to
/// collapse rather than accumulate two indistinguishable elements
pub(crate) fn merge_differing_groups<'db>(
    db: &'db dyn Db,
    left: Type<'db>,
    right: Type<'db>,
) -> Option<Type<'db>> {
    let (left, right) = (left.as_nominal_instance()?, right.as_nominal_instance()?);
    if left.regex_groups(db).is_none() && right.regex_groups(db).is_none() {
        return None;
    }
    let class = left.class(db);
    (class == right.class(db)).then(|| Type::instance(db, class))
}
