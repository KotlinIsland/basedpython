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
#include <string.h>
#include <math.h>
#include <stdint.h>
#include <stddef.h>

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

static inline int By_IsShort(ByTagged x) { return (x & BY_INT_TAG) == 0; }

static inline Py_ssize_t By_ShortValue(ByTagged x) { return ((Py_ssize_t)x) >> 1; }

static inline ByTagged By_ShortFrom(Py_ssize_t v) {
    return (ByTagged)((size_t)v << 1);
}

static inline int By_FitsShort(Py_ssize_t v) {
    return v >= BY_SHORT_MIN && v <= BY_SHORT_MAX;
}

static inline PyObject *By_LongOf(ByTagged x) {
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

static inline void By_DecRefTagged(ByTagged x) {
    if (BY_UNLIKELY(!By_IsShort(x) && x != BY_INT_ERROR)) {
        Py_DECREF(By_LongOf(x));
    }
}

static inline void By_IncRefTagged(ByTagged x) {
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

static inline ByTagged By_IntSlowBinary(ByTagged a, ByTagged b, const char *op) {
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
static inline ByTagged By_IntSlowBitwise(ByTagged a, ByTagged b, char op) {
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

static inline ByTagged By_IntAdd(ByTagged a, ByTagged b) {
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

static inline ByTagged By_IntSub(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) {
        Py_ssize_t x = (Py_ssize_t)a, y = (Py_ssize_t)b;
        Py_ssize_t diff = (Py_ssize_t)((size_t)x - (size_t)y);
        if (BY_LIKELY(((x ^ y) & (x ^ diff)) >= 0)) return (ByTagged)diff;
    }
    return By_IntSlowBinary(a, b, "-");
}

/* a product of two values within this bound cannot leave the short range */
#define BY_MUL_SAFE (((Py_ssize_t)1) << ((sizeof(Py_ssize_t) * 8 - 4) / 2))

static inline ByTagged By_IntMul(ByTagged a, ByTagged b) {
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
static inline void By_ZeroDivision(binaryfunc operation, int floating) {
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
static inline Py_ssize_t By_FloorDivSsize(Py_ssize_t a, Py_ssize_t b) {
    Py_ssize_t q = a / b;
    if ((a % b != 0) && ((a < 0) != (b < 0))) q--;
    return q;
}

static inline Py_ssize_t By_ModSsize(Py_ssize_t a, Py_ssize_t b) {
    Py_ssize_t r = a % b;
    if (r != 0 && ((r < 0) != (b < 0))) r += b;
    return r;
}

static inline ByTagged By_IntFloorDiv(ByTagged a, ByTagged b) {
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

static inline ByTagged By_IntMod(ByTagged a, ByTagged b) {
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
static inline ByTagged By_IntAnd(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) return a & b;
    return By_IntSlowBitwise(a, b, '&');
}
static inline ByTagged By_IntOr(ByTagged a, ByTagged b) {
    if (BY_LIKELY(By_IsShort(a) && By_IsShort(b))) return a | b;
    return By_IntSlowBitwise(a, b, '|');
}
static inline ByTagged By_IntXor(ByTagged a, ByTagged b) {
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

static inline char By_IntCompareSlow(ByTagged a, ByTagged b, int op) {
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
    static inline char name(ByTagged a, ByTagged b) {                          \
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
static inline ByTagged By_IntFromI64(int64_t value) {
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

static inline double By_FloatObjectSlow(double a, PyObject *b,
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

static inline double By_ObjFloatSlow(PyObject *a, double b,
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
 * back to its tail the way `By_ErrorName` trims a method's */
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

/* reading a local on a path that never assigned it. the phrasing is the running
 * python's, not the compiler's — it changed in 3.11 and a compiled module has to say
 * what the interpreter beside it would say */
static inline void By_RaiseUnboundLocal(const char *name) {
#if PY_VERSION_HEX >= 0x030B0000
    PyErr_Format(PyExc_UnboundLocalError,
                 "cannot access local variable '%s' where it is not associated with a value",
                 name);
#else
    PyErr_Format(PyExc_UnboundLocalError,
                 "local variable '%s' referenced before assignment", name);
#endif
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

/* whether a type spec can be built on this tuple of bases
 *
 * `PyType_FromSpecWithBases` gives the type it builds `type` as its own, so any base
 * with another metaclass is a conflict python rejects at import. it also wants a base
 * to pick a layout from, which an empty tuple does not offer — `type` supplies `object`
 * for that case and a spec does not */
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
 * disagree with the layout base the interpreted definition is what answers */
static inline int By_OffsetsHoldUp(PyTypeObject *type) {
    PyTypeObject *base = type->tp_base;
    if (base == NULL) {
        return 1;
    }
    return type->tp_dictoffset == base->tp_dictoffset
           && type->tp_weaklistoffset == base->tp_weaklistoffset;
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
    if (!By_OffsetsHoldUp((PyTypeObject *)cls)) {
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

/* the class `meta(name, bases, namespace, **kwds)` builds, with `methods` in that
 * namespace
 *
 * the methods go in *before* the call rather than onto the finished type, and both
 * halves of that matter. `type.__new__` fills the type slots from the namespace, so a
 * `__repr__` entry becomes `tp_repr` with no adapter of ours; and a metaclass that
 * reads the namespace — an `ABCMeta` deciding which of the base's abstract methods
 * this class left abstract — sees what the class actually defines.
 *
 * the descriptors name `object` as their owner because the type they belong to is what
 * this call produces. that is also the more faithful answer: the interpreted twin holds
 * plain functions there, and a plain function checks no receiver either */
static inline PyObject *By_TypeThroughMetaclass(PyObject *module_dict, const char *name,
                                                PyObject *bases, PyObject *orig_bases,
                                                PyObject *kwds, PyMethodDef *methods) {
    PyMethodDef *def;
    PyObject *module_name, *prepare, *args, *ns, *cls = NULL;
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
    args = Py_BuildValue("(sOO)", name, bases, ns);
    Py_DECREF(ns);
    if (args != NULL) {
        cls = PyObject_Call(meta, args, kwds);
        Py_DECREF(args);
    }
    Py_DECREF(meta);
    /* a `metaclass` that is not a type may hand back anything, and what it hands back is
     * what the name means — but it is not a type this module can hang a constant or a
     * decorated method on, so the interpreted definition is what stands under it */
    if (cls != NULL && !PyType_Check(cls)) {
        Py_DECREF(cls);
        cls = By_LookupGlobalString(module_dict, name);
    }
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
 * and a class-level constant both land after the metaclass has decided what the class
 * defines, and a metaclass that reads its namespace would disagree with them */
static inline PyObject *By_BuildClass(PyObject *module_dict, const char *name,
                                      PyObject *bases, PyObject *kwds, PyMethodDef *methods,
                                      PyType_Spec *spec, int through_metaclass) {
    PyObject *cls, *resolved;
    if (bases == NULL) return NULL;
    resolved = By_ResolveBases(bases);
    if (resolved == NULL) {
        Py_DECREF(bases);
        return NULL;
    }
    if (spec != NULL && By_SpecTakesBases(resolved)) {
        cls = PyType_FromSpecWithBases(spec, resolved);
        if (cls != NULL && !By_OffsetsHoldUp((PyTypeObject *)cls)) {
            Py_DECREF(cls);
            cls = By_LookupGlobalString(module_dict, name);
        }
    } else if (through_metaclass) {
        cls = By_TypeThroughMetaclass(module_dict, name, resolved,
                                      resolved == bases ? NULL : bases, kwds, methods);
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

#if PY_VERSION_HEX >= 0x030A0000
static inline char By_IsMatchSequence(PyObject *o) {
    return (char)(o != NULL && PyType_HasFeature(Py_TYPE(o), Py_TPFLAGS_SEQUENCE));
}
#endif

/* move a class-level constant from the interpreted definition onto the compiled
 * type
 *
 * a *static* type is immutable to `setattr`, which is what licenses direct
 * dispatch — so this writes the type's dict, the way a C extension declares its
 * own class attributes
 */
static inline int By_CopyClassConstant(PyObject *module_dict, const char *class_name,
                                       PyTypeObject *type, const char *name) {
    PyObject *twin = PyDict_GetItemString(module_dict, class_name);
    if (twin == NULL) return 0;
    PyObject *value = PyObject_GetAttrString(twin, name);
    if (value == NULL) {
        PyErr_Clear();
        return 0;
    }
    int result = PyDict_SetItemString(type->tp_dict, name, value);
    Py_DECREF(value);
    if (result == 0) PyType_Modified(type);
    return result;
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
 * whichever class was under the name by then, and it is `By_ReachesTwin`'s whole job to
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

/* whether `value` could hand one of this module's interpreted class twins back
 *
 * this is the whole safety of adopting an attribute. a twin is a class that is about to
 * stop being the one under its name, so anything holding it will answer about a class
 * nothing else can reach: that is a *silent* wrong answer, where not adopting at all
 * leaves the loud one the attribute already gave. so the shapes that provably cannot
 * hold a twin are enumerated, and every other shape answers yes and is left alone.
 *
 * a function is safe with an empty closure and simple defaults because the only other
 * route it has to a class is a name it resolves through a namespace at call time — and
 * the namespace this module owns holds the *compiled* type by then, which is the answer
 * that name should give
 *
 * `depth` bounds the recursion rather than the reachability: past it every shape answers
 * yes, so a value nested deeper than this looked is refused rather than assumed */
static int By_ReachesTwin(PyObject *value, PyObject *const *twins, Py_ssize_t count,
                          int depth) {
    Py_ssize_t index;
    if (value == NULL || value == Py_None) return 0;
    for (index = 0; index < count; index++) {
        if (value == twins[index]) return 1;
    }
    if (depth <= 0) return 1;
    if (PyBool_Check(value) || PyLong_Check(value) || PyFloat_Check(value)
        || PyComplex_Check(value) || PyUnicode_Check(value) || PyBytes_Check(value)) {
        return 0;
    }
    /* a class is safe as itself: every class this module's body wrote with a `class`
     * statement is among the twins, so one that is not is a class both the interpreted
     * module and this one hold the same object for. what it must not do is *stand* on a
     * twin — a class built at runtime over one has a base nothing else can reach */
    if (PyType_Check(value)) {
        PyObject *mro = ((PyTypeObject *)value)->tp_mro;
        Py_ssize_t at;
        if (mro == NULL || !PyTuple_Check(mro)) return 1;
        for (at = 0; at < PyTuple_GET_SIZE(mro); at++) {
            for (index = 0; index < count; index++) {
                if (PyTuple_GET_ITEM(mro, at) == twins[index]) return 1;
            }
        }
        return 0;
    }
    /* the two parameterised forms python builds in C — `list[int]` and `int | None`. each
     * is an origin and a tuple of arguments and nothing besides, both read off a member
     * rather than through anything that runs. `typing.Optional[int]` is a python object
     * whose attribute access is python code, and is not among these */
    if (Py_TYPE(value) == &Py_GenericAliasType || Py_TYPE(value) == By_UnionType()) {
        static const char *const parts[] = {"__origin__", "__args__"};
        for (index = 0; index < 2; index++) {
            PyObject *part = PyObject_GetAttrString(value, parts[index]);
            int reaches;
            if (part == NULL) {
                PyErr_Clear();
                continue;
            }
            reaches = By_ReachesTwin(part, twins, count, depth - 1);
            Py_DECREF(part);
            if (reaches) return 1;
        }
        return 0;
    }
    /* a tuple only, of the containers: a list, a dict or a set can be given a twin
     * after this has answered */
    if (PyTuple_Check(value)) {
        for (index = 0; index < PyTuple_GET_SIZE(value); index++) {
            if (By_ReachesTwin(PyTuple_GET_ITEM(value, index), twins, count, depth - 1)) {
                return 1;
            }
        }
        return 0;
    }
    if (PyFunction_Check(value)) {
        PyObject *closure = PyFunction_GetClosure(value);
        PyObject *kwdefaults;
        if (closure != NULL && PyTuple_GET_SIZE(closure) > 0) return 1;
        if (By_ReachesTwin(PyFunction_GetDefaults(value), twins, count, depth - 1)) return 1;
        kwdefaults = PyFunction_GetKwDefaults(value);
        if (kwdefaults != NULL && PyDict_Check(kwdefaults)) {
            PyObject *key, *item;
            Py_ssize_t position = 0;
            while (PyDict_Next(kwdefaults, &position, &key, &item)) {
                if (By_ReachesTwin(item, twins, count, depth - 1)) return 1;
            }
        }
        return 0;
    }
    if (Py_TYPE(value) == &PyProperty_Type || Py_TYPE(value) == &PyStaticMethod_Type
        || Py_TYPE(value) == &PyClassMethod_Type) {
        static const char *const parts[] = {"fget", "fset", "fdel", "__func__"};
        for (index = 0; index < 4; index++) {
            PyObject *part = PyObject_GetAttrString(value, parts[index]);
            int reaches;
            if (part == NULL) {
                PyErr_Clear();
                continue;
            }
            reaches = By_ReachesTwin(part, twins, count, depth - 1);
            Py_DECREF(part);
            if (reaches) return 1;
        }
        return 0;
    }
    return 1;
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
 * attribute agree with the namespace. anything else is carried only where it provably
 * cannot reach a twin at all. the answer is a borrowed reference, because every value
 * here is held by either the source dict or the module namespace */
static PyObject *By_TwinReplacement(PyObject *value, PyObject *const *twins,
                                    PyObject *const *types, Py_ssize_t count) {
    Py_ssize_t index;
    for (index = 0; index < count; index++) {
        if (value == twins[index] && types[index] != NULL) return types[index];
    }
    if (By_ReachesTwin(value, twins, count, 4)) return NULL;
    return value;
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
            if (stands == NULL) {
                Py_DECREF(carried);
                carried = By_LostAnnotations();
                break;
            }
            if (PyDict_SetItem(carried, key, stands) < 0) {
                Py_DECREF(carried);
                Py_DECREF(names);
                return -1;
            }
        }
        Py_DECREF(names);
    } else {
        /* a body that assigned `__annotations__` itself, which python leaves alone */
        PyObject *stands = By_TwinReplacement(written, twins, types, count);
        carried = stands == NULL ? By_LostAnnotations() : By_NewRef(stands);
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
 * carried attribute agree with the namespace, and every other value is carried only
 * where `By_ReachesTwin` says no.
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
            int present;
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
            if (PyDict_SetItem(target, key, carried) < 0) {
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

#if PY_VERSION_HEX >= 0x030A0000
static inline char By_IsMatchMapping(PyObject *o) {
    return (char)(o != NULL && PyType_HasFeature(Py_TYPE(o), Py_TPFLAGS_MAPPING));
}
#endif

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

/* `container[index]` where the index is already an integer register
 *
 * boxing one to look up a list element allocates a `PyLongObject` per iteration
 * that nothing ever sees. on the fast path the index never leaves its register;
 * everything else boxes it and takes the ordinary protocol
 */
static inline PyObject *By_GetItemTagged(PyObject *container, ByTagged index) {
    if (container == NULL || index == BY_INT_ERROR) return NULL;
    if (BY_LIKELY(By_IsShort(index))) {
        Py_ssize_t i = By_ShortValue(index);
        if (PyList_CheckExact(container)) {
            Py_ssize_t n = PyList_GET_SIZE(container);
            if (i < 0) i += n;
            if (i >= 0 && i < n) return By_NewRef(PyList_GET_ITEM(container, i));
            PyErr_SetString(PyExc_IndexError, "list index out of range");
            return NULL;
        }
        if (PyUnicode_CheckExact(container)) return By_StrCharAt(container, i);
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
 * each decorator is looked up in the module namespace, so `@property` and a
 * user-defined one resolve the same way. they are folded in memory and the result
 * written once, which is also what a class body does: the namespace never holds a
 * half-decorated method. `PyType_Modified` is what makes the change visible — the
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
        PyObject *fn = By_LookupGlobalString(dict, decorators[index - 1]);
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

/* apply `dict[decorator]` to `dict[name]`, in place. this is what lets a
 * decorated function still be compiled: the native one goes into the namespace,
 * then the decorator wraps it, exactly as the `def` statement would have */
static inline int By_ApplyDecorator(PyObject *dict, const char *name, const char *decorator) {
    PyObject *target = PyDict_GetItemString(dict, name);
    if (target == NULL) {
        PyErr_Format(PyExc_NameError, "name '%s' is not defined", name);
        return -1;
    }
    Py_INCREF(target);
    PyObject *fn = By_LookupGlobalString(dict, decorator);
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

/* `len` of anything with a length, as a tagged int */
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

/* resume a generator's frame, finishing it when the frame leaves by raising */
static inline PyObject *By_StepGenerator(PyObject *self, ByTagged *state,
                                         PyObject *(*resume)(PyObject *)) {
    PyObject *result = resume(self);
    if (result == NULL) By_FinishGenerator(state);
    return result;
}

/* `throw(exc)`: raise it *at the suspension point*.
 *
 * the exception goes into the state object's `$thrown` field, and the resumption
 * point raises it — which is what lets a `yield` inside `try` enter its own handler
 * rather than the exception appearing at the generator's entry.
 *
 * only the resumption finishes the machine. rejecting the argument never reaches
 * the frame at all, and python leaves a generator resumable after a `throw` it
 * refused to make sense of */
static inline PyObject *By_ThrowInto(PyObject *self, PyObject **thrown, ByTagged *state,
                                   PyObject *exception, PyObject *(*resume)(PyObject *)) {
    if (thrown == NULL) return NULL;
    PyObject *instance = NULL;
    if (PyExceptionInstance_Check(exception)) {
        instance = By_NewRef(exception);
    } else if (PyExceptionClass_Check(exception)) {
        instance = PyObject_CallNoArgs(exception);
        if (instance == NULL) return NULL;
    } else {
        PyErr_SetString(PyExc_TypeError, "exceptions must derive from BaseException");
        return NULL;
    }
    PyObject *old = *thrown;
    *thrown = instance;
    Py_XDECREF(old);
    return By_StepGenerator(self, state, resume);
}

/* `close()`: throw `GeneratorExit` in and accept the three legal outcomes.
 *
 * exhausting, re-raising `GeneratorExit`, or being already finished are all a clean
 * close. *yielding* is not — cpython calls that a `RuntimeError` */
static inline int By_CloseGenerator(PyObject *self, PyObject **thrown, ByTagged *state,
                                   PyObject *(*resume)(PyObject *)) {
    PyObject *exit = PyObject_CallNoArgs(PyExc_GeneratorExit);
    if (exit == NULL) return -1;
    PyObject *result = By_ThrowInto(self, thrown, state, exit, resume);
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

/* report every parameter the caller left out, in cpython's own wording.
 *
 * matching the message matters more than it looks: the differential harness compares
 * exception text, and a difference there is a difference a user would see */
/* every parameter with no default that nothing filled, named the way python names
 * them — and positional and keyword-only are counted separately, because python
 * reports them in two different sentences */
/* python began qualifying a method by its class in 3.10, so the name the compiler
 * wrote is trimmed back to its tail on an interpreter that would not have used it */
static inline const char *By_ErrorName(const char *fname) {
#if PY_VERSION_HEX >= 0x030A0000
    return fname;
#else
    const char *dot = strrchr(fname, '.');
    return dot == NULL ? fname : dot + 1;
#endif
}

static inline int By_CheckRequired(const char *const *names, const unsigned char *required,
                                  Py_ssize_t count, Py_ssize_t kwonly, PyObject **out,
                                  const char *fname) {
    fname = By_ErrorName(fname);
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

/* the constructor's binding: the same rules [`By_BindArgs`] applies, read off a tuple
 * and a dict rather than a fastcall vector — which is the whole of what differs
 *
 * `out[i]` receives a *borrowed* pointer, or NULL where the caller supplied nothing
 * and the default fills it. python counts `self` in its arity message and not in its
 * missing-argument one, so this does too */
static inline int By_BindInit(PyObject *args, PyObject *kwds, const char *const *names,
                              Py_ssize_t count, const unsigned char *required,
                              Py_ssize_t posonly, Py_ssize_t kwonly, PyObject **out,
                              int variadic, int extras, const char *fname, int inherited) {
    fname = By_ErrorName(fname);
    for (Py_ssize_t i = 0; i < count; i++) out[i] = NULL;
    Py_ssize_t nargs = args == NULL ? 0 : PyTuple_GET_SIZE(args);
    /* a keyword-only parameter is one nothing positional can reach, so the run a
     * caller may fill positionally ends where they begin */
    Py_ssize_t positional_limit = count - kwonly;
    if (nargs > positional_limit && !variadic) {
        /* a class with no `__init__` at all is rejected by `object.__init__`, which
         * names the class and does not count a receiver it never had. a *written*
         * `def __init__(self)` takes no arguments either and still reports as a method */
        if (inherited) {
            PyErr_Format(PyExc_TypeError, "%s() takes no arguments", fname);
            return -1;
        }
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

/* bind fastcall arguments to parameter positions, honouring keywords.
 *
 * `receiver` is 1 for a method, whose `self` arrives outside the vector but which
 * python still counts in an arity message.
 *
 * `out[i]` receives a *borrowed* pointer, or NULL where the caller did not supply
 * that parameter — the wrapper fills those from the defaults. returns -1 with an
 * exception set on a duplicate, an unexpected name, or too many positionals */
static inline int By_BindArgs(PyObject *const *args, Py_ssize_t nargs, PyObject *kwnames,
                              const char *const *names, Py_ssize_t count,
                              const unsigned char *required, Py_ssize_t posonly,
                              Py_ssize_t kwonly, PyObject **out, int variadic, int extras,
                              const char *fname, Py_ssize_t receiver) {
    fname = By_ErrorName(fname);
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

#if PY_VERSION_HEX < 0x030A0000
/* `PyIter_Send`'s contract, for interpreters that predate it.
 *
 * the outcomes it reports are the same. what it cannot do is take the `am_send`
 * shortcut, so a delegation that finishes still pays to build the
 * `StopIteration` and to read its value back through the attribute */
typedef enum { PYGEN_RETURN = 0, PYGEN_ERROR = -1, PYGEN_NEXT = 1 } PySendResult;

static inline PySendResult By_IterSend(PyObject *iter, PyObject *arg, PyObject **result) {
    PyObject *type = NULL, *value = NULL, *traceback = NULL, *carried = NULL;
    if (arg == Py_None && PyIter_Check(iter)) {
        *result = Py_TYPE(iter)->tp_iternext(iter);
        /* a plain exhausted iterator returns NULL with nothing set */
        if (*result == NULL && !PyErr_Occurred()) {
            *result = By_NewRef(Py_None);
            return PYGEN_RETURN;
        }
    } else {
        PyObject *send = PyObject_GetAttrString(iter, "send");
        if (send == NULL) {
            *result = NULL;
            return PYGEN_ERROR;
        }
        *result = PyObject_Vectorcall(send, &arg, 1, NULL);
        Py_DECREF(send);
    }
    if (*result != NULL) return PYGEN_NEXT;
    if (!PyErr_ExceptionMatches(PyExc_StopIteration)) return PYGEN_ERROR;
    PyErr_Fetch(&type, &value, &traceback);
    PyErr_NormalizeException(&type, &value, &traceback);
    if (value != NULL) carried = PyObject_GetAttrString(value, "value");
    Py_XDECREF(type);
    Py_XDECREF(value);
    Py_XDECREF(traceback);
    if (carried == NULL) {
        PyErr_Clear();
        carried = By_NewRef(Py_None);
    }
    *result = carried;
    return PYGEN_RETURN;
}
#else
#define By_IterSend PyIter_Send
#endif

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
