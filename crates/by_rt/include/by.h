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
#include <string.h>
#include <math.h>
#include <stdint.h>
#include <stddef.h>

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
 */

typedef size_t ByTagged;

#define BY_INT_TAG ((ByTagged)1)
/* a tagged word of 1 is a pointer of 0 with the tag set, so it can never be a
 * real object and is free to mean "an exception is set" */
#define BY_INT_ERROR ((ByTagged)1)
/* a float error sentinel overlaps a valid value, so an error must be confirmed
 * with PyErr_Occurred() — see RType::error_overlaps */
#define BY_FLOAT_ERROR (-113.0)

#define BY_SHORT_MAX (PY_SSIZE_T_MAX >> 1)
#define BY_SHORT_MIN (-BY_SHORT_MAX - 1)

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

/* borrow-free: returns a new reference */
static inline PyObject *By_BoxInt(ByTagged x) {
    if (By_IsShort(x)) {
        return PyLong_FromSsize_t(By_ShortValue(x));
    }
    PyObject *o = By_LongOf(x);
    Py_INCREF(o);
    return o;
}

/* takes a new reference to `o` when it cannot be represented as a short */
static inline ByTagged By_TaggedFromLong(PyObject *o) {
    int overflow = 0;
    Py_ssize_t value = PyLong_AsLongLongAndOverflow(o, &overflow);
    if (!overflow && value != -1) {
        if (By_FitsShort((Py_ssize_t)value)) {
            return By_ShortFrom((Py_ssize_t)value);
        }
    } else if (!overflow && !PyErr_Occurred() && By_FitsShort((Py_ssize_t)value)) {
        return By_ShortFrom((Py_ssize_t)value);
    }
    PyErr_Clear();
    Py_INCREF(o);
    return ((ByTagged)(void *)o) | BY_INT_TAG;
}

BY_HOT void By_DecRefTagged(ByTagged x) {
    if (BY_UNLIKELY(!By_IsShort(x) && x != BY_INT_ERROR)) {
        Py_DECREF(By_LongOf(x));
    }
}

BY_HOT void By_IncRefTagged(ByTagged x) {
    if (BY_UNLIKELY(!By_IsShort(x) && x != BY_INT_ERROR)) {
        Py_INCREF(By_LongOf(x));
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

BY_HOT ByTagged By_IntAdd(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = (Py_ssize_t)a, y = (Py_ssize_t)b;
        /* wrap in the unsigned domain, where overflow is defined, then use the
         * sign test: overflow happened iff both operands differ in sign from the
         * result */
        Py_ssize_t sum = (Py_ssize_t)((size_t)x + (size_t)y);
        if (BY_LIKELY(((x ^ sum) & (y ^ sum)) >= 0)) return (ByTagged)sum;
    }
    return By_IntSlowBinary(a, b, "+");
}

BY_HOT ByTagged By_IntSub(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = (Py_ssize_t)a, y = (Py_ssize_t)b;
        Py_ssize_t diff = (Py_ssize_t)((size_t)x - (size_t)y);
        if (BY_LIKELY(((x ^ y) & (x ^ diff)) >= 0)) return (ByTagged)diff;
    }
    return By_IntSlowBinary(a, b, "-");
}

/* a product of two values within this bound cannot leave the short range */
#define BY_MUL_SAFE (((Py_ssize_t)1) << ((sizeof(Py_ssize_t) * 8 - 4) / 2))

BY_HOT ByTagged By_IntMul(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = By_ShortValue(a), y = By_ShortValue(b);
        if (x > -BY_MUL_SAFE && x < BY_MUL_SAFE && y > -BY_MUL_SAFE && y < BY_MUL_SAFE) {
            return By_ShortFrom(x * y);
        }
    }
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
        if (!(x == PY_SSIZE_T_MIN && y == -1)) {
            return By_ShortFrom(By_FloorDivSsize(x, y));
        }
    }
    return By_IntSlowBinary(a, b, "/");
}

BY_HOT ByTagged By_IntMod(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t y = By_ShortValue(b);
        if (y == 0) {
            By_ZeroDivision(PyNumber_Remainder, 0);
            return BY_INT_ERROR;
        }
        Py_ssize_t x = By_ShortValue(a);
        if (!(x == PY_SSIZE_T_MIN && y == -1)) {
            return By_ShortFrom(By_ModSsize(x, y));
        }
    }
    return By_IntSlowBinary(a, b, "%");
}

static inline double By_IntTrueDiv(ByTagged a, ByTagged b) {
    PyObject *left = By_BoxInt(a);
    if (left == NULL) return BY_FLOAT_ERROR;
    PyObject *right = By_BoxInt(b);
    if (right == NULL) { Py_DECREF(left); return BY_FLOAT_ERROR; }
    PyObject *result = PyNumber_TrueDivide(left, right);
    Py_DECREF(left);
    Py_DECREF(right);
    if (result == NULL) return BY_FLOAT_ERROR;
    double value = PyFloat_AsDouble(result);
    Py_DECREF(result);
    return value;
}

/* `& | ^` are exact on the shifted representation: (2a)&(2b) == 2(a&b) */
BY_HOT ByTagged By_IntAnd(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) return a & b;
    return By_IntSlowBitwise(a, b, '&');
}
BY_HOT ByTagged By_IntOr(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) return a | b;
    return By_IntSlowBitwise(a, b, '|');
}
BY_HOT ByTagged By_IntXor(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) return a ^ b;
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

/* `~a` is `-1 - a`, which the tagged subtraction already handles */
static inline ByTagged By_IntInvert(ByTagged a) {
    return By_IntSub(By_ShortFrom(-1), a);
}

static inline ByTagged By_IntNeg(ByTagged a) {
    if (By_IsShort(a)) {
        Py_ssize_t x = By_ShortValue(a);
        if (x != PY_SSIZE_T_MIN) return By_ShortFrom(-x);
    }
    return By_IntSub(By_ShortFrom(0), a);
}

/* ── int comparison ───────────────────────────────────────────────────────── */

BY_COLD char By_IntCompareSlow(ByTagged a, ByTagged b, int op) {
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

static inline double By_FloatPow(double a, double b) { return pow(a, b); }

static inline double By_FloatFloorDiv(double a, double b) {
    if (b == 0.0) {
        By_ZeroDivision(PyNumber_FloorDivide, 1);
        return BY_FLOAT_ERROR;
    }
    return floor(a / b);
}

static inline double By_FloatMod(double a, double b) {
    if (b == 0.0) {
        By_ZeroDivision(PyNumber_Remainder, 1);
        return BY_FLOAT_ERROR;
    }
    double r = fmod(a, b);
    /* python's % takes the sign of the divisor */
    if (r != 0.0 && ((r < 0.0) != (b < 0.0))) r += b;
    return r;
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
    Py_INCREF(o);
    return o;
}

static inline PyObject *By_BoxNone(void) {
    Py_INCREF(Py_None);
    return Py_None;
}

static inline void By_TypeError(const char *expected, PyObject *got) {
    PyErr_Format(PyExc_TypeError, "expected %s, got %s", expected,
                 got == NULL ? "NULL" : Py_TYPE(got)->tp_name);
}

/* unboxing is a *narrowing*, so it is always checked — this is the
 * representation invariant's inserted check, not an assumption */
static inline ByTagged By_UnboxInt(PyObject *o) {
    if (o == NULL || !PyLong_Check(o)) {
        By_TypeError("int", o);
        return BY_INT_ERROR;
    }
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
        By_TypeError("None", o);
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
    Py_XINCREF(o);
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

/* a comparison yields a bit, and 2 means an exception is set */
static inline char By_ObjCompare(PyObject *a, PyObject *b, int op) {
    int result = PyObject_RichCompareBool(a, b, op);
    return result < 0 ? 2 : (char)result;
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

/* whether an emitted class may keep an instance dict beside its layout
 *
 * a managed dict lives in the pre-header, so it is the one form that leaves the struct,
 * its base's prefix and every field offset alone — but walking and releasing it is
 * `PyObject_VisitManagedDict` and `PyObject_ClearManagedDict`, which 3.13 published and
 * nothing below it offers outside the internal headers. so a module holding such a class
 * is left to its interpreted definitions on an older interpreter, decided at import
 * rather than when the C is written */
#if PY_VERSION_HEX >= 0x030D0000
#define BY_HAS_MANAGED_DICT 1
#define BY_MANAGED_DICT_FLAG Py_TPFLAGS_MANAGED_DICT
#define By_VisitManagedDict(obj, visit, arg) PyObject_VisitManagedDict((obj), (visit), (arg))
#define By_ClearManagedDict(obj) PyObject_ClearManagedDict(obj)
#else
#define BY_HAS_MANAGED_DICT 0
/* the flag is spelled in a static initializer, which is written whatever the interpreter
 * — and below 3.11 there is no such flag to name at all. these three are never reached:
 * the module falls back before any instance of such a class exists */
#define BY_MANAGED_DICT_FLAG 0
#define By_VisitManagedDict(obj, visit, arg) ((void)(obj), (void)(visit), (void)(arg))
#define By_ClearManagedDict(obj) ((void)(obj))
#endif

/* the flags a class asking for an instance dict declares, or nothing where the running
 * interpreter has no managed dict to give it
 *
 * a dict of arbitrary values has to be one the collector walks, so the two flags go
 * together and are named together — a type carrying only one of them is either a dict the
 * collector cannot reach or a collected type with nothing extra to reach. below 3.13
 * there is neither, and the class is built exactly as every emitted class was before
 * dicts existed: its layout is the whole of it. a class whose *generated* code cannot run
 * without a dict is not left to that — the module holding one refuses to install anything
 * at all down there */
#if BY_HAS_MANAGED_DICT
#define BY_INSTANCE_DICT_FLAGS (Py_TPFLAGS_HAVE_GC | Py_TPFLAGS_MANAGED_DICT)
#else
#define BY_INSTANCE_DICT_FLAGS 0
#endif

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

/* resolve a name the frame does not bind: the module namespace, then builtins
 *
 * the name arrives already interned, because this is on the path of every read of
 * a global. `PyDict_GetItemString` builds a fresh `str` and hashes it on each
 * call — twice over when the answer is a builtin, since the module namespace has
 * to miss first — where an interned key carries its hash and settles a
 * unicode-keyed dict on a pointer compare.
 *
 * both lookups are made every time, and that is not the slow half: a module that
 * rebinds a builtin name means it, and python would see it */
static inline PyObject *By_LookupGlobal(PyObject *dict, PyObject *name) {
    PyObject *value;
    if (name == NULL) return NULL;
    value = dict == NULL ? NULL : PyDict_GetItemWithError(dict, name);
    if (value == NULL) {
        PyObject *builtins;
        if (PyErr_Occurred()) return NULL;
        builtins = PyEval_GetBuiltins();
        if (builtins != NULL) value = PyDict_GetItemWithError(builtins, name);
    }
    if (value == NULL) {
        if (PyErr_Occurred()) return NULL;
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
 * a spec that *asked* for a managed dict is the one exception, and the spec is passed in
 * so that asking can be told from inheriting: python keeps a managed dict in a pre-header
 * it allocates itself, so the room is there and the offset — the sentinel `-1` — is the
 * answer that was wanted. without this a decorated class silently kept its interpreted
 * definition while every compiled function went on reading that definition's instances as
 * its own struct */
static inline int By_OffsetsHoldUp(PyTypeObject *type, PyType_Spec *spec) {
    PyTypeObject *base = type->tp_base;
    if (base == NULL) {
        return 1;
    }
    if (type->tp_weaklistoffset != base->tp_weaklistoffset) {
        return 0;
    }
    if (type->tp_dictoffset == base->tp_dictoffset) {
        return 1;
    }
    return spec != NULL && (spec->flags & BY_MANAGED_DICT_FLAG) != 0
           && type->tp_dictoffset == -1;
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
    PyObject *twin = By_LookupGlobalString(module_dict, name);
    PyObject *twin_base;
    PyObject *bases;
    PyObject *cls;
    int agrees;
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

/* what a value carried onto an emitted type becomes, as a new reference — defined with the
 * rest of the twin machinery, and named here because a class namespace is written before
 * that */
static PyObject *By_TwinReplacement(PyObject *value, PyObject *const *twins,
                                    PyObject *const *types, Py_ssize_t count);

/* the class-level constants a class body wrote, and where their values come from
 *
 * the interpreted definition evaluated each of them once at class-definition time, and it
 * is the only place the same object can come from — so the body that definition wrote is
 * what they are read off, under the substitution every carried attribute takes. that body
 * is captured while the fallback source runs and before any of the class's own decorators
 * are handed it; `By_RunModuleBody` says why the finished class will not do. `twins` and
 * `types` are the module's arrays and `classes` how many entries they hold; `body` is NULL
 * for a class no interpreted `class` statement wrote, and then there is nothing to carry */
typedef struct {
    PyObject *body;
    const char *const *names;
    Py_ssize_t count;
    PyObject *const *twins;
    PyObject *const *types;
    Py_ssize_t classes;
} By_ClassConstants;

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
    stands = By_TwinReplacement(value, constants->twins, constants->types, constants->classes);
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
 * the constants go in for the same reason, and that is the whole of what makes a class
 * with one buildable this way: copied onto the *finished* type they would land behind the
 * metaclass's back, and a `__slots__` that arrived after `type.__new__` had already given
 * the instances a dict is not a `__slots__` at all. what the copy cannot promise, the
 * check after the call does — see `By_ConstantsHeldUp` — and where the call does not get
 * that far, the raise is the same refusal by another route.
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
    PyObject *meta = By_Metaclass(bases, kwds);
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
 * again for anything the caller can only put on the *finished* type: a method decorator
 * lands after the metaclass has decided what the class defines, and a metaclass that
 * reads its namespace would disagree with it. a class-level constant does not, because
 * `constants` carries it into the namespace instead */
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

/* a call site's licence to call one compiled body without looking the method up,
 * taken once at import
 *
 * an override reached through a base-typed name has to ask the receiver which body
 * to run, and asking is the whole cost: a lookup on the type, a bound call, and the
 * boxed round trip a python-visible entry point takes. the answer is nearly always
 * the same one, so it is worked out here instead — and the two things that could
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
 * zero means the licence was refused: the name answers something this module did
 * not compile, or the type will not carry a version. no version tag is ever zero,
 * so a call site armed with zero takes the ordinary call for the life of the
 * process, which is what it did before any of this */
static inline unsigned int By_ArmMethod(PyObject *type, const char *name, PyCFunction body) {
    if (type == NULL || !PyType_Check(type)) return 0u;
    PyTypeObject *owner = (PyTypeObject *)type;
    /* the lookup is also what makes the interpreter assign a version tag: a type
     * nothing has been read from yet has none at all */
    PyObject *found = PyObject_GetAttrString(type, name);
    if (found == NULL) {
        PyErr_Clear();
        return 0u;
    }
    /* reading the type's own attribute hands back the descriptor rather than a bound
     * method, so the compiled entry point is reachable through it */
    int compiled = Py_IS_TYPE(found, &PyMethodDescr_Type)
                   && ((PyMethodDescrObject *)found)->d_method != NULL
                   && ((PyMethodDescrObject *)found)->d_method->ml_meth == body;
    Py_DECREF(found);
    if (!compiled) return 0u;
#if PY_VERSION_HEX >= 0x030C0000
    /* from 3.12 the tag is handed out on request rather than by whoever looks an
     * attribute up, and a request is the only thing that reliably produces one */
    if (!PyUnstable_Type_AssignVersionTag(owner)) return 0u;
#endif
    /* zero is both "never given a tag" and "written to since", and either is a refusal */
    return owner->tp_version_tag;
}

/* whether `o` is exactly `type`, and `type` still answers as [`By_ArmMethod`] found */
static inline char By_MethodStands(PyObject *o, PyObject *type, unsigned int armed) {
    return (char)(armed != 0u && o != NULL && (PyObject *)Py_TYPE(o) == type
                  && ((PyTypeObject *)type)->tp_version_tag == armed);
}

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
 *
 * three fields cannot be written as one, so what keeps a *reader* from seeing half of
 * one arming and half of another is that only one thread runs at a time. a site is
 * therefore only used where that holds: an emitted module says `Py_MOD_GIL_NOT_USED`,
 * and on a free-threaded build every call takes the ordinary path instead. two threads
 * arming a site at once would otherwise be able to leave one type's name paired with
 * another type's body, which is not a wrong answer but a call into the wrong object */
typedef struct {
    PyObject *type;
    unsigned int version;
    PyMethodDef *method;
} ByMethodSite;

#define BY_METHOD_SITE_INIT { NULL, 0u, NULL }

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
 * - an instance `__dict__` can shadow the type's entry, and only a type without one
 *   is safe to answer from the type alone. a static type is also the only kind whose
 *   storage layout guarantees there is no managed dict behind a zero `tp_dictoffset`
 * - anything but a builtin method descriptor has a `__get__` of its own to run
 * - a calling convention the site cannot lay the arguments out for, which is every
 *   convention that wants a tuple
 *
 * the type and its version are recorded before the first thing that can refuse, so
 * that a refusal is remembered on the same terms an answer is: a receiver this site
 * will never be able to serve — every instance of a class written in the interpreter
 * — is asked about once and then costs the same two comparisons as a hit.
 *
 * that order matters for a second reason. the attribute lookup below is the one step
 * here that another thread can run inside, and it sits *after* the type has been
 * recorded and the answer cleared — so a site another thread arms in the middle of
 * this one is left holding that thread's type against this one's version, which no
 * receiver matches, rather than that thread's type against this one's body */
static void By_ArmMethodSite(ByMethodSite *site, PyTypeObject *tp, PyObject *name,
                             Py_ssize_t nargs) {
    site->type = (PyObject *)tp;
    site->version = tp->tp_version_tag;
    site->method = NULL;
    if (!Py_IS_TYPE(tp, &PyType_Type)) return;
    if (tp->tp_flags & Py_TPFLAGS_HEAPTYPE) return;
    if (tp->tp_getattro != PyObject_GenericGetAttr) return;
    if (tp->tp_dictoffset != 0) return;
    PyObject *found = PyObject_GetAttr((PyObject *)tp, name);
    if (found == NULL) {
        PyErr_Clear();
        return;
    }
    /* reading the type's own attribute hands back the descriptor rather than a bound
     * method, so the builtin behind it is reachable through it */
    PyMethodDef *method = Py_IS_TYPE(found, &PyMethodDescr_Type)
                              ? ((PyMethodDescrObject *)found)->d_method
                              : NULL;
    Py_DECREF(found);
    if (method == NULL) return;
    int shape = method->ml_flags
                & (METH_VARARGS | METH_KEYWORDS | METH_NOARGS | METH_O | METH_FASTCALL);
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
 * on a free-threaded build there is no site at all: the three fields cannot be read
 * or written as one, so the whole thing is left to `By_CallMethod` */
static inline PyObject *By_CallMethodSite(ByMethodSite *site, PyObject *receiver,
                                          PyObject *name, PyObject **args, Py_ssize_t nargs) {
#ifndef Py_GIL_DISABLED
    if (BY_LIKELY(receiver != NULL && name != NULL)) {
        PyTypeObject *tp = Py_TYPE(receiver);
        if (BY_UNLIKELY((PyObject *)tp != site->type || tp->tp_version_tag != site->version)) {
            By_ArmMethodSite(site, tp, name, nargs);
        }
        PyMethodDef *method = site->method;
        if (BY_LIKELY(method != NULL)) {
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

/* the tests a `match` case makes about the *shape* of its subject
 *
 * a sequence pattern matches what the interpreter's own `MATCH_SEQUENCE` matches:
 * a type flagged as a sequence, which `str`, `bytes` and `bytearray` are not — so
 * `case [a, b]:` never takes a two-character string apart
 */
static inline char By_IsInstance(PyObject *o, PyObject *class_) {
    int result = PyObject_IsInstance(o, class_);
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
static PyObject *By_TwinFor(PyObject *value, PyObject *const *twins,
                            PyObject *const *types, Py_ssize_t count) {
    Py_ssize_t index;
    for (index = 0; index < count; index++) {
        if (value == twins[index] && types[index] != NULL) return types[index];
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
static PyObject *By_SettledValue(PyObject *value, PyObject *const *twins,
                                 PyObject *const *types, Py_ssize_t count, int depth);

/* whether settling `value` left it as the very object it was
 *
 * the question a place that cannot be written asks — a set's member, a dict's key, what a
 * property holds. anything it captured is settled where it stands, and a value that would
 * have had to be *replaced* is a refusal rather than a move */
static int By_SettlesInPlace(PyObject *value, PyObject *const *twins,
                             PyObject *const *types, Py_ssize_t count, int depth) {
    PyObject *stands = By_SettledValue(value, twins, types, count, depth);
    int same = stands == value;
    Py_XDECREF(stands);
    return same;
}

/* settle every value of a dict, in place, and say whether the dict now holds no twin
 *
 * a twin used as a *key* is refused rather than moved: a dict is keyed on the object a
 * class hashes as, so replacing one is a removal and an insertion, and doing that to a
 * mapping somebody else is holding is a bigger claim than this has evidence for */
static int By_SettleDictValues(PyObject *dict, PyObject *const *twins,
                               PyObject *const *types, Py_ssize_t count, int depth) {
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
        if (!By_SettlesInPlace(key, twins, types, count, depth)) {
            Py_DECREF(value);
            settled = 0;
            continue;
        }
        stands = By_SettledValue(value, twins, types, count, depth);
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
static int By_SettleListItems(PyObject *list, PyObject *const *twins,
                              PyObject *const *types, Py_ssize_t count, int depth) {
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
        stands = By_SettledValue(value, twins, types, count, depth);
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
static int By_SettleFunction(PyObject *fn, PyObject *const *twins, PyObject *const *types,
                             Py_ssize_t count, int depth) {
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
            stands = By_SettledValue(held, twins, types, count, depth);
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
        PyObject *moved = By_SettledValue(defaults, twins, types, count, depth);
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
        && !By_SettleDictValues(kwdefaults, twins, types, count, depth)) {
        settled = 0;
    }
    Py_XDECREF(kwdefaults);
    return settled;
}

static PyObject *By_SettledValue(PyObject *value, PyObject *const *twins,
                                 PyObject *const *types, Py_ssize_t count, int depth) {
    PyObject *replacement;
    Py_ssize_t index;
    if (value == NULL) return NULL;
    replacement = By_TwinFor(value, twins, types, count);
    if (replacement != NULL) return By_NewRef(replacement);
    for (index = 0; index < count; index++) {
        if (value == twins[index]) return NULL;
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
            for (index = 0; index < count; index++) {
                if (PyTuple_GET_ITEM(mro, at) == twins[index]) return NULL;
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
            if (!By_SettlesInPlace(part, twins, types, count, depth - 1)) settled = 0;
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
            PyObject *stands = By_SettledValue(item, twins, types, count, depth - 1);
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
        return By_SettleListItems(value, twins, types, count, depth - 1) ? By_NewRef(value)
                                                                        : NULL;
    }
    if (PyDict_Check(value)) {
        return By_SettleDictValues(value, twins, types, count, depth - 1) ? By_NewRef(value)
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
            if (!By_SettlesInPlace(PyList_GET_ITEM(members, at), twins, types, count,
                                   depth - 1)) {
                settled = 0;
            }
        }
        Py_DECREF(members);
        return settled ? By_NewRef(value) : NULL;
    }
    if (PyFunction_Check(value)) {
        return By_SettleFunction(value, twins, types, count, depth - 1) ? By_NewRef(value)
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
        return By_SettlesInPlace(receiver, twins, types, count, depth - 1) ? By_NewRef(value)
                                                                          : NULL;
    }
    /* a descriptor read off a type, which is every method an emitted type publishes and
     * every slot a built-in one does. it holds nothing but the type it was read from, and
     * cannot be written — so one whose owner is a twin is refused, exactly as a bound
     * method already bound to one is, and every other is safe as itself.
     *
     * `pprint` is why this is here: `_dispatch[dict.__repr__] = _pprint_dict` keys the
     * table on a slot wrapper, and refusing the *key* left all 18 values where they were */
    if (Py_TYPE(value) == &PyMethodDescr_Type || Py_TYPE(value) == &PyWrapperDescr_Type
        || Py_TYPE(value) == &PyClassMethodDescr_Type
        || Py_TYPE(value) == &PyGetSetDescr_Type || Py_TYPE(value) == &PyMemberDescr_Type) {
        PyObject *owner = PyObject_GetAttrString(value, "__objclass__");
        int settled;
        if (owner == NULL) {
            PyErr_Clear();
            return NULL;
        }
        settled = By_SettlesInPlace(owner, twins, types, count, depth - 1);
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
                      && By_SettlesInPlace(function, twins, types, count, depth - 1);
        if (receiver != NULL && !By_SettlesInPlace(receiver, twins, types, count, depth - 1)) {
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
            if (!By_SettlesInPlace(part, twins, types, count, depth - 1)) settled = 0;
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
static inline void By_SettleTwins(PyObject *value, PyObject *const *twins,
                                  PyObject *const *types, Py_ssize_t count) {
    PyObject *settled;
    if (value == NULL) return;
    settled = By_SettledValue(value, twins, types, count, BY_SETTLE_DEPTH);
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
static PyObject *By_TwinReplacement(PyObject *value, PyObject *const *twins,
                                    PyObject *const *types, Py_ssize_t count) {
    return By_SettledValue(value, twins, types, count, BY_SETTLE_DEPTH);
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
                                      PyObject *const *twins, PyObject *const *types,
                                      Py_ssize_t count) {
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
                                   : By_TwinReplacement(value, twins, types, count);
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
        PyObject *stands = By_TwinReplacement(written, twins, types, count);
        carried = stands == NULL ? By_LostAnnotations() : stands;
    }
    if (carried == NULL) return -1;
    failed = PyDict_SetItemString(target, BY_ANNOTATIONS, carried) < 0;
    Py_DECREF(carried);
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
static inline int By_AdoptTwinAttributes(PyObject *const *twins, PyObject *const *types,
                                         Py_ssize_t count) {
    Py_ssize_t index;
    for (index = 0; index < count; index++) {
        PyObject *twin = twins[index];
        PyObject *type = types[index];
        PyObject *source, *target, *names;
        Py_ssize_t at;
        if (twin == NULL || type == NULL || twin == type) continue;
        if (!PyType_Check(twin) || !PyType_Check(type)) continue;
        source = ((PyTypeObject *)twin)->tp_dict;
        target = ((PyTypeObject *)type)->tp_dict;
        if (source == NULL || target == NULL) continue;
        if (By_CarryAnnotations(source, target, twins, types, count) < 0) return -1;
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
            carried = By_TwinReplacement(value, twins, types, count);
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
static int By_RemapTwinMethods(PyObject *const *twins, PyObject *const *types,
                               Py_ssize_t count) {
    PyObject **from;
    PyObject **to;
    Py_ssize_t room = count;
    Py_ssize_t total = count;
    Py_ssize_t index;
    Py_ssize_t at;

    for (index = 0; index < count; index++) {
        PyObject *twin = twins[index];
        if (twin == NULL || !PyType_Check(twin)) continue;
        if (((PyTypeObject *)twin)->tp_dict == NULL) continue;
        room += PyDict_GET_SIZE(((PyTypeObject *)twin)->tp_dict);
    }
    if (room <= 0) return 0;
    from = PyMem_New(PyObject *, (size_t)room);
    to = PyMem_New(PyObject *, (size_t)room);
    if (from == NULL || to == NULL) {
        PyMem_Free(from);
        PyMem_Free(to);
        PyErr_NoMemory();
        return -1;
    }
    /* held rather than borrowed for the whole walk below: settling one value can run
     * arbitrary code, and a pair this is still to substitute must not go away underneath
     * it */
    for (index = 0; index < count; index++) {
        from[index] = By_NewRef(twins[index]);
        to[index] = By_NewRef(types[index]);
    }

    for (index = 0; index < count; index++) {
        PyObject *twin = twins[index];
        PyObject *type = types[index];
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
            from[total] = By_NewRef(held);
            to[total] = By_NewRef(stands);
            total++;
        }
        Py_DECREF(names);
    }

    /* the ambiguous pairs, dropped before any of them is used. a slot with nothing in it
     * matches no value, so the function it stood for is left exactly where the body put
     * it */
    for (index = count; index < total; index++) {
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

    if (total > count) {
        for (index = 0; index < count; index++) {
            PyObject *type = types[index];
            PyObject *values;
            if (type == NULL || !PyType_Check(type)) continue;
            if (((PyTypeObject *)type)->tp_dict == NULL) continue;
            /* taken out as a list for the same reason the keys above are, and it is the
             * values *inside* these that move — the type's own entries stay as they are */
            values = PyDict_Values(((PyTypeObject *)type)->tp_dict);
            if (values == NULL) {
                PyErr_Clear();
                continue;
            }
            for (at = 0; at < PyList_GET_SIZE(values); at++) {
                By_SettleTwins(PyList_GET_ITEM(values, at), from, to, total);
            }
            Py_DECREF(values);
        }
    }

    for (index = 0; index < total; index++) {
        Py_XDECREF(from[index]);
        Py_XDECREF(to[index]);
    }
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
                                       PyObject *const *twins, PyObject *const *types,
                                       Py_ssize_t count) {
    const char *const names[] = {name};
    By_ClassConstants constants = {body, names, 1, twins, types, count};
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
 * a value that merely *reaches* a twin is not moved and cannot be: an instance the body
 * built has the twin for its type, and a list holding one is the same object the body
 * kept. those stay as the body left them */
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
static inline int By_RemapTwinsInClass(PyObject *cls, PyObject *const *twins,
                                       PyObject *const *types, Py_ssize_t count) {
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
        By_SettleTwins(value, twins, types, count);
        /* a class attribute holding a twin — `Signature.empty = _empty` — is the same
         * staleness one step along, and rebinding the name answers it. only a name that
         * *is* a twin is rebound: settling a value it could not write in place hands back
         * a copy, and putting a copy where the original stood would break every other
         * holder's `is` against it */
        stands = By_TwinFor(value, twins, types, count);
        if (stands != NULL && stands != value && PyObject_SetAttr(cls, key, stands) < 0) {
            PyErr_Clear();
        }
        Py_DECREF(value);
    }
    Py_DECREF(keys);
    return 0;
}

static inline int By_RemapTwinAliases(PyObject *module_dict, PyObject *const *twins,
                                      PyObject *const *types,
                                      const char *const *names, Py_ssize_t count) {
    /* the keys first: the dict is written while this walks, and only for keys it
     * already holds, but nothing may run against it mid-walk either way */
    PyObject *keys = PyDict_Keys(module_dict);
    Py_ssize_t at;
    if (keys == NULL) return -1;
    for (at = 0; at < PyList_GET_SIZE(keys); at++) {
        PyObject *key = PyList_GET_ITEM(keys, at);
        PyObject *value = PyDict_GetItem(module_dict, key);
        Py_ssize_t index;
        if (value == NULL) continue;
        /* whatever else becomes of this name, what it holds may have captured a twin */
        By_SettleTwins(value, twins, types, count);
        if (By_RemapTwinsInClass(value, twins, types, count) < 0) {
            Py_DECREF(keys);
            return -1;
        }
        for (index = 0; index < count; index++) {
            PyObject *stands;
            if (twins[index] == NULL || value != twins[index]) continue;
            stands = PyDict_GetItemString(module_dict, names[index]);
            /* the class's own name already holds it, and one whose type was never
             * installed still holds the twin — neither is a move */
            if (stands == NULL || stands == value) break;
            if (PyDict_SetItem(module_dict, key, stands) < 0) {
                Py_DECREF(keys);
                return -1;
            }
            break;
        }
    }
    Py_DECREF(keys);
    return 0;
}

/* `__build_class__`, recording what each module-level `class` statement wrote
 *
 * `state` is `(the real __build_class__, the mapping to record into)`. the class is built
 * first and read afterwards, because the namespace itself is never handed back: python
 * gives it to the metaclass and to nobody else. what `type.__new__` made of it is the
 * closer thing anyway — it is exactly what the interpreted class holds at the moment
 * before the first of its decorators is handed it */
static PyObject *By_CaptureClassBody(PyObject *state, PyObject *args, PyObject *kwds) {
    PyObject *cls = PyObject_Call(PyTuple_GET_ITEM(state, 0), args, kwds);
    PyObject *name, *qualified, *body;
    int outermost;
    if (cls == NULL || PyTuple_GET_SIZE(args) < 2 || !PyType_Check(cls)) return cls;
    if (((PyTypeObject *)cls)->tp_dict == NULL) return cls;
    name = PyTuple_GET_ITEM(args, 1);
    /* a class written inside a function can be named the same as one at module level and
     * is not the same class. `f.<locals>.C` against `C` is what tells them apart, and the
     * body function python passes here is what carries that qualified name */
    qualified = PyObject_GetAttrString(PyTuple_GET_ITEM(args, 0), "__qualname__");
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
    if (body == NULL || PyDict_SetItem(PyTuple_GET_ITEM(state, 1), name, body) < 0) {
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
 * *its own frame's* builtins — so a copy of the builtins mapping, put in this module's
 * dict, reaches this module's body and nothing else in the process. swapping the entry in
 * the real builtins instead would be seen by every thread importing at the same time,
 * which on a free-threaded interpreter is a live hazard rather than a theoretical one.
 *
 * the copy outlives the exec whatever is done with it: python gives a function the
 * builtins its defining frame had, so every function this body defines holds this dict for
 * as long as it lives. that is why the real entry is put back afterwards rather than the
 * dict simply dropped — otherwise a class one of those functions made, at any later point
 * in the process, would still be recorded here.
 *
 * hands back `{name: body}` for the classes the body wrote at module level, as a new
 * reference, or NULL with an exception set where the body raised */
static inline PyObject *By_RunModuleBody(const By_Fallback *fallback, PyObject *dict) {
    static PyMethodDef capture = {"__build_class__",
                                  (PyCFunction)(void (*)(void))By_CaptureClassBody,
                                  METH_VARARGS | METH_KEYWORDS, NULL};
    PyObject *bodies, *stood, *mapping, *builtins, *real, *state, *wrapper, *result;
    int failed;
    bodies = PyDict_New();
    if (bodies == NULL) return NULL;
    /* an emitted module's dict has no `__builtins__` of its own, and python would then
     * give the body's frame the running interpreter's */
    stood = PyDict_GetItemString(dict, "__builtins__");
    if (stood == NULL) stood = PyEval_GetBuiltins();
    Py_XINCREF(stood);
    mapping = stood != NULL && PyModule_Check(stood) ? PyModule_GetDict(stood) : stood;
    builtins = mapping != NULL && PyDict_Check(mapping) ? PyDict_Copy(mapping) : NULL;
    real = builtins == NULL ? NULL : PyDict_GetItemString(builtins, "__build_class__");
    if (real == NULL) {
        Py_XDECREF(builtins);
        Py_XDECREF(stood);
        Py_DECREF(bodies);
        if (!PyErr_Occurred()) {
            PyErr_SetString(PyExc_RuntimeError,
                            "no builtins `__build_class__` to run the module body against");
        }
        return NULL;
    }
    Py_INCREF(real);
    state = PyTuple_Pack(2, real, bodies);
    wrapper = state == NULL ? NULL : PyCFunction_New(&capture, state);
    Py_XDECREF(state);
    failed = wrapper == NULL || PyDict_SetItemString(builtins, "__build_class__", wrapper) < 0
             || PyDict_SetItemString(dict, "__builtins__", builtins) < 0;
    Py_XDECREF(wrapper);
    result = failed ? NULL : By_ExecModuleBody(fallback, dict);
    Py_XDECREF(result);
    /* whatever the body did, the capture stops here */
    {
        PyObject *type, *value, *traceback;
        PyErr_Fetch(&type, &value, &traceback);
        if (PyDict_SetItemString(builtins, "__build_class__", real) < 0
            || PyDict_SetItemString(dict, "__builtins__", stood) < 0) {
            PyErr_Clear();
        }
        PyErr_Restore(type, value, traceback);
    }
    Py_DECREF(real);
    Py_DECREF(builtins);
    Py_DECREF(stood);
    if (failed || result == NULL) {
        Py_DECREF(bodies);
        return NULL;
    }
    return bodies;
}

/* the body captured for one class, as a borrowed reference, or NULL where there is none */
static inline PyObject *By_ClassBody(PyObject *bodies, const char *name) {
    if (bodies == NULL) return NULL;
    return PyDict_GetItemString(bodies, name);
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
 */
static inline PyObject *By_MatchPositional(PyObject *subject, PyObject *class_,
                                           Py_ssize_t index, Py_ssize_t count) {
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
    PyObject *names = PyObject_GetAttrString(class_, "__match_args__");
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

/* `k in d` answered by the very lookup `d[k]` would go on to make
 *
 * the two hash the same key and walk the same table, so where the second is only
 * reached because the first said yes, one of them is doing the other's work again.
 * this asks once and reports both answers as one value: a new reference where the
 * key is there, and NULL where it is not. NULL with an exception set is failure —
 * the caller tells the two apart with `PyErr_Occurred`, the way it already does
 * for an exhausted iterator
 *
 * asking once has to be *earned* at runtime, because both of the things that would
 * make asking twice observable are ordinary python. a dict subclass may have
 * overridden `__contains__` or `__getitem__`, and then the number and order of
 * those calls is the program's own business. a key may have a `__hash__` that
 * counts how often it is called, and then hashing once where the source hashes
 * twice is a different program. so the single probe is taken only for an exact
 * dict keyed by an exact `str`, whose hash and equality are the interpreter's and
 * have nothing to observe; everything else takes the protocol twice over, in the
 * order it would have — and `__getitem__` only where `__contains__` said yes,
 * which is the branch this stands in for */
static inline PyObject *By_DictFind(PyObject *container, PyObject *key) {
    /* a null operand carries an exception already set by whatever produced it */
    if (container == NULL || key == NULL) return NULL;
    if (BY_LIKELY(PyDict_CheckExact(container) && PyUnicode_CheckExact(key))) {
#if PY_VERSION_HEX >= 0x030D0000
        PyObject *value;
        /* -1 failed, 0 absent, 1 there — and absent leaves `value` NULL */
        if (PyDict_GetItemRef(container, key, &value) < 0) return NULL;
        return value;
#else
        PyObject *value = PyDict_GetItemWithError(container, key);
        /* absent and failed are both NULL here, which is what this returns for
         * both anyway — the exception state is what separates them */
        return value == NULL ? NULL : By_NewRef(value);
#endif
    }
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

/* build a dict from alternating key/value owned references */
static inline PyObject *By_BuildDict(PyObject **pairs, Py_ssize_t count) {
    PyObject *dict = PyDict_New();
    if (dict == NULL) {
        for (Py_ssize_t i = 0; i < count * 2; i++) Py_XDECREF(pairs[i]);
        return NULL;
    }
    for (Py_ssize_t i = 0; i < count; i++) {
        int failed = PyDict_SetItem(dict, pairs[i * 2], pairs[i * 2 + 1]) < 0;
        Py_XDECREF(pairs[i * 2]);
        Py_XDECREF(pairs[i * 2 + 1]);
        if (failed) {
            /* release whatever is left, then the dict */
            for (Py_ssize_t j = (i + 1) * 2; j < count * 2; j++) Py_XDECREF(pairs[j]);
            Py_DECREF(dict);
            return NULL;
        }
    }
    return dict;
}

static inline PyObject *By_BuildSet(PyObject **items, Py_ssize_t count) {
    PyObject *set = PySet_New(NULL);
    if (set == NULL) {
        for (Py_ssize_t i = 0; i < count; i++) Py_XDECREF(items[i]);
        return NULL;
    }
    for (Py_ssize_t i = 0; i < count; i++) {
        int failed = PySet_Add(set, items[i]) < 0;
        Py_XDECREF(items[i]);
        if (failed) {
            for (Py_ssize_t j = i + 1; j < count; j++) Py_XDECREF(items[j]);
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
    }
    PyObject *boxed = By_BoxInt(index);
    if (boxed == NULL) return NULL;
    PyObject *result = By_GetItem(container, boxed);
    Py_DECREF(boxed);
    return result;
}

/* `container[index]` where the index is already an integer register
 *
 * boxing one to look up a list element allocates a `PyLongObject` per iteration
 * that nothing ever sees. on the fast path the index never leaves its register;
 * everything else boxes it and takes the ordinary protocol.
 *
 * only the read itself is written here. an out-of-range index sets an error with
 * a message, and a container that is none of these three takes the protocol —
 * both are several times the size of what they guard, and inlining them into
 * every subscript in a loop is what a scan over a list was paying for. moving
 * them out of line is three per cent of the inheritance benchmark, and the three
 * that stay are the three the suite indexes */
static inline PyObject *By_GetItemTagged(PyObject *container, ByTagged index) {
    if (BY_LIKELY(container != NULL && By_IsShort(index))) {
        Py_ssize_t i = By_ShortValue(index);
        if (PyList_CheckExact(container)) {
            Py_ssize_t n = PyList_GET_SIZE(container);
            if (i < 0) i += n;
            if (BY_LIKELY(i >= 0 && i < n)) return By_NewRef(PyList_GET_ITEM(container, i));
        } else if (PyTuple_CheckExact(container)) {
            Py_ssize_t n = PyTuple_GET_SIZE(container);
            if (i < 0) i += n;
            if (BY_LIKELY(i >= 0 && i < n)) return By_NewRef(PyTuple_GET_ITEM(container, i));
        } else if (PyUnicode_CheckExact(container)) {
            return By_StrCharAt(container, i);
        }
    }
    return By_ItemSlow(container, index);
}

/* `s[i]` where the static type says `s` is a `str` and `i` an integer
 *
 * the general form goes through the protocol and then checks the result is a `str`
 * on the way back, because a subclass may hand back anything. an exact `str` cannot,
 * so both steps collapse into the character read — and a subclass still takes the
 * long way, check included */
static inline PyObject *By_StrItemTagged(PyObject *s, ByTagged index);

static inline PyObject *By_GetItem(PyObject *container, PyObject *index) {
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
        PyErr_SetObject(PyExc_KeyError, index);
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

static inline char By_SetItem(PyObject *container, PyObject *index, PyObject *value) {
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
 * negative index counts from the end, and out of range is `IndexError` */
static inline Py_ssize_t By_ArrayIndex(ByArrayHeader *array, ByTagged tagged) {
    if (array == NULL) return -1;
    if (BY_UNLIKELY(!By_IsShort(tagged))) {
        // a big integer cannot be a valid index into a buffer this size
        PyErr_SetString(PyExc_IndexError, "list index out of range");
        return -1;
    }
    Py_ssize_t index = By_ShortValue(tagged);
    if (index < 0) index += array->len;
    if (index < 0 || index >= array->len) {
        PyErr_SetString(PyExc_IndexError, "list index out of range");
        return -1;
    }
    return index;
}

/* grow to hold one more, doubling so a run of appends stays amortized constant */
static inline ByArrayHeader *By_ArrayGrow(ByArrayHeader *array, size_t width) {
    if (array == NULL) return NULL;
    if (array->len < array->cap) return array;
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

/* `*x` or `**x` in a display: everything `x` holds, merged into a container this
 * frame has just built — so the kind test below is exact rather than a guess
 *
 * the type errors are python's own, wording included. the harness compares
 * exception text, so a difference there is one a user would see */
static inline char By_Extend(PyObject *container, PyObject *source, int mapping) {
    if (container == NULL || source == NULL) return 2;
    if (mapping) {
        PyObject *keys = PyObject_GetAttrString(source, "keys");
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
 * the operand is *borrowed*, as every helper's is: the register holding it belongs
 * to the frame, which releases it on each exit path. `PyErr_SetObject` takes its
 * own reference, so a retain here would be a second one nobody owns — which is
 * exactly what leaked a `GeneratorExit` per abandoned generator, and the thrown
 * exception per `throw` */
static inline void By_Reraise(PyObject *value) {
    if (value == NULL) return;
    PyErr_SetObject((PyObject *)Py_TYPE(value), value);
}

/* ── a compiled method, as a decorator sees it ────────────────────────────────
 *
 * a class body hands a decorator the *function* it defined, and a python function
 * carries a `__dict__`: `abc.abstractmethod` writes `__isabstractmethod__` onto the
 * object it is given and hands that same object back. compiling the method
 * substitutes a method descriptor, which takes no attributes at all — so the
 * substitution, and not the decorator, is what would turn that into an import
 * failure.
 *
 * this is the descriptor with a `__dict__` on it: callable, binding, and writable,
 * which is the whole of what a decorator asks of a function. only a *decorated*
 * method is wrapped, so an ordinary one keeps the descriptor and the direct call
 */
typedef struct {
    PyObject_HEAD
    PyObject *fn;
    PyObject *dict;
} ByMethodObject;

static void By_Method_dealloc(ByMethodObject *self) {
    PyObject_GC_UnTrack(self);
    Py_CLEAR(self->fn);
    Py_CLEAR(self->dict);
    Py_TYPE(self)->tp_free((PyObject *)self);
}

static int By_Method_traverse(ByMethodObject *self, visitproc visit, void *arg) {
    Py_VISIT(self->fn);
    Py_VISIT(self->dict);
    return 0;
}

/* only the `__dict__`, which is the only side a cycle can run through: the method
 * itself is a descriptor owned by the type and has no way back here. leaving it in
 * place is what keeps a call and an attribute read from meeting a cleared field */
static int By_Method_clear(ByMethodObject *self) {
    Py_CLEAR(self->dict);
    return 0;
}

static PyObject *By_Method_call(ByMethodObject *self, PyObject *args, PyObject *kwds) {
    return PyObject_Call(self->fn, args, kwds);
}

/* bound the way a plain function is: reached through the type it is itself, reached
 * through an instance it is a method of that instance */
static PyObject *By_Method_descr_get(PyObject *self, PyObject *obj, PyObject *type) {
    (void)type;
    if (obj == NULL || obj == Py_None) return By_NewRef(self);
    return PyMethod_New(self, obj);
}

/* what a decorator *wrote* is ours; everything else it reads off a function —
 * `__name__`, `__qualname__`, `__doc__` — still belongs to the method */
static PyObject *By_Method_getattro(PyObject *self, PyObject *name) {
    PyObject *value = PyObject_GenericGetAttr(self, name);
    if (value == NULL && PyErr_ExceptionMatches(PyExc_AttributeError)) {
        PyErr_Clear();
        value = PyObject_GetAttr(((ByMethodObject *)self)->fn, name);
    }
    return value;
}

static PyGetSetDef By_Method_getset[] = {
    {"__dict__", PyObject_GenericGetDict, PyObject_GenericSetDict, NULL, NULL},
    {NULL, NULL, NULL, NULL, NULL},
};

static PyTypeObject By_MethodType = {
    PyVarObject_HEAD_INIT(NULL, 0)
    .tp_name = "by.method",
    .tp_basicsize = sizeof(ByMethodObject),
    .tp_itemsize = 0,
    .tp_dealloc = (destructor)By_Method_dealloc,
    .tp_call = (ternaryfunc)By_Method_call,
    .tp_getattro = By_Method_getattro,
    .tp_setattro = PyObject_GenericSetAttr,
    .tp_flags = Py_TPFLAGS_DEFAULT | Py_TPFLAGS_HAVE_GC,
    .tp_traverse = (traverseproc)By_Method_traverse,
    .tp_clear = (inquiry)By_Method_clear,
    .tp_getset = By_Method_getset,
    .tp_descr_get = By_Method_descr_get,
    .tp_dictoffset = offsetof(ByMethodObject, dict),
    .tp_free = PyObject_GC_Del,
};

static inline PyObject *By_Method(PyObject *fn) {
    ByMethodObject *self;
    if (fn == NULL) return NULL;
    if (PyType_Ready(&By_MethodType) < 0) return NULL;
    self = PyObject_GC_New(ByMethodObject, &By_MethodType);
    if (self == NULL) return NULL;
    self->fn = By_NewRef(fn);
    self->dict = NULL;
    PyObject_GC_Track(self);
    return (PyObject *)self;
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
                                     PyObject *const *twins, PyObject *const *types,
                                     Py_ssize_t classes) {
    if (count <= 0) return 0;
    if ((PyObject *)type == PyDict_GetItemString(dict, owner)) return 0;
    if (body != NULL && PyDict_GetItemString(body, name) != NULL) {
        return By_CopyClassConstant(body, type, name, twins, types, classes);
    }
    return By_ApplyMethodDecorators(type, dict, owner, name, decorators, count);
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

/* ── iteration ────────────────────────────────────────────────────────────── */

static inline PyObject *By_GetIter(PyObject *o) { return PyObject_GetIter(o); }

/* NULL means either exhausted or failed; the caller distinguishes with
 * PyErr_Occurred, exactly as the interpreter's FOR_ITER does */
static inline PyObject *By_IterNext(PyObject *it) { return PyIter_Next(it); }

/* ── str ──────────────────────────────────────────────────────────────────── */

static inline PyObject *By_StrConcat(PyObject *a, PyObject *b) {
    return PyUnicode_Concat(a, b);
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
    // the common containers know their own size in a field
    if (PyList_CheckExact(o)) return By_ShortFrom(PyList_GET_SIZE(o));
    if (PyUnicode_CheckExact(o)) return By_ShortFrom(PyUnicode_GET_LENGTH(o));
    if (PyTuple_CheckExact(o)) return By_ShortFrom(PyTuple_GET_SIZE(o));
    if (PyDict_CheckExact(o)) return By_ShortFrom(PyDict_GET_SIZE(o));
    if (PyBytes_CheckExact(o)) return By_ShortFrom(PyBytes_GET_SIZE(o));
    Py_ssize_t length = PyObject_Length(o);
    if (length < 0) return BY_INT_ERROR;
    return By_ShortFrom(length);
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
static inline void By_FinishGenerator(ByTagged *state) {
    By_DecRefTagged(*state);
    *state = By_ShortFrom(-1);
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
    const char *surface = frame == BY_FRAME_COROUTINE          ? "coroutine"
                          : frame == BY_FRAME_ASYNC_GENERATOR  ? "async generator"
                                                               : "generator";
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
 * on 3.12 and later `None` is immortal, so the pair of reference counts a `next()`
 * pays here are both branches that do no work */
static inline void By_ParkSent(PyObject **sent, PyObject *value) {
    PyObject *old = *sent;
    *sent = By_NewRef(value);
    Py_XDECREF(old);
}

/* resume a generator's frame, finishing it when the frame leaves for good */
static inline PyObject *By_StepGenerator(PyObject *self, PyObject **sent, PyObject **returned,
                                         ByTagged *state, int frame, PyObject *arg,
                                         PyObject *(*resume)(PyObject *)) {
    By_ParkSent(sent, arg);
    PyObject *result = resume(self);
    if (result != NULL) return result;
    By_FinishGenerator(state);
    /* an empty `$returned` is what says the frame left by *raising* rather than by
     * ending, and so is the one condition pep 479 asks about */
    if (*returned == NULL) By_ConvertStopIteration(frame);
    return By_TakeReturn(returned);
}

/* `throw(exc)`: raise it *at the suspension point*.
 *
 * the exception goes into the state object's `$thrown` field, and the resumption
 * point raises it — which is what lets a `yield` inside `try` enter its own handler
 * rather than the exception appearing at the generator's entry.
 *
 * rejecting the argument never reaches the frame at all, and python leaves a
 * generator resumable after a `throw` it refused to make sense of. a machine with no
 * suspension point does not reach the frame either, but a throw does finish it — see
 * below. otherwise it is the resumption that decides, and a body that catches what
 * was thrown leaves the machine usable.
 *
 * the resumption raises instead of producing a value, so nothing rides in on `$sent`
 * — it is parked as `None` all the same, because leaving the last `send`'s value
 * standing is what would let a later `yield` read it again */
static inline PyObject *By_ThrowInto(PyObject *self, PyObject **sent, PyObject **thrown,
                                   PyObject **returned, ByTagged *state, int frame,
                                   PyObject *exception,
                                   PyObject *(*resume)(PyObject *)) {
    if (thrown == NULL) return NULL;
    PyObject *instance = NULL;
    if (PyExceptionInstance_Check(exception)) {
        instance = By_NewRef(exception);
    } else if (PyExceptionClass_Check(exception)) {
        instance = PyObject_CallNoArgs(exception);
        if (instance == NULL) return NULL;
    } else {
        /* `throw` words this differently from `raise`, and names what it was given */
        PyErr_Format(PyExc_TypeError,
                     "exceptions must be classes or instances deriving from BaseException, not %s",
                     Py_TYPE(exception)->tp_name);
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
    if (By_ShortValue(*state) <= 0) {
        By_FinishGenerator(state);
        PyErr_SetObject((PyObject *)Py_TYPE(instance), instance);
        Py_DECREF(instance);
        return NULL;
    }
    PyObject *old = *thrown;
    *thrown = instance;
    Py_XDECREF(old);
    return By_StepGenerator(self, sent, returned, state, frame, Py_None, resume);
}

/* `close()`: throw `GeneratorExit` in and accept the three legal outcomes.
 *
 * exhausting, re-raising `GeneratorExit`, or being already finished are all a clean
 * close. *yielding* is not — cpython calls that a `RuntimeError`.
 *
 * the `StopIteration` accepted below is the frame's own *end*, which is the only kind
 * that can still be standing here: one the body raised has already become a
 * `RuntimeError` on its way out, and comes back as the failure it is */
static inline int By_CloseGenerator(PyObject *self, PyObject **sent, PyObject **thrown,
                                   PyObject **returned, ByTagged *state, int frame,
                                   PyObject *(*resume)(PyObject *)) {
    /* a machine with no suspension point has nothing to unwind, and closing one runs
     * no body at all — not even a `finally` the body has not reached yet. asking
     * `By_ThrowInto` would give the right answer for a finished frame and the wrong
     * one for a frame that never started, which would run the whole body under a
     * `GeneratorExit` it had no way to see */
    if (By_ShortValue(*state) <= 0) {
        By_FinishGenerator(state);
        return 0;
    }
    PyObject *exit = PyObject_CallNoArgs(PyExc_GeneratorExit);
    if (exit == NULL) return -1;
    PyObject *result = By_ThrowInto(self, sent, thrown, returned, state, frame, exit, resume);
    Py_DECREF(exit);
    if (result != NULL) {
        Py_DECREF(result);
        PyErr_SetString(PyExc_RuntimeError, "generator ignored GeneratorExit");
        return -1;
    }
    if (PyErr_ExceptionMatches(PyExc_StopIteration) || PyErr_ExceptionMatches(PyExc_GeneratorExit)) {
        PyErr_Clear();
        return 0;
    }
    return -1;
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

/* `with EXPR`: the manager's `__enter__`, looked up on the *type* the way the
 * interpreter does rather than on the instance */
/* `__aenter__` and `__aexit__`, which hand back *awaitables* rather than answers
 *
 * so these only start the call — the caller awaits what comes back, and only then
 * has the value `async with` binds or the answer that decides suppression
 */
static inline PyObject *By_AsyncEnter(PyObject *manager) {
    if (manager == NULL) return NULL;
    PyObject *method = PyObject_GetAttrString((PyObject *)Py_TYPE(manager), "__aenter__");
    if (method == NULL) {
        PyErr_Format(PyExc_TypeError,
                     "'%s' object does not support the asynchronous context manager protocol",
                     Py_TYPE(manager)->tp_name);
        return NULL;
    }
    PyObject *args[1] = {manager};
    PyObject *result = PyObject_Vectorcall(method, args, 1, NULL);
    Py_DECREF(method);
    return result;
}

static inline PyObject *By_AsyncExit(PyObject *manager, PyObject *exception) {
    if (manager == NULL) return NULL;
    PyObject *method = PyObject_GetAttrString((PyObject *)Py_TYPE(manager), "__aexit__");
    if (method == NULL) {
        PyErr_Format(PyExc_TypeError,
                     "'%s' object does not support the asynchronous context manager protocol "
                     "(missed __aexit__ method)",
                     Py_TYPE(manager)->tp_name);
        return NULL;
    }
    int raising = exception != NULL && exception != Py_None
                  && PyExceptionInstance_Check(exception);
    PyObject *type = raising ? (PyObject *)Py_TYPE(exception) : Py_None;
    PyObject *value = raising ? exception : Py_None;
    PyObject *traceback = Py_None;
    PyObject *found = NULL;
    if (raising) {
        found = PyException_GetTraceback(exception);
        if (found != NULL) traceback = found;
    }
    PyObject *args[4] = {manager, type, value, traceback};
    PyObject *result = PyObject_Vectorcall(method, args, 4, NULL);
    Py_DECREF(method);
    Py_XDECREF(found);
    return result;
}

static inline PyObject *By_Enter(PyObject *manager) {
    if (manager == NULL) return NULL;
    PyObject *method = PyObject_GetAttrString((PyObject *)Py_TYPE(manager), "__enter__");
    if (method == NULL) {
        PyErr_Format(PyExc_TypeError, "'%s' object does not support the context manager protocol",
                     Py_TYPE(manager)->tp_name);
        return NULL;
    }
    PyObject *args[1] = {manager};
    PyObject *result = PyObject_Vectorcall(method, args, 1, NULL);
    Py_DECREF(method);
    return result;
}

/* `__exit__`, on the normal path (`exception` NULL) or the exceptional one.
 *
 * returns 1 when the exception was *suppressed*, 0 when it was not, and -1 when
 * `__exit__` itself raised. the caller re-raises on 0, which is what makes
 * `with` transparent to an exception it does not swallow */
static inline int By_ExitContext(PyObject *manager, PyObject *exception) {
    if (manager == NULL) return -1;
    PyObject *method = PyObject_GetAttrString((PyObject *)Py_TYPE(manager), "__exit__");
    if (method == NULL) return -1;
    /* `None` is the normal path just as NULL is: the frontend hands over a boxed
       `None`, and reading a traceback off it would be a wild pointer */
    int raising = exception != NULL && exception != Py_None
                  && PyExceptionInstance_Check(exception);
    PyObject *type = raising ? (PyObject *)Py_TYPE(exception) : Py_None;
    PyObject *value = raising ? exception : Py_None;
    PyObject *traceback = Py_None;
    if (raising) {
        PyObject *found = PyException_GetTraceback(exception);
        if (found != NULL) traceback = found;
    }
    PyObject *args[4] = {manager, type, value, traceback};
    PyObject *result = PyObject_Vectorcall(method, args, 4, NULL);
    Py_DECREF(method);
    if (traceback != Py_None) Py_DECREF(traceback);
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
    type = Py_TYPE(awaitable);
    getter = type->tp_as_async == NULL ? NULL : type->tp_as_async->am_await;
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
                                            PyObject **returned, ByTagged *state, int frame,
                                            PyObject *(*resume)(PyObject *), PyObject *arg,
                                            PyObject **result) {
    PyObject *step;
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
                     free ? "cannot access free variable '%s' where it is not associated with a value"
                          : "cannot access local variable '%s' where it is not associated with a value",
                     name);
        return NULL;
    }
    return By_NewRef(value);
}

/* bind a method to a receiver, giving a callable. this is what a nested
 * function's name is bound to: the receiver is its closure environment, and the
 * fastcall convention hands it back as `self` on every call */
static inline PyObject *By_MakeClosure(PyMethodDef *def, PyObject *env) {
    if (env == NULL) return NULL;
    return PyCFunction_NewEx(def, env, NULL);
}

/* a value arriving where a native class is expected: check, then take a
   reference. without the check a python caller could store any object in a
   field and every later field read would follow a wild pointer */
static inline PyObject *By_UnboxInstance(PyObject *o, PyTypeObject *type) {
    if (!PyObject_TypeCheck(o, type)) {
        PyErr_Format(PyExc_TypeError, "expected %s, got %s", type->tp_name,
                     Py_TYPE(o)->tp_name);
        return NULL;
    }
    return By_NewRef(o);
}

/* a `list` is a `PyObject *` like every other container, so narrowing to one is a
 * type check rather than a change of representation — but it is still a check,
 * because the value came from somewhere that only promised an object */
static inline PyObject *By_UnboxList(PyObject *o) {
    if (o == NULL || !PyList_Check(o)) {
        By_TypeError("list", o);
        return NULL;
    }
    Py_INCREF(o);
    return o;
}

static inline PyObject *By_UnboxStr(PyObject *o) {
    if (o == NULL || !PyUnicode_Check(o)) {
        By_TypeError("str", o);
        return NULL;
    }
    Py_INCREF(o);
    return o;
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

#endif /* BY_RT_H */
