//! spell out the typing spec's `float` / `complex` special case
//!
//! python's typing spec reads a `float` annotation as `int | float` and a
//! `complex` annotation as `int | float | complex`. upstream typeshed is written
//! against that rule, so `def sleep(secs: float)` really means "give me an int or
//! a float", and the stub only says `float` because the spec supplies the rest
//!
//! basedpython has no such rule: `float` means `float`. reading upstream's
//! shorthand as written would therefore reject `time.sleep(1)`, which works
//! perfectly well at runtime — the stub would be lying about what the function
//! accepts. so this patch writes the union the spec means into the stub itself:
//!
//! ```text
//! def sleep(secs: float) -> None      ->  def sleep(secs: int | float) -> None
//! def f(z: complex) -> None           ->  def f(z: int | float | complex) -> None
//! def g(xs: list[float]) -> None      ->  def g(xs: list[int | float]) -> None
//! ```
//!
//! only positions that **accept** a value are widened, because that is all the
//! special case is about. which those are is a question of *direction*, not of
//! which syntactic slot the annotation sits in. a parameter
//! accepts; a return, a `let`, a module constant, an attribute, a class base and
//! a type variable's default all produce, and each callable parameter list along
//! the way flips which it is. so a callback handed *to* a function keeps its own
//! parameters exact, while a callable handed *back* accepts in its parameters
//! again.
//!
//! widening something that produces invents a union the reader then has to
//! narrow away — the whole reason `(0.0).real` used to come back `int | float`
//! instead of `float`. upstream typeshed is drawing the same line itself, one
//! function at a time, behind the `_typeshed.FloatInt` alias
//! (python/typeshed#16059); this patch is the whole sweep, done mechanically
//!
//! two more things accept alongside parameters: the constraints and bound of a
//! type variable, which a call solves from its arguments (`statistics.mean`
//! really does take `int`s), and a type alias the whole-tree `scan` finds used
//! for nothing but parameters — an alias is a name for the accepted set, so it
//! has to say so.
//!
//! nothing is widened where the union could not be spelled anyway: inside
//! `type[...]`, which names a class rather than a value of it (and whose
//! argument is often a constrained type parameter `int | float` cannot even
//! specialize), or in a union arm whose all-`int` reading already sits beside it
//! (`list[int] | list[float]` covers both, while `list[int | float]` — `list`
//! being invariant — would cover neither)
//!
//! an attribute the caller assigns to (`server.timeout = 5`) is an input in the
//! same sense a parameter is, but it is also a place a value is read back out
//! of, and there is no way to widen it for the writer without widening it for
//! the reader. those stay exact, so `timeout` is a `float` and an `int` written
//! there is reported — basedpython says the same about `x: float` on a class the
//! user wrote, so the stubs are not held to a different standard
//!
//! the arms are written `int | float`, not `float | int`, so the union prints in
//! the order the reader already sees everywhere else
//!
//! the rewrite is idempotent: an arm is only added where it is not already
//! reachable, so a second run over a patched tree produces no edits, and both
//! the unions upstream already spells out by hand (`float | None`, `int | float`)
//! and the constraint lists that already carry an `int` alongside the `float`
//! (`array[Element in (int, float, str)]`) are left alone

use std::collections::BTreeSet;
use std::path::Path;

use ruff_python_ast::{
    Expr, ModModule, Operator, Parameters, PySourceType, Stmt, TypeParam, TypeParams, UnaryOp,
};
use ruff_python_parser::{Parsed, parse_unchecked_source};
use ruff_text_size::Ranged;
use walkdir::WalkDir;

use crate::{Edit, Patch};

/// the numeric-tower arms, widest last. a `float` annotation admits everything
/// up to and including `float`, a `complex` annotation everything up to
/// `complex`
const TOWER: [&str; 3] = ["int", "float", "complex"];

/// every declaration in the standard library whose direction the syntax does not
/// give away, found by auditing all 2369 `float` / `complex` annotations in the
/// vendored tree against CPython. `numeric_promotion_corrections_all_apply`
/// fails if one of these stops matching, so an upstream rename is caught rather
/// than silently dropped
static CORRECTIONS: &[Correction] = &[
    // ---- a getter that hands an argument straight back ----
    Correction {
        module: "colorsys",
        path: "hls_to_rgb",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "short-circuits with `if s == 0.0: return l, l, l`, so hls_to_rgb(0, 1, 0) is (1, 1, 1)",
    },
    Correction {
        module: "colorsys",
        path: "hsv_to_rgb",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "every branch returns `v` unchanged in one slot, so hsv_to_rgb(0, 0, 1) is (1, 1, 1)",
    },
    Correction {
        module: "colorsys",
        path: "rgb_to_hsv",
        scope: Scope::Return,
        fix: Fix::Rewrite {
            from: "(float, float, float)",
            to: "(float, float, int | float)",
        },
        why: "hue and saturation are always divisions, but value is `max(r, g, b)` — one of the arguments, so rgb_to_hsv(2, 1, 1) is (0.0, 0.5, 2)",
    },
    Correction {
        module: "locale",
        path: "atof",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "returns `func(delocalize(string))` unchanged, and locale.atoi calls it with `int`",
    },
    Correction {
        module: "optparse",
        path: "check_builtin",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "dispatches on option.type through _builtin_cvt, so the \"int\" type really returns an int",
    },
    Correction {
        module: "random",
        path: "Random.triangular",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "on ZeroDivisionError (mode set and high == low) it does `return low`, handing the argument back",
    },
    Correction {
        module: "sched",
        path: "scheduler.run",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "returns the next event's `time` field unchanged: enterabs(5, ...) then run() gives back the int 5",
    },
    Correction {
        module: "asyncio.events",
        path: "TimerHandle.when",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "stores `self._when = when` with no conversion: loop.call_at(1000, cb).when() is the int 1000",
    },
    Correction {
        module: "asyncio.timeouts",
        path: "Timeout.when",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "__init__ and reschedule store `when` unchanged and when() hands it back",
    },
    Correction {
        module: "math",
        path: "sumprod",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "only takes the extended-precision path when a float appears; all-int inputs accumulate as ints",
    },
    Correction {
        module: "tkinter",
        path: "Scale.get",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "tries self.tk.getint first and only then getdouble — its own docstring says \"as integer or float\"",
    },
    Correction {
        module: "tkinter.ttk",
        path: "Scale.get",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the no-coordinate form returns the widget's stored -value object unchanged",
    },
    Correction {
        module: "importlib.abc",
        path: "SourceLoader.path_mtime",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "an override point CPython only consumes via int(); its own docstring says the value may be an int",
    },
    Correction {
        module: "_frozen_importlib_external",
        path: "SourceLoader.path_mtime",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the same override point, in the frozen bootstrap copy of importlib",
    },
    Correction {
        module: "builtins",
        path: "memoryview.cast",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "I is invariant and serves __setitem__ too, and pack_single for 'f'/'d' goes through PyFloat_AsDouble",
    },
    // ---- turtle: the state getters return exactly what was set ----
    Correction {
        module: "turtle",
        path: "TNavigator.xcor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "returns self._position[0], and goto(3, 4) stores Vec2D(3, 4) verbatim",
    },
    Correction {
        module: "turtle",
        path: "TNavigator.ycor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "as TNavigator.xcor, but over self._position[1]",
    },
    Correction {
        module: "turtle",
        path: "xcor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of TNavigator.xcor",
    },
    Correction {
        module: "turtle",
        path: "ycor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of TNavigator.ycor",
    },
    Correction {
        module: "turtle",
        path: "TurtleScreen.colormode",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the setter stores int(cmode) for the 255 mode, so the getter hands back the int 255",
    },
    Correction {
        module: "turtle",
        path: "colormode",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of TurtleScreen.colormode",
    },
    Correction {
        module: "turtle",
        path: "RawTurtle.shapesize",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "returns (*self._stretchfactor, self._outlinewidth), both stored verbatim and defaulting to the int 1",
    },
    Correction {
        module: "turtle",
        path: "shapesize",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of RawTurtle.shapesize",
    },
    Correction {
        module: "turtle",
        path: "RawTurtle.shearfactor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "pen() assigns self._shearfactor straight from the user's value with no coercion",
    },
    Correction {
        module: "turtle",
        path: "shearfactor",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of RawTurtle.shearfactor",
    },
    Correction {
        module: "turtle",
        path: "RawTurtle.shapetransform",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the setter assigns self._shapetrafo = (m11, m12, m21, m22) directly from the arguments",
    },
    Correction {
        module: "turtle",
        path: "shapetransform",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the module-level form of RawTurtle.shapetransform",
    },
    Correction {
        module: "turtle",
        path: "Vec2D.__mul__",
        scope: Scope::Return,
        fix: Fix::Accepts,
        why: "the Vec2D overload is the inner product, an int for integer vectors — while __abs__ stays float, being **0.5",
    },
    Correction {
        module: "turtle",
        path: "Vec2D",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "__new__ takes int | float and calls tuple.__new__ with it, so Vec2D(3, 4)[0] is the int 3",
    },
    // ---- a knob the caller is expected to set ----
    Correction {
        module: "socketserver",
        path: "BaseServer.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "documented as a settable attribute, forwarded only to selector timeouts",
    },
    Correction {
        module: "socketserver",
        path: "ForkingMixIn.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "the same override-this attribute, on the forking mix-in",
    },
    Correction {
        module: "socketserver",
        path: "StreamRequestHandler.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "handler subclasses set `timeout = 5`, and it goes straight to socket.settimeout",
    },
    Correction {
        module: "asyncio.events",
        path: "AbstractEventLoop.slow_callback_duration",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "a documented tuning knob only ever compared against an elapsed time",
    },
    Correction {
        module: "unittest.mock",
        path: "ThreadingMixin.DEFAULT_TIMEOUT",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "the docs tell users to reassign it; the value only reaches threading.Event.wait",
    },
    Correction {
        module: "logging.handlers",
        path: "SocketHandler.retryStart",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "a tunable used only in arithmetic against retryPeriod",
    },
    Correction {
        module: "logging.handlers",
        path: "SocketHandler.retryFactor",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as SocketHandler.retryStart, used only as a multiplier",
    },
    Correction {
        module: "logging.handlers",
        path: "SocketHandler.retryMax",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as SocketHandler.retryStart, used only in a min() against retryPeriod",
    },
    // ---- an attribute that stores a constructor argument verbatim ----
    Correction {
        module: "ftplib",
        path: "FTP.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "__init__ does `self.timeout = timeout`, and the parameter accepts an int",
    },
    Correction {
        module: "smtplib",
        path: "SMTP.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "__init__ stores the argument verbatim: SMTP(timeout=5).timeout is the int 5",
    },
    Correction {
        module: "http.client",
        path: "HTTPConnection.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "__init__ stores the argument verbatim, and it is writable before .connect()",
    },
    Correction {
        module: "logging.handlers",
        path: "SysLogHandler.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "stores the __init__ argument verbatim",
    },
    Correction {
        module: "logging.handlers",
        path: "SMTPHandler.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "stores the __init__ argument verbatim before handing it to smtplib.SMTP",
    },
    Correction {
        module: "subprocess",
        path: "TimeoutExpired.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "raised with the caller's original timeout: TimeoutExpired(args, 5).timeout is the int 5",
    },
    Correction {
        module: "threading",
        path: "Timer.interval",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "__init__ stores the argument verbatim before passing it to Event.wait",
    },
    Correction {
        module: "urllib.request",
        path: "Request.timeout",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "OpenerDirector.open assigns it from the caller's timeout argument",
    },
    Correction {
        module: "sched",
        path: "Event.time",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "enterabs stores its `time` argument verbatim, and the module documents an integer clock",
    },
    Correction {
        module: "sched",
        path: "scheduler.timefunc",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "holds the constructor's timefunc, which already accepts an int-returning clock",
    },
    // ---- a field constructed with an int ----
    Correction {
        module: "pstats",
        path: "FunctionProfile.percall_tottime",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "get_stats_profile writes `-1 if nc == 0 else float(...)`, so the int -1 is a stored value",
    },
    Correction {
        module: "pstats",
        path: "FunctionProfile.percall_cumtime",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as FunctionProfile.percall_tottime, but `-1 if cc == 0`",
    },
    Correction {
        module: "pstats",
        path: "StatsProfile.total_tt",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "an empty function list returns StatsProfile(0, {}), storing the int 0",
    },
    // ---- a TypedDict that is read back as well as written ----
    Correction {
        module: "tkinter",
        path: "_GridIndexInfo.minsize",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "grid_columnconfigure takes an int pixel count, and _gridconvvalue returns tk.getint for a value without a '.'",
    },
    Correction {
        module: "tkinter",
        path: "_GridIndexInfo.pad",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as _GridIndexInfo.minsize",
    },
    Correction {
        module: "tkinter.ttk",
        path: "_ElementCreateImageKwargs.height",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "types the keyword arguments of Style.element_create and is never returned, so a pixel count is an int",
    },
    Correction {
        module: "tkinter.ttk",
        path: "_ElementCreateImageKwargs.width",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as _ElementCreateImageKwargs.height",
    },
    Correction {
        module: "tkinter.ttk",
        path: "_ElementCreateVsapiKwargsSize.width",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as _ElementCreateImageKwargs.width, for the vsapi element form",
    },
    Correction {
        module: "tkinter.ttk",
        path: "_ElementCreateVsapiKwargsSize.height",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "as _ElementCreateImageKwargs.height, for the vsapi element form",
    },
    Correction {
        module: "turtle",
        path: "_PenState.stretchfactor",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "pen() stores the supplied value verbatim, so pen(stretchfactor=(2, 2)) round-trips as ints",
    },
    Correction {
        module: "turtle",
        path: "_PenState.shearfactor",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "pen() does self._shearfactor = p[\"shearfactor\"] with no coercion",
    },
    Correction {
        module: "turtle",
        path: "_PenState.tilt",
        scope: Scope::Declaration,
        fix: Fix::Accepts,
        why: "pen() does self._tilt = p[\"tilt\"] with no coercion, so pen(tilt=0) round-trips as an int",
    },
    // ---- an alias the whole-tree scan could not clear, being read back too ----
    Correction {
        module: "turtle",
        path: "Color",
        scope: Scope::Alias,
        fix: Fix::Accepts,
        why: "under the default colormode(255) an integer triple such as (255, 0, 0) is the normal argument",
    },
    Correction {
        module: "turtle",
        path: "PolygonCoords",
        scope: Scope::Alias,
        fix: Fix::Accepts,
        why: "register_shape takes integer vertex tuples, and _getshapepoly returns the registered polygon unchanged",
    },
    Correction {
        module: "tkinter.ttk",
        path: "Padding",
        scope: Scope::Alias,
        fix: Fix::Accepts,
        why: "used only for the padding/border/margin arguments, where a pixel count is an int",
    },
    Correction {
        module: "timeit",
        path: "_Timer",
        scope: Scope::Alias,
        fix: Fix::Accepts,
        why: "timeit only subtracts two readings, so an int-returning clock is a legitimate timer",
    },
    Correction {
        module: "unittest.result",
        path: "_DurationsType",
        scope: Scope::Alias,
        fix: Fix::Accepts,
        why: "addDuration appends its elapsed argument unchanged, and an int elapsed is accepted",
    },
    // ---- a structseq whose sequence is not what its named members are ----
    Correction {
        module: "os",
        path: "stat_result",
        scope: Scope::Bases,
        fix: Fix::Rewrite {
            from: "tuple[int, int, int, int, int, int, int, float, float, float]",
            to: "tuple[int, int, int, int, int, int, int, int, int, int]",
        },
        why: "the sequence portion is ten integers — os.stat(f)[7] is an int while os.stat(f).st_atime is a float, and the docs say the tuple interface is always integers",
    },
    Correction {
        module: "os",
        path: "stat_result",
        scope: Scope::Bases,
        fix: Fix::Rewrite {
            from: "structseq[float]",
            to: "structseq[int]",
        },
        why: "the element type of that all-integer sequence",
    },
    Correction {
        module: "sys",
        path: "_float_info",
        scope: Scope::Bases,
        fix: Fix::Rewrite {
            from: "structseq[float]",
            to: "structseq[int | float]",
        },
        why: "eight of the eleven fields are plain ints (max_exp, dig, radix, ...)",
    },
    Correction {
        module: "resource",
        path: "struct_rusage",
        scope: Scope::Bases,
        fix: Fix::Rewrite {
            from: "structseq[float]",
            to: "structseq[int | float]",
        },
        why: "two C doubles (ru_utime, ru_stime) followed by fourteen C longs",
    },
    // ---- an invariant element serving a writable value ----
    Correction {
        module: "ctypes",
        path: "c_float",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "_SimpleCData's Element is invariant and types the writable `value` as well as the initializer, which converts via PyFloat_AsDouble",
    },
    Correction {
        module: "ctypes",
        path: "c_double",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "the same invariant Element as ctypes.c_float",
    },
    Correction {
        module: "ctypes",
        path: "c_longdouble",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "the same invariant Element as ctypes.c_float",
    },
    Correction {
        module: "ctypes",
        path: "c_float_complex",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "as c_float, converting via PyComplex_AsCComplex",
    },
    Correction {
        module: "ctypes",
        path: "c_double_complex",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "the same invariant Element as ctypes.c_float_complex",
    },
    Correction {
        module: "ctypes",
        path: "c_longdouble_complex",
        scope: Scope::Bases,
        fix: Fix::Accepts,
        why: "the same invariant Element as ctypes.c_float_complex",
    },
    // ---- upstream wrote `complex` only to mean "an int is fine" ----
    //
    // reprlib's own `Repr.repr1` types this parameter `int`, and pydoc's
    // overrides only widened it so the override stayed compatible under the
    // typing spec, where `complex` reads as `int | float | complex`. it is a
    // recursion depth: it is compared with `level <= 0` and decremented, so a
    // `complex` raises TypeError. translating the idiom faithfully would keep a
    // type no value can have; `int` is what the parameter has always meant
    Correction {
        module: "pydoc",
        path: "HTMLRepr.repr1",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "a reprlib recursion depth, compared and decremented — reprlib.Repr.repr1 types it `int`",
    },
    Correction {
        module: "pydoc",
        path: "HTMLRepr.repr_string",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth, forwarded from repr1",
    },
    Correction {
        module: "pydoc",
        path: "HTMLRepr.repr_str",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth; it overrides reprlib.Repr.repr_str, whose parameter is `int`",
    },
    Correction {
        module: "pydoc",
        path: "HTMLRepr.repr_instance",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth, which reprlib compares with `level <= 0`",
    },
    Correction {
        module: "pydoc",
        path: "HTMLRepr.repr_unicode",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth, dispatched to from repr1 with the integer level",
    },
    Correction {
        module: "pydoc",
        path: "TextRepr.repr1",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth as HTMLRepr.repr1",
    },
    Correction {
        module: "pydoc",
        path: "TextRepr.repr_string",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth, forwarded from repr1",
    },
    Correction {
        module: "pydoc",
        path: "TextRepr.repr_str",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth; it overrides reprlib.Repr.repr_str, whose parameter is `int`",
    },
    Correction {
        module: "pydoc",
        path: "TextRepr.repr_instance",
        scope: Scope::Parameters,
        fix: Fix::Rewrite {
            from: "complex",
            to: "int",
        },
        why: "the same recursion depth, which reprlib compares with `level <= 0`",
    },
    // ---- an earlier overload already answers for the int ----
    Correction {
        module: "fractions",
        path: "Fraction.__add__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the `int | Fraction` overload above returns a Fraction, so Fraction + int is never a float",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__radd__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__, reflected",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__sub__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rsub__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__mul__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rmul__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__truediv__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rtruediv__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__floordiv__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the `int | Fraction` overload above returns an int",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rfloordiv__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__floordiv__, returning an int",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__mod__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rmod__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__divmod__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__rdivmod__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "the same overload shape as Fraction.__add__",
    },
    Correction {
        module: "fractions",
        path: "Fraction.__pow__",
        scope: Scope::Parameters,
        fix: Fix::Produces,
        why: "CPython special-cases an integral exponent and returns a Fraction: Fraction(1, 2) ** 2 is Fraction(1, 4). __rpow__ is deliberately absent — it has no int-only overload, and 2 ** Fraction(1, 2) really is a float",
    },
];

pub struct NumericPromotion {
    /// type aliases the whole-tree scan found standing for nothing but accepted
    /// inputs, so their right-hand sides are widened along with the parameters
    input_aliases: BTreeSet<String>,
}

impl NumericPromotion {
    pub(crate) fn new(input_aliases: BTreeSet<String>) -> Self {
        Self { input_aliases }
    }
}

impl Patch for NumericPromotion {
    fn name(&self) -> &'static str {
        "numeric-promotion"
    }

    fn target_symbols(&self) -> &'static [&'static str] {
        &["builtins.float", "builtins.complex"]
    }

    fn rewrite(&self, module_path: &Path, parsed: &Parsed<ModModule>, source: &str) -> Vec<Edit> {
        let module = crate::module_qualname(module_path);
        let mut rewriter = Rewriter {
            // `builtins` is where `float` and `complex` are *defined*, so the
            // class statements there are the builtins themselves rather than a
            // shadowing binding
            shadowed: shadowed_names(parsed, module.as_deref() == Some("builtins")),
            input_aliases: &self.input_aliases,
            module,
            enclosing: Vec::new(),
            seen: Vec::new(),
            source,
            edits: Vec::new(),
        };
        rewriter.visit_body(&parsed.syntax().body);
        rewriter.edits
    }
}

/// scan the whole stub tree for type aliases that stand for an accepted input
///
/// an alias is a name for a type, not a position, so whether the special case
/// applies to its right-hand side depends on where the alias is *used* — and a
/// use can be in any stub, which the per-file [`Patch`] contract
/// cannot see. so the decision is taken here, once, over the whole tree: an
/// alias qualifies when its right-hand side mentions `float` or `complex`, some
/// parameter annotation names it, and nothing that could be a use of *this*
/// alias reads a value back out of it
///
/// a stub that binds the identifier itself — declaring it, or importing it from
/// somewhere — is talking about its own thing, so its uses say nothing about
/// this alias. that is what keeps `statistics.Number` apart from the unrelated
/// `numbers.Number`. a stub that mentions the identifier while binding nothing
/// of that name might be reaching this one through a re-export, so it counts
///
/// a mention inside another alias counts against it. that is conservative — the
/// outer alias may itself be input-only — but it keeps the rule to a single pass
/// and errs towards leaving a type exact
pub(crate) fn scan(root: &Path) -> BTreeSet<String> {
    let mut stubs: Vec<StubUses> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_none_or(|extension| extension != "byi") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let parsed = parse_unchecked_source(&source, PySourceType::BasedPythonStub);
        let mut uses = StubUses::default();
        let mut scanner = UseScanner {
            uses: &mut uses,
            at_module_level: true,
        };
        scanner.visit_body(&parsed.syntax().body);
        stubs.push(uses);
    }

    // the rewrite is driven by identifier alone, so a name is only cleared when
    // every candidate spelled that way is
    let candidates: BTreeSet<&String> = stubs.iter().flat_map(|stub| &stub.candidates).collect();
    candidates
        .into_iter()
        .filter(|name| {
            let could_be_this_alias =
                |stub: &&StubUses| stub.candidates.contains(*name) || !stub.bound.contains(*name);
            let relevant: Vec<&StubUses> = stubs.iter().filter(could_be_this_alias).collect();
            relevant
                .iter()
                .all(|stub| !stub.used_elsewhere.contains(*name))
                && relevant
                    .iter()
                    .any(|stub| stub.used_as_parameter.contains(*name))
        })
        .cloned()
        .collect()
}

/// module-level bindings that give `float` or `complex` a meaning other than the
/// builtin class. no stdlib stub does this today, but a stub that did would have
/// its own `float` widened into a union of the builtins, which is nonsense
fn shadowed_names(parsed: &Parsed<ModModule>, is_builtins: bool) -> Vec<String> {
    let mut shadowed = Vec::new();
    let mut note = |name: &str| {
        if matches!(name, "float" | "complex") && !shadowed.iter().any(|n| n == name) {
            shadowed.push(name.to_string());
        }
    };
    for stmt in &parsed.syntax().body {
        match stmt {
            Stmt::ClassDef(class) if !is_builtins => note(class.name.as_str()),
            Stmt::FunctionDef(function) => note(function.name.as_str()),
            Stmt::TypeAlias(alias) => {
                if let Expr::Name(name) = alias.name.as_ref() {
                    note(name.id.as_str());
                }
            }
            Stmt::AnnAssign(assign) => {
                if let Expr::Name(name) = assign.target.as_ref() {
                    note(name.id.as_str());
                }
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    if let Expr::Name(name) = target {
                        note(name.id.as_str());
                    }
                }
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    note(alias.asname.as_ref().map_or(&alias.name, |a| a).as_str());
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    note(alias.asname.as_ref().map_or(&alias.name, |a| a).as_str());
                }
            }
            _ => {}
        }
    }
    shadowed
}

/// one declaration whose direction the walker cannot read off the syntax
///
/// direction is a property of the position, and the walker gets that right
/// almost everywhere. what it cannot know is what CPython does *inside*: that
/// `colorsys.hls_to_rgb` hands an argument straight back, that `server.timeout`
/// is a knob the caller sets, that `Fraction + int` is a `Fraction` and never a
/// `float`. each entry here is one such fact, and each was checked against a
/// running interpreter rather than reasoned about
struct Correction {
    /// dotted module name, e.g. `"socketserver"`
    module: &'static str,
    /// dotted path to the declaration within it, e.g. `"BaseServer.timeout"`.
    /// a bare name is module level, and a path is matched in every branch of a
    /// `sys.version_info` guard, so a declaration written twice is corrected twice
    path: &'static str,
    /// which of the declaration's annotations this is about
    scope: Scope,
    fix: Fix,
    /// what makes it true. a fact about CPython, so it has to be defensible —
    /// the rewrite does not consult it, but nothing here may be changed without
    /// one, which `every_correction_says_why` enforces
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the tests that keep the table honest")
    )]
    why: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// the return annotation, across every overload of the name
    Return,
    /// the annotation of a variable, attribute, `let`, field or `TypedDict` key
    Declaration,
    /// the class's base list
    Bases,
    /// a type alias's right-hand side
    Alias,
    /// every parameter, across every overload of the name
    Parameters,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fix {
    /// widen it after all: an `int` genuinely belongs here
    Accepts,
    /// leave it exact after all: an `int` does not
    Produces,
    /// neither — some of it accepts and some of it produces, so write the answer
    /// out. only applied while the annotation still reads `from`, so an upstream
    /// change makes this a no-op to be re-checked rather than a silent overwrite
    Rewrite {
        from: &'static str,
        to: &'static str,
    },
}

/// which side of the module boundary a position sits on
///
/// the special case is about what a position **accepts**, and "accepts" is a
/// question about direction, not about which syntactic slot the annotation is
/// written in. a parameter accepts; a return produces. but a parameter can
/// itself be a callable, and *that* callable's parameters are values the library
/// hands to the caller's function — so they produce, and widening them would
/// demand the caller's function accept an `int` rather than allowing it to:
///
/// ```text
/// def config(xscrollcommand: (float, float) -> object)   # tk only ever passes floats
/// ```
///
/// each callable parameter list flips the direction, exactly as contravariance
/// does, so a callable returned *from* a function has parameters that accept
/// again — `statistics.kde` hands back an estimator you may call with an `int`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// a value flows in from the caller, so the typing spec's `int` belongs here
    Accepts,
    /// a value flows out to the caller, so the annotation states what it is
    Produces,
}

impl Direction {
    const fn flipped(self) -> Self {
        match self {
            Direction::Accepts => Direction::Produces,
            Direction::Produces => Direction::Accepts,
        }
    }
}

/// whether a bare `int | float` can stand where the expression being replaced
/// stands, or whether it has to be parenthesised
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// the whole annotation, an arm of a `|` union, or an element of something
    /// already delimited (a subscript slice, a tuple type, a callable's
    /// parameter list) — all places a bare union reads correctly
    Bare,
    /// anywhere a bare union would re-associate: an arm of an `&` intersection,
    /// the operand of `not` or `?`, the return of an arrow callable
    Parenthesised,
}

struct Rewriter<'a> {
    shadowed: Vec<String>,
    input_aliases: &'a BTreeSet<String>,
    /// dotted module name, for looking this stub's corrections up
    module: Option<String>,
    /// the enclosing class names, so a declaration can be named by its path
    enclosing: Vec<String>,
    /// which corrections actually matched, for the staleness test
    seen: Vec<&'static Correction>,
    source: &'a str,
    edits: Vec<Edit>,
}

impl Rewriter<'_> {
    /// the corrections recorded for `<enclosing…>.<name>` at `scope`
    fn corrections(&self, name: &str, scope: Scope) -> Vec<&'static Correction> {
        let Some(module) = self.module.as_deref() else {
            return Vec::new();
        };
        let mut path = self.enclosing.join(".");
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(name);
        CORRECTIONS
            .iter()
            .filter(|c| c.module == module && c.path == path && c.scope == scope)
            .collect()
    }

    /// walk `annotation` under whatever the corrections for this position say,
    /// falling back to `default` when there are none. a `Rewrite` replaces the
    /// annotation outright and stops the walk
    fn visit_corrected(&mut self, annotation: &Expr, name: &str, scope: Scope, default: Direction) {
        let corrections = self.corrections(name, scope);
        self.seen.extend(corrections.iter().copied());
        let text = normalised(self.text_of(annotation));
        for correction in &corrections {
            if let Fix::Rewrite { from, to } = correction.fix
                && normalised(from) == text
            {
                self.replace(annotation, to.to_string());
                return;
            }
        }
        let direction = corrections
            .iter()
            .find_map(|correction| match correction.fix {
                Fix::Accepts => Some(Direction::Accepts),
                Fix::Produces => Some(Direction::Produces),
                Fix::Rewrite { .. } => None,
            })
            .unwrap_or(default);
        self.visit_type_expr(annotation, Slot::Bare, direction);
    }

    fn visit_body(&mut self, body: &[Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                self.visit_type_params(function.type_params.as_deref());
                let name = function.name.to_string();
                self.visit_parameters(&function.parameters, &name);
                // a return produces, but a callable it hands back has parameters
                // that accept again, so the return is walked rather than skipped
                if let Some(returns) = &function.returns {
                    self.visit_corrected(returns, &name, Scope::Return, Direction::Produces);
                }
                self.visit_body(&function.body);
            }
            Stmt::ClassDef(class) => {
                self.visit_type_params(class.type_params.as_deref());
                let name = class.name.to_string();
                for base in class.bases() {
                    self.visit_corrected(base, &name, Scope::Bases, Direction::Produces);
                }
                self.enclosing.push(name);
                self.visit_body(&class.body);
                self.enclosing.pop();
            }
            // `type X = …`, and the `X: TypeAlias = …` form a fresh sync still
            // sees, are widened only for the aliases the whole-tree scan cleared
            Stmt::TypeAlias(alias) => {
                if let Expr::Name(name) = alias.name.as_ref() {
                    let default = if self.input_aliases.contains(name.id.as_str()) {
                        Direction::Accepts
                    } else {
                        Direction::Produces
                    };
                    let name = name.id.to_string();
                    self.visit_corrected(&alias.value, &name, Scope::Alias, default);
                }
            }
            Stmt::AnnAssign(assign) => {
                match (assign.target.as_ref(), assign.value.as_ref()) {
                    (Expr::Name(name), Some(value))
                        if is_type_alias_annotation(&assign.annotation) =>
                    {
                        let default = if self.input_aliases.contains(name.id.as_str()) {
                            Direction::Accepts
                        } else {
                            Direction::Produces
                        };
                        let name = name.id.to_string();
                        self.visit_corrected(value, &name, Scope::Alias, default);
                    }
                    // an attribute, a `let`, a module constant: reading it produces
                    (Expr::Name(name), _) => {
                        let name = name.id.to_string();
                        self.visit_corrected(
                            &assign.annotation,
                            &name,
                            Scope::Declaration,
                            Direction::Produces,
                        );
                    }
                    _ => self.visit_type_expr(&assign.annotation, Slot::Bare, Direction::Produces),
                }
            }
            // `_T = TypeVar("_T", float, Decimal)` — the declaration form a
            // fresh sync sees for the type variables the pep 695 conversion
            // cannot lift into a header
            Stmt::Assign(assign) => {
                if let Some(call) = assign.value.as_call_expr()
                    && matches!(
                        call.func.as_name_expr().map(|name| name.id.as_str()),
                        Some("TypeVar" | "_TypeVar")
                    )
                {
                    let constraints: Vec<&Expr> = call.arguments.args.iter().skip(1).collect();
                    self.visit_alternatives(&constraints, Direction::Accepts);
                    if let Some(bound) = call.arguments.find_keyword("bound") {
                        self.visit_type_expr(&bound.value, Slot::Bare, Direction::Accepts);
                    }
                }
            }
            // `if sys.version_info >= …:` guards wrap a good deal of the stdlib,
            // and `try: import … except ImportError:` a little more
            Stmt::If(stmt) => {
                self.visit_body(&stmt.body);
                for clause in &stmt.elif_else_clauses {
                    self.visit_body(&clause.body);
                }
            }
            Stmt::Try(stmt) => {
                self.visit_body(&stmt.body);
                for handler in &stmt.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.visit_body(&handler.body);
                }
                self.visit_body(&stmt.orelse);
                self.visit_body(&stmt.finalbody);
            }
            _ => {}
        }
    }

    fn visit_parameters(&mut self, parameters: &Parameters, function: &str) {
        for parameter in parameters {
            if let Some(annotation) = parameter.annotation() {
                self.visit_corrected(annotation, function, Scope::Parameters, Direction::Accepts);
            }
        }
    }

    /// a type variable is solved from the arguments a call supplies, so its
    /// bound and its constraints are input positions. its default is not: that
    /// is what the parameter *becomes*, and nothing is accepted into it
    fn visit_type_params(&mut self, type_params: Option<&TypeParams>) {
        for type_param in type_params.into_iter().flat_map(|params| params.iter()) {
            let Some(bound) = type_param_bound(type_param) else {
                continue;
            };
            match (type_param, bound) {
                // `T in (int, float, str)` — a set of alternatives the parameter
                // ranges over, each its own type position. an `int` listed beside
                // the `float` already accepts an `int`, exactly as a union arm does
                (TypeParam::TypeVar(type_var), Expr::Tuple(tuple))
                    if type_var.is_type_mapping && tuple.parenthesized =>
                {
                    let members: Vec<&Expr> = tuple.elts.iter().collect();
                    self.visit_alternatives(&members, Direction::Accepts);
                }
                // `T: float` — an upper bound, which an argument has to fit under
                _ => self.visit_type_expr(bound, Slot::Bare, Direction::Accepts),
            }
        }
    }

    fn visit_type_expr(&mut self, expr: &Expr, slot: Slot, direction: Direction) {
        match expr {
            Expr::Name(name) => {
                if direction == Direction::Accepts
                    && let Some(replacement) = self.widened(name.id.as_str(), &[], slot)
                {
                    self.replace(expr, replacement);
                }
            }
            Expr::BinOp(binop) if binop.op == Operator::BitOr => {
                self.visit_alternatives(&union_arms(expr), direction);
            }
            // an intersection arm binds tighter than `|`, so a union put there
            // has to be parenthesised
            Expr::BinOp(binop) if binop.op == Operator::BitAnd => {
                self.visit_type_expr(&binop.left, Slot::Parenthesised, direction);
                self.visit_type_expr(&binop.right, Slot::Parenthesised, direction);
            }
            // `not T` and `T?`
            Expr::UnaryOp(unary) if matches!(unary.op, UnaryOp::Not | UnaryOp::Optional) => {
                self.visit_type_expr(&unary.operand, Slot::Parenthesised, direction);
            }
            Expr::Subscript(subscript) => {
                if name_is(&subscript.value, "Literal") {
                    // the slice holds value tokens, not type expressions
                    return;
                }
                // `type[array[float]]` names a class, not a value of it. the
                // special case is about values, and `Element` here is often a
                // constrained parameter for which `int | float` is not even a
                // legal specialization
                if name_is(&subscript.value, "type") {
                    return;
                }
                if name_is(&subscript.value, "Annotated") {
                    // only the first slice element is a type position; the rest
                    // is arbitrary metadata
                    if let Some(first) = slice_elements(&subscript.slice).first() {
                        self.visit_type_expr(first, Slot::Bare, direction);
                    }
                    return;
                }
                for element in slice_elements(&subscript.slice) {
                    self.visit_type_expr(element, Slot::Bare, direction);
                }
            }
            // `(int, str)` — a tuple type, whose parentheses delimit each element
            Expr::Tuple(tuple) => {
                for element in &tuple.elts {
                    self.visit_type_expr(element, Slot::Bare, direction);
                }
            }
            // `Callable[[int, str], bool]` — the legacy spelling a fresh sync
            // sees before `arrow_callable` rewrites it. the brackets delimit
            // each parameter, and being a callable's parameters they flip
            Expr::List(list) => {
                for element in &list.elts {
                    self.visit_type_expr(element, Slot::Bare, direction.flipped());
                }
            }
            // `(*: int)` — the unpacked element of a homogeneous tuple type
            Expr::Starred(starred) => {
                self.visit_type_expr(&starred.value, Slot::Bare, direction);
            }
            // `(int) -> str`. the parameters are inside the callable's own
            // parentheses; the return is not, and `->` binds looser than `|`
            Expr::CallableType(callable) => {
                if let Some(receiver) = &callable.receiver {
                    self.visit_type_expr(receiver, Slot::Parenthesised, direction.flipped());
                }
                for arg in &callable.args {
                    self.visit_type_expr(arg, Slot::Bare, direction.flipped());
                }
                self.visit_type_expr(&callable.returns, Slot::Parenthesised, direction);
            }
            _ => {}
        }
    }

    /// rewrite a set of alternatives — the arms of a union, or the members of a
    /// type mapping — where reaching any one of them is enough. walking them
    /// left to right means `float | complex` grows a single `int`, on the `float`
    fn visit_alternatives(&mut self, alternatives: &[&Expr], direction: Direction) {
        let mut reachable: Vec<&str> = alternatives
            .iter()
            .filter_map(|alternative| alternative.as_name_expr().map(|name| name.id.as_str()))
            .collect();
        let texts: Vec<String> = alternatives
            .iter()
            .map(|alternative| normalised(self.text_of(alternative)))
            .collect();
        for (index, alternative) in alternatives.iter().enumerate() {
            // a compound arm whose all-`int` reading is already spelled out
            // beside it is reachable as it stands. `list[int] | list[float]`
            // covers both; rewriting it to `list[int] | list[int | float]` only
            // takes `list[float]` away, because `list` is invariant
            if !matches!(alternative, Expr::Name(_))
                && let Some(all_int) = self.all_int_reading(alternative)
                && texts
                    .iter()
                    .enumerate()
                    .any(|(other, text)| other != index && *text == all_int)
            {
                continue;
            }
            if let Expr::Name(name) = alternative {
                if direction == Direction::Accepts
                    && let Some(replacement) =
                        self.widened(name.id.as_str(), &reachable, Slot::Bare)
                {
                    self.replace(alternative, replacement);
                    reachable.extend(TOWER);
                }
            } else {
                self.visit_type_expr(alternative, Slot::Bare, direction);
            }
        }
    }

    /// how this arm would read with every promoted numeric in it replaced by
    /// `int`, whitespace-normalised for comparison against its siblings
    fn all_int_reading(&self, expr: &Expr) -> Option<String> {
        let mut leaves = Vec::new();
        collect_promoted_leaves(expr, &mut leaves);
        if leaves.is_empty() {
            return None;
        }
        let base = expr.range().start().to_usize();
        let text = self.text_of(expr);
        let mut out = String::new();
        let mut cursor = 0;
        for leaf in leaves {
            let start = leaf.start().to_usize() - base;
            let end = leaf.end().to_usize() - base;
            out.push_str(&text[cursor..start]);
            out.push_str("int");
            cursor = end;
        }
        out.push_str(&text[cursor..]);
        Some(normalised(&out))
    }

    fn text_of(&self, expr: &Expr) -> &str {
        let range = expr.range();
        &self.source[range.start().to_usize()..range.end().to_usize()]
    }

    /// the union `name` stands for, given what is already `reachable` alongside
    /// it, or `None` when `name` is not a promoted numeric or the union is
    /// already there
    fn widened(&self, name: &str, reachable: &[&str], slot: Slot) -> Option<String> {
        if self.shadowed.iter().any(|shadowed| shadowed == name) {
            return None;
        }
        let width = TOWER.iter().position(|entry| *entry == name)?;
        // `int` is not promoted: it is the bottom of the tower, and an `int`
        // annotation means exactly `int`
        if width == 0 {
            return None;
        }
        let missing: Vec<&str> = TOWER[..width]
            .iter()
            .copied()
            .filter(|arm| !reachable.contains(arm))
            .collect();
        if missing.is_empty() {
            return None;
        }
        let union = format!("{} | {name}", missing.join(" | "));
        Some(match slot {
            Slot::Bare => union,
            Slot::Parenthesised => format!("({union})"),
        })
    }

    fn replace(&mut self, expr: &Expr, replacement: String) {
        let range = expr.range();
        // the parser hands back a zero-width node for the receiver a method's
        // implicit `self` was elided from; there is nothing to rewrite there
        if range.is_empty() || range.end().to_usize() > self.source.len() {
            return;
        }
        self.edits.push(Edit {
            start: range.start().to_usize(),
            end: range.end().to_usize(),
            replacement,
        });
    }
}

/// what one stub contributes to the whole-tree alias decision
#[derive(Default)]
struct StubUses {
    /// module-level type aliases whose right-hand side mentions a promoted
    /// numeric, so widening them would change something
    candidates: BTreeSet<String>,
    /// every identifier this stub binds — declared, assigned, or imported. a
    /// mention of one of these is a mention of *this* stub's thing
    bound: BTreeSet<String>,
    used_as_parameter: BTreeSet<String>,
    used_elsewhere: BTreeSet<String>,
}

/// collects one stub's contribution to the whole-tree alias decision
struct UseScanner<'a> {
    uses: &'a mut StubUses,
    at_module_level: bool,
}

impl UseScanner<'_> {
    fn visit_body(&mut self, body: &[Stmt]) {
        for stmt in body {
            self.visit_stmt(stmt);
        }
    }

    fn visit_nested_body(&mut self, body: &[Stmt]) {
        let outer = std::mem::replace(&mut self.at_module_level, false);
        self.visit_body(body);
        self.at_module_level = outer;
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::FunctionDef(function) => {
                self.uses.bound.insert(function.name.to_string());
                for parameter in &function.parameters {
                    if let Some(annotation) = parameter.annotation() {
                        collect_names(annotation, &mut self.uses.used_as_parameter);
                    }
                }
                if let Some(returns) = &function.returns {
                    collect_names(returns, &mut self.uses.used_elsewhere);
                }
                for type_param in function.type_params.iter().flat_map(|params| params.iter()) {
                    self.visit_type_param_uses(type_param);
                }
                self.visit_nested_body(&function.body);
            }
            Stmt::ClassDef(class) => {
                self.uses.bound.insert(class.name.to_string());
                for base in class.bases() {
                    collect_names(base, &mut self.uses.used_elsewhere);
                }
                for type_param in class.type_params.iter().flat_map(|params| params.iter()) {
                    self.visit_type_param_uses(type_param);
                }
                self.visit_nested_body(&class.body);
            }
            Stmt::TypeAlias(alias) => {
                if let Expr::Name(name) = alias.name.as_ref() {
                    self.uses.bound.insert(name.id.to_string());
                    if self.at_module_level && mentions_promoted_numeric(&alias.value) {
                        self.uses.candidates.insert(name.id.to_string());
                    }
                }
                collect_names(&alias.value, &mut self.uses.used_elsewhere);
            }
            Stmt::AnnAssign(assign) => {
                if let Expr::Name(name) = assign.target.as_ref() {
                    self.uses.bound.insert(name.id.to_string());
                }
                match (
                    assign.value.as_ref(),
                    is_type_alias_annotation(&assign.annotation),
                ) {
                    (Some(value), true) => {
                        if let Expr::Name(name) = assign.target.as_ref()
                            && self.at_module_level
                            && mentions_promoted_numeric(value)
                        {
                            self.uses.candidates.insert(name.id.to_string());
                        }
                        collect_names(value, &mut self.uses.used_elsewhere);
                    }
                    _ => collect_names(&assign.annotation, &mut self.uses.used_elsewhere),
                }
            }
            Stmt::Assign(assign) => {
                for target in &assign.targets {
                    if let Expr::Name(name) = target {
                        self.uses.bound.insert(name.id.to_string());
                    }
                }
                // a legacy `TypeVar(…)` declaration reaches its constraints and
                // bound from the arguments a call supplies, like a parameter
                if let Some(call) = assign.value.as_call_expr()
                    && matches!(
                        call.func.as_name_expr().map(|name| name.id.as_str()),
                        Some("TypeVar" | "_TypeVar")
                    )
                {
                    for constraint in call.arguments.args.iter().skip(1) {
                        collect_names(constraint, &mut self.uses.used_as_parameter);
                    }
                    if let Some(bound) = call.arguments.find_keyword("bound") {
                        collect_names(&bound.value, &mut self.uses.used_as_parameter);
                    }
                }
            }
            Stmt::Import(import) => {
                for alias in &import.names {
                    self.uses
                        .bound
                        .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
                }
            }
            Stmt::ImportFrom(import) => {
                for alias in &import.names {
                    self.uses
                        .bound
                        .insert(alias.asname.as_ref().unwrap_or(&alias.name).to_string());
                }
            }
            Stmt::If(stmt) => {
                self.visit_body(&stmt.body);
                for clause in &stmt.elif_else_clauses {
                    self.visit_body(&clause.body);
                }
            }
            Stmt::Try(stmt) => {
                self.visit_body(&stmt.body);
                for handler in &stmt.handlers {
                    let ruff_python_ast::ExceptHandler::ExceptHandler(handler) = handler;
                    self.visit_body(&handler.body);
                }
                self.visit_body(&stmt.orelse);
                self.visit_body(&stmt.finalbody);
            }
            _ => {}
        }
    }

    fn visit_type_param_uses(&mut self, type_param: &TypeParam) {
        self.uses.bound.insert(type_param.name().to_string());
        if let Some(bound) = type_param_bound(type_param) {
            collect_names(bound, &mut self.uses.used_as_parameter);
        }
        if let Some(default) = type_param.default() {
            collect_names(default, &mut self.uses.used_elsewhere);
        }
    }
}

/// every identifier in a type expression, so a use of an alias is seen wherever
/// it is nested
fn collect_names(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Name(name) => {
            out.insert(name.id.to_string());
        }
        Expr::BinOp(binop) => {
            collect_names(&binop.left, out);
            collect_names(&binop.right, out);
        }
        Expr::UnaryOp(unary) => collect_names(&unary.operand, out),
        Expr::Subscript(subscript) => {
            collect_names(&subscript.value, out);
            collect_names(&subscript.slice, out);
        }
        Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                collect_names(element, out);
            }
        }
        Expr::List(list) => {
            for element in &list.elts {
                collect_names(element, out);
            }
        }
        Expr::Starred(starred) => collect_names(&starred.value, out),
        // `unittest.result._DurationsType` reaches an alias without ever writing
        // its name unqualified, so the attribute counts as a mention too
        Expr::Attribute(attribute) => {
            out.insert(attribute.attr.to_string());
            collect_names(&attribute.value, out);
        }
        Expr::CallableType(callable) => {
            for arg in callable
                .receiver
                .iter()
                .map(std::convert::AsRef::as_ref)
                .chain(&callable.args)
            {
                collect_names(arg, out);
            }
            collect_names(&callable.returns, out);
        }
        _ => {}
    }
}

/// the upper bound (or, for a type mapping, the alternatives) a type parameter
/// was declared with
fn type_param_bound(type_param: &TypeParam) -> Option<&Expr> {
    match type_param {
        TypeParam::TypeVar(type_var) => type_var.bound.as_deref(),
        TypeParam::TypeVarTuple(pack) => pack.bound.as_deref(),
        TypeParam::ParamSpec(param_spec) => param_spec.bound.as_deref(),
    }
}

/// whether a type expression names `float` or `complex` anywhere inside it
fn mentions_promoted_numeric(expr: &Expr) -> bool {
    let mut names = BTreeSet::new();
    collect_names(expr, &mut names);
    names.contains("float") || names.contains("complex")
}

/// `X: TypeAlias = …`, the form a fresh reverse-transpile still produces
fn is_type_alias_annotation(annotation: &Expr) -> bool {
    match annotation {
        Expr::Name(name) => name.id.as_str() == "TypeAlias",
        Expr::Attribute(attribute) => attribute.attr.as_str() == "TypeAlias",
        Expr::StringLiteral(string) => string.value.to_str() == "TypeAlias",
        _ => false,
    }
}

/// the ranges of every bare `float` / `complex` name inside a type expression
fn collect_promoted_leaves(expr: &Expr, out: &mut Vec<ruff_text_size::TextRange>) {
    match expr {
        Expr::Name(name) if matches!(name.id.as_str(), "float" | "complex") => {
            out.push(name.range());
        }
        Expr::BinOp(binop) => {
            collect_promoted_leaves(&binop.left, out);
            collect_promoted_leaves(&binop.right, out);
        }
        Expr::UnaryOp(unary) => collect_promoted_leaves(&unary.operand, out),
        Expr::Subscript(subscript) => collect_promoted_leaves(&subscript.slice, out),
        Expr::Tuple(tuple) => {
            for element in &tuple.elts {
                collect_promoted_leaves(element, out);
            }
        }
        Expr::List(list) => {
            for element in &list.elts {
                collect_promoted_leaves(element, out);
            }
        }
        Expr::Starred(starred) => collect_promoted_leaves(&starred.value, out),
        Expr::CallableType(callable) => {
            for arg in &callable.args {
                collect_promoted_leaves(arg, out);
            }
            collect_promoted_leaves(&callable.returns, out);
        }
        _ => {}
    }
}

/// collapse whitespace so two spellings of the same type compare equal across a
/// line break the formatter put in
fn normalised(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// flatten a left-nested `A | B | C` into its arms
fn union_arms(expr: &Expr) -> Vec<&Expr> {
    let mut arms = Vec::new();
    collect_union_arms(expr, &mut arms);
    arms
}

fn collect_union_arms<'a>(expr: &'a Expr, arms: &mut Vec<&'a Expr>) {
    match expr {
        Expr::BinOp(binop) if binop.op == Operator::BitOr => {
            collect_union_arms(&binop.left, arms);
            collect_union_arms(&binop.right, arms);
        }
        _ => arms.push(expr),
    }
}

/// the type positions of a subscript slice: the elements of an unparenthesised
/// tuple (`dict[str, int]`), or the slice itself (`list[int]`)
fn slice_elements(slice: &Expr) -> Vec<&Expr> {
    match slice {
        Expr::Tuple(tuple) if !tuple.parenthesized => tuple.elts.iter().collect(),
        other => vec![other],
    }
}

fn name_is(expr: &Expr, name: &str) -> bool {
    expr.as_name_expr()
        .is_some_and(|expr| expr.id.as_str() == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ruff_python_ast::PySourceType;
    use ruff_python_parser::parse_unchecked_source;

    use crate::apply_edits;

    fn run(path: &str, src: &str) -> String {
        run_with_aliases(path, src, &[])
    }

    fn run_with_aliases(path: &str, src: &str, aliases: &[&str]) -> String {
        let parsed = parse_unchecked_source(src, PySourceType::BasedPythonStub);
        let patch = NumericPromotion::new(
            aliases
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
        );
        let edits = patch.rewrite(Path::new(path), &parsed, src);
        apply_edits(src, edits)
    }

    /// the whole-tree alias decision, run over a single stub
    fn scan_one(src: &str) -> BTreeSet<String> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("m.byi"), src).unwrap();
        scan(dir.path())
    }

    #[test]
    fn widens_parameters_and_leaves_outputs_alone() {
        let src = "\
def sleep(secs: float) -> float

class C:
    let real: float
    timeout: float
    def scale(self, by: float) -> float
    def unit(self) -> complex

pi: float
type Timer = () -> float
";
        let expected = "\
def sleep(secs: int | float) -> float

class C:
    let real: float
    timeout: float
    def scale(self, by: int | float) -> float
    def unit(self) -> complex

pi: float
type Timer = () -> float
";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn widens_complex_through_the_whole_tower() {
        let src = "def f(z: complex, w: complex | None)\n";
        let expected = "def f(z: int | float | complex, w: int | float | complex | None)\n";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn descends_into_interior_positions() {
        let src = "\
def f(
    xs: list[float],
    m: dict[str, float],
    pair: (float, float),
    cb: (float, str) -> float,
    opt: float?,
    both: A & float,
    tag: Annotated[float, 1],
    lit: Literal[1],
)
";
        let expected = "\
def f(
    xs: list[int | float],
    m: dict[str, int | float],
    pair: (int | float, int | float),
    cb: (float, str) -> (int | float),
    opt: (int | float)?,
    both: A & (int | float),
    tag: Annotated[int | float, 1],
    lit: Literal[1],
)
";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn adds_only_the_arms_a_union_is_missing() {
        let src = "def f(a: int | float, b: float | None, c: float | complex, d: str | complex)\n";
        let expected = "def f(a: int | float, b: int | float | None, c: int | float | complex, d: str | int | float | complex)\n";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn widens_a_constraint_list_that_cannot_reach_int() {
        let src = "def mean[NumberT in (float, Decimal)](data: Iterable[NumberT]) -> NumberT\n";
        let expected =
            "def mean[NumberT in (int | float, Decimal)](data: Iterable[NumberT]) -> NumberT\n";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn leaves_a_constraint_list_that_already_lists_int() {
        let src = "class array[in out Element in (int, float, str)]:\n    x: int\n";
        assert_eq!(run("m.byi", src), src);
    }

    #[test]
    fn widens_a_bound_but_not_a_default() {
        let src = "def f[T: float, U = float](x: T) -> U\n";
        let expected = "def f[T: int | float, U = float](x: T) -> U\n";
        assert_eq!(run("m.byi", src), expected);
    }

    /// a fresh sync runs this patch before the pep 695 conversion and before
    /// `arrow_callable`, so it has to recognise the legacy spellings too
    #[test]
    fn widens_the_legacy_forms_a_fresh_sync_produces() {
        let src = "\
_NumberT = TypeVar(\"_NumberT\", float, Decimal)
_ScaleT = TypeVar(\"_ScaleT\", bound=float)
_Handler: TypeAlias = Callable[[float], object]

def f(cb: Callable[[float, str], float], xs: Union[float, None])
";
        let expected = "\
_NumberT = TypeVar(\"_NumberT\", int | float, Decimal)
_ScaleT = TypeVar(\"_ScaleT\", bound=int | float)
_Handler: TypeAlias = Callable[[float], object]

def f(cb: Callable[[float, str], int | float], xs: Union[int | float, None])
";
        assert_eq!(run_with_aliases("m.byi", src, &["_Handler"]), expected);
    }

    /// a callable's parameters are values the library *hands* to the caller's
    /// function, so inside a parameter they produce rather than accept. widening
    /// them would demand the caller's function take an `int` — rejecting the
    /// obvious `def cb(a: float, b: float)` instead of admitting anything
    #[test]
    fn a_callback_parameter_inside_a_parameter_is_left_exact() {
        let src = "def config(xscrollcommand: str | ((float, float) -> object))\n";
        assert_eq!(run("m.byi", src), src);
    }

    /// the same flip, one level further: a callable a function *returns* is one
    /// the caller invokes, so its parameters accept again
    #[test]
    fn a_returned_callable_accepts_in_its_parameters() {
        let src = "def kde(data: list[float]) -> (float) -> float\n";
        let expected = "def kde(data: list[int | float]) -> (int | float) -> float\n";
        assert_eq!(run("m.byi", src), expected);
    }

    /// `list` is invariant, so `list[int] | list[float]` already covers both and
    /// `list[int | float]` would cover neither
    #[test]
    fn an_arm_whose_all_int_reading_sits_beside_it_is_left_alone() {
        let src = "def coords(xs: list[int] | list[float], ps: list[(int, int)] | list[(float, float)])\n";
        assert_eq!(run("m.byi", src), src);
    }

    /// `type[array[float]]` names the class. `Element` is constrained to
    /// `(int, float, str)`, so `array[int | float]` is not even a legal
    /// specialization — the widening produced a type nothing could match
    #[test]
    fn a_class_object_is_not_widened() {
        let src =
            "def __new__(cls: type[array[float]], initializer: Iterable[float]) -> array[float]\n";
        let expected = "def __new__(cls: type[array[float]], initializer: Iterable[int | float]) -> array[float]\n";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn is_idempotent() {
        let src = "\
def f(a: float, b: list[complex], c: A & float)
def mean[NumberT in (float, Decimal)](data: Iterable[NumberT]) -> NumberT
";
        let once = run("m.byi", src);
        assert_eq!(run("m.byi", &once), once);
    }

    #[test]
    fn skips_a_module_that_binds_its_own_float() {
        let src = "\
from x import float

def f(a: float)
";
        assert_eq!(run("m.byi", src), src);
    }

    #[test]
    fn widens_inside_the_builtins_definition() {
        let src = "\
class float:
    def __add__(self, value: float, /) -> float
";
        let expected = "\
class float:
    def __add__(self, value: int | float, /) -> float
";
        assert_eq!(run("builtins.byi", src), expected);
    }

    #[test]
    fn reaches_into_version_guards() {
        let src = "\
if sys.version_info >= (3, 13):
    def f(a: float)
else:
    def f(a: complex)
";
        let expected = "\
if sys.version_info >= (3, 13):
    def f(a: int | float)
else:
    def f(a: int | float | complex)
";
        assert_eq!(run("m.byi", src), expected);
    }

    #[test]
    fn widens_an_alias_the_scan_cleared() {
        let src = "\
type Accepted = float | str
type Produced = float | str

def f(a: Accepted) -> Produced
";
        let expected = "\
type Accepted = int | float | str
type Produced = float | str

def f(a: Accepted) -> Produced
";
        assert_eq!(run_with_aliases("m.byi", src, &["Accepted"]), expected);
    }

    #[test]
    fn scan_clears_only_parameter_only_aliases() {
        let cleared = scan_one(
            "\
type Accepted = float | str
type Produced = float | str
type Held = float
type Unused = float

def f(a: Accepted) -> Produced

class C:
    x: Held
",
        );
        assert_eq!(
            cleared,
            ["Accepted".to_string()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn scan_ignores_an_alias_with_no_promoted_numeric() {
        let cleared = scan_one("type Name = str\n\ndef f(a: Name)\n");
        assert!(cleared.is_empty());
    }

    /// every correction is a fact about one declaration that really exists. an
    /// upstream rename would otherwise turn one into a silent no-op, and the
    /// stub would quietly go back to being wrong
    #[test]
    fn corrections_all_apply() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../ty_vendored/vendor/typeshed/stdlib");
        assert!(root.is_dir(), "typeshed not found at {}", root.display());

        let mut unmatched: Vec<String> = Vec::new();
        for correction in CORRECTIONS {
            let relative = correction.module.replace('.', "/");
            let path = [
                format!("{relative}.byi"),
                format!("{relative}/__init__.byi"),
            ]
            .iter()
            .map(|candidate| root.join(candidate))
            .find(|candidate| candidate.is_file());
            let Some(path) = path else {
                unmatched.push(format!("{}: no such module", correction.module));
                continue;
            };
            let source = std::fs::read_to_string(&path).unwrap();
            let parsed = parse_unchecked_source(&source, PySourceType::BasedPythonStub);
            let mut rewriter = Rewriter {
                shadowed: Vec::new(),
                input_aliases: &BTreeSet::new(),
                module: Some(correction.module.to_string()),
                enclosing: Vec::new(),
                source: &source,
                edits: Vec::new(),
                seen: Vec::new(),
            };
            rewriter.visit_body(&parsed.syntax().body);
            if !rewriter
                .seen
                .iter()
                .any(|seen| std::ptr::eq(*seen, correction))
            {
                unmatched.push(format!(
                    "{}.{} ({:?}) matched nothing",
                    correction.module, correction.path, correction.scope
                ));
            }
        }
        assert!(
            unmatched.is_empty(),
            "stale corrections:\n{}",
            unmatched.join("\n")
        );
    }

    /// the last thing standing between a well-meaning edit and a stub that lies
    #[test]
    fn every_correction_says_why() {
        for correction in CORRECTIONS {
            assert!(
                correction.why.len() > 20,
                "{}.{} needs a reason worth reading",
                correction.module,
                correction.path
            );
        }
    }
}
