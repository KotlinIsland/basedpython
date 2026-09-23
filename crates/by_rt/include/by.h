/* basedpython native runtime
 *
 * header-only on purpose: every operation here is a few instructions whose whole
 * value is that it inlines into generated code, and a separately compiled library
 * could only inline through cross-language LTO. see
 * docs/basedpython/development/compilation/technology.md
 *
 * ## ownership
 *
 * a helper's operands are **borrowed**. the calling frame owns every register it
 * passes and releases it on each of its own exit paths, so a helper neither
 * retains an operand for the caller nor releases one on its behalf — except where
 * its comment says outright that it consumes one, as `By_StrAppend` does.
 *
 * that cuts both ways when a value is handed on to the interpreter. an api that
 * takes its own reference — `PyErr_SetObject`, `PyList_Append` — needs none added
 * here, and one that *steals* — `PyErr_SetExcInfo`, `PyException_SetCause` — must
 * be given a new one rather than the operand. getting the first wrong leaks a
 * reference per call and nothing else, which is why it survived so long: it was a
 * retain in `By_Reraise`, and it cost a `GeneratorExit` per abandoned generator
 * and the thrown exception per `throw`
 */

#ifndef BY_RT_H
#define BY_RT_H

#define PY_SSIZE_T_CLEAN
#include <Python.h>
/* `PyMarshal_ReadObjectFromString`, which reads the interpreted twin's code object
 * back. `Python.h` does not pull this one in */
#include <marshal.h>
/* `PyFrame_New`, which a traceback entry for a compiled frame is hung off. `Python.h`
 * does not pull this one in either */
#include <frameobject.h>
#include <string.h>
#include <math.h>
#include <errno.h>
#include <stdint.h>
#include <stddef.h>
#include <stdarg.h>

/* the floor, restated for the compiler
 *
 * `by compile` refuses an older interpreter before it emits anything — the floor it
 * checks is `by_build::MINIMUM_PYTHON`, which is where the number is decided. this is
 * for a compile that did not come through it: the emitted C names things an older
 * cpython has no declaration for, so the alternative here is dozens of errors none of
 * which mentions a version */
#if PY_VERSION_HEX < 0x030B0000
#error "a basedpython extension needs python 3.11 or later"
#endif

/* 3.13 made the fastcall-with-keywords function type public; below it the same type is
 * only spelled with a leading underscore */
#if PY_VERSION_HEX < 0x030D0000
typedef _PyCFunctionFastWithKeywords PyCFunctionFastWithKeywords;
#endif

/* the major and minor at the front of a cpython version string
 *
 * `Py_GetVersion` answers the whole banner — `"3.14.0a1 (main, ...) [Clang ...]"` — and
 * only its first two numbers are wanted. anything that does not begin with two
 * dot-separated runs of digits leaves both at -1, which no build matches, so an
 * unreadable banner is refused exactly as a mismatched one is */
static inline void By_ParseVersion(const char *version, int *major, int *minor) {
    const char *at = version;
    int read_major = 0;
    int read_minor = 0;
    *major = -1;
    *minor = -1;
    if (version == NULL || *at < '0' || *at > '9') return;
    while (*at >= '0' && *at <= '9') read_major = read_major * 10 + (*at++ - '0');
    if (*at != '.' || at[1] < '0' || at[1] > '9') return;
    at++;
    while (*at >= '0' && *at <= '9') read_minor = read_minor * 10 + (*at++ - '0');
    *major = read_major;
    *minor = read_minor;
}

/* does the interpreter that is running match the one this module was built against?
 *
 * every `PY_VERSION_HEX` branch in this header is decided by the headers the build
 * compiled against, so an artefact loaded by a different minor version runs branches
 * written for a layout that interpreter does not have. that is a crash rather than a
 * wrong answer, and nothing upstream of here refuses it: the version tag lives in the
 * *file name*, and a bare `.so` — which every 3.x lists in `EXTENSION_SUFFIXES` — is
 * offered to whatever is running. so an artefact renamed, or copied out of a wheel built
 * elsewhere, reaches module init with no check having happened.
 *
 * `Py_GetVersion` rather than `Py_Version`: it is the one of the two that every version
 * this header compiles against exports, and a module built against newer headers naming
 * a symbol the running interpreter lacks is the same failure by another road */
static inline int By_InterpreterMatches(void) {
    int major;
    int minor;
    By_ParseVersion(Py_GetVersion(), &major, &minor);
    if (major == PY_MAJOR_VERSION && minor == PY_MINOR_VERSION) return 1;
    PyErr_Format(PyExc_ImportError,
                 "this module was compiled for python %d.%d, and python %d.%d is running",
                 PY_MAJOR_VERSION, PY_MINOR_VERSION, major, minor);
    return 0;
}

/* ── tagged integers ──────────────────────────────────────────────────────────
 *
 * a ByTagged is a pointer-sized word. an even value is a "short": the integer
 * shifted left by one. an odd value is a PyLongObject pointer with the low bit
 * set. python's arbitrary precision is preserved — the tag is only a fast path.
 *
 * a short is always an exact `int`. a pointer is either an exact `int` too wide to be
 * a short, or an instance of an `int` subclass — `True`, or a class of the program's
 * own — whatever its value, because that object is what python hands back and what
 * python asks for its methods. so a pointer does *not* mean a value outside the short
 * range: a fast path may conclude that only from a pointer to an exact `int`
 */

typedef size_t ByTagged;

#define BY_INT_TAG ((ByTagged)1)
/* a tagged word of 1 is a pointer of 0 with the tag set, so it can never be a
 * real object and is free to mean "an exception is set" */
#define BY_INT_ERROR ((ByTagged)1)
/* a tagged word holding no reference, which a short zero is: releasing it takes the
 * test the straight line already passes rather than the branch kept for a real
 * PyLongObject.
 *
 * emitted code pointedly does *not* empty a register with this — it writes the error
 * sentinel the register was declared with, so that one constant covers the register's
 * whole lifetime and clang folds the later release away outright. see
 * `RType::undefined` for the measurement. what is left here is the one place a "holds
 * nothing" that is never compared against the sentinel is wanted: the initializer of
 * an unboxed global's memo, which is cold until its generation is stamped */
#define BY_INT_EMPTY ((ByTagged)0)
/* a float error sentinel overlaps a valid value, so an error must be confirmed
 * with PyErr_Occurred() — see RType::error_overlaps */
#define BY_FLOAT_ERROR (-113.0)

#define BY_SHORT_MAX (PY_SSIZE_T_MAX >> 1)
#define BY_SHORT_MIN (-BY_SHORT_MAX - 1)

/* the compiler decides which integer literals are short when it writes the C, and it
 * decides against this bound. a narrower word would take a literal the compiler called
 * short and silently drop its top bits */
_Static_assert(BY_SHORT_MAX == 4611686018427387903LL,
               "the emitted C sorts integer literals against a 64-bit short range");

/* a tagged integer is a machine word until it is not, and the not is rare — every
 * loop counter, index and accumulator in a real program stays short. telling the
 * compiler so is what keeps the slow path out of the straight line */
#if defined(__GNUC__) || defined(__clang__)
#define BY_LIKELY(x) __builtin_expect(!!(x), 1)
#define BY_UNLIKELY(x) __builtin_expect(!!(x), 0)
#else
#define BY_LIKELY(x) (x)
#define BY_UNLIKELY(x) (x)
#endif

/* telling the compiler which half of a tagged operation is which
 *
 * a C compiler prices an inline candidate by its whole body, cold blocks
 * included, and the cold half of a tagged operation is a python-level call with
 * error handling around it. so `By_IntAdd`, whose fast path is three
 * instructions, is priced as if it were its slow path — and once it has been
 * inlined into a compiled function, that function is over the threshold too and
 * stops being inlinable into *its* caller. a compiled `def add(a, b): return
 * a + b` was left as a real call from the loop that used it, with a full
 * prologue and epilogue around one add.
 *
 * the two attributes below say what the cost model cannot work out on its own:
 * a slow path is never worth inlining, and a fast path always is. the same
 * split is what a runtime that compiles its slow paths into a separate object
 * file gets for free.
 *
 * `BY_HOT` belongs only on a body that is genuinely a handful of instructions
 * once its slow path is out of line, because it removes the compiler's judgement
 * rather than informing it */
#if defined(__GNUC__) || defined(__clang__)
#define BY_HOT __attribute__((always_inline)) static inline
#define BY_COLD __attribute__((noinline)) static
#else
#define BY_HOT static inline
#define BY_COLD static
#endif

/* ── retaining at the width the release reads ─────────────────────────────────
 *
 * python 3.12 split `ob_refcnt` in two so an immortal object could be recognised
 * from the sign of its low half, and the two halves of a retain/release pair have
 * disagreed on access width ever since. `Py_INCREF` reads and writes
 * `ob_refcnt_split[PY_BIG_ENDIAN]`, a 32-bit field; `Py_DECREF` tests immortality
 * over the full `Py_ssize_t` and then decrements it. a narrow store followed by a
 * wider load of the same address cannot be served from the store buffer, so every
 * retain a compiled function makes stalls the release after it until the store has
 * reached L1. emitted modules are little else *but* retain/release pairs — one
 * small module carried 82 of these 32-bit stores — so it is paid everywhere.
 *
 * so retain at 64 bits and keep the immortality check, asking for it with
 * `_Py_IsImmortal`, which is the predicate `Py_DECREF` itself uses. that is the
 * point: both halves of the pair now test the same bits the same way, and the two
 * compile to the same instruction. this is not an invention but cpython's own
 * `#else` arm of `Py_INCREF` — the one a 32-bit host takes — selected on a 64-bit
 * host because the split it exists to avoid is exactly what costs us.
 *
 * that it is safe rests on one property rather than on the arithmetic matching:
 * this skips the increment on a *superset* of the objects the split form skips,
 * and every object in the difference is one `Py_DECREF` also declines to touch. so
 * a reference can never be dropped that would otherwise have been held, and an
 * immortal sentinel is never written. (an object past 2^31 live references is
 * leaked instead of counted — but it already is upstream, by the same sign test.)
 *
 * mypyc goes further and drops the check outright, as `op->ob_refcnt++`. that is
 * only sound for objects it has *proved* mortal, because a 64-bit increment of an
 * immortal refcount corrupts the sentinel, and we have no such proof.
 *
 * the fast path is opt-in behind a positive test of every name it uses. a spelling
 * missing in some configuration must not fail to compile *every* module, so
 * anything unrecognised falls back to the ordinary macros, which are always right:
 *
 *   - `Py_GIL_DISABLED` — a free-threaded build counts in `ob_ref_local` and
 *     `ob_ref_shared`, a different layout with no split to match
 *   - `Py_REF_DEBUG`, `Py_TRACE_REFS`, `Py_STATS` — the macros keep books here
 *   - `Py_LIMITED_API` — there `Py_INCREF` is a call, by design
 *   - `SIZEOF_VOID_P > 4` — a 32-bit host has no split, so no mismatch
 *   - 3.12 and 3.13 alone — 3.11 predates the split, and 3.14 removed it again:
 *     its `Py_INCREF` already stores the whole `ob_refcnt`, so there is nothing
 *     left to match and taking this path would only risk a layout we did not read
 *   - `_Py_IsImmortal`, which is not stable api and moved header between versions
 *
 * where the fast path is declined these are exactly `Py_INCREF`/`Py_XINCREF`, so no
 * caller has to know which it got. the release side needs no counterpart: it reads
 * and writes the full width already, which is the half of the pair that was right.
 *
 * defining `BY_NO_WIDE_INCREF` forces the fallback. it is the escape hatch for a
 * build that meets something none of the tests above anticipated, and it is also
 * how the two legs of the disassembly check are taken from one emitted module */
#if !defined(BY_NO_WIDE_INCREF) \
    && PY_VERSION_HEX >= 0x030C0000 && PY_VERSION_HEX < 0x030E0000 \
    && defined(_Py_IsImmortal) \
    && !defined(Py_GIL_DISABLED) && !defined(Py_LIMITED_API) \
    && !defined(Py_REF_DEBUG) && !defined(Py_TRACE_REFS) && !defined(Py_STATS) \
    && defined(SIZEOF_VOID_P) && SIZEOF_VOID_P > 4
#define BY_WIDE_INCREF 1
#endif

BY_HOT void By_IncRefObject(PyObject *o) {
#ifdef BY_WIDE_INCREF
    if (!_Py_IsImmortal(o)) o->ob_refcnt++;
#else
    Py_INCREF(o);
#endif
}

BY_HOT void By_XIncRefObject(PyObject *o) {
    if (o != NULL) By_IncRefObject(o);
}

/* an emitted register holding an instance is typed as that class's own struct, so
 * these take the cast `Py_XINCREF` takes for the same reason */
#define By_IncRef(op) By_IncRefObject((PyObject *)(op))
#define By_XIncRef(op) By_XIncRefObject((PyObject *)(op))

BY_HOT int By_IsShort(ByTagged x) { return (x & BY_INT_TAG) == 0; }

BY_HOT Py_ssize_t By_ShortValue(ByTagged x) { return ((Py_ssize_t)x) >> 1; }

BY_HOT ByTagged By_ShortFrom(Py_ssize_t v) {
    return (ByTagged)((size_t)v << 1);
}

BY_HOT int By_FitsShort(Py_ssize_t v) {
    return v >= BY_SHORT_MIN && v <= BY_SHORT_MAX;
}

BY_HOT PyObject *By_LongOf(ByTagged x) {
    return (PyObject *)(x & ~BY_INT_TAG);
}

/* whether a tagged integer holds exactly an `int` rather than a subclass: a short always
 * does, and a pointer is asked its type */
static inline char By_IsExactInt(ByTagged x) {
    return (char)(By_IsShort(x) || PyLong_CheckExact(By_LongOf(x)));
}

/* `By_DecRefTagged` and `By_IncRefTagged` release the sentinel by handing it to
 * `By_LongOf` and letting the X-forms drop the NULL, rather than comparing against
 * `BY_INT_ERROR` a second time. that only works while the sentinel carries no object
 * bits — give it any other odd value and those two would hand a bogus pointer to
 * `Py_DECREF` on every error path, silently, in every compiled module */
_Static_assert((BY_INT_ERROR & ~BY_INT_TAG) == 0,
               "BY_INT_ERROR must be the tag bit alone, or the tagged refcount "
               "helpers would release a bogus pointer");

/* borrow-free: returns a new reference */
static inline PyObject *By_BoxInt(ByTagged x) {
    if (By_IsShort(x)) {
        return PyLong_FromSsize_t(By_ShortValue(x));
    }
    PyObject *o = By_LongOf(x);
    By_IncRef(o);
    return o;
}

/* the same, for a tagged value the caller owns and is done with
 *
 * a heap `int` is handed straight on: the word held one reference and the `PyObject *`
 * that replaces it is that same reference, so there is nothing to take and nothing to
 * give back. a short owns nothing, and the object is built exactly as above.
 *
 * boxing an owned tagged value with [`By_BoxInt`] instead is a retain the caller never
 * releases — which is what made every compiled `-> int` entry leak one reference per
 * call for a value too wide to be a short */
static inline PyObject *By_BoxIntOwned(ByTagged x) {
    if (By_IsShort(x)) {
        return PyLong_FromSsize_t(By_ShortValue(x));
    }
    return By_LongOf(x);
}

/* an integer literal too wide to be a short, which the module built once at init
 *
 * the tag is added and no reference is taken: like a string literal, the module owns
 * it and a use of it is borrowed. the value lies outside the short range by
 * construction, which is what a pointer to an exact `int` always holds */
BY_HOT ByTagged By_TaggedLiteral(PyObject *o) {
    return ((ByTagged)(void *)o) | BY_INT_TAG;
}

/* `type(o).__name__` as an f-string writes it, which is how the transpiled build's
 * soundness check names what it was handed. NULL with the error set where asking raised */
BY_COLD PyObject *By_TypeNameOf(PyObject *o) {
    PyObject *name = PyObject_GetAttrString((PyObject *)Py_TYPE(o), "__name__");
    PyObject *text;
    if (name == NULL) return NULL;
    text = PyObject_Format(name, NULL);
    Py_DECREF(name);
    return text;
}

/* a refusal a hot narrowing reaches only when it fails
 *
 * `noinline` alone is not enough for these: a call to a static function inside an inline
 * narrowing such as `By_UnboxInt` is priced as if it might run, and `globals_` took 5%
 * more instructions for it. `cold` says it will not */
#if defined(__GNUC__) || defined(__clang__)
#define BY_REFUSAL __attribute__((cold)) BY_COLD
#else
#define BY_REFUSAL BY_COLD
#endif

/* the `TypeError` a narrowing raises for a value that is not the `expected` builtin,
 * worded as the transpiled build's soundness check words the same refusal: the checks
 * sit in the same places, so the two builds raise one error rather than two */
BY_REFUSAL void By_TypeError(const char *expected, PyObject *got) {
    PyObject *name;
    if (got == NULL) {
        PyErr_Format(PyExc_TypeError, "type soundness violation: expected %s, got NULL",
                     expected);
        return;
    }
    name = By_TypeNameOf(got);
    if (name == NULL) return;
    PyErr_Format(PyExc_TypeError, "type soundness violation: expected %s, got %U", expected,
                 name);
    Py_DECREF(name);
}

/* the attribute `name` of `o`, or NULL with nothing raised where it has none */
static inline int By_OptionalAttr(PyObject *o, PyObject *name, PyObject **found) {
#if PY_VERSION_HEX >= 0x030D0000
    return PyObject_GetOptionalAttr(o, name, found);
#else
    return _PyObject_LookupAttr(o, name, found);
#endif
}

/* ── soundness checks ──────────────────────────────────────────────────────────
 *
 * where a value whose type nothing verified meets a declared one, the transpiled build
 * checks it with `_soundness_check(value, target)`, and a compiled module makes the
 * same check in the same place. these are that function's two halves */

#ifndef Py_GIL_DISABLED
/* refusals worked out earlier: a type, the version its attributes and bases had, and a
 * class it does not derive from. the lookup and the walk that reach that answer cost a
 * hundred instructions between them, and are asked again on every read of an optional and
 * at every arm of a `match` a subject falls past.
 *
 * a write to the type's dict, to a base's, or to its `__bases__` moves the version, so a
 * match is the answer both would give. the class is not held: a class the type does not
 * derive from is not in its mro, and one arriving at the same address can only enter the
 * mro through `__bases__`, which moves the version too.
 *
 * there are many entries because one is not how the question is asked. a `match` ladder
 * walks a subject past every arm before the one that takes it, so a single entry is
 * overwritten by the next arm before the next subject reaches the first one again — and
 * two subject types alternating do the same to each other. one entry held three pairs in
 * the `match_` benchmark and answered two of every five questions; the entry a pair takes
 * here is settled by the pair alone, so a pair asked twice with anything in between still
 * finds its own answer, and only another pair landing on the same entry displaces it.
 * that row goes 12.21M instructions to 10.44M, a seventh of the whole row.
 *
 * the count is what makes that steady rather than lucky. the table is direct-mapped, so
 * two pairs of the moment can land on one entry and take it from each other, and which
 * pairs those are is decided by where a run's allocator put two type objects rather than
 * by anything the program did. sixteen entries measured *bimodally* on that row — 10.44M
 * on a run where its three pairs fell apart and 11.66M on one where two met — and
 * sixty-four measured 10.44M every time. the table costs its own size in a module's
 * uninitialised data and nothing else, and being wider is the whole of what buys the
 * steadiness: a mix that carries the pointers' high bits down instead was measured too,
 * and its multiplies cost more than the spreading saved.
 *
 * this is a cache with a validity test rather than an assumption: a hit is checked
 * against the type's current version, and a miss only costs the walk that would have
 * happened anyway.
 *
 * the free-threaded build has none of this. the entries are read and written without
 * synchronisation, which is sound only because the thread holding the GIL is the only
 * one running */
#define BY_REFUSED_SLOTS 64

typedef struct {
    PyTypeObject *type;
    PyObject *class_;
    unsigned int version;
} ByRefusal;

static ByRefusal by_refusals[BY_REFUSED_SLOTS];

/* where a pair's answer lives
 *
 * both pointers are shifted past the bits an allocator's alignment holds fixed at zero,
 * and by different amounts, so that neither a type asked about itself nor two pairs
 * sharing a member folds to one entry */
static inline size_t By_RefusalSlot(PyTypeObject *type, PyObject *cls) {
    uintptr_t mixed = ((uintptr_t)type >> 4) ^ ((uintptr_t)cls >> 9);
    return (size_t)(mixed & (BY_REFUSED_SLOTS - 1));
}
#endif

/* `isinstance(o, cls)` for an object that is not exactly of class `cls`
 *
 * a class whose own class is `type` has no `__instancecheck__` to ask, and python's answer
 * for it is whether `o`'s type derives from `cls`, or failing that whether `o.__class__`
 * does. while `o`'s type reads attributes the generic way and its `__class__` is
 * `object`'s own descriptor, that second question answers `type(o)` again and runs nothing,
 * so the answer is the first one alone. this is the refusal an optional takes for `None`
 * on every read, and the call it saves looks `__class__` up and calls its getter */
static int By_SoundIsOther(PyObject *o, PyObject *cls) {
    static PyObject *dunder_class = NULL;
    static PyObject *object_class = NULL;
    PyTypeObject *type = Py_TYPE(o);
    if (!Py_IS_TYPE(cls, &PyType_Type) || type->tp_getattro != PyObject_GenericGetAttr) {
        return PyObject_IsInstance(o, cls);
    }
#ifndef Py_GIL_DISABLED
    ByRefusal *refusal = &by_refusals[By_RefusalSlot(type, cls)];
    if (refusal->type == type && refusal->class_ == cls && refusal->version != 0
        && type->tp_version_tag == refusal->version) {
        return 0;
    }
#endif
    if (PyType_IsSubtype(type, (PyTypeObject *)cls)) return 1;
    if (dunder_class == NULL) {
        dunder_class = PyUnicode_InternFromString("__class__");
        if (dunder_class == NULL) return -1;
        object_class = _PyType_Lookup(&PyBaseObject_Type, dunder_class);
    }
    if (object_class == NULL || _PyType_Lookup(type, dunder_class) != object_class) {
        return PyObject_IsInstance(o, cls);
    }
#ifndef Py_GIL_DISABLED
    refusal->type = type;
    refusal->version = type->tp_version_tag;
    refusal->class_ = cls;
#endif
    return 0;
}

/* python's `isinstance(o, cls)` for one class, taking an object of exactly that class
 * without a call. that is the first thing `PyObject_IsInstance` asks itself, so the
 * answer and everything the call would run are unchanged. -1 is an error */
static inline int By_SoundIs(PyObject *o, PyObject *cls) {
    if (BY_LIKELY((PyObject *)Py_TYPE(o) == cls)) return 1;
    return By_SoundIsOther(o, cls);
}

/* the `TypeError` `_soundness_check` raises when `o` is not an instance of `target`,
 * which is whatever that check's `isinstance` was handed — a class or a tuple of them:
 *
 *     f"type soundness violation: expected {getattr(_t, '__name__', _t)}, "
 *     f"got {type(_v).__name__}" */
BY_REFUSAL void By_SoundViolation(PyObject *o, PyObject *target) {
    static PyObject *dunder_name = NULL;
    PyObject *name;
    PyObject *expected;
    PyObject *got;
    if (dunder_name == NULL) {
        dunder_name = PyUnicode_InternFromString("__name__");
        if (dunder_name == NULL) return;
    }
    if (By_OptionalAttr(target, dunder_name, &name) < 0) return;
    expected = PyObject_Format(name == NULL ? target : name, NULL);
    Py_XDECREF(name);
    if (expected == NULL) return;
    got = By_TypeNameOf(o);
    if (got == NULL) {
        Py_DECREF(expected);
        return;
    }
    PyErr_Format(PyExc_TypeError, "type soundness violation: expected %U, got %U", expected,
                 got);
    Py_DECREF(expected);
    Py_DECREF(got);
}

/* [`By_SoundViolation`] for a target of several classes, handed the tuple it builds, or
 * NULL where building it raised */
BY_REFUSAL void By_SoundViolationOf(PyObject *o, PyObject *target) {
    if (target == NULL) return;
    By_SoundViolation(o, target);
    Py_DECREF(target);
}

/* how many digits an `int` has to have before no value of that length is a short
 *
 * python 3.12 onwards keeps the digit count in the object's header, beside the sign.
 * the smallest magnitude with `n` digits is 2**(PyLong_SHIFT*(n-1)), and to be past
 * every short it has to be *strictly* past 2**62, because -2**62 is itself one — so
 * this is the first `n` for which that holds. it is four for 30-bit digits and six
 * for 15-bit ones, which is why it is worked out rather than written down */
#if PY_VERSION_HEX >= 0x030C0000 && !defined(Py_LIMITED_API)
#define BY_DIGITS_PAST_SHORT ((uintptr_t)((sizeof(Py_ssize_t) * 8 - 2) / PyLong_SHIFT + 2))
#endif

/* everything [`By_TaggedFromLong`] is handed that is not an exact `int`
 *
 * an instance of an `int` subclass is held as itself, with a reference of its own,
 * whatever its value. `True` is the one every program meets: an `int` register that
 * narrowed it to the short 1 answered `1` where python answers `True`, and a subclass
 * lost its methods the same way. anything that is not an `int` at all raises the
 * `TypeError` an unbox of it raises, and the error path never clears: an operator on
 * an `int` subclass can answer with any object at all, and swallowing the failure here
 * once tagged a `str` as an int */
BY_REFUSAL ByTagged By_TaggedFromOther(PyObject *o) {
    if (o == NULL || !PyLong_Check(o)) {
        By_TypeError("int", o);
        return BY_INT_ERROR;
    }
    By_IncRef(o);
    return ((ByTagged)(void *)o) | BY_INT_TAG;
}

/* takes a new reference to `o` when it cannot be represented as a short, which an
 * exact `int` can whenever its value fits and a subclass never can */
static inline ByTagged By_TaggedFromLong(PyObject *o) {
    if (BY_UNLIKELY(o == NULL || !PyLong_CheckExact(o))) return By_TaggedFromOther(o);
#if PY_VERSION_HEX >= 0x030C0000
    /* almost every `int` a program unboxes is one that fits a single digit, and
     * python 3.12 onwards stores such a value in the object's own header rather
     * than behind a pointer. reading it is a load and a multiply; asking
     * `PyLong_AsLongLongAndOverflow` for it is a call into libpython that cannot
     * be inlined, and a loop that reads an `int` out of a container pays that
     * call on every trip.
     *
     * `PyUnstable_Long_*` are cpython's own accessors for exactly this, and the
     * header defines them as inline functions over the macros that name them —
     * so this is the interpreter's own fast path rather than a guess about its
     * layout */
    if (BY_LIKELY(PyUnstable_Long_IsCompact((PyLongObject *)o))) {
        return By_ShortFrom(PyUnstable_Long_CompactValue((PyLongObject *)o));
    }
#endif
#ifdef BY_DIGITS_PAST_SHORT
    /* and the other end: a value this long is never a short, so the checked
     * conversion below would only find out what the header already says */
    if ((((PyLongObject *)o)->long_value.lv_tag >> _PyLong_NON_SIZE_BITS)
        >= BY_DIGITS_PAST_SHORT) {
        By_IncRef(o);
        return ((ByTagged)(void *)o) | BY_INT_TAG;
    }
#endif
    {
    /* an `int` is read without asking `__index__`, so the only failure left is one
     * too wide for the word, and that is reported through `overflow` rather than
     * raised */
    int overflow = 0;
    long long value = PyLong_AsLongLongAndOverflow(o, &overflow);
    if (value == -1 && PyErr_Occurred()) return BY_INT_ERROR;
    if (!overflow && By_FitsShort((Py_ssize_t)value)) {
        return By_ShortFrom((Py_ssize_t)value);
    }
    By_IncRef(o);
    return ((ByTagged)(void *)o) | BY_INT_TAG;
    }
}

/* the error sentinel is an odd word whose object bits are all zero, so `By_LongOf`
 * already turns it into NULL. asking `x != BY_INT_ERROR` as well puts a second
 * compare and a second branch on the straight line of every release a loop makes;
 * letting the X-forms do it instead leaves the null test in the cold half beside
 * the refcount write, where a value that is neither short nor an error never goes.
 *
 * a register that was never written still holds the sentinel and still reaches
 * here from an error path, so the case has to be handled — this only moves where */
BY_HOT void By_DecRefTagged(ByTagged x) {
    if (BY_UNLIKELY(!By_IsShort(x))) {
        Py_XDECREF(By_LongOf(x));
    }
}

BY_HOT void By_IncRefTagged(ByTagged x) {
    if (BY_UNLIKELY(!By_IsShort(x))) {
        By_XIncRef(By_LongOf(x));
    }
}

/* ── int arithmetic ───────────────────────────────────────────────────────────
 *
 * each fast path is deliberately *conservative*: when it cannot prove the result
 * fits, it falls through to the boxed path, which is always correct. so a missed
 * fast path costs speed and never correctness.
 */

BY_COLD ByTagged By_IntSlowBinary(ByTagged a, ByTagged b, const char *op) {
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return BY_INT_ERROR;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return BY_INT_ERROR; }

    PyObject *result = NULL;
    switch (op[0]) {
        case '+': result = PyNumber_Add(left, right); break;
        case '-': result = PyNumber_Subtract(left, right); break;
        case '*': result = PyNumber_Multiply(left, right); break;
        case '/': result = PyNumber_FloorDivide(left, right); break;
        case '%': result = PyNumber_Remainder(left, right); break;
        default: PyErr_SetString(PyExc_SystemError, "unknown int operation"); break;
    }
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_INT_ERROR;
    ByTagged tagged = By_TaggedFromLong(result);
    Py_DECREF(result);
    return tagged;
}

/* the operators with no worthwhile tagged fast path, or none at all */
BY_COLD ByTagged By_IntSlowBitwise(ByTagged a, ByTagged b, char op) {
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return BY_INT_ERROR;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return BY_INT_ERROR; }
    PyObject *result = NULL;
    switch (op) {
        case '&': result = PyNumber_And(left, right); break;
        case '|': result = PyNumber_Or(left, right); break;
        case '^': result = PyNumber_Xor(left, right); break;
        case '<': result = PyNumber_Lshift(left, right); break;
        case '>': result = PyNumber_Rshift(left, right); break;
        case 'p': result = PyNumber_Power(left, right, Py_None); break;
        default: PyErr_SetString(PyExc_SystemError, "unknown int operation"); break;
    }
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_INT_ERROR;
    /* `**` with a negative exponent yields a float, which cannot be tagged */
    if (!PyLong_Check(result)) {
        PyErr_SetString(PyExc_TypeError,
                        "this operation does not produce an int; annotate the result as float");
        Py_DECREF(result);
        return BY_INT_ERROR;
    }
    ByTagged tagged = By_TaggedFromLong(result);
    Py_DECREF(result);
    return tagged;
}

/* `a <op>= b` where `a` is not a short: python asks `a` for its in-place method before
 * its plain one, and an `int` subclass may define one of its own. `operation` is the
 * abstract api's in-place form, which makes both asks, so an exact `int` — which has no
 * in-place methods — gets exactly what the plain operator's slow path would give it */
BY_COLD ByTagged By_IntSlowInPlace(ByTagged a, ByTagged b, binaryfunc operation) {
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return BY_INT_ERROR;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return BY_INT_ERROR; }
    PyObject *result = operation(left, right);
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_INT_ERROR;
    ByTagged tagged = By_TaggedFromLong(result);
    Py_DECREF(result);
    return tagged;
}

/* `**=` as a two-operand in-place operation, which is the form the slow path calls */
static inline PyObject *By_NumberInPlacePower(PyObject *a, PyObject *b) {
    return PyNumber_InPlacePower(a, b, Py_None);
}

/* the fast paths alone, for a caller that branches on them: each answers 1 with the
 * result written where both operands are shorts and so is the result, and 0 where the
 * slow path has to decide. a short is an even word and each of these results is one
 * too, so what they write is never the error value — a caller only has to test for an
 * error on the slow path, which is the only one that calls into cpython */
static inline int By_IntAddShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = (Py_ssize_t)a, y = (Py_ssize_t)b;
        /* wrap in the unsigned domain, where overflow is defined, then use the
         * sign test: overflow happened iff both operands differ in sign from the
         * result */
        Py_ssize_t sum = (Py_ssize_t)((size_t)x + (size_t)y);
        if (BY_LIKELY(((x ^ sum) & (y ^ sum)) >= 0)) {
            *out = (ByTagged)sum;
            return 1;
        }
    }
    return 0;
}

static inline int By_IntSubShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = (Py_ssize_t)a, y = (Py_ssize_t)b;
        Py_ssize_t diff = (Py_ssize_t)((size_t)x - (size_t)y);
        if (BY_LIKELY(((x ^ y) & (x ^ diff)) >= 0)) {
            *out = (ByTagged)diff;
            return 1;
        }
    }
    return 0;
}

/* a product of two values within this bound cannot leave the short range */
#define BY_MUL_SAFE (((Py_ssize_t)1) << ((sizeof(Py_ssize_t) * 8 - 4) / 2))

static inline int By_IntMulShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = By_ShortValue(a), y = By_ShortValue(b);
        if (x > -BY_MUL_SAFE && x < BY_MUL_SAFE && y > -BY_MUL_SAFE && y < BY_MUL_SAFE) {
            *out = By_ShortFrom(x * y);
            return 1;
        }
    }
    return 0;
}

/* `& | ^` are exact on the shifted representation: (2a)&(2b) == 2(a&b) */
static inline int By_IntAndShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        *out = a & b;
        return 1;
    }
    return 0;
}

static inline int By_IntOrShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        *out = a | b;
        return 1;
    }
    return 0;
}

static inline int By_IntXorShort(ByTagged a, ByTagged b, ByTagged *out) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        *out = a ^ b;
        return 1;
    }
    return 0;
}

BY_HOT ByTagged By_IntAdd(ByTagged a, ByTagged b) {
    ByTagged sum;
    if (BY_LIKELY(By_IntAddShort(a, b, &sum))) return sum;
    return By_IntSlowBinary(a, b, "+");
}

BY_HOT ByTagged By_IntSub(ByTagged a, ByTagged b) {
    ByTagged diff;
    if (BY_LIKELY(By_IntSubShort(a, b, &diff))) return diff;
    return By_IntSlowBinary(a, b, "-");
}

BY_HOT ByTagged By_IntMul(ByTagged a, ByTagged b) {
    ByTagged product;
    if (BY_LIKELY(By_IntMulShort(a, b, &product))) return product;
    return By_IntSlowBinary(a, b, "*");
}

/* the exception a zero divisor raises, in the running interpreter's own words
 *
 * an unboxed path performs the division itself, so nothing in cpython has raised by the
 * time this is reached — and the wording is not ours to invent: 3.13 names the operand
 * type and the operation, 3.14 says `division by zero` for every one of them, and 3.13
 * already distinguishes `%` from `//` in a way a single string cannot. so `operation` is
 * re-performed through the abstract api on a pair that must fail the same way. the
 * message does not depend on the operands, only on their types and the operation, and
 * this is only ever reached on the way out */
BY_COLD void By_ZeroDivision(binaryfunc operation, int floating) {
    PyObject *left = floating ? PyFloat_FromDouble(1.0) : PyLong_FromLong(1);
    PyObject *right = floating ? PyFloat_FromDouble(0.0) : PyLong_FromLong(0);
    PyObject *impossible = NULL;
    if (left != NULL && right != NULL) impossible = operation(left, right);
    Py_XDECREF(left);
    Py_XDECREF(right);
    Py_XDECREF(impossible);
    /* an allocation that failed has already raised, and so has the operation */
    if (!PyErr_Occurred()) PyErr_SetString(PyExc_ZeroDivisionError, "division by zero");
}

/* python floors rather than truncating: -7 // 2 is -4, not -3 */
BY_HOT Py_ssize_t By_FloorDivSsize(Py_ssize_t a, Py_ssize_t b) {
    Py_ssize_t q = a / b;
    if ((a % b != 0) && ((a < 0) != (b < 0))) q--;
    return q;
}

BY_HOT Py_ssize_t By_ModSsize(Py_ssize_t a, Py_ssize_t b) {
    Py_ssize_t r = a % b;
    if (r != 0 && ((r < 0) != (b < 0))) r += b;
    return r;
}

BY_HOT ByTagged By_IntFloorDiv(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t y = By_ShortValue(b);
        if (y == 0) {
            By_ZeroDivision(PyNumber_FloorDivide, 0);
            return BY_INT_ERROR;
        }
        Py_ssize_t x = By_ShortValue(a);
        /* the one case where the quotient leaves the range of the operands */
        if (!(x == BY_SHORT_MIN && y == -1)) {
            return By_ShortFrom(By_FloorDivSsize(x, y));
        }
    }
    return By_IntSlowBinary(a, b, "/");
}

/* `%` works on the tagged words themselves, with no shift out and back. a short is
 * its value shifted left by one, so `(2x) % (2y)` is `2 (x % y)`, and the floor fix-up
 * adds the divisor, `2y` — the answer is already a tagged short. the one remainder that
 * overflows is by -1, and a tagged divisor is even, so it never is: only a zero divisor
 * faults */
BY_HOT ByTagged By_IntMod(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        if (b == By_ShortFrom(0)) {
            By_ZeroDivision(PyNumber_Remainder, 0);
            return BY_INT_ERROR;
        }
        return (ByTagged)By_ModSsize((Py_ssize_t)a, (Py_ssize_t)b);
    }
    return By_IntSlowBinary(a, b, "%");
}

static inline double By_IntTrueDivWith(ByTagged a, ByTagged b, binaryfunc operation) {
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return BY_FLOAT_ERROR;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return BY_FLOAT_ERROR; }
    PyObject *result = operation(left, right);
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_FLOAT_ERROR;
    double value = PyFloat_AsDouble(result);
    Py_DECREF(result);
    return value;
}

BY_HOT ByTagged By_IntAnd(ByTagged a, ByTagged b) {
    ByTagged out;
    if (BY_LIKELY(By_IntAndShort(a, b, &out))) return out;
    return By_IntSlowBitwise(a, b, '&');
}
BY_HOT ByTagged By_IntOr(ByTagged a, ByTagged b) {
    ByTagged out;
    if (BY_LIKELY(By_IntOrShort(a, b, &out))) return out;
    return By_IntSlowBitwise(a, b, '|');
}
BY_HOT ByTagged By_IntXor(ByTagged a, ByTagged b) {
    ByTagged out;
    if (BY_LIKELY(By_IntXorShort(a, b, &out))) return out;
    return By_IntSlowBitwise(a, b, '^');
}
static inline ByTagged By_IntShl(ByTagged a, ByTagged b) {
    return By_IntSlowBitwise(a, b, '<');
}
static inline ByTagged By_IntShr(ByTagged a, ByTagged b) {
    return By_IntSlowBitwise(a, b, '>');
}
static inline ByTagged By_IntPow(ByTagged a, ByTagged b) {
    return By_IntSlowBitwise(a, b, 'p');
}

static inline double By_IntTrueDiv(ByTagged a, ByTagged b) {
    return By_IntTrueDivWith(a, b, PyNumber_TrueDivide);
}

/* the augmented assignments. an exact `int` on the left has no in-place method, so the
 * operation is the plain operator's, fast path and all; anything else is asked its own
 * in-place method first. `/=` always goes through the protocol, so it asks it directly */
#define BY_DEFINE_INT_IN_PLACE(name, plain, operation)                         \
    static inline ByTagged name(ByTagged a, ByTagged b) {                      \
        if (BY_UNLIKELY(!By_IsExactInt(a))) return By_IntSlowInPlace(a, b, operation); \
        return plain(a, b);                                                    \
    }

BY_DEFINE_INT_IN_PLACE(By_IntIAdd, By_IntAdd, PyNumber_InPlaceAdd)
BY_DEFINE_INT_IN_PLACE(By_IntISub, By_IntSub, PyNumber_InPlaceSubtract)
BY_DEFINE_INT_IN_PLACE(By_IntIMul, By_IntMul, PyNumber_InPlaceMultiply)
BY_DEFINE_INT_IN_PLACE(By_IntIFloorDiv, By_IntFloorDiv, PyNumber_InPlaceFloorDivide)
BY_DEFINE_INT_IN_PLACE(By_IntIMod, By_IntMod, PyNumber_InPlaceRemainder)
BY_DEFINE_INT_IN_PLACE(By_IntIPow, By_IntPow, By_NumberInPlacePower)
BY_DEFINE_INT_IN_PLACE(By_IntIAnd, By_IntAnd, PyNumber_InPlaceAnd)
BY_DEFINE_INT_IN_PLACE(By_IntIOr, By_IntOr, PyNumber_InPlaceOr)
BY_DEFINE_INT_IN_PLACE(By_IntIXor, By_IntXor, PyNumber_InPlaceXor)
BY_DEFINE_INT_IN_PLACE(By_IntIShl, By_IntShl, PyNumber_InPlaceLshift)
BY_DEFINE_INT_IN_PLACE(By_IntIShr, By_IntShr, PyNumber_InPlaceRshift)

static inline double By_IntITrueDiv(ByTagged a, ByTagged b) {
    return By_IntTrueDivWith(a, b, PyNumber_InPlaceTrueDivide);
}

/* a unary operator on an `int` subclass, which answers through the subclass's own
 * method: `-a` is `type(a).__neg__(a)`, which the subtraction the exact forms reduce to
 * would have taken to `__rsub__` instead */
BY_COLD ByTagged By_IntUnarySlow(ByTagged a, PyObject *(*operation)(PyObject *)) {
    PyObject *operand = By_BoxInt(a);
    PyObject *result;
    ByTagged tagged;
    if (operand == NULL) return BY_INT_ERROR;
    result = operation(operand);
    Py_DECREF(operand);
    if (result == NULL) return BY_INT_ERROR;
    tagged = By_TaggedFromLong(result);
    Py_DECREF(result);
    return tagged;
}

/* `~a` is `-1 - a` for an exact `int`, which the tagged subtraction already handles */
static inline ByTagged By_IntInvert(ByTagged a) {
    if (BY_UNLIKELY(!By_IsExactInt(a))) return By_IntUnarySlow(a, PyNumber_Invert);
    return By_IntSub(By_ShortFrom(-1), a);
}

static inline ByTagged By_IntNeg(ByTagged a) {
    if (By_IsShort(a)) {
        Py_ssize_t x = By_ShortValue(a);
        /* the short range is one wider below zero than above it */
        if (x != BY_SHORT_MIN) return By_ShortFrom(-x);
    } else if (BY_UNLIKELY(!By_IsExactInt(a))) {
        return By_IntUnarySlow(a, PyNumber_Negative);
    }
    return By_IntSub(By_ShortFrom(0), a);
}

/* `+a`, which is `a` itself for an exact `int` and `int.__pos__`'s exact copy, or a
 * method of the subclass's own, for anything else */
static inline ByTagged By_IntPos(ByTagged a) {
    if (BY_UNLIKELY(!By_IsExactInt(a))) return By_IntUnarySlow(a, PyNumber_Positive);
    By_IncRefTagged(a);
    return a;
}

/* `operator.index(a)`: the exact `int` a subclass stands for, copied by value, which is
 * how `range` reads its bounds. `PyNumber_Index` asks an `int` subclass nothing of its
 * own — not even `__index__` — and only copies it */
static inline ByTagged By_IntIndex(ByTagged a) {
    if (BY_UNLIKELY(!By_IsExactInt(a))) return By_IntUnarySlow(a, PyNumber_Index);
    By_IncRefTagged(a);
    return a;
}

/* whether an `int` behind a pointer is true: always, for an exact one, and its own
 * `__bool__` for a subclass. 2 is an error. a short is tested where it is read */
BY_COLD char By_IntTruthySlow(ByTagged a) {
    PyObject *operand;
    int answer;
    /* a pointer to an exact `int` holds a value too wide to be a short, so not zero */
    if (By_IsExactInt(a)) return 1;
    operand = By_BoxInt(a);
    if (operand == NULL) return 2;
    answer = PyObject_IsTrue(operand);
    Py_DECREF(operand);
    return answer < 0 ? 2 : (char)answer;
}

/* ── int comparison ───────────────────────────────────────────────────────── */

BY_COLD char By_IntCompareSlow(ByTagged a, ByTagged b, int op) {
#if PY_VERSION_HEX >= 0x030C0000 && !defined(Py_LIMITED_API)
    /* an exact `int` is only ever held behind a pointer when it does not fit a short,
     * so against a short the two can never be equal and the pointer's sign alone says
     * which is larger. a subclass is left to its own comparison, which is what makes
     * this a fact about the representation rather than about the values */
    if (By_IsShort(a) != By_IsShort(b)) {
        PyObject *big = By_LongOf(By_IsShort(a) ? b : a);
        if (big != NULL && PyLong_CheckExact(big)) {
            /* the low two bits of the tag are 0 for positive and 2 for negative; a
             * value too big to be a short is never zero */
            int negative = (((PyLongObject *)big)->long_value.lv_tag & _PyLong_SIGN_MASK) == 2;
            /* whether `a < b` */
            char below = (char)(By_IsShort(a) ? !negative : negative);
            switch (op) {
                case Py_EQ: return 0;
                case Py_NE: return 1;
                case Py_LT:
                case Py_LE: return below;
                default: return (char)!below;
            }
        }
    }
#endif
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return 2;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return 2; }
    int result = PyObject_RichCompareBool(left, right, op);
    Py_DECREF(left);
    Py_DECREF(right);
    return result < 0 ? 2 : (char)result;
}

/* the *tagged* values are compared, not the untagged ones: a short is its value
 * shifted left by one, and shifting preserves order, so `a < b` holds exactly when
 * `a << 1 < b << 1`. that saves an arithmetic shift on each side of every
 * comparison — two per iteration in a counting loop */
#define BY_DEFINE_INT_CMP(name, c_op, py_op)                                   \
    BY_HOT char name(ByTagged a, ByTagged b) {                                 \
        if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {                                  \
            return (char)((Py_ssize_t)a c_op(Py_ssize_t) b);                   \
        }                                                                      \
        return By_IntCompareSlow(a, b, py_op);                                 \
    }

BY_DEFINE_INT_CMP(By_IntEq, ==, Py_EQ)
BY_DEFINE_INT_CMP(By_IntNe, !=, Py_NE)
BY_DEFINE_INT_CMP(By_IntLt, <, Py_LT)
BY_DEFINE_INT_CMP(By_IntLe, <=, Py_LE)
BY_DEFINE_INT_CMP(By_IntGt, >, Py_GT)
BY_DEFINE_INT_CMP(By_IntGe, >=, Py_GE)

/* a machine integer given the tagged representation. the fast path is the whole
 * point of the counter being unboxed in the first place, so it is the one tested */
BY_HOT ByTagged By_IntFromI64(int64_t value) {
    if (BY_LIKELY(By_FitsShort((Py_ssize_t)value))) {
        return By_ShortFrom((Py_ssize_t)value);
    }
    {
        PyObject *object = PyLong_FromLongLong((long long)value);
        ByTagged tagged;
        if (object == NULL) return BY_INT_ERROR;
        tagged = By_TaggedFromLong(object);
        Py_DECREF(object);
        return tagged;
    }
}

/* the boxing half of comparing an unboxed counter against a bound that is still
 * tagged, for when the bound turns out not to be short
 *
 * hoisting the bound out of the loop is not available: it is an ordinary python
 * `int` and may be arbitrarily large, so the shortness test the caller emits is
 * per-trip. when it fails the counter is boxed and the general comparison runs,
 * which is exactly what would have happened had the counter never been unboxed
 *
 * the caller does the short case itself rather than calling through one function
 * that handles both. only this half can fail, so keeping it separate keeps the
 * error test out of the loop's straight line, where it would be a second branch
 * on a value the short case has already settled at 0 or 1 */
#define BY_DEFINE_I64_CMP_SLOW(name, tagged_name)                              \
    static inline char name(int64_t a, ByTagged b) {                           \
        ByTagged boxed = By_IntFromI64(a);                                     \
        char result;                                                           \
        if (boxed == BY_INT_ERROR) return 2;                                   \
        result = tagged_name(boxed, b);                                        \
        By_DecRefTagged(boxed);                                                \
        return result;                                                         \
    }

BY_DEFINE_I64_CMP_SLOW(By_I64EqSlow, By_IntEq)
BY_DEFINE_I64_CMP_SLOW(By_I64NeSlow, By_IntNe)
BY_DEFINE_I64_CMP_SLOW(By_I64LtSlow, By_IntLt)
BY_DEFINE_I64_CMP_SLOW(By_I64LeSlow, By_IntLe)
BY_DEFINE_I64_CMP_SLOW(By_I64GtSlow, By_IntGt)
BY_DEFINE_I64_CMP_SLOW(By_I64GeSlow, By_IntGe)

/* ── floats ───────────────────────────────────────────────────────────────────
 *
 * in `.by`, `float` does not include `int`, so an unboxed double needs no
 * int-check guard on the way in. see features/no-number-promotions.md
 */

/* `a <op> b` where `a` is already a double and `b` is any object, for the case
 * where the checker has said the result is a `float`.
 *
 * the fast path is an exact float, which is the only type whose value a double
 * already holds. anything else goes through the object protocol exactly as it
 * would have — so a `Decimal` still reaches `__radd__` — and only the *shape*
 * changes: no `PyFloatObject` is allocated to hold a value that was in a register
 */
#define BY_DEFINE_FLOAT_OBJ(name, c_op, slow)                                  \
    static inline double name(double a, PyObject *b) {                         \
        if (PyFloat_CheckExact(b)) return a c_op PyFloat_AS_DOUBLE(b);         \
        return By_FloatObjectSlow(a, b, slow);                                 \
    }

BY_COLD double By_FloatObjectSlow(double a, PyObject *b,
                                        PyObject *(*op)(PyObject *, PyObject *)) {
    PyObject *boxed = PyFloat_FromDouble(a);
    if (boxed == NULL) return BY_FLOAT_ERROR;
    PyObject *result = op(boxed, b);
    Py_DECREF(boxed);
    if (result == NULL) return BY_FLOAT_ERROR;
    double value = PyFloat_AsDouble(result);
    Py_DECREF(result);
    if (value == -1.0 && PyErr_Occurred()) return BY_FLOAT_ERROR;
    return value;
}

/* `a <op> b` for an `int` on the left of a double, where `a` is not a short
 *
 * python asks the `int` first. an exact one declines a double and the double's reflected
 * method converts it, which is what the operation does with a short and what the
 * protocol does here with a value too wide for one, `OverflowError` included. a subclass
 * may answer with a method of its own, so it is asked. the checker has said the answer
 * is a `float`, and an answer that is not one is refused rather than converted */
BY_COLD double By_IntFloatSlow(ByTagged a, double b, char op) {
    PyObject *left = By_BoxInt(a);
    PyObject *right;
    PyObject *result = NULL;
    double value;
    if (left == NULL) return BY_FLOAT_ERROR;
    right = PyFloat_FromDouble(b);
    if (right == NULL) {
        Py_DECREF(left);
        return BY_FLOAT_ERROR;
    }
    switch (op) {
        case '+': result = PyNumber_Add(left, right); break;
        case '-': result = PyNumber_Subtract(left, right); break;
        case '*': result = PyNumber_Multiply(left, right); break;
        case '/': result = PyNumber_TrueDivide(left, right); break;
        case 'f': result = PyNumber_FloorDivide(left, right); break;
        case '%': result = PyNumber_Remainder(left, right); break;
        case 'p': result = PyNumber_Power(left, right, Py_None); break;
        default: PyErr_SetString(PyExc_SystemError, "unknown float operation"); break;
    }
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_FLOAT_ERROR;
    if (!PyFloat_Check(result)) {
        By_TypeError("float", result);
        Py_DECREF(result);
        return BY_FLOAT_ERROR;
    }
    value = PyFloat_AS_DOUBLE(result);
    Py_DECREF(result);
    return value;
}

/* `a <op> b` with one side a proven double and the other an object that can only
 * be an int or a float
 *
 * the fast path is an exact float. everything else goes through the object
 * protocol, which is what keeps `1.5 < 10**400` exact — python compares an int
 * against a float without converting either, so a conversion here would raise
 * where python answers
 */
static inline char By_FloatObjectCompare(double a, PyObject *b, int op, int reflected);

#define BY_DEFINE_FLOAT_OBJ_CMP(name, c_op, py_op)                             \
    static inline char name(double a, PyObject *b) {                           \
        if (PyFloat_CheckExact(b)) return (char)(a c_op PyFloat_AS_DOUBLE(b)); \
        return By_FloatObjectCompare(a, b, py_op, 0);                          \
    }                                                                          \
    static inline char name##Rev(PyObject *b, double a) {                      \
        if (PyFloat_CheckExact(b)) return (char)(PyFloat_AS_DOUBLE(b) c_op a); \
        return By_FloatObjectCompare(a, b, py_op, 1);                          \
    }

static inline char By_FloatObjectCompare(double a, PyObject *b, int op, int reflected) {
    PyObject *boxed = PyFloat_FromDouble(a);
    if (boxed == NULL) return 2;
    PyObject *result = reflected ? PyObject_RichCompare(b, boxed, op)
                                 : PyObject_RichCompare(boxed, b, op);
    Py_DECREF(boxed);
    if (result == NULL) return 2;
    int truth = PyObject_IsTrue(result);
    Py_DECREF(result);
    return truth < 0 ? 2 : (char)truth;
}

BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjEq, ==, Py_EQ)
BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjNe, !=, Py_NE)
BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjLt, <, Py_LT)
BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjLe, <=, Py_LE)
BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjGt, >, Py_GT)
BY_DEFINE_FLOAT_OBJ_CMP(By_FloatObjGe, >=, Py_GE)

/* the reflected order: the object is on the left and the double on the right,
 * which is what `xs[0] * a` lowers to. the double is only boxed on the slow path */
#define BY_DEFINE_OBJ_FLOAT(name, c_op, slow)                                  \
    static inline double name(PyObject *a, double b) {                         \
        if (PyFloat_CheckExact(a)) return PyFloat_AS_DOUBLE(a) c_op b;         \
        return By_ObjFloatSlow(a, b, slow);                                    \
    }

BY_COLD double By_ObjFloatSlow(PyObject *a, double b,
                                     PyObject *(*op)(PyObject *, PyObject *)) {
    PyObject *boxed = PyFloat_FromDouble(b);
    if (boxed == NULL) return BY_FLOAT_ERROR;
    PyObject *result = op(a, boxed);
    Py_DECREF(boxed);
    if (result == NULL) return BY_FLOAT_ERROR;
    double value = PyFloat_AsDouble(result);
    Py_DECREF(result);
    if (value == -1.0 && PyErr_Occurred()) return BY_FLOAT_ERROR;
    return value;
}

BY_DEFINE_OBJ_FLOAT(By_ObjFloatAdd, +, PyNumber_Add)
BY_DEFINE_OBJ_FLOAT(By_ObjFloatSub, -, PyNumber_Subtract)
BY_DEFINE_OBJ_FLOAT(By_ObjFloatMul, *, PyNumber_Multiply)

static inline double By_ObjFloatDiv(PyObject *a, double b) {
    if (PyFloat_CheckExact(a)) {
        if (b == 0.0) {
            By_ZeroDivision(PyNumber_TrueDivide, 1);
            return BY_FLOAT_ERROR;
        }
        return PyFloat_AS_DOUBLE(a) / b;
    }
    return By_ObjFloatSlow(a, b, PyNumber_TrueDivide);
}

BY_DEFINE_FLOAT_OBJ(By_FloatObjAdd, +, PyNumber_Add)
BY_DEFINE_FLOAT_OBJ(By_FloatObjSub, -, PyNumber_Subtract)
BY_DEFINE_FLOAT_OBJ(By_FloatObjMul, *, PyNumber_Multiply)

static inline double By_FloatObjDiv(double a, PyObject *b) {
    if (PyFloat_CheckExact(b)) {
        double divisor = PyFloat_AS_DOUBLE(b);
        if (divisor == 0.0) {
            By_ZeroDivision(PyNumber_TrueDivide, 1);
            return BY_FLOAT_ERROR;
        }
        return a / divisor;
    }
    return By_FloatObjectSlow(a, b, PyNumber_TrueDivide);
}

/* `0.0 ** -1.0`, in the running interpreter's own words: 3.13 and 3.14 spell it
 * differently, so the power is re-performed on a pair that must fail the same way */
BY_COLD void By_ZeroToNegativePower(void) {
    PyObject *zero = PyFloat_FromDouble(0.0);
    PyObject *negative = PyFloat_FromDouble(-1.0);
    PyObject *impossible = NULL;
    if (zero != NULL && negative != NULL) impossible = PyNumber_Power(zero, negative, Py_None);
    Py_XDECREF(zero);
    Py_XDECREF(negative);
    Py_XDECREF(impossible);
    if (!PyErr_Occurred()) {
        PyErr_SetString(PyExc_ZeroDivisionError, "zero to a negative power");
    }
}

/* python's `float.__pow__`, case for case (cpython's `float_pow`), for a pair whose
 * answer is known to be real: a negative base to a fractional power is a complex number
 * in python, and the lowering sends any pair that could be one through the object
 * protocol instead. the platform's `pow` is only asked about what cpython asks it, and
 * a result it overflows raises `OverflowError` with the errno cpython reports */
static double By_FloatPow(double iv, double iw) {
    int negate = 0;
    if (iw == 0.0) return 1.0;
    if (isnan(iv)) return iv;
    if (isnan(iw)) return iv == 1.0 ? 1.0 : iw;
    if (isinf(iw)) {
        iv = fabs(iv);
        if (iv == 1.0) return 1.0;
        if ((iw > 0.0) == (iv > 1.0)) return fabs(iw);
        return 0.0;
    }
    if (isinf(iv)) {
        int odd = fmod(fabs(iw), 2.0) == 1.0;
        if (iw > 0.0) return odd ? iv : fabs(iv);
        return odd ? copysign(0.0, iv) : 0.0;
    }
    if (iv == 0.0) {
        int odd = fmod(fabs(iw), 2.0) == 1.0;
        if (iw < 0.0) {
            By_ZeroToNegativePower();
            return BY_FLOAT_ERROR;
        }
        return odd ? iv : 0.0;
    }
    if (iv < 0.0) {
        if (iw != floor(iw)) {
            PyErr_SetString(PyExc_SystemError, "a complex power reached the float lowering");
            return BY_FLOAT_ERROR;
        }
        iv = -iv;
        negate = fmod(fabs(iw), 2.0) == 1.0;
    }
    if (iv == 1.0) return negate ? -1.0 : 1.0;
    errno = 0;
    double ix = pow(iv, iw);
    /* cpython's `_Py_ADJUST_ERANGE1`: a platform that leaves errno alone on overflow
     * still has to raise, and an underflow to zero is not an error */
    if (errno == 0) {
        if (ix == HUGE_VAL || ix == -HUGE_VAL) errno = ERANGE;
    } else if (errno == ERANGE && ix == 0.0) {
        errno = 0;
    }
    if (negate) ix = -ix;
    if (errno != 0) {
        PyErr_SetFromErrno(errno == ERANGE ? PyExc_OverflowError : PyExc_ValueError);
        return BY_FLOAT_ERROR;
    }
    return ix;
}

/* the quotient and remainder of a divisor already known not to be zero, as cpython's
 * `_float_div_mod` and `float_rem` compute them. emitted code tests the divisor itself
 * and jumps straight to its error edge, so the answer never has to be told apart from
 * an error value that is also a legal double
 *
 * `floor(a / b)` is not the same operation: `inf // 1.5` is a nan in python, since the
 * remainder it is derived from is one, and `1.5 // -inf` is `-1.0` */
static inline double By_FloatFloorDivNonzero(double vx, double wx) {
    double mod = fmod(vx, wx);
    double div = (vx - mod) / wx;
    if (mod != 0.0 && ((wx < 0.0) != (mod < 0.0))) div -= 1.0;
    if (div != 0.0) {
        double floordiv = floor(div);
        if (div - floordiv > 0.5) floordiv += 1.0;
        return floordiv;
    }
    return copysign(0.0, vx / wx);
}

static inline double By_FloatModNonzero(double vx, double wx) {
    double mod = fmod(vx, wx);
    /* python's % takes the sign of the divisor, a zero remainder included */
    if (mod != 0.0) {
        if ((wx < 0.0) != (mod < 0.0)) mod += wx;
        return mod;
    }
    return copysign(0.0, wx);
}

static inline double By_FloatFloorDiv(double a, double b) {
    if (b == 0.0) {
        By_ZeroDivision(PyNumber_FloorDivide, 1);
        return BY_FLOAT_ERROR;
    }
    return By_FloatFloorDivNonzero(a, b);
}

static inline double By_FloatMod(double a, double b) {
    if (b == 0.0) {
        By_ZeroDivision(PyNumber_Remainder, 1);
        return BY_FLOAT_ERROR;
    }
    return By_FloatModNonzero(a, b);
}

/* the conversion `float.__add__` performs on an `int` operand: correctly rounded,
 * and `OverflowError` when the value has no float at all. this is what makes a
 * mixed pair lowered as a double operation exact rather than approximate */
static inline double By_TaggedToDouble(ByTagged x) {
    if (By_IsShort(x)) return (double)By_ShortValue(x);
    double v = PyLong_AsDouble(By_LongOf(x));
    if (v == -1.0 && PyErr_Occurred()) return BY_FLOAT_ERROR;
    return v;
}

/* the half of the conversion that reaches cpython, for a caller that has tested the
 * value short itself: `-1.0` with an exception set is the failure, as it is for
 * `PyLong_AsDouble` */
BY_COLD double By_TaggedToDoubleSlow(ByTagged x) { return PyLong_AsDouble(By_LongOf(x)); }

static inline double By_FloatTrueDiv(double a, double b) {
    if (b == 0.0) {
        By_ZeroDivision(PyNumber_TrueDivide, 1);
        return BY_FLOAT_ERROR;
    }
    return a / b;
}

/* ── boxing ───────────────────────────────────────────────────────────────── */

static inline PyObject *By_BoxFloat(double v) { return PyFloat_FromDouble(v); }

static inline PyObject *By_BoxBool(char v) {
    PyObject *o = v ? Py_True : Py_False;
    By_IncRef(o);
    return o;
}

static inline PyObject *By_BoxNone(void) {
    By_IncRef(Py_None);
    return Py_None;
}

/* unboxing is a *narrowing*, so it is always checked — this is the
 * representation invariant's inserted check, not an assumption
 *
 * the exact-`int` test the conversion opens with is the check: what fails it is tested
 * again out of line, and only there is anything refused. asking `PyLong_Check` first as
 * well made the narrowing too big for clang to inline into a loop, and `dictget`, which
 * narrows two values a trip, retired 5% more instructions for the call */
static inline ByTagged By_UnboxInt(PyObject *o) {
    return By_TaggedFromLong(o);
}

/* python's `float` annotation admits an `int`, so a `double` parameter is a test
 * rather than a demand. a subclass is excluded too: unboxing one to a double
 * would lose everything that made it a subclass */
static inline int By_IsExactFloat(PyObject *o) {
    return o != NULL && PyFloat_CheckExact(o);
}

/* hand a call to the interpreted definition, which is the code the annotation
 * describes. reached when an argument is legal python but not the representation
 * the compiled body was built against */
/* the same, for a method: a fastcall method keeps its receiver out of the argument
 * vector, and the interpreted twin taken off the class is a plain function that wants
 * it in front */
static inline PyObject *By_CallInterpretedMethod(PyObject *fn, const char *name,
                                                 PyObject *self,
                                                 PyObject *const *args, Py_ssize_t nargs,
                                                 PyObject *kwnames) {
    PyObject *inline_vec[8];
    PyObject **vec = inline_vec;
    Py_ssize_t total = nargs + (kwnames == NULL ? 0 : PyTuple_GET_SIZE(kwnames));
    Py_ssize_t index;
    PyObject *result;
    if (fn == NULL) {
        PyErr_Format(PyExc_TypeError,
                     "%s() has no interpreted definition to fall back to", name);
        return NULL;
    }
    if (total + 1 > (Py_ssize_t)(sizeof(inline_vec) / sizeof(inline_vec[0]))) {
        vec = (PyObject **)PyMem_Malloc((size_t)(total + 1) * sizeof(PyObject *));
        if (vec == NULL) return PyErr_NoMemory();
    }
    vec[0] = self;
    for (index = 0; index < total; index++) vec[index + 1] = args[index];
    result = PyObject_Vectorcall(fn, vec, (size_t)(nargs + 1), kwnames);
    if (vec != inline_vec) PyMem_Free(vec);
    return result;
}

/* the same, for a constructor: `tp_init` is handed a tuple and a dict rather than a
 * vector, and the interpreted twin taken off the class wants the receiver in front */
static inline int By_InitInterpreted(PyObject *fn, const char *name, PyObject *self,
                                     PyObject *args, PyObject *kwds) {
    Py_ssize_t nargs = args == NULL ? 0 : PyTuple_GET_SIZE(args);
    if (fn == NULL) {
        PyErr_Format(PyExc_TypeError,
                     "%s() has no interpreted definition to fall back to", name);
        return -1;
    }
    PyObject *bound = PyTuple_New(nargs + 1);
    if (bound == NULL) return -1;
    Py_INCREF(self);
    PyTuple_SET_ITEM(bound, 0, self);
    for (Py_ssize_t i = 0; i < nargs; i++) {
        PyObject *item = PyTuple_GET_ITEM(args, i);
        Py_INCREF(item);
        PyTuple_SET_ITEM(bound, i + 1, item);
    }
    PyObject *result = PyObject_Call(fn, bound, kwds);
    Py_DECREF(bound);
    if (result == NULL) return -1;
    Py_DECREF(result);
    return 0;
}

static inline PyObject *By_CallInterpreted(PyObject *fn, const char *name,
                                           PyObject *const *args, Py_ssize_t nargs,
                                           PyObject *kwnames) {
    if (fn == NULL) {
        PyErr_Format(PyExc_TypeError,
                     "%s() was compiled for exact float arguments and has no "
                     "interpreted definition to fall back to",
                     name);
        return NULL;
    }
    return PyObject_Vectorcall(fn, args, (size_t)nargs, kwnames);
}

/* `float`'s narrowing as a test, for a caller that branches on it and reads the double
 * itself: the double a float holds may be any value at all, so a failure that is only an
 * error value would have to ask the thread whether an exception is set on every read */
static inline int By_IsFloat(PyObject *o) { return o != NULL && PyFloat_Check(o); }

BY_COLD void By_UnboxFloatFailed(PyObject *o) { By_TypeError("float", o); }

static inline double By_UnboxFloat(PyObject *o) {
    /* `PyFloat_Check` admits float subclasses but not `int`, which is exactly
     * what `.by`'s `float` means */
    if (o == NULL || !PyFloat_Check(o)) {
        By_TypeError("float", o);
        return BY_FLOAT_ERROR;
    }
    return PyFloat_AS_DOUBLE(o);
}

static inline char By_UnboxBool(PyObject *o) {
    if (o == NULL || !PyBool_Check(o)) {
        By_TypeError("bool", o);
        return 2;
    }
    return (char)(o == Py_True);
}

static inline char By_UnboxNone(PyObject *o) {
    if (o != Py_None) {
        By_TypeError("NoneType", o);
        return 2;
    }
    return 0;
}

/* ── generic operations on `object` ───────────────────────────────────────────
 *
 * the widest representation: a `PyObject *` about which nothing is assumed. an
 * operation on one goes through the abstract object protocol, which is what the
 * interpreter would have done anyway — so a boxed register costs the interpreter's
 * speed and not more.
 *
 * every one of these returns a *new* reference, or NULL with an exception set.
 */

/* widen a known-class object to `object`: the pointer is unchanged, but the
 * destination register owns what it holds, so it needs its own reference */
static inline PyObject *By_NewRef(PyObject *o) {
    By_XIncRef(o);
    return o;
}

static inline PyObject *By_ObjAdd(PyObject *a, PyObject *b) { return PyNumber_Add(a, b); }
static inline PyObject *By_ObjSub(PyObject *a, PyObject *b) { return PyNumber_Subtract(a, b); }
static inline PyObject *By_ObjMul(PyObject *a, PyObject *b) { return PyNumber_Multiply(a, b); }
static inline PyObject *By_ObjFloorDiv(PyObject *a, PyObject *b) {
    return PyNumber_FloorDivide(a, b);
}
static inline PyObject *By_ObjMod(PyObject *a, PyObject *b) { return PyNumber_Remainder(a, b); }
static inline PyObject *By_ObjTrueDiv(PyObject *a, PyObject *b) {
    return PyNumber_TrueDivide(a, b);
}
static inline PyObject *By_ObjPow(PyObject *a, PyObject *b) {
    return PyNumber_Power(a, b, Py_None);
}
static inline PyObject *By_ObjAnd(PyObject *a, PyObject *b) { return PyNumber_And(a, b); }
static inline PyObject *By_ObjOr(PyObject *a, PyObject *b) { return PyNumber_Or(a, b); }
static inline PyObject *By_ObjXor(PyObject *a, PyObject *b) { return PyNumber_Xor(a, b); }
static inline PyObject *By_ObjShl(PyObject *a, PyObject *b) { return PyNumber_Lshift(a, b); }
static inline PyObject *By_ObjShr(PyObject *a, PyObject *b) { return PyNumber_Rshift(a, b); }
/* the augmented forms: python offers the left operand the operation on *itself*
 * first, and falls back to the binary one when it has no in-place method */
static inline PyObject *By_ObjIAdd(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceAdd(a, b);
}
static inline PyObject *By_ObjISub(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceSubtract(a, b);
}
static inline PyObject *By_ObjIMul(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceMultiply(a, b);
}
static inline PyObject *By_ObjIFloorDiv(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceFloorDivide(a, b);
}
static inline PyObject *By_ObjIMod(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceRemainder(a, b);
}
static inline PyObject *By_ObjITrueDiv(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceTrueDivide(a, b);
}
static inline PyObject *By_ObjIPow(PyObject *a, PyObject *b) {
    return PyNumber_InPlacePower(a, b, Py_None);
}
static inline PyObject *By_ObjIAnd(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceAnd(a, b);
}
static inline PyObject *By_ObjIOr(PyObject *a, PyObject *b) { return PyNumber_InPlaceOr(a, b); }
static inline PyObject *By_ObjIXor(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceXor(a, b);
}
static inline PyObject *By_ObjIShl(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceLshift(a, b);
}
static inline PyObject *By_ObjIShr(PyObject *a, PyObject *b) {
    return PyNumber_InPlaceRshift(a, b);
}

static inline PyObject *By_ObjNeg(PyObject *o) { return PyNumber_Negative(o); }
static inline PyObject *By_ObjInvert(PyObject *o) { return PyNumber_Invert(o); }
static inline PyObject *By_ObjPos(PyObject *o) { return PyNumber_Positive(o); }

/* ── the two questions python's comparison can be answered without asking ──────
 *
 * both forms of a compiled comparison ask these, and they are written once here so
 * that the answer the two forms give can only be the same answer. what each form does
 * with a yes differs — one wants a bit and the other the object python would have
 * produced — and that is all that differs */

/* whether both operands are `int`s of exactly that type holding a value python stores
 * in the object's header
 *
 * python compares a pair of the same type without asking either side to yield to the
 * other, so `int`'s own comparison is what it reaches — and for a value stored that way
 * that comparison is a comparison of those values. a subclass is deliberately not here:
 * it may have replaced the comparison, and `bool` reaches this from `x == 0` */
static inline int By_BothInlineInts(PyObject *a, PyObject *b) {
#if PY_VERSION_HEX >= 0x030C0000
    return BY_LIKELY(PyLong_CheckExact(a) && PyLong_CheckExact(b))
           && BY_LIKELY(PyUnstable_Long_IsCompact((PyLongObject *)a)
                        && PyUnstable_Long_IsCompact((PyLongObject *)b));
#else
    (void)a;
    (void)b;
    return 0;
#endif
}

/* `a <op> b` for a pair [`By_BothInlineInts`] answered yes for */
static inline int By_InlineIntCompare(PyObject *a, PyObject *b, int op) {
#if PY_VERSION_HEX >= 0x030C0000
    Py_ssize_t x = PyUnstable_Long_CompactValue((PyLongObject *)a);
    Py_ssize_t y = PyUnstable_Long_CompactValue((PyLongObject *)b);
    switch (op) {
        case Py_EQ: return x == y;
        case Py_NE: return x != y;
        case Py_LT: return x < y;
        case Py_LE: return x <= y;
        case Py_GT: return x > y;
        default: return x >= y;
    }
#else
    (void)a;
    (void)b;
    (void)op;
    return 0;
#endif
}

/* whether the whole of python's answer is what the *right* operand's type says
 *
 * a left operand whose type inherits `object`'s comparison has nothing to say about `==`
 * or `!=` beyond identity, so python's answer is whatever the right operand's type says,
 * and identity if that declines too. asking the right operand first is python's own order
 * wherever it reaches an answer at all: it asks the right side first when that side's type
 * derives from the left's, and otherwise asks it second, after a `NotImplemented` the
 * inherited comparison gives for every pair but an identical one — which is why identity
 * is left to the general path, along with the orderings, whose default is a `TypeError`
 * rather than an answer.
 *
 * this is a ladder of `case` arms against literals, where the subject is an instance of
 * some class and each literal is an `int` or a `str` that refuses it. it also skips the
 * recursion guard `PyObject_RichCompare` enters, which costs one level of headroom once
 * rather than a level per round of a recursion that goes through it — the call it makes
 * next is counted either way. see runtime.md#how-deep-a-recursion-goes */
static inline int By_RightOperandDecides(PyObject *a, PyObject *b, int op) {
    (void)b;
    return (op == Py_EQ || op == Py_NE) && a != b
           && Py_TYPE(a)->tp_richcompare == PyBaseObject_Type.tp_richcompare;
}

/* what the right operand's type answers for a pair [`By_RightOperandDecides`] said yes
 * for, or `Py_NotImplemented` where it has nothing to say — a new reference either way,
 * or NULL with an exception set
 *
 * `==` and `!=` are each their own reflection, so the operands swap and the operator
 * does not */
static inline PyObject *By_AskRightOperand(PyObject *a, PyObject *b, int op) {
    richcmpfunc other = Py_TYPE(b)->tp_richcompare;
    if (other == NULL) return Py_NewRef(Py_NotImplemented);
    return other(b, a, op);
}

/* `a <op> b` through the abstract protocol, as a bit — and 2 means an exception is set
 *
 * this is `PyObject_RichCompareBool` with its one shortcut removed. that function
 * answers `Py_EQ` on a pair of identical pointers `True` *before* it asks the type,
 * on the documented guarantee that identity implies equality — a guarantee ordinary
 * python does not make. a NaN is not equal to itself, and neither is an instance of
 * a class whose `__eq__` says so, so a comparison lowered onto it disagreed with the
 * interpreted twin while raising nothing and declining nothing.
 *
 * the shortcut is correct where the *interpreter* takes it, and it does take it in
 * places this must not disturb: `x in xs` and `xs.index(x)` reach it through
 * `PySequence_Contains` and `list.index`, and a dict lookup reaches it through
 * `lookdict`. those are answered by cpython's own code here — [`By_Contains`] and an
 * ordinary method call — so they keep it by construction and are not this helper's
 * to decide.
 *
 * what is left is the rest of that function: compare, then collapse the answer to a
 * bit. so the pointer test is the whole of what removing the shortcut costs.
 *
 * a NULL operand carries an exception set by whatever produced it, and the comparison
 * would dereference it — [`By_StrCompare`] hands its own such pair straight here */
static inline char By_ObjCompare(PyObject *a, PyObject *b, int op) {
    if (BY_UNLIKELY(a == NULL || b == NULL)) return 2;
    /* the bit is reached without a `bool` ever being made */
    if (By_BothInlineInts(a, b)) return (char)By_InlineIntCompare(a, b, op);
    if (By_RightOperandDecides(a, b, op)) {
        PyObject *answer = By_AskRightOperand(a, b, op);
        if (answer == NULL) return 2;
        if (answer != Py_NotImplemented) {
            int truth = PyObject_IsTrue(answer);
            Py_DECREF(answer);
            return truth < 0 ? 2 : (char)truth;
        }
        Py_DECREF(answer);
        /* the right operand declined too, so identity decides */
        return (char)(op == Py_NE);
    }
    PyObject *result = PyObject_RichCompare(a, b, op);
    if (result == NULL) return 2;
    int truth = PyObject_IsTrue(result);
    Py_DECREF(result);
    return truth < 0 ? 2 : (char)truth;
}

/* `a <op> b` through the abstract protocol, as the object python's comparison answered
 * — which need not be a `bool`, since a `__eq__` may answer anything at all
 *
 * this is `PyObject_RichCompare` with the same two questions asked in front of it that
 * [`By_ObjCompare`] asks, and each of them produces the very object that function would
 * have produced: `int`'s own comparison answers `Py_True` or `Py_False`, and a right
 * operand that answers is answering through the slot `do_richcompare` would have called.
 * where the right operand declines, python's own fallthrough for `==` and `!=` is
 * identity, and this pair is not identical
 *
 * a NULL operand carries an exception set by whatever produced it */
static inline PyObject *By_ObjRichCompare(PyObject *a, PyObject *b, int op) {
    if (BY_UNLIKELY(a == NULL || b == NULL)) return NULL;
    if (By_BothInlineInts(a, b)) {
        return Py_NewRef(By_InlineIntCompare(a, b, op) ? Py_True : Py_False);
    }
    if (By_RightOperandDecides(a, b, op)) {
        PyObject *answer = By_AskRightOperand(a, b, op);
        if (answer == NULL) return NULL;
        if (answer != Py_NotImplemented) return answer;
        Py_DECREF(answer);
        return Py_NewRef(op == Py_NE ? Py_True : Py_False);
    }
    return PyObject_RichCompare(a, b, op);
}

/* the type python names in an `AttributeError`
 *
 * the instance's *own* type, not the class that declared the field: a subclass
 * inherits the layout, and python names the subclass. a compiled type carries its
 * module in `tp_name` where a class defined in python does not, so it is trimmed
 * back to its tail */
static inline const char *By_TypeName(PyObject *o) {
    const char *name = Py_TYPE(o)->tp_name;
    const char *dot = strrchr(name, '.');
    return dot == NULL ? name : dot + 1;
}

/* raise python's `AttributeError` for an instance without a field of its layout
 *
 * out of line, because it is the rare branch beside every field read nothing proves
 * assigned, and building the message there made those reads' functions too big to
 * inline into their callers. `qualified` names the type in full, which is how a slot's
 * descriptor words it */
BY_COLD void By_FieldMissing(PyObject *o, const char *name, int qualified) {
    if (qualified) {
        PyErr_Format(PyExc_AttributeError, "'%T' object has no attribute '%s'", o, name);
    } else {
        PyErr_Format(PyExc_AttributeError, "'%s' object has no attribute '%s'", By_TypeName(o),
                     name);
    }
}

/* raise the `AttributeError` a descriptor with no setter raises, for a field that
 * cannot be written */
BY_COLD int By_FieldNotWritable(PyObject *o, const char *name) {
    PyErr_Format(PyExc_AttributeError, "attribute '%s' of '%s' objects is not writable", name,
                 Py_TYPE(o)->tp_name);
    return -1;
}

/* python truthiness, which can raise from a user `__bool__` or `__len__` */
static inline char By_Truthy(PyObject *o) {
    int result = PyObject_IsTrue(o);
    return result < 0 ? 2 : (char)result;
}

/* ── calling out of the compilation unit ──────────────────────────────────────
 *
 * a name the compiler does not own is resolved the way `LOAD_GLOBAL` resolves it:
 * the module's own namespace first — which the interpreted fallback populated —
 * then builtins. the result is cached per call site, because a module global is
 * not expected to be rebound underneath a running program
 */

/* whether a class may append storage to a base's instance
 *
 * PEP 697, so 3.12. below that there is no way to ask where such storage would be, and
 * a class needing one runs from its interpreted definition instead — decided at import
 * rather than when the C is written, so one build of it serves either interpreter */
#if PY_VERSION_HEX >= 0x030C0000
#define BY_HAS_TYPE_DATA 1
#define By_TypeData(obj, cls)                                                  \
    PyObject_GetTypeData((PyObject *)(obj), (PyTypeObject *)(cls))
#else
#define BY_HAS_TYPE_DATA 0
/* never reached: the type falls back before any instance of it exists */
#define By_TypeData(obj, cls) ((void *)(obj))
#endif

/* the flag naming a type whose instances keep their attributes *inline*, where reading
 * the dict is what builds it
 *
 * such a type is refused a licence outright: an inline type's dict does not exist until
 * something asks for it, so there is no word to read and asking is an allocation per
 * instance. 3.13 is where the two became separate flags — before it every managed dict
 * was an inline one, so naming the managed flag there names exactly the same set of
 * types */
#if PY_VERSION_HEX >= 0x030D0000
#define BY_INLINE_VALUES_FLAG Py_TPFLAGS_INLINE_VALUES
#elif PY_VERSION_HEX >= 0x030B0000
#define BY_INLINE_VALUES_FLAG Py_TPFLAGS_MANAGED_DICT
#else
#define BY_INLINE_VALUES_FLAG 0
#endif

/* the member type naming a `Py_ssize_t`, which is how a spec says where its dict is
 *
 * `PyType_Spec` has no field for `tp_dictoffset` and no slot id for it either. the one
 * way to set it on a type built from a spec is a member called `__dictoffset__`, which
 * `PyType_FromSpec` lifts out of the table rather than binding as an attribute. 3.12
 * renamed every member type and left the old spellings in `structmember.h` */
#if PY_VERSION_HEX >= 0x030C0000
#define BY_DICT_OFFSET_MEMBER Py_T_PYSSIZET
#define BY_DICT_OFFSET_FLAGS Py_READONLY
#else
#include <structmember.h>
#define BY_DICT_OFFSET_MEMBER T_PYSSIZET
#define BY_DICT_OFFSET_FLAGS READONLY
#endif

/* the flags a class asking for an instance dict declares
 *
 * the dict holds whatever was put in it, so the collector has to be able to walk it, and
 * the class hands over a traverse and a clear that reach it. where the dict itself sits
 * is not a flag: it is a word in the class's own struct, named to the type through
 * [`BY_DICT_OFFSET_MEMBER`], so that a call site can read it with one load */
#define BY_INSTANCE_DICT_FLAGS Py_TPFLAGS_HAVE_GC

/* reading a local on a path that never assigned it. the phrasing is the running
 * python's, not the compiler's — 3.11 rewrote it, and since 3.11 is the floor there is
 * only the one wording left to say */
static inline void By_RaiseUnboundLocal(const char *name) {
    PyErr_Format(PyExc_UnboundLocalError,
                 "cannot access local variable '%s' where it is not associated with a value",
                 name);
}

/* an interned string, built once per call site
 *
 * the length is passed rather than measured: a string literal is arbitrary text and
 * may contain a NUL, which every C-string form of the constructor reads as the end
 * of the string. the bytes the emitter writes are utf-8, which is what this decodes
 */
static inline PyObject *By_InternedStr(const char *data, Py_ssize_t size) {
    PyObject *text = PyUnicode_FromStringAndSize(data, size);
    if (text == NULL) return NULL;
    PyUnicode_InternInPlace(&text);
    return text;
}

/* a name a runtime helper always looks up under, interned once for the life of the
 * module and kept in the caller's own static
 *
 * `PyObject_GetAttrString` builds a fresh `str` for every call it is given. that
 * string is hashed from scratch, compared byte for byte in each dict along the mro,
 * and freed again — and its *address* is new every time, which is what the
 * interpreter's type attribute cache is keyed on, so a lookup made that way can never
 * hit that cache. an interned name carries its hash with it and is found by pointer.
 *
 * the reference is deliberately never released: the name is wanted for as long as the
 * module can run, and an interned string is one object however many times it is asked
 * for */
static inline PyObject *By_FixedName(PyObject **slot, const char *name, Py_ssize_t size) {
    if (*slot == NULL) *slot = By_InternedStr(name, size);
    return *slot;
}

/* the builtins namespace this module's compiled code falls back to
 *
 * one per emitted module, because each is its own translation unit — which is the
 * whole point: python gives a function the builtins of the module it was *defined*
 * in, and never the caller's. NULL until `By_BindBuiltins` has run, which `by_exec`
 * does before anything can read a global */
static PyObject *by_module_builtins = NULL;

/* ── one module object per process ───────────────────────────────────────────
 *
 * a compiled module keeps its namespace, and every memo of a name in it, in statics the
 * whole process shares. a second module object — `del sys.modules[name]` and an import,
 * or an import in another interpreter — would have those statics pointed at its own
 * namespace, and the first object's compiled code would read and write the second's.
 *
 * so the first module to finish executing is recorded in the interpreter that imported
 * it, and a later import there hands that module back, as it does for a single-phase C
 * extension. an import in any other interpreter is refused, since there is no second set
 * of statics to give it. the record is keyed by this build's module definition, so two
 * builds of one module name are never taken for each other */

/* the interpreter the module executed in, and NULL until it has */
static PyInterpreterState *by_module_home = NULL;

/* the interpreter-wide dict key the module is recorded under, new, or NULL on failure */
static PyObject *By_ModuleRecordKey(PyModuleDef *def) {
    return PyUnicode_FromFormat("by.module:%s:%p", def->m_name, (void *)def);
}

/* the module this interpreter already holds for `def`, borrowed, or NULL with no error
 * set where it holds none */
static PyObject *By_RecordedModule(PyModuleDef *def) {
    PyObject *registry = PyInterpreterState_GetDict(PyInterpreterState_Get());
    PyObject *key;
    PyObject *found;
    if (registry == NULL) return NULL;
    key = By_ModuleRecordKey(def);
    if (key == NULL) return NULL;
    found = PyDict_GetItemWithError(registry, key);
    Py_DECREF(key);
    return found;
}

/* `Py_mod_create`: the module already imported here, or a new one */
static PyObject *By_CreateModule(PyObject *spec, PyModuleDef *def) {
    PyInterpreterState *interp = PyInterpreterState_Get();
    PyObject *found;
    PyObject *name;
    PyObject *module;
    if (by_module_home != NULL && by_module_home != interp) {
        PyErr_Format(PyExc_ImportError,
                     "the compiled module '%s' is already imported in another interpreter, "
                     "and its state is shared by the whole process",
                     def->m_name);
        return NULL;
    }
    found = By_RecordedModule(def);
    if (found != NULL) {
        /* python forgets the state pointer of whatever module a create slot hands back —
         * `PyModule_FromDefAndSpec2` writes NULL over it the moment this returns, 3.11
         * through 3.15 — and the exec slot then allocates a fresh one. the state the module
         * was given at its first import would be left behind at every import after it, so
         * it is let go of here, just before python forgets it. this definition asks for no
         * state, so nothing reads it in between */
        void *state = PyModule_GetState(found);
        if (state != NULL) PyMem_Free(state);
        return By_NewRef(found);
    }
    if (PyErr_Occurred()) return NULL;
    name = PyObject_GetAttrString(spec, "name");
    if (name == NULL) return NULL;
    module = PyModule_NewObject(name);
    Py_DECREF(name);
    return module;
}

/* whether `module` is the one this interpreter already executed, whose exec slot is then
 * run again only because the import machinery hands it back to be executed */
static int By_ModuleExecuted(PyObject *module, PyModuleDef *def) {
    PyObject *found = By_RecordedModule(def);
    if (found == NULL && PyErr_Occurred()) return -1;
    return found == module;
}

/* record `module` as the one this interpreter holds, once its exec slot has succeeded */
static int By_RecordModule(PyObject *module, PyModuleDef *def) {
    PyObject *registry = PyInterpreterState_GetDict(PyInterpreterState_Get());
    PyObject *key;
    int failed;
    if (registry == NULL) {
        PyErr_SetString(PyExc_ImportError, "the interpreter has no dict to record a module in");
        return -1;
    }
    key = By_ModuleRecordKey(def);
    if (key == NULL) return -1;
    failed = PyDict_SetItem(registry, key, module);
    Py_DECREF(key);
    if (failed < 0) return -1;
    by_module_home = PyInterpreterState_Get();
    return 0;
}

/* bind that namespace, the way python binds one for an interpreted module
 *
 * python reaches a function's builtins through `__globals__['__builtins__']`, fixed
 * when the `def` ran. an extension module's namespace is not built by `exec`, so
 * nothing has put that entry there — and reading `PyEval_GetBuiltins` instead, which
 * answers about the frame currently running, would hand whichever frame happens to
 * call a compiled function the say over what `str` means. a compiled function pushes
 * no frame of its own, so that caller could be anyone.
 *
 * the entry is read once and not again, because python reads it once too: rebinding
 * `m.__dict__['__builtins__']` after the module has run leaves the functions it
 * already defined resolving through the namespace they were made with. mutating that
 * namespace is a different thing and is still seen, here as in python.
 *
 * the reference is kept for the life of the process rather than released with the
 * module: what reads it is compiled code, and a memo below borrows values out of it.
 *
 * an entry already standing in the namespace is deferred to rather than replaced,
 * which is python's rule and not an observed case: `by_exec` runs against a namespace
 * cpython has just built, so the read below always misses, and `importlib.reload` does
 * not run an extension's exec slot a second time. nothing reaches the two branches
 * that read it, so nothing tests them either — they are here because what a module's
 * builtins are is not this runtime's to decide */
static int By_BindBuiltins(PyObject *dict) {
    PyObject *stood;
    if (dict == NULL) return -1;
    stood = PyDict_GetItemString(dict, "__builtins__");
    if (stood == NULL) {
        /* what python's own `exec` fills in for a namespace that carries no entry,
         * and so what a module imported the ordinary way ends up holding */
        stood = PyEval_GetBuiltins();
        if (stood == NULL || PyDict_SetItemString(dict, "__builtins__", stood) < 0) {
            if (!PyErr_Occurred()) {
                PyErr_SetString(PyExc_ImportError,
                                "no builtins namespace to resolve this module's globals in");
            }
            return -1;
        }
    }
    /* python accepts the entry as either the `builtins` module or its dict */
    if (PyModule_Check(stood)) stood = PyModule_GetDict(stood);
    if (stood == NULL || !PyDict_Check(stood)) {
        PyErr_SetString(PyExc_ImportError,
                        "this module's `__builtins__` is neither a module nor a dict");
        return -1;
    }
    by_module_builtins = Py_NewRef(stood);
    return 0;
}

/* the two namespaces a global can be answered by, in order, reporting which one did
 *
 * the name arrives already interned, because this is on the path of every read of
 * a global. `PyDict_GetItemString` builds a fresh `str` and hashes it on each
 * call — twice over when the answer is a builtin, since the module namespace has
 * to miss first — where an interned key carries its hash and settles a
 * unicode-keyed dict on a pointer compare.
 *
 * both lookups are made every time, and that is not the slow half: a module that
 * rebinds a builtin name means it, and python would see it.
 *
 * the answer is borrowed, and `answered` is left NULL where the name is bound in
 * neither namespace. this is the single place either lookup path decides what
 * builtins are, so a build with the memo below and a build without it — an
 * interpreter too old for dict watchers — cannot drift apart on the question */
static inline PyObject *By_ProbeGlobal(PyObject *dict, PyObject *name,
                                       PyObject **answered) {
    PyObject *value;
    *answered = NULL;
    if (name == NULL) return NULL;
    value = dict == NULL ? NULL : PyDict_GetItemWithError(dict, name);
    if (value != NULL) {
        *answered = dict;
        return value;
    }
    if (PyErr_Occurred() || by_module_builtins == NULL) return NULL;
    value = PyDict_GetItemWithError(by_module_builtins, name);
    if (value != NULL) *answered = by_module_builtins;
    return value;
}

/* resolve a name the frame does not bind, in full */
static inline PyObject *By_LookupGlobal(PyObject *dict, PyObject *name) {
    PyObject *answered;
    PyObject *value = By_ProbeGlobal(dict, name, &answered);
    if (value == NULL) {
        if (PyErr_Occurred()) return NULL;
        if (name == NULL) return NULL;
        PyErr_Format(PyExc_NameError, "name '%U' is not defined", name);
        return NULL;
    }
    Py_INCREF(value);
    return value;
}

/* the same resolution for a name that arrives as a C string
 *
 * every caller of this form is module init — a class body, an import, a decorator
 * — where interning once per call is not worth a slot to hold it. measuring the
 * name is safe where measuring a literal is not: what a frame can bind is an
 * identifier, and an identifier holds no NUL */
static inline PyObject *By_LookupGlobalString(PyObject *dict, const char *name) {
    PyObject *key = By_InternedStr(name, (Py_ssize_t)strlen(name));
    PyObject *value;
    if (key == NULL) return NULL;
    value = By_LookupGlobal(dict, key);
    Py_DECREF(key);
    return value;
}

/* `root.a.b`: the root the way `LOAD_GLOBAL` resolves it, then a `getattr` each
 *
 * this is what a decorator expression written as a chain of attributes does, and all
 * of what it does — every step is a read, which is why evaluating it at module init
 * rather than where the `def` stood is faithful. a python identifier holds no `.`, so
 * the path arrives as one string and is split back apart here */
static inline PyObject *By_LookupDotted(PyObject *dict, const char *path) {
    const char *dot = strchr(path, '.');
    PyObject *value;
    if (dot == NULL) return By_LookupGlobalString(dict, path);
    {
        PyObject *key = By_InternedStr(path, (Py_ssize_t)(dot - path));
        if (key == NULL) return NULL;
        value = By_LookupGlobal(dict, key);
        Py_DECREF(key);
    }
    while (value != NULL && dot != NULL) {
        const char *segment = dot + 1;
        const char *next = strchr(segment, '.');
        Py_ssize_t length = next == NULL ? (Py_ssize_t)strlen(segment)
                                         : (Py_ssize_t)(next - segment);
        PyObject *attr = By_InternedStr(segment, length);
        PyObject *got;
        if (attr == NULL) {
            Py_DECREF(value);
            return NULL;
        }
        got = PyObject_GetAttr(value, attr);
        Py_DECREF(attr);
        Py_DECREF(value);
        value = got;
        dot = next;
    }
    return value;
}

/* bind a name in the module namespace: an assignment under a `global` declaration
 *
 * this is the write `By_LookupGlobal` is the read of, and it has to reach the same
 * dict. binding a register instead would keep the new value to the frame, where
 * python's binding is the module's — every other reader sees it at once, the
 * interpreted twin included, since that twin's `__globals__` *is* this dict.
 *
 * builtins are pointedly not consulted: python's `STORE_GLOBAL` binds in the module
 * namespace whether or not the name already resolved to a builtin */
static inline char By_StoreGlobal(PyObject *dict, PyObject *name, PyObject *value) {
    if (dict == NULL || name == NULL || value == NULL) return 2;
    return PyDict_SetItem(dict, name, value) < 0 ? 2 : 0;
}

/* unbind a name in the module namespace: `del x` under a `global x`
 *
 * a dict raises `KeyError` for a key it does not hold and python raises `NameError`
 * for a name it does not bind, so the one has to be translated into the other */
static inline char By_DeleteGlobal(PyObject *dict, PyObject *name) {
    if (dict == NULL || name == NULL) return 2;
    if (PyDict_DelItem(dict, name) < 0) {
        if (PyErr_ExceptionMatches(PyExc_KeyError)) {
            PyErr_Clear();
            PyErr_Format(PyExc_NameError, "name '%U' is not defined", name);
        }
        return 2;
    }
    return 0;
}

/* whether `warn` walks past a file rather than blaming it
 *
 * `setup_context` never reports a warning against the import machinery: while counting
 * `stacklevel` frames it steps over every frame whose file name holds both "importlib"
 * and "_bootstrap", so a module warned about during its own import is blamed on
 * whoever asked for the import. the test is on the file name alone, so a file named
 * anything else that happens to hold both words is walked past too — python's rule
 * reproduced, not improved on.
 *
 * checked against 3.11 through 3.15: a file holding only one of the two words is
 * blamed like any other */
static inline int By_WarnWalksPast(PyObject *filename) {
    PyObject *needle;
    int held;

    if (filename == NULL || !PyUnicode_Check(filename)) return 0;
    needle = By_InternedStr("importlib", 9);
    if (needle == NULL) {
        PyErr_Clear();
        return 0;
    }
    held = PyUnicode_Contains(filename, needle);
    Py_DECREF(needle);
    if (held <= 0) {
        if (held < 0) PyErr_Clear();
        return 0;
    }
    needle = By_InternedStr("_bootstrap", 10);
    if (needle == NULL) {
        PyErr_Clear();
        return 0;
    }
    held = PyUnicode_Contains(filename, needle);
    Py_DECREF(needle);
    if (held < 0) {
        PyErr_Clear();
        return 0;
    }
    return held > 0;
}

static inline int By_WarnWalksPastFrame(PyFrameObject *frame) {
    PyCodeObject *code;
    int past;

    if (frame == NULL) return 0;
    code = PyFrame_GetCode(frame);
    if (code == NULL) return 0;
    past = By_WarnWalksPast(code->co_filename);
    Py_DECREF(code);
    return past;
}

/* what the code object of a published forwarder says its file is
 *
 * kept in step with `by_irbuild::shims::SHIM_FILE`, which is what writes it — a
 * test in `by_build` fails if the two ever disagree */
#define BY_FORWARDER_FILE "<by native forwarder>"

/* the frame `warn` would have blamed at a stack level above one, as a new reference
 *
 * python starts the count at the frame the warning is written in and steps back
 * `stacklevel - 1` times. that first frame is the compiled function's own and does not
 * exist — but every frame below it does, and it is exactly the frame the interpreted
 * definition's own would have sat on, which is what `PyEval_GetFrame` answers with. so
 * the walk starts with its first step already taken, and takes `stacklevel - 2` more.
 *
 * `plain` is python's own branch, decided by the frame the count starts from: a
 * warning written in the import machinery counts plain frames, and one written
 * anywhere else walks past the machinery. the frame it starts from is the compiled
 * function's, so which branch applies is settled by the module's own file name.
 *
 * NULL for a walk that ran off the end, which python reports against `sys` rather than
 * treating as an error */
static inline PyFrameObject *By_WarnFrame(int stacklevel, int plain) {
    PyFrameObject *frame = PyEval_GetFrame();
    int step;

    Py_XINCREF(frame);
    /* the forwarder a module publishes under this function's name *is* a python
     * frame, and it stands exactly where the compiled function's own frame would
     * have stood — so the walk above has already counted it, and it has to be
     * stepped over for the count to start from the caller the way it says it does.
     * only the topmost one: a forwarder further down stands for a compiled frame
     * that is genuinely between here and the caller */
    if (frame != NULL) {
        PyCodeObject *code = PyFrame_GetCode(frame);
        if (code != NULL) {
            int forwarder = PyUnicode_CompareWithASCIIString(code->co_filename,
                                                             BY_FORWARDER_FILE) == 0;
            Py_DECREF(code);
            if (forwarder) {
                PyFrameObject *back = PyFrame_GetBack(frame);
                Py_DECREF(frame);
                frame = back;
            }
        }
    }
    for (step = 1;; step++) {
        if (!plain) {
            while (frame != NULL && By_WarnWalksPastFrame(frame)) {
                PyFrameObject *back = PyFrame_GetBack(frame);
                Py_DECREF(frame);
                frame = back;
            }
        }
        if (step >= stacklevel - 1 || frame == NULL) return frame;
        {
            PyFrameObject *back = PyFrame_GetBack(frame);
            Py_DECREF(frame);
            frame = back;
        }
    }
}

/* what `warn` reports a warning against once it has run out of frames
 *
 * the wording moved in 3.13: the file was `sys` at line 1 and is now `<sys>` at line
 * zero. the namespace stayed `sys`'s own either way, so the blamed module is `sys` on
 * every version */
#if PY_VERSION_HEX >= 0x030D0000
#define BY_WARN_OFF_THE_END_FILE "<sys>"
#define BY_WARN_OFF_THE_END_LINE 0
#else
#define BY_WARN_OFF_THE_END_FILE "sys"
#define BY_WARN_OFF_THE_END_LINE 1
#endif

/* `warnings.warn(message, category, stacklevel)` written out with the context supplied
 *
 * `warn` decides which module a warning is blamed on by counting frames back from its
 * own caller, and a compiled function pushes none — so the frame it lands on belongs
 * to the caller, in another module. that decides the `file:line` the message prints
 * and, through the blamed module's `__name__`, whether the filters show the warning at
 * all: `urllib.request.URLopener.__init__` warned and the compiled leg printed nothing
 * where python printed a `DeprecationWarning`.
 *
 * the missing frame is the *only* difference, and it is the one this fills in. at the
 * default stack level the frame `warn` would have blamed is the compiled function's
 * own, and every field it would have read off it is either known when the module is
 * built — the file and the line — or sitting in the module namespace at the moment of
 * the call: `__name__`, and the `__warningregistry__` that records what has already
 * been shown. above the default level the blamed frame is further out, and every frame
 * out there is a real one: the interpreted definition's frame would have sat directly
 * on `PyEval_GetFrame`, so counting from there with the first step taken reaches the
 * frame python reaches. either way the call is made to `warn_explicit`, which takes
 * the context, and no frame of this function's is needed.
 *
 * what a compiled function *below* this one would have contributed is still missing,
 * and no context filled in here can supply it. a caller in the same module is turned
 * down by the compiler for that reason; one reached through a value or from another
 * module is a frame neither side can see, and an interpreted definition of this
 * function would lose it just the same.
 *
 * what follows is `warn`'s own preamble, which has to be reproduced exactly or the
 * lowering is distinguishable. every branch was read off cpython 3.11 through 3.15
 * rather than off `warnings.py`, because the module that actually answers is the C
 * accelerator `_warnings` and the two differ:
 *
 *   - a `Warning` *instance* supplies the category, overriding whatever was written.
 *     the test was `PyObject_IsInstance`, which honours a `__class__` property, but the
 *     category taken is `Py_TYPE(message)` — the real type. an object that lied about
 *     its class therefore reached the subclass test as its true type and was refused.
 *     3.15 made this a type check too, so such an object is now warned about as an
 *     ordinary message under the category the call wrote
 *   - a failure inside that instance test propagates, where a failure inside the
 *     subclass test below is swallowed and reworded. from 3.15 neither test asks
 *     anything that can fail
 *   - the subclass test was `PyObject_IsSubclass` rather than a type check, so a
 *     non-type with a `__bases__` naming `Warning` passed it and failed later at the
 *     call. 3.15 turned that into a type check, and reworded the refusal at the same
 *     time — both are gated below
 *   - `module_globals` is left out. `warnings.py` passes the namespace there, but the
 *     C `warn` passes nothing, and the difference is visible: the namespace reaches
 *     `linecache`, which warns about a module without a `__spec__.loader`
 *
 * `PyErr_WarnExplicitObject` is the public spelling of `warn_explicit` and takes no
 * `source`, so a call that writes one is left to its interpreted definition */
static inline PyObject *By_Warn(PyObject *message, PyObject *category,
                                PyObject *module_dict, const char *filename,
                                int lineno, int stacklevel) {
    PyObject *chosen = category;
    PyObject *globals = NULL;
    PyObject *key;
    PyObject *name;
    PyObject *module;
    PyObject *registry;
    PyObject *path = NULL;
    int rc;

    /* 3.15 made this a real type check as well, so a message whose `__class__` claims
     * to be a warning stops supplying the category — and a `__class__` that raises
     * stops being asked at all */
#if PY_VERSION_HEX >= 0x030F0000
    rc = PyObject_TypeCheck(message, (PyTypeObject *)PyExc_Warning);
#else
    rc = PyObject_IsInstance(message, PyExc_Warning);
    if (rc < 0) return NULL;
#endif
    if (rc > 0) {
        chosen = (PyObject *)Py_TYPE(message);
    } else if (chosen == NULL || chosen == Py_None) {
        chosen = PyExc_UserWarning;
    }
    /* 3.15 stopped honouring a `__bases__` naming `Warning` on something that is not a
     * class: a category has to be a real type there, where before one of these passed
     * this test and failed later at the call */
#if PY_VERSION_HEX >= 0x030F0000
    rc = PyType_Check(chosen) ? PyObject_IsSubclass(chosen, PyExc_Warning) : 0;
#else
    rc = PyObject_IsSubclass(chosen, PyExc_Warning);
#endif
    if (rc < 0) {
        PyErr_Clear();
        rc = 0;
    }
    if (rc == 0) {
        /* 3.15 rewrote this wording, and rewrote it in two directions: a category that
         * is a class is named as one rather than as `type`, and either way the name is
         * the fully qualified one — the module and the qualified name, with `builtins`
         * and `__main__` left off, which is what `%N` and `%T` spell */
#if PY_VERSION_HEX >= 0x030F0000
        if (PyType_Check(chosen)) {
            PyErr_Format(PyExc_TypeError,
                         "category must be a Warning subclass, not class '%N'", chosen);
        } else {
            PyErr_Format(PyExc_TypeError,
                         "category must be a Warning subclass, not '%T'", chosen);
        }
#else
        PyErr_Format(PyExc_TypeError,
                     "category must be a Warning subclass, not '%s'",
                     Py_TYPE(chosen)->tp_name);
#endif
        return NULL;
    }

    if (stacklevel <= 1) {
        if (module_dict == NULL) {
            PyErr_SetString(PyExc_SystemError, "a warning has no module namespace");
            return NULL;
        }
        globals = Py_NewRef(module_dict);
        path = PyUnicode_DecodeUTF8(filename, (Py_ssize_t)strlen(filename), "replace");
    } else {
        PyObject *written = PyUnicode_DecodeUTF8(filename, (Py_ssize_t)strlen(filename),
                                                 "replace");
        PyFrameObject *frame;
        if (written == NULL) return NULL;
        frame = By_WarnFrame(stacklevel, By_WarnWalksPast(written));
        Py_DECREF(written);
        if (frame == NULL) {
            /* python reads the namespace off the interpreter's own `sys` here, which
             * is `sys.__dict__` and holds the `__name__` the blamed module is taken
             * from */
            PyObject *sys = PyImport_ImportModule("sys");
            if (sys != NULL) {
                PyObject *dict = PyModule_GetDict(sys);
                globals = Py_XNewRef(dict);
                Py_DECREF(sys);
            }
            path = PyUnicode_FromString(BY_WARN_OFF_THE_END_FILE);
            lineno = BY_WARN_OFF_THE_END_LINE;
        } else {
            PyCodeObject *code = PyFrame_GetCode(frame);
            globals = PyFrame_GetGlobals(frame);
            if (code != NULL) {
                path = Py_NewRef(code->co_filename);
                Py_DECREF(code);
            }
            lineno = PyFrame_GetLineNumber(frame);
            Py_DECREF(frame);
        }
    }
    if (globals == NULL || path == NULL) {
        Py_XDECREF(globals);
        Py_XDECREF(path);
        if (!PyErr_Occurred()) {
            PyErr_SetString(PyExc_SystemError, "a warning has no module namespace");
        }
        return NULL;
    }

    /* the module the warning is blamed on, exactly as `warn` reads it off the frame's
     * globals: `__name__` when it is a string or `None`, and `"<string>"` for anything
     * else, a missing name included. `None` is deliberately passed through rather than
     * replaced — a `module` filter matches neither, but leaving it out of the call
     * entirely would make `warn_explicit` derive one from the file name instead */
    key = By_InternedStr("__name__", 8);
    if (key == NULL) {
        Py_DECREF(globals);
        Py_DECREF(path);
        return NULL;
    }
    name = PyDict_GetItemWithError(globals, key);
    Py_DECREF(key);
    if (name == NULL && PyErr_Occurred()) {
        Py_DECREF(globals);
        Py_DECREF(path);
        return NULL;
    }
    if (name == Py_None || (name != NULL && PyUnicode_Check(name))) {
        module = Py_NewRef(name);
    } else {
        module = PyUnicode_FromString("<string>");
        if (module == NULL) {
            Py_DECREF(globals);
            Py_DECREF(path);
            return NULL;
        }
    }

    /* the registry is the blamed module's own, created on first use the way `warn`
     * creates it. whatever is already bound is passed on untouched, non-dicts included,
     * so a module that has bound something else there raises what python raises */
    key = By_InternedStr("__warningregistry__", 19);
    if (key == NULL) {
        Py_DECREF(module);
        Py_DECREF(globals);
        Py_DECREF(path);
        return NULL;
    }
    registry = PyDict_GetItemWithError(globals, key);
    if (registry == NULL && !PyErr_Occurred()) {
        registry = PyDict_New();
        if (registry != NULL && PyDict_SetItem(globals, key, registry) < 0) {
            Py_CLEAR(registry);
        }
    } else {
        Py_XINCREF(registry);
    }
    Py_DECREF(key);
    Py_DECREF(globals);
    if (registry == NULL) {
        Py_DECREF(module);
        Py_DECREF(path);
        return NULL;
    }

    rc = PyErr_WarnExplicitObject(chosen, message, path, lineno, module, registry);
    Py_DECREF(path);
    Py_DECREF(registry);
    Py_DECREF(module);
    if (rc < 0) return NULL;
    return Py_NewRef(Py_None);
}

/* ── recursion depth ──────────────────────────────────────────────────────────
 *
 * a compiled call pushes a C frame and no python frame, so nothing the interpreter
 * counts stands between a compiled recursion and the end of the stack: `depth(10**6)`
 * segfaulted where python raises `RecursionError`. two things are watched instead, at
 * every call that can go round a cycle of compiled calls and at every resumption of a
 * compiled generator or coroutine.
 *
 * the stack itself, always. python frames live on the heap, and a program that raises
 * the recursion limit is allowed as deep as its memory goes, where a C frame cannot go
 * past the stack its thread was given. so the stack is a limit of its own, and the
 * thing that turns running out of it into `RecursionError` rather than a crash.
 *
 * python's recursion limit, unless the build was told not to follow it. the count is
 * the interpreter's own `py_recursion_remaining`, taken and given back exactly as a
 * python frame takes and gives it back, so `sys.setrecursionlimit` reaches a compiled
 * recursion and python frames and compiled ones share one budget. a build that defines
 * `BY_RECURSION_STACK_ONLY` counts nothing and watches the stack alone.
 *
 * `ByDepth` is what a function on a cycle is handed by its caller, so a call round the
 * cycle reads nothing the callee could not have been given: the thread state and the
 * stack limit are looked up once, where the cycle is entered */

/* the lowest address a compiled frame may start below the top of, per thread
 *
 * zero until the thread first asks. a quarter of the thread's stack is left under it,
 * which is the room whatever runs past the last check — an acyclic chain of compiled
 * calls, the interpreter raising the error and building its traceback, a C api the
 * last frame called — has to finish in.
 *
 * windows keeps no copy, and asks the thread each time instead. a thread-local is the
 * only place a copy per thread could live, and the compiler an interpreter without
 * `sysconfig`'s `CC` is built against is usually mingw's gcc, which makes `_Thread_local`
 * out of emulated tls in libgcc's own dll. python does not search `PATH` for an
 * extension's dependencies, so every module that asked failed to load */
#if !defined(_WIN32)
static _Thread_local uintptr_t by_stack_limit = 0;
#endif

#if defined(__APPLE__)
#include <pthread.h>
#elif defined(__FreeBSD__)
#include <pthread.h>
#include <pthread_np.h>
#elif defined(__linux__) && (defined(__GLIBC__) || defined(__BIONIC__))
#include <pthread.h>
#elif defined(_WIN32)
/* declared rather than taken from `windows.h`, whose macros collide with names in
 * this header. it is `kernel32`'s, which every windows process links, and its words are
 * `ULONG_PTR`s, which is not `uintptr_t` on a 32-bit build */
#if defined(_WIN64)
typedef unsigned long long ByStackWord;
#else
typedef unsigned long ByStackWord;
#endif
__declspec(dllimport) void __stdcall GetCurrentThreadStackLimits(ByStackWord *low, ByStackWord *high);
#endif

/* where this frame is on the stack. every platform a python wheel is built for grows
 * its stack downwards, which is what every comparison below assumes */
BY_HOT uintptr_t By_StackHere(void) {
#if defined(__GNUC__) || defined(__clang__)
    return (uintptr_t)__builtin_frame_address(0);
#else
    char here = 0;
    return (uintptr_t)&here;
#endif
}

#if defined(_WIN32)

/* the thread's own information block holds both ends of the stack reserved for it, so
 * there is always an answer, and reading it is two loads behind a call */
static inline uintptr_t By_StackLimit(void) {
    ByStackWord low = 0;
    ByStackWord high = 0;
    GetCurrentThreadStackLimits(&low, &high);
    return (uintptr_t)low + (uintptr_t)(high - low) / 4;
}

#else

/* ask the platform how big this thread's stack is, once
 *
 * musl is left to the estimate: its answer for the main thread is the part of the stack
 * already mapped rather than the part the thread may grow into, which cpython found
 * imposes a limit far below the real one. an estimate is also what a platform with no
 * way to ask gets — the stack a thread is typically given, measured down from the
 * frame that asked first, which is not far below the top of the stack */
BY_COLD uintptr_t By_ReadStackLimit(void) {
    uintptr_t low = 0;
    uintptr_t size = 0;
#if defined(__APPLE__)
    pthread_t self = pthread_self();
    size = (uintptr_t)pthread_get_stacksize_np(self);
    low = (uintptr_t)pthread_get_stackaddr_np(self) - size;
#elif defined(__FreeBSD__) || (defined(__linux__) && (defined(__GLIBC__) || defined(__BIONIC__)))
    pthread_attr_t attr;
    void *address = NULL;
    size_t length = 0;
    size_t guard = 0;
    int failed;
#if defined(__FreeBSD__)
    failed = pthread_attr_init(&attr);
    if (failed == 0) failed = pthread_attr_get_np(pthread_self(), &attr);
#else
    failed = pthread_getattr_np(pthread_self(), &attr);
#endif
    if (failed == 0) {
        failed = pthread_attr_getstack(&attr, &address, &length);
        if (failed == 0 && pthread_attr_getguardsize(&attr, &guard) != 0) guard = 0;
        pthread_attr_destroy(&attr);
    }
    if (failed == 0 && length > guard) {
        low = (uintptr_t)address + guard;
        size = (uintptr_t)(length - guard);
    }
#endif
    if (size == 0) {
        size = (uintptr_t)1 << 20;
        low = By_StackHere() - size;
    }
    by_stack_limit = low + size / 4;
    return by_stack_limit;
}

static inline uintptr_t By_StackLimit(void) {
    uintptr_t limit = by_stack_limit;
    if (BY_UNLIKELY(limit == 0)) limit = By_ReadStackLimit();
    return limit;
}

#endif

/* the thread's own count of the frames it may still push, which 3.12 split out of the
 * count it had shared with C calls */
#if PY_VERSION_HEX >= 0x030C0000
#define BY_PY_RECURSION_REMAINING py_recursion_remaining
#else
#define BY_PY_RECURSION_REMAINING recursion_remaining
#endif

BY_COLD char By_StackExhausted(void) {
    PyErr_SetString(PyExc_RecursionError, "maximum recursion depth exceeded");
    return 1;
}

#ifdef BY_RECURSION_STACK_ONLY

typedef uintptr_t ByDepth;

static inline ByDepth By_DepthHere(void) { return By_StackLimit(); }

/* nonzero, with `RecursionError` set, where the frame about to be pushed would start
 * too near the end of the stack */
BY_HOT char By_DepthEnter(ByDepth depth) {
    if (BY_UNLIKELY(By_StackHere() < depth)) return By_StackExhausted();
    return 0;
}

BY_HOT void By_DepthLeave(ByDepth depth) { (void)depth; }

#else

typedef struct {
    PyThreadState *thread;
    uintptr_t stack_limit;
} ByDepth;

static inline ByDepth By_DepthHere(void) {
    ByDepth depth;
#if PY_VERSION_HEX >= 0x030D0000
    depth.thread = PyThreadState_GetUnchecked();
#else
    depth.thread = _PyThreadState_UncheckedGet();
#endif
    depth.stack_limit = By_StackLimit();
    return depth;
}

/* the count ran out: the interpreter's `_Py_CheckRecursiveCallPy`, on a frame whose
 * count has already been taken
 *
 * while the thread is already raising one of these it is allowed fifty frames more, as
 * python allows them. python aborts the process past those; this raises again instead,
 * which is the one place it parts from the interpreter, and a crash is not an answer.
 * a refusal gives the count back, because the frame it was taken for is never pushed */
BY_COLD char By_RecursionLimitReached(PyThreadState *thread) {
    if (thread->recursion_headroom && thread->BY_PY_RECURSION_REMAINING >= -50) return 0;
    thread->BY_PY_RECURSION_REMAINING++;
    thread->recursion_headroom++;
    PyErr_SetString(PyExc_RecursionError, "maximum recursion depth exceeded");
    thread->recursion_headroom--;
    return 1;
}

/* take one frame from the count and check the stack, as python does on pushing a frame.
 * nonzero, with `RecursionError` set and nothing taken, where either has run out */
BY_HOT char By_DepthEnter(ByDepth depth) {
    if (BY_UNLIKELY(By_StackHere() < depth.stack_limit)) return By_StackExhausted();
    if (BY_UNLIKELY(--depth.thread->BY_PY_RECURSION_REMAINING < 0)) {
        return By_RecursionLimitReached(depth.thread);
    }
    return 0;
}

/* give back the frame an `By_DepthEnter` that succeeded took */
BY_HOT void By_DepthLeave(ByDepth depth) { depth.thread->BY_PY_RECURSION_REMAINING++; }

#endif /* BY_RECURSION_STACK_ONLY */

/* a global read that remembers what the name resolved to last time
 *
 * `By_LookupGlobal` is two dict probes on every trip, and for a name that resolves to
 * a builtin — `str`, `len`, an exception class — the module namespace has to miss
 * before the second one is even reached. that is a quarter of `keybuild` and of
 * `excs`, and a twelfth of `words`, against a ±1% floor two identical builds measured
 * for themselves; nothing else in the suite resolves a global often enough to notice
 * either way.
 *
 * the memo below is a cache with a validity test, not an assumption: a name that is
 * rebound is seen at once, which is what separates this from the early binding that
 * tier 3 gets to make. what makes the test cheap is that it asks about the *dicts*
 * rather than about the name — one counter, bumped by a dict watcher whenever any
 * namespace this process ever resolved a global through is written to.
 *
 * the counter is deliberately one number for the whole interpreter rather than one
 * per dict. a write to any module's namespace therefore re-resolves every memo in
 * the process, which is far more invalidation than is needed — but a memo can never
 * be left holding an answer from a dict that has since moved on, and the cost of
 * over-invalidating is one lookup, where the cost of under-invalidating is a silent
 * wrong answer.
 *
 * the counter is also the *whole* of the test, which it can only be because there are
 * exactly two dicts a site's answer can come from and both are fixed for the life of
 * the module: `by_module_dict`, and the `by_module_builtins` bound beside it. neither
 * can be swapped underneath a call — a compiled function reads them out of statics
 * rather than out of a frame — so every way an answer can go stale is a write to one
 * of those two dicts, and a write is exactly what the counter reports. an arm that
 * cannot get a namespace watched leaves the site cold rather than trusting it */
typedef struct {
    /* the counter's value when this was armed. zero is "cold": the counter starts
     * at one and only ever rises, so a cold site can never match it */
    uint64_t generation;
    /* borrowed from whichever dict answered: the dict holds the reference, and the
     * only way the value can leave that dict is an event that bumps the counter */
    PyObject *value;
} ByGlobalSite;

#define BY_GLOBAL_SITE_INIT { 0u, NULL }

/* dict watchers arrived in 3.12, and there is no other way to be told that a
 * namespace was written to that does not read a field deprecated for exactly this
 * use. below that version every lookup is made in full */
#if !defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030C0000
#define BY_GLOBAL_SITES 1
#endif

#ifdef BY_GLOBAL_SITES

/* what every memo in one interpreter is invalidated by
 *
 * cpython hands out eight dict watchers per interpreter and a project has more
 * modules than that, so a module that registered its own would be the eighth one to
 * work. the registration is made once and found again through the interpreter's own
 * dictionary, which is where an extension module is meant to keep what it shares
 * with other extension modules */
typedef struct {
    uint64_t generation;
    int watcher;
} ByGlobals;

/* what this module's own sites read
 *
 * it always names a struct, so that a site's test is the one compare it is about rather
 * than that compare behind a test for whether there is anything to compare against. a
 * module that memoises nothing is pointed at the struct below instead, which carries a
 * generation nothing ever stamps: the counter starts at one and is moved by one write
 * to a namespace at a time, so reaching this value would take more writes than a
 * process can make, and a cold site's zero is no nearer it. so every site misses, on
 * every read, which is what "memoises nothing" has to mean.
 *
 * its watcher is the identifier `PyDict_AddWatcher` never hands out, and nothing reads
 * it: every path to the watcher asks [`BY_GLOBALS_FOUND`] first */
static ByGlobals by_globals_off = { UINT64_MAX, -1 };

static ByGlobals *by_globals = &by_globals_off;

/* whether this module found a counter to share, which is the question the old spelling
 * `by_globals != NULL` asked */
#define BY_GLOBALS_FOUND (by_globals != &by_globals_off)

/* what this module's callback bumps, if this is the module that registered the
 * watcher. it is deliberately *not* the same variable
 *
 * only one module in an interpreter registers, and every other module's sites are
 * invalidated by that one module's callback. so a module that gives up memoising —
 * by pointing `by_globals` back at the off struct — must not be able to take the
 * invalidation of everyone else's sites with it. this is set once, when the
 * registration succeeds, and never cleared */
static ByGlobals *by_globals_watched = NULL;

static int By_GlobalsChanged(PyDict_WatchEvent event, PyObject *dict, PyObject *key,
                             PyObject *value) {
    (void)event;
    (void)dict;
    (void)key;
    (void)value;
    if (by_globals_watched != NULL) by_globals_watched->generation += 1;
    return 0;
}

/* find this interpreter's counter, registering the watcher the first time
 *
 * the struct outlives every module that reaches it, on purpose: the capsule is given
 * no destructor. the readers are compiled code in every module that found it, and
 * freeing it while any of them could still run would turn a memo into a read of
 * freed memory. it is two words per interpreter.
 *
 * a failure at any step is answered by leaving `by_globals` at the off struct, whose
 * generation no site can match — a build that cannot be told about writes makes every
 * lookup in full rather than making one it cannot invalidate */
static void By_FindGlobals(void) {
    static const char *key = "_by_global_memo";
    PyInterpreterState *interpreter;
    PyObject *state;
    PyObject *capsule;
    ByGlobals *shared;
    if (BY_GLOBALS_FOUND) return;
    interpreter = PyInterpreterState_Get();
    if (interpreter == NULL) return;
    state = PyInterpreterState_GetDict(interpreter);
    if (state == NULL) return;
    capsule = PyDict_GetItemString(state, key);
    if (capsule != NULL) {
        by_globals = (ByGlobals *)PyCapsule_GetPointer(capsule, key);
        if (by_globals == NULL) {
            by_globals = &by_globals_off;
            PyErr_Clear();
        }
        return;
    }
    PyErr_Clear();
    shared = (ByGlobals *)PyMem_RawMalloc(sizeof(ByGlobals));
    if (shared == NULL) return;
    /* one, not zero, so that a cold site's zero can never be mistaken for a match */
    shared->generation = 1u;
    shared->watcher = PyDict_AddWatcher(By_GlobalsChanged);
    if (shared->watcher < 0) {
        PyErr_Clear();
        PyMem_RawFree(shared);
        return;
    }
    by_globals_watched = shared;
    capsule = PyCapsule_New(shared, key, NULL);
    if (capsule == NULL) {
        PyErr_Clear();
        /* the watcher cannot be handed back, so the struct is kept and serves this
         * module alone rather than being freed out from under a live registration */
        by_globals = shared;
        return;
    }
    if (PyDict_SetItemString(state, key, capsule) < 0) PyErr_Clear();
    Py_DECREF(capsule);
    by_globals = shared;
}

/* start watching a namespace, and refuse to memoise anything resolved through one
 * that will not be watched
 *
 * every dict a site's answer can come from passes through here first, so a write
 * that this build would not hear about is a write no site was armed against */
static int By_WatchGlobals(PyObject *dict) {
    if (!BY_GLOBALS_FOUND || dict == NULL || !PyDict_Check(dict)) return 0;
    if (PyDict_Watch(by_globals->watcher, dict) < 0) {
        PyErr_Clear();
        return 0;
    }
    return 1;
}

/* the module namespace this module's compiled code resolves globals through
 *
 * the bump is what keeps a second `by_exec` — a module re-executed, or executed in
 * another interpreter — from being answered out of the first one's namespace */
static void By_WatchModule(PyObject *dict) {
    By_FindGlobals();
    if (!By_WatchGlobals(dict)) {
        by_globals = &by_globals_off;
        return;
    }
    /* asked again rather than inferred from the line above: the off struct's generation
     * is one increment away from the zero a cold site carries, so a stamp that ever
     * reached it would make every cold site in the module match at once. `By_WatchGlobals`
     * already answers no for it, and this is that answer written where the write is */
    if (BY_GLOBALS_FOUND) by_globals->generation += 1;
}

/* resolve the name in full and record what answered, or record nothing
 *
 * the counter is read *before* the lookups rather than after, so that whatever a site
 * ends up holding is stamped with the generation the answer was found under and never
 * with a later one. nothing between the two points can currently move the counter —
 * every namespace here is an exact dict, and reading one runs no python — but reading
 * after would be the kind of correct-by-accident that a new step in the middle
 * silently turns into a memo of a value that has already been replaced */
static PyObject *By_ArmGlobalSite(ByGlobalSite *site, PyObject *dict, PyObject *name) {
    uint64_t generation;
    PyObject *value;
    PyObject *answered;
    site->generation = 0u;
    if (!BY_GLOBALS_FOUND || dict == NULL || name == NULL) {
        return By_LookupGlobal(dict, name);
    }
    generation = by_globals->generation;
    value = By_ProbeGlobal(dict, name, &answered);
    if (value == NULL) {
        if (PyErr_Occurred()) return NULL;
        /* a name bound nowhere is pointedly *not* remembered: a refusal has no dict
         * entry behind it, so nothing would invalidate a memo of one when the name
         * is finally defined */
        PyErr_Format(PyExc_NameError, "name '%U' is not defined", name);
        return NULL;
    }
    /* whichever namespace answered has to be one this build hears about being written
     * to, or the answer is handed back without being kept */
    if (By_WatchGlobals(answered)) {
        site->value = value;
        site->generation = generation;
    }
    return Py_NewRef(value);
}

#else

/* nothing is remembered on this build, so there is nothing to invalidate */
static void By_WatchModule(PyObject *dict) { (void)dict; }

#endif /* BY_GLOBAL_SITES */

/* `By_LookupGlobal` through a per-site memo of its answer
 *
 * on a free-threaded build there is no memo at all: the three fields cannot be read
 * or written as one, and an emitted module says `Py_MOD_GIL_NOT_USED`, so two
 * threads could otherwise leave one dict's generation paired with another's answer */
static inline PyObject *By_LookupGlobalSite(ByGlobalSite *site, PyObject *dict,
                                            PyObject *name) {
#ifdef BY_GLOBAL_SITES
    if (BY_LIKELY(site->generation == by_globals->generation)) {
        return Py_NewRef(site->value);
    }
    return By_ArmGlobalSite(site, dict, name);
#else
    (void)site;
    return By_LookupGlobal(dict, name);
#endif
}

/* ── a global read whose only use is to unbox it ──────────────────────────────
 *
 * `while i < _limit` reads a name, is handed the object the namespace holds under it,
 * narrows that object to an `int`, and lets the object go again — every trip. the memo
 * above already spares the lookup, but not the reference it takes for an answer the
 * caller wants only in order to take it apart.
 *
 * so the *unboxed* value is what is remembered, beside the same generation. what stays
 * is the compare; what goes is a retain, a release and the narrowing.
 *
 * remembering the narrowed value is exactly as safe as remembering the object, because
 * for these representations it either is not a reference at all or is the same borrow:
 *
 * - a `double` and a `char` are copied out of the object and have no lifetime of their
 *   own — a later write to the object cannot exist, since `float` and `bool` are
 *   immutable and a rebinding is a write to the namespace, which is what moves the
 *   counter
 * - a tagged `int` is either a short, which is a copy in the same sense, or a pointer
 *   to the object the dict itself holds — borrowed on precisely the terms
 *   [`ByGlobalSite::value`] is borrowed on, and let go of by the very same event
 *
 * what is remembered is therefore only ever read back under the generation the answer
 * was found under, and a site is stamped only where the boxed site was: that is what
 * says the namespace that answered is one this build hears about being written to */
typedef struct {
    uint64_t generation;
    union {
        ByTagged tagged;
        double number;
        char bit;
    } value;
} ByGlobalUnboxedSite;

#define BY_GLOBAL_UNBOXED_SITE_INIT { 0u, { BY_INT_EMPTY } }

/* whether the read that has just been made may be remembered in unboxed form, and under
 * which generation — the boxed site's answer to both, since it is the one that decides
 * what may be kept at all */
static inline uint64_t By_UnboxedGlobalStamp(const ByGlobalSite *boxed) {
#ifdef BY_GLOBAL_SITES
    return boxed->generation;
#else
    (void)boxed;
    return 0u;
#endif
}

/* each of the three is the same shape: read the name in full, narrow it, stamp the memo
 * where the boxed site was stamped, and hand the narrowed value back. they are apart
 * only because each answers a different C type */

static ByTagged By_ArmGlobalTaggedSite(ByGlobalUnboxedSite *site, ByGlobalSite *boxed,
                                       PyObject *dict, PyObject *name) {
    PyObject *value;
    ByTagged unboxed;
    site->generation = 0u;
    value = By_LookupGlobalSite(boxed, dict, name);
    if (value == NULL) return BY_INT_ERROR;
    unboxed = By_UnboxInt(value);
    if (unboxed == BY_INT_ERROR) {
        Py_DECREF(value);
        return BY_INT_ERROR;
    }
    /* the narrowing answers an owned tagged value, and that one reference is the
     * caller's: what the site keeps beside it is a *borrow*, on the same terms as
     * [`ByGlobalSite::value`]. taking a second here would be a reference the site never
     * gives back, since a re-arm writes over what it holds rather than releasing it —
     * which showed up as a heap `int` gaining two references per rebinding */
    site->value.tagged = unboxed;
    site->generation = By_UnboxedGlobalStamp(boxed);
    Py_DECREF(value);
    return unboxed;
}

static inline ByTagged By_LookupGlobalTaggedSite(ByGlobalUnboxedSite *site,
                                                 ByGlobalSite *boxed, PyObject *dict,
                                                 PyObject *name) {
#ifdef BY_GLOBAL_SITES
    if (BY_LIKELY(site->generation == by_globals->generation)) {
        ByTagged held = site->value.tagged;
        By_IncRefTagged(held);
        return held;
    }
#endif
    return By_ArmGlobalTaggedSite(site, boxed, dict, name);
}

/* a double has no value to spare for an error, so failure is reported the way every
 * other float narrowing reports it: the answer is the sentinel and the caller confirms
 * it against the thread's exception */
static double By_ArmGlobalFloatSite(ByGlobalUnboxedSite *site, ByGlobalSite *boxed,
                                    PyObject *dict, PyObject *name) {
    PyObject *value;
    double unboxed;
    site->generation = 0u;
    value = By_LookupGlobalSite(boxed, dict, name);
    if (value == NULL) return BY_FLOAT_ERROR;
    unboxed = By_UnboxFloat(value);
    Py_DECREF(value);
    if (unboxed == BY_FLOAT_ERROR && PyErr_Occurred()) return BY_FLOAT_ERROR;
    site->value.number = unboxed;
    site->generation = By_UnboxedGlobalStamp(boxed);
    return unboxed;
}

static inline double By_LookupGlobalFloatSite(ByGlobalUnboxedSite *site,
                                              ByGlobalSite *boxed, PyObject *dict,
                                              PyObject *name) {
#ifdef BY_GLOBAL_SITES
    if (BY_LIKELY(site->generation == by_globals->generation)) {
        return site->value.number;
    }
#endif
    return By_ArmGlobalFloatSite(site, boxed, dict, name);
}

/* `bool` and `None` share this: both narrow to a byte, and both report failure as 2 */
static char By_ArmGlobalBitSite(ByGlobalUnboxedSite *site, ByGlobalSite *boxed,
                                PyObject *dict, PyObject *name, int is_none) {
    PyObject *value;
    char unboxed;
    site->generation = 0u;
    value = By_LookupGlobalSite(boxed, dict, name);
    if (value == NULL) return 2;
    unboxed = is_none ? By_UnboxNone(value) : By_UnboxBool(value);
    Py_DECREF(value);
    if (unboxed == 2) return 2;
    site->value.bit = unboxed;
    site->generation = By_UnboxedGlobalStamp(boxed);
    return unboxed;
}

static inline char By_LookupGlobalBitSite(ByGlobalUnboxedSite *site, ByGlobalSite *boxed,
                                          PyObject *dict, PyObject *name, int is_none) {
#ifdef BY_GLOBAL_SITES
    if (BY_LIKELY(site->generation == by_globals->generation)) {
        return site->value.bit;
    }
#endif
    return By_ArmGlobalBitSite(site, boxed, dict, name, is_none);
}

/* ── a module function a compiled caller reaches directly ─────────────────────
 *
 * a compiled call to a function the module defines goes straight to its native entry,
 * with the defaults it was compiled with filled in. python instead calls whatever the
 * function object under that name is when the call is made, running the code it holds
 * and binding a missing argument from the defaults it holds, so the direct call is only
 * the same program while the published function is still the one the module installed
 * and still holds what it was installed with.
 *
 * `ByFunctionSite` is that question, asked once per function rather than per call. two
 * watchers keep its answer. a dict watcher is told of every write to the namespace, and
 * one to the function's name takes the answer back until a call finds the published
 * function under the name again. a function watcher is told when the function object's
 * code, defaults or keyword defaults are reassigned, and from then on `moved` is set for
 * good: the compiled callers call the function object, and the function's own python
 * entry hands the call to the interpreted definition, given the defaults the published
 * function holds now. an edit made *inside* the `__kwdefaults__` dict is not a
 * reassignment and nothing reports it, so that one is not seen.
 *
 * python before 3.12 has no watchers to register, and a free-threaded build registers
 * none, so there the question is asked at each call instead: whether the name still holds
 * the published function, and whether that function still holds the code, defaults and
 * keyword defaults the site was armed with. the one answer that differs is for a function
 * given back the very tuple or dict it was armed with after a reassignment: the watchers
 * keep it moved, where this finds it standing again, so an edit made inside that
 * `__kwdefaults__` dict in the meantime is not seen */

/* what a watcher registered by any compiled module writes. its layout is fixed, because
 * the watcher is registered once per interpreter by whichever module gets there first,
 * and that module may have been built by another version of this header */
typedef struct {
    /* a compiled caller may go straight to the native entry */
    char stands;
    /* the published function's code or defaults were reassigned */
    char moved;
} ByFunctionFlags;

/* one name a dict watcher answers for: the namespace it is a name in, and the flags a
 * write to it takes back. the namespace is borrowed, and a compiled module's lives as long
 * as the process does. fixed for the same reason the flags are */
typedef struct {
    PyObject *dict;
    ByFunctionFlags *flags;
} ByFunctionName;

#if !defined(Py_GIL_DISABLED) && PY_VERSION_HEX >= 0x030C0000
#define BY_FUNCTION_SITES 1
#endif

typedef struct {
    ByFunctionFlags flags;
    /* the function this module published under the name, and the interpreted
     * definition it forwards for. both strong, and both for the life of the process:
     * the module stands behind one module object and no more */
    PyObject *forwarder;
    PyObject *twin;
    /* the name, interned when the site is armed */
    PyObject *name;
    /* what the dict watcher holds for this site */
    ByFunctionName entry;
    /* both watchers answer for this site, so a call that finds the published function
     * under its name again may go straight to the native entry again */
    char watched;
#ifndef BY_FUNCTION_SITES
    /* what the published function held when the site was armed, which a build with no
     * watchers compares against at each call. held strongly, so a replacement can never
     * be allocated where one of these was and compare equal to it */
    PyObject *code;
    PyObject *defaults;
    PyObject *kwdefaults;
#endif
} ByFunctionSite;

#define BY_FUNCTION_SITE_INIT { { 0, 0 }, NULL, NULL, NULL, { NULL, NULL }, 0 }

#ifdef BY_FUNCTION_SITES

/* the interpreter's one function watcher and one namespace watcher for every compiled
 * module, and what they answer for.
 *
 * `published` is a dict from the function to a capsule holding its flags. the key is the
 * function object itself, which the dict keeps alive, so a function that dies can never
 * be mistaken for one born at its address. `names` is a dict from a name to a list of
 * capsules, one per namespace publishing a function under that name */
typedef struct {
    int watcher;
    int dict_watcher;
    PyObject *published;
    PyObject *names;
    /* one bit per name `names` holds, at its hash modulo 64. a write under a name whose
     * bit is clear cannot be one of them, which is nearly every write, and is passed over
     * without looking anything up */
    uint64_t hashes;
} ByFunctionWatch;

#define BY_FUNCTION_WATCH_KEY "_by_function_watch_v1"
#define BY_FUNCTION_FLAGS_CAPSULE "by.function_flags"
#define BY_FUNCTION_NAME_CAPSULE "by.function_name"

static ByFunctionWatch *by_function_watch = NULL;
/* the registration this module's callback reads — see `by_globals_watched` for why it is
 * not the same variable as the one this module's sites read */
static ByFunctionWatch *by_function_watch_registered = NULL;

/* a function python tells every watcher about. creation and destruction happen to every
 * function in the process and say nothing about a published one, so they are passed over
 * before anything is looked up */
static int By_FunctionChanged(PyFunction_WatchEvent event, PyFunctionObject *func,
                              PyObject *new_value) {
    (void)new_value;
    if (event != PyFunction_EVENT_MODIFY_CODE && event != PyFunction_EVENT_MODIFY_DEFAULTS
        && event != PyFunction_EVENT_MODIFY_KWDEFAULTS) {
        return 0;
    }
    ByFunctionWatch *watch = by_function_watch_registered;
    if (watch == NULL) return 0;
    /* a function hashes by identity, so looking it up runs no python and cannot fail
     * for a reason of its own; an exception already standing is left as it was */
    PyObject *type, *value, *traceback;
    PyErr_Fetch(&type, &value, &traceback);
    PyObject *found = PyDict_GetItemWithError(watch->published, (PyObject *)func);
    if (found != NULL) {
        ByFunctionFlags *flags = (ByFunctionFlags *)PyCapsule_GetPointer(found, BY_FUNCTION_FLAGS_CAPSULE);
        if (flags != NULL) {
            flags->moved = 1;
            flags->stands = 0;
        }
    }
    PyErr_Clear();
    PyErr_Restore(type, value, traceback);
    return 0;
}

/* take back the answer of every site `names` holds for a function published in `dict`
 * under `key`, or under any name at all where `key` is NULL */
static void By_TakeBackNames(PyObject *names, PyObject *dict, PyObject *key) {
    PyObject *lists = NULL;
    Py_ssize_t cursor = 0;
    if (key != NULL) {
        PyObject *list = PyDict_GetItemWithError(names, key);
        if (list == NULL || !PyList_Check(list)) return;
        for (Py_ssize_t at = 0; at < PyList_GET_SIZE(list); at++) {
            ByFunctionName *name = (ByFunctionName *)PyCapsule_GetPointer(
                PyList_GET_ITEM(list, at), BY_FUNCTION_NAME_CAPSULE);
            if (name != NULL && name->dict == dict) name->flags->stands = 0;
        }
        return;
    }
    while (PyDict_Next(names, &cursor, NULL, &lists)) {
        if (!PyList_Check(lists)) continue;
        for (Py_ssize_t at = 0; at < PyList_GET_SIZE(lists); at++) {
            ByFunctionName *name = (ByFunctionName *)PyCapsule_GetPointer(
                PyList_GET_ITEM(lists, at), BY_FUNCTION_NAME_CAPSULE);
            if (name != NULL && name->dict == dict) name->flags->stands = 0;
        }
    }
}

/* a write to a namespace some compiled module publishes functions in
 *
 * every write to a watched namespace comes through here, so the common one — a name no
 * function is published under — is one lookup of an exact `str` that misses. a key that
 * is not an exact `str` can still stand for a name, through a hash and an equality of its
 * own, so a write under one takes back every answer the namespace gave. a write that
 * empties the namespace, or throws it away, does the same. the lookups here run no python
 * and cannot fail, so an exception already standing is left alone */
static int By_FunctionNameWritten(PyDict_WatchEvent event, PyObject *dict, PyObject *key,
                                  PyObject *new_value) {
    (void)event;
    (void)new_value;
    ByFunctionWatch *watch = by_function_watch_registered;
    if (watch == NULL) return 0;
    if (key != NULL && PyUnicode_CheckExact(key)) {
        /* a key already in a dict has its hash cached, and an exact `str` has no hash of
         * its own to run for one that does not, so this cannot fail */
        Py_hash_t hash = ((PyASCIIObject *)key)->hash;
        if (hash == -1) hash = PyObject_Hash(key);
        if (!(watch->hashes & ((uint64_t)1 << ((uint64_t)hash & 63u)))) return 0;
        By_TakeBackNames(watch->names, dict, key);
        return 0;
    }
    By_TakeBackNames(watch->names, dict, NULL);
    return 0;
}

/* find this interpreter's watchers, registering them the first time. like the dict
 * watcher's counter they are never freed: a site in any module may still be armed by them */
static void By_FindFunctionWatch(void) {
    PyInterpreterState *interpreter;
    PyObject *state;
    PyObject *capsule;
    ByFunctionWatch *shared;
    if (by_function_watch != NULL) return;
    interpreter = PyInterpreterState_Get();
    if (interpreter == NULL) return;
    state = PyInterpreterState_GetDict(interpreter);
    if (state == NULL) return;
    capsule = PyDict_GetItemString(state, BY_FUNCTION_WATCH_KEY);
    if (capsule != NULL) {
        by_function_watch = (ByFunctionWatch *)PyCapsule_GetPointer(capsule, BY_FUNCTION_WATCH_KEY);
        if (by_function_watch == NULL) PyErr_Clear();
        return;
    }
    PyErr_Clear();
    shared = (ByFunctionWatch *)PyMem_RawMalloc(sizeof(ByFunctionWatch));
    if (shared == NULL) return;
    shared->hashes = 0u;
    shared->published = PyDict_New();
    shared->names = PyDict_New();
    if (shared->published == NULL || shared->names == NULL) {
        PyErr_Clear();
        Py_XDECREF(shared->published);
        Py_XDECREF(shared->names);
        PyMem_RawFree(shared);
        return;
    }
    shared->watcher = PyFunction_AddWatcher(By_FunctionChanged);
    if (shared->watcher < 0) {
        PyErr_Clear();
        Py_DECREF(shared->published);
        Py_DECREF(shared->names);
        PyMem_RawFree(shared);
        return;
    }
    shared->dict_watcher = PyDict_AddWatcher(By_FunctionNameWritten);
    if (shared->dict_watcher < 0) {
        PyErr_Clear();
        (void)PyFunction_ClearWatcher(shared->watcher);
        PyErr_Clear();
        Py_DECREF(shared->published);
        Py_DECREF(shared->names);
        PyMem_RawFree(shared);
        return;
    }
    by_function_watch_registered = shared;
    capsule = PyCapsule_New(shared, BY_FUNCTION_WATCH_KEY, NULL);
    if (capsule == NULL) {
        PyErr_Clear();
        by_function_watch = shared;
        return;
    }
    if (PyDict_SetItemString(state, BY_FUNCTION_WATCH_KEY, capsule) < 0) PyErr_Clear();
    Py_DECREF(capsule);
    by_function_watch = shared;
}

#endif /* BY_FUNCTION_SITES */

/* take the function this module has just published under `name` in hand, and start
 * being told about it
 *
 * a site that cannot be watched is left not standing, which sends every compiled call
 * through the function object: slower, and the same program. a build with no watchers
 * at all — before 3.12, or free-threaded — takes note of what the function holds
 * instead, and asks again at each call. `called` says whether any compiled call asks the
 * site: one nobody asks needs no word of writes to its name, only of what happens to the
 * function its own python entry stands for */
static int By_ArmFunctionSite(ByFunctionSite *site, PyObject *dict, const char *name, int called) {
    if (site->name == NULL) site->name = By_InternedStr(name, (Py_ssize_t)strlen(name));
    if (site->name == NULL) return -1;
    PyObject *forwarder = PyDict_GetItemWithError(dict, site->name);
    if (forwarder == NULL) {
        if (!PyErr_Occurred()) {
            PyErr_Format(PyExc_ImportError, "this module published nothing under '%s'", name);
        }
        return -1;
    }
    site->forwarder = Py_NewRef(forwarder);
    if (PyFunction_Check(forwarder)) {
        PyObject *twin = PyObject_GetAttrString(forwarder, "__wrapped__");
        if (twin == NULL) {
            PyErr_Clear();
        } else if (PyFunction_Check(twin)) {
            site->twin = twin;
        } else {
            Py_DECREF(twin);
        }
    }
#ifdef BY_FUNCTION_SITES
    By_FindFunctionWatch();
    if (by_function_watch != NULL && site->twin != NULL) {
        ByFunctionWatch *watch = by_function_watch;
        PyObject *flags = PyCapsule_New(&site->flags, BY_FUNCTION_FLAGS_CAPSULE, NULL);
        if (flags == NULL) return -1;
        int failed = PyDict_SetItem(watch->published, forwarder, flags) < 0;
        Py_DECREF(flags);
        if (failed) return -1;
        if (!called) return 0;
        if (PyDict_Watch(watch->dict_watcher, dict) < 0) {
            /* a namespace that will not be watched is one whose writes no site hears
             * about, so nothing is answered for it */
            PyErr_Clear();
            return 0;
        }
        site->entry.dict = dict;
        site->entry.flags = &site->flags;
        PyObject *entry = PyCapsule_New(&site->entry, BY_FUNCTION_NAME_CAPSULE, NULL);
        if (entry == NULL) return -1;
        PyObject *list = PyDict_GetItemWithError(watch->names, site->name);
        if (list == NULL) {
            if (PyErr_Occurred()) {
                Py_DECREF(entry);
                return -1;
            }
            list = PyList_New(0);
            if (list == NULL || PyDict_SetItem(watch->names, site->name, list) < 0) {
                Py_XDECREF(list);
                Py_DECREF(entry);
                return -1;
            }
            Py_DECREF(list);
        }
        failed = PyList_Append(list, entry) < 0;
        Py_DECREF(entry);
        if (failed) return -1;
        watch->hashes |= (uint64_t)1 << ((uint64_t)PyObject_Hash(site->name) & 63u);
        site->watched = 1;
        site->flags.stands = 1;
    }
#else
    (void)called;
    if (site->twin != NULL) {
        site->code = Py_NewRef(PyFunction_GET_CODE(forwarder));
        site->defaults = Py_XNewRef(PyFunction_GET_DEFAULTS(forwarder));
        site->kwdefaults = Py_XNewRef(PyFunction_GET_KW_DEFAULTS(forwarder));
    }
#endif
    return 0;
}

#ifndef BY_FUNCTION_SITES
/* whether the published function holds what it held when the site was armed. a site
 * that is not armed, or published something other than a function, has nothing to hold */
static inline char By_FunctionUnmoved(ByFunctionSite *site) {
    PyObject *forwarder = site->forwarder;
    return site->twin != NULL && PyFunction_GET_CODE(forwarder) == site->code
           && PyFunction_GET_DEFAULTS(forwarder) == site->defaults
           && PyFunction_GET_KW_DEFAULTS(forwarder) == site->kwdefaults;
}
#endif

/* what `By_ResolveFunction` answers where the read failed. an address nothing can be, so
 * NULL is left free to say the native entry stands */
static const char by_resolve_failed = 0;
#define BY_RESOLVE_FAILED ((PyObject *)(void *)&by_resolve_failed)

/* the object under a published function's name, read the way a global read is */
BY_COLD PyObject *By_ResolveFunctionSlow(ByFunctionSite *site, PyObject *dict, const char *name) {
    PyObject *found;
    /* a call made while the module is still being imported reaches a site that is not
     * armed yet, and the name is whatever the body has left there so far */
    if (site->name == NULL) {
        site->name = By_InternedStr(name, (Py_ssize_t)strlen(name));
        if (site->name == NULL) return BY_RESOLVE_FAILED;
    }
    /* the name holds the function this module published, and nothing since has moved it:
     * both watchers are listening again from here, so the direct call stands again */
    if (site->watched && !site->flags.moved) {
        PyObject *held = PyDict_GetItemWithError(dict, site->name);
        if (held == site->forwarder) {
            site->flags.stands = 1;
            return NULL;
        }
        if (held == NULL && PyErr_Occurred()) return BY_RESOLVE_FAILED;
    }
    found = By_LookupGlobal(dict, site->name);
    return found != NULL ? found : BY_RESOLVE_FAILED;
}

/* NULL where the native entry may stand in for the function object, the object under the
 * name as a new reference where it may not, or `BY_RESOLVE_FAILED` with the error set */
static inline PyObject *By_ResolveFunction(ByFunctionSite *site, PyObject *dict, const char *name) {
#ifdef BY_FUNCTION_SITES
    if (BY_LIKELY(site->flags.stands)) return NULL;
#else
    /* only compared, so a name another thread rebinds meanwhile cannot hand back a
     * reference that is gone */
    if (BY_LIKELY(By_FunctionUnmoved(site))) {
        PyObject *held = PyDict_GetItemWithError(dict, site->name);
        if (BY_LIKELY(held == site->forwarder)) return NULL;
        if (held == NULL && PyErr_Occurred()) return BY_RESOLVE_FAILED;
    }
#endif
    return By_ResolveFunctionSlow(site, dict, name);
}

/* whether a compiled call may go straight to the native entry, asked just after the name
 * was resolved: nothing can fail, and a call that may not goes through the function
 * object, which raises what python raises
 *
 * with no watchers the resolution has read the name itself, and nothing has run since,
 * so what is left to ask is whether the function it found has moved */
static inline char By_FunctionStands(ByFunctionSite *site) {
#ifdef BY_FUNCTION_SITES
    return site->flags.stands;
#else
    return By_FunctionUnmoved(site);
#endif
}

/* whether a module function stands, asked on the way into a loop that asks nothing again
 * until it reaches code that can run python
 *
 * as for a builtin, only python code can rebind the name or move the function, so the
 * answer stands only while no other thread can run either, and a build without the GIL
 * answers no. a failed read answers no too, and leaves no error behind: the loop as
 * written reads the name again where python would, and raises what python raises */
static inline char By_FunctionStandsOnEntry(ByFunctionSite *site, PyObject *dict) {
#if defined(Py_GIL_DISABLED)
    (void)site;
    (void)dict;
    return 0;
#elif defined(BY_FUNCTION_SITES)
    (void)dict;
    return site->flags.stands;
#else
    PyObject *held;
    if (!By_FunctionUnmoved(site)) return 0;
    held = PyDict_GetItemWithError(dict, site->name);
    if (held == NULL) {
        PyErr_Clear();
        return 0;
    }
    return held == site->forwarder;
#endif
}

/* whether the published function's code or defaults have moved since the module installed
 * it, which sends a call its own python entry makes to the interpreted definition */
static inline char By_FunctionMoved(ByFunctionSite *site) {
#ifdef BY_FUNCTION_SITES
    return site->flags.moved;
#else
    return site->twin != NULL && !By_FunctionUnmoved(site);
#endif
}

/* the object a call the native entry was turned away from goes through, as a new
 * reference: what the name was read as, or where the read found the published function
 * still standing, that function — whose code or defaults the arguments may since have
 * moved, which python would then run */
static inline PyObject *By_FunctionCallee(PyObject *resolved, PyObject *published) {
    PyObject *callee = resolved != NULL ? resolved : published;
    if (callee == NULL) {
        PyErr_SetString(PyExc_SystemError, "a compiled call has no function to go through");
        return NULL;
    }
    return Py_NewRef(callee);
}

/* a tuple of interned keyword names, built once per call site that passes them */
BY_COLD PyObject *By_KeywordNames(Py_ssize_t count, ...) {
    PyObject *names = PyTuple_New(count);
    va_list given;
    if (names == NULL) return NULL;
    va_start(given, count);
    for (Py_ssize_t at = 0; at < count; at++) {
        const char *name = va_arg(given, const char *);
        PyObject *interned = By_InternedStr(name, (Py_ssize_t)strlen(name));
        if (interned == NULL) {
            va_end(given);
            Py_DECREF(names);
            return NULL;
        }
        PyTuple_SET_ITEM(names, at, interned);
    }
    va_end(given);
    return names;
}

/* `callee(*positional, **keywords)` with the arguments handed over one at a time: the
 * `nargs` positional ones, then one per name in `kwnames` */
BY_COLD PyObject *By_CallThrough(PyObject *callee, PyObject *kwnames, Py_ssize_t nargs, ...) {
    PyObject *small[8];
    PyObject **argv = small;
    Py_ssize_t total = nargs + (kwnames != NULL ? PyTuple_GET_SIZE(kwnames) : 0);
    va_list given;
    PyObject *result;
    if (total > (Py_ssize_t)(sizeof(small) / sizeof(small[0]))) {
        argv = (PyObject **)PyMem_Malloc((size_t)total * sizeof(PyObject *));
        if (argv == NULL) return PyErr_NoMemory();
    }
    va_start(given, nargs);
    for (Py_ssize_t at = 0; at < total; at++) argv[at] = va_arg(given, PyObject *);
    va_end(given);
    result = PyObject_Vectorcall(callee, argv, (size_t)nargs, kwnames);
    if (argv != small) PyMem_Free(argv);
    return result;
}

/* the published function's own python entry, once its code or defaults have moved
 *
 * the compiled wrapper has the defaults it was compiled with written into it, so the call
 * goes to the interpreted definition instead, carrying the defaults and keyword defaults
 * the published function holds now — which is exactly what binding against the published
 * function would have used */
BY_COLD PyObject *By_CallMoved(ByFunctionSite *site, PyObject *const *args, Py_ssize_t nargs,
                               PyObject *kwnames) {
    PyObject *twin = site->twin;
    PyObject *forwarder = site->forwarder;
    if (twin == NULL || forwarder == NULL || !PyFunction_Check(forwarder)) {
        PyErr_SetString(PyExc_SystemError,
                        "a compiled function whose defaults moved has no interpreted definition");
        return NULL;
    }
    PyObject *defaults = PyFunction_GetDefaults(forwarder);
    if (defaults != PyFunction_GetDefaults(twin)
        && PyFunction_SetDefaults(twin, defaults ? defaults : Py_None) < 0) {
        return NULL;
    }
    PyObject *keywords = PyFunction_GetKwDefaults(forwarder);
    if (keywords != PyFunction_GetKwDefaults(twin)
        && PyFunction_SetKwDefaults(twin, keywords ? keywords : Py_None) < 0) {
        return NULL;
    }
    return PyObject_Vectorcall(twin, args, (size_t)nargs, kwnames);
}

/* whether a type spec can be built on this tuple of bases
 *
 * `PyType_FromSpecWithBases` gives the type it builds `type` as its own, so any base
 * with another metaclass is a conflict. python's own answer to it moved: 3.13 builds the
 * type anyway and warns that it will stop, and 3.14 raises `TypeError: Metaclasses with
 * custom tp_new are not supported`. so the two supported versions disagree about a base
 * whose metaclass is `abc.ABCMeta` — which is the shape nearly every declining class in
 * the standard library has — and this refuses it on both rather than emitting a module
 * that imports on one and not the other.
 *
 * it also wants a base to pick a layout from, which an empty tuple does not offer —
 * `type` supplies `object` for that case and a spec does not */
static inline int By_SpecTakesBases(PyObject *bases) {
    Py_ssize_t index;
    if (PyTuple_GET_SIZE(bases) == 0) return 0;
    for (index = 0; index < PyTuple_GET_SIZE(bases); index++) {
        if (Py_TYPE(PyTuple_GET_ITEM(bases, index)) != &PyType_Type) return 0;
    }
    return 1;
}

/* whether a spec-built type's `__dict__` and weakrefs are where the type says they are
 *
 * a spec adds neither: it takes its whole instance shape from the one base python picks
 * the layout out of, so both offsets have to be the ones that base already had. but they
 * are inherited from whichever base *declares* one, which need not be that base at all —
 * so a class over a base keeping a managed `__dict__`, beside a base owning the layout,
 * is handed the offset of a dict there is no room for. python reads it against the
 * instance it does have and lands inside it: `subtype_dealloc` releases whatever it
 * finds there, which is how 24 of the `encodings` modules segfaulted at the first
 * deallocation.
 *
 * a class statement works the shape out from every base at once, so where the offsets
 * disagree with the layout base the interpreted definition is what answers.
 *
 * a spec that *asked* for a dict of its own is the one exception, and the spec is passed
 * in so that asking can be told from inheriting: the class declared a word for it in its
 * own struct, so the room is there and the offset naming that word is the answer that was
 * wanted. without this a decorated class silently kept its interpreted definition while
 * every compiled function went on reading that definition's instances as its own struct */
static inline Py_ssize_t By_SpecMemberOffset(PyType_Spec *spec, const char *name) {
    PyType_Slot *slot;
    if (spec == NULL || spec->slots == NULL) return 0;
    for (slot = spec->slots; slot->slot != 0; slot++) {
        PyMemberDef *member;
        if (slot->slot != Py_tp_members || slot->pfunc == NULL) continue;
        for (member = (PyMemberDef *)slot->pfunc; member->name != NULL; member++) {
            if (strcmp(member->name, name) == 0) return member->offset;
        }
    }
    return 0;
}

/* the same exception holds for the weak-reference list, which a spec asks for on the same
 * terms as the dict */
static inline int By_OffsetHoldsUp(Py_ssize_t built, Py_ssize_t inherited, PyType_Spec *spec,
                                   const char *name) {
    Py_ssize_t asked;
    if (built == inherited) return 1;
    asked = By_SpecMemberOffset(spec, name);
    return asked != 0 && built == asked;
}

static inline int By_OffsetsHoldUp(PyTypeObject *type, PyType_Spec *spec) {
    PyTypeObject *base = type->tp_base;
    if (base == NULL) {
        return 1;
    }
    return By_OffsetHoldsUp(type->tp_weaklistoffset, base->tp_weaklistoffset, spec,
                            "__weaklistoffset__")
           && By_OffsetHoldsUp(type->tp_dictoffset, base->tp_dictoffset, spec,
                               "__dictoffset__");
}

/* the type for a class whose fields sit past a base's instance, or nothing at all
 *
 * such a class has exactly one construction. the storage is appended by the spec and
 * `PyObject_GetTypeData` is the only way to reach it, so every compiled read and write
 * of a field is an offset into an instance only *this* type allocates. no other type can
 * stand under the name — the interpreted definition least of all, whose instances stop
 * where the base's do, so a field write lands past the end of the object. answering with
 * nothing is what leaves module init able to refuse: it has installed nothing yet, and
 * the whole module stays as the interpreted definition already built it.
 *
 * the bases are the ones that definition settled on rather than the names read a second
 * time. python has resolved `__mro_entries__` and picked the base the layout comes from
 * by the time this runs, so the three questions are all asked of the very tuple the type
 * is then built on:
 *
 * - the base has to be one python allocates and frees itself. this class supplies
 *   `tp_dealloc`, `tp_traverse` and `tp_clear`, because the base cannot see storage
 *   appended after its own data, and each of the three calls the base's. python's own
 *   three — the ones a `class` statement's type carries — resolve which base to chain to
 *   from `Py_TYPE(self)` rather than from the type that declared them, and there they
 *   find *this* class's function and call it straight back, until the stack runs out. a
 *   heap base that writes its own three instead is no better: it drops the instance's
 *   reference to its type, which this class's deallocator drops again
 * - the bases have to be ones a spec can build on at all — see `By_SpecTakesBases`
 * - and the type it builds has to keep its `__dict__` and its weakrefs where it says
 *   they are — see `By_OffsetsHoldUp`
 *
 * the last of those is only knowable from the finished type, which is why this builds
 * one rather than predicting it. a base a spec cannot extend at all — a variable-size
 * one without `Py_TPFLAGS_ITEMS_AT_END` — refuses in the same place, as the failure
 * `PyType_FromSpecWithBases` reports */
static inline PyObject *By_SpecClass(PyObject *module_dict, const char *name,
                                     PyType_Spec *spec) {
    PyObject *twin = By_LookupGlobalString(module_dict, name);
    PyObject *cls;
    PyTypeObject *base;
    if (twin == NULL) {
        PyErr_Clear();
        return NULL;
    }
    /* a name that is not a class answers no, which is the same refusal a missing one
     * gets */
    if (!PyType_Check(twin)) {
        Py_DECREF(twin);
        return NULL;
    }
    base = ((PyTypeObject *)twin)->tp_base;
    if (base == NULL || (base->tp_flags & Py_TPFLAGS_HEAPTYPE)
        || !By_SpecTakesBases(((PyTypeObject *)twin)->tp_bases)) {
        Py_DECREF(twin);
        return NULL;
    }
    cls = PyType_FromSpecWithBases(spec, ((PyTypeObject *)twin)->tp_bases);
    Py_DECREF(twin);
    if (cls == NULL) {
        PyErr_Clear();
        return NULL;
    }
    if (!By_OffsetsHoldUp((PyTypeObject *)cls, spec)) {
        Py_DECREF(cls);
        return NULL;
    }
    return cls;
}

/* the type for a class appending storage past one *this module also appends to*
 *
 * `By_SpecClass` refuses a heap base outright, and a class this module writes is always
 * one: the interpreted definition under that name is a `class` statement's type, whose
 * `tp_dealloc` is `subtype_dealloc`. that refusal is the right answer for a base python
 * built — `subtype_dealloc` picks the deallocator to chain to out of `Py_TYPE(self)`,
 * finds this class's own, and calls it back until the stack runs out.
 *
 * a base *this module builds from a spec* is the one heap base that is not like that.
 * its three slots are ones we emitted: each reads the base to chain to from the type
 * that declared it, so the chain walks down to the outside base and stops. so the whole
 * chain of appended storage can be built, innermost first, each spec standing on the
 * finished type of the one below rather than on the interpreted definition.
 *
 * `base` is that finished type. what is checked here is that the interpreted definition
 * agrees the two are related the way the emitted pair are: `base_name` is still the twin
 * this module's `base` was built from — nothing of this module's own is installed yet —
 * and the twin settled on exactly it, as its layout base and as its only base. anything
 * else and the emitted type would answer a shape the source never wrote */
static inline PyObject *By_SpecSubclass(PyObject *module_dict, const char *name,
                                        PyType_Spec *spec, const char *base_name,
                                        PyObject *base) {
    PyObject *twin;
    PyObject *twin_base;
    PyObject *bases;
    PyObject *cls;
    int agrees;
    /* the base is a class this module builds too, and it may have stood down: its own
     * spec refused, or it went with a family that did. a class chaining its storage onto
     * one that is not there has nowhere to put it, so it stands down as well */
    if (base == NULL) {
        return NULL;
    }
    twin = By_LookupGlobalString(module_dict, name);
    if (twin == NULL) {
        PyErr_Clear();
        return NULL;
    }
    twin_base = By_LookupGlobalString(module_dict, base_name);
    if (twin_base == NULL) {
        PyErr_Clear();
        Py_DECREF(twin);
        return NULL;
    }
    agrees = PyType_Check(twin) && PyType_Check(twin_base)
             && (PyObject *)((PyTypeObject *)twin)->tp_base == twin_base
             && PyTuple_GET_SIZE(((PyTypeObject *)twin)->tp_bases) == 1
             && PyTuple_GET_ITEM(((PyTypeObject *)twin)->tp_bases, 0) == twin_base
             && By_SpecTakesBases(((PyTypeObject *)twin)->tp_bases);
    Py_DECREF(twin_base);
    Py_DECREF(twin);
    if (!agrees) {
        return NULL;
    }
    bases = PyTuple_Pack(1, base);
    if (bases == NULL) {
        PyErr_Clear();
        return NULL;
    }
    cls = PyType_FromSpecWithBases(spec, bases);
    Py_DECREF(bases);
    if (cls == NULL) {
        PyErr_Clear();
        return NULL;
    }
    /* the same last question `By_SpecClass` asks, and for the same reason: a spec adds
     * neither a `__dict__` nor a weakref, so both offsets have to be the base's */
    if (!By_OffsetsHoldUp((PyTypeObject *)cls, spec)) {
        Py_DECREF(cls);
        return NULL;
    }
    return cls;
}

/* the bases a class statement actually builds on
 *
 * python resolves `__mro_entries__` before it works out a metaclass or calls one: a
 * base that is not a class may stand for a tuple of them. `typing.Generic[T]` is the
 * familiar one, and a name bound to a proxy — which is what a lazily imported base is
 * until it is first read — is another */
static inline PyObject *By_ResolveBases(PyObject *bases) {
    Py_ssize_t index, prior;
    PyObject *tuple;
    /* the tuple itself where nothing needed replacing, so that a caller can tell the
     * two apart by identity — which is how python decides whether to record
     * `__orig_bases__` */
    PyObject *resolved = NULL;
    for (index = 0; index < PyTuple_GET_SIZE(bases); index++) {
        PyObject *base = PyTuple_GET_ITEM(bases, index);
        PyObject *entries, *replacement;
        int failed;
        if (PyType_Check(base)) {
            if (resolved != NULL && PyList_Append(resolved, base) < 0) goto failed;
            continue;
        }
        entries = PyObject_GetAttrString(base, "__mro_entries__");
        if (entries == NULL) {
            PyErr_Clear();
            if (resolved != NULL && PyList_Append(resolved, base) < 0) goto failed;
            continue;
        }
        replacement = PyObject_CallOneArg(entries, bases);
        Py_DECREF(entries);
        if (replacement == NULL) goto failed;
        if (!PyTuple_Check(replacement)) {
            PyErr_SetString(PyExc_TypeError, "__mro_entries__ must return a tuple");
            Py_DECREF(replacement);
            goto failed;
        }
        if (resolved == NULL) {
            /* the first replacement: everything before it went by untouched */
            resolved = PyList_New(0);
            if (resolved == NULL) {
                Py_DECREF(replacement);
                return NULL;
            }
            for (prior = 0; prior < index; prior++) {
                if (PyList_Append(resolved, PyTuple_GET_ITEM(bases, prior)) < 0) {
                    Py_DECREF(replacement);
                    goto failed;
                }
            }
        }
        failed = PyList_SetSlice(resolved, PyList_GET_SIZE(resolved), PyList_GET_SIZE(resolved),
                                 replacement) < 0;
        Py_DECREF(replacement);
        if (failed) goto failed;
    }
    if (resolved == NULL) return By_NewRef(bases);
    tuple = PyList_AsTuple(resolved);
    Py_DECREF(resolved);
    return tuple;
failed:
    Py_XDECREF(resolved);
    return NULL;
}

/* ── building a class through its metaclass ───────────────────────────────────
 *
 * `meta(name, bases, namespace, **keywords)` is what a `class` statement does, and a
 * type spec cannot do: a spec takes no keywords and gives the type it builds `type`
 * for a metaclass, which any other one rejects. what it costs is the instance layout
 * — how big an instance is becomes the metaclass's answer rather than this module's —
 * so only a class that adds no fields of its own is built this way
 */

/* the metaclass to call, with `metaclass` taken out of the keywords
 *
 * python's rule, which is not the same as "the type of the first base": the explicit
 * keyword where there is one, otherwise the first base's type, and then the most
 * derived of that and every other base's type */
static inline PyObject *By_Metaclass(PyObject *bases, PyObject *kwds) {
    PyObject *winner = NULL;
    Py_ssize_t index;
    if (kwds != NULL) {
        winner = PyDict_GetItemString(kwds, "metaclass");
        if (winner != NULL) {
            Py_INCREF(winner);
            if (PyDict_DelItemString(kwds, "metaclass") < 0) {
                Py_DECREF(winner);
                return NULL;
            }
        }
    }
    if (winner == NULL) {
        winner = PyTuple_GET_SIZE(bases) > 0
                     ? (PyObject *)Py_TYPE(PyTuple_GET_ITEM(bases, 0))
                     : (PyObject *)&PyType_Type;
        Py_INCREF(winner);
    }
    /* a `metaclass` that is not a type is called as it stands — there is no subtype
     * relation to work a winner out of, and python does not look for one */
    if (!PyType_Check(winner)) return winner;
    for (index = 0; index < PyTuple_GET_SIZE(bases); index++) {
        PyTypeObject *candidate = Py_TYPE(PyTuple_GET_ITEM(bases, index));
        if (PyType_IsSubtype((PyTypeObject *)winner, candidate)) continue;
        if (PyType_IsSubtype(candidate, (PyTypeObject *)winner)) {
            Py_DECREF(winner);
            winner = (PyObject *)candidate;
            Py_INCREF(winner);
            continue;
        }
        Py_DECREF(winner);
        PyErr_SetString(PyExc_TypeError,
                        "metaclass conflict: the metaclass of a derived class must be a "
                        "(non-strict) subclass of the metaclasses of all its bases");
        return NULL;
    }
    return winner;
}

/* bind `key` in a class namespace
 *
 * a `__prepare__` may hand back any mapping, not a dict — so the general form is the
 * one that has to work, and the dict is only the case worth taking a shortcut on */
static inline int By_SetInNamespace(PyObject *ns, const char *key, PyObject *value) {
    PyObject *name;
    int failed;
    if (PyDict_CheckExact(ns)) return PyDict_SetItemString(ns, key, value);
    name = PyUnicode_FromString(key);
    if (name == NULL) return -1;
    failed = PyObject_SetItem(ns, name, value);
    Py_DECREF(name);
    return failed;
}

/* one attribute an emitted type keeps in its instance layout
 *
 * `optional` is the field python's own class only writes on some paths. `tp_alloc` zeroes
 * the presence byte beside such a field, so leaving one out is exactly the state an
 * `__init__` that skipped it produces — while a field the layout treats as always defined
 * has no presence byte and no check at any read, so leaving *that* one out reads back as
 * whatever zero means for its representation. an unwritten `PyObject *` raises, but an
 * unwritten tagged integer answers `0` and an unwritten `char` answers `False`, quietly.
 * that is the whole reason the names are published rather than guessed at from the getset
 * table, which cannot tell a field from a property */
typedef struct {
    const char *name;
    int optional;
} By_Field;

/* every interpreted definition this module replaced, and what stands where each did
 *
 * `twins[i]` is the definition a statement of the interpreted body left and `types[i]` is
 * what took its name. for a `class` statement that is the emitted type — NULL until the
 * type is built, which is what makes a constant naming a class further down the module a
 * refusal rather than a stale copy. for a `def` it is the *forwarder* the module publishes
 * under that name, and the pair is why an ordinary `handler = fn` in a class body answers
 * the same object the module's own name does: the interpreted definition and the forwarder
 * are two callables that agree on every call and disagree on `is`, so nothing but an
 * identity test could see the difference and everything keyed on identity took the wrong
 * branch.
 *
 * `layouts[i]` is the instance layout of `types[i]`, or NULL for a class whose instances
 * cannot be moved onto it — and NULL for every function pair, which has no instances; see
 * `By_MovedInstance`. `moved` is the mapping from a twin's *instance* to the instance that
 * replaced it, and it is what keeps two holders of one object agreeing.
 *
 * every walk that is about classes tests `PyType_Check` on both halves of a pair before
 * using it, so a function pair sitting in the same arrays is passed over there */
typedef struct {
    PyObject *const *twins;
    PyObject *const *types;
    const By_Field *const *layouts;
    Py_ssize_t count;
    PyObject *moved;
} By_Twins;

/* what a value carried onto an emitted type becomes, as a new reference — defined with the
 * rest of the twin machinery, and named here because a class namespace is written before
 * that */
static PyObject *By_TwinReplacement(PyObject *value, const By_Twins *twins);

/* the namespace entries a class takes off the body its interpreted definition ran, and
 * where their values come from
 *
 * the interpreted definition evaluated each of them once at class-definition time, and it
 * is the only place the same object can come from — so the body that definition wrote is
 * what they are read off, under the substitution every carried attribute takes. that body
 * is captured while the fallback source runs and before any of the class's own decorators
 * are handed it; `By_RunModuleBody` says why the finished class will not do. `twins` is the
 * module's map of replaced definitions; `body` is NULL for a class no interpreted `class`
 * statement wrote, and then there is nothing to carry */
typedef struct {
    PyObject *body;
    const char *const *names;
    Py_ssize_t count;
    /* how many of `names`, counted from the front, the body *has* to hold
     *
     * a class-level constant it did not write is simply not carried: the class ends up
     * without the name, which is what a body that never wrote it means. a decorated method
     * is not like that. the method table has already put an entry under the same name into
     * the namespace, and that entry is the method with nothing applied to it — so a carry
     * that does not happen leaves an *undecorated* method standing where the class wrote a
     * decorated one, which no `@abstractmethod` would survive. those names come first and
     * this is how many of them there are */
    Py_ssize_t required;
    const By_Twins *twins;
} By_ClassConstants;

/* whether the body holds every name that has to be taken off it — see `required` */
static inline int By_BodyCarriesRequired(const By_ClassConstants *constants) {
    Py_ssize_t at;
    if (constants == NULL || constants->required <= 0) return 1;
    if (constants->body == NULL) return 0;
    for (at = 0; at < constants->required; at++) {
        if (PyDict_GetItemString(constants->body, constants->names[at]) == NULL) return 0;
    }
    return 1;
}

/* the value one of them takes, as a new reference
 *
 * NULL is "the body did not write that name", which is not a failure and leaves no
 * exception set: a body under a conditional may not have written it */
static inline PyObject *By_ConstantValue(const By_ClassConstants *constants, Py_ssize_t at) {
    PyObject *value, *stands;
    if (constants == NULL || constants->body == NULL) return NULL;
    /* read out of the mapping rather than through a lookup on the class: a lookup runs
     * the descriptor protocol, so a `__class_getitem__ = classmethod(f)` would come back
     * as a method already bound to the interpreted class rather than as the classmethod
     * the body wrote */
    value = PyDict_GetItemString(constants->body, constants->names[at]);
    if (value == NULL) return NULL;
    /* a value that only *reaches* a twin keeps what the body gave it, exactly as
     * `By_CopyClassConstant` leaves it — this is the value half of that copy */
    stands = By_TwinReplacement(value, constants->twins);
    return stands != NULL ? stands : By_NewRef(value);
}

/* write the constants into a class namespace, and hand back what was written
 *
 * the mapping is `{name: value}` for exactly the names the captured body wrote, and it is
 * what the class is checked against afterwards */
static inline PyObject *By_CarryConstants(PyObject *ns, const By_ClassConstants *constants) {
    PyObject *carried = PyDict_New();
    Py_ssize_t at;
    if (carried == NULL) return NULL;
    for (at = 0; constants != NULL && at < constants->count; at++) {
        PyObject *value = By_ConstantValue(constants, at);
        int failed;
        if (value == NULL) continue;
        failed = By_SetInNamespace(ns, constants->names[at], value) < 0
                 || PyDict_SetItemString(carried, constants->names[at], value) < 0;
        Py_DECREF(value);
        if (failed) {
            Py_DECREF(carried);
            return NULL;
        }
    }
    return carried;
}

/* whether the finished class answers every constant with the object it was handed
 *
 * writing them into the namespace is what lets the metaclass see them, and it is enough
 * for a metaclass that only *reads* one — a `__slots__` an `ABCMeta` passes to
 * `type.__new__`, an `_fields` a registry records. it is not enough for one that *makes*
 * something of what the body wrote: an `EnumType` handed `STRICT = 'strict'` builds a
 * member out of it, and the member is not the value, so every reference the module body
 * already took would name the old one.
 *
 * name for name is what separates the two, and it is asked of the class's own dict —
 * which is where a `class` statement's namespace lands, entry for entry, and the only
 * place the comparison can be made against the raw object the body wrote. a lookup on the
 * class would run the descriptor protocol instead, so a `__class_getitem__ = classmethod(f)`
 * would answer a freshly bound method and never be identical to anything. where the check
 * fails, the interpreted definition stands — which is the answer such a class had before
 * any of this.
 *
 * that fallback carries the limit every fallback in `By_BuildClass` carries: a twin
 * extends the *twin's* base, so a class refused here while a base of this module's is
 * emitted would answer `issubclass` False where python answers True. the compile-time
 * cascade is what keeps that from arising and the runtime cannot reach as far — the
 * choice left here is between the twin and a failed import, and the twin is the better
 * of the two. nothing over the stdlib is refused here at all */
static inline int By_ConstantsHeldUp(PyObject *cls, PyObject *carried) {
    PyObject *name, *wanted;
    PyObject *own = cls != NULL && PyType_Check(cls) ? ((PyTypeObject *)cls)->tp_dict : NULL;
    Py_ssize_t position = 0;
    if (carried == NULL) return 1;
    while (PyDict_Next(carried, &position, &name, &wanted)) {
        int same;
        if (own != NULL) {
            /* borrowed, and nothing here runs while the walk is open */
            same = PyDict_GetItem(own, name) == wanted;
        } else {
            /* a metaclass answering with something that is not a class at all */
            PyObject *got = PyObject_GetAttr(cls, name);
            if (got == NULL) {
                PyErr_Clear();
                return 0;
            }
            same = got == wanted;
            Py_DECREF(got);
        }
        if (!same) return 0;
    }
    return 1;
}

/* ── a compiled method, as its class publishes it ─────────────────────────────
 *
 * python's class holds a `function` under a method's name, and reading it off an
 * instance hands back a bound `method` whose `__func__` is that function and whose
 * `__self__` is the instance. a `method_descriptor` binds to a
 * `builtin_function_or_method` instead, which has no `__func__` — so anything that
 * takes a bound method apart and puts it back together, `weakref.WeakMethod` among
 * them, refuses what a compiled class hands out.
 *
 * so the class holds one of these under the name, over the `method_descriptor` the
 * method table built. it binds the way a function does, through `PyMethod_New`, and a
 * call goes straight to the descriptor's own vectorcall, which still checks the
 * receiver's type before the compiled body is reached. like a function it can be
 * referred to weakly and written to: a class body hands a decorator the function it
 * defined, and `abc.abstractmethod` writes `__isabstractmethod__` onto the object it is
 * given and hands that same object back. what a decorator did not write is read through
 * to the descriptor, so apart from binding it answers as the descriptor did.
 *
 * the type is shared by every module in the interpreter, the way the function watcher
 * is: a direct call is licensed by recognising the method standing under a name, and
 * a method a class of another module published has to be recognised as well */
typedef struct {
    PyObject_HEAD
    vectorcallfunc vectorcall;
    /* what a call goes to: the `method_descriptor` the method table built, or whatever a
     * decorated method's lookup answered where that was not one */
    PyObject *fn;
    PyObject *dict;
    PyObject *weakrefs;
    /* the interpreted definition's function, once the twin class has been read, which is
     * where the annotations python evaluated for the method are — see
     * `By_Method_getattro` */
    PyObject *twin;
} ByMethodObject;

#define BY_METHOD_TYPE_KEY "_by_method_type_v2"

/* the type every module in this interpreter publishes its methods as, once the first
 * of them has published one — see `By_FindMethodType` */
static PyTypeObject *by_method_type = NULL;

static void By_Method_dealloc(ByMethodObject *self) {
    PyObject_GC_UnTrack(self);
    if (self->weakrefs != NULL) PyObject_ClearWeakRefs((PyObject *)self);
    Py_CLEAR(self->fn);
    Py_CLEAR(self->dict);
    Py_CLEAR(self->twin);
    PyObject_GC_Del(self);
}

static int By_Method_traverse(ByMethodObject *self, visitproc visit, void *arg) {
    Py_VISIT(self->fn);
    Py_VISIT(self->dict);
    Py_VISIT(self->twin);
    return 0;
}

/* only the `__dict__`, which is the only side a cycle can run through that the class's
 * own dict does not break first. leaving `fn` in place is what keeps a call and an
 * attribute read from meeting a cleared field */
static int By_Method_clear(ByMethodObject *self) {
    Py_CLEAR(self->dict);
    Py_CLEAR(self->twin);
    return 0;
}

/* a class method's descriptor has no vectorcall of its own: python calls it by packing the
 * arguments into a tuple, binding the class and calling what that made. where the class is
 * one the method is defined on, that is the table entry called with the class in front,
 * under the recursion check the bound call would have made — and anything else takes
 * python's own path, which raises in its own words */
static PyObject *By_ClassMethodCall(PyObject *fn, PyObject *const *args, size_t nargsf,
                                    PyObject *kwnames) {
    PyMethodDescrObject *descriptor = (PyMethodDescrObject *)fn;
    PyMethodDef *def = descriptor->d_method;
    Py_ssize_t nargs = PyVectorcall_NARGS(nargsf);
    PyObject *result;
    if ((def->ml_flags & (METH_FASTCALL | METH_KEYWORDS | METH_METHOD))
            != (METH_FASTCALL | METH_KEYWORDS)
        || nargs < 1 || !PyType_Check(args[0])
        || !PyType_IsSubtype((PyTypeObject *)args[0], PyDescr_TYPE(descriptor))) {
        return PyObject_Vectorcall(fn, args, nargsf, kwnames);
    }
    if (Py_EnterRecursiveCall(" while calling a Python object")) return NULL;
    result = ((PyCFunctionFastWithKeywords)(void (*)(void))def->ml_meth)(args[0], args + 1,
                                                                         nargs - 1, kwnames);
    Py_LeaveRecursiveCall();
    return result;
}

static PyObject *By_Method_vectorcall(PyObject *callable, PyObject *const *args, size_t nargsf,
                                      PyObject *kwnames) {
    PyObject *fn = ((ByMethodObject *)callable)->fn;
    if (BY_LIKELY(Py_IS_TYPE(fn, &PyMethodDescr_Type))) {
        vectorcallfunc call = ((PyMethodDescrObject *)fn)->vectorcall;
        if (BY_LIKELY(call != NULL)) return call(fn, args, nargsf, kwnames);
    }
    if (Py_IS_TYPE(fn, &PyClassMethodDescr_Type)) {
        return By_ClassMethodCall(fn, args, nargsf, kwnames);
    }
    return PyObject_Vectorcall(fn, args, nargsf, kwnames);
}

static PyObject *By_Method_descr_get(PyObject *self, PyObject *obj, PyObject *type) {
    (void)type;
    if (obj == NULL || obj == Py_None) return By_NewRef(self);
    return PyMethod_New(self, obj);
}

/* as a function prints itself, under the qualified name it answers — which is what a
 * decorator that wrote one sees too. the builtin the call goes to prints itself as a
 * builtin: a static method's as a method of a type object */
static PyObject *By_Method_repr(ByMethodObject *self) {
    PyObject *qualname = PyObject_GetAttrString((PyObject *)self, "__qualname__");
    PyObject *text;
    if (qualname == NULL) {
        if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return NULL;
        PyErr_Clear();
        return PyObject_Repr(self->fn);
    }
    text = PyUnicode_Check(qualname) ? PyUnicode_FromFormat("<function %U at %p>", qualname, self)
                                     : PyUnicode_FromFormat("<function %R at %p>", qualname, self);
    Py_DECREF(qualname);
    return text;
}

/* whether `name` is one of what a function answers about the `def` it was made from — where
 * it was defined, its defaults, its code and what python evaluated its annotations into —
 * none of which the builtin a call goes to has an answer to. `inspect.signature` and
 * `typing.get_type_hints` read these, as `functools.wraps` copies `__module__`
 *
 * `__closure__` is not one: the interpreted definition's cells belong to the class it was
 * defined in, which is not the type standing under the class's name */
static int By_IsDefinitionName(PyObject *name) {
    static const char *const names[] = {
        "__annotations__", "__annotate__", "__type_params__", "__module__", "__defaults__",
        "__kwdefaults__",  "__code__",     "__globals__",     "__builtins__",
    };
    size_t at;
    if (!PyUnicode_Check(name)) return 0;
    for (at = 0; at < sizeof(names) / sizeof(names[0]); at++) {
        if (PyUnicode_CompareWithASCIIString(name, names[at]) == 0) return 1;
    }
    return 0;
}

/* what a decorator wrote is this object's own; everything else it reads off a function
 * belongs to what the call goes to until the method has a definition — and then what
 * describes the `def` is that definition's, since python made it for the interpreted
 * definition and nothing compiled has a true answer to it. those are read when asked, so on
 * 3.14 the annotations are evaluated no earlier than python evaluates them. a static
 * method's builtin does answer `__module__`, with `None`, so the definition is asked first
 * rather than on a miss */
static PyObject *By_Method_getattro(PyObject *self, PyObject *name) {
    PyObject *value = PyObject_GenericGetAttr(self, name);
    PyObject *twin = ((ByMethodObject *)self)->twin;
    if (value == NULL && PyErr_ExceptionMatches(PyExc_AttributeError)) {
        if (twin != NULL && By_IsDefinitionName(name)) {
            PyErr_Clear();
            return PyObject_GetAttr(twin, name);
        }
        /* one with a definition stands in for that `function`, which answers nothing else
         * this object does not: the builtin's `__self__`, `__objclass__` and
         * `__text_signature__` are answers the definition would raise for */
        if (twin != NULL) return NULL;
        PyErr_Clear();
        value = PyObject_GetAttr(((ByMethodObject *)self)->fn, name);
    }
    return value;
}

/* a name this type itself answers — `__doc__` is one on every type — read the same way:
 * written onto the object, or else the definition's, or else read through. `closure` is
 * the name. the definition comes before the builtin because a class a metaclass built
 * holds builtins owned by `object`, whose `__qualname__` names `object` */
static PyObject *By_Method_read_through(PyObject *self, void *closure) {
    PyObject *dict = ((ByMethodObject *)self)->dict;
    PyObject *twin = ((ByMethodObject *)self)->twin;
    if (dict != NULL) {
        PyObject *own = PyDict_GetItemString(dict, (const char *)closure); /* borrowed */
        if (own != NULL) return By_NewRef(own);
    }
    if (twin != NULL) return PyObject_GetAttrString(twin, (const char *)closure);
    return PyObject_GetAttrString(((ByMethodObject *)self)->fn, (const char *)closure);
}

static int By_Method_write_through(PyObject *self, PyObject *value, void *closure) {
    PyObject *dict = PyObject_GenericGetDict(self, NULL);
    int result;
    if (dict == NULL) return -1;
    if (value != NULL) {
        result = PyDict_SetItemString(dict, (const char *)closure, value);
    } else {
        result = PyDict_DelItemString(dict, (const char *)closure);
        if (result < 0 && PyErr_ExceptionMatches(PyExc_KeyError)) {
            PyErr_Clear();
            PyErr_SetString(PyExc_AttributeError, (const char *)closure);
        }
    }
    Py_DECREF(dict);
    return result;
}

/* `copy` and `pickle` both go through this, and the descriptor's answer is to be looked
 * up again by name on its class — which finds this object */
static PyObject *By_Method_reduce(PyObject *self, PyObject *unused) {
    (void)unused;
    return PyObject_CallMethod(((ByMethodObject *)self)->fn, "__reduce__", NULL);
}

static PyMethodDef By_Method_methods[] = {
    {"__reduce__", By_Method_reduce, METH_NOARGS, NULL},
    {NULL, NULL, 0, NULL},
};

static PyGetSetDef By_Method_getset[] = {
    {"__dict__", PyObject_GenericGetDict, PyObject_GenericSetDict, NULL, NULL},
    {"__name__", By_Method_read_through, By_Method_write_through, NULL, "__name__"},
    {"__qualname__", By_Method_read_through, By_Method_write_through, NULL, "__qualname__"},
    {"__doc__", By_Method_read_through, By_Method_write_through, NULL, "__doc__"},
    {NULL, NULL, NULL, NULL, NULL},
};

/* named as what it stands in for, which is also what says a compiled body answered: an
 * interpreted class holds a `function` under the same name */
static PyTypeObject By_MethodType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.method_descriptor",
    .tp_basicsize = sizeof(ByMethodObject),
    .tp_itemsize = 0,
    .tp_dealloc = (destructor)By_Method_dealloc,
    .tp_vectorcall_offset = offsetof(ByMethodObject, vectorcall),
    .tp_repr = (reprfunc)By_Method_repr,
    .tp_call = PyVectorcall_Call,
    .tp_getattro = By_Method_getattro,
    .tp_setattro = PyObject_GenericSetAttr,
    /* so `obj.method()` from python calls it with `obj` in front rather than building
     * the bound method first, which is what binding it would have handed the call */
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_HAVE_VECTORCALL
                | Py_TPFLAGS_METHOD_DESCRIPTOR,
    .tp_traverse = (traverseproc)By_Method_traverse,
    .tp_clear = (inquiry)By_Method_clear,
    .tp_weaklistoffset = offsetof(ByMethodObject, weakrefs),
    .tp_methods = By_Method_methods,
    .tp_getset = By_Method_getset,
    .tp_descr_get = By_Method_descr_get,
    .tp_dictoffset = offsetof(ByMethodObject, dict),
    .tp_free = PyObject_GC_Del,
};

/* settle `by_method_type`: the type an earlier module shared, or else this module's own,
 * readied and shared. the key carries the layout's version, so two modules only ever
 * share a type whose objects they both read the same way */
static int By_FindMethodType(void) {
    PyInterpreterState *interpreter;
    PyObject *state, *found;
    if (by_method_type != NULL) return 0;
    interpreter = PyInterpreterState_Get();
    state = interpreter == NULL ? NULL : PyInterpreterState_GetDict(interpreter);
    if (state == NULL) {
        PyErr_SetString(PyExc_RuntimeError, "no interpreter state to share compiled methods in");
        return -1;
    }
    found = PyDict_GetItemString(state, BY_METHOD_TYPE_KEY); /* borrowed */
    if (found != NULL && PyType_Check(found)) {
        by_method_type = (PyTypeObject *)By_NewRef(found);
        return 0;
    }
    if (PyType_Ready(&By_MethodType) < 0) return -1;
    if (PyDict_SetItemString(state, BY_METHOD_TYPE_KEY, (PyObject *)&By_MethodType) < 0) return -1;
    by_method_type = (PyTypeObject *)By_NewRef((PyObject *)&By_MethodType);
    return 0;
}

/* `fn` as a method: itself where it already is one, or else one over it */
static inline PyObject *By_Method(PyObject *fn) {
    ByMethodObject *self;
    if (fn == NULL) return NULL;
    if (By_FindMethodType() < 0) return NULL;
    if (Py_IS_TYPE(fn, by_method_type)) return By_NewRef(fn);
    self = PyObject_GC_New(ByMethodObject, by_method_type);
    if (self == NULL) return NULL;
    self->vectorcall = By_Method_vectorcall;
    self->fn = By_NewRef(fn);
    self->dict = NULL;
    self->weakrefs = NULL;
    self->twin = NULL;
    PyObject_GC_Track(self);
    return (PyObject *)self;
}

/* the `method_descriptor` standing under a name, whether the class holds it as itself or
 * as a compiled method over it; NULL, with nothing raised, for anything else */
static inline PyObject *By_MethodDescriptorOf(PyObject *found) {
    if (found == NULL) return NULL;
    if (by_method_type != NULL && Py_IS_TYPE(found, by_method_type)) {
        found = ((ByMethodObject *)found)->fn;
    }
    return Py_IS_TYPE(found, &PyMethodDescr_Type) ? found : NULL;
}

/* publish each plain method among the `count` entries of `table` that `type` holds as
 * the descriptor the table built as a compiled method over that descriptor
 *
 * the caller names the entries a `class` statement wrote. the rest of a table is what
 * the runtime adds — a generator's `send`, a `__getstate__` — and python's own of those
 * are builtins that bind as builtins
 *
 * an entry is replaced only where the class's own dict holds the descriptor of that very
 * table entry. a name a slot backs holds the slot's wrapper instead, and a class the
 * interpreted definition stands for holds functions, and both are left as they are.
 *
 * a class method and a static method are published the way a class statement holds them,
 * as a `classmethod` or a `staticmethod` over the method, rather than as the descriptor and
 * the builtin the table built. the class method binds the class through `PyMethod_New`,
 * so what `C.make` hands back has a `__func__` and a `__self__`, and a call still goes to
 * the table's own descriptor, which checks the class */
static PyObject *By_PublishedMethod(PyObject *found, PyMethodDef *def) {
    PyObject *method, *published;
    if (def->ml_flags & METH_CLASS) {
        if (!Py_IS_TYPE(found, &PyClassMethodDescr_Type)
            || ((PyMethodDescrObject *)found)->d_method != def) {
            return By_NewRef(Py_None);
        }
        method = By_Method(found);
        if (method == NULL) return NULL;
        published = PyClassMethod_New(method);
        Py_DECREF(method);
        return published;
    }
    if (def->ml_flags & METH_STATIC) {
        PyObject *function;
        int ours;
        if (!Py_IS_TYPE(found, &PyStaticMethod_Type)) return By_NewRef(Py_None);
        function = PyObject_GetAttrString(found, "__func__");
        if (function == NULL) return NULL;
        ours = PyCFunction_Check(function) && ((PyCFunctionObject *)function)->m_ml == def;
        method = ours ? By_Method(function) : NULL;
        Py_DECREF(function);
        if (!ours) return By_NewRef(Py_None);
        if (method == NULL) return NULL;
        published = PyStaticMethod_New(method);
        Py_DECREF(method);
        return published;
    }
    if (!Py_IS_TYPE(found, &PyMethodDescr_Type) || ((PyMethodDescrObject *)found)->d_method != def) {
        return By_NewRef(Py_None);
    }
    return By_Method(found);
}

static int By_PublishMethods(PyObject *type, PyMethodDef *table, Py_ssize_t count) {
    PyObject *dict;
    Py_ssize_t at;
    int replaced = 0;
    if (type == NULL || !PyType_Check(type)) return 0;
    dict = ((PyTypeObject *)type)->tp_dict;
    if (dict == NULL) return 0;
    if (By_FindMethodType() < 0) return -1;
    for (at = 0; at < count; at++) {
        PyMethodDef *def = &table[at];
        PyObject *found, *method;
        int failed;
        found = PyDict_GetItemString(dict, def->ml_name); /* borrowed */
        if (found == NULL) continue;
        /* `None` for an entry the dict does not hold as the table built it */
        method = By_PublishedMethod(found, def);
        if (method == NULL) return -1;
        if (method == Py_None) {
            Py_DECREF(method);
            continue;
        }
        failed = PyDict_SetItemString(dict, def->ml_name, method) < 0;
        Py_DECREF(method);
        if (failed) return -1;
        replaced = 1;
    }
    if (replaced) PyType_Modified((PyTypeObject *)type);
    return 0;
}

/* one entry of a method table, as the descriptor a class namespace holds
 *
 * the three cases `type_add_methods` distinguishes: a class method and a static method
 * bind something other than the instance, and treating either as the plain kind hands
 * the function the wrong receiver */
static inline PyObject *By_MethodDescriptor(PyTypeObject *owner, PyMethodDef *def) {
    if (def->ml_flags & METH_CLASS) return PyDescr_NewClassMethod(owner, def);
    if (def->ml_flags & METH_STATIC) {
        PyObject *function = PyCFunction_NewEx(def, (PyObject *)owner, NULL);
        PyObject *descriptor;
        if (function == NULL) return NULL;
        descriptor = PyStaticMethod_New(function);
        Py_DECREF(function);
        return descriptor;
    }
    return PyDescr_NewMethod(owner, def);
}

/* the class `meta(name, bases, namespace, **kwds)` builds, with `methods` and
 * `constants` in that namespace
 *
 * the methods go in *before* the call rather than onto the finished type, and both
 * halves of that matter. `type.__new__` fills the type slots from the namespace, so a
 * `__repr__` entry becomes `tp_repr` with no adapter of ours; and a metaclass that
 * reads the namespace — an `ABCMeta` deciding which of the base's abstract methods
 * this class left abstract — sees what the class actually defines.
 *
 * what is carried off the body goes in for the same reason, and that is the whole of what
 * makes a class with a constant or a decorated method buildable this way: copied onto the
 * *finished* type they would land behind the metaclass's back, and a `__slots__` that
 * arrived after `type.__new__` had already given the instances a dict is not a `__slots__`
 * at all — nor is an `abstractmethod` mark that arrived after `ABCMeta` collected them.
 * what the copy cannot promise, the check after the call does — see `By_ConstantsHeldUp` —
 * and where the call does not get that far, the raise is the same refusal by another
 * route.
 *
 * the descriptors name `object` as their owner because the type they belong to is what
 * this call produces. that is also the more faithful answer: the interpreted twin holds
 * plain functions there, and a plain function checks no receiver either */
static inline PyObject *By_TypeThroughMetaclass(PyObject *module_dict, const char *name,
                                                PyObject *bases, PyObject *orig_bases,
                                                PyObject *kwds, PyMethodDef *methods,
                                                const By_ClassConstants *constants) {
    PyMethodDef *def;
    PyObject *module_name, *prepare, *args, *ns, *carried, *cls;
    PyObject *meta;
    /* asked before anything is built, because the answer is that nothing should be: a
     * namespace missing a name the body had to supply would leave the method table's own
     * undecorated entry standing under it, and the class would define something the
     * `class` statement never wrote */
    if (!By_BodyCarriesRequired(constants)) return By_LookupGlobalString(module_dict, name);
    meta = By_Metaclass(bases, kwds);
    if (meta == NULL) return NULL;
    args = Py_BuildValue("(sO)", name, bases);
    if (args == NULL) {
        Py_DECREF(meta);
        return NULL;
    }
    /* the mapping a class body is written into. `type` hands back a plain dict, but a
     * metaclass may ask for something else — an `EnumType` returns one that records the
     * order members were written in, and rejects a plain dict outright */
    prepare = PyObject_GetAttrString(meta, "__prepare__");
    if (prepare == NULL) {
        PyErr_Clear();
        ns = PyDict_New();
    } else {
        ns = PyObject_Call(prepare, args, kwds);
        Py_DECREF(prepare);
    }
    Py_DECREF(args);
    if (ns == NULL) {
        Py_DECREF(meta);
        return NULL;
    }
    /* a class body binds `__module__` from the module's own `__name__`. without it a
     * type built from C reports `builtins`, because the frame python would read that
     * name off does not exist here */
    module_name = module_dict == NULL ? NULL : PyDict_GetItemString(module_dict, "__name__");
    if (module_name != NULL && By_SetInNamespace(ns, "__module__", module_name) < 0) {
        Py_DECREF(ns);
        Py_DECREF(meta);
        return NULL;
    }
    /* what was written between the parentheses, where that is not what the class ended
     * up being built on. python records it under this name and the typing machinery
     * reads it back */
    if (orig_bases != NULL && By_SetInNamespace(ns, "__orig_bases__", orig_bases) < 0) {
        Py_DECREF(ns);
        Py_DECREF(meta);
        return NULL;
    }
    for (def = methods; def != NULL && def->ml_name != NULL; def++) {
        PyObject *descriptor = By_MethodDescriptor(&PyBaseObject_Type, def);
        int failed = descriptor == NULL || By_SetInNamespace(ns, def->ml_name, descriptor) < 0;
        Py_XDECREF(descriptor);
        if (failed) {
            Py_DECREF(ns);
            Py_DECREF(meta);
            return NULL;
        }
    }
    /* after the methods, so that a body writing a name as both leaves the value there —
     * which is the answer the check below is made against */
    carried = By_CarryConstants(ns, constants);
    if (carried == NULL) {
        Py_DECREF(ns);
        Py_DECREF(meta);
        return NULL;
    }
    args = Py_BuildValue("(sOO)", name, bases, ns);
    Py_DECREF(ns);
    if (args == NULL) {
        Py_DECREF(carried);
        Py_DECREF(meta);
        return NULL;
    }
    cls = PyObject_Call(meta, args, kwds);
    Py_DECREF(args);
    Py_DECREF(meta);
    /* the interpreted definition already built this class — the fallback source ran
     * before any of this — so a metaclass raising here is the reconstruction being wrong
     * rather than the class being unbuildable, and taking the whole import down for it
     * would be the worst of the three answers. `ssl`'s `Purpose` is the case: `EnumType`
     * is handed a namespace whose members are the twin's finished ones, and building a
     * member out of a member raises before the check below could turn it down */
    if (cls == NULL) PyErr_Clear();
    /* a `metaclass` that is not a type may hand back anything, and what it hands back is
     * what the name means — but it is not a type this module can hang a decorated method
     * on, so the interpreted definition is what stands under it. a class that disagrees
     * with what its body wrote is turned down the same way and for the same reason */
    if (cls == NULL || !PyType_Check(cls) || !By_ConstantsHeldUp(cls, carried)) {
        Py_XDECREF(cls);
        cls = By_LookupGlobalString(module_dict, name);
    }
    Py_DECREF(carried);
    return cls;
}

/* the type a class on bases outside this module gets at import
 *
 * three constructions, and which one applies is settled as late as it can be, because
 * only the running interpreter knows what the names resolved to. `bases` is the
 * unresolved tuple, which this call takes over.
 *
 * a spec is the direct construction — the type slots are this module's own functions
 * rather than python's dispatchers — so it is taken wherever it can be. `spec` is NULL
 * where the class cannot use one at all: a class keyword has nowhere to go in a spec,
 * and a class appending storage to its base needs PEP 697 to say where.
 *
 * calling the metaclass covers everything a spec cannot, at the cost of the instance
 * layout — so `through_metaclass` is false for a class with fields of its own, and the
 * interpreted definition the fallback already ran is what answers for it. it is false
 * again for anything the caller can only put on the *finished* type, because a metaclass
 * that reads its namespace would disagree with it. neither a class-level constant nor a
 * decorated method is in that position: `constants` carries both into the namespace,
 * ahead of the call, with the value the interpreted body gave them */
static inline PyObject *By_BuildClass(PyObject *module_dict, const char *name,
                                      PyObject *bases, PyObject *kwds, PyMethodDef *methods,
                                      PyType_Spec *spec, int through_metaclass,
                                      const By_ClassConstants *constants) {
    PyObject *cls, *resolved;
    if (bases == NULL) return NULL;
    resolved = By_ResolveBases(bases);
    if (resolved == NULL) {
        Py_DECREF(bases);
        return NULL;
    }
    if (spec != NULL && By_SpecTakesBases(resolved)) {
        cls = PyType_FromSpecWithBases(spec, resolved);
        if (cls != NULL && !By_OffsetsHoldUp((PyTypeObject *)cls, spec)) {
            Py_DECREF(cls);
            cls = By_LookupGlobalString(module_dict, name);
        }
    } else if (through_metaclass) {
        cls = By_TypeThroughMetaclass(module_dict, name, resolved,
                                      resolved == bases ? NULL : bases, kwds, methods,
                                      constants);
    } else {
        cls = By_LookupGlobalString(module_dict, name);
    }
    Py_DECREF(resolved);
    Py_DECREF(bases);
    return cls;
}

static inline PyObject *By_CallPython(PyObject *fn, PyObject **args, Py_ssize_t nargs) {
    if (fn == NULL) return NULL;
    return PyObject_Vectorcall(fn, args, (size_t)nargs, NULL);
}

/* the module a `from` statement reads names off
 *
 * `__import__(name, globals, None, fromlist, level)`, through the name rather than
 * through `PyImport_ImportModuleLevelObject`, so an import hook that replaced
 * `__import__` is still the one that runs. the fromlist is what makes the importer
 * resolve a *submodule* of that name, and the globals are what a relative import
 * takes its package from
 */
static inline PyObject *By_ImportModule(const char *name, PyObject *globals,
                                        const char *const *fromlist, Py_ssize_t nfrom,
                                        int level) {
    PyObject *fn = By_LookupGlobalString(globals, "__import__");
    if (fn == NULL) return NULL;
    PyObject *from = Py_None;
    if (nfrom > 0) {
        from = PyTuple_New(nfrom);
        if (from == NULL) { Py_DECREF(fn); return NULL; }
        for (Py_ssize_t i = 0; i < nfrom; i++) {
            PyObject *item = PyUnicode_FromString(fromlist[i]);
            if (item == NULL) { Py_DECREF(from); Py_DECREF(fn); return NULL; }
            PyTuple_SET_ITEM(from, i, item);
        }
    } else {
        Py_INCREF(from);
    }
    PyObject *module = PyObject_CallFunction(fn, "sOOOi", name,
                                             globals == NULL ? Py_None : globals,
                                             Py_None, from, level);
    Py_DECREF(from);
    Py_DECREF(fn);
    return module;
}

/* the `ImportError` a failed `from` raises, with the attributes an
 * `except ImportError as e` reads off it
 *
 * `PyErr_SetImportError` fills `name` and `path` but not `name_from`, which 3.12
 * added and which the interpreter's own `from` failure sets
 */
static inline void By_RaiseImportError(PyObject *message, PyObject *name, PyObject *path,
                                       PyObject *name_from) {
    PyObject *exception = PyObject_CallOneArg(PyExc_ImportError, message);
    if (exception == NULL) return;
    if (name != NULL && PyObject_SetAttrString(exception, "name", name) < 0) PyErr_Clear();
    if (path != NULL && PyObject_SetAttrString(exception, "path", path) < 0) PyErr_Clear();
#if PY_VERSION_HEX >= 0x030C0000
    if (name_from != NULL && PyObject_SetAttrString(exception, "name_from", name_from) < 0) {
        PyErr_Clear();
    }
#else
    (void)name_from;
#endif
    PyErr_SetObject(PyExc_ImportError, exception);
    Py_DECREF(exception);
}

/* whether a module is still running its own body, which is what makes a failed
 * `from` report a circular import rather than a missing name */
static inline int By_ModuleIsInitializing(PyObject *module) {
    PyObject *spec = PyObject_GetAttrString(module, "__spec__");
    if (spec == NULL) { PyErr_Clear(); return 0; }
    PyObject *flag = PyObject_GetAttrString(spec, "_initializing");
    Py_DECREF(spec);
    if (flag == NULL) { PyErr_Clear(); return 0; }
    int initializing = PyObject_IsTrue(flag);
    Py_DECREF(flag);
    if (initializing < 0) { PyErr_Clear(); return 0; }
    return initializing;
}

/* one name off a module a `from` statement imported
 *
 * not a plain attribute read, and the difference is what a guarded lazy import
 * rests on: a name the module does not have is an `ImportError`, not an
 * `AttributeError`. a circular import is the other half — it can leave the
 * attribute unset on the parent while the submodule is already in `sys.modules`
 * under its full name
 */
static inline PyObject *By_ImportFrom(PyObject *module, const char *name) {
    if (module == NULL) return NULL;
    PyObject *attr = PyUnicode_FromString(name);
    if (attr == NULL) return NULL;
    PyObject *value = PyObject_GetAttr(module, attr);
    if (value != NULL) {
        Py_DECREF(attr);
        return value;
    }
    if (!PyErr_ExceptionMatches(PyExc_AttributeError)) {
        Py_DECREF(attr);
        return NULL;
    }
    PyErr_Clear();

    PyObject *package = PyObject_GetAttrString(module, "__name__");
    if (package == NULL) {
        PyErr_Clear();
    } else if (!PyUnicode_Check(package)) {
        Py_CLEAR(package);
    }
    if (package != NULL) {
        PyObject *full = PyUnicode_FromFormat("%U.%U", package, attr);
        if (full == NULL) {
            Py_DECREF(package);
            Py_DECREF(attr);
            return NULL;
        }
        PyObject *found = PyImport_GetModule(full);
        Py_DECREF(full);
        if (found != NULL) {
            Py_DECREF(package);
            Py_DECREF(attr);
            return found;
        }
        if (PyErr_Occurred()) {
            Py_DECREF(package);
            Py_DECREF(attr);
            return NULL;
        }
    }

    PyObject *named = package;
    if (named == NULL) {
        named = PyUnicode_FromString("<unknown module name>");
        if (named == NULL) { Py_DECREF(attr); return NULL; }
    } else {
        Py_INCREF(named);
    }
    PyObject *path = PyModule_GetFilenameObject(module);
    if (path != NULL && !PyUnicode_Check(path)) Py_CLEAR(path);
    PyObject *message;
    if (path == NULL) {
        PyErr_Clear();
        message = PyUnicode_FromFormat("cannot import name %R from %R (unknown location)",
                                       attr, named);
    } else if (By_ModuleIsInitializing(module)) {
        message = PyUnicode_FromFormat(
            "cannot import name %R from partially initialized module %R "
            "(most likely due to a circular import) (%S)", attr, named, path);
    } else {
        message = PyUnicode_FromFormat("cannot import name %R from %R (%S)", attr, named, path);
    }
    if (message != NULL) {
        By_RaiseImportError(message, package, path, attr);
        Py_DECREF(message);
    }
    Py_DECREF(named);
    Py_XDECREF(path);
    Py_XDECREF(package);
    Py_DECREF(attr);
    return NULL;
}

/* a method call, resolved on the receiver. `PyObject_VectorcallMethod` avoids
 * materializing the bound method object, which is the whole reason it exists */
static inline PyObject *By_CallMethod(PyObject *receiver, PyObject *name, PyObject **args,
                                     Py_ssize_t nargs) {
    if (receiver == NULL || name == NULL) return NULL;
    /* the vectorcall method protocol wants the receiver as args[0] */
    args[0] = receiver;
    return PyObject_VectorcallMethod(name, args, (size_t)(nargs + 1) | PY_VECTORCALL_ARGUMENTS_OFFSET,
                                     NULL);
}

/* whether `o`'s *own* dict answers `name`, so that nothing read off its type is the
 * answer an attribute lookup would give
 *
 * a method is a non-data descriptor, which is the whole of why this has to be asked: a
 * value stored under the same name on the instance wins over the type's entry, so
 * `t.step = f` makes `t.step(1)` call `f` and every answer taken from the type is the
 * wrong one. an emitted class keeps its fields in its layout and the dict beside them
 * exists only so that `obj.extra = 3` works the way the interpreted twin does — so the
 * dict is very nearly always absent, and absent is a load and a comparison to establish.
 *
 * `offset` is where the dict pointer sits in the instance, which is `tp_dictoffset` and
 * so is settled long before the call: zero for a class whose instances have no dict at
 * all. it is passed rather than read here because the one thing this must not do is
 * *call* — `_PyObject_GetDictPtr` is what a question about a managed dict has to go
 * through, it lives in libpython so it cannot be inlined, and on a loop calling one
 * method it was **89 per cent** of the running time: the call is a barrier the C
 * compiler cannot keep the loop's values in registers across.
 *
 * every offset that reaches here is therefore non-negative. a negative one is the managed
 * dict this cannot read, and recording the offset is where such a type is refused — the
 * refusal leaves the licence's version zero and the site's method NULL, both of which are
 * tested before this is, so a refused type never reaches the load at all */
static inline int By_DictShadowsAt(PyObject *o, Py_ssize_t offset, PyObject *name) {
    if (offset == 0) return 0;
    PyObject *dict = *(PyObject **)((char *)o + offset);
    if (dict == NULL) return 0;
    int found = PyDict_Contains(dict, name);
    if (found < 0) {
        PyErr_Clear();
        return 1;
    }
    return found;
}

/* a call site's licence to call one compiled body without looking the method up,
 * taken once at import
 *
 * an override reached through a base-typed name has to ask the receiver which body
 * to run, and asking is the whole cost: a lookup on the type, a bound call, and the
 * boxed round trip a python-visible entry point takes. the answer is nearly always
 * the same one, so it is worked out here instead — and the three things that could
 * make it wrong later are each given a test the call site can afford.
 *
 * a *different class* is caught by comparing the receiver's type, which is exact:
 * a subclass written in the interpreter has a type object of its own.
 *
 * a method *rebound* on the class is caught by the version tag. the interpreter
 * bumps it whenever a type or any of its bases is written to — that is the signal
 * its own attribute caches watch — so the version that held when the answer was
 * checked is enough for every later call to test with one comparison.
 *
 * a value on the *instance* under the same name is caught by [`By_DictShadowsAt`], which
 * is why the name and the offset its dict sits at are kept here rather than only being
 * read at import. nothing about the class says whether one instance of it has been
 * written to.
 *
 * a zero version means the licence was refused: the name answers something this module
 * did not compile, the type will not carry a version, or its instances keep their
 * attributes somewhere the third question cannot be asked of. no version tag is ever
 * zero, so a call site armed with zero takes the ordinary call for the life of the
 * process, which is what it did before any of this */
typedef struct {
    unsigned int version;
    PyObject *name;
    /* `tp_dictoffset`, which for every type that gets this far is a word in the
     * instance — see [`By_DictShadowsAt`] */
    Py_ssize_t dict_offset;
} ByMethodLicence;

#define BY_METHOD_LICENCE_INIT { 0u, NULL, 0 }

static inline void By_ArmMethod(ByMethodLicence *licence, PyObject *type, const char *name,
                                PyCFunction body) {
    licence->version = 0u;
    licence->name = NULL;
    licence->dict_offset = 0;
    if (type == NULL || !PyType_Check(type)) return;
    PyTypeObject *owner = (PyTypeObject *)type;
    if (owner->tp_flags & BY_INLINE_VALUES_FLAG) return;
    /* a negative offset is a *managed* dict, which lives outside the instance and can
     * only be reached through a call. refusing one keeps the test at the call site down
     * to loads — and no emitted class has one, because a class asking for a dict is
     * given a word of its own for it */
    if (owner->tp_dictoffset < 0) return;
    licence->dict_offset = owner->tp_dictoffset;
    /* the lookup is also what makes the interpreter assign a version tag: a type
     * nothing has been read from yet has none at all */
    PyObject *found = PyObject_GetAttrString(type, name);
    if (found == NULL) {
        PyErr_Clear();
        return;
    }
    /* reading the type's own attribute hands back the descriptor rather than a bound
     * method, so the compiled entry point is reachable through it */
    PyObject *descriptor = By_MethodDescriptorOf(found);
    int compiled = descriptor != NULL
                   && ((PyMethodDescrObject *)descriptor)->d_method != NULL
                   && ((PyMethodDescrObject *)descriptor)->d_method->ml_meth == body;
    Py_DECREF(found);
    if (!compiled) return;
#if PY_VERSION_HEX >= 0x030C0000
    /* from 3.12 the tag is handed out on request rather than by whoever looks an
     * attribute up, and a request is the only thing that reliably produces one */
    if (!PyUnstable_Type_AssignVersionTag(owner)) return;
#endif
    PyObject *interned = By_InternedStr(name, (Py_ssize_t)strlen(name));
    if (interned == NULL) {
        PyErr_Clear();
        return;
    }
    licence->name = interned;
    /* zero is both "never given a tag" and "written to since", and either is a refusal */
    licence->version = owner->tp_version_tag;
}

/* a receiver an emitted site could not have had
 *
 * the two tests below read the receiver's type without asking first whether there is a
 * receiver. they can, because an emitted site has no way to reach them without one:
 *
 * - every read of a register is on a path that has written it, which the IR verifier
 *   proves, and a register a *name* may not have been bound in yet carries a byte that
 *   every read of it tests — so the path this is about raises `UnboundLocalError`
 * - an operation that can fail takes its error edge before it writes its destination,
 *   so a written register holds what the operation answered and never the failure
 * - a register a store or a display took the reference over from is one liveness has
 *   already said nothing reads again, and a *parameter* can never be one of those at
 *   all: handing over is refused for anything below the parameter count
 *
 * asking anyway cost 5.2 per cent of the `props_ext` benchmark and 1.5 of `inherit`,
 * because the `&&` before the load is a sequence point the C compiler may not move the
 * type read across: the whole chain stays a ladder of branches rather than folding into
 * a run of straight-line loads and bitwise tests.
 *
 * the claim is not simply dropped, though: the re-check build is where a licence's
 * claims are asked out loud, and this is one of them */
#ifdef BY_LICENCE_RECHECK
#define BY_LICENCE_RECEIVER(o, where) \
    do { if ((o) == NULL) Py_FatalError("by: " where " was given no receiver"); } while (0)
#else
#define BY_LICENCE_RECEIVER(o, where) ((void)0)
#endif

/* whether `o` is exactly `type`, `type` still answers as [`By_ArmMethod`] found, and
 * `o` itself does not answer the name */
static inline char By_MethodStands(PyObject *o, PyObject *type,
                                   const ByMethodLicence *licence) {
    BY_LICENCE_RECEIVER(o, "By_MethodStands");
    return (char)(licence->version != 0u && (PyObject *)Py_TYPE(o) == type
                  && ((PyTypeObject *)type)->tp_version_tag == licence->version
                  && !By_DictShadowsAt(o, licence->dict_offset, licence->name));
}

/* a read or a write of one `@property`, licensed to go straight to the compiled half
 *
 * a class another class extends is emitted as a heap type that sets
 * `Py_TPFLAGS_BASETYPE`, so a class written in the interpreter may subclass it and
 * override a half — which is why the direct call a class nothing extends gets is not
 * available here. without a middle option the read and the write go the whole way round
 * the descriptor protocol, by name, which on the `props_ext` benchmark is 28 times what
 * the same loop costs with this test in front of the halves.
 *
 * this is that middle option, and it is the [`ByMethodLicence`] question with one part
 * of it gone. a *different class* is caught by comparing the receiver's type, which is
 * exact: an interpreted subclass has a type object of its own, whether or not it
 * overrides anything. a half *rebound* on the class — `C.v = property(...)`, or a write
 * to any base — is caught by the version tag, which the interpreter zeroes on the type
 * and on every subclass of it whenever one of them is written to.
 *
 * what is *not* asked, and does not need to be, is whether the instance shadows the
 * name. a `property` is a data descriptor, so `PyObject_GenericGetAttr` takes it from
 * the type and never consults the instance dict — a method needs that question only
 * because it is a non-data descriptor a value on the instance can sit in front of.
 *
 * a zero version means the licence was refused, and no version tag is ever zero, so a
 * site armed with zero takes the protocol for the life of the process */
typedef struct {
    unsigned int version;
} ByAccessorLicence;

#define BY_ACCESSOR_LICENCE_INIT { 0u }

/* whether `property_object`'s `half` is the descriptor `def` was published under
 *
 * both halves are asked about even where the site only reads one, because what this has
 * to establish is that the object standing under the name is the one this module built —
 * a decorator that wrapped only the setter leaves the getter reachable and the pair no
 * longer ours. `def` is NULL for a half the class never wrote, and python answers `None`
 * for one a `property` was not given, so absence has to match absence too */
static inline int By_AccessorHalfIs(PyObject *property_object, const char *half,
                                    PyMethodDef *def) {
    PyObject *found = PyObject_GetAttrString(property_object, half);
    int matches;
    if (found == NULL) {
        PyErr_Clear();
        return 0;
    }
    if (def == NULL) {
        matches = found == Py_None;
    } else {
        matches = Py_IS_TYPE(found, &PyMethodDescr_Type)
                  && ((PyMethodDescrObject *)found)->d_method == def;
    }
    Py_DECREF(found);
    return matches;
}

/* work out, once at import, whether `type`'s `name` is still the pair this module
 * compiled — and record the version that held when it was
 *
 * the type is asked rather than trusted, so a decorator that replaced the property, a
 * twin whose own `property` was carried over, or a base that answers the name first all
 * refuse the licence instead of being called past */
static inline void By_ArmAccessor(ByAccessorLicence *licence, PyObject *type, const char *name,
                                  PyMethodDef *get, PyMethodDef *set) {
    licence->version = 0u;
    if (type == NULL || !PyType_Check(type)) return;
    PyTypeObject *owner = (PyTypeObject *)type;
    /* the licence skips the whole lookup, so a type that answers a read or a write with
     * anything of its own is not one this can speak for */
    if (owner->tp_getattro != PyObject_GenericGetAttr) return;
    if (owner->tp_setattro != PyObject_GenericSetAttr) return;
    PyObject *found = PyObject_GetAttrString(type, name);
    if (found == NULL) {
        PyErr_Clear();
        return;
    }
    /* a `property` reached through the type hands back itself rather than calling a
     * half, which is what makes the two halves readable off it here */
    int published = Py_IS_TYPE(found, &PyProperty_Type) && By_AccessorHalfIs(found, "fget", get)
                    && By_AccessorHalfIs(found, "fset", set);
    Py_DECREF(found);
    if (!published) return;
#if PY_VERSION_HEX >= 0x030C0000
    /* from 3.12 the tag is handed out on request rather than by whoever looks an
     * attribute up, and a request is the only thing that reliably produces one */
    if (!PyUnstable_Type_AssignVersionTag(owner)) return;
#endif
    /* zero is both "never given a tag" and "written to since", and either is a refusal */
    licence->version = owner->tp_version_tag;
}

/* one member's licence joined into its class's: the class's stands only where every member
 * was found as compiled, all under the one version */
static inline void By_JoinLicence(unsigned int *version, int *first, unsigned int member) {
    if (*first) {
        *version = member;
        *first = 0;
    } else if (*version != member) {
        *version = 0u;
    }
}

/* whether `o` is exactly `type` and `type` still answers as [`By_ArmAccessor`] found */
static inline char By_AccessorStands(PyObject *o, PyObject *type,
                                     const ByAccessorLicence *licence) {
    BY_LICENCE_RECEIVER(o, "By_AccessorStands");
    return (char)(licence->version != 0u && (PyObject *)Py_TYPE(o) == type
                  && ((PyTypeObject *)type)->tp_version_tag == licence->version);
}

/* ── the licence re-check mode ──────────────────────────────────────────────────
 *
 * a *licence* is a compile-time decision that a call may go straight to a compiled
 * body. some of them rest on nothing at runtime at all — a class the emitter laid out
 * as a static type refuses both subclassing and `setattr`, so the compiler concludes
 * that no override and no rebinding can exist and emits the call bare. the others stand
 * behind the tests above, which are a type-pointer comparison and a cached version tag:
 * cheap stand-ins for the lookup, not the lookup.
 *
 * either way the licence is a claim nothing checks, and a licence that is wrong is a
 * wrong answer with nothing to report it — an override that stops being seen, a
 * rebinding nothing notices. the checks below are that claim asked out loud, against
 * the receiver in hand, at the moment the licensed call is about to run: they do the
 * whole lookup the call skipped and compare where it lands with the body about to be
 * called.
 *
 * they cost a lookup per call, which is the entire thing a licence exists to avoid, so
 * `by compile --licence-recheck` is what turns them on and nothing else defines
 * `BY_LICENCE_RECHECK`. the generated C says which mode it was written in, so a build
 * and its own source never disagree about it */
#ifdef BY_LICENCE_RECHECK

#include <stdio.h>
#include <stdlib.h>

/* what a licensed call site does when the lookup it skipped would have reached
 * something other than the body it is about to run
 *
 * there is no answer left to give: the fast arm has already been chosen and the next
 * instruction runs a body that is not what the name means. so it names the class, the
 * member and what changed, and stops the process */
static void By_LicenceFailed(const char *class_name, const char *member, const char *what,
                             const char *detail) {
    /* whatever the program has already printed goes out first, so the abort lands after
     * the output that led to it rather than in place of it. python's `sys.stdout` does
     * its own buffering above the C stream, so both layers have to be asked — and a
     * failure to flush is nothing to report on top of what is already being reported */
    PyErr_Clear();
    PyObject *out = PySys_GetObject("stdout");
    if (out != NULL) {
        PyObject *flushed = PyObject_CallMethod(out, "flush", NULL);
        Py_XDECREF(flushed);
        PyErr_Clear();
    }
    fflush(NULL);
    fprintf(stderr, "by: licence re-check failed on %s.%s: %s", class_name, member, what);
    if (detail != NULL) fprintf(stderr, " (%s)", detail);
    fprintf(stderr,
            "\nby: the compiled call was licensed to skip this lookup, and the lookup no "
            "longer agrees\n");
    fflush(stderr);
    abort();
}

/* re-ask the method lookup a licensed direct call skipped
 *
 * `body` is the compiled entry point the call is about to run. it is NULL for a member
 * the emitted type answers through a *slot* — an operator dunder, say: `PyType_Ready`
 * publishes a wrapper around the slot under that name rather than the `tp_methods`
 * entry, so there is no `PyMethodDef` to compare against and what remains checkable is
 * the receiver's class, which for an operator is the whole of the licence anyway */
static void By_RecheckMethod(PyObject *o, PyObject *type, const char *class_name,
                             const char *member, PyCFunction body) {
    if (o == NULL) {
        By_LicenceFailed(class_name, member, "the receiver is NULL", NULL);
        return;
    }
    if ((PyObject *)Py_TYPE(o) != type) {
        By_LicenceFailed(class_name, member, "the receiver is not this class",
                         Py_TYPE(o)->tp_name);
        return;
    }
    if (body == NULL) return;
    /* the instance is asked, not the type: a method is a non-data descriptor, so a
     * value stored on the instance under the same name is what a lookup answers with,
     * and that is one of the three ways a licence goes wrong */
    PyObject *found = PyObject_GetAttrString(o, member);
    if (found == NULL) {
        PyErr_Clear();
        By_LicenceFailed(class_name, member, "the name no longer resolves on the receiver",
                         NULL);
        return;
    }
    /* the lookup hands back a *bound* method, so both halves are asked about: the
     * definition it carries has to be the compiled body, and it has to be bound to this
     * receiver rather than to something the lookup found on the way. a compiled method
     * binds as a function does, and a table entry nothing replaced binds as a builtin */
    int compiled;
    if (PyMethod_Check(found)) {
        PyObject *descriptor = By_MethodDescriptorOf(PyMethod_GET_FUNCTION(found));
        compiled = descriptor != NULL
                   && ((PyMethodDescrObject *)descriptor)->d_method->ml_meth == body
                   && PyMethod_GET_SELF(found) == o;
    } else {
        compiled = (Py_IS_TYPE(found, &PyCFunction_Type) || Py_IS_TYPE(found, &PyCMethod_Type))
                   && ((PyCFunctionObject *)found)->m_ml != NULL
                   && ((PyCFunctionObject *)found)->m_ml->ml_meth == body
                   && ((PyCFunctionObject *)found)->m_self == o;
    }
    /* the name of a type outlives the reference the lookup took: a bound builtin's type
     * is one of the interpreter's own statics */
    const char *reached = Py_TYPE(found)->tp_name;
    Py_DECREF(found);
    if (!compiled) {
        By_LicenceFailed(class_name, member,
                         "the lookup reaches something other than the compiled body",
                         reached);
    }
}

/* re-ask the property lookup a licensed direct call to one half skipped
 *
 * the *descriptor* is resolved and left alone rather than invoked: calling a half here
 * would run a body the program is already running, and a getter that writes to `self`
 * would then have done it twice. resolving is enough, because resolving is the whole of
 * what the licensed call skipped — a `property` is a data descriptor, so where the
 * descriptor comes from decides the answer */
static void By_RecheckAccessor(PyObject *o, PyObject *type, const char *class_name,
                               const char *name, PyMethodDef *get, PyMethodDef *set) {
    if (o == NULL) {
        By_LicenceFailed(class_name, name, "the receiver is NULL", NULL);
        return;
    }
    if ((PyObject *)Py_TYPE(o) != type) {
        By_LicenceFailed(class_name, name, "the receiver is not this class",
                         Py_TYPE(o)->tp_name);
        return;
    }
    PyTypeObject *owner = (PyTypeObject *)type;
    if (owner->tp_getattro != PyObject_GenericGetAttr
        || owner->tp_setattro != PyObject_GenericSetAttr) {
        By_LicenceFailed(class_name, name,
                         "the class answers reads or writes with a hook of its own", NULL);
        return;
    }
    PyObject *found = PyObject_GetAttrString(type, name);
    if (found == NULL) {
        PyErr_Clear();
        By_LicenceFailed(class_name, name, "the name no longer resolves on the class", NULL);
        return;
    }
    int published = Py_IS_TYPE(found, &PyProperty_Type) && By_AccessorHalfIs(found, "fget", get)
                    && By_AccessorHalfIs(found, "fset", set);
    const char *reached = Py_TYPE(found)->tp_name;
    Py_DECREF(found);
    if (!published) {
        By_LicenceFailed(class_name, name,
                         "the class no longer publishes the compiled property", reached);
    }
}

#endif /* BY_LICENCE_RECHECK */

/* what one call site remembers about the method name it keeps calling
 *
 * `line.split(" ")`, `part.startswith("w")`, `part.upper()` — a loop over strings
 * calls the same builtin on the same type every trip, and `PyObject_VectorcallMethod`
 * re-derives it every trip: a lookup down the type, then a `method_vectorcall_*`
 * that unpacks the calling convention again. on the string benchmark that rederivation
 * is **28 per cent** of the running time, more than the splitting, the uppercasing
 * and the joining put together.
 *
 * so a site records what it found. `method` is the answer and doubles as the
 * armed/refused flag; `type` and `version` are the two things that could make the
 * answer wrong later, and both are recorded on a refusal too, so a receiver the site
 * cannot serve is asked about once rather than on every trip.
 *
 * a *different class* is caught by the type pointer, which is exact — a subclass
 * that overrides the method has a type object of its own and never matches. a method
 * *rebound* on the type is caught by the version tag, which the interpreter zeroes
 * whenever a type or any of its bases is written to. `type` is only ever compared,
 * never followed, so a type that has since been freed cannot be read through it.
 * a value on the *instance* under the same name is caught where the answer is used,
 * because it is a fact about the receiver rather than about its class.
 *
 * `misses` is what keeps a site that cannot settle from costing more than having no
 * site at all. a loop over a list holding two classes turn about re-derives the answer
 * every trip and never gets to use one, and re-deriving is dearer than the ordinary
 * call — so after a few of those the site stops trying and every later call takes the
 * ordinary path. it counts re-derivations rather than calls, so a site that settles
 * pays nothing for it, and a site whose one class is written to a handful of times
 * still gets to re-settle.
 *
 * the fields cannot be written as one, so what keeps a *reader* from seeing half of
 * one arming and half of another is that only one thread runs at a time. a site is
 * therefore only used where that holds: an emitted module says `Py_MOD_GIL_NOT_USED`,
 * and on a free-threaded build every call takes the ordinary path instead. two threads
 * arming a site at once would otherwise be able to leave one type's name paired with
 * another type's body, which is not a wrong answer but a call into the wrong object */
typedef struct {
    PyObject *type;
    unsigned int version;
    PyMethodDef *method;
    unsigned int misses;
    /* where this type keeps an instance's dict, for the shadow test the answer is used
     * under — see [`By_DictShadowsAt`] */
    Py_ssize_t dict_offset;
} ByMethodSite;

#define BY_METHOD_SITE_INIT { NULL, 0u, NULL, 0u, 0 }

/* how many times a site re-derives its answer before it settles for the ordinary call
 *
 * low, because a site that has not settled by now is one whose receivers vary, and each
 * further attempt is a lookup on top of the call it was supposed to save. high enough
 * that arming, a refusal, and a class written to once or twice all fit under it */
#define BY_METHOD_SITE_MISSES 8u

#ifndef Py_GIL_DISABLED

/* the two calling conventions a site can dispatch without repacking the arguments */
typedef PyObject *(*ByFastCall)(PyObject *, PyObject *const *, Py_ssize_t);
typedef PyObject *(*ByFastKwCall)(PyObject *, PyObject *const *, Py_ssize_t, PyObject *);

/* work out what `name` on `tp` is, and record it — or record that it cannot be served
 *
 * every refusal here is a case where reaching the method through the descriptor would
 * not be what an attribute lookup does:
 *
 * - a metaclass, or a `tp_getattro` of the type's own, can answer the name with
 *   something other than what is on the type
 * - a type whose instances keep their attributes inline, or in a managed dict: an
 *   instance's own value shadows the type's entry, so the answer only stands where the
 *   receiver can be asked, and neither of those two can be asked with a load. every
 *   other storage is one, made at the call rather than here — nothing about the class
 *   says whether one instance of it has been written to
 * - anything but a builtin method descriptor has a `__get__` of its own to run
 * - a calling convention the site cannot lay the arguments out for, which is every
 *   convention that wants a tuple, and every one that wants an argument this does not
 *   have to give
 *
 * a heap type is *not* refused, though it was while a zero `tp_dictoffset` was the only
 * way this had of ruling an instance dict out. the rest of what a heap type can do that
 * a static one cannot is caught by what is already here: a rebinding zeroes the version
 * tag, on the class or on any of its bases; a subclass has a type object of its own; and
 * a version tag is drawn from a counter that only ever goes up, so a type freed and
 * another built where it stood cannot answer to the first one's version. what the
 * refusal *was* also covering, and had to be written out on its own, is the calling
 * convention: `METH_METHOD` is a heap type's alone.
 *
 * the type and its version are recorded before the first thing that can refuse, so
 * that a refusal is remembered on the same terms an answer is: a receiver this site
 * will never be able to serve — every instance of a class written in the interpreter
 * — is asked about once and then costs the same two comparisons as a hit.
 *
 * that order matters for a second reason. the lookup below runs no python of its own,
 * but it was once an attribute read that could, and nothing here should rest on which
 * it is: it sits *after* the type has been recorded and the answer cleared, so a site
 * another thread arms in the middle of this one is left holding that thread's type
 * against this one's version, which no receiver matches, rather than that thread's type
 * against this one's body */
static void By_ArmMethodSite(ByMethodSite *site, PyTypeObject *tp, PyObject *name,
                             Py_ssize_t nargs) {
    site->type = (PyObject *)tp;
    site->version = tp->tp_version_tag;
    site->method = NULL;
    site->dict_offset = 0;
    if (!Py_IS_TYPE(tp, &PyType_Type)) return;
    if (tp->tp_getattro != PyObject_GenericGetAttr) return;
    if (tp->tp_flags & BY_INLINE_VALUES_FLAG) return;
    if (tp->tp_dictoffset < 0) return;
    site->dict_offset = tp->tp_dictoffset;
    /* the entry an instance's lookup finds on the type, borrowed and with no descriptor
     * run to find it. reading the attribute off the class instead runs a `__get__` of
     * the entry's own, and what that hands back for the class need not be what it hands
     * back for an instance */
    PyObject *found = By_MethodDescriptorOf(_PyType_Lookup(tp, name));
    if (found == NULL) return;
    /* a method descriptor serves the instances of the type that defined it, and refuses
     * anything else with a `TypeError`. `pop = list.pop` in a class of its own is that
     * refusal, and calling the entry point directly would hand `list_pop` a receiver
     * that is not a list */
    if (!PyType_IsSubtype(tp, PyDescr_TYPE(found))) return;
    PyMethodDef *method = ((PyMethodDescrObject *)found)->d_method;
    /* the whole of the flags decides, with only the one bit that says nothing about the
     * arguments set aside. a mask naming the conventions this knows would *drop* the
     * bits it does not know rather than refuse them, and `METH_METHOD` is exactly such
     * a bit: its entry point takes the defining class between the receiver and the
     * arguments, so calling it as a plain fast call writes the argument array into the
     * class parameter. `re.Pattern.search` is one, and only a heap type can carry the
     * flag at all — which is why this went unseen while heap types were refused
     * outright */
    int shape = method->ml_flags & ~METH_COEXIST;
    if (!((shape == METH_NOARGS && nargs == 0) || (shape == METH_O && nargs == 1)
          || shape == METH_FASTCALL || shape == (METH_FASTCALL | METH_KEYWORDS))) {
        return;
    }
#if PY_VERSION_HEX >= 0x030C0000
    /* from 3.12 the tag is handed out on request rather than by whoever looks an
     * attribute up, and a request is the only thing that reliably produces one */
    if (!PyUnstable_Type_AssignVersionTag(tp)) return;
#endif
    site->version = tp->tp_version_tag;
    /* zero is both "never given a tag" and "written to since", and either is a refusal */
    if (site->version == 0u) return;
    site->method = method;
}

#endif /* Py_GIL_DISABLED */

/* a method call that remembers what the name resolved to last time
 *
 * the direct call is the body of `method_vectorcall_*` with the convention already
 * decided. what it leaves out is that function's `Py_EnterRecursiveCall`, which is
 * a third of what this saves — and which is not what bounds recursion here anyway:
 * a builtin can only recurse by calling back into python, and every route back into
 * python passes through the interpreter's own eval loop, which counts. every other
 * direct C-API call in this runtime — `By_GetItem` reaching a `__getitem__`, and the
 * rest — is already written on that understanding.
 *
 * on a free-threaded build there is no site at all: the four fields cannot be read
 * or written as one, so the whole thing is left to `By_CallMethod` */
static inline PyObject *By_CallMethodSite(ByMethodSite *site, PyObject *receiver,
                                          PyObject *name, PyObject **args, Py_ssize_t nargs) {
#ifndef Py_GIL_DISABLED
    if (BY_LIKELY(receiver != NULL && name != NULL)) {
        PyTypeObject *tp = Py_TYPE(receiver);
        if (BY_UNLIKELY((PyObject *)tp != site->type || tp->tp_version_tag != site->version)) {
            if (site->misses >= BY_METHOD_SITE_MISSES) {
                return By_CallMethod(receiver, name, args, nargs);
            }
            site->misses++;
            By_ArmMethodSite(site, tp, name, nargs);
        }
        PyMethodDef *method = site->method;
        if (BY_LIKELY(method != NULL)
            && BY_LIKELY(!By_DictShadowsAt(receiver, site->dict_offset, name))) {
            switch (method->ml_flags
                    & (METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O | METH_FASTCALL)) {
            case METH_NOARGS:
                return method->ml_meth(receiver, NULL);
            case METH_O:
                return method->ml_meth(receiver, args[1]);
            case METH_FASTCALL:
                return ((ByFastCall)(void (*)(void))method->ml_meth)(receiver, args + 1, nargs);
            default:
                return ((ByFastKwCall)(void (*)(void))method->ml_meth)(receiver, args + 1, nargs,
                                                                       NULL);
            }
        }
    }
#else
    (void)site;
#endif
    return By_CallMethod(receiver, name, args, nargs);
}

/* `list.append(value)` without the attribute lookup
 *
 * the lookup is the whole cost of appending in a loop: `PyObject_VectorcallMethod`
 * walks the type to find the method every time, where the receiver's type was
 * known when the call was compiled. anything that is not an exact list takes the
 * ordinary path, so a subclass with its own `append` still reaches it
 */
static inline PyObject *By_ListAppend(PyObject *receiver, PyObject *name, PyObject **args,
                                      Py_ssize_t nargs) {
    if (nargs == 1 && receiver != NULL && PyList_CheckExact(receiver)) {
        if (PyList_Append(receiver, args[1]) < 0) return NULL;
        return By_NewRef(Py_None);
    }
    return By_CallMethod(receiver, name, args, nargs);
}

/* the `str` methods that have a C-API entry point of their own
 *
 * a method site already skips the attribute lookup, but it still ends in an
 * indirect call into the method's python-facing wrapper, which unpacks the
 * argument array again before doing the work. for these four the work itself is
 * reachable directly, so the wrapper and the argument array both go.
 *
 * each one takes the ordinary path — the same site, so the fallback is no worse
 * than it was — unless the receiver is an *exact* `str` and the arguments are the
 * shape the C-API entry point serves. that is what keeps the answer python's own
 * rather than an approximation of it: a `str` subclass with a method of its own, a
 * separator that is not a string, a `startswith` given a tuple of prefixes or a
 * start and end range, all fall through to `str`'s own method. these are all
 * ordinary calls, so the argument slot 0 that `By_CallMethod` fills with the
 * receiver is still reserved and the arguments still begin at `args[1]` */
static inline PyObject *By_StrSplit(ByMethodSite *site, PyObject *receiver, PyObject *name,
                                    PyObject **args, Py_ssize_t nargs) {
    if (BY_LIKELY(receiver != NULL && PyUnicode_CheckExact(receiver))) {
        /* `PyUnicode_Split` is `str.split`'s body with the argument parsing lifted
         * off, and it reads a NULL separator as the whitespace split — which is what
         * `str.split()` and `str.split(None)` both mean */
        if (nargs == 0) return PyUnicode_Split(receiver, NULL, -1);
        if (nargs == 1) {
            PyObject *separator = args[1];
            if (separator == Py_None) return PyUnicode_Split(receiver, NULL, -1);
            if (BY_LIKELY(PyUnicode_Check(separator))) {
                return PyUnicode_Split(receiver, separator, -1);
            }
        }
    }
    return By_CallMethodSite(site, receiver, name, args, nargs);
}

static inline PyObject *By_StrStartswith(ByMethodSite *site, PyObject *receiver, PyObject *name,
                                         PyObject **args, Py_ssize_t nargs) {
    if (BY_LIKELY(nargs == 1 && receiver != NULL && PyUnicode_CheckExact(receiver))
        && BY_LIKELY(PyUnicode_Check(args[1]))) {
        /* the range `str.startswith` uses when it is given none, and `-1` for the
         * direction is the prefix end. a tuple of prefixes is not this shape and is
         * left to the method, which knows how to walk one */
        Py_ssize_t matched = PyUnicode_Tailmatch(receiver, args[1], 0, PY_SSIZE_T_MAX, -1);
        if (BY_UNLIKELY(matched < 0)) return NULL;
        return By_NewRef(matched ? Py_True : Py_False);
    }
    return By_CallMethodSite(site, receiver, name, args, nargs);
}

static inline PyObject *By_StrJoin(ByMethodSite *site, PyObject *receiver, PyObject *name,
                                   PyObject **args, Py_ssize_t nargs) {
    if (BY_LIKELY(nargs == 1 && receiver != NULL && PyUnicode_CheckExact(receiver))) {
        return PyUnicode_Join(receiver, args[1]);
    }
    return By_CallMethodSite(site, receiver, name, args, nargs);
}

/* `str.upper` has no C-API entry point, so this is the one of the four that has to
 * repeat what the interpreter does rather than call it. it repeats only the arm
 * that is small enough to be obviously the same: for a string that is entirely
 * ascii, `unicode_upper` builds an ascii result of the same length and maps each
 * byte through `Py_TOUPPER`, which is what this does. every other string — anything
 * carrying a character above 127, where the mapping is the full unicode one and can
 * change the length — is left to the method */
static inline PyObject *By_StrUpper(ByMethodSite *site, PyObject *receiver, PyObject *name,
                                    PyObject **args, Py_ssize_t nargs) {
    if (BY_LIKELY(nargs == 0 && receiver != NULL && PyUnicode_CheckExact(receiver))
        && BY_LIKELY(PyUnicode_IS_ASCII(receiver))) {
        Py_ssize_t length = PyUnicode_GET_LENGTH(receiver);
        PyObject *upper = PyUnicode_New(length, 127);
        if (BY_UNLIKELY(upper == NULL)) return NULL;
        const unsigned char *source = (const unsigned char *)PyUnicode_DATA(receiver);
        unsigned char *target = (unsigned char *)PyUnicode_DATA(upper);
        for (Py_ssize_t i = 0; i < length; i++) {
            target[i] = (unsigned char)Py_TOUPPER(source[i]);
        }
        return upper;
    }
    return By_CallMethodSite(site, receiver, name, args, nargs);
}

/* the tests a `match` case makes about the *shape* of its subject
 *
 * a sequence pattern matches what the interpreter's own `MATCH_SEQUENCE` matches:
 * a type flagged as a sequence, which `str`, `bytes` and `bytearray` are not — so
 * `case [a, b]:` never takes a two-character string apart
 */
/* a class pattern's own test is `isinstance`, and `By_SoundIs` is that function for one
 * class. it matters most when the answer is no, which is the common answer in a ladder
 * of cases: `PyObject_IsInstance` asks the subject for its `__class__` every time it
 * decides against, and that question runs nothing at all for a subject whose type reads
 * attributes the generic way and inherits `object`'s own `__class__` — the two facts
 * `By_SoundIsOther` establishes before it skips the lookup */
static inline char By_IsInstance(PyObject *o, PyObject *class_) {
    int result = By_SoundIs(o, class_);
    return result < 0 ? 2 : (char)result;
}

static inline char By_IsMatchSequence(PyObject *o) {
    return (char)(o != NULL && PyType_HasFeature(Py_TYPE(o), Py_TPFLAGS_SEQUENCE));
}

/* the refusal `__annotations__` gives where a class's own could not be carried across
 *
 * `type_get_annotations` hands whatever it finds under that name in `tp_dict` to its own
 * `tp_descr_get`, so a descriptor written there is how an emitted type says no. it has
 * to be able to: an *absent* `__annotations__` is not a loud failure the way an absent
 * attribute is, because python invents an empty mapping for it on the spot — which is a
 * wrong answer wearing a right one's clothes */
static PyObject *By_LostAnnotations_get(PyObject *self, PyObject *object, PyObject *type) {
    (void)self;
    (void)object;
    PyErr_Format(PyExc_AttributeError, "type object '%s' has no attribute '__annotations__'",
                 type != NULL && PyType_Check(type) ? ((PyTypeObject *)type)->tp_name
                                                    : "<unknown>");
    return NULL;
}

static PyTypeObject By_LostAnnotationsType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.lost_annotations",
    .tp_basicsize = sizeof(PyObject),
    .tp_itemsize = 0,
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_descr_get = By_LostAnnotations_get,
};

static inline PyObject *By_LostAnnotations(void) {
    if (PyType_Ready(&By_LostAnnotationsType) < 0) return NULL;
    return PyObject_New(PyObject, &By_LostAnnotationsType);
}

/* where a class keeps the annotations its body wrote, once they are a mapping
 *
 * python 3.14 stopped writing them at the `class` statement: the body leaves a function
 * that computes them, and the mapping is worked out at the first read and kept under a
 * key of its own. below that the body writes the mapping straight into `__annotations__`
 * and there is no second key. either way this is the one the type reads back through */
#if PY_VERSION_HEX >= 0x030E0000
#define BY_ANNOTATIONS "__annotations_cache__"
#else
#define BY_ANNOTATIONS "__annotations__"
#endif

/* make a class's annotations a mapping now, while the names in them still mean what the
 * body meant
 *
 * on 3.14 they are worked out on demand, and every name in one resolves through the
 * module namespace — which is about to stop holding this module's classes, because the
 * compiled types are about to replace them. so a deferred read would answer about
 * whichever class was under the name by then, and it is `By_SettledValue`'s whole job to
 * know that the two are different. reading here settles them against the definitions the
 * body wrote, exactly as every version below 3.14 settles them at the `class` statement.
 *
 * deferring is also what lets an annotation name something that never gets defined, and
 * such a class has no mapping at all — so what goes in is the refusal, which
 * `By_CarryAnnotations` will carry across as one. an empty mapping would be the wrong
 * answer, and the twin is about to stop being reachable in any case */
static inline void By_SettleAnnotations(PyObject *cls) {
#if PY_VERSION_HEX >= 0x030E0000
    PyObject *dict = ((PyTypeObject *)cls)->tp_dict;
    PyObject *settled = PyObject_GetAttrString(cls, "__annotations__");
    if (settled == NULL) {
        PyErr_Clear();
        settled = By_LostAnnotations();
        if (settled == NULL) {
            PyErr_Clear();
            return;
        }
    }
    if (dict == NULL || PyDict_SetItemString(dict, BY_ANNOTATIONS, settled) < 0) {
        PyErr_Clear();
    }
    Py_DECREF(settled);
#else
    (void)cls;
#endif
}

/* the interpreted definition still standing under a class's name
 *
 * what a `class` statement leaves is a heap type, so a name the module body went on to
 * rebind to something from outside — `operator`'s trailing `from _operator import *` —
 * is not one, and answers nothing rather than answering somebody else's class */
static inline PyObject *By_ClassTwin(PyObject *module_dict, const char *name) {
    PyObject *cls = PyDict_GetItemString(module_dict, name);
    if (cls == NULL) {
        PyErr_Clear();
        return NULL;
    }
    if (!PyType_Check(cls) || !(((PyTypeObject *)cls)->tp_flags & Py_TPFLAGS_HEAPTYPE)) {
        return NULL;
    }
    /* this is the moment the namespace still holds every definition the body wrote */
    By_SettleAnnotations(cls);
    return By_NewRef(cls);
}

/* the type `int | str` is an instance of, which no public header names
 *
 * a `types.UnionType` is one of the two shapes a `class` body's annotation takes that
 * python builds in C, and the only way to reach it from an extension is to build one and
 * ask. the type is immortal, so holding the pointer past the value is sound */
static PyTypeObject *By_UnionType(void) {
    static PyTypeObject *cached = NULL;
    if (cached == NULL) {
        PyObject *probe = PyNumber_Or((PyObject *)&PyLong_Type, (PyObject *)&PyUnicode_Type);
        if (probe == NULL) {
            PyErr_Clear();
            return NULL;
        }
        cached = Py_TYPE(probe);
        Py_DECREF(probe);
    }
    return cached;
}

/* how far into a value the settling below looks
 *
 * it bounds the recursion rather than the reachability: past it every shape is refused
 * rather than assumed safe, so a value nested deeper than this is left where it is */
#define BY_SETTLE_DEPTH 4

/* the type that replaced `value`, where `value` is one of this module's twins and the
 * replacement already stands. the answer is borrowed, because the module holds it
 *
 * a twin whose type has not been built yet answers nothing, and must: the class arrays are
 * filled one class at a time and a constant is copied as each type is made, so a body
 * naming a class further down the module is asking about a replacement that does not exist
 * yet. it is not safe as itself either — it is about to stop being what its name means */
static PyObject *By_TwinFor(PyObject *value, const By_Twins *twins) {
    Py_ssize_t index;
    for (index = 0; index < twins->count; index++) {
        if (value == twins->twins[index] && twins->types[index] != NULL) return twins->types[index];
    }
    return NULL;
}

/* what should stand where `value` does, as a new reference, or NULL where nothing may
 *
 * this is the whole safety of carrying a value the fallback module body produced. a twin
 * is a class that has stopped being the one under its name, so anything still holding one
 * answers about a class nothing else in the process can reach: that is a *silent* wrong
 * answer, where refusing leaves the loud one the value already gave.
 *
 * three outcomes, then. a value that *is* a twin becomes the type that replaced it. a
 * value that provably cannot hold a twin — a number, a string, a class standing on no twin
 * — is handed back exactly as it is. and everything between is **settled**: the shapes
 * whose contents can be reached are walked, every twin found inside is moved onto its
 * replacement, and only a shape with no known route to its contents is refused.
 *
 * settling is the part a bare "does this reach a twin?" predicate could not do, and it is
 * what a class needs to keep what its module body gave it after the `class` statement.
 * `multiprocessing.managers` is the case that found it: sixteen `SyncManager.register(...)`
 * calls follow the class statement, and each installs a closure over the proxy type it
 * registers while recording the same in a `_registry` dict. a predicate answers "may reach
 * a twin" for a closure and for a dict alike, so the emitted `SyncManager` carried none of
 * the sixteen methods and an empty registry — it lost everything the body gave it.
 *
 * where a container can be written it is settled in place, so every holder of it sees the
 * same move; where it cannot — a tuple — a settled copy is built and the original left
 * alone. a caller that wants only the moves throws the answer away, which is what
 * `By_SettleTwins` is for.
 *
 * a container keeps settling its remaining members after one of them has been refused.
 * that matters for exactly that caller: a dict with one unreachable value still has every
 * other entry moved, and only the *answer* records that it cannot be carried */
static PyObject *By_SettledValue(PyObject *value, const By_Twins *twins, int depth);

/* whether settling `value` left it as the very object it was
 *
 * the question a place that cannot be written asks — a set's member, a dict's key, what a
 * property holds. anything it captured is settled where it stands, and a value that would
 * have had to be *replaced* is a refusal rather than a move */
static int By_SettlesInPlace(PyObject *value, const By_Twins *twins, int depth) {
    PyObject *stands = By_SettledValue(value, twins, depth);
    int same = stands == value;
    Py_XDECREF(stands);
    return same;
}

/* settle every value of a dict, in place, and say whether the dict now holds no twin
 *
 * a twin used as a *key* is refused rather than moved: a dict is keyed on the object a
 * class hashes as, so replacing one is a removal and an insertion, and doing that to a
 * mapping somebody else is holding is a bigger claim than this has evidence for */
static int By_SettleDictValues(PyObject *dict, const By_Twins *twins, int depth) {
    /* the keys first: a value is settled one at a time and anything at all may run while
     * that happens, so nothing may be walking the dict itself */
    PyObject *keys = PyDict_Keys(dict);
    Py_ssize_t at;
    int settled = 1;
    if (keys == NULL) {
        PyErr_Clear();
        return 0;
    }
    for (at = 0; at < PyList_GET_SIZE(keys); at++) {
        PyObject *key = PyList_GET_ITEM(keys, at);
        /* held rather than borrowed: settling one entry can run arbitrary code, and the
         * entry this is about to write back over must not have gone away underneath it */
        PyObject *value = By_NewRef(PyDict_GetItem(dict, key));
        PyObject *stands;
        if (value == NULL) continue;
        if (!By_SettlesInPlace(key, twins, depth)) {
            Py_DECREF(value);
            settled = 0;
            continue;
        }
        stands = By_SettledValue(value, twins, depth);
        if (stands == NULL) {
            Py_DECREF(value);
            settled = 0;
            continue;
        }
        if (stands != value && PyDict_SetItem(dict, key, stands) < 0) {
            PyErr_Clear();
            settled = 0;
        }
        Py_DECREF(stands);
        Py_DECREF(value);
    }
    Py_DECREF(keys);
    return settled;
}

/* settle every item of a list, in place, and say whether the list now holds no twin */
static int By_SettleListItems(PyObject *list, const By_Twins *twins, int depth) {
    Py_ssize_t at;
    int settled = 1;
    /* the size is read afresh each time round: settling an item can run arbitrary code,
     * and a list that shrank under us must not be indexed past its end */
    for (at = 0; at < PyList_GET_SIZE(list); at++) {
        PyObject *value = By_NewRef(PyList_GetItem(list, at)); /* held, not borrowed */
        PyObject *stands;
        if (value == NULL) {
            PyErr_Clear();
            settled = 0;
            continue;
        }
        stands = By_SettledValue(value, twins, depth);
        if (stands == NULL) {
            Py_DECREF(value);
            settled = 0;
            continue;
        }
        if (stands != value && at < PyList_GET_SIZE(list)
            && PyList_SetItem(list, at, By_NewRef(stands)) < 0) {
            PyErr_Clear();
            settled = 0;
        }
        Py_DECREF(stands);
        Py_DECREF(value);
    }
    return settled;
}

/* settle what a function captured, in place, and say whether it now holds no twin
 *
 * a `def` evaluates its defaults and closes over its cells where it stands, and everything
 * the fallback module body produced did that before any emitted type was installed. so a
 * default or a captured name holding a class of this module holds the **twin**, while
 * every later read of that name answers the type that replaced it. the two are different
 * objects, and a body comparing them by identity gets the wrong answer:
 *
 *     class Empty: pass
 *     def f(ann=Empty): return ann is Empty     # python True, and this said False
 *
 * that is every sentinel-by-identity api in a compiled module at once — it is why
 * `inspect.Signature()` rendered `() -> _empty`. a cell is the same staleness through the
 * other route a function keeps a value, and it is the route a factory that installs a
 * method uses: `BaseManager.register` closes over the proxy type it was handed and
 * `setattr`s the result, so refusing a closure outright cost `SyncManager` all sixteen of
 * its methods.
 *
 * a definition with nothing to move is left exactly as it was — the defaults tuple is
 * rebuilt only when some entry really is stale, so the common case allocates nothing */
static int By_SettleFunction(PyObject *fn, const By_Twins *twins, int depth) {
    PyObject *closure = PyFunction_GetClosure(fn); /* borrowed, NULL when there is none */
    PyObject *defaults, *kwdefaults;
    Py_ssize_t at;
    int settled = 1;

    if (closure != NULL && PyTuple_Check(closure)) {
        for (at = 0; at < PyTuple_GET_SIZE(closure); at++) {
            PyObject *cell = PyTuple_GET_ITEM(closure, at);
            PyObject *held, *stands;
            if (!PyCell_Check(cell)) {
                settled = 0;
                continue;
            }
            held = PyCell_Get(cell); /* a new reference, and NULL for an unbound cell */
            if (held == NULL) {
                PyErr_Clear();
                continue;
            }
            stands = By_SettledValue(held, twins, depth);
            if (stands == NULL) {
                settled = 0;
            } else {
                if (stands != held && PyCell_Set(cell, stands) < 0) {
                    PyErr_Clear();
                    settled = 0;
                }
                Py_DECREF(stands);
            }
            Py_DECREF(held);
        }
    }

    /* read after the closure and not before: settling a cell can run arbitrary code, and
     * what this is about to write back must be what the function holds now */
    defaults = By_NewRef(PyFunction_GetDefaults(fn));
    if (defaults != NULL && PyTuple_Check(defaults)) {
        PyObject *moved = By_SettledValue(defaults, twins, depth);
        if (moved == NULL) {
            settled = 0;
        } else {
            if (moved != defaults && PyFunction_SetDefaults(fn, moved) < 0) {
                PyErr_Clear();
                settled = 0;
            }
            Py_DECREF(moved);
        }
    }
    Py_XDECREF(defaults);

    kwdefaults = By_NewRef(PyFunction_GetKwDefaults(fn));
    if (kwdefaults != NULL && PyDict_Check(kwdefaults)
        && !By_SettleDictValues(kwdefaults, twins, depth)) {
        settled = 0;
    }
    Py_XDECREF(kwdefaults);
    return settled;
}

/* whether an instance layout names `name` as one of its own fields
 *
 * a key that is not a string is not a field: a mapping's keys are whatever was put in it,
 * and `obj.__dict__[1] = 2` is a shape python allows. saying no is what sends it on to be
 * carried, where the attribute write turns it down for the same reason python would */
static int By_LayoutHolds(const By_Field *layout, PyObject *name) {
    const By_Field *field;
    if (!PyUnicode_Check(name)) return 0;
    for (field = layout; field->name != NULL; field++) {
        if (PyUnicode_CompareWithASCIIString(name, field->name) == 0) return 1;
    }
    return 0;
}

/* the mapping a moved instance is remembered in is keyed on the twin's *address*
 *
 * the twin cannot be the key itself: a dict lookup hashes and then compares, and both of
 * those are arbitrary python on an object the module body wrote — a `__hash__` that raises
 * would turn a lookup into a failure, and an `__eq__` that answers True for a different
 * instance would hand back the wrong replacement. an address is none of those things.
 *
 * what keeps the address honest is the entry itself: the value is `(twin, replacement)`,
 * so the twin outlives the mapping and no second object can be allocated where it stands */
static PyObject *By_MovedFor(PyObject *value, PyObject *moved) {
    PyObject *key = PyLong_FromVoidPtr(value);
    PyObject *pair;
    if (key == NULL) {
        PyErr_Clear();
        return NULL;
    }
    pair = PyDict_GetItem(moved, key);
    Py_DECREF(key);
    if (pair == NULL) {
        PyErr_Clear();
        return NULL;
    }
    return pair;
}

/* record that `value` has been moved onto `stands`, or that it never can be
 *
 * `stands` is `Py_None` for the refusal, and the refusal is recorded rather than left out
 * so that a second reading of the same instance refuses the same way. leaving it out would
 * let the next reading try the move again and succeed, and then two holders of one object
 * would disagree about what it is */
static int By_RememberMove(PyObject *value, PyObject *stands, PyObject *moved) {
    PyObject *key = PyLong_FromVoidPtr(value);
    PyObject *pair;
    int failed;
    if (key == NULL) {
        PyErr_Clear();
        return -1;
    }
    pair = PyTuple_Pack(2, value, stands);
    if (pair == NULL) {
        PyErr_Clear();
        Py_DECREF(key);
        return -1;
    }
    failed = PyDict_SetItem(moved, key, pair) < 0;
    Py_DECREF(pair);
    Py_DECREF(key);
    if (failed) PyErr_Clear();
    return failed ? -1 : 0;
}

/* refuse every move recorded since the mapping held `mark` entries
 *
 * a move registers itself before its fields are filled, which is what lets a cyclic graph
 * resolve to one object: a field that leads back finds the instance rather than starting a
 * second move of it. the cost is that the instance is *reachable* while it is still half
 * written, and the graph member that reached it has already put it in a field of its own.
 * so an instance dropped after that has not gone anywhere — it is standing in somebody
 * else's field with the rest of its layout still zeroed, which reads back as `0` for a
 * tagged integer and raises `SystemError` for a field holding a pointer.
 *
 * nothing tracks which members took it, so every move begun inside the failed one is
 * refused. that is wider than it has to be — a member that never reached back is turned
 * down with the rest — but each of those is refused the way an unmovable instance already
 * is, and the alternative is a half-written object nothing marks.
 *
 * the mapping is only ever added to, and a refusal overwrites an entry in place, so the
 * moves begun inside a fill are exactly the entries from `mark` on */
static void By_UnwindMoves(PyObject *moved, Py_ssize_t mark) {
    PyObject *keys = PyDict_Keys(moved);
    Py_ssize_t at;
    if (keys == NULL) {
        PyErr_Clear();
        return;
    }
    for (at = mark; at < PyList_GET_SIZE(keys); at++) {
        PyObject *key = PyList_GET_ITEM(keys, at);
        PyObject *pair = PyDict_GetItem(moved, key);
        PyObject *refused;
        if (pair == NULL || !PyTuple_Check(pair) || PyTuple_GET_SIZE(pair) != 2) {
            PyErr_Clear();
            continue;
        }
        if (PyTuple_GET_ITEM(pair, 1) == Py_None) continue;
        refused = PyTuple_Pack(2, PyTuple_GET_ITEM(pair, 0), Py_None);
        if (refused == NULL) {
            PyErr_Clear();
            continue;
        }
        if (PyDict_SetItem(moved, key, refused) < 0) PyErr_Clear();
        Py_DECREF(refused);
    }
    Py_DECREF(keys);
}

/* an instance the module body built, standing on the type that replaced its class
 *
 * this is the value shape `By_SettledValue` could not answer for, and it is the one the
 * module body produces most: `logging` writes `Logger.root = root` and
 * `Logger.manager = Manager(Logger.root)`, `_pydatetime` writes `timedelta.min`,
 * `timedelta.max`, `timezone.utc`. every one of those is an object built by the
 * *interpreted* class, so its type is the twin — a class nothing else in the process can
 * reach — and leaving it where it stands makes `isinstance(Logger.root, Logger)` False and
 * hands a compiled method an object it refuses outright.
 *
 * it cannot be re-`__class__`'d onto the emitted type, because the twin built it with the
 * twin's layout, and it cannot be built again, because that means running a constructor
 * whose side effects have already happened once. so its *state* is moved instead: an
 * instance of the emitted type is allocated without any constructor running, and each
 * attribute the layout keeps is read off the twin and written through the type's own
 * setter, which is the same conversion an assignment from python goes through.
 *
 * two things make that sound rather than merely plausible.
 *
 * the first is that the move is decided before it is begun. a field the layout treats as
 * always defined has no presence byte and no check at any read, so leaving one unwritten
 * does not raise — an unwritten tagged integer reads back as `0`. so every such field must
 * be found on the twin *first*, and an instance carrying state the emitted type has nowhere
 * to put is refused outright rather than moved with the remainder dropped.
 *
 * nowhere is narrower than "outside the layout". an emitted class keeps a dict beside its
 * layout wherever the source did not declare `__slots__` throughout, and a name written on
 * the twin from outside `__init__` goes into it — the same place `o.brand_new = 7` on a
 * freshly built emitted instance goes, and `__dict__` answers over both halves. so such a
 * name is carried across with the fields, and only a class whose instances have no dict at
 * all still refuses.
 *
 * the second is that the move is remembered. `Manager(Logger.root)` captures the very
 * object `Logger.root` holds, so a move that produced a fresh instance per reading would
 * leave the module with two roots and `Logger.manager.root is Logger.root` False — a new
 * silent wrong answer in place of the old loud one. the mapping is written *before* the
 * fields are filled, so a graph that leads back to this instance finds it rather than
 * starting a second move of it.
 *
 * what is left where it stands is a field whose own value cannot be settled — the same
 * rule `By_ConstantValue` applies to a class-level constant, and for the same reason: the
 * alternative is dropping an attribute that was right about everything else.
 *
 * `layouts[i]` is NULL for a class this cannot be done for at all, and the emitting side
 * decides that: a class whose instance is not wholly its own field run — one standing on a
 * base python allocates — keeps state in a place nothing here can read */
static PyObject *By_MovedInstance(PyObject *value, const By_Twins *twins, int depth) {
    PyObject *type = NULL;
    const By_Field *layout = NULL;
    const By_Field *field;
    PyObject *already, *dict, *extras = NULL, *fresh;
    PyTypeObject *target;
    Py_ssize_t index, mark;

    if (twins->moved == NULL) return NULL;
    for (index = 0; index < twins->count; index++) {
        if ((PyObject *)Py_TYPE(value) != twins->twins[index]) continue;
        type = twins->types[index];
        layout = twins->layouts[index];
        break;
    }
    /* a type not built yet is not a refusal to remember: the class arrays are filled one
     * class at a time, so this is "not yet" rather than "never". it is unreachable from a
     * class body, which can only name a class already defined above it */
    if (type == NULL || layout == NULL || !PyType_Check(type)) return NULL;

    already = By_MovedFor(value, twins->moved);
    if (already != NULL) {
        PyObject *stands = PyTuple_GET_ITEM(already, 1);
        /* a reading is a lookup and costs nothing, so it is answered at any depth — the
         * bound is on starting a *new* move, which walks the instance's own fields */
        return stands == Py_None ? NULL : By_NewRef(stands);
    }
    if (depth <= 0) return NULL;
    target = (PyTypeObject *)type;

    /* state the layout has no name for, and where it can go.
     *
     * a class that keeps `__slots__` throughout has no mapping at all and there is nothing
     * to check. one that has a mapping is usually carrying only its own fields — but a
     * name the body set from outside `__init__` is in there too, and where it goes decides
     * whether this instance can move.
     *
     * an emitted class that keeps a dict of its own has somewhere to put it: the emitted
     * instance keeps its fields in the layout and everything else in that dict, exactly as
     * `o.brand_new = 7` on a freshly built one does, and `__dict__` answers over both
     * halves. so those names are carried across with the fields, below. a class whose
     * instances have no dict — a chain declaring `__slots__` throughout — has nowhere, and
     * the name would simply disappear, so the instance is refused instead.
     *
     * `tp_dictoffset` is the question, not the class's own source: it is what the emitting
     * side wrote out for exactly this, and it is what `PyObject_GenericSetAttr` consults */
    dict = PyObject_GetAttrString(value, "__dict__");
    if (dict == NULL) {
        PyErr_Clear();
    } else {
        PyObject *names = PyDict_CheckExact(dict) ? PyDict_Keys(dict) : NULL;
        int roomy = names != NULL;
        Py_ssize_t at;
        for (at = 0; roomy && at < PyList_GET_SIZE(names); at++) {
            PyObject *name = PyList_GET_ITEM(names, at);
            if (By_LayoutHolds(layout, name)) continue;
            if (target->tp_dictoffset == 0) {
                roomy = 0;
                break;
            }
            if (extras == NULL && (extras = PyList_New(0)) == NULL) {
                roomy = 0;
                break;
            }
            if (PyList_Append(extras, name) < 0) {
                roomy = 0;
                break;
            }
        }
        Py_XDECREF(names);
        if (!roomy) {
            PyErr_Clear();
            Py_XDECREF(extras);
            Py_DECREF(dict);
            By_RememberMove(value, Py_None, twins->moved);
            return NULL;
        }
        if (extras == NULL) Py_CLEAR(dict);
    }

    /* a field the twin never wrote, which only a layout with a presence byte for it can
     * reproduce. read through the attribute rather than out of the mapping, so a class
     * keeping its state in `__slots__` is reached the same way — and so that a name the
     * body left to a class-level default is carried as the default, which is what the
     * emitted instance's own read would have answered */
    for (field = layout; field->name != NULL; field++) {
        PyObject *probe = PyObject_GetAttrString(value, field->name);
        if (probe != NULL) {
            Py_DECREF(probe);
            continue;
        }
        PyErr_Clear();
        if (field->optional) continue;
        Py_XDECREF(extras);
        Py_XDECREF(dict);
        By_RememberMove(value, Py_None, twins->moved);
        return NULL;
    }

    if (target->tp_alloc == NULL) {
        Py_XDECREF(extras);
        Py_XDECREF(dict);
        return NULL;
    }
    /* allocated rather than constructed: `tp_new` may be a written `__new__` and `tp_init`
     * is the constructor whose side effects have already happened once. what `tp_alloc`
     * leaves is a zeroed layout, which is exactly the state a field nothing writes is in */
    fresh = target->tp_alloc(target, 0);
    if (fresh == NULL) {
        PyErr_Clear();
        Py_XDECREF(extras);
        Py_XDECREF(dict);
        return NULL;
    }
    if (By_RememberMove(value, fresh, twins->moved) < 0) {
        Py_XDECREF(extras);
        Py_XDECREF(dict);
        Py_DECREF(fresh);
        return NULL;
    }
    /* taken after this instance's own entry, so a failure below refuses the moves begun
     * inside the fill and leaves this one to the refusal that follows it */
    mark = PyDict_GET_SIZE(twins->moved);

    for (field = layout; field->name != NULL; field++) {
        PyObject *stands;
        PyObject *held = PyObject_GetAttrString(value, field->name);
        int failed;
        if (held == NULL) {
            /* absent, and the pass above established that only an optional field can be */
            PyErr_Clear();
            continue;
        }
        stands = By_SettledValue(held, twins, depth - 1);
        /* the setter is the type's own, so the value goes through the same conversion an
         * assignment from python does — and a value that could not be settled keeps what
         * the twin gave it rather than costing the instance the whole attribute */
        failed = PyObject_SetAttrString(fresh, field->name,
                                        stands != NULL ? stands : held)
                 < 0;
        Py_XDECREF(stands);
        Py_DECREF(held);
        if (failed) {
            /* the layout disagrees with what the class actually holds, which is a wrong
             * inference rather than a value this may keep. the refusal is recorded so that
             * every later reading refuses too, and the half-written instance is dropped —
             * along with every move begun while filling it, which is what keeps this one
             * from being left standing in a cyclic partner's field */
            PyErr_Clear();
            Py_XDECREF(extras);
            Py_XDECREF(dict);
            By_RememberMove(value, Py_None, twins->moved);
            By_UnwindMoves(twins->moved, mark);
            Py_DECREF(fresh);
            return NULL;
        }
    }

    /* and the names the layout has none of, into the dict the emitted instance keeps
     * beside it. they go on after the fields, which is the order the object acquired them
     * in and the order `__dict__` reports them in, and each goes through the attribute so
     * that it lands where an assignment from python would have put it.
     *
     * settled like a field, because an extra can hold a twin as readily as a field can —
     * `obj.owner = SomeClass` is the shape, and leaving it would name a class nothing else
     * in the module can reach */
    for (index = 0; extras != NULL && index < PyList_GET_SIZE(extras); index++) {
        PyObject *name = PyList_GET_ITEM(extras, index);
        PyObject *held = PyDict_GetItem(dict, name); /* borrowed */
        PyObject *stands;
        int failed;
        /* the body ran to completion before any of this, but settling a value runs code,
         * and a name that has gone since the keys were taken is simply not carried */
        if (held == NULL) continue;
        Py_INCREF(held);
        stands = By_SettledValue(held, twins, depth - 1);
        failed = PyObject_SetAttr(fresh, name, stands != NULL ? stands : held) < 0;
        Py_XDECREF(stands);
        Py_DECREF(held);
        if (failed) {
            PyErr_Clear();
            Py_DECREF(extras);
            Py_DECREF(dict);
            By_RememberMove(value, Py_None, twins->moved);
            By_UnwindMoves(twins->moved, mark);
            Py_DECREF(fresh);
            return NULL;
        }
    }
    Py_XDECREF(extras);
    Py_XDECREF(dict);
    return fresh;
}

static PyObject *By_SettledValue(PyObject *value, const By_Twins *twins, int depth) {
    PyObject *replacement;
    Py_ssize_t index;
    if (value == NULL) return NULL;
    replacement = By_TwinFor(value, twins);
    if (replacement != NULL) return By_NewRef(replacement);
    for (index = 0; index < twins->count; index++) {
        if (value == twins->twins[index]) return NULL;
    }
    /* the atoms are answered before the bound, not after it. `depth` is there to stop the
     * recursion, and a value with nothing inside it is not a step into anything — reading
     * it costs the same at any depth, and refusing it would refuse whatever holds it. a
     * `True` sitting five levels down as a keyword default is how that was found: it took
     * the whole `_registry` dict of `multiprocessing.managers` with it */
    if (value == Py_None || PyBool_Check(value) || PyLong_Check(value)
        || PyFloat_Check(value) || PyComplex_Check(value) || PyUnicode_Check(value)
        || PyBytes_Check(value)) {
        return By_NewRef(value);
    }
    /* an instance one of this module's twins built, which has its own bound to keep and
     * answers a reading of an instance already moved at any depth — see `By_MovedInstance` */
    if (twins->moved != NULL) {
        PyObject *moved = By_MovedInstance(value, twins, depth);
        if (moved != NULL) return moved;
    }
    if (depth <= 0) return NULL;
    /* a class is safe as itself: every class this module's body wrote with a `class`
     * statement is among the twins, so one that is not is a class both the interpreted
     * module and this one hold the same object for. what it must not do is *stand* on a
     * twin — a class built at runtime over one has a base nothing else can reach, and its
     * bases are not something this can rewrite */
    if (PyType_Check(value)) {
        PyObject *mro = ((PyTypeObject *)value)->tp_mro;
        Py_ssize_t at;
        if (mro == NULL || !PyTuple_Check(mro)) return NULL;
        for (at = 0; at < PyTuple_GET_SIZE(mro); at++) {
            for (index = 0; index < twins->count; index++) {
                if (PyTuple_GET_ITEM(mro, at) == twins->twins[index]) return NULL;
            }
        }
        return By_NewRef(value);
    }
    /* the two parameterised forms python builds in C — `list[int]` and `int | None`. each
     * is an origin and a tuple of arguments and nothing besides, both read off a member
     * rather than through anything that runs. neither can be written, so one reaching a
     * twin is refused rather than settled. `typing.Optional[int]` is a python object whose
     * attribute access is python code, and is not among these */
    if (Py_TYPE(value) == &Py_GenericAliasType || Py_TYPE(value) == By_UnionType()) {
        static const char *const parts[] = {"__origin__", "__args__"};
        int settled = 1;
        for (index = 0; index < 2; index++) {
            PyObject *part = PyObject_GetAttrString(value, parts[index]);
            if (part == NULL) {
                PyErr_Clear();
                continue;
            }
            if (!By_SettlesInPlace(part, twins, depth - 1)) settled = 0;
            Py_DECREF(part);
        }
        return settled ? By_NewRef(value) : NULL;
    }
    /* a tuple cannot be written, so a settled copy is built and the original left for
     * whoever else holds it. the copy is made only when something really moved */
    if (PyTuple_Check(value)) {
        Py_ssize_t size = PyTuple_GET_SIZE(value);
        PyObject *moved = NULL;
        int settled = 1;
        for (index = 0; index < size; index++) {
            PyObject *item = PyTuple_GET_ITEM(value, index);
            PyObject *stands = By_SettledValue(item, twins, depth - 1);
            if (stands == NULL) {
                settled = 0;
                continue;
            }
            if (stands != item) {
                if (moved == NULL) {
                    Py_ssize_t at;
                    moved = PyTuple_New(size);
                    if (moved == NULL) {
                        PyErr_Clear();
                        Py_DECREF(stands);
                        return NULL;
                    }
                    for (at = 0; at < size; at++) {
                        PyTuple_SET_ITEM(moved, at, By_NewRef(PyTuple_GET_ITEM(value, at)));
                    }
                }
                Py_DECREF(PyTuple_GET_ITEM(moved, index));
                PyTuple_SET_ITEM(moved, index, By_NewRef(stands));
            }
            Py_DECREF(stands);
        }
        if (!settled) {
            Py_XDECREF(moved);
            return NULL;
        }
        return moved != NULL ? moved : By_NewRef(value);
    }
    if (PyList_Check(value)) {
        return By_SettleListItems(value, twins, depth - 1) ? By_NewRef(value)
                                                                        : NULL;
    }
    if (PyDict_Check(value)) {
        return By_SettleDictValues(value, twins, depth - 1) ? By_NewRef(value)
                                                                         : NULL;
    }
    /* a set's members are what it is hashed on, so one holding a twin is refused rather
     * than settled — the same reason a dict's keys are */
    if (PyAnySet_Check(value)) {
        PyObject *members = PySequence_List(value);
        Py_ssize_t at;
        int settled = 1;
        if (members == NULL) {
            PyErr_Clear();
            return NULL;
        }
        for (at = 0; settled && at < PyList_GET_SIZE(members); at++) {
            if (!By_SettlesInPlace(PyList_GET_ITEM(members, at), twins, depth - 1)) {
                settled = 0;
            }
        }
        Py_DECREF(members);
        return settled ? By_NewRef(value) : NULL;
    }
    if (PyFunction_Check(value)) {
        return By_SettleFunction(value, twins, depth - 1) ? By_NewRef(value)
                                                                       : NULL;
    }
    /* a compiled method is settled as what its calls go to */
    if (by_method_type != NULL && Py_IS_TYPE(value, by_method_type)) {
        return By_SettlesInPlace(((ByMethodObject *)value)->fn, twins, depth - 1)
                   ? By_NewRef(value)
                   : NULL;
    }
    /* a function written in C. the only thing it can hand back that python chose is
     * `__self__` — the module it was defined in, or the object a method of a built-in type
     * is bound to. its body resolves no names through a closure and none through a
     * namespace this module owns, so there is no other route in. a module receiver is safe
     * outright, including this module's own: by the time anything is settled its namespace
     * already holds the compiled types.
     *
     * `threading.RLock` is why this is here — `multiprocessing.managers` registers it, and
     * refusing a built-in cost the whole `_registry` dict it sits in */
    if (PyCFunction_Check(value)) {
        PyObject *receiver = PyCFunction_GetSelf(value);
        if (receiver == NULL) {
            PyErr_Clear();
            return By_NewRef(value);
        }
        if (PyModule_Check(receiver)) return By_NewRef(value);
        return By_SettlesInPlace(receiver, twins, depth - 1) ? By_NewRef(value)
                                                                          : NULL;
    }
    /* a descriptor read off a type, which is every method an emitted type publishes and
     * every slot a built-in one does. it holds nothing but the type it was read from, and
     * cannot be written — so one whose owner is a twin is refused, exactly as a bound
     * method already bound to one is, and every other is safe as itself.
     *
     * `pprint` is why this is here: `_dispatch[dict.__repr__] = _pprint_dict` keys the
     * table on a slot wrapper, and refusing the *key* left all 18 values where they were */
    if (By_MethodDescriptorOf(value) != NULL || Py_TYPE(value) == &PyWrapperDescr_Type
        || Py_TYPE(value) == &PyClassMethodDescr_Type
        || Py_TYPE(value) == &PyGetSetDescr_Type || Py_TYPE(value) == &PyMemberDescr_Type) {
        PyObject *owner = PyObject_GetAttrString(value, "__objclass__");
        int settled;
        if (owner == NULL) {
            PyErr_Clear();
            return NULL;
        }
        settled = By_SettlesInPlace(owner, twins, depth - 1);
        Py_DECREF(owner);
        return settled ? By_NewRef(value) : NULL;
    }
    /* a bound method is what a class hands back for the `classmethod` in its dict, so this
     * is the shape a declined class's methods are read as. what it binds is settled where
     * it stands; what it is bound *to* cannot be rewritten, so a method already bound to a
     * twin is refused */
    if (PyMethod_Check(value)) {
        PyObject *function = PyMethod_Function(value); /* borrowed */
        PyObject *receiver = PyMethod_Self(value);     /* borrowed */
        int settled = function != NULL
                      && By_SettlesInPlace(function, twins, depth - 1);
        if (receiver != NULL && !By_SettlesInPlace(receiver, twins, depth - 1)) {
            settled = 0;
        }
        return settled ? By_NewRef(value) : NULL;
    }
    /* a property and the two method wrappers hold functions they will not let go of, so
     * what they hold is settled where it stands and never replaced */
    if (Py_TYPE(value) == &PyProperty_Type || Py_TYPE(value) == &PyStaticMethod_Type
        || Py_TYPE(value) == &PyClassMethod_Type) {
        static const char *const parts[] = {"fget", "fset", "fdel", "__func__"};
        int settled = 1;
        for (index = 0; index < 4; index++) {
            PyObject *part = PyObject_GetAttrString(value, parts[index]);
            if (part == NULL) {
                PyErr_Clear();
                continue;
            }
            if (!By_SettlesInPlace(part, twins, depth - 1)) settled = 0;
            Py_DECREF(part);
        }
        return settled ? By_NewRef(value) : NULL;
    }
    return NULL;
}

/* settle whatever `value` reaches and discard the answer
 *
 * `By_RemapTwinAliases` walks the module namespace for the names still bound to a twin,
 * and everything it passes is settled on the way — a function's defaults and closure, a
 * dict the body built, a declined class's methods. whether the value would be *carriable*
 * is not the question there, because it stays where it is either way; only the moves
 * matter, so a settled copy of something that could not be written is thrown away rather
 * than put in the original's place */
static inline void By_SettleTwins(PyObject *value, const By_Twins *twins) {
    PyObject *settled;
    if (value == NULL) return;
    /* a replaced *definition* is answered by its replacement rather than walked into, so
     * asking for a settled value would never reach what it holds itself. it still has to
     * be reached: the forwarder standing in for it keeps it under `__wrapped__`, which is
     * where `inspect.signature` reads a default from — and a default naming a class of
     * this module is the twin the forwarder no longer is */
    if (PyFunction_Check(value) && By_TwinFor(value, twins) != NULL) {
        By_SettleFunction(value, twins, BY_SETTLE_DEPTH);
        return;
    }
    settled = By_SettledValue(value, twins, BY_SETTLE_DEPTH);
    Py_XDECREF(settled);
}

/* whether a name is one python spells with two underscores at each end */
static inline int By_IsDunder(PyObject *name) {
    Py_ssize_t size = PyUnicode_GET_LENGTH(name);
    return size > 4 && PyUnicode_READ_CHAR(name, 0) == '_'
           && PyUnicode_READ_CHAR(name, 1) == '_'
           && PyUnicode_READ_CHAR(name, size - 1) == '_'
           && PyUnicode_READ_CHAR(name, size - 2) == '_';
}

/* what should stand for `value` on an emitted type, or NULL where nothing may
 *
 * a value that *is* a twin becomes the type replacing it, which is what makes a carried
 * attribute agree with the namespace, and everything else goes through `By_SettledValue`.
 * the answer is a new reference: settling can have to *build* the value that stands, and a
 * borrowed answer would have nobody holding that one */
static PyObject *By_TwinReplacement(PyObject *value, const By_Twins *twins) {
    return By_SettledValue(value, twins, BY_SETTLE_DEPTH);
}

/* the annotations a class body wrote, carried onto the type that takes its place
 *
 * python keeps a class's own annotations in its `tp_dict` — under `BY_ANNOTATIONS`, and
 * see there for which key that is — and reads them back through a getset on the
 * metatype: one that looks at that dict and no base's, so a class with
 * none of its own answers an empty mapping rather than inheriting one. carrying the
 * twin's entry reproduces both halves at once, and it is the only place the values can
 * come from: a `from __future__ import annotations` module wrote strings and every other
 * module wrote whatever the body's expressions evaluated to, and neither can be worked
 * out again from here.
 *
 * `__annotations__` is the one dunder carried, and it is carried because the objection to
 * the rest does not reach it: it fills no type slot, so there is no second answer for it
 * to disagree with. the getset *is* the only way to read it, on a compiled class and an
 * interpreted one alike.
 *
 * the values are subject to the same rule every carried attribute is — see
 * `By_TwinReplacement` — and where one of them fails it, the whole mapping is replaced by
 * the refusal rather than by a mapping missing an entry */
static inline int By_CarryAnnotations(PyObject *source, PyObject *target,
                                      const By_Twins *twins) {
    PyObject *written = PyDict_GetItemString(source, BY_ANNOTATIONS);
    PyObject *carried;
    int failed;
    if (written == NULL) {
        PyErr_Clear();
        /* a body that wrote no annotation still answers an empty mapping */
        carried = PyDict_New();
    } else if (PyDict_CheckExact(written)) {
        /* the keys first, because a value is read back out one at a time and nothing
         * may run while a dict is being walked */
        PyObject *names = PyDict_Keys(written);
        Py_ssize_t at;
        if (names == NULL) return -1;
        carried = PyDict_New();
        for (at = 0; carried != NULL && at < PyList_GET_SIZE(names); at++) {
            PyObject *key = PyList_GET_ITEM(names, at);
            PyObject *value = PyDict_GetItem(written, key);
            PyObject *stands = value == NULL
                                   ? NULL
                                   : By_TwinReplacement(value, twins);
            int failed_here;
            if (stands == NULL) {
                Py_DECREF(carried);
                carried = By_LostAnnotations();
                break;
            }
            failed_here = PyDict_SetItem(carried, key, stands) < 0;
            Py_DECREF(stands);
            if (failed_here) {
                Py_DECREF(carried);
                Py_DECREF(names);
                return -1;
            }
        }
        Py_DECREF(names);
    } else {
        /* a body that assigned `__annotations__` itself, which python leaves alone */
        PyObject *stands = By_TwinReplacement(written, twins);
        carried = stands == NULL ? By_LostAnnotations() : stands;
    }
    if (carried == NULL) return -1;
    failed = PyDict_SetItemString(target, BY_ANNOTATIONS, carried) < 0;
    Py_DECREF(carried);
    return failed ? -1 : 0;
}

/* the abstract-base registry a module body built, carried onto the type that takes the
 * class's place
 *
 * `X.register(Y)` records `Y` inside `X`'s own `_abc_impl`, and the module body ran
 * against the interpreted definition — so every registration a body made landed on the
 * twin, while `ABCMeta.__new__` gave the type replacing it a fresh and empty one.
 * `_collections_abc` is the whole shape of it: `MutableMapping.register(dict)` and
 * fifteen more, after which `issubclass(dict, MutableMapping)` answered False off the
 * compiled module and True off the interpreted one, with nothing to announce it.
 *
 * the twin's object is handed over rather than its contents copied. `_abc_data` is opaque
 * from here — there is no way to read a registration back out of one, and no way to put
 * one in but `register`, which would need the very classes that cannot be read. the twin
 * is on its way out and what it holds is exactly the registry the class was given, so
 * moving it is what makes the two agree.
 *
 * both sides have to hold one: a target without `_abc_impl` is not a class `ABCMeta`
 * built, and a source without one has no registry to hand over */
static inline int By_AdoptAbcRegistry(PyObject *source, PyObject *target) {
    PyObject *held = PyDict_GetItemString(source, "_abc_impl");
    if (held == NULL || PyDict_GetItemString(target, "_abc_impl") == NULL) return 0;
    return PyDict_SetItemString(target, "_abc_impl", held);
}

/* `value` as a class holds it, or the callable inside the `classmethod` or `staticmethod`
 * it is, as a new reference */
static PyObject *By_UnwrappedMethod(PyObject *value) {
    if (Py_IS_TYPE(value, &PyClassMethod_Type) || Py_IS_TYPE(value, &PyStaticMethod_Type)) {
        return PyObject_GetAttrString(value, "__func__");
    }
    return By_NewRef(value);
}

/* hand each compiled method in `target`, a type's dict, the function the twin class holds
 * under its name in `source`, which is where the method's annotations are — see
 * `By_Method_getattro`
 *
 * a `classmethod` or `staticmethod` copies its callable's `__name__`, `__qualname__`,
 * `__module__`, `__doc__` (and on 3.13 its `__annotations__`) when it is made, which was
 * before the method had a definition to answer them from. so each one over a method given
 * its definition here is made again, in place, the way python makes it. that is left until
 * the dict has been walked, because making one reads attributes */
static int By_AdoptMethodTwins(PyObject *source, PyObject *target) {
    PyObject *key, *value, *remade;
    Py_ssize_t position = 0, at;
    int failed = 0;
    if (by_method_type == NULL) return 0;
    remade = PyList_New(0);
    if (remade == NULL) return -1;
    while (!failed && PyDict_Next(target, &position, &key, &value)) {
        PyObject *method = By_UnwrappedMethod(value);
        if (method == NULL) {
            failed = 1;
            break;
        }
        if (Py_IS_TYPE(method, by_method_type)) {
            PyObject *twin = PyDict_GetItemWithError(source, key); /* borrowed */
            PyObject *function = twin == NULL ? NULL : By_UnwrappedMethod(twin);
            if (function == NULL && PyErr_Occurred()) {
                failed = 1;
            } else if (function != NULL && PyFunction_Check(function)) {
                Py_XSETREF(((ByMethodObject *)method)->twin, function);
                if (value != method && PyList_Append(remade, value) < 0) failed = 1;
            } else {
                Py_XDECREF(function);
            }
        }
        Py_DECREF(method);
    }
    for (at = 0; !failed && at < PyList_GET_SIZE(remade); at++) {
        PyObject *wrapper = PyList_GET_ITEM(remade, at); /* borrowed */
        PyObject *method = PyObject_GetAttrString(wrapper, "__func__");
        PyObject *arguments = method == NULL ? NULL : PyTuple_Pack(1, method);
        failed = arguments == NULL || Py_TYPE(wrapper)->tp_init(wrapper, arguments, NULL) < 0;
        Py_XDECREF(arguments);
        Py_XDECREF(method);
    }
    Py_DECREF(remade);
    return failed ? -1 : 0;
}

/* what the module body gave a class *after* its `class` statement, carried onto the
 * type that takes its place
 *
 * the interpreted definition runs first and the whole module body runs against it, so
 * everything the body sets on a class after the statement lands on that definition —
 * the twin — and the compiled type replacing it in the namespace never sees any of it.
 * `urllib.parse` sets `_encoded_counterpart` from a helper, `xml.dom.minidom` installs
 * five properties from one, and `turtle` sixty forwarded methods.
 *
 * a plain copy is not available, and was tried: the value `ParseResult` is given is
 * `ParseResultBytes` — a *twin* — so copying it makes the class answer with an object
 * `isinstance` says is not the `ParseResultBytes` under that name. so a value that is
 * itself a twin is replaced by the type standing in for it, which is what makes the
 * carried attribute agree with the namespace, and every other value goes through
 * `By_SettledValue` — carried where every twin in it could be moved, and refused where one
 * could not. that is what lets a class keep the methods a factory installed on it after
 * the `class` statement: `multiprocessing.managers` installs sixteen closures on
 * `SyncManager` that way, and a rule that only tested for a twin refused all of them.
 *
 * a dunder is never carried. a name written into `tp_dict` does not fill a type slot,
 * so an adopted `__ge__` would answer `a.__ge__(b)` while `a >= b` still went to the
 * slot — two answers where the interpreted class has one. `__annotations__` is the one
 * exception, and `By_CarryAnnotations` says why: it fills no slot, so there is nothing
 * for it to disagree with */
static inline int By_AdoptTwinAttributes(const By_Twins *twins) {
    Py_ssize_t index;
    for (index = 0; index < twins->count; index++) {
        PyObject *twin = twins->twins[index];
        PyObject *type = twins->types[index];
        PyObject *source, *target, *names;
        Py_ssize_t at;
        if (twin == NULL || type == NULL || twin == type) continue;
        if (!PyType_Check(twin) || !PyType_Check(type)) continue;
        source = ((PyTypeObject *)twin)->tp_dict;
        target = ((PyTypeObject *)type)->tp_dict;
        if (source == NULL || target == NULL) continue;
        if (By_CarryAnnotations(source, target, twins) < 0) return -1;
        if (By_AdoptMethodTwins(source, target) < 0) return -1;
        /* ahead of the walk below, which leaves a name the target already holds alone —
         * and the target holds an `_abc_impl` of its own, the empty one it was built with */
        if (By_AdoptAbcRegistry(source, target) < 0) return -1;
        /* the keys are taken as a list first: the values are read back out of the
         * source one at a time, and nothing here may run while a dict is being walked */
        names = PyDict_Keys(source);
        if (names == NULL) return -1;
        for (at = 0; at < PyList_GET_SIZE(names); at++) {
            PyObject *key = PyList_GET_ITEM(names, at);
            PyObject *value, *carried;
            int present, failed;
            if (!PyUnicode_Check(key) || By_IsDunder(key)) continue;
            present = PyDict_Contains(target, key);
            if (present != 0) {
                if (present < 0) {
                    Py_DECREF(names);
                    return -1;
                }
                continue;
            }
            value = PyDict_GetItem(source, key);
            if (value == NULL) continue;
            carried = By_TwinReplacement(value, twins);
            if (carried == NULL) continue;
            failed = PyDict_SetItem(target, key, carried) < 0;
            Py_DECREF(carried);
            if (failed) {
                Py_DECREF(names);
                return -1;
            }
        }
        Py_DECREF(names);
        /* the attribute cache would otherwise go on serving what the type had before */
        PyType_Modified((PyTypeObject *)type);
    }
    return 0;
}

/* what the module body wrote over a member *after* the `class` statement, carried onto the
 * type that took the class's place
 *
 * `By_AdoptTwinAttributes` leaves a name the type already holds alone, and a type holds
 * every name its own body lowered — so a later `C.v = property(...)`, `C.width = f` or
 * `del C.extra` landed on the twin and the emitted type went on answering with what the
 * `class` statement wrote. `body` is the class's dict as `type.__new__` left it, before
 * anything after the statement could reach it (see `By_CaptureClassBody`), which is what
 * tells a rewrite from the definition itself: a name whose value on the twin is no longer
 * the object the body put there was rewritten, and the type takes the twin's answer or
 * loses the name with it.
 *
 * the licences a class hands out are armed after this and ask the type's dict, so a
 * rewritten member refuses the licence it would have held and every compiled read of it
 * takes the full lookup. a decorated class is left out: a decorator the source wrote is
 * applied to the type again, and what it rewrote on the twin is what it rewrites there,
 * while one the transpiler wrote — `@dataclass(slots=True)` — hands back another class,
 * which `statement` tells apart. a dunder is left out for the reason
 * `By_AdoptTwinAttributes` gives */
static inline int By_CarryRewrittenMembers(PyObject *body, PyObject *statement,
                                           const By_Twins *twins, Py_ssize_t index) {
    PyObject *twin = twins->twins[index];
    PyObject *type = twins->types[index];
    PyObject *source, *target, *names;
    Py_ssize_t at;
    int rewrote = 0;
    /* a twin that is not the class its statement built was replaced by a decorator, and
     * everything it holds differs from the body for that reason alone */
    if (body == NULL || twin == NULL || twin != statement || type == NULL || twin == type) {
        return 0;
    }
    if (!PyDict_Check(body) || !PyType_Check(twin) || !PyType_Check(type)) return 0;
    source = ((PyTypeObject *)twin)->tp_dict;
    target = ((PyTypeObject *)type)->tp_dict;
    if (source == NULL || target == NULL) return 0;
    /* the keys first: a value is read back out one at a time, and carrying one can run
     * python while a dict would otherwise still be walked */
    names = PyDict_Keys(body);
    if (names == NULL) return -1;
    for (at = 0; at < PyList_GET_SIZE(names); at++) {
        PyObject *key = PyList_GET_ITEM(names, at);
        PyObject *written, *now, *held, *carried;
        int failed;
        if (!PyUnicode_Check(key) || By_IsDunder(key)) continue;
        written = PyDict_GetItem(body, key);
        now = PyDict_GetItem(source, key);
        if (now == written) continue;
        held = PyDict_GetItem(target, key);
        if (held == NULL || held == now) continue;
        rewrote = 1;
        if (now == NULL) {
            if (PyDict_DelItem(target, key) < 0) {
                Py_DECREF(names);
                return -1;
            }
            continue;
        }
        /* a value that refuses to settle is still the one the body chose, and the member
         * the class statement wrote is the one answer known to be wrong */
        carried = By_TwinReplacement(now, twins);
        if (carried == NULL) {
            if (PyErr_Occurred()) {
                Py_DECREF(names);
                return -1;
            }
            carried = Py_NewRef(now);
        }
        failed = PyDict_SetItem(target, key, carried) < 0;
        Py_DECREF(carried);
        if (failed) {
            Py_DECREF(names);
            return -1;
        }
    }
    Py_DECREF(names);
    if (rewrote) PyType_Modified((PyTypeObject *)type);
    return 0;
}

/* whether `entry` under `name` is what `By_CarryRewrittenMembers` carried: the twin's own
 * answer, NULL where the module body deleted the name, and not the one the body wrote */
static inline int By_RewrittenMember(PyObject *body, PyObject *statement, PyObject *twin,
                                     const char *name, PyObject *entry) {
    PyObject *written, *now;
    if (body == NULL || !PyDict_Check(body) || twin == NULL || twin != statement) return 0;
    if (!PyType_Check(twin) || ((PyTypeObject *)twin)->tp_dict == NULL) return 0;
    written = PyDict_GetItemString(body, name);
    if (written == NULL) {
        PyErr_Clear();
        return 0;
    }
    now = PyDict_GetItemString(((PyTypeObject *)twin)->tp_dict, name);
    if (now == NULL) PyErr_Clear();
    return entry == now && entry != written;
}

/* ── the members `@dataclass` generates ───────────────────────────────────────
 *
 * a `data class` is `@dataclass(slots=True)` on the twin, so python's own decorator gives
 * the twin's class a `__repr__`, an `__eq__`, a `__hash__`, `__match_args__` and the
 * bookkeeping `dataclasses` reads back — and a frozen one a `__setattr__` and
 * `__delattr__` besides. none of that crosses in `By_AdoptTwinAttributes`, so the emitted
 * type has to have it of its own.
 *
 * the four below are what the emitted type's slots point at. each is handed the class's
 * fields as a table of `(name, getter)` pairs, the getter being the one the getset
 * publishes — so a field is read through exactly the descriptor python reads it through */

/* one field of an emitted data class, in the order `@dataclass` lists it */
typedef struct {
    const char *name;
    getter get;
} By_DataField;

/* `f'{qualname}({name}={value!r}, …)'`, which is what `@dataclass` writes
 *
 * `Py_ReprEnter` stands for the `reprlib.recursive_repr` the generated `__repr__` is
 * wrapped in: a dataclass holding itself prints `C(kid=...)` rather than recurring until
 * the stack runs out */
static inline PyObject *By_DataclassRepr(PyObject *self, const By_DataField *fields) {
    PyObject *parts, *sep, *joined, *result;
    const By_DataField *field;
    int entered = Py_ReprEnter(self);
    if (entered != 0) {
        return entered > 0 ? PyUnicode_FromString("...") : NULL;
    }
    parts = PyList_New(0);
    if (parts == NULL) {
        Py_ReprLeave(self);
        return NULL;
    }
    for (field = fields; field->name != NULL; field++) {
        PyObject *value = field->get(self, NULL);
        PyObject *piece;
        if (value == NULL) {
            Py_DECREF(parts);
            Py_ReprLeave(self);
            return NULL;
        }
        piece = PyUnicode_FromFormat("%s=%R", field->name, value);
        Py_DECREF(value);
        if (piece == NULL || PyList_Append(parts, piece) < 0) {
            Py_XDECREF(piece);
            Py_DECREF(parts);
            Py_ReprLeave(self);
            return NULL;
        }
        Py_DECREF(piece);
    }
    sep = PyUnicode_FromString(", ");
    joined = sep == NULL ? NULL : PyUnicode_Join(sep, parts);
    Py_XDECREF(sep);
    Py_DECREF(parts);
    if (joined == NULL) {
        Py_ReprLeave(self);
        return NULL;
    }
    /* the *instance's* type rather than the one that declared the slot, because that is
     * what `self.__class__.__qualname__` in the generated `__repr__` reads */
    result = PyUnicode_FromFormat("%s(%U)", By_TypeName(self), joined);
    Py_DECREF(joined);
    Py_ReprLeave(self);
    return result;
}

/* the `__eq__` a `@dataclass` generates, which is
 *
 *     if self is other: return True
 *     if other.__class__ is self.__class__:
 *         return self.a==other.a and self.b==other.b
 *     return NotImplemented
 *
 * three things in that are easy to get subtly wrong, and each has been:
 *
 * - the identity test is a *rule*, not a shortcut. without it `p == p` would be False for
 *   a dataclass holding a NaN, and it is True
 * - the class test is identity too, so a subclass instance is never equal to a base one
 * - the terms are `==`, so no identity shortcut applies to a field. `PyObject_RichCompareBool`
 *   has one, which would make two instances sharing one NaN object compare equal where
 *   python says they do not
 *
 * `and` hands back the *term* rather than a bool, so a field whose `__eq__` answers with
 * something other than True or False is what the whole comparison answers with — and the
 * chain stops at the first falsy one. `__ne__` is not generated: python's own derives it
 * from this by negating, so the four ordering opcodes are refused here and `Py_NE` is the
 * negation `object.__ne__` would have produced */
static inline PyObject *By_DataclassEq(PyObject *self, PyObject *other, int op,
                                       const By_DataField *fields) {
    const By_DataField *field;
    PyObject *result;
    int truthy = 1;
    if (op != Py_EQ && op != Py_NE) Py_RETURN_NOTIMPLEMENTED;
    if (self == other) return PyBool_FromLong(op == Py_EQ);
    if (Py_TYPE(self) != Py_TYPE(other)) Py_RETURN_NOTIMPLEMENTED;
    /* what a class with no fields at all answers, where python writes a bare `True` */
    result = Py_NewRef(Py_True);
    for (field = fields; field->name != NULL; field++) {
        PyObject *mine = field->get(self, NULL);
        PyObject *theirs;
        if (mine == NULL) {
            Py_DECREF(result);
            return NULL;
        }
        theirs = field->get(other, NULL);
        if (theirs == NULL) {
            Py_DECREF(mine);
            Py_DECREF(result);
            return NULL;
        }
        Py_DECREF(result);
        result = PyObject_RichCompare(mine, theirs, Py_EQ);
        Py_DECREF(mine);
        Py_DECREF(theirs);
        if (result == NULL) return NULL;
        truthy = PyObject_IsTrue(result);
        if (truthy < 0) {
            Py_DECREF(result);
            return NULL;
        }
        if (!truthy) break;
    }
    if (op == Py_EQ) return result;
    Py_DECREF(result);
    return PyBool_FromLong(!truthy);
}

/* `hash((self.a, self.b))`, which is what a frozen `@dataclass` hashes
 *
 * the tuple is built rather than folded by hand: two instances that compare equal have to
 * hash equal, and the only way to promise that against python's own tuple hash is to use
 * it */
static inline Py_hash_t By_DataclassHash(PyObject *self, const By_DataField *fields) {
    Py_ssize_t count = 0;
    Py_ssize_t at;
    PyObject *values;
    Py_hash_t hash;
    while (fields[count].name != NULL) count++;
    values = PyTuple_New(count);
    if (values == NULL) return -1;
    for (at = 0; at < count; at++) {
        PyObject *value = fields[at].get(self, NULL);
        if (value == NULL) {
            Py_DECREF(values);
            return -1;
        }
        PyTuple_SET_ITEM(values, at, value);
    }
    hash = PyObject_Hash(values);
    Py_DECREF(values);
    return hash;
}

/* `dataclasses.FrozenInstanceError`, held for the life of the module
 *
 * the twin's own source imports `dataclasses` to be decorated at all, so by the time an
 * emitted type can be handed an instance the module is in `sys.modules` and this costs a
 * dict lookup */
static inline PyObject *By_FrozenInstanceError(void) {
    static PyObject *held = NULL;
    if (held == NULL) {
        PyObject *module = PyImport_ImportModule("dataclasses");
        if (module == NULL) return NULL;
        held = PyObject_GetAttrString(module, "FrozenInstanceError");
        Py_DECREF(module);
    }
    return held;
}

/* the `__setattr__` and `__delattr__` a frozen `@dataclass` generates, which share one
 * slot here the way they share one body there
 *
 * python's pair refuses every name on an instance of the class that declared them, and
 * only a *subclass* instance writing a name that is not a field gets through to the
 * ordinary attribute machinery — which is why `owner` is asked about rather than assumed */
static inline int By_FrozenSetAttr(PyObject *self, PyObject *name, PyObject *value,
                            PyTypeObject *owner, const By_DataField *fields) {
    const By_DataField *field;
    int mine = Py_TYPE(self) == owner;
    for (field = fields; field->name != NULL && !mine; field++) {
        mine = PyUnicode_CompareWithASCIIString(name, field->name) == 0;
    }
    if (mine) {
        PyObject *error = By_FrozenInstanceError();
        if (error == NULL) return -1;
        PyErr_Format(error,
                     value == NULL ? "cannot delete field %R" : "cannot assign to field %R",
                     name);
        return -1;
    }
    return PyObject_GenericSetAttr(self, name, value);
}

/* the dataclass bookkeeping that has no slot to fill, carried off the twin's class
 *
 * `__dataclass_fields__` is a dict of `dataclasses.Field` objects and `__dataclass_params__`
 * an object of that module's own — neither is code this compiler could emit, and both are
 * exactly right for the emitted type as they stand, because every one of them describes
 * the *fields* rather than the class. `dataclasses.fields`, `asdict`, `astuple` and
 * `replace` all read the first of them and then go through the ordinary attribute and
 * construction machinery, which the emitted type answers.
 *
 * `By_AdoptTwinAttributes` refuses every dunder, and rightly: a name written into
 * `tp_dict` does not fill a type slot, so an adopted `__eq__` would answer `a.__eq__(b)`
 * while `a == b` still went to the slot. none of the names here has a slot, so that
 * hazard does not reach them — and they are listed one by one rather than admitted as a
 * class, because the ones with slots are exactly what this compiler emits instead */
static inline int By_CarryDataclassMembers(PyObject *twin, PyObject *type) {
    static const char *const carried[] = {
        "__dataclass_fields__", "__dataclass_params__", "__match_args__",
        /* `copy.replace` reaches for this, and the function it names calls
         * `dataclasses.replace`, which is carried above */
        "__replace__",
        /* `@dataclass` writes the class's signature here where the body wrote no
         * docstring, and leaves the docstring alone where it did */
        "__doc__",
        /* the field names `slots=True` declares, which `copyreg` reads to learn what an
         * instance with no dict holds — and so what `copy` and `pickle` hand over */
        "__slots__",
        /* what `slots=True` adds to a frozen class, which pickles through them: python's
         * own state protocol would set each field back through the `__setattr__` that
         * refuses every write */
        "__getstate__", "__setstate__",
        NULL};
    const char *const *name;
    PyObject *source, *target;
    if (twin == NULL || type == NULL) return 0;
    if (!PyType_Check(twin) || !PyType_Check(type)) return 0;
    source = ((PyTypeObject *)twin)->tp_dict;
    target = ((PyTypeObject *)type)->tp_dict;
    if (source == NULL || target == NULL) return 0;
    for (name = carried; *name != NULL; name++) {
        PyObject *value = PyDict_GetItemString(source, *name);
        if (value == NULL) {
            if (PyErr_Occurred()) return -1;
            continue;
        }
        if (PyDict_SetItemString(target, *name, value) < 0) return -1;
    }
    PyType_Modified((PyTypeObject *)type);
    return 0;
}

/* the compiled methods an emitted type answers with, standing where its body's own
 * functions do
 *
 * a class body that fills a table with methods of its own class writes the *interpreted*
 * function into it, because the body that ran is the twin's:
 *
 *     class Unpickler:
 *         dispatch = {}
 *         def load_proto(self): ...
 *         dispatch[PROTO[0]] = load_proto      # 68 of these in `pickle`
 *
 * that table is copied onto the emitted type as the object the body left, and two things
 * follow. every call through it lands in the interpreted definition rather than the
 * compiled one, which is slow. and `dispatch[k] is Unpickler.load_proto` answers False
 * where the interpreted class answers True, which is *wrong* — the type answers with a
 * compiled method while the table it publishes answers with the twin's function. so the
 * table is moved rather than left.
 *
 * the pairing is by name, out of the two `tp_dict`s, which is the substitution
 * `By_TwinReplacement` makes for a class one scope in — and the move itself is the same
 * settling walk, so a table nested in a list or a dict of tables is reached the same way.
 * three things are left alone:
 *
 *  - an entry the twin holds that is not a plain python function. whatever else stands
 *    there is a wrapper the emitted type need not have rebuilt the same way, and a
 *    `staticmethod` object is not the object a call through the table wants
 *  - a function standing under two names in the twin's dict. `__str__ = __repr__` puts one
 *    object under both, and there is no single compiled method it should become
 *  - a method the type declined, which has no entry to pair against. what the type answers
 *    under that name is the twin's function too — `By_AdoptTwinAttributes` carried it — so
 *    the table and the type still agree, slow rather than wrong
 *
 * the class pairs are carried along in the same arrays, so a twin class sitting in one of
 * these tables moves onto its type at the same time */
static int By_RemapTwinMethods(const By_Twins *twins) {
    PyObject **from;
    PyObject **to;
    /* the layouts run alongside the pairs and are read at the same index, so the wider
     * arrays need one too. a method pair is not a class, so every entry past the class
     * pairs is NULL — no instance is ever moved onto a function */
    const By_Field **layouts;
    By_Twins wider;
    Py_ssize_t room = twins->count;
    Py_ssize_t total = twins->count;
    Py_ssize_t index;
    Py_ssize_t at;

    for (index = 0; index < twins->count; index++) {
        PyObject *twin = twins->twins[index];
        if (twin == NULL || !PyType_Check(twin)) continue;
        if (((PyTypeObject *)twin)->tp_dict == NULL) continue;
        room += PyDict_GET_SIZE(((PyTypeObject *)twin)->tp_dict);
    }
    if (room <= 0) return 0;
    from = PyMem_New(PyObject *, (size_t)room);
    to = PyMem_New(PyObject *, (size_t)room);
    layouts = PyMem_New(const By_Field *, (size_t)room);
    if (from == NULL || to == NULL || layouts == NULL) {
        PyMem_Free(from);
        PyMem_Free(to);
        PyMem_Free(layouts);
        PyErr_NoMemory();
        return -1;
    }
    for (index = 0; index < room; index++) {
        layouts[index] = index < twins->count ? twins->layouts[index] : NULL;
    }
    /* held rather than borrowed for the whole walk below: settling one value can run
     * arbitrary code, and a pair this is still to substitute must not go away underneath
     * it */
    for (index = 0; index < twins->count; index++) {
        from[index] = By_NewRef(twins->twins[index]);
        to[index] = By_NewRef(twins->types[index]);
    }

    for (index = 0; index < twins->count; index++) {
        PyObject *twin = twins->twins[index];
        PyObject *type = twins->types[index];
        PyObject *source, *target, *names;
        if (twin == NULL || type == NULL || twin == type) continue;
        if (!PyType_Check(twin) || !PyType_Check(type)) continue;
        source = ((PyTypeObject *)twin)->tp_dict;
        target = ((PyTypeObject *)type)->tp_dict;
        if (source == NULL || target == NULL) continue;
        /* the keys first, as `By_AdoptTwinAttributes` takes them: nothing may be walking
         * a dict while its values are read back out one at a time */
        names = PyDict_Keys(source);
        if (names == NULL) {
            PyErr_Clear();
            continue;
        }
        for (at = 0; at < PyList_GET_SIZE(names) && total < room; at++) {
            PyObject *key = PyList_GET_ITEM(names, at);
            PyObject *held = PyDict_GetItem(source, key);
            PyObject *stands;
            if (held == NULL || !PyFunction_Check(held)) continue;
            stands = PyDict_GetItem(target, key);
            if (stands == NULL) continue;
            /* a name under which the type holds the very function the twin does is not a
             * replacement at all — it is the *alias* half of `also = own`, carried across
             * verbatim. taking it as a pair would say that one function has two answers
             * and the ambiguity rule would then drop both, so the definition's own name
             * would stop being paired because something else pointed at it */
            if (stands == held) continue;
            from[total] = By_NewRef(held);
            to[total] = By_NewRef(stands);
            total++;
        }
        Py_DECREF(names);
    }

    /* the ambiguous pairs, dropped before any of them is used. a slot with nothing in it
     * matches no value, so the function it stood for is left exactly where the body put
     * it */
    for (index = twins->count; index < total; index++) {
        PyObject *paired = from[index];
        Py_ssize_t other;
        int ambiguous = 0;
        if (paired == NULL) continue;
        /* the pair being compared against is cleared *after* the scan, not during it —
         * clearing it first would stop a third name under the same function matching */
        for (other = index + 1; other < total; other++) {
            if (from[other] != paired) continue;
            Py_CLEAR(from[other]);
            Py_CLEAR(to[other]);
            ambiguous = 1;
        }
        if (ambiguous) {
            Py_CLEAR(from[index]);
            Py_CLEAR(to[index]);
        }
    }

    if (total > twins->count) {
        wider.twins = from;
        wider.types = to;
        wider.layouts = layouts;
        wider.count = total;
        wider.moved = twins->moved;
        for (index = 0; index < twins->count; index++) {
            PyObject *type = twins->types[index];
            PyObject *dict;
            PyObject *keys;
            if (type == NULL || !PyType_Check(type)) continue;
            dict = ((PyTypeObject *)type)->tp_dict;
            if (dict == NULL) continue;
            /* taken out as a list for the same reason the keys above are: a value is read
             * back one at a time and nothing may be walking the dict while that happens */
            keys = PyDict_Keys(dict);
            if (keys == NULL) {
                PyErr_Clear();
                continue;
            }
            for (at = 0; at < PyList_GET_SIZE(keys); at++) {
                PyObject *key = PyList_GET_ITEM(keys, at);
                PyObject *value = PyDict_GetItem(dict, key);
                PyObject *stands;
                if (value == NULL) continue;
                By_SettleTwins(value, &wider);
                /* and the entry itself, where it *is* one of this class's own functions.
                 * `also = own` in a class body is that: the body bound both names to one
                 * object, the definition's name now answers a compiled method, and the
                 * alias was left holding the twin's function — so `C.also is C.own` was
                 * False where python says True.
                 *
                 * only a function is moved here, and only one paired above. an entry
                 * holding anything else is a value the carry already made its own
                 * substitution for, and making a second one here would be this walk
                 * deciding something it has not established */
                if (!PyFunction_Check(value)) continue;
                stands = By_TwinFor(value, &wider);
                if (stands == NULL || stands == value) continue;
                if (PyDict_SetItem(dict, key, stands) < 0) {
                    PyErr_Clear();
                    continue;
                }
                /* the attribute cache would otherwise go on serving the twin's function */
                PyType_Modified((PyTypeObject *)type);
            }
            Py_DECREF(keys);
        }
    }

    for (index = 0; index < total; index++) {
        Py_XDECREF(from[index]);
        Py_XDECREF(to[index]);
    }
    PyMem_Free(layouts);
    PyMem_Free(from);
    PyMem_Free(to);
    return 0;
}

/* move a class-level constant from the interpreted definition onto the compiled type
 *
 * a *static* type is immutable to `setattr`, which is what licenses direct dispatch — so
 * this writes the type's dict, the way a C extension declares its own class attributes.
 *
 * the value comes out of the body that definition wrote, so `attr = C` in a class body
 * hands over the *interpreted* `C`, and copying that verbatim gives the type an attribute
 * naming a class nothing else in the module can reach. a value that *is* a twin is
 * therefore replaced by the type standing in for it, exactly as a carried attribute is.
 *
 * a value that merely *reaches* one is left as the interpreted definition had it, and that
 * is the one place this differs from `By_AdoptTwinAttributes`. dropping it instead was
 * built and backed out on the measurement: it loses 65 attributes over the corpus —
 * `ipaddress` its network constants among them. absence would be the better failure if the
 * reach were new, but it is a defect this copy has always had, and a question of its own
 * rather than one to settle as a side effect of the identity
 */
static inline int By_CopyClassConstant(PyObject *body, PyTypeObject *type, const char *name,
                                       const By_Twins *twins) {
    const char *const names[] = {name};
    By_ClassConstants constants = {body, names, 1, 0, twins};
    /* the same value a class built through its metaclass is handed before the call, so the
     * two constructions cannot drift apart about what a constant is */
    PyObject *stands = By_ConstantValue(&constants, 0);
    int result;
    if (stands == NULL) return 0;
    result = PyDict_SetItemString(type->tp_dict, name, stands);
    Py_DECREF(stands);
    if (result == 0) PyType_Modified(type);
    return result;
}

/* the module-level names still bound to an interpreted twin, moved onto what replaced it
 *
 * the whole module body runs against the interpreted definitions, so every name it binds
 * to a class holds the twin — `Kind = C`, a re-export under another spelling, a name a
 * conditional picked — while the compiled type only ever replaces the one name the
 * `class` statement wrote. what that leaves is two classes of the same name in the same
 * module: `Kind()` builds an object `isinstance(obj, C)` denies, and a compiled method
 * handed one refuses it outright with `doesn't apply to a 'C' object`.
 *
 * a name that *is* a twin is the one shape that can be moved soundly, and it is the same
 * substitution `By_TwinReplacement` makes for a carried attribute. it is made against
 * whatever now stands under the class's own name rather than against the type directly,
 * so a decorated class hands its aliases the decorator's answer — which is what the body
 * bound them to — instead of the type the decorator was given.
 *
 * an instance one of the twins built is moved the same way, onto the instance that took
 * its place — `logging` binds `root` at module level to the very object it also writes
 * into `Logger.root`, and the two must go on being one object. a value that merely
 * *reaches* a twin is not moved and cannot be: a list holding one is the same object the
 * body kept, and it is settled in place instead */

/* what should stand under a name still bound to `value`, borrowed, or NULL for a name that
 * is to be left alone
 *
 * only a value that *is* something replaced is rebound. settling a value the module could
 * not write in place hands back a copy, and putting a copy where the original stood would
 * break every other holder's `is` against it — so the two shapes answered here are the two
 * with a replacement of their own: a twin class, and an instance already moved onto one */
static PyObject *By_StandsFor(PyObject *value, const By_Twins *twins) {
    PyObject *held;
    PyObject *stands = By_TwinFor(value, twins);
    if (stands != NULL) return stands;
    if (twins->moved == NULL) return NULL;
    held = By_MovedFor(value, twins->moved);
    if (held == NULL) return NULL;
    stands = PyTuple_GET_ITEM(held, 1);
    return stands == Py_None ? NULL : stands;
}

/* everything a declined class still holds, given the same treatment as a module-level name
 *
 * a class this module left to its interpreted definition keeps its own methods, and those
 * methods captured their defaults and their closures while the fallback source ran — so
 * what they hold for a class that *was* replaced is the twin. `inspect.Signature.__init__`
 * is the case that found this: its `return_annotation=_empty` kept the twin `_empty` while
 * the module's own name answered the compiled type, so `Signature()` rendered
 * `() -> _empty` where python renders `()`.
 *
 * only a heap type is walked, and only its own dict. a type this module emitted is not one
 * of these — its attributes come from `By_AdoptTwinAttributes` and `By_CopyClassConstant`,
 * which make the same substitution at the point they copy */
static inline int By_RemapTwinsInClass(PyObject *cls, const By_Twins *twins) {
    PyObject *dict;
    PyObject *keys;
    Py_ssize_t at;
    if (!PyType_Check(cls) || !(((PyTypeObject *)cls)->tp_flags & Py_TPFLAGS_HEAPTYPE)) {
        return 0;
    }
    dict = PyObject_GetAttrString(cls, "__dict__");
    if (dict == NULL) {
        PyErr_Clear();
        return 0;
    }
    keys = PyMapping_Keys(dict);
    Py_DECREF(dict);
    if (keys == NULL) {
        PyErr_Clear();
        return 0;
    }
    for (at = 0; at < PyList_GET_SIZE(keys); at++) {
        PyObject *key = PyList_GET_ITEM(keys, at);
        PyObject *value = PyObject_GetAttr(cls, key);
        PyObject *stands;
        if (value == NULL) {
            PyErr_Clear();
            continue;
        }
        By_SettleTwins(value, twins);
        /* a class attribute holding a twin — `Signature.empty = _empty` — is the same
         * staleness one step along, and rebinding the name answers it. `By_StandsFor` says
         * which values that is */
        stands = By_StandsFor(value, twins);
        if (stands != NULL && stands != value && PyObject_SetAttr(cls, key, stands) < 0) {
            PyErr_Clear();
        }
        Py_DECREF(value);
    }
    Py_DECREF(keys);
    return 0;
}

static inline int By_RemapTwinAliases(PyObject *module_dict, const By_Twins *twins,
                                      const char *const *names) {
    /* the keys first: the dict is written while this walks, and only for keys it
     * already holds, but nothing may run against it mid-walk either way */
    PyObject *keys = PyDict_Keys(module_dict);
    Py_ssize_t at;
    if (keys == NULL) return -1;
    for (at = 0; at < PyList_GET_SIZE(keys); at++) {
        PyObject *key = PyList_GET_ITEM(keys, at);
        PyObject *value = PyDict_GetItem(module_dict, key);
        PyObject *moved;
        Py_ssize_t index;
        if (value == NULL) continue;
        /* whatever else becomes of this name, what it holds may have captured a twin —
         * and where it holds an instance one of them built, this is what moves it */
        By_SettleTwins(value, twins);
        if (By_RemapTwinsInClass(value, twins) < 0) {
            Py_DECREF(keys);
            return -1;
        }
        for (index = 0; index < twins->count; index++) {
            PyObject *stands;
            if (twins->twins[index] == NULL || value != twins->twins[index]) continue;
            /* a class is read back through its own name rather than off the array, so a
             * decorated one hands its aliases the decorator's answer — which is what the
             * body bound them to. a `def` gets no such second pass, and its own name is
             * not holding the forwarder yet: what stands for one is the replacement
             * itself */
            stands = PyType_Check(twins->twins[index])
                         ? PyDict_GetItemString(module_dict, names[index])
                         : twins->types[index];
            /* the class's own name already holds it, and one whose type was never
             * installed still holds the twin — neither is a move */
            if (stands == NULL) PyErr_Clear();
            if (stands == NULL || stands == value) break;
            if (PyDict_SetItem(module_dict, key, stands) < 0) {
                Py_DECREF(keys);
                return -1;
            }
            break;
        }
        /* a moved instance has no name of its own to be read back through the way a class
         * does — nothing decorates it — so it is rebound against the move directly */
        if (twins->moved == NULL) continue;
        moved = By_MovedFor(value, twins->moved);
        if (moved == NULL) continue;
        moved = PyTuple_GET_ITEM(moved, 1);
        if (moved == Py_None || moved == value) continue;
        if (PyDict_SetItem(module_dict, key, moved) < 0) {
            Py_DECREF(keys);
            return -1;
        }
    }
    Py_DECREF(keys);
    return 0;
}

/* ── what the module installed, against what it meant to install ─────────────────────
 *
 * a class that reports as compiled and then does not stand under its own name is the
 * one failure nothing else here can see. every sweep compares the compiled leg against
 * the interpreted twin, and a class that fell back to its interpreted definition
 * answers *identically* — so it agrees with every rung at once, while `--annotate` goes
 * on reporting it as compiled. that made every coverage figure this project has quoted
 * an upper bound rather than a count, and the first proof of it was a `MutableMapping`
 * subclass whose report said `6 compiled, 0 left interpreted` while `type(m.D)` was
 * `abc.ABCMeta` at import.
 *
 * so init ends by asking the finished namespace what is actually in it. four questions,
 * each of them a wrong answer that has already been shipped from here:
 *
 *   the module's own name holds the emitted type, rather than the interpreted
 *   definition still sitting behind it
 *
 *   `__bases__` holds the emitted base by identity, where this module emitted one. a
 *   class left standing on an orphaned copy of its base answers `isinstance` False
 *   where python answers True
 *
 *   `_abc_impl` is the twin's object, where the twin had one. `X.register(Y)` records
 *   `Y` inside `X`'s own registry and the module body ran against the twin, so a type
 *   holding the fresh empty one it was built with answers `issubclass(dict, Mapping)`
 *   False against an interpreted leg that answers True
 *
 *   a method the class lowered answers as a descriptor. a `function` under that name is
 *   the interpreted definition carried across, and it takes no receiver from a slot
 *
 * a class the module *deliberately* stood down is none of those. the layout guard and
 * the install gate exist to leave a class interpreted where installing it would be
 * wrong, and there the interpreted definition keeping the name is the answer — so that
 * is recorded rather than raised. what is raised is a class that was installed and then
 * does not hold up, which is a wrong answer the program has simply not reached yet */

/* the environment variable a build sets to collect the census, naming a file to append
 * to. the census is what closes the measurement hole: `--annotate` says which classes a
 * module *meant* to compile, this says which ones an import actually stood a type
 * under, and a build that compares the two is holding the report to what ran */
#define BY_INSTALL_CENSUS_ENV "BY_INSTALL_CENSUS"

/* one census row, appended. reopened per row on purpose: a module init that fails after
 * this point must still leave what it had already found on disk, and there is no later
 * moment in the extension's life at which a held file would be closed */
static void By_RecordInstall(const char *module, const char *name, const char *verdict) {
    const char *path = getenv(BY_INSTALL_CENSUS_ENV);
    FILE *out;
    if (path == NULL || path[0] == '\0') return;
    out = fopen(path, "a");
    if (out == NULL) return;
    fprintf(out, "%s\t%s\t%s\n", module, name, verdict);
    fclose(out);
}

/* one census row for a published property, named `Class.name` so it can never be taken for
 * a class's own row. nothing is formatted unless the census was asked for */
static void By_RecordProperty(const char *module, const char *owner, const char *name,
                              const char *verdict) {
    char qualified[512];
    const char *path = getenv(BY_INSTALL_CENSUS_ENV);
    if (path == NULL || path[0] == '\0') return;
    PyOS_snprintf(qualified, sizeof qualified, "%s.%s", owner, name);
    By_RecordInstall(module, qualified, verdict);
}

/* whether `wanted` is among the type's own bases, by identity */
static int By_HasBase(PyObject *type, PyObject *wanted) {
    PyObject *chain = ((PyTypeObject *)type)->tp_bases;
    Py_ssize_t at;
    if (chain == NULL || !PyTuple_Check(chain)) return 0;
    for (at = 0; at < PyTuple_GET_SIZE(chain); at++) {
        if (PyTuple_GET_ITEM(chain, at) == wanted) return 1;
    }
    return 0;
}

/* whether the class settled this name itself after the type was built, so that what is
 * under it is no longer the descriptor the method table put there — a decorated method
 * above all, whose value is whatever the decorator returned */
static int By_SettledMember(const char *const *settled, Py_ssize_t count, const char *name) {
    Py_ssize_t at;
    for (at = 0; at < count; at++) {
        if (strcmp(settled[at], name) == 0) return 1;
    }
    return 0;
}

/* one class, checked against what the module emitted for it. 0 when it holds up or was
 * stood down on purpose, -1 with an `ImportError` set when it does not
 *
 * `stands` is what this class's name means when init has finished — init's own
 * `by_type` slot, which holds the emitted type where the class installed and the
 * interpreted definition where it did not. `emitted` is the type object the module
 * built, which is *not* the same question: the install gate stands a whole family down
 * together, so a class whose own construction worked can still be left interpreted
 * because another in its family refused. `twin` is the interpreted definition, borrowed
 * and still alive at this point.
 *
 * `bases` are what this module's own bases for the class *mean*, taken from the same
 * `by_type` slots — so a family that stood down together is checked against its own
 * standing definitions rather than against types nothing can reach. `methods` is the
 * class's `tp_methods` table, which is exactly the set of names lowered into
 * descriptors, and `settled` names the entries a decorator has since replaced. `body` is the
 * class's dict as its `class` statement left it and `statement` the class that statement
 * built, each NULL where none was captured */
static int By_VerifyClass(const char *module, PyObject *dict, const char *name,
                          PyObject *stands, PyObject *emitted, PyObject *twin,
                          PyObject *const *bases, Py_ssize_t base_count,
                          PyMethodDef *methods, const char *const *settled,
                          Py_ssize_t settled_count, int decorated, PyObject *body,
                          PyObject *statement) {
    PyObject *published, *target, *source, *held;
    Py_ssize_t at;
    /* `emitted != twin` because a construction that could not be rebuilt hands the
     * interpreted definition back to stand as the class — see `By_TypeThroughMetaclass`.
     * that leaves the name holding what it already held, and reading it as an install
     * would be the very lie this exists to catch */
    int installed =
        emitted != NULL && emitted != twin && stands == emitted && PyType_Check(emitted);

    /* `interpreted` where no type was built and `twin` where one was and then was not
     * what took the name — the second being the one the report has no idea about */
    By_RecordInstall(module, name,
                     installed ? "installed" : (emitted == NULL ? "interpreted" : "twin"));

    if (installed) {
        published = PyDict_GetItemString(dict, name);
        /* a class decorator is arbitrary python handed the class, and it is entitled to
         * publish something else entirely under that name. so the name is held to the
         * type only where nothing decorated it */
        if (!decorated && published != emitted) {
            PyErr_Format(PyExc_ImportError,
                         "%s.%s was compiled but did not install: the module publishes %s "
                         "under that name%s",
                         module, name,
                         published == NULL ? "nothing" : Py_TYPE(published)->tp_name,
                         published != NULL && published == twin
                             ? ", which is the interpreted definition"
                             : "");
            return -1;
        }
    }

    /* asked of a standing interpreted definition too, and against what each base means
     * rather than against the type built for it. a class left standing while the class
     * below it was replaced is on an orphaned copy of that base, and `isinstance` then
     * answers False where python answers True — a wrong answer whichever of the two
     * objects is under the name */
    if (stands != NULL && PyType_Check(stands)) {
        for (at = 0; at < base_count; at++) {
            if (bases[at] == NULL || !PyType_Check(bases[at])) continue;
            if (!By_HasBase(stands, bases[at])) {
                PyErr_Format(PyExc_ImportError,
                             "%s.%s stands on an orphaned base: %s is what this module's "
                             "own base of that name means and is not among its __bases__, "
                             "so isinstance answers False where python answers True",
                             module, name, ((PyTypeObject *)bases[at])->tp_name);
                return -1;
            }
        }
    }

    /* the two below are about what an emitted type holds, and an interpreted definition
     * standing in for one holds neither: its registry *is* the twin's, and its methods
     * are the functions the `class` statement wrote */
    if (!installed) return 0;

    target = ((PyTypeObject *)emitted)->tp_dict;
    if (twin != NULL && PyType_Check(twin) && target != NULL) {
        source = ((PyTypeObject *)twin)->tp_dict;
        held = source == NULL ? NULL : PyDict_GetItemString(source, "_abc_impl");
        if (held != NULL && PyDict_GetItemString(target, "_abc_impl") != held) {
            PyErr_Format(PyExc_ImportError,
                         "%s.%s was compiled without the abstract-base registry its "
                         "interpreted definition holds: every register() the module body "
                         "made is invisible to it",
                         module, name);
            return -1;
        }
    }

    for (at = 0; methods != NULL && methods[at].ml_name != NULL; at++) {
        PyObject *entry;
        if (By_SettledMember(settled, settled_count, methods[at].ml_name)) continue;
        entry = target == NULL ? NULL : PyDict_GetItemString(target, methods[at].ml_name);
        /* the module body rewrote or deleted it after the `class` statement, and the type
         * took that answer on purpose */
        if (By_RewrittenMember(body, statement, twin, methods[at].ml_name, entry)
            && (entry == NULL || PyFunction_Check(entry))) {
            continue;
        }
        if (entry == NULL) {
            PyErr_Format(PyExc_ImportError,
                         "%s.%s was compiled but publishes nothing under %s, which it "
                         "lowered", module, name, methods[at].ml_name);
            return -1;
        }
        /* the interpreted definition, standing where a descriptor was lowered. it is
         * not merely slow: a `function` in a type's dict binds through the descriptor
         * protocol at every call, and the compiled body it shadows is never reached */
        if (PyFunction_Check(entry)) {
            PyErr_Format(PyExc_ImportError,
                         "%s.%s.%s answers as a python function where a descriptor was "
                         "lowered: the interpreted definition is what runs",
                         module, name, methods[at].ml_name);
            return -1;
        }
    }
    return 0;
}

/* record one class body, and the class its statement built, against its name, or -1 with
 * an exception set */
static int By_RecordClassBody(PyObject *state, PyObject *name, PyObject *body, PyObject *cls) {
    PyObject *bodies = PyDict_GetItemString(state, "bodies");
    PyObject *statements = PyDict_GetItemString(state, "statements");
    if (bodies == NULL || statements == NULL) {
        PyErr_SetString(PyExc_RuntimeError, "module body capture lost its record");
        return -1;
    }
    if (PyDict_SetItem(bodies, name, body) < 0) return -1;
    return PyDict_SetItem(statements, name, cls);
}

/* the class a `class` statement bound, after every one of its decorators, recorded with
 * what its dict held at that moment
 *
 * `pair` is `(state, decorator)`, where `decorator` is the outermost one the statement
 * wrote. its answer is what the name is bound to, so the dict read here is exactly what the
 * decorators left and nothing the module body did afterwards — see `By_BodyOutgrewClass` */
static PyObject *By_RecordBound(PyObject *pair, PyObject *written) {
    PyObject *state = PyTuple_GET_ITEM(pair, 0);
    PyObject *decorator = PyTuple_GET_ITEM(pair, 1);
    PyObject *bound = PyDict_GetItemString(state, "bound");
    PyObject *cls, *name, *snapshot, *record;
    int failed;
    if (bound == NULL) {
        PyErr_SetString(PyExc_RuntimeError, "module body capture lost its record");
        return NULL;
    }
    cls = PyObject_CallOneArg(decorator, written);
    if (cls == NULL || !PyType_Check(cls) || ((PyTypeObject *)cls)->tp_dict == NULL) return cls;
    name = PyObject_GetAttrString(cls, "__name__");
    snapshot = name == NULL ? NULL : PyDict_Copy(((PyTypeObject *)cls)->tp_dict);
    record = snapshot == NULL ? NULL : PyTuple_Pack(2, cls, snapshot);
    /* a record that cannot be kept is raised out of the statement, as a body that cannot
     * be is: init would otherwise leave the module interpreted without saying why */
    failed = record == NULL || PyDict_SetItem(bound, name, record) < 0;
    Py_XDECREF(record);
    Py_XDECREF(snapshot);
    Py_XDECREF(name);
    if (failed) {
        Py_DECREF(cls);
        return NULL;
    }
    return cls;
}

/* the decorator `__build_class__(decorator)` hands back in place of `decorator`: the same
 * call, recorded by `By_RecordBound` */
static PyObject *By_BoundRecorder(PyObject *state, PyObject *decorator) {
    static PyMethodDef record = {"__build_class__", (PyCFunction)By_RecordBound, METH_O, NULL};
    PyObject *pair = PyTuple_Pack(2, state, decorator), *recorder;
    if (pair == NULL) return NULL;
    recorder = PyCFunction_New(&record, pair);
    Py_DECREF(pair);
    return recorder;
}

/* `__build_class__`, recording what each module-level `class` statement wrote
 *
 * `state` holds `delegate` (the `__build_class__` this one displaced), `bodies` (the
 * mapping to record into) and `globals` (the module dict whose body is being recorded).
 * the class is built first and read afterwards, because the namespace itself is never
 * handed back: python gives it to the metaclass and to nobody else. what `type.__new__`
 * made of it is the closer thing anyway — it is exactly what the interpreted class holds
 * at the moment before the first of its decorators is handed it.
 *
 * this stands in the *real* builtins while a body runs, so every `class` statement in the
 * process reaches it and all but one module's have nothing to do with it. delegating is
 * therefore the ordinary case and recording the exception — see `By_RunModuleBody` */
static PyObject *By_CaptureClassBody(PyObject *state, PyObject *args, PyObject *kwds) {
    PyObject *delegate = PyDict_GetItemString(state, "delegate");
    PyObject *held = PyDict_GetItemString(state, "globals");
    PyObject *cls, *written, *name, *qualified, *body;
    int outermost;
    /* `globals` is `None` once the run is over and never absent, so a miss here is this
     * state having been broken rather than the run having ended, and passing it over would
     * cost every class after it its constants without saying so */
    if (delegate == NULL || held == NULL) {
        PyErr_SetString(PyExc_RuntimeError, "module body capture lost its `__build_class__`");
        return NULL;
    }
    /* `@__build_class__(decorator)` is what the twin writes around the outermost decorator
     * of a class init checks against what its statement bound. python's own refuses a
     * single argument, so the form means nothing else — and the frame asking is what says
     * whether it was written in this module or reached this hook from another run's */
    if (PyTuple_GET_SIZE(args) == 1 && (kwds == NULL || PyDict_GET_SIZE(kwds) == 0)
        && PyEval_GetGlobals() == held) {
        return By_BoundRecorder(state, PyTuple_GET_ITEM(args, 0));
    }
    cls = PyObject_Call(delegate, args, kwds);
    if (cls == NULL || PyTuple_GET_SIZE(args) < 2 || !PyType_Check(cls)) return cls;
    if (((PyTypeObject *)cls)->tp_dict == NULL) return cls;
    /* the body function python passes carries the globals of the module the `class`
     * statement was written in, which is what tells this module's statements from every
     * other one running against the same builtins — another thread's import, or a module
     * this body imported itself. once the run is over `globals` is `None` and nothing
     * matches it again */
    written = PyTuple_GET_ITEM(args, 0);
    if (!PyFunction_Check(written) || PyFunction_GetGlobals(written) != held) return cls;
    name = PyTuple_GET_ITEM(args, 1);
    /* a class written inside a function can be named the same as one at module level and
     * is not the same class. `f.<locals>.C` against `C` is what tells them apart, and the
     * body function python passes here is what carries that qualified name */
    qualified = PyObject_GetAttrString(written, "__qualname__");
    if (qualified == NULL) {
        PyErr_Clear();
        return cls;
    }
    outermost = PyObject_RichCompareBool(qualified, name, Py_EQ);
    Py_DECREF(qualified);
    if (outermost != 1) {
        if (outermost < 0) PyErr_Clear();
        return cls;
    }
    body = PyDict_Copy(((PyTypeObject *)cls)->tp_dict);
    /* a body that cannot be recorded is raised out of the `class` statement rather than
     * passed over: what would follow is a type carrying no constants at all, and for a
     * decorated class that is the defect this capture exists to remove */
    if (body == NULL || By_RecordClassBody(state, name, body, cls) < 0) {
        Py_XDECREF(body);
        Py_DECREF(cls);
        return NULL;
    }
    Py_DECREF(body);
    return cls;
}

/* a module's interpreted twin, in the two forms an artefact carries it
 *
 * `source` is the twin as text and is always here. `code` is the same program already
 * compiled, by the interpreter this artefact was built for, and it is the whole reason
 * an import is cheap: parsing a module the size of `argparse` costs milliseconds and
 * reading a marshalled code object costs tens of microseconds.
 *
 * it is a cache of the source rather than a replacement for it, and the two fields below
 * it say who may use it. an interpreter that does not match compiles the source instead,
 * which is slower and is the same program — the outcome must never turn on which of the
 * two ran */
typedef struct {
    const char *source;
    /* `marshal.dumps` of the module body's code object, or NULL where the build had no
     * interpreter to compile it with */
    const char *code;
    Py_ssize_t length;
    /* the bytecode magic of the interpreter that wrote it. cpython bumps this whenever a
     * code object stops meaning what it did, which is why an upgraded interpreter
     * regenerates a `.pyc` rather than misreading one — the same check, for the same
     * reason */
    long magic;
    /* the optimization level it was compiled at. `-O` takes `assert` out of the bytecode
     * and `-OO` takes docstrings too, so the same source at another level is a different
     * program. running the twin under `python -O` has always meant `-O`, and reading back
     * a code object compiled without it would quietly stop meaning that */
    int optimize;
} By_Fallback;

/* this interpreter's optimization level, or -1 where it will not say
 *
 * `sys.flags.optimize` rather than any of the C-level flags: those have been deprecated
 * and moved about across the versions this compiler targets, and this one is the reading
 * python's own `compile` takes */
static inline int By_OptimizeLevel(void) {
    PyObject *flags = PySys_GetObject("flags"); /* borrowed */
    PyObject *level;
    long value;
    if (flags == NULL) {
        PyErr_Clear();
        return -1;
    }
    level = PyObject_GetAttrString(flags, "optimize");
    if (level == NULL) {
        PyErr_Clear();
        return -1;
    }
    value = PyLong_AsLong(level);
    Py_DECREF(level);
    if (value == -1 && PyErr_Occurred()) {
        PyErr_Clear();
        return -1;
    }
    return (int)value;
}

/* pick the spelling of a docstring this interpreter would have produced itself
 *
 * from 3.13 the compiler cleans a docstring on its way into `__doc__`: tabs expanded to
 * eight-column stops, the first line's leading spaces gone, and the indentation shared by
 * the lines below it gone with them. 3.12 and earlier store the literal untouched. so the
 * two are different strings for almost every docstring written across more than one line,
 * and only the version compiling this file can say which one belongs here */
#if PY_VERSION_HEX >= 0x030D0000
#define BY_DOC(raw, cleaned) (cleaned)
#else
#define BY_DOC(raw, cleaned) (raw)
#endif

/* drop the docstrings out of a method table when this interpreter is running at `-OO`
 *
 * a docstring is baked into the artefact at build time, but the level the artefact is
 * *run* at is not settled until the import — and under `-OO` python compiles a function
 * with no docstring at all, so a compiled definition still answering with one would
 * disagree with the interpreted definition of the same source in the same process.
 *
 * clearing `ml_doc` is enough for both surfaces: a `builtin_function_or_method` and a
 * `method_descriptor` each read `__doc__` off the `PyMethodDef` they point at rather than
 * copying it, so this reaches every object built from the table, before or after.
 *
 * `sys.flags.optimize` cannot change once the interpreter is up, so this is a single
 * decision taken at module init rather than a test on every read */
static inline void By_StripDocsAtOO(PyMethodDef *methods) {
    if (By_OptimizeLevel() < 2) return;
    for (; methods->ml_name != NULL; methods++) methods->ml_doc = NULL;
}

/* the same, for a def standing on its own rather than in a table
 *
 * a `@property`'s halves are not in the class's method table — they are reached only
 * through the `property` published over them — so each is a `PyMethodDef` of its own with
 * no terminating entry for the walk above to stop on */
static inline void By_StripDocAtOO(PyMethodDef *def) {
    if (By_OptimizeLevel() < 2) return;
    def->ml_doc = NULL;
}

/* the twin's code object, where this interpreter may use the one the artefact carries
 *
 * hands back a new reference, or NULL. NULL with no exception set means there is nothing
 * here for *this* interpreter and the source should be compiled instead; NULL with one
 * set means there was and it would not read, which is a broken artefact rather than a
 * mismatched one and is raised rather than papered over. that distinction is what keeps a
 * defect in how these bytes are emitted from showing up as nothing worse than a slow
 * import nobody looks at */
static inline PyObject *By_FallbackCode(const By_Fallback *fallback) {
    PyObject *code;
    if (fallback->code == NULL || fallback->length <= 0) return NULL;
    if (PyImport_GetMagicNumber() != fallback->magic) {
        PyErr_Clear();
        return NULL;
    }
    if (By_OptimizeLevel() != fallback->optimize) return NULL;
    code = PyMarshal_ReadObjectFromString(fallback->code, fallback->length);
    if (code == NULL) {
        if (!PyErr_Occurred()) {
            PyErr_SetString(PyExc_ImportError,
                            "the interpreted definitions of this module could not be read");
        }
        return NULL;
    }
    if (!PyCode_Check(code)) {
        Py_DECREF(code);
        PyErr_SetString(PyExc_ImportError,
                        "the interpreted definitions of this module are not a code object");
        return NULL;
    }
    return code;
}

/* run the twin in `dict`, from the code object where there is a usable one
 *
 * `PyEval_EvalCode` and `PyRun_String` reach the same evaluator by the same route, and
 * both take the builtins the frame runs against from `dict["__builtins__"]` — which is
 * what lets the capture below swap that entry and have it seen */
static inline PyObject *By_ExecModuleBody(const By_Fallback *fallback, PyObject *dict) {
    PyObject *code = By_FallbackCode(fallback), *result;
    if (code != NULL) {
        result = PyEval_EvalCode(code, dict, dict);
        Py_DECREF(code);
        return result;
    }
    if (PyErr_Occurred()) return NULL;
    return PyRun_String(fallback->source, Py_file_input, dict, dict);
}

/* run a module's interpreted twin, capturing each class body before its decorators run
 *
 * the twin is the whole interpreted module and it runs to completion before any emitted
 * type is built, so by the time a class-level constant is copied onto one, the class it
 * would be copied off has already been through its own decorators. a decorator that only
 * *reads* the class leaves the value where the body put it, but one that makes something
 * of it does not: `@dataclass` deletes the `field(init=False)` a body wrote, and leaves a
 * bare `2` where `field(default=2, repr=False)` stood.
 *
 * so the body is taken while it still is the body. python routes every `class` statement
 * through `__build_class__`, and the one a statement reaches is the `__build_class__` of
 * *its own frame's* builtins — so the entry is displaced in the builtins the body will
 * actually run against, and put back when it is done.
 *
 * that has to be the interpreter's own builtins mapping rather than a copy of it, and the
 * reason is that the mapping outlives the exec. python gives a function the builtins its
 * defining module dict had *at the moment the function was made*, so every function and
 * every method this body defines holds this mapping for as long as it lives — and those
 * are exactly the definitions a declined function runs from. against a copy they would
 * read a snapshot of `builtins` frozen at import: a name rebound afterwards would still
 * answer with the old one, a name deleted would still answer, and a name added would not
 * be found. the compiled half of the same module sees all three, so the two halves would
 * disagree with each other as well as with the interpreted leg.
 *
 * standing in the real builtins means every `class` statement in the process reaches this
 * hook while a body runs — another thread's import, or a module this body imports itself.
 * two things keep that safe. the hook is *transparent*: it calls the entry it displaced
 * and records only where the `class` statement was written in this module's own globals,
 * so a statement that is not ours comes out of it exactly as it went in. and the entry is
 * put back only if this hook is still the one standing — where another run displaced it in
 * turn, that run holds this hook as its own delegate and will restore it, and restoring
 * over the top would drop that run's captures on the floor. a hook left standing that way
 * has already had its `globals` cleared, so it records nothing further and is pure
 * delegation.
 *
 * hands back `{name: body}` for the classes the body wrote at module level, as a new
 * reference, or NULL with an exception set where the body raised. `*statements` is given
 * `{name: class}` alongside it, the class each of those statements built before any
 * decorator was handed it, and `*bound` is given `{name: (class, dict)}` for each statement
 * the twin wrote `@__build_class__(...)` over — see `By_RecordBound` — each as a new
 * reference or NULL */
static inline PyObject *By_RunModuleBody(const By_Fallback *fallback, PyObject *dict,
                                         PyObject **statements, PyObject **bound) {
    static PyMethodDef capture = {"__build_class__",
                                  (PyCFunction)(void (*)(void))By_CaptureClassBody,
                                  METH_VARARGS | METH_KEYWORDS, NULL};
    PyObject *bodies, *stood, *mapping, *displaced, *state, *wrapper, *result;
    int failed;
    *statements = NULL;
    *bound = NULL;
    bodies = PyDict_New();
    if (bodies == NULL) return NULL;
    *statements = PyDict_New();
    *bound = PyDict_New();
    if (*statements == NULL || *bound == NULL) {
        Py_DECREF(bodies);
        Py_CLEAR(*statements);
        Py_CLEAR(*bound);
        return NULL;
    }
    /* an emitted module's dict has no `__builtins__` of its own, and python would then
     * give the body's frame the running interpreter's. naming it here is what makes the
     * functions the body defines take the same mapping rather than that fallback */
    stood = PyDict_GetItemString(dict, "__builtins__");
    if (stood == NULL) stood = PyEval_GetBuiltins();
    Py_XINCREF(stood);
    mapping = stood != NULL && PyModule_Check(stood) ? PyModule_GetDict(stood) : stood;
    if (mapping != NULL && !PyDict_Check(mapping)) mapping = NULL;
    displaced = mapping == NULL ? NULL : PyDict_GetItemString(mapping, "__build_class__");
    if (displaced == NULL) {
        Py_XDECREF(stood);
        Py_DECREF(bodies);
        Py_CLEAR(*statements);
        Py_CLEAR(*bound);
        if (!PyErr_Occurred()) {
            PyErr_SetString(PyExc_RuntimeError,
                            "no builtins `__build_class__` to run the module body against");
        }
        return NULL;
    }
    Py_INCREF(displaced);
    state = PyDict_New();
    failed = state == NULL || PyDict_SetItemString(state, "delegate", displaced) < 0
             || PyDict_SetItemString(state, "bodies", bodies) < 0
             || PyDict_SetItemString(state, "statements", *statements) < 0
             || PyDict_SetItemString(state, "bound", *bound) < 0
             || PyDict_SetItemString(state, "globals", dict) < 0;
    wrapper = failed ? NULL : PyCFunction_New(&capture, state);
    failed = failed || wrapper == NULL
             || PyDict_SetItemString(dict, "__builtins__", stood) < 0
             || PyDict_SetItemString(mapping, "__build_class__", wrapper) < 0;
    result = failed ? NULL : By_ExecModuleBody(fallback, dict);
    Py_XDECREF(result);
    /* whatever the body did, the capture stops here */
    if (state != NULL) {
        PyObject *type, *value, *traceback, *standing;
        PyErr_Fetch(&type, &value, &traceback);
        if (PyDict_SetItemString(state, "globals", Py_None) < 0) PyErr_Clear();
        standing = wrapper == NULL ? NULL : PyDict_GetItemString(mapping, "__build_class__");
        if (standing == wrapper && wrapper != NULL
            && PyDict_SetItemString(mapping, "__build_class__", displaced) < 0) {
            PyErr_Clear();
        }
        PyErr_Restore(type, value, traceback);
    }
    Py_XDECREF(wrapper);
    Py_XDECREF(state);
    Py_DECREF(displaced);
    Py_XDECREF(stood);
    if (failed || result == NULL) {
        Py_DECREF(bodies);
        Py_CLEAR(*statements);
        Py_CLEAR(*bound);
        return NULL;
    }
    return bodies;
}

/* the body captured for one class, as a borrowed reference, or NULL where there is none */
static inline PyObject *By_ClassBody(PyObject *bodies, const char *name) {
    if (bodies == NULL) return NULL;
    return PyDict_GetItemString(bodies, name);
}

/* whether `a` and `b` hold different objects under `key`, absence included */
static inline int By_HoldsApart(PyObject *a, PyObject *b, PyObject *key) {
    return PyDict_GetItem(a, key) != PyDict_GetItem(b, key);
}

/* whether reading the class wrote `key` into `now`, where the `class` statement left
 * `body` without it
 *
 * 3.14 works a class's annotations out on demand, and two reads write the answer back:
 * `__annotations__` keeps the mapping under `BY_ANNOTATIONS`, which is carried, and
 * `__annotate__` — which the first also asks — keeps `None` under `__annotate_func__`
 * where the class has no function computing them. a class with one holds it from the
 * statement on, so anything else under that key, or its going, is a write the body made */
static inline int By_ReadWrote(PyObject *now, PyObject *body, PyObject *key) {
#if PY_VERSION_HEX >= 0x030E0000
    return PyUnicode_CompareWithASCIIString(key, "__annotate_func__") == 0
           && PyDict_GetItem(body, key) == NULL && PyDict_GetItem(now, key) == Py_None;
#else
    (void)now;
    (void)body;
    (void)key;
    return 0;
#endif
}

/* whether `key` is a dunder the class as it stands, `now`, and the dict its statement
 * left, `body`, hold different objects under — the annotations aside, which are carried
 * (see `By_CarryAnnotations`), and so is what a read writes (see `By_ReadWrote`) */
static inline int By_DunderApart(PyObject *now, PyObject *body, PyObject *key) {
    return PyUnicode_CheckExact(key) && By_IsDunder(key)
           && PyUnicode_CompareWithASCIIString(key, BY_ANNOTATIONS) != 0
           && !By_ReadWrote(now, body, key) && By_HoldsApart(now, body, key);
}

/* whether the module body changed a class after its `class` statement in a way the type
 * replacing it cannot answer for, so that init has to leave the module as its interpreted
 * definition built it. both kinds of change are read off the twin against the dict the
 * statement left — see `By_CaptureClassBody`:
 *
 * - a dunder added, rewritten or deleted, on any class. `By_AdoptTwinAttributes` says why
 *   none is carried: a dunder is what a type slot answers, and the slot would go on
 *   answering the compiled body. the frontend declines a class the module body writes a
 *   dunder onto by name, `C.__len__ = f`, and this is the same refusal for the shapes no
 *   reading of the source can see — `setattr(C, name, f)`, or a helper the body calls. the
 *   annotations are carried, so they are left out
 * - anything written over one of `members`, which names what compiled code reaches on the
 *   class with no lookup at all: a method called, a property half run, a field read at its
 *   offset. that is sound only because nothing can write to the class once import is over,
 *   and a write the body made before then is carried into the type's dict, where none of
 *   those accesses looks. NULL for a class whose every access is checked, which copes with
 *   a rewrite as it copes with one made after import
 *
 * a class whose name no longer holds what its statement built is left alone, as
 * `By_CarryRewrittenMembers` leaves it. `rebuilt` says the twin is a `data class`, which
 * `@dataclass(slots=True)` made again out of what the statement built and wrote every
 * dunder it generates onto — so what that class is read against is not the body but what
 * the statement *bound*, after its decorators, which `bound` recorded (see
 * `By_RecordBound`). a dunder the module body wrote afterwards is then told apart from the
 * decorator's own exactly, whatever the decorator wrote. a data class with no record says
 * nothing about what its decorator left, and is answered the way that cannot be wrong.
 * the keys are exact strings because nothing that can run python may be asked while a dict
 * is being walked */
static inline int By_BodyOutgrewClass(PyObject *bodies, PyObject *statements, PyObject *bound,
                                      PyObject *dict, const char *name,
                                      const char *const *members, int rebuilt) {
    PyObject *body = By_ClassBody(bodies, name);
    PyObject *statement = By_ClassBody(statements, name);
    PyObject *twin = PyDict_GetItemString(dict, name);
    PyObject *now, *key, *value, *member;
    Py_ssize_t position = 0, at;
    int apart;
    if (rebuilt) {
        PyObject *record = By_ClassBody(bound, name);
        if (record == NULL || !PyTuple_Check(record) || PyTuple_GET_SIZE(record) != 2) return 1;
        statement = PyTuple_GET_ITEM(record, 0);
        body = PyTuple_GET_ITEM(record, 1);
    }
    if (body == NULL || twin == NULL || twin != statement) return 0;
    if (!PyDict_Check(body) || !PyType_Check(twin)) return 0;
    now = ((PyTypeObject *)twin)->tp_dict;
    if (now == NULL) return 0;
    /* both dicts, so that a dunder only one of them holds is seen whichever it is */
    while (PyDict_Next(now, &position, &key, &value)) {
        if (By_DunderApart(now, body, key)) return 1;
    }
    position = 0;
    while (PyDict_Next(body, &position, &key, &value)) {
        if (By_DunderApart(now, body, key)) return 1;
    }
    for (at = 0; members != NULL && members[at] != NULL; at++) {
        member = PyUnicode_FromString(members[at]);
        if (member == NULL) {
            /* a question that cannot be asked is answered the way that cannot be wrong */
            PyErr_Clear();
            return 1;
        }
        apart = By_HoldsApart(now, body, member);
        Py_DECREF(member);
        if (apart) return 1;
    }
    return 0;
}

/* the answer a class pattern gives when the attribute it named is simply absent
 *
 * a missing attribute is *no match*, not an error — `case Point(z=1):` against a
 * point with no `z` falls through to the next case. so the lookup needs a third
 * answer beyond a value and a failure, and this is it: an object no python value
 * can be identical to, because nothing else holds a reference to it
 */
static inline PyObject *By_MatchMissing(void) {
    static PyObject *missing = NULL;
    if (missing == NULL) missing = PyList_New(0);
    return missing;
}

/* `subject.<name>`, where absent is an answer rather than a failure */
static inline PyObject *By_MatchAttr(PyObject *subject, PyObject *name) {
    if (subject == NULL || name == NULL) return NULL;
    PyObject *value = PyObject_GetAttr(subject, name);
    if (value == NULL && PyErr_ExceptionMatches(PyExc_AttributeError)) {
        PyErr_Clear();
        return By_NewRef(By_MatchMissing());
    }
    return value;
}

/* `__aiter__` and `__anext__`, with the errors `async for` raises rather than the
 * ones an attribute lookup would
 *
 * python reports an object it cannot iterate, where a plain lookup reports a
 * missing attribute — a different exception type for the same mistake
 */
static inline PyObject *By_AsyncIter(PyObject *o, int next) {
    if (o == NULL) return NULL;
    PyAsyncMethods *async_ = Py_TYPE(o)->tp_as_async;
    unaryfunc get = NULL;
    if (async_ != NULL) get = next ? async_->am_anext : async_->am_aiter;
    if (get == NULL) {
        if (next) {
            PyErr_Format(PyExc_TypeError,
                         "'async for' received an object from __aiter__ that does not "
                         "implement __anext__: %.100s",
                         Py_TYPE(o)->tp_name);
        } else {
            PyErr_Format(PyExc_TypeError,
                         "'async for' requires an object with __aiter__ method, got %.100s",
                         Py_TYPE(o)->tp_name);
        }
        return NULL;
    }
    return get(o);
}

static inline char By_IsMatchMapping(PyObject *o) {
    return (char)(o != NULL && PyType_HasFeature(Py_TYPE(o), Py_TPFLAGS_MAPPING));
}

/* `map[key]`, where absent is an answer rather than a failure */
static inline PyObject *By_MatchKey(PyObject *map, PyObject *key) {
    if (map == NULL || key == NULL) return NULL;
    PyObject *value = PyObject_GetItem(map, key);
    if (value == NULL && PyErr_ExceptionMatches(PyExc_KeyError)) {
        PyErr_Clear();
        return By_NewRef(By_MatchMissing());
    }
    return value;
}

/* the dict a mapping pattern's `**rest` binds: the subject without the keys the
 * pattern named, and always a `dict` whatever the subject was */
static inline PyObject *By_MatchRestMapping(PyObject *map, PyObject *keys) {
    if (map == NULL || keys == NULL) return NULL;
    PyObject *rest = PyDict_New();
    if (rest == NULL) return NULL;
    if (PyDict_Update(rest, map) < 0) {
        Py_DECREF(rest);
        return NULL;
    }
    Py_ssize_t count = PyTuple_GET_SIZE(keys);
    for (Py_ssize_t i = 0; i < count; i++) {
        if (PyDict_DelItem(rest, PyTuple_GET_ITEM(keys, i)) < 0) {
            Py_DECREF(rest);
            return NULL;
        }
    }
    return rest;
}

/* whether a class matches its subject *whole* rather than by component
 *
 * these are the builtins with no `__match_args__` to name a part of one, so
 * `case int(x):` binds the int itself. the set is the one the language reference
 * lists, rather than a type flag, because the flag is not public API
 */
static inline int By_MatchesSelf(PyObject *class_) {
    PyTypeObject *types[] = {&PyBool_Type,     &PyByteArray_Type, &PyBytes_Type,
                             &PyDict_Type,     &PyFloat_Type,     &PyFrozenSet_Type,
                             &PyLong_Type,     &PyList_Type,      &PySet_Type,
                             &PyUnicode_Type,  &PyTuple_Type};
    for (size_t i = 0; i < sizeof(types) / sizeof(types[0]); i++) {
        if (class_ == (PyObject *)types[i]) return 1;
    }
    return 0;
}

/* the attribute the `index`th positional sub-pattern of a class pattern names
 *
 * `__match_args__` is what a class publishes to say which of its attributes
 * `case Cls(a, b)` means, in order
 *
 * a negative `index` counts back from the end of that tuple, the way python's
 * own list indexing does. that is what a sub-pattern basedpython wrote after a
 * `*_` needs: `case Cls(a, *_, b)` says `b` is the last of the names, and only
 * here is it known how many there are
 */
static inline PyObject *By_MatchPositional(PyObject *subject, PyObject *class_,
                                           Py_ssize_t index, Py_ssize_t count) {
    static PyObject *by_match_args = NULL;
    PyObject *key;
    if (subject == NULL || class_ == NULL) return NULL;
    const char *class_name = ((PyTypeObject *)class_)->tp_name;
    if (By_MatchesSelf(class_)) {
        if (count > 1) {
            PyErr_Format(PyExc_TypeError,
                         "%s() accepts 1 positional sub-pattern (%zd given)",
                         class_name, count);
            return NULL;
        }
        return By_NewRef(subject);
    }
    key = By_FixedName(&by_match_args, "__match_args__", 14);
    if (key == NULL) return NULL;
    /* one of these per positional sub-pattern, so `case Point(x, y)` asks twice for
     * every subject that reaches the arm */
    PyObject *names = PyObject_GetAttr(class_, key);
    if (names == NULL) {
        if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return NULL;
        PyErr_Clear();
        PyErr_Format(PyExc_TypeError,
                     "%s() accepts 0 positional sub-patterns (%zd given)", class_name,
                     count);
        return NULL;
    }
    if (!PyTuple_CheckExact(names)) {
        PyErr_Format(PyExc_TypeError, "%s.__match_args__ must be a tuple (got %s)",
                     class_name, Py_TYPE(names)->tp_name);
        Py_DECREF(names);
        return NULL;
    }
    Py_ssize_t available = PyTuple_GET_SIZE(names);
    if (count > available) {
        PyErr_Format(PyExc_TypeError,
                     "%s() accepts %zd positional sub-pattern%s (%zd given)", class_name,
                     available, available == 1 ? "" : "s", count);
        Py_DECREF(names);
        return NULL;
    }
    if (index < 0) index += available;
    PyObject *name = PyTuple_GET_ITEM(names, index);
    if (!PyUnicode_CheckExact(name)) {
        PyErr_Format(PyExc_TypeError,
                     "__match_args__ elements must be strings (got %s)",
                     Py_TYPE(name)->tp_name);
        Py_DECREF(names);
        return NULL;
    }
    PyObject *value = By_MatchAttr(subject, name);
    Py_DECREF(names);
    return value;
}

/* what a starred sequence pattern binds: everything between the fixed elements
 * at either end
 *
 * always a `list`, whatever the subject was — `case (a, *rest):` against a tuple
 * still binds a list, which is what the interpreter's `UNPACK_EX` produces
 */
static inline PyObject *By_MatchRest(PyObject *sequence, Py_ssize_t start, Py_ssize_t after) {
    if (sequence == NULL) return NULL;
    Py_ssize_t length = PySequence_Size(sequence);
    if (length < 0) return NULL;
    PyObject *slice = PySequence_GetSlice(sequence, start, length - after);
    if (slice == NULL) return NULL;
    PyObject *rest = PySequence_List(slice);
    Py_DECREF(slice);
    return rest;
}

/* the element a starred pattern's *trailing* fixed patterns name, counted from
 * the end so the star's own length does not have to be known here */
static inline PyObject *By_MatchFromEnd(PyObject *sequence, Py_ssize_t from_end) {
    if (sequence == NULL) return NULL;
    Py_ssize_t length = PySequence_Size(sequence);
    if (length < 0) return NULL;
    return PySequence_GetItem(sequence, length - from_end);
}

/* `value in container`, which is `__contains__` where the type has one and a scan
 * of the iterator otherwise — `PySequence_Contains` picks between them
 *
 * the membership arm of the pair [`By_GetItem`] and [`By_SetItem`] already have:
 * `sq_contains` on an exact dict is `PyDict_Contains`, reached through one more
 * indirection than a table lookup needs */
static inline char By_Contains(PyObject *container, PyObject *value, int negated) {
    if (container == NULL || value == NULL) return 2;
    int found = PyDict_CheckExact(container) ? PyDict_Contains(container, value)
                                             : PySequence_Contains(container, value);
    if (found < 0) return 2;
    return (char)(negated ? !found : found);
}

/* whether a dict's keys table holds nothing but exact `str` keys
 *
 * cpython keeps a table of exact `str` keys in a form of its own, and turns it into the
 * general form the moment any other key is stored. which form a table is in is
 * `dk_kind`, a field of the private `PyDictKeysObject` head, so reading it trusts a
 * layout no public header promises. that trust is earned twice over: only on the
 * versions whose layout was read (3.13 and 3.14, with the GIL, where the head is
 * `dk_refcnt`, `dk_log2_size`, `dk_log2_index_bytes`, `dk_kind`), and only once
 * [`By_CheckDictKinds`] has built dicts whose form is known and found the field saying
 * so. anything else answers 0, which is the answer that is never wrong */
#if PY_VERSION_HEX >= 0x030D0000 && PY_VERSION_HEX < 0x030F0000 && !defined(Py_GIL_DISABLED) \
    && !defined(Py_LIMITED_API)
#define BY_DICT_KINDS 1

typedef struct {
    Py_ssize_t dk_refcnt;
    uint8_t dk_log2_size;
    uint8_t dk_log2_index_bytes;
    uint8_t dk_kind;
} ByDictKeysHead;

/* 1 once the layout has been checked and found as read, 0 before that or if it was not */
static char by_dict_kinds_checked = 0;

static inline const ByDictKeysHead *By_DictKeysHead(PyObject *dict) {
    return (const ByDictKeysHead *)((PyDictObject *)dict)->ma_keys;
}

static inline char By_DictKeysAllStr(PyObject *dict) {
    return by_dict_kinds_checked && By_DictKeysHead(dict)->dk_kind != 0;
}

/* whether the head of `dict`'s table reads as a fresh table of `kind` would: a minimum
 * sized table owned by this dict alone, or a table shared between instances */
static char By_DictKeysReadAs(PyObject *dict, uint8_t kind) {
    const ByDictKeysHead *head = By_DictKeysHead(dict);
    if (head->dk_kind != kind) return 0;
    /* a split table is shared by the class and every instance using it */
    if (kind == 2) return head->dk_refcnt >= 1;
    return head->dk_refcnt == 1 && head->dk_log2_size == 3 && head->dk_log2_index_bytes == 3;
}

/* build one dict of each form and check the private read names each correctly
 *
 * `{"a": 1}` is a table of exact `str` keys, `{1: 1}` a general one, and the `__dict__`
 * of an instance of a fresh class a split one. a failure of any step, or any answer
 * other than the one the layout predicts, leaves every lookup on the form that reads no
 * private field, for as long as this module is loaded */
static void By_CheckDictKinds(void) {
    PyObject *text = NULL, *number = NULL, *type = NULL, *instance = NULL, *split = NULL;
    PyObject *name = NULL, *one = NULL;
    char held = 0;
    if (by_dict_kinds_checked) return;
    one = PyLong_FromLong(1);
    name = PyUnicode_FromString("a");
    text = PyDict_New();
    number = PyDict_New();
    if (one == NULL || name == NULL || text == NULL || number == NULL) goto done;
    if (PyDict_SetItem(text, name, one) < 0 || PyDict_SetItem(number, one, one) < 0) goto done;
    type = PyObject_CallFunction((PyObject *)&PyType_Type, "s(){}", "by_dict_kinds");
    if (type == NULL) goto done;
    instance = PyObject_CallNoArgs(type);
    if (instance == NULL || PyObject_SetAttr(instance, name, one) < 0) goto done;
    split = PyObject_GenericGetDict(instance, NULL);
    if (split == NULL || !PyDict_CheckExact(split)) goto done;
    held = By_DictKeysReadAs(text, 1) && By_DictKeysReadAs(number, 0)
           && By_DictKeysReadAs(split, 2);
done:
    if (PyErr_Occurred()) PyErr_Clear();
    Py_XDECREF(split);
    Py_XDECREF(instance);
    Py_XDECREF(type);
    Py_XDECREF(number);
    Py_XDECREF(text);
    Py_XDECREF(name);
    Py_XDECREF(one);
    by_dict_kinds_checked = held;
}

#else

static inline void By_CheckDictKinds(void) {}

#endif /* BY_DICT_KINDS */

/* `k in d` answered by the very lookup `d[k]` would go on to make
 *
 * the two hash the same key and walk the same table, so where the second is only
 * reached because the first said yes, one of them is doing the other's work again.
 * this asks once and reports both answers as one value: a new reference where the
 * key is there, and NULL where it is not. NULL with an exception set is failure —
 * the caller tells the two apart with `PyErr_Occurred`, the way it already does
 * for an exhausted iterator
 *
 * asking once has to be *earned* at runtime, because everything that would make
 * asking twice observable is ordinary python. a dict subclass may have overridden
 * `__contains__` or `__getitem__`, and then the number and order of those calls is
 * the program's own business. a key may have a `__hash__` that counts how often it is
 * called, and then hashing once where the source hashes twice is a different program.
 * and a key already *stored* in the dict, hashing like the probe, has its `__eq__`
 * asked by every probe that walks past it — which can count, or delete the key or
 * rewrite its value between the test and the read. so the single probe is taken only
 * for an exact dict keyed by an exact `str` whose table holds nothing but exact `str`
 * keys, where every hash and comparison is the interpreter's own; everything else
 * takes the protocol twice over, in the order it would have — and `__getitem__` only
 * where `__contains__` said yes, which is the branch this stands in for */
static inline PyObject *By_DictFind(PyObject *container, PyObject *key) {
    /* a null operand carries an exception already set by whatever produced it */
    if (container == NULL || key == NULL) return NULL;
#ifdef BY_DICT_KINDS
    if (BY_LIKELY(PyDict_CheckExact(container) && PyUnicode_CheckExact(key)
                  && By_DictKeysAllStr(container))) {
        PyObject *value;
        /* -1 failed, 0 absent, 1 there — and absent leaves `value` NULL */
        if (PyDict_GetItemRef(container, key, &value) < 0) return NULL;
        return value;
    }
#endif
    int found = PySequence_Contains(container, key);
    if (found <= 0) return NULL;
    return PyObject_GetItem(container, key);
}

/* what a class body *assigned* to a slot dunder, bound to the receiver
 *
 * a name in `tp_dict` does not fill a type slot: python reads `tp_repr` for `repr(x)`
 * and never consults the name. so a class writing `__repr__ = _repr` gets a slot of its
 * own that reaches the assigned value, and this is how that slot binds it — the way
 * python's own `slot_tp_repr` does, through the descriptor protocol. a `def` becomes a
 * bound method, a `staticmethod` unwraps to the plain function, a `classmethod` binds to
 * the type, and a callable that is not a descriptor at all is handed over as it stands
 * and so is called *without* a receiver, which is what python does with one too
 *
 * the value comes out of a cell module init filled from the type's dict, rather than out
 * of a lookup made here: a lookup would find the slot wrapper `PyType_Ready` writes for
 * this very slot in the window before the copy, and calling that would call back into
 * here forever */
static inline PyObject *By_BindSlotAlias(PyObject *value, PyObject *self) {
    descrgetfunc bind;
    if (value == NULL) {
        PyErr_SetString(PyExc_SystemError,
                        "a type slot was emitted for a name the class body never bound");
        return NULL;
    }
    bind = Py_TYPE(value)->tp_descr_get;
    if (bind == NULL) return By_NewRef(value);
    return bind(value, self, (PyObject *)Py_TYPE(self));
}

/* an assigned dunder called with the arguments its slot was handed */
static inline PyObject *By_CallSlotAlias(PyObject *value, PyObject *self,
                                         PyObject *const *argv, Py_ssize_t argc) {
    /* `PyObject_Vectorcall` reads no argument when there are none, but it is still handed
     * somewhere to read from rather than NULL */
    PyObject *empty = NULL;
    PyObject *bound = By_BindSlotAlias(value, self);
    PyObject *result;
    if (bound == NULL) return NULL;
    result = PyObject_Vectorcall(bound, argc > 0 ? argv : &empty, (size_t)argc, NULL);
    Py_DECREF(bound);
    return result;
}

/* the same, for the one slot handed a tuple and a dict rather than a vector */
static inline PyObject *By_CallSlotAliasTuple(PyObject *value, PyObject *self, PyObject *args,
                                              PyObject *kwargs) {
    PyObject *bound = By_BindSlotAlias(value, self);
    PyObject *result;
    if (bound == NULL) return NULL;
    result = PyObject_Call(bound, args, kwargs);
    Py_DECREF(bound);
    return result;
}

/* take the assigned value out of the type's dict and hold it for the slot to call
 *
 * run at module init, straight after `By_CopyClassConstant` has put the value there. an
 * absent name means the emitter wrote a slot for something the body never bound, which is
 * a defect in the emitter rather than anything the module can carry on from.
 *
 * the second refusal is the sharper one. `PyType_Ready` writes a slot wrapper into the
 * dict under the name of every slot the spec filled, so a copy that did not happen leaves
 * *this slot's own wrapper* sitting where the assigned value should be — and holding that
 * would make the slot call itself until the stack ran out. it is the same defect as an
 * absent name and it has to fail the same way, at import and by name, rather than on the
 * first `repr()` and as a `RecursionError` */
static inline int By_HoldSlotAlias(PyTypeObject *type, const char *name, PyObject **held) {
    PyObject *value = PyDict_GetItemString(type->tp_dict, name); /* borrowed */
    if (value == NULL) {
        PyErr_Format(PyExc_SystemError, "`%s.%s` fills a type slot and was never bound",
                     type->tp_name, name);
        return -1;
    }
    if (Py_IS_TYPE(value, &PyWrapperDescr_Type)
        && ((PyDescrObject *)value)->d_type == type) {
        PyErr_Format(PyExc_SystemError, "`%s.%s` fills a type slot with the slot itself",
                     type->tp_name, name);
        return -1;
    }
    Py_XSETREF(*held, By_NewRef(value));
    return 0;
}

/* the one attribute a field with a class-level value publishes
 *
 * `tag = None` standing beside a `self.tag = tag` is two answers under one name, and
 * python keeps them in two places: the class's own dict holds the class-level one, and an
 * instance that assigned holds its own in its dict, where it shadows the other. an emitted
 * instance has no dict — it is its layout — so both answers have to come out of the single
 * entry in the type's dict, and a plain `PyGetSetDef` cannot give them: `getset_get`
 * answers a read off the class with the *descriptor*, which is the convention for one and
 * exactly the wrong answer here.
 *
 * so the entry is a descriptor of our own. it holds neither answer. the class-level value
 * is the module's, in a cell beside the type; the instance's is in the layout, behind the
 * field's own getter. what this holds is how to choose — a question put to the presence
 * byte, which is what says whether the instance has an answer at all. `del` needs no case
 * here: the field's setter clears that same byte, so the instance goes back to reading the
 * class's value exactly as python's does when the entry leaves its dict */
typedef int (*By_FieldPresent)(PyObject *);

typedef struct {
    PyObject_HEAD
    getter by_get;
    /* NULL where the class publishes no setters at all, as a frozen one does */
    setter by_set;
    /* the module's cell for the class-level value. borrowed: the cell outlives every
     * type in the module, and a strong reference here would be a second owner of a value
     * nothing ever takes back */
    PyObject **by_value;
    /* emitted beside the getter, and reaching the byte the same way it does. a class
     * appending its storage past an outside base keeps that storage in a region the
     * instance pointer does not begin at, so an offset from the object would be reading
     * the base's own fields */
    By_FieldPresent by_present;
    const char *by_name;
    /* strongly held, and visited, for the reason every descriptor holds its type: the
     * type's own dict is what holds this, so the two are a cycle and only the collector
     * can take them apart */
    PyTypeObject *by_owner;
} By_FieldDefaultObject;

static PyObject *By_FieldDefault_get(PyObject *self, PyObject *object, PyObject *type) {
    By_FieldDefaultObject *field = (By_FieldDefaultObject *)self;
    (void)type;
    /* python's own convention: no object means the read was off the class */
    if (object == NULL) return By_NewRef(*field->by_value);
    if (!PyObject_TypeCheck(object, field->by_owner)) {
        PyErr_Format(PyExc_TypeError,
                     "descriptor '%s' for '%s' objects doesn't apply to a '%s' object",
                     field->by_name, field->by_owner->tp_name, Py_TYPE(object)->tp_name);
        return NULL;
    }
    if (!field->by_present(object)) return By_NewRef(*field->by_value);
    return field->by_get(object, NULL);
}

static int By_FieldDefault_set(PyObject *self, PyObject *object, PyObject *value) {
    By_FieldDefaultObject *field = (By_FieldDefaultObject *)self;
    if (field->by_set == NULL) {
        PyErr_Format(PyExc_AttributeError, "attribute '%s' of '%s' objects is not writable",
                     field->by_name, field->by_owner->tp_name);
        return -1;
    }
    return field->by_set(object, value, NULL);
}

static int By_FieldDefault_traverse(PyObject *self, visitproc visit, void *arg) {
    Py_VISIT((PyObject *)((By_FieldDefaultObject *)self)->by_owner);
    return 0;
}

static void By_FieldDefault_dealloc(PyObject *self) {
    PyObject_GC_UnTrack(self);
    Py_CLEAR(((By_FieldDefaultObject *)self)->by_owner);
    PyObject_GC_Del(self);
}

static PyTypeObject By_FieldDefaultType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.field_default",
    .tp_basicsize = sizeof(By_FieldDefaultObject),
    .tp_itemsize = 0,
    .tp_dealloc = By_FieldDefault_dealloc,
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC,
    .tp_traverse = By_FieldDefault_traverse,
    .tp_descr_get = By_FieldDefault_get,
    .tp_descr_set = By_FieldDefault_set,
};

/* publish the attribute a defaulted field answers through, replacing the plain copy
 *
 * `hold` is set for the class whose body wrote the value: `By_CopyClassConstant` has put
 * it in the type's dict already, and this takes it into the module's cell before the
 * descriptor goes over the top of it. a subclass that inherits the field passes zero and
 * shares the cell, which is what makes the base's value the one a subclass with nothing of
 * its own answers with */
static inline int By_HoldFieldDefault(PyTypeObject *type, const char *name, PyObject **cell,
                                      int hold, getter get, setter set,
                                      By_FieldPresent present) {
    By_FieldDefaultObject *field;
    if (hold) {
        PyObject *value = PyDict_GetItemString(type->tp_dict, name); /* borrowed */
        if (value == NULL) {
            PyErr_Format(PyExc_SystemError, "`%s.%s` has a class-level value and was never bound",
                         type->tp_name, name);
            return -1;
        }
        Py_XSETREF(*cell, By_NewRef(value));
    }
    if (PyType_Ready(&By_FieldDefaultType) < 0) return -1;
    field = PyObject_GC_New(By_FieldDefaultObject, &By_FieldDefaultType);
    if (field == NULL) return -1;
    field->by_get = get;
    field->by_set = set;
    field->by_value = cell;
    field->by_present = present;
    field->by_name = name;
    field->by_owner = (PyTypeObject *)By_NewRef((PyObject *)type);
    PyObject_GC_Track(field);
    if (PyDict_SetItemString(type->tp_dict, name, (PyObject *)field) < 0) {
        Py_DECREF(field);
        return -1;
    }
    Py_DECREF(field);
    PyType_Modified(type);
    return 0;
}

/* work out, once at import, whether `type` still answers `name` with the descriptor its
 * field `name` was published through — and record the version that held when it was
 *
 * a field is published either as a getset or, beside a class-level value, as a descriptor
 * of this runtime's own; either carries the field's getter, which is what is compared.
 * a type answering reads or writes with a hook of its own is refused, as an accessor is */
static inline void By_ArmField(ByAccessorLicence *licence, PyObject *type, const char *name,
                               getter get) {
    PyObject *key;
    PyObject *found;
    int published;
    licence->version = 0u;
    if (type == NULL || !PyType_Check(type)) return;
    PyTypeObject *owner = (PyTypeObject *)type;
    if (owner->tp_getattro != PyObject_GenericGetAttr) return;
    if (owner->tp_setattro != PyObject_GenericSetAttr) return;
    key = PyUnicode_InternFromString(name);
    if (key == NULL) {
        PyErr_Clear();
        return;
    }
    /* borrowed, and no descriptor is run to find it */
    found = _PyType_Lookup(owner, key);
    Py_DECREF(key);
    if (found == NULL) return;
    published = (Py_IS_TYPE(found, &PyGetSetDescr_Type)
                 && ((PyGetSetDescrObject *)found)->d_getset->get == get)
                || (Py_IS_TYPE(found, &By_FieldDefaultType)
                    && ((By_FieldDefaultObject *)found)->by_get == get);
    if (!published) return;
#if PY_VERSION_HEX >= 0x030C0000
    if (!PyUnstable_Type_AssignVersionTag(owner)) return;
#endif
    licence->version = owner->tp_version_tag;
}

#ifdef BY_LICENCE_RECHECK

/* re-ask the lookup a field read or written at its offset skipped: that the receiver is
 * exactly the class, the class answers reads and writes the generic way, and the name still
 * resolves on the class to the descriptor this module published for the field
 *
 * the descriptor is found without being run, as [`By_ArmField`] finds it */
static void By_RecheckField(PyObject *o, PyObject *type, const char *class_name,
                            const char *name, getter get) {
    if (o == NULL) {
        By_LicenceFailed(class_name, name, "the receiver is NULL", NULL);
        return;
    }
    if ((PyObject *)Py_TYPE(o) != type) {
        By_LicenceFailed(class_name, name, "the receiver is not this class",
                         Py_TYPE(o)->tp_name);
        return;
    }
    PyTypeObject *owner = (PyTypeObject *)type;
    if (owner->tp_getattro != PyObject_GenericGetAttr
        || owner->tp_setattro != PyObject_GenericSetAttr) {
        By_LicenceFailed(class_name, name,
                         "the class answers reads or writes with a hook of its own", NULL);
        return;
    }
    PyObject *key = PyUnicode_InternFromString(name);
    if (key == NULL) {
        PyErr_Clear();
        By_LicenceFailed(class_name, name, "the name could not be interned", NULL);
        return;
    }
    PyObject *found = _PyType_Lookup(owner, key);
    Py_DECREF(key);
    if (found == NULL) {
        By_LicenceFailed(class_name, name, "the name no longer resolves on the class", NULL);
        return;
    }
    int published = (Py_IS_TYPE(found, &PyGetSetDescr_Type)
                     && ((PyGetSetDescrObject *)found)->d_getset->get == get)
                    || (Py_IS_TYPE(found, &By_FieldDefaultType)
                        && ((By_FieldDefaultObject *)found)->by_get == get);
    if (!published) {
        By_LicenceFailed(class_name, name,
                         "the class no longer publishes the compiled field", Py_TYPE(found)->tp_name);
    }
}

#endif /* BY_LICENCE_RECHECK */

/* raise `KeyError(key)`, with the key as the one argument whatever it is
 *
 * `PyErr_SetObject` reads a tuple value as the whole argument list and an exception
 * instance as the exception itself, so handing it the key raised `KeyError(1, 2)` for a
 * missing `(1, 2)` and the key itself for a missing `KeyError('x')`. packing the key into
 * a tuple of one is what cpython's own dict does for the same reason */
static inline void By_RaiseKeyError(PyObject *key) {
    PyObject *args = PyTuple_Pack(1, key);
    if (args == NULL) return;
    PyErr_SetObject(PyExc_KeyError, args);
    Py_DECREF(args);
}

/* ── an emitted instance's `__dict__` ─────────────────────────────────────────
 *
 * an emitted instance keeps its attributes in two places. the ones the class itself
 * writes are the layout, at a fixed offset and with no name to look up; anything else
 * put on the object afterwards goes into the dict beside them, which is what makes
 * `obj.extra = 3` work the way the interpreted twin does.
 *
 * python's `__dict__` is one mapping over the whole of an object's state, so handing
 * back the second of those alone would name the *extra* attributes and none of the real
 * ones — an empty answer where the interpreted class gives a full one. that is a quiet
 * wrong answer, and worse than the refusal it would replace. a class whose whole state
 * is the dict has no such gap and answers with the dict itself.
 *
 * every other class answers with this: a `dict` **subclass** holding the whole mapping,
 * filled from the object when it is handed out, whose writes go back through the object
 * so that a name the layout knows reaches the layout.
 *
 * it has to be a real `dict`, and it has to carry the entries in the base's own storage.
 * `isinstance(obj.__dict__, dict)` gates a great deal of library code, and every reader
 * the C api offers — `json.dumps`, `==`, `copy()`, `PyDict_Next` — reads that storage and
 * ignores an override completely: a subclass answering out of a side table serialises as
 * `{}`, silently. so the entries are really there, and every inherited operation is
 * exactly `dict`'s.
 *
 * what that costs is liveness in one direction. the mapping is filled when `__dict__` is
 * read and refilled after every write through it, so anything that takes it and uses it
 * sees the object as it stands — but a *held* mapping does not see an attribute written
 * afterwards by some other means. keeping it live would mean the object telling it, and
 * the object writes its fields at a compile-time offset with nothing to hang that on
 */
typedef struct {
    const char *name;
    /* the field's own presence predicate, and NULL for every field that does not need
     * one. it is needed exactly where a class-level value stands beside the field:
     * reading the attribute then answers with the *class's* value when the instance
     * never wrote one, and python's `__dict__` does not name a class attribute. every
     * other field reports its own absence by raising, which the read below reads */
    By_FieldPresent present;
    /* the field's own setter, which also deletes, and NULL where the class publishes
     * none. a write through `__dict__` naming the field is a write of the field, and
     * reaching it through the setter rather than the attribute is what keeps a subclass's
     * `__setattr__` or a descriptor shadowing the name from running */
    setter set;
} By_DictField;

typedef struct {
    /* the base's own storage, and it must come first: everything reading this as a
     * `dict` reads from here */
    PyDictObject by_base;
    /* the object this mapping stands for, **borrowed**, and NULL once that object has
     * gone. it is borrowed because the object holds the mapping — it is the object's own
     * dict word — so a strong reference back would be a cycle nothing but the collector
     * could break. an object on its way out detaches the mapping first, and what is left
     * is an ordinary dict holding the state that stood at the end: which is exactly what
     * python leaves behind for `d = r.__dict__; del r` */
    PyObject *owner;
    /* static, and outlives every instance: it is the table the module emitted beside
     * the type */
    const By_DictField *fields;
} ByInstanceDictObject;

/* where an emitted instance keeps its dict, or NULL for a class whose instances keep
 * none. the word is written to as well as read: a published `__dict__` is installed here
 * and becomes the instance's own dict from then on */
static PyObject **By_InstanceDictSlot(PyObject *owner) {
    Py_ssize_t offset = Py_TYPE(owner)->tp_dictoffset;
    if (offset <= 0) return NULL;
    return (PyObject **)((char *)owner + offset);
}

/* the dict an emitted instance keeps beside its layout, borrowed, or NULL
 *
 * read rather than asked for, for the reason [`By_DictShadowsAt`] gives, and because
 * `PyObject_GenericGetDict` *creates* one — an instance that was only ever looked at
 * would come away carrying an empty dict it never needed */
static PyObject *By_InstanceExtras(PyObject *owner) {
    PyObject **slot = By_InstanceDictSlot(owner);
    return slot == NULL ? NULL : *slot;
}

/* one layout field's value: 1 with a new reference, 0 when the instance has none, and
 * -1 with an exception set
 *
 * the value comes from the attribute rather than from the field's getter, because the
 * attribute is where python's own precedence is applied — a class publishing no setters
 * leaves its fields as non-data descriptors, and a name written onto such an instance
 * goes to the dict and shadows the layout from then on */
static int By_InstanceDictField(PyObject *owner, const By_DictField *field, PyObject **out) {
    PyObject *value;
    *out = NULL;
    if (field->present != NULL && !field->present(owner)) return 0;
    value = PyObject_GetAttrString(owner, field->name);
    if (value == NULL) {
        if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return -1;
        PyErr_Clear();
        return 0;
    }
    *out = value;
    return 1;
}

/* the whole of an instance's state as a plain dict, built fresh
 *
 * the layout first and the extras after, because that is the order the object acquired
 * them: `__init__` writes the fields and anything else is put on afterwards. an extra
 * standing under a field's own name is already the answer the read above gave, so it is
 * not written twice and the field keeps its position */
static PyObject *By_InstanceMapping(PyObject *owner, const By_DictField *fields) {
    PyObject *out = PyDict_New();
    PyObject *extras;
    const By_DictField *field;
    if (out == NULL) return NULL;
    for (field = fields; field->name != NULL; field++) {
        PyObject *value;
        int failed;
        int held = By_InstanceDictField(owner, field, &value);
        if (held < 0) {
            Py_DECREF(out);
            return NULL;
        }
        if (held == 0) continue;
        failed = PyDict_SetItemString(out, field->name, value);
        Py_DECREF(value);
        if (failed < 0) {
            Py_DECREF(out);
            return NULL;
        }
    }
    extras = By_InstanceExtras(owner);
    if (extras != NULL) {
        Py_ssize_t pos = 0;
        PyObject *key, *value;
        Py_INCREF(extras);
        while (PyDict_Next(extras, &pos, &key, &value)) {
            int found = PyDict_Contains(out, key);
            if (found < 0 || (found == 0 && PyDict_SetItem(out, key, value) < 0)) {
                Py_DECREF(extras);
                Py_DECREF(out);
                return NULL;
            }
        }
        Py_DECREF(extras);
    }
    return out;
}

static PyTypeObject By_InstanceDictType;

/* what an emitted class answers `__dict__` with, and what it keeps in its dict word from
 * then on
 *
 * the mapping is *installed*, replacing whatever plain dict the extra attributes were
 * living in — which is what makes it the object's storage for everything the layout does
 * not name, so a name written on the object afterwards lands in this mapping with nothing
 * having to be told. the layout half is told: every write to a field publishes into here,
 * which is what a *held* mapping needs to keep answering with what the object holds now.
 *
 * asked twice it answers with the same object, exactly as python's own `__dict__` does */
static inline PyObject *By_InstanceDict(PyObject *owner, const By_DictField *fields) {
    PyObject **slot = By_InstanceDictSlot(owner);
    PyObject *standing;
    ByInstanceDictObject *self;
    if (slot != NULL && *slot != NULL && Py_IS_TYPE(*slot, &By_InstanceDictType)) {
        return By_NewRef(*slot);
    }
    if (By_InstanceDictType.tp_base == NULL) By_InstanceDictType.tp_base = &PyDict_Type;
    if (PyType_Ready(&By_InstanceDictType) < 0) return NULL;
    /* read before the mapping is installed, while the extras are still where they were */
    standing = By_InstanceMapping(owner, fields);
    if (standing == NULL) return NULL;
    self = (ByInstanceDictObject *)PyObject_CallNoArgs((PyObject *)&By_InstanceDictType);
    if (self == NULL) {
        Py_DECREF(standing);
        return NULL;
    }
    self->owner = owner;
    self->fields = fields;
    if (PyDict_Update((PyObject *)self, standing) < 0) {
        Py_DECREF(standing);
        Py_DECREF(self);
        return NULL;
    }
    Py_DECREF(standing);
    if (slot != NULL) {
        PyObject *previous = *slot;
        *slot = By_NewRef((PyObject *)self);
        Py_XDECREF(previous);
    } else {
        /* nowhere to install it, so nothing will tell it about a later write. a class
         * publishing a view always has the word, so this is the shape that never occurs */
        self->owner = NULL;
    }
    return (PyObject *)self;
}

static void By_InstanceDict_dealloc(PyObject *selfobj) {
    PyObject_GC_UnTrack(selfobj);
    PyDict_Type.tp_dealloc(selfobj);
}

static int By_InstanceDict_traverse(PyObject *selfobj, visitproc visit, void *arg) {
    /* the owner is borrowed, so it is not visited: the reference runs the other way */
    return PyDict_Type.tp_traverse(selfobj, visit, arg);
}

static int By_InstanceDict_clear(PyObject *selfobj) {
    return PyDict_Type.tp_clear(selfobj);
}

/* the layout field a key names, or NULL for a key the layout has no field for */
static const By_DictField *By_InstanceDictFieldNamed(ByInstanceDictObject *self, PyObject *key) {
    const By_DictField *field;
    if (!PyUnicode_Check(key)) return NULL;
    for (field = self->fields; field->name != NULL; field++) {
        if (PyUnicode_CompareWithASCIIString(key, field->name) == 0) return field;
    }
    return NULL;
}

/* one entry written into, or with a NULL value deleted from, the mapping an instance
 * answers `__dict__` with
 *
 * python's instance dict is a plain dict, so a write through it runs no descriptor and
 * no `__setattr__`: it is the entry and nothing else. a compiled instance keeps the names
 * its layout declares in fields rather than in the dict, so a key naming one writes the
 * field — through the field's own setter, which publishes what the layout stored back into
 * this mapping, and which checks the value's representation as any write of the field
 * does. every other key is an entry here, and this mapping is the object's dict word, so
 * an attribute read finds it exactly as it finds one python stored */
static int By_InstanceDict_assign(PyObject *selfobj, PyObject *key, PyObject *value) {
    ByInstanceDictObject *self = (ByInstanceDictObject *)selfobj;
    const By_DictField *field;
    /* detached, so there is no object to write back to and this is a plain dict */
    if (self->owner == NULL) {
        return value == NULL ? PyDict_DelItem(selfobj, key)
                             : PyDict_SetItem(selfobj, key, value);
    }
    field = By_InstanceDictFieldNamed(self, key);
    if (field == NULL) {
        return value == NULL ? PyDict_DelItem(selfobj, key)
                             : PyDict_SetItem(selfobj, key, value);
    }
    if (value == NULL) {
        /* a field the instance has no value in is a key the mapping does not hold */
        int held = PyDict_Contains(selfobj, key);
        if (held < 0) return -1;
        if (held == 0) {
            By_RaiseKeyError(key);
            return -1;
        }
    }
    if (field->set == NULL) {
        PyErr_Format(PyExc_AttributeError, "attribute '%s' of '%s' objects is not writable",
                     field->name, Py_TYPE(self->owner)->tp_name);
        return -1;
    }
    return field->set(self->owner, value, NULL);
}

/* one `key, value` pair of an update, as `By_InstanceDict_assign` writes it */
static int By_InstanceDictWritePair(PyObject *selfobj, PyObject *key, PyObject *value) {
    int failed;
    Py_INCREF(key);
    Py_INCREF(value);
    failed = By_InstanceDict_assign(selfobj, key, value);
    Py_DECREF(value);
    Py_DECREF(key);
    return failed;
}

/* `dict.update`'s positional argument, written one entry at a time in the order python's
 * own update writes them, so that an entry naming a field reaches the field
 *
 * a dict is walked as a dict, anything with `keys` is asked for its keys and then each
 * value, and anything else is taken as pairs — with python's wording for a pair that is
 * not one */
static int By_InstanceDictWriteAll(ByInstanceDictObject *self, PyObject *source) {
    PyObject *selfobj = (PyObject *)self;
    PyObject *keys_attr;
    if (self->owner == NULL) {
        PyObject *update = PyObject_GetAttrString((PyObject *)&PyDict_Type, "update");
        PyObject *done;
        if (update == NULL) return -1;
        done = PyObject_CallFunctionObjArgs(update, selfobj, source, NULL);
        Py_DECREF(update);
        Py_XDECREF(done);
        return done == NULL ? -1 : 0;
    }
    if (PyDict_Check(source)) {
        PyObject *items = PyDict_Items(source);
        Py_ssize_t index;
        if (items == NULL) return -1;
        for (index = 0; index < PyList_GET_SIZE(items); index++) {
            PyObject *pair = PyList_GET_ITEM(items, index);
            if (By_InstanceDictWritePair(selfobj, PyTuple_GET_ITEM(pair, 0),
                                         PyTuple_GET_ITEM(pair, 1)) < 0) {
                Py_DECREF(items);
                return -1;
            }
        }
        Py_DECREF(items);
        return 0;
    }
    keys_attr = PyObject_GetAttrString(source, "keys");
    if (keys_attr != NULL) {
        PyObject *keys = PyObject_CallNoArgs(keys_attr);
        PyObject *iterator;
        PyObject *key;
        int failed = 0;
        Py_DECREF(keys_attr);
        if (keys == NULL) return -1;
        iterator = PyObject_GetIter(keys);
        Py_DECREF(keys);
        if (iterator == NULL) return -1;
        while (!failed && (key = PyIter_Next(iterator)) != NULL) {
            PyObject *value = PyObject_GetItem(source, key);
            failed = value == NULL || By_InstanceDictWritePair(selfobj, key, value) < 0;
            Py_XDECREF(value);
            Py_DECREF(key);
        }
        Py_DECREF(iterator);
        return failed || PyErr_Occurred() ? -1 : 0;
    }
    if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return -1;
    PyErr_Clear();
    {
        PyObject *iterator = PyObject_GetIter(source);
        PyObject *item;
        Py_ssize_t at = 0;
        if (iterator == NULL) return -1;
        while ((item = PyIter_Next(iterator)) != NULL) {
            PyObject *pair = PySequence_Fast(item, "");
            int failed;
            if (pair == NULL) {
                if (PyErr_ExceptionMatches(PyExc_TypeError)) {
                    PyErr_Format(PyExc_TypeError,
                                 "cannot convert dictionary update sequence element #%zd to a sequence",
                                 at);
                }
                Py_DECREF(item);
                Py_DECREF(iterator);
                return -1;
            }
            if (PySequence_Fast_GET_SIZE(pair) != 2) {
                PyErr_Format(PyExc_ValueError,
                             "dictionary update sequence element #%zd has length %zd; 2 is required",
                             at, PySequence_Fast_GET_SIZE(pair));
                Py_DECREF(pair);
                Py_DECREF(item);
                Py_DECREF(iterator);
                return -1;
            }
            failed = By_InstanceDictWritePair(selfobj, PySequence_Fast_GET_ITEM(pair, 0),
                                              PySequence_Fast_GET_ITEM(pair, 1));
            Py_DECREF(pair);
            Py_DECREF(item);
            if (failed < 0) {
                Py_DECREF(iterator);
                return -1;
            }
            at++;
        }
        Py_DECREF(iterator);
        return PyErr_Occurred() ? -1 : 0;
    }
}

/* `update(other=(), /, **kwargs)`: the positional argument first, then each keyword */
static PyObject *By_InstanceDict_update(PyObject *selfobj, PyObject *args, PyObject *kwargs) {
    PyObject *other = NULL;
    if (!PyArg_UnpackTuple(args, "update", 0, 1, &other)) return NULL;
    if (other != NULL && By_InstanceDictWriteAll((ByInstanceDictObject *)selfobj, other) < 0) {
        return NULL;
    }
    if (kwargs != NULL && By_InstanceDictWriteAll((ByInstanceDictObject *)selfobj, kwargs) < 0) {
        return NULL;
    }
    Py_RETURN_NONE;
}

static PyObject *By_InstanceDict_ior(PyObject *selfobj, PyObject *other) {
    if (By_InstanceDictWriteAll((ByInstanceDictObject *)selfobj, other) < 0) return NULL;
    return By_NewRef(selfobj);
}

/* every name the object has, removed one at a time. a field the layout treats as always
 * defined refuses, which is what an emitted instance being its layout means — and it
 * refuses loudly rather than leaving half a mapping behind */
static PyObject *By_InstanceDict_clearmethod(PyObject *selfobj, PyObject *unused) {
    PyObject *keys;
    Py_ssize_t index;
    Py_ssize_t count;
    (void)unused;
    if (((ByInstanceDictObject *)selfobj)->owner == NULL) {
        PyDict_Clear(selfobj);
        Py_RETURN_NONE;
    }
    keys = PyDict_Keys(selfobj);
    if (keys == NULL) return NULL;
    count = PyList_GET_SIZE(keys);
    for (index = 0; index < count; index++) {
        if (By_InstanceDict_assign(selfobj, PyList_GET_ITEM(keys, index), NULL) < 0) {
            Py_DECREF(keys);
            return NULL;
        }
    }
    Py_DECREF(keys);
    Py_RETURN_NONE;
}

static PyObject *By_InstanceDict_pop(PyObject *selfobj, PyObject *const *args,
                                     Py_ssize_t nargs) {
    PyObject *value;
    if (nargs < 1 || nargs > 2) {
        PyErr_SetString(PyExc_TypeError, "pop expected 1 or 2 arguments");
        return NULL;
    }
    value = PyDict_GetItemWithError(selfobj, args[0]);
    if (value == NULL) {
        if (PyErr_Occurred()) return NULL;
        if (nargs == 2) return By_NewRef(args[1]);
        By_RaiseKeyError(args[0]);
        return NULL;
    }
    Py_INCREF(value);
    if (By_InstanceDict_assign(selfobj, args[0], NULL) < 0) {
        Py_DECREF(value);
        return NULL;
    }
    return value;
}

static PyObject *By_InstanceDict_popitem(PyObject *selfobj, PyObject *unused) {
    PyObject *items = PyDict_Items(selfobj);
    PyObject *last;
    Py_ssize_t count;
    (void)unused;
    if (items == NULL) return NULL;
    count = PyList_GET_SIZE(items);
    if (count == 0) {
        Py_DECREF(items);
        PyErr_SetString(PyExc_KeyError, "popitem(): dictionary is empty");
        return NULL;
    }
    last = By_NewRef(PyList_GET_ITEM(items, count - 1));
    Py_DECREF(items);
    if (By_InstanceDict_assign(selfobj, PyTuple_GET_ITEM(last, 0), NULL) < 0) {
        Py_DECREF(last);
        return NULL;
    }
    return last;
}

static PyObject *By_InstanceDict_setdefault(PyObject *selfobj, PyObject *const *args,
                                            Py_ssize_t nargs) {
    PyObject *value;
    PyObject *fallback;
    if (nargs < 1 || nargs > 2) {
        PyErr_SetString(PyExc_TypeError, "setdefault expected 1 or 2 arguments");
        return NULL;
    }
    value = PyDict_GetItemWithError(selfobj, args[0]);
    if (value != NULL) return By_NewRef(value);
    if (PyErr_Occurred()) return NULL;
    fallback = nargs == 2 ? args[1] : Py_None;
    if (By_InstanceDict_assign(selfobj, args[0], fallback) < 0) return NULL;
    return By_NewRef(fallback);
}

/* only the operations that *change* the mapping are written here. every reader is the
 * one `dict` already has, reading the storage this keeps filled — which is the whole
 * reason for being a `dict` at all */
static PyMethodDef By_InstanceDict_methods[] = {
    {"update", (PyCFunction)(void (*)(void))By_InstanceDict_update, METH_VARARGS | METH_KEYWORDS,
     NULL},
    {"clear", By_InstanceDict_clearmethod, METH_NOARGS, NULL},
    {"pop", (PyCFunction)(void (*)(void))By_InstanceDict_pop, METH_FASTCALL, NULL},
    {"popitem", By_InstanceDict_popitem, METH_NOARGS, NULL},
    {"setdefault", (PyCFunction)(void (*)(void))By_InstanceDict_setdefault, METH_FASTCALL, NULL},
    {NULL, NULL, 0, NULL},
};

/* only the subscript that assigns: `mp_length` and `mp_subscript` are left NULL and
 * inherited, which is what python's slot inheritance does with a half-filled table */
static PyMappingMethods By_InstanceDict_mapping = {
    .mp_ass_subscript = By_InstanceDict_assign,
};

static PyNumberMethods By_InstanceDict_number = {
    .nb_inplace_or = By_InstanceDict_ior,
};

static PyTypeObject By_InstanceDictType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.instance_dict",
    .tp_basicsize = sizeof(ByInstanceDictObject),
    .tp_itemsize = 0,
    .tp_dealloc = By_InstanceDict_dealloc,
    .tp_as_number = &By_InstanceDict_number,
    .tp_as_mapping = &By_InstanceDict_mapping,
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC,
    .tp_traverse = By_InstanceDict_traverse,
    .tp_clear = By_InstanceDict_clear,
    .tp_methods = By_InstanceDict_methods,
};

/* whether an instance's dict word holds a mapping that was handed out and so has to be
 * kept saying what the object says
 *
 * the word is NULL for an instance nobody has asked for a `__dict__` or written a stray
 * attribute on, which is nearly every instance, so the common answer costs one load and a
 * comparison — and the boxing a publish needs sits past this, so an unboxed field pays
 * nothing at all for a mapping that was never taken */
static inline int By_HasPublishedDict(PyObject *dict) {
    return dict != NULL && Py_IS_TYPE(dict, &By_InstanceDictType);
}

/* name in a published mapping what a layout field now holds, taking the reference it is
 * handed
 *
 * an emitted instance's storage is its layout and stays its layout: a field read is a
 * load at a compile-time offset, with nothing in front of it. what this keeps right is a
 * mapping somebody is *holding* — `__dict__` hands one out and installs it as the
 * object's dict word, and every reader the C api offers reads that mapping's own storage,
 * so a field written afterwards has to reach it.
 *
 * the value is the one the layout now holds rather than the one the write was handed: an
 * unboxed field keeps a representation of its own, and `x = 3` on a float field is `3.0`.
 *
 * a failure is not raised. the write this follows has already happened, so the object is
 * correct and only the mapping is behind — and a field write is not something the
 * exception model gives an edge to: a body the checker proved cannot raise has no error
 * path to jump to at all. the one thing here that can fail is an allocation, so it is
 * reported the way python reports a failing `__del__` — unraisably, on stderr, where it
 * cannot be read as the program's own answer */
static void By_PublishedField(PyObject *dict, const char *name, PyObject *value) {
    int failed = value == NULL || PyDict_SetItemString(dict, name, value) < 0;
    Py_XDECREF(value);
    if (failed) PyErr_WriteUnraisable(dict);
}

/* take a layout field out of a published mapping, for a `del` that put it back into the
 * state `tp_alloc` left it in
 *
 * a delete that is not published is the write's mistake read backwards: the mapping goes
 * on naming an attribute the object no longer has. a missing key is not a failure here */
static void By_UnpublishedField(PyObject *dict, const char *name) {
    if (PyDict_DelItemString(dict, name) < 0) PyErr_Clear();
}

/* let go of an instance's dict word, detaching a published mapping first
 *
 * a mapping somebody still holds outlives the object it stood for, and its pointer back
 * would dangle. cleared here, it goes on as an ordinary dict holding the last state the
 * object had — which is what python leaves behind in the same situation */
static void By_ReleaseInstanceDict(PyObject **slot) {
    PyObject *dict = *slot;
    if (dict == NULL) return;
    if (Py_IS_TYPE(dict, &By_InstanceDictType)) {
        ((ByInstanceDictObject *)dict)->owner = NULL;
    }
    *slot = NULL;
    Py_DECREF(dict);
}

/* `object.__getstate__` as python's own answers it for an instance holding slots, less the
 * one refusal it makes about the layout
 *
 * python pickles such an instance as a pair: its dict, or `None` where that is empty, and
 * the slots it has written, which `copyreg._slotnames` names. before that it checks that the
 * instance is no bigger than `object`, a dict, a weak-reference list and one word a slot —
 * and refuses the object otherwise, taking the rest for C state it cannot see. an emitted
 * instance can be bigger without holding any: a base keeps the word a subclass keeps its
 * dict in whether or not it has one itself, and a field that may be absent keeps a byte.
 * see `By_PublishSlottedState` for where this stands in for python's */
static PyObject *By_SlottedGetState(PyObject *self, PyObject *unused) {
    PyTypeObject *type = Py_TYPE(self);
    PyObject *state = Py_None, *copyreg, *names, *held, *pair;
    Py_ssize_t index;
    (void)unused;
    if (type->tp_dictoffset > 0) {
        PyObject *dict = *(PyObject **)((char *)self + type->tp_dictoffset);
        if (dict != NULL && PyDict_Check(dict) && PyDict_GET_SIZE(dict) > 0) state = dict;
    }
    Py_INCREF(state);
    copyreg = PyImport_ImportModule("copyreg");
    names = copyreg == NULL ? NULL
                            : PyObject_CallMethod(copyreg, "_slotnames", "O", (PyObject *)type);
    Py_XDECREF(copyreg);
    if (names == NULL || !PyList_Check(names)) {
        if (names != NULL) PyErr_SetString(PyExc_TypeError, "copyreg._slotnames didn't return a list");
        Py_XDECREF(names);
        Py_DECREF(state);
        return NULL;
    }
    held = PyDict_New();
    if (held == NULL) goto failed;
    for (index = 0; index < PyList_GET_SIZE(names); index++) {
        PyObject *name = PyList_GET_ITEM(names, index);
        PyObject *value = PyObject_GetAttr(self, name);
        if (value == NULL) {
            if (!PyErr_ExceptionMatches(PyExc_AttributeError)) goto failed;
            PyErr_Clear();
            continue;
        }
        if (PyDict_SetItem(held, name, value) < 0) {
            Py_DECREF(value);
            goto failed;
        }
        Py_DECREF(value);
    }
    Py_DECREF(names);
    if (PyDict_GET_SIZE(held) == 0) {
        Py_DECREF(held);
        return state;
    }
    pair = PyTuple_Pack(2, state, held);
    Py_DECREF(state);
    Py_DECREF(held);
    return pair;
failed:
    Py_DECREF(names);
    Py_XDECREF(held);
    Py_DECREF(state);
    return NULL;
}

static PyMethodDef By_SlottedGetStateDef = {"__getstate__", By_SlottedGetState, METH_NOARGS,
                                             NULL};

/* how many names `copyreg._slotnames` would list for `type`, or -1 with an exception set
 *
 * counted the way it walks them — every class in the mro whose own dict holds `__slots__`,
 * a string standing for one name, `__dict__` and `__weakref__` left out — rather than by
 * calling it: it caches its answer in the class's dict and needs `copyreg` imported, and
 * python does neither to a class until one of its instances is pickled */
static inline Py_ssize_t By_SlotNameCount(PyTypeObject *type) {
    PyObject *mro = type->tp_mro, *cached;
    Py_ssize_t index, count = 0;
    cached = PyDict_GetItemString(type->tp_dict, "__slotnames__");
    if (cached != NULL && PyList_Check(cached)) return PyList_GET_SIZE(cached);
    if (mro == NULL || !PyTuple_Check(mro)) return 0;
    for (index = 0; index < PyTuple_GET_SIZE(mro); index++) {
        PyObject *base = PyTuple_GET_ITEM(mro, index), *slots, *iterator, *name;
        if (!PyType_Check(base) || ((PyTypeObject *)base)->tp_dict == NULL) continue;
        slots = PyDict_GetItemString(((PyTypeObject *)base)->tp_dict, "__slots__");
        if (slots == NULL) continue;
        if (PyUnicode_Check(slots)) {
            count++;
            continue;
        }
        iterator = PyObject_GetIter(slots);
        if (iterator == NULL) return -1;
        while ((name = PyIter_Next(iterator)) != NULL) {
            if (!PyUnicode_Check(name)
                || (PyUnicode_CompareWithASCIIString(name, "__dict__") != 0
                    && PyUnicode_CompareWithASCIIString(name, "__weakref__") != 0)) {
                count++;
            }
            Py_DECREF(name);
        }
        Py_DECREF(iterator);
        if (PyErr_Occurred()) return -1;
    }
    return count;
}

/* whether `type` answers `name` with `object`'s own, or -1 with an exception set */
static inline int By_AnswersAsObject(PyObject *type, const char *name) {
    PyObject *standing = PyObject_GetAttrString(type, name);
    PyObject *object_s = standing == NULL
                             ? NULL
                             : PyObject_GetAttrString((PyObject *)&PyBaseObject_Type, name);
    int same = standing != NULL && standing == object_s;
    Py_XDECREF(standing);
    Py_XDECREF(object_s);
    if (PyErr_Occurred()) return -1;
    return same;
}

/* give `type` [`By_SlottedGetState`] where python's own `__getstate__` would refuse it
 *
 * only there. python makes the check on one path alone — `object.__reduce_ex__` asking for
 * the state of an object that is not a `list` or a `dict` and hands `__new__` nothing — so a
 * class that reduces itself, a container, and one with `__getnewargs__` are never refused,
 * and a direct call never is. where the layout is the size python expects, its own answer
 * is already the right one, and a class answering `__getstate__` with anything but
 * `object`'s — the source's own, or the one `@dataclass(frozen=True, slots=True)` writes —
 * keeps what it has */
static inline int By_PublishSlottedState(PyObject *type) {
    PyTypeObject *owner = (PyTypeObject *)type;
    static const char *const asked[] = {"__getstate__", "__reduce_ex__", "__reduce__", NULL};
    const char *const *name;
    PyObject *method;
    Py_ssize_t expected = PyBaseObject_Type.tp_basicsize, names;
    if (PyType_IsSubtype(owner, &PyList_Type) || PyType_IsSubtype(owner, &PyDict_Type)) return 0;
    for (name = asked; *name != NULL; name++) {
        int same = By_AnswersAsObject(type, *name);
        if (same <= 0) return same;
    }
    if (PyObject_HasAttrString(type, "__getnewargs_ex__")
        || PyObject_HasAttrString(type, "__getnewargs__")) {
        return 0;
    }
    names = By_SlotNameCount(owner);
    if (names < 0) return -1;
    expected += (Py_ssize_t)sizeof(PyObject *) * names;
    if (owner->tp_dictoffset != 0) expected += (Py_ssize_t)sizeof(PyObject *);
    if (owner->tp_weaklistoffset > 0) expected += (Py_ssize_t)sizeof(PyObject *);
    if (owner->tp_basicsize <= expected) return 0;
    method = PyDescr_NewMethod(owner, &By_SlottedGetStateDef);
    if (method == NULL) return -1;
    if (PyDict_SetItemString(owner->tp_dict, "__getstate__", method) < 0) {
        Py_DECREF(method);
        return -1;
    }
    Py_DECREF(method);
    PyType_Modified(owner);
    return 0;
}

/* `object.__getstate__`, over the whole of an instance's state
 *
 * python's own reads the dict word straight out of the instance, which on an emitted one
 * is the half holding the *extra* attributes — so `copy` and `pickle` are handed a state
 * naming none of the class's own fields, and handed it quietly. `None` is python's own
 * answer where there is nothing in it.
 *
 * `slots` names what the class holds in a slot, as `copyreg._slotnames` lists it, and
 * NULL where it holds nothing there. python hands those over beside the mapping, as a
 * pair, wherever the instance has one written; one it has not is left out */
static inline PyObject *By_InstanceState(PyObject *owner, const By_DictField *fields,
                                         const char *const *slots) {
    PyObject *state = By_InstanceMapping(owner, fields);
    PyObject *held, *pair;
    if (state == NULL) return NULL;
    if (PyDict_Size(state) == 0) {
        Py_SETREF(state, By_NewRef(Py_None));
    }
    if (slots == NULL) return state;
    held = PyDict_New();
    if (held == NULL) {
        Py_DECREF(state);
        return NULL;
    }
    for (; *slots != NULL; slots++) {
        PyObject *value = PyObject_GetAttrString(owner, *slots);
        if (value == NULL) {
            if (!PyErr_ExceptionMatches(PyExc_AttributeError)) goto failed;
            PyErr_Clear();
            continue;
        }
        if (PyDict_SetItemString(held, *slots, value) < 0) {
            Py_DECREF(value);
            goto failed;
        }
        Py_DECREF(value);
    }
    if (PyDict_Size(held) == 0) {
        Py_DECREF(held);
        return state;
    }
    pair = PyTuple_Pack(2, state, held);
    Py_DECREF(state);
    Py_DECREF(held);
    return pair;
failed:
    Py_DECREF(state);
    Py_DECREF(held);
    return NULL;
}

/* `obj.__dict__ = mapping`, which python takes as replacing the whole of an object's
 * state — so every name the object has and the mapping does not is removed first. a
 * field the layout treats as always defined cannot be removed and says so */
static inline int By_InstanceDictReplace(PyObject *owner, const By_DictField *fields,
                                         PyObject *value) {
    PyObject *standing;
    PyObject *keys;
    Py_ssize_t index;
    Py_ssize_t count;
    int failed = 0;
    if (value == NULL) {
        PyErr_Format(PyExc_TypeError, "cannot delete __dict__ of a '%s' object",
                     Py_TYPE(owner)->tp_name);
        return -1;
    }
    if (!PyMapping_Check(value)) {
        PyErr_Format(PyExc_TypeError, "__dict__ must be set to a mapping, not '%s'",
                     Py_TYPE(value)->tp_name);
        return -1;
    }
    standing = By_InstanceMapping(owner, fields);
    if (standing == NULL) return -1;
    keys = PyDict_Keys(standing);
    Py_DECREF(standing);
    if (keys == NULL) return -1;
    count = PyList_GET_SIZE(keys);
    for (index = 0; index < count && !failed; index++) {
        PyObject *key = PyList_GET_ITEM(keys, index);
        int held = PySequence_Contains(value, key);
        if (held < 0 || (held == 0 && PyObject_DelAttr(owner, key) < 0)) failed = 1;
    }
    Py_DECREF(keys);
    if (!failed) {
        PyObject *view = By_InstanceDict(owner, fields);
        if (view == NULL) return -1;
        failed = By_InstanceDictWriteAll((ByInstanceDictObject *)view, value) < 0;
        Py_DECREF(view);
    }
    return failed ? -1 : 0;
}

/* the `Py_hash_t` python makes of what a written `__hash__` answered
 *
 * `slot_tp_hash`'s own conversion, and pointedly not `PyObject_Hash`. a value that fits a
 * `Py_ssize_t` is taken as it stands; only one too large for that is folded, through
 * `int.__hash__`, into the range a hash occupies. hashing every answer would fold the
 * large ones twice — `_pydatetime.timedelta` caches a hash of its state tuple and hands
 * that back, and folding it a second time moved every value past 2**61 - 1 to one the
 * interpreted class never produced.
 *
 * `-1` is how a slot reports a failure, so python moves an answer of `-1` to `-2` */
static inline Py_hash_t By_HashResult(PyObject *value) {
    if (!PyLong_Check(value)) {
        PyErr_SetString(PyExc_TypeError, "__hash__ method should return an integer");
        return -1;
    }
    Py_hash_t hash = (Py_hash_t)PyLong_AsSsize_t(value);
    if (hash == -1 && PyErr_Occurred()) {
        PyErr_Clear();
        hash = PyLong_Type.tp_hash(value);
    }
    if (hash == -1) hash = -2;
    return hash;
}

/* publish a written `__new__` by *assigning* it onto the finished type
 *
 * not through the spec. a `tp_new` filled from a slot table is a C function, and python
 * reads one of those as a base that owns the allocation: `tp_new_wrapper` walks up from
 * the class looking for the allocator, stops at ours, and refuses `object.__new__(cls)`
 * as unsafe — which is how nearly every written `__new__` gets the instance it fills in.
 *
 * assigning is what a `class` statement does. `type_setattro` runs python's own slot
 * fixup, which sees a `__new__` in the dict and installs the dispatcher that looks the
 * name back up on every construction. the class then holds exactly the `tp_new` an
 * interpreted one holds, the allocation check walks past it to `object`, and the body's
 * `object.__new__(cls)` is the plain allocation it was written as.
 *
 * the wrapper is bound as a `staticmethod` because that is what python makes `__new__`:
 * the class arrives as the first argument rather than as a receiver, and the dispatcher
 * puts it there */
static inline int By_PublishNew(PyObject *type, PyMethodDef *def) {
    PyObject *function = PyCFunction_NewEx(def, NULL, NULL);
    if (function == NULL) return -1;
    PyObject *published = PyStaticMethod_New(function);
    Py_DECREF(function);
    if (published == NULL) return -1;
    int stored = PyObject_SetAttrString(type, "__new__", published);
    Py_DECREF(published);
    return stored;
}

/* publish the `__new__` that hands an interpreted subclass the allocator of the layout it
 * extends, while the class itself keeps `object`'s `tp_new`
 *
 * the class's own construction needs nothing from it: `object`'s `tp_new` allocates through
 * the class's own `tp_alloc`, which is the one that marks the fields absent — and keeping
 * `object`'s is what lets `object.__new__(cls)` allocate the class at all, since python
 * refuses that for a class whose `tp_new` is a C function of its own. the entry goes
 * straight into the dict rather than through an assignment, which would install the
 * dispatcher on the class itself.
 *
 * a subclass is what the entry is for. the `class` statement that makes one finds a
 * `__new__` among its bases and installs the dispatcher that looks the name up, so every
 * construction of the subclass comes through here and is handed the allocator first */
static inline int By_PublishAllocatingNew(PyObject *type, PyMethodDef *def) {
    PyTypeObject *owner = (PyTypeObject *)type;
    PyObject *function = PyCFunction_NewEx(def, NULL, NULL);
    if (function == NULL) return -1;
    PyObject *published = PyStaticMethod_New(function);
    Py_DECREF(function);
    if (published == NULL) return -1;
    int stored = PyDict_SetItemString(owner->tp_dict, "__new__", published);
    Py_DECREF(published);
    if (stored == 0) PyType_Modified(owner);
    return stored;
}

/* the class a published `__new__` is asked to allocate, refused where python's own
 * `__new__` refuses it: missing, not a class, or not built on `owner` */
static inline PyTypeObject *By_NewTarget(PyObject *owner, PyObject *const *args, Py_ssize_t nargs) {
    const char *name = ((PyTypeObject *)owner)->tp_name;
    if (nargs < 1) {
        PyErr_Format(PyExc_TypeError, "%s.__new__(): not enough arguments", name);
        return NULL;
    }
    if (!PyType_Check(args[0])) {
        PyErr_Format(PyExc_TypeError, "%s.__new__(X): X is not a type object (%s)", name,
                     Py_TYPE(args[0])->tp_name);
        return NULL;
    }
    PyTypeObject *subtype = (PyTypeObject *)args[0];
    if (!PyType_IsSubtype(subtype, (PyTypeObject *)owner)) {
        PyErr_Format(PyExc_TypeError, "%s.__new__(%s): %s is not a subtype of %s", name,
                     subtype->tp_name, subtype->tp_name, name);
        return NULL;
    }
    return subtype;
}

/* `obj.__weakref__`: the head of the weak references made of the object, or `None`
 *
 * what python's own descriptor answers, reached through the offset the type was built
 * with rather than a member of one struct, so every emitted class can publish the one
 * getter */
static PyObject *By_GetWeakrefList(PyObject *obj, void *closure) {
    (void)closure;
    Py_ssize_t offset = Py_TYPE(obj)->tp_weaklistoffset;
    PyObject *head = offset > 0 ? *(PyObject **)((char *)obj + offset) : NULL;
    return Py_NewRef(head != NULL ? head : Py_None);
}

/* hand an interpreted subclass the allocator of the emitted layout it extends
 *
 * an emitted class whose `int` fields start absent writes the absent value from its own
 * `tp_alloc`, because a zeroed `int` field is the `int` zero. python does not let a
 * subclass inherit that allocator: `type.__new__` gives every class it makes
 * `PyType_GenericAlloc`, so `S.__new__(S)` handed back a block whose fields read as `0`
 * where python raises `AttributeError`. the emitted allocator asks its base's for the block
 * and only then writes the absent values, so for the subclass it is the generic allocation
 * plus those writes.
 *
 * a class that wrote an allocator of its own is left with it, as is one that is not built
 * on `owner` at all, which the call it arrived through is about to refuse */
static inline void By_AdoptAllocator(PyTypeObject *type, PyObject *owner, allocfunc alloc) {
    if (type->tp_alloc == PyType_GenericAlloc && type != (PyTypeObject *)owner
        && PyType_IsSubtype(type, (PyTypeObject *)owner)) {
        type->tp_alloc = alloc;
    }
}

/* publish the `__init_subclass__` that hands every interpreted subclass the allocator of
 * the layout it extends, the moment the subclass is made
 *
 * the published `__new__` does that for an instance made through the class, but
 * `object.__new__(cls)` reaches the subclass's own allocator without passing it, and until
 * the subclass had been constructed once that allocator was the generic one: an unset `int`
 * field read as `0`. `type.__new__` asks the bases for `__init_subclass__` whenever it makes
 * a class, and the answer is looked up through the mro, so a subclass that writes one of
 * its own and chains up with `super()` still reaches this.
 *
 * it is wrapped as the `classmethod` a class statement makes of an `__init_subclass__`, and
 * fills no slot, so nothing else about the type changes by its being there */
static inline int By_PublishInitSubclass(PyObject *type, PyMethodDef *def) {
    PyTypeObject *owner = (PyTypeObject *)type;
    PyObject *function = PyCFunction_NewEx(def, NULL, NULL);
    if (function == NULL) return -1;
    PyObject *published = PyClassMethod_New(function);
    Py_DECREF(function);
    if (published == NULL) return -1;
    int stored = PyDict_SetItemString(owner->tp_dict, "__init_subclass__", published);
    Py_DECREF(published);
    if (stored == 0) PyType_Modified(owner);
    return stored;
}

/* the body of a published `__init_subclass__`: hand `cls` the allocator, then go on up the
 * chain exactly as a written one calling `super().__init_subclass__(...)` would, with every
 * argument it was given — so a keyword no class takes is refused in python's own words */
static PyObject *By_InitSubclass(PyObject *owner, allocfunc alloc, PyObject *const *args,
                                 Py_ssize_t nargsf, PyObject *kwnames) {
    Py_ssize_t nargs = PyVectorcall_NARGS(nargsf);
    PyObject *cls;
    PyObject *above;
    PyObject *next;
    PyObject *result;
    if (nargs < 1 || !PyType_Check(args[0])) {
        PyErr_Format(PyExc_TypeError, "%s.__init_subclass__() needs the class it is made for",
                     ((PyTypeObject *)owner)->tp_name);
        return NULL;
    }
    cls = args[0];
    By_AdoptAllocator((PyTypeObject *)cls, owner, alloc);
    above = PyObject_CallFunctionObjArgs((PyObject *)&PySuper_Type, owner, cls, NULL);
    if (above == NULL) return NULL;
    next = PyObject_GetAttrString(above, "__init_subclass__");
    Py_DECREF(above);
    if (next == NULL) return NULL;
    result = PyObject_Vectorcall(next, args + 1, nargs - 1, kwnames);
    Py_DECREF(next);
    return result;
}

/* publish a `@property` as the object python builds out of its halves
 *
 * not as a `tp_getset` entry, which this was. a getset is two function pointers, and what
 * python hands back for one is a `getset_descriptor` — so `C.value.fget` raises where the
 * interpreted class answers with the getter, and so do `.fset`, `.getter(...)`,
 * `.setter(...)` and `isinstance(C.value, property)`. everything that treats a property
 * as an attribute agreed already; everything that treats it as an *object* did not. the
 * price of the real object is one more call per access, because the property dispatches
 * to the half rather than the getset going straight to C.
 *
 * each half is a `PyDescr_NewMethod`, which is exactly what every other method of this
 * class already is: it carries the same `__name__` and `__qualname__` a sibling method
 * carries, and takes the receiver as its first argument the same way. a half the class
 * has no body for arrives NULL, and `property` reads that as the half being absent —
 * which is what raises python's own wording for a missing setter or deleter.
 *
 * no fourth argument, deliberately: `property` takes its `__doc__` off the getter exactly
 * when it is passed none, which is what a class body gets.
 *
 * `__set_name__` is called because `type.__new__` calls it on every value a class body
 * leaves behind, and nothing here has run that. it is not decoration: it is the only
 * thing that tells a property its own name before 3.13, which added a fallback to the
 * getter's. without the call, 3.11 and 3.12 report a missing half as "property of 'C'
 * object has no setter" — the name dropped out of the middle of python's own message.
 *
 * the property goes straight into `tp_dict` rather than through `setattr`, because a
 * class nothing mutates is sealed against `setattr` and this is the class's own
 * definition being written rather than an outside change to it. `PyType_Modified` is what
 * stops the attribute cache going on serving what the type held before.
 *
 * two constructions publish nothing, and the name is what tells them apart from the one
 * that does. a spec is built out of the method table, which a property's halves are not in,
 * so the type a spec produced holds nothing under this name and this is the only thing that
 * will ever put one there. the other two arrive with the name already answered:
 *
 * - a construction that fell back to the interpreted definition, which is the same test
 *   `By_DecoratedMethod` makes and here is not merely redundant work being skipped. the
 *   halves are this module's own bodies, which read the instance as the struct *this*
 *   module lays out; the twin's instances stop where python's do, so a half published onto
 *   the twin would read a field past the end of the object
 * - a construction through the metaclass, which was handed the `property` the interpreted
 *   body built — see `carried_off_the_body` for why it has to be. that object is what the
 *   class statement itself would have left under the name, so it is the more faithful of
 *   the two answers and it stays
 *
 * so a group on a class python's own metaclass machinery built keeps running interpreted.
 * that is the same answer it had before any of this, and it is reached without the class
 * having to decline.
 *
 * which of the two it was is only known here, at import: whether a base's metaclass is
 * `type` is a question about what the base's name meant. so each property writes a census
 * row of its own — `property-compiled` or `property-interpreted` under `Class.name`, see
 * `By_RecordProperty` — and the report's count of compiled halves is held to that */
static inline int By_PublishProperty(const char *module, PyObject *type, PyObject *module_dict,
                                     const char *owner, const char *name, PyMethodDef *get,
                                     PyMethodDef *set, PyMethodDef *del) {
    PyMethodDef *defs[3];
    PyObject *halves[3];
    PyObject *published, *named, *dict;
    int at, stored;
    if (type == PyDict_GetItemString(module_dict, owner)
        || (PyType_Check(type) && ((PyTypeObject *)type)->tp_dict != NULL
            && PyDict_GetItemString(((PyTypeObject *)type)->tp_dict, name) != NULL)) {
        By_RecordProperty(module, owner, name, "property-interpreted");
        return 0;
    }
    defs[0] = get;
    defs[1] = set;
    defs[2] = del;
    for (at = 0; at < 3; at++) {
        if (defs[at] == NULL) {
            halves[at] = Py_NewRef(Py_None);
            continue;
        }
        halves[at] = PyDescr_NewMethod((PyTypeObject *)type, defs[at]);
        if (halves[at] == NULL) {
            while (at-- > 0) Py_DECREF(halves[at]);
            return -1;
        }
    }
    published = PyObject_CallFunctionObjArgs((PyObject *)&PyProperty_Type, halves[0], halves[1],
                                             halves[2], NULL);
    for (at = 0; at < 3; at++) Py_DECREF(halves[at]);
    if (published == NULL) return -1;
    named = PyObject_CallMethod(published, "__set_name__", "Os", type, name);
    if (named == NULL) {
        Py_DECREF(published);
        return -1;
    }
    Py_DECREF(named);
    dict = ((PyTypeObject *)type)->tp_dict;
    if (dict == NULL) {
        PyErr_Format(PyExc_SystemError, "type '%s' has no dict to publish '%s' into",
                     ((PyTypeObject *)type)->tp_name, name);
        Py_DECREF(published);
        return -1;
    }
    stored = PyDict_SetItemString(dict, name, published);
    Py_DECREF(published);
    PyType_Modified((PyTypeObject *)type);
    if (stored == 0) By_RecordProperty(module, owner, name, "property-compiled");
    return stored;
}

/* the comparison this class did not write, answered where python would have answered it
 *
 * one `tp_richcompare` backs all six comparisons, so a class writing `__lt__` takes the
 * slot over from its base for the other five as well — and the base's is not empty.
 * `object`'s is what gives `!=` its default meaning: call `__eq__` and negate the answer.
 * returning `NotImplemented` for the five a body did not write threw that away, and
 * `Ordered(1) != Ordered(1)` answered `True` compiled where the interpreted class
 * answers `False`.
 *
 * the base is taken from the type the slot was emitted for rather than from
 * `Py_TYPE(self)`: a subclass that writes no comparison of its own inherits this very
 * function, and what it has to fall back to is the step up from *here* — reading the
 * instance's own type would hand it straight back to itself.
 *
 * `object_richcompare` answers `!=` by calling `Py_TYPE(self)->tp_richcompare` again with
 * `Py_EQ`, which is this function once more. that terminates: `Py_EQ` is either one this
 * class wrote, or another step up, and the walk is strictly upwards */
static inline PyObject *By_BaseRichCompare(PyObject *type, PyObject *self, PyObject *other,
                                           int op) {
    PyTypeObject *base = ((PyTypeObject *)type)->tp_base;
    if (base == NULL || base->tp_richcompare == NULL) Py_RETURN_NOTIMPLEMENTED;
    return base->tp_richcompare(self, other, op);
}

/* drop the names a filled slot published that the class body never wrote
 *
 * `PyType_Ready` adds a wrapper descriptor for *every* name a filled slot backs, so a
 * class writing `__lt__` publishes all six comparisons and one writing `__add__`
 * publishes `__radd__` alongside it. what reads a class by name then sees methods the
 * `class` statement never wrote. `functools.total_ordering` is the shape that shows what
 * that costs: it looks for the comparisons a class is missing, found none missing, filled
 * none in, and said nothing — and the first `<=` raised.
 *
 * this removes behaviour from nothing. python reaches a comparison through the slot,
 * which is untouched, and the name an attribute lookup now finds is the one an
 * interpreted class of the same body would have found */
static inline int By_UnpublishSlotNames(PyObject *type, const char *const *names) {
    PyObject *dict = ((PyTypeObject *)type)->tp_dict;
    Py_ssize_t at;
    if (dict == NULL) {
        PyErr_Format(PyExc_SystemError, "type '%s' has no dict to unpublish from",
                     ((PyTypeObject *)type)->tp_name);
        return -1;
    }
    for (at = 0; names[at] != NULL; at++) {
        PyObject *key = PyUnicode_FromString(names[at]);
        int held;
        if (key == NULL) return -1;
        held = PyDict_Contains(dict, key);
        if (held < 0 || (held && PyDict_DelItem(dict, key) < 0)) {
            Py_DECREF(key);
            return -1;
        }
        Py_DECREF(key);
    }
    /* the attribute cache would otherwise go on serving the wrappers just removed */
    PyType_Modified((PyTypeObject *)type);
    return 0;
}

/* say `__hash__ = None` under the name as well as in the slot
 *
 * a class python makes unhashable carries two things: `tp_hash` is
 * `PyObject_HashNotImplemented`, and `__hash__` in the dict is `None`. an emitted type
 * fills the slot and used to leave the name to `PyType_Ready`, which writes the `None`
 * itself — but it decides to by comparing the slot's value against its own
 * `PyObject_HashNotImplemented`, and a module that reaches that function through an
 * import stub does not hand it the address it is comparing against. the comparison then
 * fails, a wrapper descriptor over the stub is published instead of the `None`, and
 * `C.__hash__ is None` answers False where the interpreted class answers True. that is
 * what a compiled `data class` did on windows.
 *
 * `hash(x)` raises either way, so what this settles is only what the *name* answers —
 * which is what `@dataclass`, `copy`, and anything asking a class whether it is hashable
 * actually read. writing it here says it outright rather than hoping it is inferred */
static inline int By_PublishNoHash(PyObject *type) {
    PyObject *dict = ((PyTypeObject *)type)->tp_dict;
    if (dict == NULL) {
        PyErr_Format(PyExc_SystemError, "type '%s' has no dict to write `__hash__` into",
                     ((PyTypeObject *)type)->tp_name);
        return -1;
    }
    if (PyDict_SetItemString(dict, "__hash__", Py_None) < 0) return -1;
    /* the attribute cache would otherwise go on serving whatever the name held */
    PyType_Modified((PyTypeObject *)type);
    return 0;
}

/* a `tp_call` slot is handed a tuple and a dict where a method wrapper wants a
 * vector, so the arguments are laid out flat and the keyword names follow — the
 * shape `PyObject_Vectorcall` uses, built here once per call */
static inline PyObject *By_CallSlot(
    PyObject *(*wrapper)(PyObject *, PyObject *const *, Py_ssize_t, PyObject *), PyObject *self,
    PyObject *args, PyObject *kwargs) {
    Py_ssize_t positional = PyTuple_GET_SIZE(args);
    Py_ssize_t named = kwargs == NULL ? 0 : PyDict_Size(kwargs);
    PyObject **flat = PyMem_Malloc(sizeof(PyObject *) * (size_t)(positional + named + 1));
    if (flat == NULL) return PyErr_NoMemory();
    for (Py_ssize_t i = 0; i < positional; i++) flat[i] = PyTuple_GET_ITEM(args, i);
    PyObject *names = NULL;
    if (named > 0) {
        names = PyTuple_New(named);
        if (names == NULL) {
            PyMem_Free(flat);
            return NULL;
        }
        Py_ssize_t position = 0, index = 0;
        PyObject *key, *value;
        while (PyDict_Next(kwargs, &position, &key, &value)) {
            PyTuple_SET_ITEM(names, index, By_NewRef(key));
            flat[positional + index] = value;
            index++;
        }
    }
    PyObject *result = wrapper(self, flat, positional, names);
    Py_XDECREF(names);
    PyMem_Free(flat);
    return result;
}

/* an async generator's frame finishing is `StopAsyncIteration`, not `StopIteration`
 *
 * the resume method raises the latter, because that is what a generator's exhaustion
 * *is* — the surface is what differs, so the conversion happens here rather than in
 * the state machine */
static inline PyObject *By_EndAsyncIteration(void) {
    if (PyErr_ExceptionMatches(PyExc_StopIteration)) {
        PyErr_Clear();
        PyErr_SetNone(PyExc_StopAsyncIteration);
    }
    return NULL;
}

static inline PyObject *By_GetAttr(PyObject *o, PyObject *name) {
    if (o == NULL || name == NULL) return NULL;
    return PyObject_GetAttr(o, name);
}

static inline char By_SetAttr(PyObject *o, PyObject *name, PyObject *value) {
    if (o == NULL || name == NULL) return 2;
    return PyObject_SetAttr(o, name, value) < 0 ? 2 : 0;
}

/* the arm a licensed field or property access takes where its licence does not stand: the
 * attribute through the object protocol, narrowed to the representation the compiled
 * access holds or boxed from it
 *
 * one call kept out of line, because it is the rare arm of a test the hot path asks every
 * time. written as a lookup and a narrowing apiece, the arm gave its function a register, a
 * retain, a release and two error tests, and the C compiler stopped writing a two-line
 * accessor out in place. the name is interned into `*slot` the first time */
static inline PyObject *By_UnboxStr(PyObject *o);

BY_COLD PyObject *By_ReadAttrObject(PyObject *o, PyObject **slot, const char *text,
                                    Py_ssize_t length) {
    if (*slot == NULL) *slot = By_InternedStr(text, length);
    if (*slot == NULL) return NULL;
    return By_GetAttr(o, *slot);
}

BY_COLD ByTagged By_ReadAttrInt(PyObject *o, PyObject **slot, const char *text,
                                Py_ssize_t length) {
    PyObject *found = By_ReadAttrObject(o, slot, text, length);
    if (found == NULL) return BY_INT_ERROR;
    ByTagged narrowed = By_UnboxInt(found);
    Py_DECREF(found);
    return narrowed;
}

BY_COLD double By_ReadAttrFloat(PyObject *o, PyObject **slot, const char *text,
                                Py_ssize_t length) {
    PyObject *found = By_ReadAttrObject(o, slot, text, length);
    if (found == NULL) return BY_FLOAT_ERROR;
    double narrowed = By_UnboxFloat(found);
    Py_DECREF(found);
    return narrowed;
}

BY_COLD char By_ReadAttrBool(PyObject *o, PyObject **slot, const char *text,
                             Py_ssize_t length) {
    PyObject *found = By_ReadAttrObject(o, slot, text, length);
    if (found == NULL) return 2;
    char narrowed = By_UnboxBool(found);
    Py_DECREF(found);
    return narrowed;
}

BY_COLD PyObject *By_ReadAttrStr(PyObject *o, PyObject **slot, const char *text,
                                 Py_ssize_t length) {
    PyObject *found = By_ReadAttrObject(o, slot, text, length);
    if (found == NULL) return NULL;
    PyObject *narrowed = By_UnboxStr(found);
    Py_DECREF(found);
    return narrowed;
}

BY_COLD char By_WriteAttrObject(PyObject *o, PyObject **slot, const char *text,
                                Py_ssize_t length, PyObject *value) {
    if (*slot == NULL) *slot = By_InternedStr(text, length);
    if (*slot == NULL) return 2;
    return By_SetAttr(o, *slot, value);
}

/* a value boxed, handed to the protocol and let go of again, as the boxing and the store
 * would have been */
BY_COLD char By_WriteAttrBoxed(PyObject *o, PyObject **slot, const char *text,
                               Py_ssize_t length, PyObject *boxed) {
    if (boxed == NULL) return 2;
    char refused = By_WriteAttrObject(o, slot, text, length, boxed);
    Py_DECREF(boxed);
    return refused;
}

BY_COLD char By_WriteAttrInt(PyObject *o, PyObject **slot, const char *text,
                             Py_ssize_t length, ByTagged value) {
    return By_WriteAttrBoxed(o, slot, text, length, By_BoxInt(value));
}

BY_COLD char By_WriteAttrFloat(PyObject *o, PyObject **slot, const char *text,
                               Py_ssize_t length, double value) {
    return By_WriteAttrBoxed(o, slot, text, length, By_BoxFloat(value));
}

BY_COLD char By_WriteAttrBool(PyObject *o, PyObject **slot, const char *text,
                              Py_ssize_t length, char value) {
    return By_WriteAttrBoxed(o, slot, text, length, By_BoxBool(value));
}

/* build a list from `nargs` owned references, stealing each */
static inline PyObject *By_BuildList(PyObject **items, Py_ssize_t nargs) {
    PyObject *list = PyList_New(nargs);
    if (list == NULL) {
        for (Py_ssize_t i = 0; i < nargs; i++) Py_XDECREF(items[i]);
        return NULL;
    }
    for (Py_ssize_t i = 0; i < nargs; i++) {
        PyList_SET_ITEM(list, i, items[i]);
    }
    return list;
}

/* build a dict from alternating key/value *borrowed* references
 *
 * a tuple or a list takes a reference over, so an item goes into one by being handed
 * the caller's. a dict and a set do not: `PyDict_SetItem` and `PySet_Add` take a
 * reference of their own, so handing one over would be a reference made at the call
 * site only to be dropped again here — two operations per key and two per value, for
 * nothing. these two borrow instead, and the caller goes on owning what it passed */
static inline PyObject *By_BuildDict(PyObject **pairs, Py_ssize_t count) {
    PyObject *dict = PyDict_New();
    if (dict == NULL) return NULL;
    for (Py_ssize_t i = 0; i < count; i++) {
        if (PyDict_SetItem(dict, pairs[i * 2], pairs[i * 2 + 1]) < 0) {
            Py_DECREF(dict);
            return NULL;
        }
    }
    return dict;
}

static inline PyObject *By_BuildSet(PyObject **items, Py_ssize_t count) {
    PyObject *set = PySet_New(NULL);
    if (set == NULL) return NULL;
    for (Py_ssize_t i = 0; i < count; i++) {
        if (PySet_Add(set, items[i]) < 0) {
            Py_DECREF(set);
            return NULL;
        }
    }
    return set;
}

/* build a tuple, stealing each reference */
static inline PyObject *By_BuildTuple(PyObject **items, Py_ssize_t count) {
    PyObject *tuple = PyTuple_New(count);
    if (tuple == NULL) {
        for (Py_ssize_t i = 0; i < count; i++) Py_XDECREF(items[i]);
        return NULL;
    }
    for (Py_ssize_t i = 0; i < count; i++) PyTuple_SET_ITEM(tuple, i, items[i]);
    return tuple;
}

/* ── subscripting ─────────────────────────────────────────────────────────── */

/* `container[index]`.
 *
 * the fast paths are guarded on the **exact** type, never on `PyList_Check` — a
 * subclass may override `__getitem__`, and a fast path that ignored that would be a
 * wrong answer rather than a fast one. everything unrecognised falls through to the
 * protocol, so a missed case costs speed and never correctness */
/* forward: the tagged form falls back to this when the index is not a short or
 * the container is not one it knows */
static inline PyObject *By_GetItem(PyObject *container, PyObject *index);

/* `s[i]` for an exact `str`, the index already an integer
 *
 * `PyUnicode_FromOrdinal` is the same call the interpreter's own `__getitem__`
 * makes, so a latin-1 character comes back as the cached singleton rather than as
 * a fresh one-character string — the object is the one cpython would have handed
 * back, identity included */
static inline PyObject *By_StrCharAt(PyObject *s, Py_ssize_t i) {
    Py_ssize_t n = PyUnicode_GET_LENGTH(s);
    if (i < 0) i += n;
    if (i < 0 || i >= n) {
        PyErr_SetString(PyExc_IndexError, "string index out of range");
        return NULL;
    }
    return PyUnicode_FromOrdinal((int) PyUnicode_READ_CHAR(s, i));
}

/* everything an indexed read can do apart from finding the element: the index
 * that is out of range, the index that is not a machine integer, and the
 * container that answers through the protocol
 *
 * it repeats the fast cases rather than being reached only after them, so that
 * it is a complete answer on its own and the caller above is free to test as
 * few of them as it likes */
BY_COLD PyObject *By_ItemSlow(PyObject *container, ByTagged index) {
    if (container == NULL || index == BY_INT_ERROR) return NULL;
    if (By_IsShort(index)) {
        Py_ssize_t i = By_ShortValue(index);
        if (PyList_CheckExact(container)) {
            Py_ssize_t n = PyList_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) return By_NewRef(PyList_GET_ITEM(container, i));
            PyErr_SetString(PyExc_IndexError, "list index out of range");
            return NULL;
        }
        if (PyTuple_CheckExact(container)) {
            Py_ssize_t n = PyTuple_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) return By_NewRef(PyTuple_GET_ITEM(container, i));
            PyErr_SetString(PyExc_IndexError, "tuple index out of range");
            return NULL;
        }
        if (PyUnicode_CheckExact(container)) return By_StrCharAt(container, i);
    }
    PyObject *boxed = By_BoxInt(index);
    if (boxed == NULL) return NULL;
    PyObject *result = By_GetItem(container, boxed);
    Py_DECREF(boxed);
    return result;
}

/* the element of an exact `list`, or NULL for everything else
 *
 * a `list` may have been subclassed and the subclass may have overridden
 * `__getitem__`, so it is the exact type that licenses reading the array
 * directly. every other answer — a subclass, another container, an index out of
 * range, an index that is not an integer at all — comes back NULL and is the
 * caller's to take somewhere slower. NULL is free to mean that here because this
 * never raises: a read that would have raised has not been attempted yet
 *
 * the index is an `int64_t` rather than a `Py_ssize_t` because that is what an
 * unboxed counter is. it is narrowed only after `i < n` has proved it is a
 * position in this list
 *
 * the element is the list's, not the caller's: it stays alive only while nothing
 * can run python, since python is what could shrink the list and drop it */
static inline PyObject *By_ListItemBorrowed(PyObject *container, int64_t i) {
    if (BY_LIKELY(container != NULL && PyList_CheckExact(container))) {
        int64_t n = (int64_t)PyList_GET_SIZE(container);
        if (i < 0) i += n;
        if (BY_LIKELY(i >= 0 && i < n)) {
            return PyList_GET_ITEM(container, (Py_ssize_t)i);
        }
    }
    return NULL;
}

/* as [`By_ListItemBorrowed`], with a reference of the caller's own */
static inline PyObject *By_ListItemAt(PyObject *container, int64_t i) {
    PyObject *item = By_ListItemBorrowed(container, i);
    return item == NULL ? NULL : By_NewRef(item);
}

/* `container[index]` where the index is already an integer register
 *
 * boxing one to look up a list element allocates a `PyLongObject` per iteration
 * that nothing ever sees. on the fast path the index never leaves its register;
 * everything else boxes it and takes the ordinary protocol.
 *
 * only the *list* read is written here. the tuple and `str` reads that used to
 * sit beside it answer from `By_ItemSlow` now, which had the tuple already and
 * has gained the `str`. three arms plus their bounds arithmetic is more than a C
 * compiler will inline: it emitted this as an ordinary function, and every `a[i]`
 * in a loop paid a call to it. dropping to one arm is 1.14x of the inheritance
 * benchmark and 1.04x of the dot product, and the tuple that now pays a call for
 * it measures unchanged — `tuples` reads 0.996-1.000 against a 0.4% floor */
static inline PyObject *By_GetItemTagged(PyObject *container, ByTagged index) {
    if (BY_LIKELY(By_IsShort(index))) {
        PyObject *item = By_ListItemAt(container, (int64_t)By_ShortValue(index));
        if (BY_LIKELY(item != NULL)) return item;
    }
    return By_ItemSlow(container, index);
}

/* `args[i]` where `args` is the function's own `*args` parameter
 *
 * the one arm [`By_GetItemTagged`] keeps inline answers a `list`, and a `*args` is
 * never one — so that read misses the head every time and pays the call into the
 * tail, which then tests the `list` again before reaching the tuple. a call site
 * that knows which container it has takes the arm that suits it, exactly as
 * `By_StrItemTagged` does for a `str`.
 *
 * the exact type is still tested rather than assumed. the calling convention builds
 * that tuple, so the parameter holds one on entry — but a body is free to rebind the
 * name, and a subclass of `tuple` bound there may have overridden `__getitem__`.
 * anything the test turns down takes the same tail as before */
static inline PyObject *By_TupleItemTagged(PyObject *container, ByTagged index) {
    if (BY_LIKELY(container != NULL && By_IsShort(index) && PyTuple_CheckExact(container))) {
        Py_ssize_t i = (Py_ssize_t)By_ShortValue(index);
        Py_ssize_t n = PyTuple_GET_SIZE(container);
        if (i < 0) i += n;
        if (BY_LIKELY(i >= 0 && i < n)) {
            return By_NewRef(PyTuple_GET_ITEM(container, i));
        }
    }
    return By_ItemSlow(container, index);
}

/* everything `By_GetItemI64` does not do itself, once the index has its tagged
 * representation back */
BY_COLD PyObject *By_ItemSlowI64(PyObject *container, int64_t i) {
    ByTagged index = By_IntFromI64(i);
    if (index == BY_INT_ERROR) return NULL;
    PyObject *result = By_ItemSlow(container, index);
    By_DecRefTagged(index);
    return result;
}

/* `container[i]` where the index is a machine integer
 *
 * a counter that `unbox_counters` has given a machine representation is boxed
 * back to a tagged `int` at every use that wants one, and a subscript does not:
 * the element it names is at an offset, and the offset is the number already in
 * the register. the box is a shift out and a shift straight back in, once per
 * element, along with the error edge that boxing an arbitrary `int` needs —
 * boxing is what allocates, so it is what can fail. what is left here is the
 * failure the read itself can have, which the caller was checking anyway */
static inline PyObject *By_GetItemI64(PyObject *container, int64_t i) {
    PyObject *item = By_ListItemAt(container, i);
    if (BY_LIKELY(item != NULL)) return item;
    return By_ItemSlowI64(container, i);
}

/* `s[i]` where the static type says `s` is a `str` and `i` an integer
 *
 * the general form goes through the protocol and then checks the result is a `str`
 * on the way back, because a subclass may hand back anything. an exact `str` cannot,
 * so both steps collapse into the character read — and a subclass still takes the
 * long way, check included */
static inline PyObject *By_StrItemTagged(PyObject *s, ByTagged index);

/* everything an object-indexed read can do apart from finding a value in a dict:
 * the integer index into a sequence, and the container that answers through the
 * protocol
 *
 * as with [`By_ItemSlow`], it repeats the case its caller tests rather than being
 * reached only after it, so it is a complete answer on its own */
BY_COLD PyObject *By_GetItemSlow(PyObject *container, PyObject *index);

/* `container[index]` where the index is an object
 *
 * `By_GetItemTagged` was split into a fast path and an out-of-line tail because a
 * C compiler prices an inline candidate by its whole body, and the tail is several
 * times the size of what it guards. this is the same split for the form the
 * *unindexed* subscript takes, which had never had it: a `d[k]` in a loop was
 * paying a call whose body then tested four container types before reaching the
 * table.
 *
 * an exact dict is what the head answers, because an index that is not a machine
 * integer is overwhelmingly a mapping key — the sequence cases arrive through
 * `By_GetItemTagged`, which has a fast path of its own. a subclass may have
 * overridden `__getitem__` and so is never answered here */
static inline PyObject *By_GetItem(PyObject *container, PyObject *index) {
    if (BY_LIKELY(container != NULL && index != NULL && PyDict_CheckExact(container))) {
#if PY_VERSION_HEX >= 0x030D0000
        PyObject *value;
        if (BY_UNLIKELY(PyDict_GetItemRef(container, index, &value) < 0)) return NULL;
        if (BY_LIKELY(value != NULL)) return value;
#else
        PyObject *value = PyDict_GetItemWithError(container, index);
        if (BY_LIKELY(value != NULL)) return By_NewRef(value);
        if (PyErr_Occurred()) return NULL;
#endif
        /* cpython raises the *key*, not a message */
        By_RaiseKeyError(index);
        return NULL;
    }
    return By_GetItemSlow(container, index);
}

BY_COLD PyObject *By_GetItemSlow(PyObject *container, PyObject *index) {
    if (container == NULL || index == NULL) return NULL;
    if (PyLong_CheckExact(index)) {
        Py_ssize_t i = PyLong_AsSsize_t(index);
        if (i == -1 && PyErr_Occurred()) {
            /* a value too large to be any index; the protocol reports it */
            PyErr_Clear();
        } else if (PyList_CheckExact(container)) {
            Py_ssize_t n = PyList_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) return By_NewRef(PyList_GET_ITEM(container, i));
            PyErr_SetString(PyExc_IndexError, "list index out of range");
            return NULL;
        } else if (PyUnicode_CheckExact(container)) {
            return By_StrCharAt(container, i);
        } else if (PyTuple_CheckExact(container)) {
            Py_ssize_t n = PyTuple_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) return By_NewRef(PyTuple_GET_ITEM(container, i));
            PyErr_SetString(PyExc_IndexError, "tuple index out of range");
            return NULL;
        }
    }
    if (PyDict_CheckExact(container)) {
        PyObject *value = PyDict_GetItemWithError(container, index);
        if (value != NULL) return By_NewRef(value);
        if (PyErr_Occurred()) return NULL;
        /* cpython raises the *key*, not a message */
        By_RaiseKeyError(index);
        return NULL;
    }
    return PyObject_GetItem(container, index);
}

static inline char By_SetItem(PyObject *container, PyObject *index, PyObject *value);

/* `container[index] = value` where the index is already an integer register, for
 * the reason [`By_GetItemTagged`] exists */
static inline char By_SetItemTagged(PyObject *container, ByTagged index, PyObject *value) {
    if (container == NULL || index == BY_INT_ERROR) return 2;
    if (BY_LIKELY(By_IsShort(index) && PyList_CheckExact(container))) {
        Py_ssize_t i = By_ShortValue(index);
        Py_ssize_t n = PyList_GET_SIZE(container);
        if (i < 0) i += n;
        if (i >= 0 && i < n) {
            PyObject *old = PyList_GET_ITEM(container, i);
            PyList_SET_ITEM(container, i, By_NewRef(value));
            Py_XDECREF(old);
            return 0;
        }
        PyErr_SetString(PyExc_IndexError, "list assignment index out of range");
        return 2;
    }
    PyObject *boxed = By_BoxInt(index);
    if (boxed == NULL) return 2;
    char result = By_SetItem(container, boxed, value);
    Py_DECREF(boxed);
    return result;
}

BY_COLD char By_SetItemSlow(PyObject *container, PyObject *index, PyObject *value);

/* `container[index] = value` where the index is an object, split for the reason
 * [`By_GetItem`] is: the store into an exact dict is the case a loop repeats, and
 * everything else is bigger than it and belongs out of line
 *
 * a null value means `del`, which is [`By_DeleteItem`]'s job — the head hands one
 * on rather than to `PyDict_SetItem`, which has no such meaning for it */
static inline char By_SetItem(PyObject *container, PyObject *index, PyObject *value) {
    if (BY_LIKELY(container != NULL && index != NULL && value != NULL
                  && PyDict_CheckExact(container))) {
        return PyDict_SetItem(container, index, value) < 0 ? 2 : 0;
    }
    return By_SetItemSlow(container, index, value);
}

BY_COLD char By_SetItemSlow(PyObject *container, PyObject *index, PyObject *value) {
    if (container == NULL || index == NULL) return 2;
    if (PyList_CheckExact(container) && PyLong_CheckExact(index)) {
        Py_ssize_t i = PyLong_AsSsize_t(index);
        if (!(i == -1 && PyErr_Occurred())) {
            Py_ssize_t n = PyList_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) {
                PyObject *old = PyList_GET_ITEM(container, i);
                PyList_SET_ITEM(container, i, By_NewRef(value));
                Py_XDECREF(old);
                return 0;
            }
            PyErr_SetString(PyExc_IndexError, "list assignment index out of range");
            return 2;
        }
        PyErr_Clear();
    }
    /* the write side of the arm [`By_GetItem`] already takes: `mp_ass_subscript`
     * on an exact dict is `PyDict_SetItem` with a deletion arm folded in, taken
     * on a null value. that arm is `By_DeleteItem`'s job here, so a null is left
     * to the protocol rather than handed to `PyDict_SetItem`, which has no such
     * meaning for one */
    if (PyDict_CheckExact(container) && value != NULL) {
        return PyDict_SetItem(container, index, value) < 0 ? 2 : 0;
    }
    return PyObject_SetItem(container, index, value) < 0 ? 2 : 0;
}

/* ── formatting ───────────────────────────────────────────────────────────────
 *
 * an f-string interpolation: apply the conversion, then the format spec. a null
 * spec means `format(value)` with no spec, which for most types is `str(value)`
 */

#define BY_CONV_NONE 0
#define BY_CONV_STR 1
#define BY_CONV_REPR 2
#define BY_CONV_ASCII 3

static inline PyObject *By_Format(PyObject *value, PyObject *spec, int conversion) {
    if (value == NULL) return NULL;
    PyObject *converted = NULL;
    switch (conversion) {
        case BY_CONV_STR: converted = PyObject_Str(value); break;
        case BY_CONV_REPR: converted = PyObject_Repr(value); break;
        case BY_CONV_ASCII: converted = PyObject_ASCII(value); break;
        default: converted = value; Py_INCREF(converted); break;
    }
    if (converted == NULL) return NULL;
    PyObject *result = PyObject_Format(converted, spec);
    Py_DECREF(converted);
    return result;
}

/* ── exception handling ───────────────────────────────────────────────────────
 *
 * a handler needs three things: the pending exception taken out of the thread
 * state, a test of whether it matches, and a way to put it back when it does not
 */

/* one place in a compiled function a traceback entry names: the file, the function and
 * the line, and the code object built for them the first time an exception passed */
typedef struct {
    PyCodeObject *code;
    const char *file;
    const char *name;
    int line;
} ByTracebackSite;

/* add the traceback entry python adds for a frame an exception is raised in or passes
 * through
 *
 * a compiled function has no frame, so it hangs the entry off one made for the purpose: an
 * empty code object naming the file, the function and the line, built once for the site
 * and kept, and a new frame over it in the module's namespace. python's own entry is made
 * the same way, from the frame it already has. a failure to build either leaves the
 * exception as it was, one entry short */
BY_COLD void By_TracebackHere(ByTracebackSite *site, PyObject *globals) {
    PyObject *type, *value, *traceback;
    PyErr_Fetch(&type, &value, &traceback);
    if (site->code == NULL) site->code = PyCode_NewEmpty(site->file, site->name, site->line);
    PyFrameObject *frame = site->code == NULL || globals == NULL
                               ? NULL
                               : PyFrame_New(PyThreadState_Get(), site->code, globals, NULL);
    PyErr_Restore(type, value, traceback);
    if (frame == NULL) return;
    PyTraceBack_Here(frame);
    Py_DECREF(frame);
}

/* take the pending exception. returns the value, or NULL when nothing is set */
static inline PyObject *By_FetchException(void) {
    PyObject *type = NULL, *value = NULL, *traceback = NULL;
    PyErr_Fetch(&type, &value, &traceback);
    if (type == NULL && value == NULL) {
        Py_XDECREF(traceback);
        return NULL;
    }
    PyErr_NormalizeException(&type, &value, &traceback);
    if (traceback != NULL) {
        PyException_SetTraceback(value, traceback);
    }
    Py_XDECREF(type);
    Py_XDECREF(traceback);
    return value;
}

static inline char By_ExceptionMatches(PyObject *value, PyObject *cls) {
    if (value == NULL || cls == NULL) return 0;
    return (char)(PyErr_GivenExceptionMatches(value, cls) != 0);
}

/* fixed-width division, which still has to raise on a zero divisor. python floors
 * rather than truncating, so a negative result is one less than C's */
static inline int64_t By_FixedFloorDiv(int64_t a, int64_t b) {
    if (b == 0) {
        By_ZeroDivision(PyNumber_FloorDivide, 0);
        return INT64_MIN;
    }
    int64_t q = a / b;
    if ((a % b != 0) && ((a < 0) != (b < 0))) q--;
    return q;
}

static inline int64_t By_FixedMod(int64_t a, int64_t b) {
    if (b == 0) {
        By_ZeroDivision(PyNumber_Remainder, 0);
        return INT64_MIN;
    }
    int64_t r = a % b;
    if (r != 0 && ((r < 0) != (b < 0))) r += b;
    return r;
}

static inline char By_DeleteItem(PyObject *container, PyObject *index) {
    if (container == NULL || index == NULL) return 2;
    return PyObject_DelItem(container, index) < 0 ? 2 : 0;
}

static inline char By_DeleteAttr(PyObject *receiver, const char *name) {
    if (receiver == NULL) return 2;
    return PyObject_DelAttrString(receiver, name) < 0 ? 2 : 0;
}

/* ── unboxed arrays ───────────────────────────────────────────────────────────
 *
 * a `list` whose elements are stored unboxed, in a buffer of its own rather than
 * as a `PyObject *` each. it is internal to a compilation unit: reaching python
 * means building a real `list` from it.
 *
 * the buffer carries **its own reference count**, and that is the design decision
 * worth stating. an owned resource with no retain would need move semantics the IR
 * does not have, and two registers could not both hold one. with a count it retains
 * and releases exactly like everything else, so the refcount pass, the borrow pass
 * and the verifier's release-set check all apply to it unchanged — it lives inside
 * the ownership discipline rather than beside it.
 */
typedef struct {
    Py_ssize_t refs;
    Py_ssize_t len;
    Py_ssize_t cap;
} ByArrayHeader;

static inline void By_ArrayIncRef(ByArrayHeader *array) {
    if (array != NULL) array->refs++;
}

static inline void By_ArrayDecRef(ByArrayHeader *array) {
    if (array != NULL && --array->refs == 0) PyMem_Free(array);
}

/* allocate a buffer for `cap` elements of `width` bytes, with the header inline
 * so one allocation holds both */
static inline ByArrayHeader *By_ArrayNew(Py_ssize_t cap, size_t width) {
    if (cap < 0) cap = 0;
    ByArrayHeader *array = (ByArrayHeader *)PyMem_Malloc(sizeof(ByArrayHeader) + (size_t)cap * width);
    if (array == NULL) {
        PyErr_NoMemory();
        return NULL;
    }
    array->refs = 1;
    array->len = 0;
    array->cap = cap;
    return array;
}

/* the elements, which sit immediately after the header */
static inline void *By_ArrayItems(ByArrayHeader *array) { return (void *)(array + 1); }

/* the index a `list` would use, normalized and bounds-checked the same way — a
 * negative index counts from the end, and out of range is `IndexError`. a read and a
 * store refuse in different words, as `list` does, so `out_of_range` is the one this
 * access raises
 *
 * this form takes an index a counter holds as a machine integer. no index past the short
 * range fits a buffer, so this answers exactly as the tagged form does for every value
 * the counter can hold */
static inline Py_ssize_t By_ArrayIndexIn(ByArrayHeader *array, int64_t at,
                                         const char *out_of_range) {
    if (array == NULL) return -1;
    if (BY_UNLIKELY(at < PY_SSIZE_T_MIN || at > PY_SSIZE_T_MAX)) {
        PyErr_SetString(PyExc_IndexError, "cannot fit 'int' into an index-sized integer");
        return -1;
    }
    Py_ssize_t index = (Py_ssize_t)at;
    if (index < 0) index += array->len;
    if (index < 0 || index >= array->len) {
        PyErr_SetString(PyExc_IndexError, out_of_range);
        return -1;
    }
    return index;
}

/* an index held tagged as an object: an exact `int` too big to index a buffer, or an
 * `int` subclass — `True` among them — which indexes by its value like any other. which
 * error a big one raises depends on whether it fits `Py_ssize_t`: python converts the
 * index before it compares it with the length */
BY_COLD Py_ssize_t By_ArrayIndexObject(ByArrayHeader *array, ByTagged tagged,
                                       const char *out_of_range) {
    PyObject *big = By_LongOf(tagged);
    Py_ssize_t value = PyLong_AsSsize_t(big);
    if (value == -1 && PyErr_Occurred()) {
        if (!PyErr_ExceptionMatches(PyExc_OverflowError)) return -1;
        PyErr_Clear();
        PyErr_Format(PyExc_IndexError, "cannot fit '%.200s' into an index-sized integer",
                     Py_TYPE(big)->tp_name);
        return -1;
    }
    return By_ArrayIndexIn(array, value, out_of_range);
}

static inline Py_ssize_t By_ArrayIndexTaggedIn(ByArrayHeader *array, ByTagged tagged,
                                               const char *out_of_range) {
    if (array == NULL) return -1;
    if (BY_UNLIKELY(!By_IsShort(tagged))) {
        return By_ArrayIndexObject(array, tagged, out_of_range);
    }
    return By_ArrayIndexIn(array, By_ShortValue(tagged), out_of_range);
}

static inline Py_ssize_t By_ArrayIndexI64(ByArrayHeader *array, int64_t at) {
    return By_ArrayIndexIn(array, at, "list index out of range");
}

static inline Py_ssize_t By_ArrayIndex(ByArrayHeader *array, ByTagged tagged) {
    return By_ArrayIndexTaggedIn(array, tagged, "list index out of range");
}

static inline Py_ssize_t By_ArrayStoreIndexI64(ByArrayHeader *array, int64_t at) {
    return By_ArrayIndexIn(array, at, "list assignment index out of range");
}

static inline Py_ssize_t By_ArrayStoreIndex(ByArrayHeader *array, ByTagged tagged) {
    return By_ArrayIndexTaggedIn(array, tagged, "list assignment index out of range");
}

/* grow a full buffer, doubling so a run of appends stays amortized constant */
BY_COLD ByArrayHeader *By_ArrayGrowFull(ByArrayHeader *array, size_t width) {
    if (array == NULL) return NULL;
    Py_ssize_t cap = array->cap < 4 ? 4 : array->cap * 2;
    ByArrayHeader *grown =
        (ByArrayHeader *)PyMem_Realloc(array, sizeof(ByArrayHeader) + (size_t)cap * width);
    if (grown == NULL) {
        PyErr_NoMemory();
        return NULL;
    }
    grown->cap = cap;
    return grown;
}

/* grow to hold one more */
static inline ByArrayHeader *By_ArrayGrow(ByArrayHeader *array, size_t width) {
    if (array == NULL) return NULL;
    if (array->len < array->cap) return array;
    return By_ArrayGrowFull(array, width);
}

/* `*x` or `**x` in a display: everything `x` holds, merged into a container this
 * frame has just built — so the kind test below is exact rather than a guess
 *
 * the type errors are python's own, wording included. the harness compares
 * exception text, so a difference there is one a user would see */
static inline char By_Extend(PyObject *container, PyObject *source, int mapping) {
    static PyObject *by_keys = NULL;
    if (container == NULL || source == NULL) return 2;
    if (mapping) {
        PyObject *name = By_FixedName(&by_keys, "keys", 4);
        if (name == NULL) return 2;
        PyObject *keys = PyObject_GetAttr(source, name);
        if (keys == NULL) {
            PyErr_Clear();
            PyErr_Format(PyExc_TypeError, "'%.200s' object is not a mapping",
                         Py_TYPE(source)->tp_name);
            return 2;
        }
        Py_DECREF(keys);
        return PyDict_Update(container, source) < 0 ? 2 : 0;
    }
    PyObject *iterator = PyObject_GetIter(source);
    if (iterator == NULL) {
        if (PyErr_ExceptionMatches(PyExc_TypeError)) {
            PyErr_Format(PyExc_TypeError, "Value after * must be an iterable, not %.200s",
                         Py_TYPE(source)->tp_name);
        }
        return 2;
    }
    PyObject *item;
    while ((item = PyIter_Next(iterator)) != NULL) {
        int failed = PyList_Check(container) ? PyList_Append(container, item)
                                             : PySet_Add(container, item);
        Py_DECREF(item);
        if (failed < 0) {
            Py_DECREF(iterator);
            return 2;
        }
    }
    Py_DECREF(iterator);
    return PyErr_Occurred() ? 2 : 0;
}

/* what python calls a callable in an error about the arguments it was handed
 *
 * `module.qualname()`, or `qualname()` for one of the builtins, or the object's `str`
 * where it has no qualified name at all */
BY_COLD PyObject *By_FunctionStr(PyObject *callable) {
    PyObject *qualname = PyObject_GetAttrString(callable, "__qualname__");
    PyObject *module;
    PyObject *result = NULL;
    if (qualname == NULL) {
        if (!PyErr_ExceptionMatches(PyExc_AttributeError)) return NULL;
        PyErr_Clear();
        return PyObject_Str(callable);
    }
    module = PyObject_GetAttrString(callable, "__module__");
    if (module == NULL) {
        if (!PyErr_ExceptionMatches(PyExc_AttributeError)) goto done;
        PyErr_Clear();
    } else if (module != Py_None) {
        int other = PyUnicode_Check(module) ? PyUnicode_CompareWithASCIIString(module, "builtins")
                                            : 1;
        if (other != 0) {
            result = PyUnicode_FromFormat("%S.%S()", module, qualname);
            goto done;
        }
    }
    result = PyUnicode_FromFormat("%S()", qualname);
done:
    Py_XDECREF(module);
    Py_DECREF(qualname);
    return result;
}

/* the error a failed merge into a call's keywords reports, in python's words
 *
 * python rewords the two errors a merge raises with a single argument, whatever raised
 * them: an `AttributeError` says the operand was no mapping, and a `KeyError` names the
 * keyword given twice. anything else is left as it was raised */
BY_COLD void By_KeywordsMergeError(PyObject *callee, PyObject *source) {
    int attribute = PyErr_ExceptionMatches(PyExc_AttributeError);
    PyObject *type, *value, *traceback, *args, *name;
    if (!attribute && !PyErr_ExceptionMatches(PyExc_KeyError)) return;
    PyErr_Fetch(&type, &value, &traceback);
    PyErr_NormalizeException(&type, &value, &traceback);
    args = value == NULL ? NULL : ((PyBaseExceptionObject *)value)->args;
    if (args == NULL || !PyTuple_Check(args) || PyTuple_GET_SIZE(args) != 1) {
        PyErr_Restore(type, value, traceback);
        return;
    }
    name = By_FunctionStr(callee);
    if (name != NULL) {
        PyObject *key = PyTuple_GET_ITEM(args, 0);
        if (attribute) {
            PyErr_Format(PyExc_TypeError, "%U argument after ** must be a mapping, not %.200s",
                         name, Py_TYPE(source)->tp_name);
        } else {
            PyErr_Format(PyExc_TypeError, "%U got multiple values for keyword argument '%S'",
                         name, key);
        }
        Py_DECREF(name);
    }
    Py_XDECREF(type);
    Py_XDECREF(value);
    Py_XDECREF(traceback);
}

/* one key of a `**` merged into a call's keywords, refusing one already there */
static int By_MergeKeyword(PyObject *keywords, PyObject *key, PyObject *value) {
    int present = PyDict_Contains(keywords, key);
    if (present < 0) return -1;
    if (present) {
        By_RaiseKeyError(key);
        return -1;
    }
    return PyDict_SetItem(keywords, key, value);
}

/* `**source` in a call to `callee`, merged into the keywords built so far
 *
 * the merge python makes for a call, which differs from a display's in refusing a key
 * that is already there. a dict that iterates as a dict is walked as one, whatever its
 * `keys` says; anything else is asked for its keys and then for each value */
static inline char By_MergeKeywords(PyObject *keywords, PyObject *source, PyObject *callee) {
    int failed = 0;
    if (keywords == NULL || source == NULL || callee == NULL) return 2;
    if (PyDict_Check(source) && Py_TYPE(source)->tp_iter == PyDict_Type.tp_iter) {
        Py_ssize_t at = 0;
        PyObject *key, *value;
        while (!failed && PyDict_Next(source, &at, &key, &value)) {
            Py_INCREF(key);
            Py_INCREF(value);
            failed = By_MergeKeyword(keywords, key, value) < 0;
            Py_DECREF(value);
            Py_DECREF(key);
        }
    } else {
        PyObject *keys = PyMapping_Keys(source);
        PyObject *iterator = keys == NULL ? NULL : PyObject_GetIter(keys);
        PyObject *key;
        Py_XDECREF(keys);
        failed = iterator == NULL;
        while (!failed && (key = PyIter_Next(iterator)) != NULL) {
            PyObject *value = PyObject_GetItem(source, key);
            failed = value == NULL || By_MergeKeyword(keywords, key, value) < 0;
            Py_XDECREF(value);
            Py_DECREF(key);
        }
        Py_XDECREF(iterator);
        failed = failed || PyErr_Occurred() != NULL;
    }
    if (!failed) return 0;
    By_KeywordsMergeError(callee, source);
    return 2;
}

/* unpack `value` into `count` slots, the way an assignment target list does
 *
 * `starred` is the index that collects the surplus into a list, or -1. this drives
 * the *iterator*, as python does, rather than materializing the whole sequence: a
 * `a, b = <infinite generator>` has to raise rather than run out of memory.
 *
 * the messages are python's own, wording included — the differential harness
 * compares exception text, so a difference there is one a user would see. nothing in
 * the c api raises them for a caller, so where the wording has changed between
 * versions the version is what selects it */
static inline int By_Unpack(PyObject *value, PyObject **out, Py_ssize_t count,
                            Py_ssize_t starred) {
    for (Py_ssize_t i = 0; i < count; i++) out[i] = NULL;
    PyObject *iterator = PyObject_GetIter(value);
    if (iterator == NULL) {
        if (PyErr_ExceptionMatches(PyExc_TypeError)) {
            PyErr_Format(PyExc_TypeError, "cannot unpack non-iterable %.200s object",
                         Py_TYPE(value)->tp_name);
        }
        return -1;
    }
    Py_ssize_t before = starred < 0 ? count : starred;
    Py_ssize_t after = starred < 0 ? 0 : count - starred - 1;
    for (Py_ssize_t i = 0; i < before; i++) {
        PyObject *item = PyIter_Next(iterator);
        if (item == NULL) {
            if (!PyErr_Occurred()) {
                PyErr_Format(PyExc_ValueError,
                             starred < 0
                                 ? "not enough values to unpack (expected %zd, got %zd)"
                                 : "not enough values to unpack (expected at least %zd, got %zd)",
                             before + after, i);
            }
            goto failed;
        }
        out[i] = item;
    }
    if (starred < 0) {
        PyObject *extra = PyIter_Next(iterator);
        if (extra != NULL) {
            Py_DECREF(extra);
#if PY_VERSION_HEX >= 0x030E0000
            /* 3.14 goes on to name the count it *got*, but only for the three types
             * whose length it can read without running anything — a subclass of one of
             * them, and anything else with a `__len__`, still get the shorter message */
            if (PyList_CheckExact(value) || PyTuple_CheckExact(value)
                || PyDict_CheckExact(value)) {
                Py_ssize_t size =
                    PyDict_CheckExact(value) ? PyDict_Size(value) : Py_SIZE(value);
                if (size > count) {
                    PyErr_Format(PyExc_ValueError,
                                 "too many values to unpack (expected %zd, got %zd)", count,
                                 size);
                    goto failed;
                }
            }
#endif
            PyErr_Format(PyExc_ValueError, "too many values to unpack (expected %zd)", count);
            goto failed;
        }
        if (PyErr_Occurred()) goto failed;
        Py_DECREF(iterator);
        return 0;
    }
    /* the rest goes to the star, and the tail is moved back out of it */
    PyObject *rest = PySequence_List(iterator);
    if (rest == NULL) goto failed;
    Py_ssize_t size = PyList_GET_SIZE(rest);
    if (size < after) {
        PyErr_Format(PyExc_ValueError,
                     "not enough values to unpack (expected at least %zd, got %zd)",
                     before + after, before + size);
        Py_DECREF(rest);
        goto failed;
    }
    for (Py_ssize_t i = 0; i < after; i++) {
        out[count - 1 - i] = By_NewRef(PyList_GET_ITEM(rest, size - 1 - i));
    }
    if (PyList_SetSlice(rest, size - after, size, NULL) < 0) {
        Py_DECREF(rest);
        goto failed;
    }
    out[starred] = rest;
    Py_DECREF(iterator);
    return 0;
failed:
    for (Py_ssize_t i = 0; i < count; i++) Py_CLEAR(out[i]);
    Py_DECREF(iterator);
    return -1;
}

/* mark `exception` as the one being handled, handing back whatever was before —
 * `Py_None` when there was nothing, so a register always holds a real object
 *
 * this is what makes *implicit* chaining work. `PyErr_SetObject` reads the handled
 * exception and makes it the new one's `__context__`, so a raise inside an `except`
 * block chains the way python's does — and so does one inside anything it calls,
 * which is why this goes through the thread state rather than being passed along */
static inline PyObject *By_PushHandled(PyObject *exception) {
    PyObject *type = NULL, *value = NULL, *traceback = NULL;
    PyErr_GetExcInfo(&type, &value, &traceback);
    PyObject *previous = value == NULL ? By_NewRef(Py_None) : By_NewRef(value);
    Py_XDECREF(type);
    Py_XDECREF(value);
    Py_XDECREF(traceback);
    if (exception == NULL) {
        PyErr_SetExcInfo(NULL, NULL, NULL);
    } else {
        /* all three are stolen */
        PyErr_SetExcInfo(By_NewRef((PyObject *)Py_TYPE(exception)), By_NewRef(exception),
                         PyException_GetTraceback(exception));
    }
    return previous;
}

/* put back what was being handled before, which `Py_None` spells as nothing */
static inline void By_PopHandled(PyObject *previous) {
    if (previous == NULL || previous == Py_None) {
        PyErr_SetExcInfo(NULL, NULL, NULL);
        return;
    }
    PyErr_SetExcInfo(By_NewRef((PyObject *)Py_TYPE(previous)), By_NewRef(previous),
                     PyException_GetTraceback(previous));
}

/* `raise <exception>`, and `raise <exception> from <cause>`, in general
 *
 * a class is instantiated and an instance used as it is, which is what the statement
 * itself does. a cause sets `__cause__`, and with it `__suppress_context__` — that is
 * what makes `from` hide the exception being handled */
static inline void By_RaiseObject(PyObject *exception, PyObject *cause) {
    if (exception == NULL) return;
    PyObject *instance = NULL;
    if (PyExceptionClass_Check(exception)) {
        instance = PyObject_CallNoArgs(exception);
        if (instance == NULL) return;
        if (!PyExceptionInstance_Check(instance)) {
            PyErr_Format(PyExc_TypeError,
                         "calling %R should have returned an instance of BaseException, not %R",
                         exception, Py_TYPE(instance));
            Py_DECREF(instance);
            return;
        }
    } else if (PyExceptionInstance_Check(exception)) {
        instance = By_NewRef(exception);
    } else {
        PyErr_SetString(PyExc_TypeError, "exceptions must derive from BaseException");
        return;
    }
    if (cause != NULL) {
        PyObject *made = NULL;
        if (cause == Py_None || PyExceptionInstance_Check(cause)) {
            made = By_NewRef(cause);
        } else if (PyExceptionClass_Check(cause)) {
            made = PyObject_CallNoArgs(cause);
            if (made == NULL) {
                Py_DECREF(instance);
                return;
            }
        } else {
            PyErr_SetString(PyExc_TypeError,
                            "exception causes must derive from BaseException");
            Py_DECREF(instance);
            return;
        }
        /* steals `made` */
        PyException_SetCause(instance, made);
    }
    PyErr_SetObject((PyObject *)Py_TYPE(instance), instance);
    Py_DECREF(instance);
}

/* put an exception back, for a handler that did not match or a bare re-raise
 *
 * the exception is restored as it stands, which is python's re-raise. raising it
 * with `PyErr_SetObject` would chain it again onto whatever is being handled by
 * now — and on the way out of an `except` block that is the exception from before
 * the block, so the context the exception picked up where it was raised would be
 * overwritten with an older one
 *
 * the operand is *borrowed*, as every helper's is: the register holding it belongs
 * to the frame, which releases it on each exit path. the restore steals, so the
 * reference handed over is a new one — keeping a second would be one nobody owns,
 * which is exactly what leaked a `GeneratorExit` per abandoned generator, and the
 * thrown exception per `throw` */
static inline void By_Reraise(PyObject *value) {
    if (value == NULL) return;
#if PY_VERSION_HEX >= 0x030C0000
    PyErr_SetRaisedException(By_NewRef(value));
#else
    PyErr_Restore(By_NewRef((PyObject *)Py_TYPE(value)), By_NewRef(value),
                  PyException_GetTraceback(value));
#endif
}

/* apply a method's decorators, innermost first, to the finished type — which is the
 * only place a type spec leaves for them.
 *
 * each decorator is resolved out of the module namespace by `By_LookupDotted`, so
 * `@property` and `@abc.abstractmethod` come out the same way. they are folded in
 * memory and the result written once, which is also what a class body does: the
 * namespace never holds a half-decorated method. `PyType_Modified` is what makes the change visible — the
 * attribute cache would otherwise keep serving the undecorated one */
static inline int By_ApplyMethodDecorators(PyTypeObject *type, PyObject *dict,
                                           const char *owner, const char *name,
                                           const char *const *decorators, Py_ssize_t count) {
    PyObject *target;
    Py_ssize_t index;
    int failed;
    if (count <= 0) return 0;
    /* a class whose construction fell back to the interpreted definition is already
       decorated — the fallback source ran its `def`s — and it is under its own name
       in the namespace, where nothing this module built can be yet */
    if ((PyObject *)type == PyDict_GetItemString(dict, owner)) return 0;
    target = PyObject_GetAttrString((PyObject *)type, name);
    if (target == NULL) return -1;
    {
        PyObject *writable = By_Method(target);
        Py_DECREF(target);
        if (writable == NULL) return -1;
        target = writable;
    }
    for (index = count; index > 0; index--) {
        PyObject *args[1] = {target};
        PyObject *fn = By_LookupDotted(dict, decorators[index - 1]);
        PyObject *wrapped;
        if (fn == NULL) {
            Py_DECREF(target);
            return -1;
        }
        wrapped = PyObject_Vectorcall(fn, args, 1, NULL);
        Py_DECREF(fn);
        Py_DECREF(target);
        if (wrapped == NULL) return -1;
        target = wrapped;
    }
    /* a *static* type is immutable to `setattr`, so the entry goes into `tp_dict`
       directly. that is safe here and only here: module init, before anything has
       looked the attribute up */
    failed = PyDict_SetItemString(type->tp_dict, name, target) < 0;
    Py_DECREF(target);
    if (failed) return -1;
    PyType_Modified(type);
    return 0;
}

/* the decorated method, taken from the class body where the body already built one
 *
 * a method's decorators run *inside* the class body: `@mark def g` is a `def` statement,
 * and the interpreted definition ran it before anything of this module existed. so the
 * body already holds the decorator's answer, and applying the decorators again to the
 * native method calls them a **second time**. a decorator that only reads its argument is
 * unharmed; one that registers registers twice, which is a silent miscompile —
 * `@atexit.register`, a route table, any `SEEN.append(fn)`.
 *
 * so the body's answer is taken where there is one. the price is that such a method is the
 * *interpreted* one: a decorator is handed whatever the body gave it, and there is no way
 * to hand it the native method without calling it again. an undecorated method is not
 * touched and stays native, which is where the speed of a compiled class lives anyway.
 *
 * where there is no body answer to take, the decorators are applied — and that is not a
 * second application but the only one, because the double is *caused* by a body having run
 * them. a class with no interpreted `class` statement never ran any.
 *
 * a class whose construction fell back to the interpreted definition is already exactly
 * what this would build, and is under its own name in the namespace where nothing this
 * module built can be yet */
static inline int By_DecoratedMethod(PyObject *body, PyTypeObject *type, PyObject *dict,
                                     const char *owner, const char *name,
                                     const char *const *decorators, Py_ssize_t count,
                                     const By_Twins *twins) {
    if (count <= 0) return 0;
    if ((PyObject *)type == PyDict_GetItemString(dict, owner)) return 0;
    if (body != NULL && PyDict_GetItemString(body, name) != NULL) {
        return By_CopyClassConstant(body, type, name, twins);
    }
    return By_ApplyMethodDecorators(type, dict, owner, name, decorators, count);
}

/* a method every decorator of which only marks what it was handed, published compiled
 *
 * `typing.override`, `typing.final` and `abc.abstractmethod` hand back the very function
 * they were given, having written one attribute onto it. so the body's answer is the
 * function its `def` made, and all it holds beyond what a fresh `def` holds is its
 * `__dict__`. the compiled method takes a copy of that dict and stands under the name,
 * which is what `By_DecoratedMethod` could not do: no decorator runs a second time, and
 * the method that answers is the compiled one.
 *
 * the entry the type holds is the method table's own `def` where the type came from its
 * spec, and the body's function where it was built by handing its metaclass a namespace —
 * the body's function is carried into that namespace so a metaclass reads the marks the
 * way it reads them off a `class` statement's, `abc.ABCMeta` collecting
 * `__abstractmethods__` above all. either way what is published is `def`.
 *
 * a body holding something other than a function under the name did not get it from one
 * of these, and its answer is taken as `By_DecoratedMethod` takes it. where there is no
 * body answer at all the decorators never ran, and are applied: that is their only
 * application */
static inline int By_MarkedMethod(PyObject *body, PyTypeObject *type, PyObject *dict,
                                  const char *owner, PyMethodDef *def,
                                  const char *const *decorators, Py_ssize_t count,
                                  const By_Twins *twins) {
    PyObject *answer, *found, *descriptor, *method, *marks, *own;
    int failed;
    if ((PyObject *)type == PyDict_GetItemString(dict, owner)) return 0;
    answer = body == NULL ? NULL : PyDict_GetItemString(body, def->ml_name); /* borrowed */
    if (answer == NULL) {
        return By_ApplyMethodDecorators(type, dict, owner, def->ml_name, decorators, count);
    }
    if (!PyFunction_Check(answer)) {
        return By_DecoratedMethod(body, type, dict, owner, def->ml_name, decorators, count, twins);
    }
    found = PyDict_GetItemString(type->tp_dict, def->ml_name); /* borrowed */
    descriptor = By_MethodDescriptorOf(found);
    if (descriptor != NULL && ((PyMethodDescrObject *)descriptor)->d_method == def) {
        method = By_Method(found);
    } else {
        descriptor = By_MethodDescriptor(type, def);
        method = By_Method(descriptor);
        Py_XDECREF(descriptor);
    }
    if (method == NULL) return -1;
    marks = PyObject_GetAttrString(answer, "__dict__");
    own = marks == NULL ? NULL : PyObject_GenericGetDict(method, NULL);
    failed = own == NULL || PyDict_Update(own, marks) < 0
             || PyDict_SetItemString(type->tp_dict, def->ml_name, method) < 0;
    Py_XDECREF(own);
    Py_XDECREF(marks);
    Py_DECREF(method);
    if (failed) return -1;
    PyType_Modified(type);
    return 0;
}

/* apply the decorator `decorator` names to `dict[name]`, in place. this is what
 * lets a decorated function still be compiled: the native one goes into the
 * namespace, then the decorator wraps it, exactly as the `def` statement would
 * have. `decorator` is a dotted path — see `By_LookupDotted` */
static inline int By_ApplyDecorator(PyObject *dict, const char *name, const char *decorator) {
    PyObject *target = PyDict_GetItemString(dict, name);
    if (target == NULL) {
        PyErr_Format(PyExc_NameError, "name '%s' is not defined", name);
        return -1;
    }
    Py_INCREF(target);
    PyObject *fn = By_LookupDotted(dict, decorator);
    if (fn == NULL) {
        Py_DECREF(target);
        return -1;
    }
    PyObject *args[1] = {target};
    PyObject *wrapped = PyObject_Vectorcall(fn, args, 1, NULL);
    Py_DECREF(fn);
    Py_DECREF(target);
    if (wrapped == NULL) return -1;
    int failed = PyDict_SetItemString(dict, name, wrapped) < 0;
    Py_DECREF(wrapped);
    return failed ? -1 : 0;
}

/* take a name back out of the module namespace, where it is still there
 *
 * used for the forwarder installer, which is a name the twin binds only so that
 * module init has something to call. an exit that never calls it has to unbind it
 * anyway: python's own module does not have that name, and a `dir()` that showed it
 * would be this compiler's namespace rather than the program's */
static inline void By_DropName(PyObject *dict, const char *name) {
    if (PyDict_DelItemString(dict, name) < 0) PyErr_Clear();
}

/* publish a real `function` under each name in `methods`, forwarding to the native
 *
 * a `PyCFunction` is not a descriptor, so a module that publishes one under its own
 * function name hands out a callable that never receives a receiver: `Cls.method =
 * mod.fn` — which is what `functools.total_ordering` does to every class it
 * decorates — installs something that drops slot zero. cpython has no supported way
 * to make one bind: it has no `tp_descr_get`, `staticmethod(len)` does not bind
 * either, and `PyDescr_NewMethod` refuses to be called without a receiver at all, so
 * it cannot stand where a module-level function stands.
 *
 * so the module publishes what python itself publishes: a `function`. the twin
 * carries one per exported definition, written by `by_irbuild::shims` and compiled
 * by the interpreter along with the rest of the twin, and `installer` names the
 * function that closes each of them over its native object and binds it in the
 * module namespace. the natives themselves never go into that namespace — the only
 * thing holding them is the closure cell of the forwarder that calls them.
 *
 * building and installing are two steps because they want two different moments.
 *
 * a forwarder has to *exist* early: it is what stands for the interpreted definition
 * everywhere the body captured one — `handler = fn` in a class body, an alias, a
 * default argument — and a class carries its constants across as its type is built,
 * which is long before the end of init. so the twins are paired with their
 * replacements as soon as the definitions have been made.
 *
 * a forwarder has to be *bound* late, and not one statement earlier. the moment it
 * takes the name is the moment the definition stops being what the name means, and
 * everything init still has to do with that name — a class decorator resolved out of
 * the namespace above all — is what the interpreted module would have done with the
 * `def`'s own object.
 *
 * so the installer is called against a copy of the namespace. it reads each
 * definition out of the copy and binds each forwarder back into it, leaving the
 * module's own namespace exactly as it was, and the copy is what the pairs and the
 * later install are both read from. `staging` is that copy, a new reference, and its
 * entries are borrowed for as long as it is held */
static inline PyObject *By_BuildForwarders(PyObject *module, PyObject *dict, PyMethodDef *methods,
                                           Py_ssize_t count, const char *installer) {
    PyObject *modname, *natives, *install, *result, *staging;
    Py_ssize_t at;
    staging = PyDict_Copy(dict);
    if (staging == NULL) return NULL;
    modname = PyModule_GetNameObject(module);
    if (modname == NULL) {
        Py_DECREF(staging);
        return NULL;
    }
    natives = PyTuple_New(count);
    if (natives == NULL) {
        Py_DECREF(modname);
        Py_DECREF(staging);
        return NULL;
    }
    for (at = 0; at < count; at++) {
        /* the same construction `PyModule_AddFunctions` would have used, module and
         * all, so a forwarder calls exactly the object that used to stand here */
        PyObject *native = PyCFunction_NewEx(&methods[at], module, modname);
        if (native == NULL) {
            Py_DECREF(natives);
            Py_DECREF(modname);
            Py_DECREF(staging);
            return NULL;
        }
        PyTuple_SET_ITEM(natives, at, native);
    }
    Py_DECREF(modname);
    install = PyDict_GetItemString(dict, installer); /* borrowed */
    if (install == NULL) {
        Py_DECREF(natives);
        Py_DECREF(staging);
        PyErr_Format(PyExc_ImportError,
                     "this module's interpreted definitions did not define '%s'", installer);
        return NULL;
    }
    Py_INCREF(install);
    {
        /* the namespace is handed in rather than taken with `globals()`, because a
         * module that defines a function called `globals` would have made that name
         * a local of the installer before the installer ever ran. `BaseException` is
         * handed in for the same reason: a forwarder catches it by that name */
        PyObject *args[3] = {natives, staging, PyExc_BaseException};
        result = PyObject_Vectorcall(install, args, 3, NULL);
    }
    Py_DECREF(install);
    Py_DECREF(natives);
    if (result == NULL) {
        Py_DECREF(staging);
        return NULL;
    }
    Py_DECREF(result);
    return staging;
}

/* what a built forwarder is, borrowed out of the staging namespace, or NULL where the
 * installer left nothing under that name */
static inline PyObject *By_BuiltForwarder(PyObject *staging, const char *name) {
    PyObject *built = PyDict_GetItemString(staging, name);
    if (built == NULL) PyErr_Clear();
    return built;
}

/* bind each built forwarder under the name its definition holds, which is the moment
 * the definition stops being what that name means
 *
 * the installer took itself back out of the *copy* it was handed, so the module's own
 * namespace is still carrying it and this is where that is undone */
static inline int By_InstallForwarders(PyObject *dict, PyObject *staging, PyMethodDef *methods,
                                       Py_ssize_t count, const char *installer) {
    Py_ssize_t at;
    for (at = 0; at < count; at++) {
        PyObject *built = By_BuiltForwarder(staging, methods[at].ml_name);
        if (built == NULL) {
            PyErr_Format(PyExc_ImportError,
                         "this module's installer published no forwarder for '%s'",
                         methods[at].ml_name);
            return -1;
        }
        if (PyDict_SetItemString(dict, methods[at].ml_name, built) < 0) return -1;
    }
    By_DropName(dict, installer);
    return 0;
}

/* ── iteration ────────────────────────────────────────────────────────────── */

static inline PyObject *By_GetIter(PyObject *o) { return PyObject_GetIter(o); }

/* NULL means either exhausted or failed; the caller distinguishes with
 * PyErr_Occurred, exactly as the interpreter's FOR_ITER does */
static inline PyObject *By_IterNext(PyObject *it) { return PyIter_Next(it); }

/* what a `for` loop iterates over, when the loop keeps a cursor of its own
 *
 * an exact list stands in for its own iterator: the loop already holds a reference
 * for as long as it would have held one to a `list_iterator`, and the position it
 * would have kept inside that object lives in a register instead. anything else —
 * a subclass that may have overridden `__iter__` included — still gets a real
 * iterator, and [`By_CursorStep`] still steps it through the protocol */
static inline PyObject *By_CursorIter(PyObject *o) {
    if (BY_LIKELY(o != NULL && PyList_CheckExact(o))) return By_NewRef(o);
    return PyObject_GetIter(o);
}

/* the position a cursor that steps a real iterator holds instead of one
 *
 * whether the loop walks an exact list is asked once, of what [`By_CursorIter`] handed
 * back, and kept in the cursor's sign: a list's exact type cannot change, and a loop
 * never walks anything but what it started with */
#define BY_CURSOR_PROTOCOL ((int64_t)-1)

static inline int64_t By_CursorStart(PyObject *it) {
    return PyList_CheckExact(it) ? 0 : BY_CURSOR_PROTOCOL;
}

/* one step of such a loop: the element, or NULL at the end
 *
 * a position that is not negative is one into an exact list — see
 * [`By_CursorStart`] — so the list is not asked what it is again on every step.
 *
 * the length is read again every step, because that is what cpython's list iterator
 * does — a list appended to under a `for` keeps feeding it and one popped from ends
 * it early. reading a length once at the top would quietly disagree on both.
 *
 * `PyList_GET_ITEM` is safe here for the reason the bounds test just established,
 * and the exactness test is what makes reading `ob_item` at all legitimate: a list
 * subclass could have overridden `__getitem__`, and it never reaches this arm.
 *
 * a slot inside an exact list's length holds an element and never NULL, which is what
 * cpython's own list iterator takes a reference to without asking.
 *
 * the bounds test is unsigned, so it rejects a negative position as well as one past
 * the end at no extra cost. a cursor should never be negative — the emitted loop sets
 * it to zero before the first step — but the position is the one number here that
 * indexes memory directly, and a test that only holds one end of it would let a
 * register that never got set read outside the list.
 *
 * the position is an `int64_t` rather than a `Py_ssize_t` because that is the width
 * the emitted register has, and the two are distinct types even where they are the
 * same size */
static inline PyObject *By_CursorStep(PyObject *it, int64_t *at) {
    if (BY_LIKELY(*at >= 0)) {
        if (BY_LIKELY((uint64_t) *at < (uint64_t) PyList_GET_SIZE(it))) {
            PyObject *item = PyList_GET_ITEM(it, (Py_ssize_t) *at);
            *at += 1;
            Py_INCREF(item);
            return item;
        }
        return NULL;
    }
    /* a position no loop started holds nothing to step */
    if (*at != BY_CURSOR_PROTOCOL) return NULL;
    return PyIter_Next(it);
}

/* ── str ──────────────────────────────────────────────────────────────────── */

/* two strings joined as the pieces of an f-string are, asking neither anything */
static inline PyObject *By_StrConcat(PyObject *a, PyObject *b) {
    return PyUnicode_Concat(a, b);
}

/* `a + b`, or `a += b`, over two operands that are not both exact `str`s
 *
 * a subclass may define `__add__`, `__radd__` or `__iadd__`, and python asks them the
 * way it asks any operand's. what they answer is what the register holding a `str` is
 * given, so an answer that is not one is refused rather than stored */
BY_COLD PyObject *By_StrAddSlow(PyObject *a, PyObject *b, int in_place) {
    PyObject *result = in_place ? PyNumber_InPlaceAdd(a, b) : PyNumber_Add(a, b);
    if (result != NULL && BY_UNLIKELY(!PyUnicode_Check(result))) {
        By_TypeError("str", result);
        Py_DECREF(result);
        return NULL;
    }
    return result;
}

/* `a + b` or `a += b` where the checker says both are `str`
 *
 * the interpreter's own concatenation is the answer only for two exact `str`s, which
 * have no operator methods a program can have replaced */
static inline PyObject *By_StrAdd(PyObject *a, PyObject *b, int in_place) {
    if (BY_LIKELY(PyUnicode_CheckExact(a) && PyUnicode_CheckExact(b))) {
        return PyUnicode_Concat(a, b);
    }
    return By_StrAddSlow(a, b, in_place);
}

/* the widest decimal an ssize_t reaches, with room for a sign and a terminator */
#define BY_INT_DIGITS 22

/* the decimal digits of a machine integer, written backwards into a scratch
 * buffer and copied forward
 *
 * `snprintf` and `PyUnicode_FromFormat` both cost more than boxing the value and
 * asking python for its `str`, which is the thing this exists to be cheaper than.
 * so the conversion is written out: a divide and a remainder per digit, and one
 * pass to reverse them */
static inline int By_DecimalDigits(char *out, Py_ssize_t value) {
    char buffer[BY_INT_DIGITS];
    int taken = 0;
    int length = 0;
    /* negated as unsigned, because the most negative value has no positive twin */
    size_t magnitude = value < 0 ? (size_t)(-(value + 1)) + 1u : (size_t)value;
    do {
        buffer[taken++] = (char)('0' + (magnitude % 10));
        magnitude /= 10;
    } while (magnitude);
    if (value < 0) out[length++] = '-';
    while (taken > 0) out[length++] = buffer[--taken];
    /* a compact string carries a terminator past its last character */
    out[length] = '\0';
    return length;
}

/* the `str` of a machine integer, built directly
 *
 * every character a decimal integer can have is ascii, so the object is made at
 * the widest an ssize_t reaches and cut back to the digits actually written. that
 * is one allocation for the whole conversion, against the two — a `PyLong` to
 * throw away and the string a formatter builds — that going through `PyObject_Str`
 * costs */
static inline PyObject *By_ShortToStr(Py_ssize_t value) {
    PyObject *text = PyUnicode_New(BY_INT_DIGITS, 127);
    if (text == NULL) return NULL;
    ((PyASCIIObject *)text)->length =
        By_DecimalDigits((char *)PyUnicode_1BYTE_DATA(text), value);
    return text;
}

/* whether `fn` is the interpreter's own builtin `name`
 *
 * what a name resolves to is looked up on every call, and a native lowering of a builtin
 * is only exact while the lookup answers with the builtin itself. that is a type the
 * interpreter defines statically under that bare name, which is how a type's module
 * comes out as `builtins`, or a C function the `builtins` module owns under that name.
 * neither can be built from python, and a module attribute written from outside or a
 * patched `builtins` is neither.
 *
 * the answer for the object last found to be the builtin is remembered in `genuine`,
 * which holds a reference to it, so that no other object can come to live at the
 * address it names and be taken for it */
static inline char By_IsBuiltin(PyObject *fn, const char *name, PyObject **genuine) {
    if (BY_LIKELY(fn == *genuine)) return 1;
    if (PyType_Check(fn)) {
        PyTypeObject *type = (PyTypeObject *)fn;
        if ((type->tp_flags & Py_TPFLAGS_HEAPTYPE) || strcmp(type->tp_name, name) != 0) {
            return 0;
        }
    } else if (PyCFunction_CheckExact(fn)) {
        PyObject *owner = PyCFunction_GET_SELF(fn);
        const char *module;
        if (strcmp(((PyCFunctionObject *)fn)->m_ml->ml_name, name) != 0) return 0;
        if (owner == NULL || !PyModule_CheckExact(owner)) return 0;
        module = PyModule_GetName(owner);
        if (module == NULL) {
            PyErr_Clear();
            return 0;
        }
        if (strcmp(module, "builtins") != 0) return 0;
    } else {
        return 0;
    }
    Py_XSETREF(*genuine, Py_NewRef(fn));
    return 1;
}

/* one site asking whether a name still resolves to the builtin of that name
 *
 * the answer is kept beside the lookup's memo and stands exactly as long as that does:
 * any write to a namespace the answer could have come from moves the counter, and the
 * next ask looks the name up again */
typedef struct {
    ByGlobalSite lookup;
    char answer;
    /* interned on the first ask, which is the slow one anyway */
    PyObject *name;
    /* see [`By_IsBuiltin`] */
    PyObject *genuine;
} ByBuiltinSite;

#define BY_BUILTIN_SITE_INIT { BY_GLOBAL_SITE_INIT, 0, NULL, NULL }

/* resolve the name and work the answer out again. 2 is the `NameError` of a name bound
 * nowhere, or a failure to intern it */
static char By_ArmBuiltinSite(ByBuiltinSite *site, PyObject *dict, const char *builtin) {
    PyObject *found;
    if (site->name == NULL) {
        site->name = By_InternedStr(builtin, (Py_ssize_t)strlen(builtin));
        if (site->name == NULL) return 2;
    }
#ifdef BY_GLOBAL_SITES
    found = By_ArmGlobalSite(&site->lookup, dict, site->name);
#else
    found = By_LookupGlobal(dict, site->name);
#endif
    if (found == NULL) return 2;
    site->answer = By_IsBuiltin(found, builtin, &site->genuine);
    Py_DECREF(found);
    return site->answer;
}

/* whether `builtin` resolves, through the module namespace and then builtins, to the
 * interpreter's own builtin of that name
 *
 * no reference is taken on the way, and while nothing has been written to a namespace
 * the answer is one comparison and a load: it is asked every trip round a loop */
static inline char By_BuiltinStands(ByBuiltinSite *site, PyObject *dict, const char *builtin) {
#ifdef BY_GLOBAL_SITES
    if (BY_LIKELY(site->lookup.generation == by_globals->generation)) {
        return site->answer;
    }
#endif
    return By_ArmBuiltinSite(site, dict, builtin);
}

/* whether `builtin` resolves to the builtin, asked on the way into a loop that asks
 * nothing again until it reaches code that can run python
 *
 * only python code can write a namespace, so while this thread runs no python the answer
 * stands — but only while no other thread can run either. a build without the GIL answers
 * no. a failed lookup answers no too, and leaves no error behind: the loop as written asks
 * the question again where python would, and raises what python raises */
static inline char By_BuiltinStandsOnEntry(ByBuiltinSite *site, PyObject *dict,
                                           const char *builtin) {
#ifdef Py_GIL_DISABLED
    (void)site;
    (void)dict;
    (void)builtin;
    return 0;
#else
    char answer = By_BuiltinStands(site, dict, builtin);
    if (BY_UNLIKELY(answer == 2)) {
        PyErr_Clear();
        return 0;
    }
    return answer;
#endif
}


/* `str(n)` for a tagged integer, given whatever the name `str` resolved to
 *
 * the resolution is the caller's and still happens every time, so a module that
 * rebinds `str` is obeyed — `fn` is compared rather than assumed. what the fast
 * path rests on is that the slow one boxes an unboxed value with
 * `PyLong_FromSsize_t`, which builds a plain `int` and never a subclass, and
 * `str` of a plain `int` is its decimal digits. a tagged value that is *not*
 * short holds a `PyLongObject` that may well be a subclass, so it goes the long
 * way round and is asked */
static inline PyObject *By_StrOfInt(PyObject *fn, ByTagged n) {
    if (BY_LIKELY(fn == (PyObject *)&PyUnicode_Type && By_IsShort(n))) {
        return By_ShortToStr(By_ShortValue(n));
    }
    {
        PyObject *boxed = By_BoxInt(n);
        PyObject *result;
        PyObject *argv[1];
        if (boxed == NULL) return NULL;
        argv[0] = boxed;
        result = By_CallPython(fn, argv, 1);
        Py_DECREF(boxed);
        return result;
    }
}

/* `left` followed by the decimal digits of a machine integer, in one allocation
 *
 * the digits are all ascii, so the answer needs no wider a storage than `left`
 * already has — and `left`'s is the narrowest its own characters fit in, which
 * makes it the narrowest the answer's fit in too. an empty string is stored as
 * ascii, so a kind wider than one byte says `left` holds a character that needs it
 * and the answer holds that character as well.
 *
 * this is what `PyUnicode_Concat` would work out for itself, from the same
 * `PyUnicode_MAX_CHAR_VALUE`; what is saved is the string the digits would have
 * been built into first, and the second pass that copied them back out of it */
static inline PyObject *By_ConcatShortToStr(PyObject *left, Py_ssize_t value) {
    char digits[BY_INT_DIGITS];
    int taken = By_DecimalDigits(digits, value);
    Py_ssize_t length = PyUnicode_GET_LENGTH(left);
    int kind = PyUnicode_KIND(left);
    PyObject *text = PyUnicode_New(length + taken, PyUnicode_MAX_CHAR_VALUE(left));
    void *data;
    int at;
    if (text == NULL) return NULL;
    data = PyUnicode_DATA(text);
    memcpy(data, PyUnicode_DATA(left), (size_t)(length * kind));
    if (BY_LIKELY(kind == PyUnicode_1BYTE_KIND)) {
        memcpy((Py_UCS1 *)data + length, digits, (size_t)taken);
    } else if (kind == PyUnicode_2BYTE_KIND) {
        for (at = 0; at < taken; at++)
            ((Py_UCS2 *)data)[length + at] = (Py_UCS2)digits[at];
    } else {
        for (at = 0; at < taken; at++)
            ((Py_UCS4 *)data)[length + at] = (Py_UCS4)digits[at];
    }
    return text;
}

/* `left + str(n)` for a tagged integer, given whatever the name `str` resolved to
 *
 * the two guards are `By_StrOfInt`'s own and mean the same things: the resolution
 * is compared rather than assumed, so a module that rebinds `str` is obeyed, and a
 * tagged value that is not short holds an object whose `__str__` has to be asked.
 *
 * the slow path hands what came back straight to the operator, because a rebound
 * `str` may return anything at all and python adds whatever it returned: a `str`
 * subclass answers through its own `__radd__`, and anything else is refused in the
 * operator's own words. the fast path needs no such care for the digits: it built
 * them itself.
 *
 * it tests `left` for an exact `str`, both because a subclass may define `__add__`
 * and because reading a header that is not a string's would decide how long the
 * answer is from whatever the field happens to overlap — memory written past the end
 * rather than a wrong answer */
static inline PyObject *By_StrConcatInt(PyObject *left, PyObject *fn, ByTagged n, int in_place) {
    if (BY_LIKELY(fn == (PyObject *)&PyUnicode_Type && By_IsShort(n)
                  && PyUnicode_CheckExact(left))) {
        return By_ConcatShortToStr(left, By_ShortValue(n));
    }
    {
        PyObject *right = By_StrOfInt(fn, n);
        PyObject *result;
        if (right == NULL) return NULL;
        result = By_StrAdd(left, right, in_place);
        Py_DECREF(right);
        return result;
    }
}

/* concatenate, taking over the caller's reference to `left`
 *
 * a `str` grows in place only when nothing else can see it, so the caller handing
 * its reference over is what makes the count one and the append a resize rather
 * than a copy. that is the difference between a chain of concatenations being
 * linear and being quadratic.
 *
 * the reference is consumed on every path, the failing one included — `left` is
 * gone by the time this returns NULL.
 *
 * appending a string to *itself* is the one case a sole owner does not license:
 * the resize would move the buffer the copy is still reading from. the pass never
 * offers that pair and the verifier rejects it, so the test here is this helper's
 * own precondition rather than a case it expects — the cost of getting it wrong is
 * memory corruption, and one comparison is the wrong thing to save */
static inline PyObject *By_StrAppend(PyObject *left, PyObject *right) {
    if (BY_UNLIKELY(left == NULL || right == NULL)) {
        Py_XDECREF(left);
        return NULL;
    }
    if (BY_LIKELY(left != right && PyUnicode_Check(left))) {
        PyUnicode_Append(&left, right); /* NULLs `left` when it fails */
        return left;
    }
    PyObject *result = PyUnicode_Concat(left, right);
    Py_DECREF(left);
    return result;
}

/* `By_StrAppend` for `a + b` or `a += b`, which asks a subclass's operator methods
 *
 * the in-place append is the interpreter's own concatenation, so it is taken for two
 * exact `str`s alone, and the reference to `left` is consumed on every path here too */
static inline PyObject *By_StrAddAppend(PyObject *left, PyObject *right, int in_place) {
    if (BY_UNLIKELY(left == NULL || right == NULL)) {
        Py_XDECREF(left);
        return NULL;
    }
    if (BY_LIKELY(left != right && PyUnicode_CheckExact(left) && PyUnicode_CheckExact(right))) {
        PyUnicode_Append(&left, right); /* NULLs `left` when it fails */
        return left;
    }
    PyObject *result = By_StrAdd(left, right, in_place);
    Py_DECREF(left);
    return result;
}

/* whether two exact `str`s hold the same text
 *
 * the same three tests `unicode_richcompare` makes, in the same order: a string is
 * stored in the narrowest kind its widest character needs, so two equal strings
 * always agree on kind, and the interpreter rejects a mismatch outright rather than
 * comparing across widths */
static inline char By_StrEqual(PyObject *a, PyObject *b) {
    if (a == b) return 1;
    Py_ssize_t length = PyUnicode_GET_LENGTH(a);
    if (length != PyUnicode_GET_LENGTH(b)) return 0;
    int kind = PyUnicode_KIND(a);
    if (kind != PyUnicode_KIND(b)) return 0;
    return (char) (memcmp(PyUnicode_DATA(a), PyUnicode_DATA(b), (size_t) (length * kind)) == 0);
}

/* `a <op> b` where both are `str`
 *
 * the abstract protocol's work is deciding *whose* comparison to run, and for a
 * pair of exact `str`s that is settled. a subclass may have overridden it, so the
 * exact check is what keeps this a fast path rather than a different answer */
static inline char By_StrCompare(PyObject *a, PyObject *b, int op) {
    if (BY_UNLIKELY(a == NULL || b == NULL)) return By_ObjCompare(a, b, op);
    if (BY_UNLIKELY(!PyUnicode_CheckExact(a) || !PyUnicode_CheckExact(b))) {
        return By_ObjCompare(a, b, op);
    }
    if (op == Py_EQ) return By_StrEqual(a, b);
    if (op == Py_NE) return (char) !By_StrEqual(a, b);
    /* two exact `str`s: the comparison itself cannot raise */
    int order = PyUnicode_Compare(a, b);
    switch (op) {
        case Py_LT: return (char) (order < 0);
        case Py_LE: return (char) (order <= 0);
        case Py_GT: return (char) (order > 0);
        default: return (char) (order >= 0);
    }
}

/* `len` of anything with a length, as a tagged int
 *
 * the tail is deliberately left inline, unlike the one in [`By_GetItemTagged`].
 * putting it behind a call means every caller has a call *somewhere* in the
 * block, and a c compiler that cannot see past one stops keeping things in
 * registers across it. that costs nothing where the loop already calls out, and
 * it cost the character scan — whose whole body is a length, an index and a
 * comparison — twelve per cent, against six per cent gained on the inheritance
 * benchmark. so this one stays whole */
static inline ByTagged By_Len(PyObject *o) {
    // the common containers know their own size in a field, and hold at least a byte an
    // item, so the size is below the short range on any address space there is
    if (PyList_CheckExact(o)) return By_ShortFrom(PyList_GET_SIZE(o));
    if (PyUnicode_CheckExact(o)) return By_ShortFrom(PyUnicode_GET_LENGTH(o));
    if (PyTuple_CheckExact(o)) return By_ShortFrom(PyTuple_GET_SIZE(o));
    if (PyDict_CheckExact(o)) return By_ShortFrom(PyDict_GET_SIZE(o));
    if (PyBytes_CheckExact(o)) return By_ShortFrom(PyBytes_GET_SIZE(o));
    // anything else answers `__len__` with whatever it likes, `range(2**62)` included
    Py_ssize_t length = PyObject_Length(o);
    if (length < 0) return BY_INT_ERROR;
    return By_IntFromI64((int64_t)length);
}

/* raise `cls(message)` — the shape `assert` and a bare `raise Cls(...)` need */
static inline void By_RaiseWithMessage(PyObject *cls, const char *message) {
    PyErr_SetString(cls, message);
}

/* defined with the rest of the await protocol, below; a resumable frame's return
 * has to be able to reach it from here */
static inline void By_RaiseWith(PyObject *error, PyObject *value);

/* the frame has left for good, so `$state` says finished
 *
 * python marks a generator completed the moment control leaves its frame, whether
 * off the end or by raising, and a later `send`, `throw` or `close` then finds
 * nothing to resume. the raising half is the one that is easy to miss and the one
 * that matters: the exception has already unwound the body's `finally` blocks on
 * its way out, and a machine still calling itself suspended would be resumed by
 * finalization and run every one of them a second time */
static inline void By_FinishGenerator(int64_t *state) {
    *state = -1;
}

/* the value a `return` handed back, turned into the exception the iterator protocol
 * expects
 *
 * a resume reports its return by *storing* it in `$returned` rather than by raising,
 * so that `am_send` can answer what a frame returned without an exception ever being
 * built. every consumer that owes python a raise builds it here instead, which is the
 * one place the two faces can drift apart and so the one place to keep them together.
 *
 * `*returned` empty means the frame left by raising and the error is already set */
static inline PyObject *By_TakeReturn(PyObject **returned) {
    PyObject *value = *returned;
    if (value == NULL) return NULL;
    *returned = NULL;
    By_RaiseWith(PyExc_StopIteration, value);
    Py_DECREF(value);
    return NULL;
}

/* which surface a resumable frame presents, which pep 479 words its error after and
 * an async generator needs one more conversion than the other two */
#define BY_FRAME_GENERATOR 0
#define BY_FRAME_COROUTINE 1
#define BY_FRAME_ASYNC_GENERATOR 2

/* what `$state` holds while the frame runs: its resume writes this before anything else,
 * and a suspension or a finish writes over it. the frontend's `RUNNING_STATE` */
#define BY_FRAME_RUNNING -2

/* whether a frame's resume is on the stack right now */
static inline int By_FrameRunning(int64_t state) {
    return state == BY_FRAME_RUNNING;
}

/* what python calls this surface in a message it writes about one */
static inline const char *By_FrameNoun(int frame) {
    return frame == BY_FRAME_COROUTINE         ? "coroutine"
           : frame == BY_FRAME_ASYNC_GENERATOR ? "async generator"
                                               : "generator";
}

/* refuse to resume a frame whose resume is on the stack, which is its own body asking */
static inline int By_RefuseRunning(int64_t state, int frame) {
    if (BY_LIKELY(!By_FrameRunning(state))) return 0;
    PyErr_Format(PyExc_ValueError, "%s already executing", By_FrameNoun(frame));
    return -1;
}

/* refuse a resumption python itself would not have performed
 *
 * two refusals, one at each end of a machine's life, and they are the same question:
 * can this frame take what is being sent into it.
 *
 * a frame that has never run is suspended at no `yield`, so there is no expression for
 * a sent value to become. python refuses a non-`None` one rather than dropping it, and
 * all three surfaces refuse it — only the noun in the message differs. `next(g)` and
 * every resumption that carries nothing send `None`, so they pass.
 *
 * a *coroutine* that has finished is not an exhausted iterator, it is spent: awaiting
 * one twice is a mistake about ownership rather than the end of a sequence, so python
 * raises where a generator answers `StopIteration` and an async generator answers
 * `StopAsyncIteration`. that half is the coroutine's alone. `close()` is exempt because
 * closing a spent coroutine is how a caller says it is done with it, and `throw` into
 * an *unstarted* coroutine is exempt too — the thrown exception simply propagates.
 *
 * and a frame that is *running* can take nothing at all: its body is what is asking.
 * every surface refuses that, before either of the two above.
 *
 * `state` is 0 before the frame first runs and -1 once it has left for good, so
 * anything above 0 is a real suspension point with a `yield` to resume. `arg` is NULL
 * where the caller carries no sent value at all, as a `throw` does */
static inline int By_RefuseResumption(int64_t state, int frame, PyObject *arg) {
    if (By_RefuseRunning(state, frame) < 0) return -1;
    int64_t at = state;
    if (at == 0 && arg != NULL && arg != Py_None) {
        PyErr_Format(PyExc_TypeError, "can't send non-None value to a just-started %s",
                     By_FrameNoun(frame));
        return -1;
    }
    if (at < 0 && frame == BY_FRAME_COROUTINE) {
        PyErr_SetString(PyExc_RuntimeError, "cannot reuse already awaited coroutine");
        return -1;
    }
    return 0;
}

/* pep 479: a `StopIteration` that *escapes* a generator frame becomes a
 * `RuntimeError`, so that an accidental one — most often from a bare `next()` on an
 * exhausted iterator somewhere inside the body — cannot masquerade as the frame
 * having ended.
 *
 * the distinction this rests on is the whole reason a finish is [`Op::FinishFrame`]
 * and not a raise. a frame that *ends* reports its value through `$returned` and no
 * exception is built until a consumer needs one, so the only way an exception can be
 * standing here is that the body raised it. were the two the same operation, this
 * conversion would turn every ordinary `return` into a `RuntimeError`.
 *
 * an async generator converts `StopAsyncIteration` as well, and for the same reason:
 * that is the exception *its* protocol uses to mean "ended", so a body raising one
 * would be forging its own exhaustion. a plain generator raising `StopAsyncIteration`
 * means nothing in particular and is left alone.
 *
 * the original is chained as both `__cause__` and `__context__`, which is what
 * `_PyErr_FormatFromCause` does for cpython's own generators — setting the cause is
 * also what sets `__suppress_context__`, so the traceback shows the conversion once
 * rather than twice */
static inline void By_ConvertStopIteration(int frame) {
    const char *ended;
    if (PyErr_ExceptionMatches(PyExc_StopIteration)) {
        ended = "StopIteration";
    } else if (frame == BY_FRAME_ASYNC_GENERATOR
               && PyErr_ExceptionMatches(PyExc_StopAsyncIteration)) {
        ended = "StopAsyncIteration";
    } else {
        return;
    }
    const char *surface = By_FrameNoun(frame);
    PyObject *type, *value, *tb;
    PyErr_Fetch(&type, &value, &tb);
    PyErr_NormalizeException(&type, &value, &tb);
    if (value == NULL) {
        /* nothing to convert and nothing to put back; normalization only fails when
         * it is already raising something else, which is left standing */
        Py_XDECREF(type);
        Py_XDECREF(tb);
        return;
    }
    if (tb != NULL) PyException_SetTraceback(value, tb);
    PyErr_Format(PyExc_RuntimeError, "%s raised %s", surface, ended);
    PyObject *raised_type, *raised, *raised_tb;
    PyErr_Fetch(&raised_type, &raised, &raised_tb);
    PyErr_NormalizeException(&raised_type, &raised, &raised_tb);
    if (raised == NULL) {
        Py_XDECREF(raised_type);
        Py_XDECREF(raised_tb);
        PyErr_Restore(type, value, tb);
        return;
    }
    /* both setters *steal*, so the cause needs its own reference and the context
     * consumes the one this function has been holding */
    PyException_SetCause(raised, By_NewRef(value));
    PyException_SetContext(raised, value);
    PyErr_Restore(raised_type, raised, raised_tb);
    Py_XDECREF(type);
    Py_XDECREF(tb);
}

/* park the value the suspended `yield` expression is about to evaluate to.
 *
 * every resumption carries one, and a resumption that carries nothing carries `None`:
 * `next(g)` *is* `g.send(None)`, and a python generator has no third state. the store
 * cannot be skipped when the value is `None`, which is the whole bug this exists to
 * close — the field would keep whatever the last `send` left in it, and the next
 * `yield` would read that same value a second time.
 *
 * what it can skip is a store of the very object the field already holds, which leaves
 * the field as it was — and that is every `next()` after the first, since each one
 * parks `None` over the `None` the one before it parked */
static inline void By_ParkSent(PyObject **sent, PyObject *value) {
    PyObject *old = *sent;
    if (old == value) return;
    *sent = By_NewRef(value);
    Py_XDECREF(old);
}

/* write `None` over a parked field on a frame's way out, letting go of what it held
 *
 * the field holds `None` before the old value is let go of, so a finalizer that runs on
 * that release finds nothing of it left behind */
static inline void By_ClearField(PyObject **field) {
    PyObject *old = *field;
    *field = By_NewRef(Py_None);
    Py_XDECREF(old);
}

/* resume a generator's frame, finishing it when the frame leaves for good
 *
 * `resume` is the type's counted step, which takes a frame from the thread's depth around
 * the body as python does on resuming one, and answers NULL with `RecursionError` set
 * where the frame could not be pushed. every helper below that takes a `resume` means the
 * same function */
static inline PyObject *By_StepGenerator(PyObject *self, PyObject **sent, PyObject **returned,
                                         int64_t *state, int frame, PyObject *arg,
                                         PyObject *(*resume)(PyObject *)) {
    if (By_RefuseResumption(*state, frame, arg) < 0) return NULL;
    By_ParkSent(sent, arg);
    PyObject *result = resume(self);
    if (result != NULL) return result;
    By_FinishGenerator(state);
    /* an empty `$returned` is what says the frame left by *raising* rather than by
     * ending, and so is the one condition pep 479 asks about */
    if (*returned == NULL) By_ConvertStopIteration(frame);
    return By_TakeReturn(returned);
}

/* what `tp_iternext` answers once a generator's resume handed back nothing, which is
 * cpython's `gen_iternext`: a frame that raised passes its error on, a frame that returned
 * `None` ends the iteration with no exception at all, and any other value rides out on
 * `StopIteration`. the `for`, `list` or `next` asking treats a NULL with nothing raised as
 * the end, which is the whole point — a finish builds no exception.
 *
 * only this slot answers so. `send` and `throw` raise `StopIteration` for a `None` too,
 * because `gen_send` and `gen_throw` do.
 *
 * `quiet` is a compiled `for` stepping the generator itself, which is `PyIter_Next`: the
 * value a finish carries is dropped however it would have ridden out, since that is the
 * exception `PyIter_Next` would only have cleared */
static inline PyObject *By_IterFinished(int64_t *state, PyObject **returned, int quiet) {
    By_FinishGenerator(state);
    PyObject *value = *returned;
    if (value == NULL) {
        By_ConvertStopIteration(BY_FRAME_GENERATOR);
        return NULL;
    }
    *returned = NULL;
    if (!quiet && value != Py_None) By_RaiseWith(PyExc_StopIteration, value);
    Py_DECREF(value);
    return NULL;
}

/* the argument count `throw` and `athrow` take, in the words python uses for both
 *
 * `throw` asks at the call. `athrow` asks when the awaitable it hands back is first
 * stepped, because it keeps its arguments whole until then */
static inline int By_CountThrowArguments(const char *method, Py_ssize_t nargs) {
    if (nargs < 1) {
        PyErr_Format(PyExc_TypeError, "%s expected at least 1 argument, got %zd", method, nargs);
        return -1;
    }
    if (nargs > 3) {
        PyErr_Format(PyExc_TypeError, "%s expected at most 3 arguments, got %zd", method, nargs);
        return -1;
    }
    return 0;
}

/* the warning the `(type, value, traceback)` form carries, deprecated since 3.12 and
 * given at the call on every surface */
static inline int By_WarnThrowSignature(const char *method, Py_ssize_t nargs) {
#if PY_VERSION_HEX >= 0x030C0000
    if (nargs > 1
        && PyErr_WarnFormat(PyExc_DeprecationWarning, 1,
                            "the (type, exc, tb) signature of %s() is deprecated, use the "
                            "single-arg signature instead.",
                            method) < 0) {
        return -1;
    }
#else
    (void)method;
    (void)nargs;
#endif
    return 0;
}

/* the exception `throw(type, value, traceback)` raises, built as python builds it
 *
 * a class is instantiated from the value the way a `raise` of the pair would be: an
 * instance of it is kept, a tuple is the argument list, anything else is the one
 * argument. an instance takes no value of its own. a traceback given is the one the
 * exception carries when it is raised at the suspension.
 *
 * the answer is a new reference to an exception instance, or NULL with the refusal set */
static inline PyObject *By_ThrownException(PyObject *const *args, Py_ssize_t nargs) {
    PyObject *type = args[0];
    PyObject *value = nargs > 1 ? args[1] : NULL;
    PyObject *traceback = nargs > 2 ? args[2] : NULL;
    if (traceback == Py_None) {
        traceback = NULL;
    } else if (traceback != NULL && !PyTraceBack_Check(traceback)) {
        PyErr_SetString(PyExc_TypeError, "throw() third argument must be a traceback object");
        return NULL;
    }
    PyObject *instance;
    if (PyExceptionClass_Check(type)) {
        PyObject *built_type = By_NewRef(type);
        PyObject *built = value == NULL ? NULL : By_NewRef(value);
        PyObject *built_tb = traceback == NULL ? NULL : By_NewRef(traceback);
        /* a constructor that raises leaves *its* exception in the triple, and that is
         * what python throws in instead */
        PyErr_NormalizeException(&built_type, &built, &built_tb);
        Py_XDECREF(built_type);
        Py_XDECREF(built_tb);
        if (built == NULL || !PyExceptionInstance_Check(built)) {
            Py_XDECREF(built);
            if (!PyErr_Occurred()) PyErr_BadInternalCall();
            return NULL;
        }
        instance = built;
    } else if (PyExceptionInstance_Check(type)) {
        if (value != NULL && value != Py_None) {
            PyErr_SetString(PyExc_TypeError, "instance exception may not have a separate value");
            return NULL;
        }
        instance = By_NewRef(type);
    } else {
        /* `throw` words this differently from `raise`, and names what it was given */
        PyErr_Format(PyExc_TypeError,
                     "exceptions must be classes or instances deriving from BaseException, not %s",
                     Py_TYPE(type)->tp_name);
        return NULL;
    }
    if (traceback != NULL && PyException_SetTraceback(instance, traceback) < 0) {
        Py_DECREF(instance);
        return NULL;
    }
    return instance;
}

/* raise `instance` *at* a suspended frame's suspension point, taking the reference
 *
 * the exception goes into the state object's `$thrown` field, and the resumption point
 * raises it — which is what lets a `yield` inside `try` enter its own handler rather than
 * the exception appearing at the generator's entry.
 *
 * the resumption raises instead of producing a value, so nothing rides in on `$sent` — it
 * is parked as `None` all the same, because leaving the last `send`'s value standing is
 * what would let a later `yield` read it again */
static inline PyObject *By_ResumeRaising(PyObject *self, PyObject **sent, PyObject **thrown,
                                         PyObject **returned, int64_t *state, int frame,
                                         PyObject *instance, PyObject *(*resume)(PyObject *)) {
    PyObject *old = *thrown;
    *thrown = instance;
    Py_XDECREF(old);
    return By_StepGenerator(self, sent, returned, state, frame, Py_None, resume);
}


/* python's `gen_close_iter`: close the iterator a frame is delegating to, before the frame
 * itself is unwound. an iterator with no `close` has nothing to close, and one whose
 * `close` cannot even be looked up is reported and passed over */
static inline int By_CloseDelegate(PyObject *delegate) {
    static PyObject *by_close = NULL;
    PyObject *name = By_FixedName(&by_close, "close", 5);
    if (name == NULL) return -1;
    PyObject *close;
    if (By_OptionalAttr(delegate, name, &close) < 0) {
#if PY_VERSION_HEX >= 0x030E0000
        PyErr_FormatUnraisable("Exception ignored while closing generator %R", delegate);
#else
        PyErr_WriteUnraisable(delegate);
#endif
        close = NULL;
    }
    if (close == NULL) return 0;
    PyObject *result = PyObject_CallNoArgs(close);
    Py_DECREF(close);
    if (result == NULL) return -1;
    Py_DECREF(result);
    return 0;
}

/* `throw` into a frame suspended in `yield from` or `await`, which python hands to the
 * iterator the frame is delegating to rather than to the frame. the answer is NULL with
 * `*forwarded` 0 where the delegation has nothing to do with the throw and it is raised at
 * the suspension as an ordinary one would be.
 *
 * `GeneratorExit` closes the inner iterator instead — except in an async generator, whose
 * `aclose` has to let what it awaits work through the exit — and anything else goes to
 * the inner iterator's own `throw`, with the arguments exactly as they came: the frame
 * never makes an exception of them. an inner iterator with no `throw` leaves the throw to
 * the frame. what the inner iterator yields back is what this `throw` answers, and the
 * frame stays suspended where it was; what it raises back is raised at the suspension.
 *
 * the frame counts as running for as long as the inner iterator has it, so a body that
 * reaches back into the frame is refused as python refuses it */
static inline PyObject *By_ThrowIntoDelegate(PyObject *self, PyObject **sent, PyObject **thrown,
                                             PyObject **returned, int64_t *state, int frame,
                                             PyObject *const *args, Py_ssize_t nargs,
                                             PyObject *delegate,
                                             PyObject *(*resume)(PyObject *), int *forwarded) {
    static PyObject *by_throw = NULL;
    int64_t suspended = *state;
    *forwarded = 1;
    Py_INCREF(delegate);
    if (frame != BY_FRAME_ASYNC_GENERATOR
        && PyErr_GivenExceptionMatches(args[0], PyExc_GeneratorExit)) {
        *state = BY_FRAME_RUNNING;
        int closed = By_CloseDelegate(delegate);
        *state = suspended;
        Py_DECREF(delegate);
        if (closed == 0) {
            *forwarded = 0;
            return NULL;
        }
    } else {
        PyObject *name = By_FixedName(&by_throw, "throw", 5);
        PyObject *method = NULL;
        if (name == NULL || By_OptionalAttr(delegate, name, &method) < 0) {
            Py_DECREF(delegate);
            return NULL;
        }
        if (method == NULL) {
            Py_DECREF(delegate);
            *forwarded = 0;
            return NULL;
        }
        /* python hands a generator or coroutine of its own the three arguments without
         * going through `throw`, so the deprecated form warns once, at the outermost
         * frame, however deep the delegation. asking the method would warn again: that
         * generator is handed the exception it would have built out of them instead */
        PyObject *answer;
        if (nargs > 1 && (PyGen_CheckExact(delegate) || PyCoro_CheckExact(delegate))) {
            PyObject *instance = By_ThrownException(args, nargs);
            if (instance == NULL) {
                answer = NULL;
            } else {
                *state = BY_FRAME_RUNNING;
                answer = PyObject_CallOneArg(method, instance);
                *state = suspended;
                Py_DECREF(instance);
            }
        } else {
            *state = BY_FRAME_RUNNING;
            answer = PyObject_Vectorcall(method, args, (size_t)nargs, NULL);
            *state = suspended;
        }
        Py_DECREF(method);
        Py_DECREF(delegate);
        if (answer != NULL) return answer;
    }
    PyObject *raised = By_FetchException();
    if (raised == NULL) return NULL;
    return By_ResumeRaising(self, sent, thrown, returned, state, frame, raised, resume);
}

/* `throw(...)`: raise it *at the suspension point*.
 *
 * the arguments are `throw`'s own, already counted. a frame suspended in a delegation
 * hands them to the iterator it delegates to — see `By_ThrowIntoDelegate` — and
 * `delegate` is what says which iterator that is, where there is one: it answers NULL
 * for a suspension at a `yield`, and is NULL itself for a frame with no delegations.
 *
 * otherwise python builds the exception out of the arguments before it asks anything of
 * the frame, so a refusal of the arguments comes first, and it never reaches the frame at
 * all: python leaves a generator resumable after a `throw` it refused to make sense of. a
 * machine with no suspension point does not reach the frame either, but a throw does
 * finish it — see below. otherwise it is the resumption that decides, and a body that
 * catches what was thrown leaves the machine usable */
static inline PyObject *By_ThrowInto(PyObject *self, PyObject **sent, PyObject **thrown,
                                   PyObject **returned, int64_t *state, int frame,
                                   PyObject *const *args, Py_ssize_t nargs,
                                   PyObject *(*delegate)(PyObject *),
                                   PyObject *(*resume)(PyObject *)) {
    if (thrown == NULL) return NULL;
    PyObject *inner = delegate != NULL && *state > 0
                          ? delegate(self)
                          : NULL;
    if (inner != NULL) {
        int forwarded;
        PyObject *answer = By_ThrowIntoDelegate(self, sent, thrown, returned, state, frame,
                                                args, nargs, inner, resume, &forwarded);
        if (forwarded) return answer;
    }
    PyObject *instance = By_ThrownException(args, nargs);
    if (instance == NULL) return NULL;
    /* a spent coroutine refuses a throw for the same reason it refuses a send: python
     * answers the reuse rather than whatever was being thrown. no sent value rides in on
     * a throw, so the just-started half cannot fire here */
    if (By_RefuseResumption(*state, frame, NULL) < 0) {
        Py_DECREF(instance);
        return NULL;
    }
    /* a machine with no suspension point has nowhere to raise *at*: one that never
     * started has not reached a `yield` yet, and a finished one has left its frame for
     * good. python raises the exception at the call site for both and runs no body at
     * all, so this does not catch its own throw:
     *
     *     def g():
     *         try:
     *             yield 1
     *         except ValueError:
     *             yield 2
     *     g().throw(ValueError)       # ValueError, and the generator is now closed
     *
     * resuming instead would run the body from the top for a machine that never
     * started, and report exhaustion for one that has finished — two different wrong
     * answers about which exception the caller is holding */
    if (*state <= 0) {
        By_FinishGenerator(state);
        PyErr_SetObject((PyObject *)Py_TYPE(instance), instance);
        Py_DECREF(instance);
        return NULL;
    }
    return By_ResumeRaising(self, sent, thrown, returned, state, frame, instance, resume);
}

/* the value a `StopIteration` carries, or NULL where `exception` is not one
 *
 * a `StopIteration` raised at a frame's delegation is the delegation *finishing*: python
 * takes its value as what `yield from` or `await` evaluates to, which is how an inner
 * iterator's `throw` that ends it hands back a result */
static inline PyObject *By_StopIterationValue(PyObject *exception) {
    if (exception == NULL || !PyExceptionInstance_Check(exception)
        || !PyErr_GivenExceptionMatches(exception, PyExc_StopIteration)) {
        return NULL;
    }
    return By_NewRef(((PyStopIterationObject *)exception)->value);
}

/* report a close that failed while finalizing a frame, which has nowhere to raise
 *
 * 3.14 says what it was doing when it reports one; earlier versions give the object and
 * nothing more */
static inline void By_CloseUnraisable(PyObject *self) {
#if PY_VERSION_HEX >= 0x030E0000
    PyErr_FormatUnraisable("Exception ignored while closing generator %R", self);
#else
    PyErr_WriteUnraisable(self);
#endif
}

/* python's `RuntimeWarning` for a coroutine dropped before it ever ran
 *
 * issued through `warnings.warn` rather than a C warning, because python's own hands the
 * coroutine over as the warning's `source` — which is what `tracemalloc` hangs the
 * allocation traceback off. the stack level is the frame the coroutine was dropped in,
 * the same one python's `stacklevel=2` reaches from the helper it warns through. a failure
 * to warn has nowhere to go and is reported as unraisable, as python reports it */
static inline void By_WarnUnawaitedCoroutine(PyObject *coroutine, const char *qualname) {
    PyObject *warnings = PyImport_ImportModule("warnings");
    PyObject *warn = warnings == NULL ? NULL : PyObject_GetAttrString(warnings, "warn");
    Py_XDECREF(warnings);
    PyObject *message =
        warn == NULL ? NULL : PyUnicode_FromFormat("coroutine '%s' was never awaited", qualname);
    PyObject *level = message == NULL ? NULL : PyLong_FromLong(1);
    PyObject *result = NULL;
    if (level != NULL) {
        PyObject *args[] = {message, PyExc_RuntimeWarning, level, coroutine};
        result = PyObject_Vectorcall(warn, args, 4, NULL);
    }
    Py_XDECREF(level);
    Py_XDECREF(message);
    Py_XDECREF(warn);
    if (result == NULL) {
        PyErr_WriteUnraisable(coroutine);
        return;
    }
    Py_DECREF(result);
}

/* `close()`: throw `GeneratorExit` in and accept the three legal outcomes, answering
 * what `close()` returns.
 *
 * exhausting, re-raising `GeneratorExit`, or being already finished are all a clean
 * close. *yielding* is not — cpython calls that a `RuntimeError`, and the frame stays
 * suspended where it yielded, so a later step resumes it and finalizing it closes it again.
 *
 * since 3.13 a frame that *returns* while it unwinds hands that value back as `close()`'s
 * answer, and every other clean close answers `None`. the `StopIteration` the value rides
 * on is the frame's own *end*, which is the only kind that can still be standing here: one
 * the body raised has already become a `RuntimeError` on its way out, and comes back as the
 * failure it is.
 *
 * the throw is python's own `GeneratorExit`, so a frame suspended in a delegation closes
 * the iterator it delegates to first — see `By_ThrowIntoDelegate` */
static inline PyObject *By_CloseGenerator(PyObject *self, PyObject **sent, PyObject **thrown,
                                         PyObject **returned, int64_t *state, int frame,
                                         PyObject *(*delegate)(PyObject *),
                                         PyObject *(*resume)(PyObject *)) {
    if (By_RefuseRunning(*state, frame) < 0) return NULL;
    /* a machine with no suspension point has nothing to unwind, and closing one runs
     * no body at all — not even a `finally` the body has not reached yet. asking
     * `By_ThrowInto` would give the right answer for a finished frame and the wrong
     * one for a frame that never started, which would run the whole body under a
     * `GeneratorExit` it had no way to see */
    if (*state <= 0) {
        By_FinishGenerator(state);
        Py_RETURN_NONE;
    }
    PyObject *exit = PyExc_GeneratorExit;
    PyObject *result = By_ThrowInto(self, sent, thrown, returned, state, frame, &exit, 1,
                                    delegate, resume);
    if (result != NULL) {
        Py_DECREF(result);
        PyErr_Format(PyExc_RuntimeError, "%s ignored GeneratorExit", By_FrameNoun(frame));
        return NULL;
    }
    if (PyErr_ExceptionMatches(PyExc_GeneratorExit)) {
        PyErr_Clear();
        Py_RETURN_NONE;
    }
    if (PyErr_ExceptionMatches(PyExc_StopIteration)) {
#if PY_VERSION_HEX >= 0x030D0000
        PyObject *ended = By_FetchException();
        if (ended == NULL) return NULL;
        PyObject *value = By_StopIterationValue(ended);
        Py_DECREF(ended);
        return value;
#else
        PyErr_Clear();
        Py_RETURN_NONE;
#endif
    }
    return NULL;
}

/* the parameter a keyword names, or -1 when none does.
 *
 * a positional-only parameter is not reachable by name, so a keyword spelling one
 * names nothing at all — which is what sends it to a `**kwargs`, or to the error a
 * signature without one gives */
static inline Py_ssize_t By_NamedSlot(const char *text, const char *const *names,
                                      Py_ssize_t count, Py_ssize_t posonly) {
    for (Py_ssize_t i = posonly; i < count; i++) {
        if (strcmp(text, names[i]) == 0) return i;
    }
    return -1;
}

/* the error for a keyword nothing takes, which python words differently when the
 * name *is* a parameter that a keyword cannot reach */
static inline int By_RejectKeyword(const char *text, const char *const *names,
                                   Py_ssize_t posonly, const char *fname) {
    if (By_NamedSlot(text, names, posonly, 0) >= 0) {
        PyErr_Format(PyExc_TypeError,
                     "%s() got some positional-only arguments passed as keyword arguments: "
                     "'%s'",
                     fname, text);
    } else {
        PyErr_Format(PyExc_TypeError, "%s() got an unexpected keyword argument '%s'", fname,
                     text);
    }
    return -1;
}

/* python's own wording for too many positionals, which changes shape when some of
 * them have defaults — the harness compares exception text, so this is not cosmetic.
 *
 * `receiver` is the `self` python counts in the message and the binding never sees */
static inline int By_TooManyPositional(const char *fname, const unsigned char *required,
                                       Py_ssize_t limit, Py_ssize_t nargs,
                                       Py_ssize_t receiver) {
    Py_ssize_t least = 0;
    for (Py_ssize_t i = 0; i < limit; i++) {
        if (required[i]) least++;
    }
    Py_ssize_t reported = nargs + receiver;
    if (least < limit) {
        PyErr_Format(PyExc_TypeError,
                     "%s() takes from %zd to %zd positional arguments but %zd %s given",
                     fname, least + receiver, limit + receiver, reported,
                     reported == 1 ? "was" : "were");
    } else {
        PyErr_Format(PyExc_TypeError, "%s() takes %zd positional argument%s but %zd %s given",
                     fname, limit + receiver, limit + receiver == 1 ? "" : "s", reported,
                     reported == 1 ? "was" : "were");
    }
    return -1;
}

/* `*args`: the positionals past the named parameters, as a tuple */
static inline PyObject *By_PackArgs(PyObject *const *args, Py_ssize_t nargs, Py_ssize_t from) {
    Py_ssize_t extra = nargs > from ? nargs - from : 0;
    PyObject *packed = PyTuple_New(extra);
    if (packed == NULL) return NULL;
    for (Py_ssize_t i = 0; i < extra; i++) {
        PyTuple_SET_ITEM(packed, i, By_NewRef(args[from + i]));
    }
    return packed;
}

/* `**kwargs`: the keywords that match no named parameter, as a dict */
static inline PyObject *By_PackKwargs(PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames,
                                     const char *const *names, Py_ssize_t count,
                                     Py_ssize_t posonly) {
    PyObject *packed = PyDict_New();
    if (packed == NULL) return NULL;
    if (kwnames == NULL) return packed;
    Py_ssize_t keywords = PyTuple_GET_SIZE(kwnames);
    for (Py_ssize_t k = 0; k < keywords; k++) {
        PyObject *name = PyTuple_GET_ITEM(kwnames, k);
        const char *text = PyUnicode_AsUTF8(name);
        if (text == NULL) {
            Py_DECREF(packed);
            return NULL;
        }
        if (By_NamedSlot(text, names, count, posonly) >= 0) continue;
        if (PyDict_SetItem(packed, name, args[nargs + k]) < 0) {
            Py_DECREF(packed);
            return NULL;
        }
    }
    return packed;
}

/* the same two, for a constructor: `tp_init` is handed a tuple and a dict rather
 * than a vector, so the surplus is already a tuple and the keywords already a dict */
static inline PyObject *By_PackInitArgs(PyObject *args, Py_ssize_t from) {
    Py_ssize_t nargs = args == NULL ? 0 : PyTuple_GET_SIZE(args);
    if (nargs <= from) return PyTuple_New(0);
    return PyTuple_GetSlice(args, from, nargs);
}

static inline PyObject *By_PackInitKwargs(PyObject *kwds, const char *const *names,
                                          Py_ssize_t count, Py_ssize_t posonly) {
    PyObject *packed = PyDict_New();
    if (packed == NULL || kwds == NULL) return packed;
    PyObject *key = NULL, *value = NULL;
    Py_ssize_t pos = 0;
    while (PyDict_Next(kwds, &pos, &key, &value)) {
        const char *text = PyUnicode_AsUTF8(key);
        if (text == NULL) {
            Py_DECREF(packed);
            return NULL;
        }
        if (By_NamedSlot(text, names, count, posonly) >= 0) continue;
        if (PyDict_SetItem(packed, key, value) < 0) {
            Py_DECREF(packed);
            return NULL;
        }
    }
    return packed;
}

/* every parameter with no default that nothing filled, named the way python names them
 * — positional and keyword-only counted separately, because python reports them in two
 * different sentences
 *
 * this is the wording of *last resort*: [`By_Rephrase`] runs first and lets the
 * interpreter word the refusal itself, and only a shape it could not build falls back
 * to here. so the list joined below is deliberately left as it always was, one comma
 * short of python's — a differential test that sees this text is a test whose rephrasing
 * never ran, which is the one thing a comparison of two identical strings could not
 * otherwise tell anyone */
static inline int By_CheckRequired(const char *const *names, const unsigned char *required,
                                  Py_ssize_t count, Py_ssize_t kwonly, PyObject **out,
                                  const char *fname) {
    Py_ssize_t positional = count - kwonly;
    for (int pass = 0; pass < 2; pass++) {
        Py_ssize_t from = pass == 0 ? 0 : positional;
        Py_ssize_t to = pass == 0 ? positional : count;
        Py_ssize_t missing = 0;
        for (Py_ssize_t i = from; i < to; i++) {
            if (required[i] && out[i] == NULL) missing++;
        }
        if (missing == 0) continue;
        PyObject *listed = PyUnicode_FromString("");
        if (listed == NULL) return -1;
        Py_ssize_t seen = 0;
        for (Py_ssize_t i = from; i < to; i++) {
            if (!required[i] || out[i] != NULL) continue;
            seen++;
            const char *separator = seen == 1 ? "" : (seen == missing ? " and " : ", ");
            PyObject *piece = PyUnicode_FromFormat("%s'%s'", separator, names[i]);
            if (piece == NULL) {
                Py_DECREF(listed);
                return -1;
            }
            PyObject *joined = PyUnicode_Concat(listed, piece);
            Py_DECREF(piece);
            Py_DECREF(listed);
            if (joined == NULL) return -1;
            listed = joined;
        }
        PyErr_Format(PyExc_TypeError, "%s() missing %zd required %s argument%s: %U", fname,
                     missing, pass == 0 ? "positional" : "keyword-only",
                     missing == 1 ? "" : "s", listed);
        Py_DECREF(listed);
        return -1;
    }
    return 0;
}

/* a spelling no parameter already has, for a synthetic one that has to be named
 *
 * the receiver, the `*args` and the `**kwargs` [`By_Rephrase`] writes are named in
 * source nothing reads back, so any free spelling does and underscores are appended
 * until one is free. free of the *real* names is not on its own enough, though:
 * python offers a near miss to a caller who spelled a keyword wrongly, and it draws
 * that suggestion from the parameters between the positional-only run and the end of
 * the keyword-only one. a `*args` or `**kwargs` name lies outside that range and a
 * positional-only one before it, which is why the receiver is written as one */
static inline void By_SpareName(char *buffer, size_t size, const char *stem,
                                const char *const *names, Py_ssize_t count) {
    size_t used = strlen(stem);
    if (used + 1 > size) used = size - 1;
    memcpy(buffer, stem, used);
    buffer[used] = '\0';
    while (used + 1 < size) {
        Py_ssize_t i = 0;
        while (i < count && strcmp(buffer, names[i]) != 0) i++;
        if (i == count) return;
        buffer[used++] = '_';
        buffer[used] = '\0';
    }
}

/* the caller's positionals with the receiver python counts put back in front of them */
static inline PyObject *By_ShapeArgs(PyObject *args, Py_ssize_t receiver) {
    Py_ssize_t nargs = args == NULL ? 0 : PyTuple_GET_SIZE(args);
    Py_ssize_t extra = receiver ? 1 : 0;
    PyObject *made = PyTuple_New(nargs + extra);
    if (made == NULL) return NULL;
    if (extra) PyTuple_SET_ITEM(made, 0, By_NewRef(Py_None));
    for (Py_ssize_t i = 0; i < nargs; i++) {
        PyTuple_SET_ITEM(made, i + extra, By_NewRef(PyTuple_GET_ITEM(args, i)));
    }
    return made;
}

/* the refusal the interpreter itself would word for a call to a function of this shape
 *
 * nothing in the c api formats one. `format_missing`, `too_many_positional` and
 * `format_kwargs_error` are all static to `ceval.c`, and their wording is fussier than
 * it looks: `and` from two names up, a comma *before* that `and` from three up, a range
 * rather than a count once any parameter has a default, and a receiver counted in the
 * arity sentence but not in the missing-argument one. writing those rules out is what
 * left the comma out of this message for the whole of the project's life, and it was
 * right when it was written — so the next rule to change would go the same way
 *
 * so rather than the rules, the *shape*: a python function with the same parameters,
 * handed the same call. its body is `pass`, so the only thing the call can do is raise
 * what the interpreter raises for the real one. returns 1 having left that exception
 * pending, or 0 having left none — which is the two binders disagreeing, and is why the
 * caller's own wording stays behind this
 *
 * the caller's exception must be off the thread before this is reached: it compiles and
 * it calls, and neither is reached with one pending */
static inline int By_Rephrase(const char *const *names, const unsigned char *required,
                              Py_ssize_t count, Py_ssize_t posonly, Py_ssize_t kwonly,
                              int variadic, int extras, const char *fname,
                              Py_ssize_t receiver, PyObject *args, PyObject *kwds) {
    char self_name[32], rest_name[32], keys_name[32];
    By_SpareName(self_name, sizeof(self_name), "_by_self", names, count);
    By_SpareName(rest_name, sizeof(rest_name), "_by_rest", names, count);
    By_SpareName(keys_name, sizeof(keys_name), "_by_keys", names, count);
    /* a keyword spelled like the synthetic receiver would bind to it, and the shape
     * would then answer about a parameter the real function does not have */
    if (kwds != NULL && receiver && PyDict_GetItemString(kwds, self_name) != NULL) return 0;

    Py_ssize_t limit = count - kwonly;
    PyObject *source = PyUnicode_FromString("def _(");
    if (receiver) {
        PyUnicode_AppendAndDel(&source, PyUnicode_FromFormat("%s, ", self_name));
        /* positional-only, so that no near miss is ever offered against it. where the
         * function has a positional-only run of its own the marker comes after that */
        if (posonly == 0) PyUnicode_AppendAndDel(&source, PyUnicode_FromString("/, "));
    }
    for (Py_ssize_t i = 0; i < limit; i++) {
        PyUnicode_AppendAndDel(
            &source, PyUnicode_FromFormat("%s%s, ", names[i], required[i] ? "" : "=None"));
        if (i + 1 == posonly) PyUnicode_AppendAndDel(&source, PyUnicode_FromString("/, "));
    }
    if (variadic) {
        PyUnicode_AppendAndDel(&source, PyUnicode_FromFormat("*%s, ", rest_name));
    } else if (kwonly > 0) {
        PyUnicode_AppendAndDel(&source, PyUnicode_FromString("*, "));
    }
    for (Py_ssize_t i = limit; i < count; i++) {
        PyUnicode_AppendAndDel(
            &source, PyUnicode_FromFormat("%s%s, ", names[i], required[i] ? "" : "=None"));
    }
    if (extras) PyUnicode_AppendAndDel(&source, PyUnicode_FromFormat("**%s", keys_name));
    PyUnicode_AppendAndDel(&source, PyUnicode_FromString("): pass\n"));

    /* the module namespace the definition lands in, which is also where it is read back
     * from. nothing in the source needs a builtin, and evaluation supplies the
     * interpreter's own where a namespace carries none */
    PyObject *shape = NULL, *scope = NULL;
    const char *text = source == NULL ? NULL : PyUnicode_AsUTF8(source);
    if (text != NULL) scope = PyDict_New();
    if (scope != NULL) {
        PyObject *ran = PyRun_String(text, Py_file_input, scope, scope);
        Py_XDECREF(ran);
        if (ran != NULL) shape = PyDict_GetItemString(scope, "_");
    }
    int reworded = 0;
    if (shape != NULL) {
        /* every one of these messages names the *qualified* name, which for a function
         * is the one it carries rather than the one its code object was compiled with */
        PyObject *label = PyUnicode_FromString(fname);
        int named = label != NULL && PyObject_SetAttrString(shape, "__qualname__", label) == 0;
        Py_XDECREF(label);
        PyObject *positional = named ? By_ShapeArgs(args, receiver) : NULL;
        if (positional != NULL) {
            PyObject *answer = PyObject_Call(shape, positional, kwds);
            Py_DECREF(positional);
            if (answer != NULL) Py_DECREF(answer);
            else if (PyErr_ExceptionMatches(PyExc_TypeError)) reworded = 1;
        }
    }
    Py_XDECREF(source);
    Py_XDECREF(scope);
    /* a shape that could not be built, or one that refused for a reason of its own,
     * leaves the thread as it found it */
    if (!reworded) PyErr_Clear();
    return reworded;
}

/* the constructor's binding: the same rules [`By_BindArgs`] applies, read off a tuple
 * and a dict rather than a fastcall vector — which is the whole of what differs
 *
 * `out[i]` receives a *borrowed* pointer, or NULL where the caller supplied nothing
 * and the default fills it. python counts `self` in its arity message and not in its
 * missing-argument one, so this does too */
static inline int By_BindInitPlain(PyObject *args, PyObject *kwds,
                                   const char *const *names, Py_ssize_t count,
                                   const unsigned char *required, Py_ssize_t posonly,
                                   Py_ssize_t kwonly, PyObject **out, int variadic,
                                   int extras, const char *fname, int inherited) {
    for (Py_ssize_t i = 0; i < count; i++) out[i] = NULL;
    Py_ssize_t nargs = args == NULL ? 0 : PyTuple_GET_SIZE(args);
    /* a keyword-only parameter is one nothing positional can reach, so the run a
     * caller may fill positionally ends where they begin */
    Py_ssize_t positional_limit = count - kwonly;
    /* a class with no `__init__` at all is rejected by `object.__init__`, which names
     * the class, does not count a receiver it never had, and asks only whether it was
     * given anything — a keyword is as much an excess argument as a positional, and
     * saying which one would be a distinction `object_init` never draws. such a class
     * takes no parameters at all, so there is nothing else the call could be about. a
     * *written* `def __init__(self)` takes no arguments either and still reports as a
     * method, which is why this turns on how the class was written and not on `count` */
    if (inherited) {
        if (nargs > 0 || (kwds != NULL && PyDict_Size(kwds) > 0)) {
            PyErr_Format(PyExc_TypeError, "%s() takes no arguments", fname);
            return -1;
        }
    } else if (nargs > positional_limit && !variadic) {
        return By_TooManyPositional(fname, required, positional_limit, nargs, 1);
    }
    Py_ssize_t positional = nargs < positional_limit ? nargs : positional_limit;
    for (Py_ssize_t i = 0; i < positional; i++) out[i] = PyTuple_GET_ITEM(args, i);
    if (kwds != NULL) {
        PyObject *key = NULL, *value = NULL;
        Py_ssize_t pos = 0;
        while (PyDict_Next(kwds, &pos, &key, &value)) {
            const char *text = PyUnicode_AsUTF8(key);
            if (text == NULL) return -1;
            Py_ssize_t found = By_NamedSlot(text, names, count, posonly);
            if (found < 0) {
                /* a `**kwargs` parameter takes it; without one it is an error */
                if (extras) continue;
                return By_RejectKeyword(text, names, posonly, fname);
            }
            if (out[found] != NULL) {
                PyErr_Format(PyExc_TypeError, "%s() got multiple values for argument '%s'",
                             fname, text);
                return -1;
            }
            out[found] = value;
        }
    }
    return By_CheckRequired(names, required, count, kwonly, out, fname);
}

/* the same binding, with a refusal put back into the interpreter's own words
 *
 * the plain binding writes nothing but `out`, and rewrites all of it before reading any
 * — so where the shape declines to reword, running it a second time is how its own
 * message comes back, and nothing has to be carried across the attempt */
static inline int By_BindInit(PyObject *args, PyObject *kwds, const char *const *names,
                              Py_ssize_t count, const unsigned char *required,
                              Py_ssize_t posonly, Py_ssize_t kwonly, PyObject **out,
                              int variadic, int extras, const char *fname, int inherited) {
    if (By_BindInitPlain(args, kwds, names, count, required, posonly, kwonly, out, variadic,
                         extras, fname, inherited) == 0) {
        return 0;
    }
    /* a class that wrote no `__init__` is refused by `object.__init__`, which is not a
     * python function and has no shape to model. and a refusal that is not a `TypeError`
     * is not an arity one — it is the binding itself having failed */
    if (inherited || !PyErr_ExceptionMatches(PyExc_TypeError)) return -1;
    PyErr_Clear();
    if (By_Rephrase(names, required, count, posonly, kwonly, variadic, extras,
                    fname, 1, args, kwds)) {
        return -1;
    }
    return By_BindInitPlain(args, kwds, names, count, required, posonly, kwonly, out,
                            variadic, extras, fname, inherited);
}

/* bind fastcall arguments to parameter positions, honouring keywords.
 *
 * `receiver` is 1 for a method, whose `self` arrives outside the vector but which
 * python still counts in an arity message.
 *
 * `out[i]` receives a *borrowed* pointer, or NULL where the caller did not supply
 * that parameter — the wrapper fills those from the defaults. returns -1 with an
 * exception set on a duplicate, an unexpected name, or too many positionals */
static inline int By_BindArgsPlain(PyObject *const *args, Py_ssize_t nargs,
                                   PyObject *kwnames, const char *const *names,
                                   Py_ssize_t count, const unsigned char *required,
                                   Py_ssize_t posonly, Py_ssize_t kwonly, PyObject **out,
                                   int variadic, int extras, const char *fname,
                                   Py_ssize_t receiver) {
    /* a keyword-only parameter is one nothing positional can reach, so the run a
     * caller may fill positionally ends where they begin */
    Py_ssize_t positional_limit = count - kwonly;
    for (Py_ssize_t i = 0; i < count; i++) out[i] = NULL;
    if (nargs > positional_limit && !variadic) {
        return By_TooManyPositional(fname, required, positional_limit, nargs, receiver);
    }
    Py_ssize_t positional = nargs < positional_limit ? nargs : positional_limit;
    for (Py_ssize_t i = 0; i < positional; i++) out[i] = args[i];
    if (kwnames == NULL) return By_CheckRequired(names, required, count, kwonly, out, fname);
    Py_ssize_t keywords = PyTuple_GET_SIZE(kwnames);
    for (Py_ssize_t k = 0; k < keywords; k++) {
        PyObject *name = PyTuple_GET_ITEM(kwnames, k);
        const char *text = PyUnicode_AsUTF8(name);
        if (text == NULL) return -1;
        Py_ssize_t found = By_NamedSlot(text, names, count, posonly);
        if (found < 0) {
            /* a `**kwargs` parameter takes it; without one it is an error */
            if (extras) continue;
            return By_RejectKeyword(text, names, posonly, fname);
        }
        if (out[found] != NULL) {
            PyErr_Format(PyExc_TypeError, "%s() got multiple values for argument '%s'",
                         fname, text);
            return -1;
        }
        out[found] = args[nargs + k];
    }
    return By_CheckRequired(names, required, count, kwonly, out, fname);
}

/* a fastcall vector as the tuple and dict a plain call takes. only the error path needs
 * either, and it is cold */
static inline PyObject *By_VectorTuple(PyObject *const *args, Py_ssize_t nargs) {
    PyObject *made = PyTuple_New(nargs);
    if (made == NULL) return NULL;
    for (Py_ssize_t i = 0; i < nargs; i++) PyTuple_SET_ITEM(made, i, By_NewRef(args[i]));
    return made;
}

static inline PyObject *By_VectorKwds(PyObject *const *args, Py_ssize_t nargs,
                                      PyObject *kwnames) {
    PyObject *made = PyDict_New();
    if (made == NULL || kwnames == NULL) return made;
    Py_ssize_t keywords = PyTuple_GET_SIZE(kwnames);
    for (Py_ssize_t k = 0; k < keywords; k++) {
        if (PyDict_SetItem(made, PyTuple_GET_ITEM(kwnames, k), args[nargs + k]) < 0) {
            Py_DECREF(made);
            return NULL;
        }
    }
    return made;
}

/* the same binding, with a refusal put back into the interpreter's own words */
static inline int By_BindArgs(PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames,
                              const char *const *names, Py_ssize_t count,
                              const unsigned char *required, Py_ssize_t posonly,
                              Py_ssize_t kwonly, PyObject **out, int variadic, int extras,
                              const char *fname, Py_ssize_t receiver) {
    if (By_BindArgsPlain(args, nargs, kwnames, names, count, required, posonly, kwonly, out,
                         variadic, extras, fname, receiver) == 0) {
        return 0;
    }
    if (!PyErr_ExceptionMatches(PyExc_TypeError)) return -1;
    PyErr_Clear();
    PyObject *tuple = By_VectorTuple(args, nargs);
    PyObject *dict = tuple == NULL ? NULL : By_VectorKwds(args, nargs, kwnames);
    int reworded = dict != NULL
                   && By_Rephrase(names, required, count, posonly, kwonly, variadic, extras,
                                  fname, receiver, tuple, dict);
    Py_XDECREF(tuple);
    Py_XDECREF(dict);
    if (reworded) return -1;
    PyErr_Clear();
    return By_BindArgsPlain(args, nargs, kwnames, names, count, required, posonly, kwonly,
                            out, variadic, extras, fname, receiver);
}

/* the binding for a boundary whose parameters are all required and all reachable
 * positionally: no `*args` or `**kwargs` to collect into, and no positional-only or
 * keyword-only run
 *
 * such a boundary handed exactly one positional argument per parameter and no
 * keywords is what [`By_BindArgsPlain`] would have walked its way to: the vector is
 * copied across unchanged and nothing is missing. saying so directly is worth about
 * a tenth of what it costs to reach a compiled function through its wrapper at all,
 * which is the whole of the gap on a call whose body is one arithmetic operation
 *
 * every other call — a keyword, too few arguments, too many — goes the long way, so
 * the refusals and their wording are still decided in exactly one place */
static inline int By_BindArgsQuick(PyObject *const *args, Py_ssize_t nargs,
                                   PyObject *kwnames, const char *const *names,
                                   Py_ssize_t count, const unsigned char *required,
                                   PyObject **out, const char *fname,
                                   Py_ssize_t receiver) {
    if (BY_LIKELY(nargs == count && kwnames == NULL)) {
        for (Py_ssize_t i = 0; i < count; i++) out[i] = args[i];
        return 0;
    }
    return By_BindArgs(args, nargs, kwnames, names, count, required, 0, 0, out, 0, 0, fname,
                       receiver);
}

/* `with EXPR`: the manager's `__enter__`, looked up on the *type* the way the
 * interpreter does rather than on the instance */
/* what one `with` block's protocol lookup resolved to last time
 *
 * interning the name took the lookup down from three quarters of the `with_`
 * benchmark's compiled loop to a quarter of it, and a quarter is still more than
 * either of the calls it resolves costs. the same block enters and leaves the same
 * manager every pass, so the answer is the same answer every time.
 *
 * this is a cache with a validity test rather than an assumption, and the test is the
 * one the interpreter's own specialiser makes: the receiver's type, and that type's
 * version tag. rebinding `__enter__` on the class — or on any of its bases — runs
 * `PyType_Modified`, which zeroes the tag on the class and on every subclass, so a
 * site holding the old answer stops matching at the very next call. a manager whose
 * `__class__` was reassigned since arrives with a different type and misses on the
 * pointer.
 *
 * the type is compared and never dereferenced, so a type freed since cannot be read
 * through the pointer; and a version tag is drawn from a counter that only ever rises,
 * so a type built where a freed one stood cannot answer to the freed one's version.
 *
 * on a free-threaded build there is no site at all: the fields cannot be read or
 * written as one, and an emitted module says `Py_MOD_GIL_NOT_USED`, so two threads
 * arming at once could otherwise leave one type's version paired with another type's
 * method — which is not a wrong answer but a call into the wrong object */
typedef struct {
    PyObject *type;
    unsigned int version;
    /* borrowed. what is kept is the entry the type's own dict holds, and the version
     * tag is what stands for that entry not having moved — see [`By_ArmProtocolSite`] */
    PyObject *method;
    unsigned int misses;
} ByProtocolSite;

#define BY_PROTOCOL_SITE_INIT { NULL, 0u, NULL, 0u }

/* how many types a site sees before it settles for the ordinary lookup, on the same
 * reasoning as [`BY_METHOD_SITE_MISSES`]: a site still re-deriving by now is one whose
 * managers vary, and each further attempt is a lookup on top of the one it saves */
#define BY_PROTOCOL_SITE_MISSES 8u

#ifndef Py_GIL_DISABLED

/* work out what `name` on `tp` is, and record it — or record that it cannot be served
 *
 * the type and its version are recorded before the first thing that can refuse, so a
 * manager this site will never be able to serve is asked about once and then costs the
 * same two comparisons as a hit */
static void By_ArmProtocolSite(ByProtocolSite *site, PyTypeObject *tp, PyObject *name) {
    PyObject *found;
    site->type = (PyObject *)tp;
    site->version = tp->tp_version_tag;
    site->method = NULL;
    /* the entry the type's mro holds, which is what the interpreter looks a special
     * method up as. it is borrowed from that dict and no descriptor runs to find it, so
     * nothing a `__get__` builds on the way — a `functools.partialmethod` builds a new
     * function every time — can be what is kept */
    found = _PyType_Lookup(tp, name);
    if (found == NULL) return;
    /* the two that bind to the manager by taking it as their first argument, so calling
     * the entry with the manager in front is what binding it would have done. anything
     * else binds some other way, or not at all, and is looked up in full every time */
    if (!PyFunction_Check(found) && By_MethodDescriptorOf(found) == NULL) return;
#if PY_VERSION_HEX >= 0x030C0000
    /* from 3.12 the tag is handed out on request rather than by whoever reads an
     * attribute, and a request is the only thing that reliably produces one */
    if (!PyUnstable_Type_AssignVersionTag(tp)) return;
#endif
    site->version = tp->tp_version_tag;
    /* zero is both "never given a tag" and "written to since", and either is a refusal */
    if (site->version == 0u) return;
    site->method = found;
}

#endif /* Py_GIL_DISABLED */

/* a special method of `self`, looked up the way the interpreter looks one up for a
 * protocol: on the type alone, past the instance and past any metaclass, and bound to
 * `self` through the descriptor protocol
 *
 * `*prepend` says whether the answer still wants `self` in front of the arguments. a
 * plain function and a method descriptor are handed back unbound, which saves building
 * the bound method they would give; everything else comes back already bound, or is
 * not a descriptor and is called as it is. NULL with no exception set is a name the type
 * does not have */
static inline PyObject *By_LookupSpecial(PyObject *self, PyObject *name, int *prepend) {
    PyObject *found = _PyType_Lookup(Py_TYPE(self), name);
    *prepend = 0;
    if (found == NULL) return NULL;
    if (PyFunction_Check(found) || By_MethodDescriptorOf(found) != NULL) {
        *prepend = 1;
        return Py_NewRef(found);
    }
    descrgetfunc get = Py_TYPE(found)->tp_descr_get;
    if (get == NULL) return Py_NewRef(found);
    /* the entry is only borrowed, and `__get__` is arbitrary python */
    Py_INCREF(found);
    PyObject *bound = get(found, self, (PyObject *)Py_TYPE(self));
    Py_DECREF(found);
    return bound;
}

/* the direction of a binary operator a class did not write, answered the way python's own
 * number slot answers it: by looking the name up on the operand's type, which reaches
 * whatever a base defines under it
 *
 * a `list` subclass writing only `__mul__` still answers `2 * bag` through `list.__rmul__`
 * this way. `adapter` is the slot asking, and an entry wrapping that same slot is refused
 * rather than called, since calling it would only ask this function again */
static inline PyObject *By_InheritedBinary(PyObject *receiver, PyObject *other,
                                           const char *name, void *adapter) {
    PyObject *key = PyUnicode_InternFromString(name);
    if (key == NULL) return NULL;
    PyObject *found = _PyType_Lookup(Py_TYPE(receiver), key);
    if (found == NULL
        || (Py_IS_TYPE(found, &PyWrapperDescr_Type)
            && ((PyWrapperDescrObject *)found)->d_wrapped == adapter)) {
        Py_DECREF(key);
        Py_RETURN_NOTIMPLEMENTED;
    }
    int prepend;
    PyObject *method = By_LookupSpecial(receiver, key, &prepend);
    Py_DECREF(key);
    if (method == NULL) {
        if (PyErr_Occurred()) return NULL;
        Py_RETURN_NOTIMPLEMENTED;
    }
    PyObject *result;
    if (prepend) {
        PyObject *argv[] = { receiver, other };
        result = PyObject_Vectorcall(method, argv, 2, NULL);
    } else {
        PyObject *argv[] = { other };
        result = PyObject_Vectorcall(method, argv, 1, NULL);
    }
    Py_DECREF(method);
    return result;
}

/* [`By_LookupSpecial`] through a memo of what it last answered */
static inline PyObject *By_ProtocolMethod(ByProtocolSite *site, PyObject *manager,
                                          PyObject *name, int *prepend) {
    PyTypeObject *tp = Py_TYPE(manager);
#ifndef Py_GIL_DISABLED
    if (BY_UNLIKELY((PyObject *)tp != site->type || tp->tp_version_tag != site->version)) {
        if (site->misses >= BY_PROTOCOL_SITE_MISSES) {
            return By_LookupSpecial(manager, name, prepend);
        }
        site->misses++;
        By_ArmProtocolSite(site, tp, name);
    }
    if (BY_LIKELY(site->method != NULL)) {
        *prepend = 1;
        return By_NewRef(site->method);
    }
#else
    (void)site;
#endif
    return By_LookupSpecial(manager, name, prepend);
}

/* call what [`By_ProtocolMethod`] answered with `args`, whose first slot holds the
 * manager and is left out where the answer is already bound */
static inline PyObject *By_CallProtocol(PyObject *method, int prepend, PyObject **args,
                                        Py_ssize_t nargs) {
    if (prepend) return PyObject_Vectorcall(method, args, (size_t)nargs, NULL);
    return PyObject_Vectorcall(method, args + 1,
                               (size_t)(nargs - 1) | PY_VECTORCALL_ARGUMENTS_OFFSET, NULL);
}

/* whether a manager's type has `name`, the half of the protocol a `with` calls on the way
 * out: 1 or 0, and -1 with an exception set
 *
 * python looks both halves up before it calls either, so a manager missing `__exit__` is
 * refused before its `__enter__` runs. only the presence is asked here, and the half is
 * looked up again where it is called. a site whose memo still stands for this type was
 * armed by a call that got past this test already, and a type written to since is a
 * miss */
static inline int By_ManagerHas(ByProtocolSite *site, PyObject *manager, PyObject **cache,
                                const char *name, Py_ssize_t length) {
    PyTypeObject *tp = Py_TYPE(manager);
    PyObject *key;
#ifndef Py_GIL_DISABLED
    if (BY_LIKELY((PyObject *)tp == site->type && tp->tp_version_tag == site->version
                  && site->method != NULL)) {
        return 1;
    }
#else
    (void)site;
#endif
    key = By_FixedName(cache, name, length);
    if (key == NULL) return -1;
    return _PyType_Lookup(tp, key) != NULL;
}

/* the refusal for a manager whose type lacks `exit`, the half a `with` calls on the way
 * out, where `has_enter` says whether it has the other half
 *
 * from 3.14 the half called on the way out is looked up first, and the message names
 * whichever half is missing; before that the half called on the way in is looked up
 * first, and only a missing exit is named */
BY_COLD void By_ManagerMissingExit(PyObject *manager, const char *protocol, const char *exit,
                                   int has_enter) {
#if PY_VERSION_HEX >= 0x030E0000
    (void)has_enter;
    PyErr_Format(PyExc_TypeError, "'%s' object does not support the %s protocol (missed %s method)",
                 Py_TYPE(manager)->tp_name, protocol, exit);
#else
    if (!has_enter) {
        PyErr_Format(PyExc_TypeError, "'%s' object does not support the %s protocol",
                     Py_TYPE(manager)->tp_name, protocol);
        return;
    }
    PyErr_Format(PyExc_TypeError, "'%s' object does not support the %s protocol (missed %s method)",
                 Py_TYPE(manager)->tp_name, protocol, exit);
#endif
}

/* the refusal for a manager whose type has the half called on the way out and lacks
 * `enter`, the one called on the way in */
BY_COLD void By_ManagerMissingEnter(PyObject *manager, const char *protocol, const char *enter) {
#if PY_VERSION_HEX >= 0x030E0000
    PyErr_Format(PyExc_TypeError, "'%s' object does not support the %s protocol (missed %s method)",
                 Py_TYPE(manager)->tp_name, protocol, enter);
#else
    (void)enter;
    PyErr_Format(PyExc_TypeError, "'%s' object does not support the %s protocol",
                 Py_TYPE(manager)->tp_name, protocol);
#endif
}

/* a manager's `enter` and its callable, after checking it has `exit` as python does before
 * calling either — NULL with an exception set where it lacks one */
static inline PyObject *By_ManagerEnter(ByProtocolSite *site, PyObject *manager,
                                        PyObject **enter_name, const char *enter,
                                        PyObject **exit_name, const char *exit,
                                        const char *protocol, int *prepend) {
    PyObject *name;
    PyObject *method;
    int has_exit = By_ManagerHas(site, manager, exit_name, exit, (Py_ssize_t)strlen(exit));
    if (has_exit < 0) return NULL;
    name = By_FixedName(enter_name, enter, (Py_ssize_t)strlen(enter));
    if (name == NULL) return NULL;
    if (!has_exit) {
        By_ManagerMissingExit(manager, protocol, exit, _PyType_Lookup(Py_TYPE(manager), name) != NULL);
        return NULL;
    }
    method = By_ProtocolMethod(site, manager, name, prepend);
    if (method == NULL && !PyErr_Occurred()) By_ManagerMissingEnter(manager, protocol, enter);
    return method;
}

/* `__aenter__` and `__aexit__`, which hand back *awaitables* rather than answers
 *
 * so these only start the call — the caller awaits what comes back, and only then
 * has the value `async with` binds or the answer that decides suppression
 */
static inline PyObject *By_AsyncEnter(ByProtocolSite *site, PyObject *manager) {
    static PyObject *by_aenter = NULL;
    static PyObject *by_aexit = NULL;
    if (manager == NULL) return NULL;
    int prepend;
    PyObject *method = By_ManagerEnter(site, manager, &by_aenter, "__aenter__", &by_aexit,
                                       "__aexit__", "asynchronous context manager", &prepend);
    if (method == NULL) return NULL;
    PyObject *args[1] = {manager};
    PyObject *result = By_CallProtocol(method, prepend, args, 1);
    Py_DECREF(method);
    return result;
}

/* the half of the protocol a `with` calls on its way out — `__exit__`, or `__aexit__`
 * for `async with` — looked up as the block is entered, which is where python binds it
 *
 * the answer is what [`By_CallExit`] calls with. a plain function is kept as it is and
 * called with the manager in front, as the lookup does for every call it makes; anything
 * a descriptor handed back is already bound, and is kept in a one-element tuple so the
 * exit knows not to add the manager. a function is never a tuple, so the two cannot be
 * taken for each other */
static inline PyObject *By_BindExit(ByProtocolSite *site, PyObject *manager, int is_async) {
    static PyObject *by_exit = NULL;
    static PyObject *by_aexit = NULL;
    static PyObject *by_enter = NULL;
    static PyObject *by_aenter = NULL;
    if (manager == NULL) return NULL;
    PyObject *name = is_async ? By_FixedName(&by_aexit, "__aexit__", 9)
                              : By_FixedName(&by_exit, "__exit__", 8);
    if (name == NULL) return NULL;
    int prepend;
    PyObject *method = By_ProtocolMethod(site, manager, name, &prepend);
    if (method == NULL) {
        if (PyErr_Occurred()) return NULL;
        PyObject *enter = is_async ? By_FixedName(&by_aenter, "__aenter__", 10)
                                   : By_FixedName(&by_enter, "__enter__", 9);
        if (enter == NULL) return NULL;
        By_ManagerMissingExit(manager, is_async ? "asynchronous context manager" : "context manager",
                              is_async ? "__aexit__" : "__exit__",
                              _PyType_Lookup(Py_TYPE(manager), enter) != NULL);
        return NULL;
    }
    if (prepend) return method;
    PyObject *bound = PyTuple_Pack(1, method);
    Py_DECREF(method);
    return bound;
}

/* call what [`By_BindExit`] bound, with `args` holding the manager in its first slot */
static inline PyObject *By_CallExit(PyObject *exit, PyObject **args, Py_ssize_t nargs) {
    if (PyTuple_CheckExact(exit)) return By_CallProtocol(PyTuple_GET_ITEM(exit, 0), 0, args, nargs);
    return By_CallProtocol(exit, 1, args, nargs);
}

/* the arguments an exit is called with: the exception's type, value and traceback, or three
 * `None`s on the normal path. the traceback is a new reference where there is one */
static inline void By_ExitArguments(PyObject *exception, PyObject **args) {
    int raising = exception != NULL && exception != Py_None
                  && PyExceptionInstance_Check(exception);
    args[0] = raising ? (PyObject *)Py_TYPE(exception) : Py_None;
    args[1] = raising ? exception : Py_None;
    PyObject *found = raising ? PyException_GetTraceback(exception) : NULL;
    args[2] = found != NULL ? found : Py_None;
}

static inline PyObject *By_AsyncExit(PyObject *manager, PyObject *exit, PyObject *exception) {
    if (manager == NULL || exit == NULL) return NULL;
    PyObject *args[4] = {manager};
    By_ExitArguments(exception, args + 1);
    PyObject *result = By_CallExit(exit, args, 4);
    if (args[3] != Py_None) Py_DECREF(args[3]);
    return result;
}

static inline PyObject *By_Enter(ByProtocolSite *site, PyObject *manager) {
    static PyObject *by_enter = NULL;
    static PyObject *by_exit = NULL;
    if (manager == NULL) return NULL;
    int prepend;
    PyObject *method = By_ManagerEnter(site, manager, &by_enter, "__enter__", &by_exit,
                                       "__exit__", "context manager", &prepend);
    if (method == NULL) return NULL;
    PyObject *args[1] = {manager};
    PyObject *result = By_CallProtocol(method, prepend, args, 1);
    Py_DECREF(method);
    return result;
}

/* `__exit__`, on the normal path (`exception` NULL) or the exceptional one.
 *
 * returns 1 when the exception was *suppressed*, 0 when it was not, and -1 when
 * `__exit__` itself raised. the caller re-raises on 0, which is what makes
 * `with` transparent to an exception it does not swallow */
static inline int By_ExitContext(PyObject *manager, PyObject *exit, PyObject *exception) {
    if (manager == NULL || exit == NULL) return -1;
    /* `None` is the normal path just as NULL is: the frontend hands over a boxed
       `None`, and reading a traceback off it would be a wild pointer */
    int raising = exception != NULL && exception != Py_None
                  && PyExceptionInstance_Check(exception);
    PyObject *args[4] = {manager};
    By_ExitArguments(exception, args + 1);
    PyObject *result = By_CallExit(exit, args, 4);
    if (args[3] != Py_None) Py_DECREF(args[3]);
    if (result == NULL) return -1;
    int suppressed = PyObject_IsTrue(result);
    Py_DECREF(result);
    if (suppressed < 0) return -1;
    /* on the normal path `__exit__`'s answer is ignored — there is nothing to
       suppress, and a truthy return must not look like a suppressed exception */
    return raising ? suppressed : 0;
}

/* declare a type a coroutine to `collections.abc`.
 *
 * `asyncio.iscoroutine` tests `isinstance(x, collections.abc.Coroutine)`, and an
 * extension type that merely answers `__await__` is not one until it registers */
static inline int By_RegisterCoroutine(PyObject *type) {
    PyObject *module = PyImport_ImportModule("collections.abc");
    if (module == NULL) return -1;
    PyObject *abc = PyObject_GetAttrString(module, "Coroutine");
    Py_DECREF(module);
    if (abc == NULL) return -1;
    PyObject *result = PyObject_CallMethod(abc, "register", "O", type);
    Py_DECREF(abc);
    if (result == NULL) return -1;
    Py_DECREF(result);
    return 0;
}

/* `PyIter_Send` is the call the `SEND` opcode makes, and 3.11 is the floor, so there is
 * nothing older to stand in for it */
#define By_IterSend PyIter_Send

/* one step of delegation: send `sent` into `inner` and report what happened.
 *
 * three outcomes, and they have to be distinguishable without an exception check at
 * every use: a yielded value, a return value, or a real error. `*done` says which of
 * the first two, and NULL with an exception set is the third
 *
 * `PyIter_Send` *is* that contract, and it is the call the `SEND` opcode makes.
 * that matters twice over. a generator or coroutine answers the `am_send` slot,
 * which reports a return without ever building the `StopIteration`; and where one
 * does have to be built, the rule for reading its value back is subtle enough to be
 * worth borrowing rather than restating — a bare `StopIteration` carries `None`, a
 * subclass may carry anything, and a raised *type* has to be made an instance first */
static inline PyObject *By_DelegateStep(PyObject *inner, PyObject *sent, int *done) {
    PyObject *result = NULL;
    PySendResult outcome = By_IterSend(inner, sent == NULL ? Py_None : sent, &result);
    if (outcome == PYGEN_ERROR) {
        *done = 0;
        return NULL;
    }
    *done = outcome == PYGEN_RETURN;
    return result;
}

/* whether a generator's code carries `CO_ITERABLE_COROUTINE`, which `types.coroutine`
 * sets to let `await` drive it: 1 or 0, and -1 with an exception set */
static int By_GeneratorIsCoroutine(PyObject *generator) {
    PyObject *code;
    int flags;
#if PY_VERSION_HEX >= 0x030C0000
    code = (PyObject *)PyGen_GetCode((PyGenObject *)generator);
#else
    code = PyObject_GetAttrString(generator, "gi_code");
#endif
    if (code == NULL) return -1;
    flags = PyCode_Check(code) ? ((PyCodeObject *)code)->co_flags : 0;
    Py_DECREF(code);
    return (flags & CO_ITERABLE_COROUTINE) != 0;
}

/* the iterator `__await__` hands back for a compiled coroutine: python's
 * `coroutine_wrapper`
 *
 * a coroutine is awaitable and is not an iterator — `next(coro)` is a `TypeError` — but
 * pep 492 says `__await__` owes an iterator, so it hands back this object wrapped
 * around the coroutine. every step forwards: the coroutine's send slot for `__next__`
 * and for a caller that sends, and its own `send`, `throw` and `close` methods by name.
 *
 * an `await` compiled here never builds one — see `By_AwaitIter` — and neither does
 * python's own for its own coroutines. it exists for the other callers of `__await__`:
 * an interpreted `await`, and anything that asks for the method directly */
typedef struct {
    PyObject_HEAD
    PyObject *coroutine;
} ByCoroutineWrapper;

static void By_CoroutineWrapper_dealloc(PyObject *self) {
    PyObject_GC_UnTrack(self);
    Py_XDECREF(((ByCoroutineWrapper *)self)->coroutine);
    Py_TYPE(self)->tp_free(self);
}

static int By_CoroutineWrapper_traverse(PyObject *self, visitproc visit, void *arg) {
    Py_VISIT(((ByCoroutineWrapper *)self)->coroutine);
    return 0;
}

static PySendResult By_CoroutineWrapper_send_slot(PyObject *self, PyObject *arg,
                                                  PyObject **result) {
    PyObject *coroutine = ((ByCoroutineWrapper *)self)->coroutine;
    return Py_TYPE(coroutine)->tp_as_async->am_send(coroutine, arg, result);
}

/* python's `gen_iternext`: a return of `None` ends the iteration with no exception at all,
 * and any other return value rides out on the `StopIteration` */
static PyObject *By_CoroutineWrapper_next(PyObject *self) {
    PyObject *result = NULL;
    PySendResult outcome = By_CoroutineWrapper_send_slot(self, Py_None, &result);
    if (outcome != PYGEN_RETURN) return result;
    if (result != Py_None) By_RaiseWith(PyExc_StopIteration, result);
    Py_DECREF(result);
    return NULL;
}

static PyObject *By_CoroutineWrapper_send(PyObject *self, PyObject *arg) {
    PyObject *name = PyUnicode_InternFromString("send");
    if (name == NULL) return NULL;
    PyObject *result = PyObject_CallMethodOneArg(((ByCoroutineWrapper *)self)->coroutine, name, arg);
    Py_DECREF(name);
    return result;
}

static PyObject *By_CoroutineWrapper_throw(PyObject *self, PyObject *const *args,
                                           Py_ssize_t nargs) {
    if (By_CountThrowArguments("throw", nargs) < 0) return NULL;
    PyObject *name = PyUnicode_InternFromString("throw");
    if (name == NULL) return NULL;
    PyObject *forwarded[4] = {((ByCoroutineWrapper *)self)->coroutine};
    for (Py_ssize_t i = 0; i < nargs; i++) forwarded[i + 1] = args[i];
    PyObject *result = PyObject_VectorcallMethod(name, forwarded, (size_t)nargs + 1, NULL);
    Py_DECREF(name);
    return result;
}

static PyObject *By_CoroutineWrapper_close(PyObject *self, PyObject *unused) {
    (void)unused;
    PyObject *name = PyUnicode_InternFromString("close");
    if (name == NULL) return NULL;
    PyObject *result = PyObject_CallMethodNoArgs(((ByCoroutineWrapper *)self)->coroutine, name);
    Py_DECREF(name);
    return result;
}

static PyMethodDef By_CoroutineWrapper_methods[] = {
    {"send", By_CoroutineWrapper_send, METH_O, NULL},
    {"throw", (PyCFunction)(void (*)(void))By_CoroutineWrapper_throw, METH_FASTCALL, NULL},
    {"close", By_CoroutineWrapper_close, METH_NOARGS, NULL},
    {NULL, NULL, 0, NULL},
};

static PyAsyncMethods By_CoroutineWrapper_async = {
    .am_send = By_CoroutineWrapper_send_slot,
};

static PyTypeObject By_CoroutineWrapperType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.coroutine_wrapper",
    .tp_basicsize = sizeof(ByCoroutineWrapper),
    .tp_itemsize = 0,
    .tp_dealloc = By_CoroutineWrapper_dealloc,
    .tp_as_async = &By_CoroutineWrapper_async,
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_DISALLOW_INSTANTIATION,
    .tp_traverse = By_CoroutineWrapper_traverse,
    .tp_iter = PyObject_SelfIter,
    .tp_iternext = By_CoroutineWrapper_next,
    .tp_methods = By_CoroutineWrapper_methods,
};

/* `__await__` of every compiled coroutine, one function for all of them so that an await
 * can recognise one by its slot */
static PyObject *By_CoroutineAwait(PyObject *coroutine) {
    if (PyType_Ready(&By_CoroutineWrapperType) < 0) return NULL;
    ByCoroutineWrapper *wrapper = PyObject_GC_New(ByCoroutineWrapper, &By_CoroutineWrapperType);
    if (wrapper == NULL) return NULL;
    wrapper->coroutine = By_NewRef(coroutine);
    PyObject_GC_Track(wrapper);
    return (PyObject *)wrapper;
}

/* the iterator a delegation drives: `iter(x)` for `yield from`, `x.__await__()` for
 * `await`. keeping them apart matters — awaiting an ordinary iterable is an error
 *
 * this is `GET_AWAITABLE`'s own resolution, down to the checks it makes on what
 * `__await__` handed back. reaching the slot rather than the attribute is the point:
 * `PyObject_GetAttrString` builds a fresh `str` per await, which misses the type
 * method cache — that cache compares name *pointers* — and then allocates a bound
 * method-wrapper to call once and throw away */
static inline PyObject *By_AwaitIter(PyObject *awaitable) {
    PyTypeObject *type;
    unaryfunc getter;
    PyObject *iterator;
    if (awaitable == NULL) return NULL;
    /* a coroutine is already the thing to drive: its own `__await__` only hands
     * back a wrapper around itself */
    if (PyCoro_CheckExact(awaitable)) return By_NewRef(awaitable);
    /* and so is a generator `types.coroutine` marked as one, which has no `__await__`
     * at all — python tells it by the flag on its code */
    if (PyGen_CheckExact(awaitable)) {
        int marked = By_GeneratorIsCoroutine(awaitable);
        if (marked < 0) return NULL;
        if (marked) return By_NewRef(awaitable);
    }
    type = Py_TYPE(awaitable);
    getter = type->tp_as_async == NULL ? NULL : type->tp_as_async->am_await;
    /* a compiled coroutine is driven through its own send slot, as python drives its own
     * coroutines, rather than through the wrapper `__await__` would build around it */
    if (getter == By_CoroutineAwait) return By_NewRef(awaitable);
    if (getter == NULL) {
        /* `_PyCoro_GetAwaitableIter` raises this, and it is core-only — so the wording
         * is carried, and 3.14 rewrote it */
#if PY_VERSION_HEX >= 0x030E0000
        PyErr_Format(PyExc_TypeError, "'%.100s' object can't be awaited", type->tp_name);
#else
        PyErr_Format(PyExc_TypeError, "object %.100s can't be used in 'await' expression",
                     type->tp_name);
#endif
        return NULL;
    }
    iterator = getter(awaitable);
    if (iterator == NULL) return NULL;
    /* pep 492: `__await__` owes an *iterator*. a delegation that took anything else
     * on trust would drive it through `send` and report the failure against the
     * wrong object */
    if (PyCoro_CheckExact(iterator)) {
        Py_DECREF(iterator);
        PyErr_SetString(PyExc_TypeError, "__await__() returned a coroutine");
        return NULL;
    }
    if (!PyIter_Check(iterator)) {
        PyErr_Format(PyExc_TypeError, "__await__() returned non-iterator of type '%.100s'",
                     Py_TYPE(iterator)->tp_name);
        Py_DECREF(iterator);
        return NULL;
    }
    return iterator;
}

/* raise a standard error carrying a value, which is how a generator's `return`
 * reaches its consumer: `StopIteration(value)`
 *
 * `PyErr_SetObject` leaves the exception uninstantiated where it can, which is
 * worth keeping — most raises through here are a frame finishing, and nothing
 * looks. but for two shapes the delay changes the answer, because the value is
 * then read as the *argument list*: a tuple is spread across it, so a returned
 * `(1, 2)` came back as `1`; and an exception instance is raised in place of the
 * error asked for, so a returned `StopIteration(9)` came back as `9`. python
 * instantiates exactly these two by hand, for exactly this reason */
static inline void By_RaiseWith(PyObject *error, PyObject *value) {
    if (value == NULL) return;
    if (PyTuple_Check(value) || PyExceptionInstance_Check(value)) {
        PyObject *built = PyObject_CallOneArg(error, value);
        if (built == NULL) return;
        PyErr_SetObject(error, built);
        Py_DECREF(built);
        return;
    }
    PyErr_SetObject(error, value);
}

/* an iterator that has already failed: stepping it hands back nothing and leaves the
 * pending exception exactly where it was.
 *
 * a frame that finishes *by raising* still has to be reported to `am_send`'s caller
 * as one of the three outcomes, and a `StopIteration` among those raises is a
 * *return* rather than an error. the rule for reading its value back is subtle — a
 * bare one carries `None`, a subclass carries whatever its own `value` holds, a
 * raised type has to be instantiated first, and a tuple must not be spread across the
 * constructor — and a copy of it that drifted would be a wrong answer about what a
 * frame returned. so rather than restate the rule, this asks for it: handing cpython
 * an iterator that has already failed is the shape `PyIter_Send` applies the rule to,
 * and the answer comes back the same as if the slot had never been there.
 *
 * the type is never readied and never instantiated. `PyIter_Send` reads `tp_as_async`
 * and `tp_iternext` off it and nothing else, and both are what the initializer says */
typedef struct {
    PyObject_HEAD
} ByRaisedIter;

static PyObject *By_RaisedIter_next(PyObject *self) {
    (void)self;
    return NULL;
}

static PyTypeObject By_RaisedIter_Type = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.raised",
    .tp_basicsize = sizeof(ByRaisedIter),
    .tp_flags = Py_TPFLAGS_DEFAULT,
    .tp_iternext = By_RaisedIter_next,
};

static ByRaisedIter By_RaisedIter = {PyObject_HEAD_INIT(&By_RaisedIter_Type)};

/* one step of a resumable frame, reported the way `PyIter_Send` reports one
 *
 * a yielded value, a return, or a real error — and the return arrives structurally,
 * out of `$returned`, which is the whole reason this slot is worth answering. that is
 * the difference between a completed `await` costing an exception and costing a
 * pointer read.
 *
 * `arg` is parked exactly as `send` parks it, `None` included — `PyIter_Send` with
 * `None` is `next()`, and `next()` is `send(None)`, so all three leave the suspended
 * `yield` evaluating to the same thing. treating `None` as "carries nothing" and
 * skipping the store is what used to let a value survive into a later `yield` */
static inline PySendResult By_SendGenerator(PyObject *self, PyObject **sent,
                                            PyObject **returned, int64_t *state, int frame,
                                            PyObject *(*resume)(PyObject *), PyObject *arg,
                                            PyObject **result) {
    PyObject *step;
    if (By_RefuseResumption(*state, frame, arg) < 0) return PYGEN_ERROR;
    By_ParkSent(sent, arg);
    step = resume(self);
    if (step != NULL) {
        *result = step;
        return PYGEN_NEXT;
    }
    By_FinishGenerator(state);
    step = *returned;
    if (step != NULL) {
        *returned = NULL;
        *result = step;
        return PYGEN_RETURN;
    }
    /* the frame raised rather than ended — this slot's other exit, and the one pep
     * 479 speaks about. converting here rather than only in `By_StepGenerator` is
     * what keeps `yield from` and `await`, which reach a frame through this slot,
     * from seeing an error the iterator protocol would not have shown them */
    By_ConvertStopIteration(frame);
    return By_IterSend((PyObject *)&By_RaisedIter, Py_None, result);
}

/* read a shared closure cell. a cell starts unset, exactly as a python cell does,
 * and reading one before it is written is `UnboundLocalError` rather than a zero */
static inline PyObject *By_ReadCell(PyObject *value, const char *name, int free) {
    if (value == NULL) {
        /* the *reading* frame decides: a frame that owns the name sees a local and
         * `UnboundLocalError`, one that closes over it sees a free variable and a
         * plain `NameError`. python distinguishes the two, wording included */
        PyErr_Format(free ? PyExc_NameError : PyExc_UnboundLocalError,
                     free ? "cannot access free variable '%s' where it is not associated with a value in enclosing scope"
                          : "cannot access local variable '%s' where it is not associated with a value",
                     name);
        return NULL;
    }
    return By_NewRef(value);
}

/* read a shared cell held as a tagged `int`. its error value is what unset means,
 * because a zero is a value like any other */
static inline ByTagged By_ReadCellTagged(ByTagged value, const char *name, int free) {
    if (value == BY_INT_ERROR) {
        (void)By_ReadCell(NULL, name, free);
        return BY_INT_ERROR;
    }
    By_IncRefTagged(value);
    return value;
}

/* the flag a type with a vectorcall slot sets, public from 3.12 */
#ifndef Py_TPFLAGS_HAVE_VECTORCALL
#define Py_TPFLAGS_HAVE_VECTORCALL _Py_TPFLAGS_HAVE_VECTORCALL
#endif

/* ── a compiled nested function ──────────────────────────────────────────────
 *
 * a nested function is a closure over its environment, and python's own is a
 * `function`: a descriptor, so installed on a class it binds the receiver; a thing
 * with a `__dict__` and a writable `__name__`, so `functools.wraps` can dress it; and
 * a thing that says where it was written, `counter.<locals>.step`, rather than what
 * holds its captures. a `PyCFunction` over the environment is none of those, and the
 * first is a silent wrong answer: a method installed from one never receives `self`.
 *
 * so a nested function is one of these. a call is `vectorcall` straight into the
 * function's own boundary, which reads the environment off the object — there is no
 * trampoline between the caller and the boundary. what the definition says about
 * itself is in a static `ByFunctionSpec`, and a name, qualname, module or docstring
 * only becomes an object when something reads it or writes over it.
 *
 * where the body reads its own name, the environment holds the function and the
 * function holds the environment, so both are collected types: this one traverses
 * the environment, and the environment traverses its cells. `tp_clear` leaves the
 * environment alone, so a call reached from a finalizer during a collection still has
 * one to read — clearing the environment's cells is what breaks the cycle.
 *
 * `copy` treats only `function` and `builtin_function_or_method` as atomic, so this
 * says it is atomic itself: a copy of a function is the function. and pickling one
 * pickles it by name, as python does, which for a nested function is the same refusal
 * python gives, in its own words */
typedef struct {
    /* the boundary, handed the function object itself as the callable */
    vectorcallfunc call;
    const char *name;
    const char *qualname;
    /* NULL where the definition has no docstring */
    const char *doc;
    /* the module's namespace, where `__module__` is read from */
    PyObject **globals;
    /* where the definition wrote annotations, the python source of the function that
     * evaluates them — see `by_irbuild::annotations` — and NULL where it wrote none */
    const char *annotations;
    /* whether `annotations` only raises, because the build could not say what they are:
     * then it raises where they are asked for, and not where the `def` stands */
    int annotations_refused;
    /* the values of the enclosing names the annotations read, as a list, from the
     * environment; NULL where they read none */
    PyObject *(*annotation_values)(PyObject *env);
    /* the function `annotations` defines, once it has been compiled */
    PyObject **annotation_factory;
    /* `(__defaults__, __kwdefaults__)` as the boundary fills them in, from the environment;
     * NULL where the definition has no defaults */
    PyObject *(*defaults)(PyObject *env);
    /* the interpreted module, whose code objects hold this definition's own */
    const By_Fallback *fallback;
    /* that code object, once it has been found */
    PyObject **code;
} ByFunctionSpec;

typedef struct {
    PyObject_HEAD
    vectorcallfunc vectorcall;
    PyObject *env;
    const ByFunctionSpec *spec;
    PyObject *name;
    PyObject *qualname;
    PyObject *module;
    PyObject *doc;
    PyObject *dict;
    /* the evaluated `__annotations__`, once there are some */
    PyObject *annotations;
    /* an `__annotate__` written over the definition's, `None` included, and `None` once
     * `__annotations__` has been written; NULL where neither was */
    PyObject *annotate;
    /* `__type_params__`, once it has been made or written */
    PyObject *type_params;
    /* the type parameters the definition made, which its annotations name whatever
     * `__type_params__` is written over with */
    PyObject *made_type_params;
    /* before 3.14, the values the enclosing names the annotations read held where the
     * `def` stood, kept until the annotations are first asked for — see
     * `By_AnnotateAtDefinition` */
    PyObject *annotation_values;
} ByFunctionObject;

static void By_Function_dealloc(ByFunctionObject *self) {
    PyObject_GC_UnTrack(self);
    Py_CLEAR(self->env);
    Py_CLEAR(self->name);
    Py_CLEAR(self->qualname);
    Py_CLEAR(self->module);
    Py_CLEAR(self->doc);
    Py_CLEAR(self->dict);
    Py_CLEAR(self->annotations);
    Py_CLEAR(self->annotate);
    Py_CLEAR(self->type_params);
    Py_CLEAR(self->made_type_params);
    Py_CLEAR(self->annotation_values);
    PyObject_GC_Del(self);
}

static int By_Function_traverse(ByFunctionObject *self, visitproc visit, void *arg) {
    Py_VISIT(self->env);
    Py_VISIT(self->module);
    Py_VISIT(self->doc);
    Py_VISIT(self->dict);
    Py_VISIT(self->annotations);
    Py_VISIT(self->annotate);
    Py_VISIT(self->type_params);
    Py_VISIT(self->made_type_params);
    Py_VISIT(self->annotation_values);
    return 0;
}

static int By_Function_clear(ByFunctionObject *self) {
    Py_CLEAR(self->module);
    Py_CLEAR(self->doc);
    Py_CLEAR(self->dict);
    Py_CLEAR(self->annotations);
    Py_CLEAR(self->annotate);
    Py_CLEAR(self->type_params);
    Py_CLEAR(self->made_type_params);
    Py_CLEAR(self->annotation_values);
    return 0;
}

/* bound the way a python function is: through the type it is itself, and through
 * an instance a method of that instance */
static PyObject *By_Function_descr_get(PyObject *self, PyObject *obj, PyObject *type) {
    (void)type;
    if (obj == NULL || obj == Py_None) return By_NewRef(self);
    return PyMethod_New(self, obj);
}

static PyObject *By_Function_repr(ByFunctionObject *self) {
    PyObject *qualname = self->qualname != NULL
        ? By_NewRef(self->qualname)
        : PyUnicode_FromString(self->spec->qualname);
    if (qualname == NULL) return NULL;
    PyObject *repr = PyUnicode_FromFormat("<function %U at %p>", qualname, (void *)self);
    Py_DECREF(qualname);
    return repr;
}

/* one of the two names: what was written over it, or else what the definition says */
static PyObject *By_Function_name_of(PyObject **slot, const char *spelled) {
    if (*slot == NULL) {
        *slot = PyUnicode_InternFromString(spelled);
        if (*slot == NULL) return NULL;
    }
    return By_NewRef(*slot);
}

static int By_Function_set_name_of(PyObject **slot, PyObject *value, const char *which) {
    if (value == NULL || !PyUnicode_Check(value)) {
        PyErr_Format(PyExc_TypeError, "%s must be set to a string object", which);
        return -1;
    }
    Py_XSETREF(*slot, By_NewRef(value));
    return 0;
}

static PyObject *By_Function_get_name(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    return By_Function_name_of(&function->name, function->spec->name);
}

static int By_Function_set_name(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    return By_Function_set_name_of(&((ByFunctionObject *)self)->name, value, "__name__");
}

static PyObject *By_Function_get_qualname(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    return By_Function_name_of(&function->qualname, function->spec->qualname);
}

static int By_Function_set_qualname(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    return By_Function_set_name_of(&((ByFunctionObject *)self)->qualname, value,
                                   "__qualname__");
}

/* python takes a function's `__module__` from its globals' `__name__` where the `def`
 * runs, and lets it be written over afterwards */
static PyObject *By_Function_get_module(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->module == NULL) {
        PyObject *globals = *function->spec->globals;
        PyObject *name = globals == NULL ? NULL : PyDict_GetItemString(globals, "__name__");
        function->module = By_NewRef(name == NULL ? Py_None : name);
    }
    return By_NewRef(function->module);
}

static int By_Function_set_module(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    Py_XSETREF(((ByFunctionObject *)self)->module, By_NewRef(value == NULL ? Py_None : value));
    return 0;
}

static PyObject *By_Function_get_doc(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->doc == NULL) {
        function->doc = function->spec->doc == NULL
            ? By_NewRef(Py_None)
            : PyUnicode_FromString(function->spec->doc);
        if (function->doc == NULL) return NULL;
    }
    return By_NewRef(function->doc);
}

static int By_Function_set_doc(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    Py_XSETREF(((ByFunctionObject *)self)->doc, By_NewRef(value == NULL ? Py_None : value));
    return 0;
}

/* python's own `__globals__`: the namespace of the module the `def` was written in, which
 * is what `typing.get_type_hints` evaluates a string annotation against */
static PyObject *By_Function_get_globals(PyObject *self, void *closure) {
    (void)closure;
    PyObject *globals = *((ByFunctionObject *)self)->spec->globals;
    if (globals == NULL) {
        PyErr_SetString(PyExc_AttributeError, "__globals__");
        return NULL;
    }
    return By_NewRef(globals);
}

/* the definition python would have made where the `def` stands, with its annotations and no
 * body, over the values the enclosing names held where the `def` stood where those were
 * kept, and over the values they hold now otherwise — see `by_irbuild::annotations` */
static PyObject *By_AnnotatedDefinition(PyObject *self) {
    const ByFunctionSpec *spec = ((ByFunctionObject *)self)->spec;
    PyObject *factory = *spec->annotation_factory;
    if (factory == NULL) {
        PyObject *globals = *spec->globals;
        PyObject *code, *ran, *locals;
        if (globals == NULL) {
            PyErr_SetString(PyExc_RuntimeError, "the module's namespace is gone");
            return NULL;
        }
        code = Py_CompileString(spec->annotations, "<by annotations>", Py_file_input);
        if (code == NULL) return NULL;
        locals = PyDict_New();
        if (locals == NULL) {
            Py_DECREF(code);
            return NULL;
        }
        ran = PyEval_EvalCode(code, globals, locals);
        Py_DECREF(code);
        if (ran == NULL) {
            Py_DECREF(locals);
            return NULL;
        }
        Py_DECREF(ran);
        factory = PyDict_GetItemString(locals, "_by_annotations"); /* borrowed */
        if (factory == NULL) {
            Py_DECREF(locals);
            PyErr_SetString(PyExc_RuntimeError, "the annotation factory defines no function");
            return NULL;
        }
        *spec->annotation_factory = By_NewRef(factory);
        Py_DECREF(locals);
    }
    PyObject *values = ((ByFunctionObject *)self)->annotation_values;
    if (values != NULL) {
        values = By_NewRef(values);
    } else if (spec->annotation_values == NULL) {
        values = By_NewRef(Py_None);
    } else {
        values = spec->annotation_values(((ByFunctionObject *)self)->env);
    }
    if (values == NULL) return NULL;
    ByFunctionObject *function = (ByFunctionObject *)self;
    PyObject *made = function->made_type_params == NULL ? Py_None : function->made_type_params;
    PyObject *definition = PyObject_CallFunctionObjArgs(factory, values, made, NULL);
    Py_DECREF(values);
    if (definition != NULL && function->made_type_params == NULL) {
        function->made_type_params = PyObject_GetAttrString(definition, "__type_params__");
        if (function->made_type_params == NULL) Py_CLEAR(definition);
    }
    return definition;
}

/* 3.13 evaluates a function's annotations where its `def` stands, and a closure made there
 * is handed them. evaluating them is a python call that makes a whole definition, which
 * cost a closure four thousand instructions more to make than one without annotations, so
 * what is taken where the `def` stands is only the values the enclosing names hold, and
 * the annotations are evaluated over those when they are first asked for. the reference is
 * taken over, and let go of where reading the values raises */
static inline PyObject *By_AnnotateAtDefinition(PyObject *self) {
#if PY_VERSION_HEX < 0x030E0000
    if (self == NULL || ((ByFunctionObject *)self)->spec->annotations == NULL
        || ((ByFunctionObject *)self)->spec->annotations_refused
        || ((ByFunctionObject *)self)->spec->annotation_values == NULL) {
        return self;
    }
    PyObject *values =
        ((ByFunctionObject *)self)->spec->annotation_values(((ByFunctionObject *)self)->env);
    if (values == NULL) {
        Py_DECREF(self);
        return NULL;
    }
    ((ByFunctionObject *)self)->annotation_values = values;
#endif
    return self;
}

/* python makes a function's type parameters with the function, so they are made once for
 * each closure and kept */
static PyObject *By_Function_get_type_params(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->type_params == NULL) {
        if (function->spec->annotations == NULL) {
            function->type_params = PyTuple_New(0);
        } else {
            if (function->made_type_params == NULL) {
                PyObject *definition = By_AnnotatedDefinition(self);
                if (definition == NULL) return NULL;
                Py_DECREF(definition);
            }
            function->type_params = By_NewRef(function->made_type_params);
        }
        if (function->type_params == NULL) return NULL;
    }
    return By_NewRef(function->type_params);
}

static int By_Function_set_type_params(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    if (value == NULL || !PyTuple_Check(value)) {
        PyErr_SetString(PyExc_TypeError, "__type_params__ must be set to a tuple");
        return -1;
    }
    Py_XSETREF(((ByFunctionObject *)self)->type_params, By_NewRef(value));
    return 0;
}

#if PY_VERSION_HEX >= 0x030E0000
/* from 3.14 python evaluates annotations when they are first asked for, through an
 * `__annotate__` over the enclosing frames' cells. this one is python's own, made for the
 * definition from the values the enclosing names hold when it is asked for */
static PyObject *By_Function_get_annotate(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->annotate != NULL) return By_NewRef(function->annotate);
    if (function->spec->annotations == NULL) return By_NewRef(Py_None);
    PyObject *definition = By_AnnotatedDefinition(self);
    if (definition == NULL) return NULL;
    PyObject *annotate = PyObject_GetAttrString(definition, "__annotate__");
    Py_DECREF(definition);
    if (annotate == NULL || annotate == Py_None) return annotate;
    PyObject *qualname = PyUnicode_FromFormat("%s.__annotate__", function->spec->qualname);
    int named = qualname == NULL ? -1 : PyObject_SetAttrString(annotate, "__qualname__", qualname);
    Py_XDECREF(qualname);
    if (named < 0) {
        Py_DECREF(annotate);
        return NULL;
    }
    return annotate;
}

static int By_Function_set_annotate(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (value == NULL) {
        PyErr_SetString(PyExc_TypeError, "__annotate__ cannot be deleted");
        return -1;
    }
    if (value != Py_None && !PyCallable_Check(value)) {
        PyErr_SetString(PyExc_TypeError, "__annotate__ must be callable or None");
        return -1;
    }
    Py_XSETREF(function->annotate, By_NewRef(value));
    if (value != Py_None) Py_CLEAR(function->annotations);
    return 0;
}
#endif

static PyObject *By_Function_get_annotations(PyObject *self, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->annotations == NULL) {
#if PY_VERSION_HEX >= 0x030E0000
        PyObject *annotate = By_Function_get_annotate(self, NULL);
        if (annotate == NULL) return NULL;
        if (annotate != Py_None && PyCallable_Check(annotate)) {
            PyObject *value_format = PyLong_FromLong(1);
            PyObject *annotations =
                value_format == NULL ? NULL : PyObject_CallOneArg(annotate, value_format);
            Py_XDECREF(value_format);
            Py_DECREF(annotate);
            if (annotations == NULL) return NULL;
            if (!PyDict_Check(annotations)) {
                PyErr_Format(PyExc_TypeError, "__annotate__ returned non-dict of type '%.100s'",
                             Py_TYPE(annotations)->tp_name);
                Py_DECREF(annotations);
                return NULL;
            }
            function->annotations = annotations;
        } else {
            Py_DECREF(annotate);
        }
#else
        /* evaluated the first time they are asked for, unless something has been written
         * over them since, and the values kept for it are let go of once they have been.
         * annotations the build could not supply raise here instead */
        if (function->spec->annotations != NULL && function->annotate == NULL) {
            PyObject *definition = By_AnnotatedDefinition(self);
            if (definition == NULL) return NULL;
            PyObject *annotations = PyObject_GetAttrString(definition, "__annotations__");
            Py_DECREF(definition);
            if (annotations == NULL) return NULL;
            function->annotations = annotations;
            Py_CLEAR(function->annotation_values);
        }
#endif
        if (function->annotations == NULL) {
            function->annotations = PyDict_New();
            if (function->annotations == NULL) return NULL;
        }
    }
    return By_NewRef(function->annotations);
}

static int By_Function_set_annotations(PyObject *self, PyObject *value, void *closure) {
    (void)closure;
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (value == Py_None) value = NULL;
    if (value != NULL && !PyDict_Check(value)) {
        PyErr_SetString(PyExc_TypeError, "__annotations__ must be set to a dict object");
        return -1;
    }
    Py_XSETREF(function->annotations, Py_XNewRef(value));
    Py_XSETREF(function->annotate, By_NewRef(Py_None));
    return 0;
}

/* the code object python compiled for this definition in the interpreted module, found by
 * its qualified name among the constants of every code object the module holds
 *
 * `count` is how many answer to the name: a module whose body defines the same qualified
 * name twice — a `def` under each arm of an `if` — does not say which is this one */
static void By_FindCode(PyObject *code, PyObject *qualname, PyObject **found, int *count) {
    PyObject *consts = PyObject_GetAttrString(code, "co_consts");
    if (consts == NULL) {
        PyErr_Clear();
        return;
    }
    if (PyTuple_Check(consts)) {
        for (Py_ssize_t at = 0; at < PyTuple_GET_SIZE(consts); at++) {
            PyObject *item = PyTuple_GET_ITEM(consts, at);
            if (!PyCode_Check(item)) continue;
            PyObject *named = PyObject_GetAttrString(item, "co_qualname");
            if (named == NULL) {
                PyErr_Clear();
            } else {
                int same = PyUnicode_Check(named) && PyUnicode_Compare(named, qualname) == 0;
                Py_DECREF(named);
                if (same) {
                    (*count)++;
                    Py_XSETREF(*found, By_NewRef(item));
                }
            }
            By_FindCode(item, qualname, found, count);
        }
    }
    Py_DECREF(consts);
}

/* python's `__code__`: the one the interpreted definition runs, which is what says what
 * parameters the function takes — `inspect.signature` reads them off it, as it does off a
 * cython function */
static PyObject *By_Function_get_code(PyObject *self, void *closure) {
    (void)closure;
    const ByFunctionSpec *spec = ((ByFunctionObject *)self)->spec;
    if (spec->fallback == NULL || spec->code == NULL) {
        PyErr_SetString(PyExc_AttributeError, "__code__");
        return NULL;
    }
    if (*spec->code == NULL) {
        PyObject *module = By_FallbackCode(spec->fallback);
        if (module == NULL) {
            if (PyErr_Occurred()) return NULL;
            module = Py_CompileString(spec->fallback->source, "<string>", Py_file_input);
            if (module == NULL) return NULL;
        }
        PyObject *qualname = PyUnicode_FromString(spec->qualname);
        PyObject *found = NULL;
        int count = 0;
        if (qualname != NULL) By_FindCode(module, qualname, &found, &count);
        Py_XDECREF(qualname);
        Py_DECREF(module);
        if (qualname == NULL) return NULL;
        if (count != 1) {
            Py_XDECREF(found);
            PyErr_Format(PyExc_AttributeError,
                         "the interpreted module does not define `%s` exactly once, so this "
                         "compiled function has no `__code__`",
                         spec->qualname);
            return NULL;
        }
        *spec->code = found;
    }
    return By_NewRef(*spec->code);
}

/* one half of what `spec->defaults` hands back */
static PyObject *By_Function_default_half(PyObject *self, Py_ssize_t half) {
    ByFunctionObject *function = (ByFunctionObject *)self;
    if (function->spec->defaults == NULL) return By_NewRef(Py_None);
    PyObject *both = function->spec->defaults(function->env);
    if (both == NULL) return NULL;
    PyObject *answer = By_NewRef(PyTuple_GET_ITEM(both, half));
    Py_DECREF(both);
    return answer;
}

static PyObject *By_Function_get_defaults(PyObject *self, void *closure) {
    (void)closure;
    return By_Function_default_half(self, 0);
}

static PyObject *By_Function_get_kwdefaults(PyObject *self, void *closure) {
    (void)closure;
    return By_Function_default_half(self, 1);
}

static PyObject *By_Function_self(PyObject *self, PyObject *unused) {
    (void)unused;
    return By_NewRef(self);
}

/* pickled by name, as a python function is. the name of a nested one runs through
 * `<locals>`, so pickle refuses it exactly as it refuses python's */
static PyObject *By_Function_reduce(PyObject *self, PyObject *unused) {
    (void)unused;
    return By_Function_get_qualname(self, NULL);
}

static PyMethodDef By_Function_methods[] = {
    {"__copy__", By_Function_self, METH_NOARGS, NULL},
    {"__deepcopy__", By_Function_self, METH_O, NULL},
    {"__reduce__", By_Function_reduce, METH_NOARGS, NULL},
    {NULL, NULL, 0, NULL},
};

static PyGetSetDef By_Function_getset[] = {
    {"__name__", By_Function_get_name, By_Function_set_name, NULL, NULL},
    {"__qualname__", By_Function_get_qualname, By_Function_set_qualname, NULL, NULL},
    {"__module__", By_Function_get_module, By_Function_set_module, NULL, NULL},
    {"__doc__", By_Function_get_doc, By_Function_set_doc, NULL, NULL},
    {"__dict__", PyObject_GenericGetDict, PyObject_GenericSetDict, NULL, NULL},
    {"__globals__", By_Function_get_globals, NULL, NULL, NULL},
    {"__code__", By_Function_get_code, NULL, NULL, NULL},
    {"__type_params__", By_Function_get_type_params, By_Function_set_type_params, NULL, NULL},
    {"__defaults__", By_Function_get_defaults, NULL, NULL, NULL},
    {"__kwdefaults__", By_Function_get_kwdefaults, NULL, NULL, NULL},
    {"__annotations__", By_Function_get_annotations, By_Function_set_annotations, NULL, NULL},
#if PY_VERSION_HEX >= 0x030E0000
    {"__annotate__", By_Function_get_annotate, By_Function_set_annotate, NULL, NULL},
#endif
    {NULL, NULL, NULL, NULL, NULL},
};

static PyTypeObject By_FunctionType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.function",
    .tp_basicsize = sizeof(ByFunctionObject),
    .tp_itemsize = 0,
    .tp_dealloc = (destructor)By_Function_dealloc,
    .tp_vectorcall_offset = offsetof(ByFunctionObject, vectorcall),
    .tp_repr = (reprfunc)By_Function_repr,
    .tp_call = PyVectorcall_Call,
    .tp_getattro = PyObject_GenericGetAttr,
    .tp_setattro = PyObject_GenericSetAttr,
    /* a method descriptor as python's own function is one: `obj.f()` through the type
     * calls it with `obj` in front rather than building a bound method first */
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_HAVE_VECTORCALL
                | Py_TPFLAGS_METHOD_DESCRIPTOR,
    .tp_traverse = (traverseproc)By_Function_traverse,
    .tp_clear = (inquiry)By_Function_clear,
    .tp_methods = By_Function_methods,
    .tp_getset = By_Function_getset,
    .tp_descr_get = By_Function_descr_get,
    .tp_dictoffset = offsetof(ByFunctionObject, dict),
    .tp_free = PyObject_GC_Del,
};

/* the function a `def` binds, over the environment the frame made for it */
static inline PyObject *By_MakeFunction(const ByFunctionSpec *spec, PyObject *env) {
    ByFunctionObject *self;
    if (env == NULL) return NULL;
    self = PyObject_GC_New(ByFunctionObject, &By_FunctionType);
    if (self == NULL) return NULL;
    self->vectorcall = spec->call;
    self->env = By_NewRef(env);
    self->spec = spec;
    self->name = NULL;
    self->qualname = NULL;
    self->module = NULL;
    self->doc = NULL;
    self->dict = NULL;
    self->annotations = NULL;
    self->annotate = NULL;
    self->type_params = NULL;
    self->made_type_params = NULL;
    self->annotation_values = NULL;
    PyObject_GC_Track(self);
    return (PyObject *)self;
}

/* the environment a nested function's boundary reads its captures from */
static inline PyObject *By_FunctionEnvironment(PyObject *callable) {
    return ((ByFunctionObject *)callable)->env;
}

/* narrowing an object to a refcounted type is a test rather than a change of
 * representation: the answer *is* the argument. so each of these comes in two
 * forms — a `By_Check…` that hands the same object back without a reference, and
 * a `By_Unbox…` that takes one — and the second is written in terms of the first
 * so the test cannot drift between them. which form a register gets is the borrow
 * pass's answer: where it proved the source goes on holding the value across
 * every use, the check is the whole cost and the reference pair is waste */

/* a value arriving where a native class is expected. without the check a python
   caller could store any object in a field and every later field read would
   follow a wild pointer */
static inline PyObject *By_CheckInstance(PyObject *o, PyTypeObject *type) {
    if (!PyObject_TypeCheck(o, type)) {
        By_SoundViolation(o, (PyObject *)type);
        return NULL;
    }
    return o;
}

static inline PyObject *By_UnboxInstance(PyObject *o, PyTypeObject *type) {
    PyObject *checked = By_CheckInstance(o, type);
    if (checked == NULL) return NULL;
    return By_NewRef(checked);
}

/* a `list` is a `PyObject *` like every other container, so narrowing to one is a
 * type check rather than a change of representation — but it is still a check,
 * because the value came from somewhere that only promised an object */
static inline PyObject *By_CheckList(PyObject *o) {
    if (o == NULL || !PyList_Check(o)) {
        By_TypeError("list", o);
        return NULL;
    }
    return o;
}

static inline PyObject *By_UnboxList(PyObject *o) {
    PyObject *checked = By_CheckList(o);
    if (checked == NULL) return NULL;
    Py_INCREF(checked);
    return checked;
}

static inline PyObject *By_CheckStr(PyObject *o) {
    if (o == NULL || !PyUnicode_Check(o)) {
        By_TypeError("str", o);
        return NULL;
    }
    return o;
}

static inline PyObject *By_UnboxStr(PyObject *o) {
    PyObject *checked = By_CheckStr(o);
    if (checked == NULL) return NULL;
    Py_INCREF(checked);
    return checked;
}

static inline PyObject *By_StrItemTagged(PyObject *s, ByTagged index) {
    if (BY_UNLIKELY(s == NULL || index == BY_INT_ERROR)) return NULL;
    if (BY_LIKELY(PyUnicode_CheckExact(s) && By_IsShort(index))) {
        return By_StrCharAt(s, By_ShortValue(index));
    }
    PyObject *item = By_GetItemTagged(s, index);
    if (item == NULL) return NULL;
    PyObject *checked = By_UnboxStr(item);
    Py_DECREF(item);
    return checked;
}

/* `s[i] <op> c`, where `c` is the one-code-point `str` of `codepoint`
 *
 * a `str` compares by code point and an exact `str` holds its code points directly,
 * so a right-hand side of one code point makes the whole comparison a question the
 * character can answer without ever becoming a `str` of its own. that allocation is
 * the entire cost of a scan that only ever asks what a character *is*.
 *
 * a `str` subclass may have overridden `__getitem__` and may hand back any `str` at
 * all — including one of no code points, or of several — and may have overridden
 * `__eq__` besides. so the slow path is the ordinary one, character built and
 * compared as an object, and the right-hand side is built from the same code point
 * the fast path tested rather than from a literal of its own, so the two cannot
 * drift apart */
static inline char By_StrItemCompareChar(PyObject *s, ByTagged index, Py_UCS4 codepoint,
                                         int op) {
    if (BY_LIKELY(s != NULL && index != BY_INT_ERROR && PyUnicode_CheckExact(s)
                  && By_IsShort(index))) {
        Py_ssize_t length = PyUnicode_GET_LENGTH(s);
        Py_ssize_t i = By_ShortValue(index);
        if (i < 0) i += length;
        if (BY_LIKELY(i >= 0 && i < length)) {
            Py_UCS4 found = PyUnicode_READ_CHAR(s, i);
            switch (op) {
                case Py_EQ: return (char) (found == codepoint);
                case Py_NE: return (char) (found != codepoint);
                case Py_LT: return (char) (found < codepoint);
                case Py_LE: return (char) (found <= codepoint);
                case Py_GT: return (char) (found > codepoint);
                default: return (char) (found >= codepoint);
            }
        }
    }
    PyObject *item = By_StrItemTagged(s, index);
    if (item == NULL) return 2;
    PyObject *character = PyUnicode_FromOrdinal((int) codepoint);
    if (character == NULL) {
        Py_DECREF(item);
        return 2;
    }
    char answer = By_StrCompare(item, character, op);
    Py_DECREF(character);
    Py_DECREF(item);
    return answer;
}

/* the same question as `By_StrItemCompareChar`, asked with a machine index
 *
 * a scan reaches this with its counter already in a register, and the tagged form
 * would make the counter a tagged integer only for this call to shift it straight
 * back — the whole of which is a round trip the fast path never needed. the range
 * test is done in the machine width so that an index too large for a `Py_ssize_t`
 * falls out of range rather than wrapping into it
 *
 * everything the fast path does not answer is handed to the tagged form, so a
 * subclass, an out-of-range index and an index that has to become an object are
 * all still answered in exactly one place */
/* the rest of [`By_StrItemCompareCharI64`]: a subclass, or an index out of range */
BY_COLD char By_StrItemCompareCharI64Slow(PyObject *s, int64_t index, Py_UCS4 codepoint,
                                          int op) {
    ByTagged tagged = By_IntFromI64(index);
    char answer;
    if (BY_UNLIKELY(tagged == BY_INT_ERROR)) return 2;
    answer = By_StrItemCompareChar(s, tagged, codepoint, op);
    By_DecRefTagged(tagged);
    return answer;
}

/* inlined, with the slow half kept out of line: a loop the compiler has duplicated reads a
 * character in both copies, and the compiler weighs inlining the whole helper twice */
BY_HOT char By_StrItemCompareCharI64(PyObject *s, int64_t index, Py_UCS4 codepoint, int op) {
    if (BY_LIKELY(s != NULL && PyUnicode_CheckExact(s))) {
        int64_t length = (int64_t) PyUnicode_GET_LENGTH(s);
        int64_t at = index < 0 ? index + length : index;
        if (BY_LIKELY(at >= 0 && at < length)) {
            Py_UCS4 found = PyUnicode_READ_CHAR(s, (Py_ssize_t) at);
            switch (op) {
                case Py_EQ: return (char) (found == codepoint);
                case Py_NE: return (char) (found != codepoint);
                case Py_LT: return (char) (found < codepoint);
                case Py_LE: return (char) (found <= codepoint);
                case Py_GT: return (char) (found > codepoint);
                default: return (char) (found >= codepoint);
            }
        }
    }
    return By_StrItemCompareCharI64Slow(s, index, codepoint, op);
}

#endif /* BY_RT_H */
