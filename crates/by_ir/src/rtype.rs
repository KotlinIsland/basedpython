//! runtime types — what the bits look like, as opposed to what the checker knows
//!
//! an [`RType`] is a *representation*. `list[int]`, `list[str]` and `list[T]` all
//! erase to one `RType`, because they are all a `PyObject *` pointing at a list.
//! the representation invariant is that compiled code may assume, without
//! checking, that a register holds a value matching its `RType` — so narrowing to
//! a more precise one always needs a proof or an inserted check.

use std::fmt;

/// the primitive representations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Primitive {
    /// `PyObject *` — the top of the lattice, anything at all
    Object,
    /// a tagged integer: even means the value shifted left by one, odd means a
    /// pointer to a `PyLongObject`. arbitrary precision is preserved
    Int,
    /// a fixed-width native integer, used only where a range proves it fits
    Fixed(IntWidth),
    /// an unboxed `double`. in `.by`, `float` excludes `int`, so this needs no
    /// int-check guard
    Float,
    /// a `char` holding 0 or 1
    Bool,
    /// a comparison result: 0 or 1, and never an error value
    Bit,
    /// zero-width — only the static type carries the information
    None,
    /// a `str`, with a known object layout
    Str,
    /// a `bytes`, with a known object layout
    Bytes,
    /// a `list`, with a known object layout
    List,
    /// a `dict`, with a known object layout
    Dict,
    /// a `tuple` of unknown length. fixed-length tuples are [`RType::Tuple`]
    Tuple,
}

/// the width and signedness of a [`Primitive::Fixed`]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum IntWidth {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntWidth {
    /// the C type this width is emitted as
    pub const fn ctype(self) -> &'static str {
        match self {
            Self::I8 => "int8_t",
            Self::I16 => "int16_t",
            Self::I32 => "int32_t",
            Self::I64 => "int64_t",
            Self::U8 => "uint8_t",
            Self::U16 => "uint16_t",
            Self::U32 => "uint32_t",
            Self::U64 => "uint64_t",
        }
    }

    /// whether this width can represent negative values
    pub const fn is_signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    /// the inclusive range of values this width holds
    pub const fn range(self) -> (i128, i128) {
        match self {
            Self::I8 => (-128, 127),
            Self::I16 => (-32_768, 32_767),
            Self::I32 => (-2_147_483_648, 2_147_483_647),
            Self::I64 => (i64::MIN as i128, i64::MAX as i128),
            Self::U8 => (0, 255),
            Self::U16 => (0, 65_535),
            Self::U32 => (0, 4_294_967_295),
            Self::U64 => (0, u64::MAX as i128),
        }
    }

    /// the narrowest width holding every value in `lo..=hi`, if any does
    pub fn fitting(lo: i128, hi: i128) -> Option<Self> {
        [
            Self::U8,
            Self::I8,
            Self::U16,
            Self::I16,
            Self::U32,
            Self::I32,
            Self::U64,
            Self::I64,
        ]
        .into_iter()
        .find(|width| {
            let (min, max) = width.range();
            min <= lo && hi <= max
        })
    }
}

impl fmt::Display for IntWidth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
        };
        f.write_str(name)
    }
}

/// a runtime type
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RType {
    Primitive(Primitive),
    /// a fixed-length tuple, laid out as a C struct with no object header
    Tuple(Box<[RType]>),
    /// a `list` whose elements are stored **unboxed**, in a buffer of its own
    /// rather than as a `PyObject *` each.
    ///
    /// the buffer carries its own reference count, so it retains and releases the
    /// way everything else here does — which is what keeps it inside the ownership
    /// discipline the verifier already checks, rather than beside it.
    ///
    /// it is internal to a compilation unit: reaching python means `Box`, which
    /// builds a real `list`. that is why nothing else has to know about it —
    /// anything wanting a `PyObject *` gets one, and the verifier rejects a lowering
    /// that skips the step
    Array(Box<RType>),
    /// an instance of a native class, by name.
    ///
    /// there is deliberately no C type here: what an instance is emitted as depends
    /// on which classes the *module* lays out, so only the emitter can say
    ///
    /// `exact` means the runtime class is exactly this one and not a subclass,
    /// which is what licenses a direct call and a pinned instance size. it comes
    /// from a `@final` class or a use-site `final T`
    Instance {
        class: String,
        exact: bool,
    },
}

impl RType {
    /// every native class this representation names, outermost first
    pub fn instance_classes(&self) -> Vec<&str> {
        match self {
            Self::Instance { class, .. } => vec![class.as_str()],
            Self::Tuple(items) => items.iter().flat_map(Self::instance_classes).collect(),
            Self::Array(element) => element.instance_classes(),
            Self::Primitive(_) => Vec::new(),
        }
    }

    pub const OBJECT: Self = Self::Primitive(Primitive::Object);
    pub const INT: Self = Self::Primitive(Primitive::Int);
    pub const FLOAT: Self = Self::Primitive(Primitive::Float);
    pub const BOOL: Self = Self::Primitive(Primitive::Bool);
    pub const BIT: Self = Self::Primitive(Primitive::Bit);
    pub const NONE: Self = Self::Primitive(Primitive::None);
    pub const STR: Self = Self::Primitive(Primitive::Str);
    pub const LIST: Self = Self::Primitive(Primitive::List);

    pub const fn fixed(width: IntWidth) -> Self {
        Self::Primitive(Primitive::Fixed(width))
    }

    /// whether the value is represented without a `PyObject` header
    pub fn is_unboxed(&self) -> bool {
        match self {
            Self::Primitive(primitive) => !matches!(
                primitive,
                Primitive::Object
                    | Primitive::Str
                    | Primitive::Bytes
                    | Primitive::List
                    | Primitive::Dict
                    | Primitive::Tuple
            ),
            Self::Tuple(_) => true,
            // the buffer has no `PyObject` header of its own, which is the point
            Self::Array(_) => true,
            // an instance is a real object with a header
            Self::Instance { .. } => false,
        }
    }

    /// whether a register of this type owns a reference that must be released.
    ///
    /// a tagged `int` is refcounted: the small-value case is a bare word, but the
    /// overflow case is a real `PyLongObject *`, so the emitter cannot know
    /// statically which it holds
    pub fn is_refcounted(&self) -> bool {
        match self {
            Self::Primitive(primitive) => !matches!(
                primitive,
                Primitive::Fixed(_)
                    | Primitive::Float
                    | Primitive::Bool
                    | Primitive::Bit
                    | Primitive::None
            ),
            Self::Tuple(items) => items.iter().any(Self::is_refcounted),
            // the buffer is owned, and owning is what this asks
            Self::Array(_) | Self::Instance { .. } => true,
        }
    }

    /// whether this type's error sentinel is also a valid value, so that an error
    /// has to be confirmed with `PyErr_Occurred()`.
    ///
    /// no fixed-width integer has a spare bit pattern to reserve, and a `double`
    /// could legitimately be any bit pattern too
    pub fn error_overlaps(&self) -> bool {
        matches!(
            self,
            Self::Primitive(Primitive::Fixed(_) | Primitive::Float | Primitive::Bool)
        )
    }

    /// the value a register of this type holds before it is assigned, and the
    /// value a fallible function returns on the error path
    pub fn undefined(&self) -> String {
        match self {
            Self::Array(_) => "NULL".to_string(),
            Self::Primitive(primitive) => match primitive {
                Primitive::Object
                | Primitive::Str
                | Primitive::Bytes
                | Primitive::List
                | Primitive::Dict
                | Primitive::Tuple => "NULL",
                Primitive::Int => "BY_INT_ERROR",
                Primitive::Fixed(_) => "-113",
                Primitive::Float => "BY_FLOAT_ERROR",
                Primitive::Bool | Primitive::Bit | Primitive::None => "2",
            }
            .to_string(),
            // each member takes its own undefined value rather than a zero, so that
            // the members which reserve a bit pattern carry it here too — that is
            // what [`Self::error_sentinel`] reads back. every refcounted member is
            // still one the release discipline accepts: `NULL` for a pointer, and
            // `BY_INT_ERROR` for a tagged `int`, which `By_DecRefTagged` guards
            Self::Tuple(items) if !items.is_empty() => format!(
                "{{ {} }}",
                items
                    .iter()
                    .map(Self::undefined)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Tuple(_) => "{0}".to_string(),
            Self::Instance { .. } => "NULL".to_string(),
        }
    }

    /// where inside a value of this type an error can be read back off the value
    /// itself: the field path to the member holding the sentinel, and the pattern
    /// that member reserves
    ///
    /// a struct has no spare bit pattern of its own, so a fixed-length tuple borrows
    /// one from the first member that reserves one. that is what lets a call
    /// returning a pair be checked with a compare instead of by asking the thread
    /// whether an exception is set — and the compare is not merely the cheaper of
    /// the two. `PyErr_Occurred` is an opaque call, so a C compiler has to assume it
    /// clobbers everything and stops inlining the callee into the loop around it
    ///
    /// `None` where nothing can be read back — an empty tuple, or one whose members
    /// are all `double`s and fixed-width integers — and the caller falls back to the
    /// thread's exception state
    pub fn error_sentinel(&self) -> Option<(String, String)> {
        match self {
            Self::Tuple(items) => items.iter().enumerate().find_map(|(index, item)| {
                let (path, pattern) = item.error_sentinel()?;
                Some((format!(".f{index}{path}"), pattern))
            }),
            _ if self.error_overlaps() => None,
            _ => Some((String::new(), self.undefined())),
        }
    }
}

/// a stable, C-identifier-safe suffix naming a fixed-length tuple's layout, so
/// that two structurally identical tuples share one emitted struct
pub fn tuple_mangle(items: &[RType]) -> String {
    let mut out = String::new();
    for item in items {
        out.push('_');
        out.push_str(&mangle_component(item));
    }
    out
}

fn mangle_component(ty: &RType) -> String {
    match ty {
        RType::Array(element) => format!("arr{}", mangle_component(element)),
        RType::Primitive(primitive) => match primitive {
            Primitive::Object => "obj".to_string(),
            Primitive::Int => "int".to_string(),
            Primitive::Fixed(width) => width.to_string(),
            Primitive::Float => "float".to_string(),
            Primitive::Bool => "bool".to_string(),
            Primitive::Bit => "bit".to_string(),
            Primitive::None => "none".to_string(),
            Primitive::Str => "str".to_string(),
            Primitive::Bytes => "bytes".to_string(),
            Primitive::List => "list".to_string(),
            Primitive::Dict => "dict".to_string(),
            Primitive::Tuple => "tuple".to_string(),
        },
        RType::Tuple(items) => format!("t{}", tuple_mangle(items)),
        RType::Instance { class, .. } => format!("i{class}"),
    }
}

impl fmt::Display for RType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Array(element) => write!(f, "[{element}]"),
            Self::Primitive(primitive) => {
                let name = match primitive {
                    Primitive::Object => "object",
                    Primitive::Int => "int",
                    Primitive::Fixed(width) => return write!(f, "{width}"),
                    Primitive::Float => "float",
                    Primitive::Bool => "bool",
                    Primitive::Bit => "bit",
                    Primitive::None => "None",
                    Primitive::Str => "str",
                    Primitive::Bytes => "bytes",
                    Primitive::List => "list",
                    Primitive::Dict => "dict",
                    Primitive::Tuple => "tuple",
                };
                f.write_str(name)
            }
            Self::Instance { class, exact } => {
                if *exact {
                    f.write_str("final ")?;
                }
                f.write_str(class)
            }
            Self::Tuple(items) => {
                f.write_str("(")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str(")")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unboxed_types_have_no_object_header() {
        assert!(RType::INT.is_unboxed());
        assert!(RType::FLOAT.is_unboxed());
        assert!(RType::fixed(IntWidth::U8).is_unboxed());
        assert!(!RType::OBJECT.is_unboxed());
        assert!(!RType::STR.is_unboxed());
    }

    #[test]
    fn an_array_owns_its_buffer_like_anything_else() {
        // the point of giving the buffer its own count: it retains and releases the
        // same way, so the ownership discipline applies to it unchanged rather than
        // needing a second kind of owned thing
        let array = RType::Array(Box::new(RType::fixed(IntWidth::I64)));
        assert!(array.is_refcounted());
        assert!(array.is_unboxed());
        assert_eq!(array.undefined(), "NULL");
    }

    #[test]
    fn an_array_names_the_classes_its_elements_name() {
        let array = RType::Array(Box::new(RType::Instance {
            class: "Vec2".to_string(),
            exact: false,
        }));
        assert_eq!(array.instance_classes(), ["Vec2"]);
    }

    #[test]
    fn tagged_int_is_refcounted_but_fixed_width_is_not() {
        // the overflow case of a tagged int is a real PyLongObject
        assert!(RType::INT.is_refcounted());
        assert!(!RType::fixed(IntWidth::I64).is_refcounted());
        assert!(!RType::FLOAT.is_refcounted());
        assert!(RType::OBJECT.is_refcounted());
    }

    #[test]
    fn a_tuple_is_refcounted_when_any_element_is() {
        let plain = RType::Tuple(Box::new([RType::FLOAT, RType::BOOL]));
        let boxed = RType::Tuple(Box::new([RType::FLOAT, RType::STR]));
        assert!(!plain.is_refcounted());
        assert!(boxed.is_refcounted());
    }

    #[test]
    fn error_overlaps_exactly_where_no_sentinel_is_reservable() {
        assert!(RType::fixed(IntWidth::I64).error_overlaps());
        assert!(RType::FLOAT.error_overlaps());
        // a tagged int reserves a bit pattern, and a pointer reserves NULL
        assert!(!RType::INT.error_overlaps());
        assert!(!RType::OBJECT.error_overlaps());
    }

    #[test]
    fn a_tuple_borrows_its_error_sentinel_from_the_first_member_that_has_one() {
        let pair = RType::Tuple(Box::new([RType::INT, RType::INT]));
        assert_eq!(
            pair.error_sentinel(),
            Some((".f0".to_string(), "BY_INT_ERROR".to_string()))
        );
        // a `double` reserves nothing, so the sentinel comes from further along
        let mixed = RType::Tuple(Box::new([RType::FLOAT, RType::STR]));
        assert_eq!(
            mixed.error_sentinel(),
            Some((".f1".to_string(), "NULL".to_string()))
        );
        let nested = RType::Tuple(Box::new([
            RType::FLOAT,
            RType::Tuple(Box::new([RType::fixed(IntWidth::I64), RType::INT])),
        ]));
        assert_eq!(
            nested.error_sentinel(),
            Some((".f1.f1".to_string(), "BY_INT_ERROR".to_string()))
        );
    }

    #[test]
    fn a_tuple_with_nothing_to_reserve_has_no_error_sentinel() {
        let scalars = RType::Tuple(Box::new([RType::FLOAT, RType::fixed(IntWidth::I64)]));
        assert_eq!(scalars.error_sentinel(), None);
        assert_eq!(RType::Tuple(Box::new([])).error_sentinel(), None);
    }

    #[test]
    fn a_tuple_is_undefined_member_by_member() {
        assert_eq!(
            RType::Tuple(Box::new([RType::INT, RType::STR])).undefined(),
            "{ BY_INT_ERROR, NULL }"
        );
        // an empty struct has no member to name, so the zero initializer stands
        assert_eq!(RType::Tuple(Box::new([])).undefined(), "{0}");
    }

    #[test]
    fn fitting_picks_the_narrowest_width() {
        assert_eq!(IntWidth::fitting(0, 255), Some(IntWidth::U8));
        assert_eq!(IntWidth::fitting(0, 256), Some(IntWidth::U16));
        assert_eq!(IntWidth::fitting(-1, 127), Some(IntWidth::I8));
        assert_eq!(IntWidth::fitting(-1, 128), Some(IntWidth::I16));
        assert_eq!(
            IntWidth::fitting(0, i128::from(u64::MAX)),
            Some(IntWidth::U64)
        );
        assert_eq!(IntWidth::fitting(-1, i128::from(u64::MAX)), None);
    }

    #[test]
    fn structurally_equal_tuples_mangle_the_same() {
        let a = [RType::INT, RType::STR];
        let b = [RType::INT, RType::STR];
        assert_eq!(tuple_mangle(&a), tuple_mangle(&b));
        assert_ne!(tuple_mangle(&a), tuple_mangle(&[RType::STR, RType::INT]));
    }

    #[test]
    fn display_round_trips_the_shapes_the_printer_emits() {
        assert_eq!(RType::INT.to_string(), "int");
        assert_eq!(RType::fixed(IntWidth::U8).to_string(), "u8");
        assert_eq!(
            RType::Tuple(Box::new([RType::INT, RType::FLOAT])).to_string(),
            "(int, float)"
        );
    }
}
