//! zsh/mathfunc, after Src/Modules/mathfunc.c: the libm functions
//! available in arithmetic expressions, and rand48.

use crate::arith::Num;
use libc::c_int;

// libm functions std does not expose (or exposes only as unstable).
extern "C" {
    fn erf(x: f64) -> f64;
    fn erfc(x: f64) -> f64;
    fn tgamma(x: f64) -> f64;
    fn lgamma(x: f64) -> f64;
    fn j0(x: f64) -> f64;
    fn j1(x: f64) -> f64;
    fn jn(n: c_int, x: f64) -> f64;
    fn y0(x: f64) -> f64;
    fn y1(x: f64) -> f64;
    fn yn(n: c_int, x: f64) -> f64;
    fn ilogb(x: f64) -> c_int;
    fn logb(x: f64) -> f64;
    fn nextafter(x: f64, y: f64) -> f64;
    fn ldexp(x: f64, e: c_int) -> f64;
    fn scalbn(x: f64, e: c_int) -> f64;
    // The sign of Gamma(x) from the last lgamma() call.
    #[link_name = "signgam"]
    static SIGNGAM: c_int;
}

// Name, minimum and maximum argument count: zsh's mftab.
const FUNCS: &[(&str, usize, usize)] = &[
    ("abs", 1, 1),
    ("acos", 1, 1),
    ("acosh", 1, 1),
    ("asin", 1, 1),
    ("asinh", 1, 1),
    ("atan", 1, 2),
    ("atanh", 1, 1),
    ("cbrt", 1, 1),
    ("ceil", 1, 1),
    ("copysign", 2, 2),
    ("cos", 1, 1),
    ("cosh", 1, 1),
    ("erf", 1, 1),
    ("erfc", 1, 1),
    ("exp", 1, 1),
    ("expm1", 1, 1),
    ("fabs", 1, 1),
    ("float", 1, 1),
    ("floor", 1, 1),
    ("fmod", 2, 2),
    ("gamma", 1, 1),
    ("hypot", 2, 2),
    ("ilogb", 1, 1),
    ("int", 1, 1),
    ("isinf", 1, 1),
    ("isnan", 1, 1),
    ("j0", 1, 1),
    ("j1", 1, 1),
    ("jn", 2, 2),
    ("ldexp", 2, 2),
    ("lgamma", 1, 1),
    ("log", 1, 1),
    ("log10", 1, 1),
    ("log1p", 1, 1),
    ("log2", 1, 1),
    ("logb", 1, 1),
    ("nextafter", 2, 2),
    ("rint", 1, 1),
    ("scalb", 2, 2),
    ("signgam", 0, 0),
    ("sin", 1, 1),
    ("sinh", 1, 1),
    ("sqrt", 1, 1),
    ("tan", 1, 1),
    ("tanh", 1, 1),
    ("y0", 1, 1),
    ("y1", 1, 1),
    ("yn", 2, 2),
];

/// Whether `name` is a zsh/mathfunc function taking its argument as the
/// raw text between the parentheses (zsh's STRMATHFUNC): only rand48.
pub fn is_string_func(name: &str) -> bool {
    name == "rand48"
}

/// Calls the numeric zsh/mathfunc function `name`. None when `name` is
/// not one (rand48 included: see `call_string`).
pub fn call(name: &str, args: &[Num]) -> Option<Result<Num, String>> {
    let &(_, min, max) = FUNCS.iter().find(|f| f.0 == name)?;
    if args.len() < min || args.len() > max {
        let list: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        return Some(Err(format!(
            "wrong number of arguments: {}({})",
            name,
            list.join(",")
        )));
    }
    Some(Ok(math_func(name, args)))
}

// zsh's math_func(). Integer arguments are converted to float except for
// abs, and for the integer argument of jn/yn (first) and ldexp/scalb
// (second), which C truncates to int.
fn math_func(name: &str, args: &[Num]) -> Num {
    let argd = args.first().map_or(0.0, |a| a.as_float_pub());
    let argd2 = args.get(1).map_or(0.0, |a| a.as_float_pub());
    let argi = match name {
        "jn" | "yn" => args.first(),
        "ldexp" | "scalb" => args.get(1),
        _ => None,
    }
    .map_or(0, |a| a.as_int() as c_int);
    // SAFETY: the extern functions are pure libm calls on plain values,
    // and SIGNGAM is a plain int that libm writes only from lgamma().
    let retd = unsafe {
        match name {
            "abs" => {
                return match args[0] {
                    Num::I(n) => Num::I(n.wrapping_abs()),
                    Num::F(d) => Num::F(d.abs()),
                }
            }
            "acos" => argd.acos(),
            "acosh" => argd.acosh(),
            "asin" => argd.asin(),
            "asinh" => argd.asinh(),
            "atan" if args.len() == 2 => argd.atan2(argd2),
            "atan" => argd.atan(),
            "atanh" => argd.atanh(),
            "cbrt" => argd.cbrt(),
            "ceil" => argd.ceil(),
            "copysign" => argd.copysign(argd2),
            "cos" => argd.cos(),
            "cosh" => argd.cosh(),
            "erf" => erf(argd),
            "erfc" => erfc(argd),
            "exp" => argd.exp(),
            "expm1" => argd.exp_m1(),
            "fabs" => argd.abs(),
            "float" => argd,
            "floor" => argd.floor(),
            "fmod" => argd % argd2,
            "gamma" => tgamma(argd),
            "hypot" => argd.hypot(argd2),
            "ilogb" => return Num::I(i64::from(ilogb(argd))),
            "int" => return Num::I(argd as i64),
            // glibc's isinf() is -1 for negative infinity.
            "isinf" => {
                return Num::I(match argd.is_infinite() {
                    true if argd < 0.0 => -1,
                    true => 1,
                    false => 0,
                })
            }
            "isnan" => return Num::I(i64::from(argd.is_nan())),
            "j0" => j0(argd),
            "j1" => j1(argd),
            "jn" => jn(argi, argd2),
            "ldexp" => ldexp(argd, argi),
            "lgamma" => lgamma(argd),
            "log" => argd.ln(),
            "log10" => argd.log10(),
            "log1p" => argd.ln_1p(),
            "log2" => argd.log2(),
            "logb" => logb(argd),
            "nextafter" => nextafter(argd, argd2),
            // rint() rounds in the default mode: to nearest, ties to even.
            "rint" => argd.round_ties_even(),
            "scalb" => scalbn(argd, argi),
            "signgam" => return Num::I(i64::from(SIGNGAM)),
            "sin" => argd.sin(),
            "sinh" => argd.sinh(),
            "sqrt" => argd.sqrt(),
            "tan" => argd.tan(),
            "tanh" => argd.tanh(),
            "y0" => y0(argd),
            "y1" => y1(argd),
            "yn" => yn(argi, argd2),
            _ => 0.0,
        }
    };
    Num::F(retd)
}

/// Calls the string zsh/mathfunc function `name` (rand48) with the raw
/// text between its parentheses. None when `name` is not one.
///
/// `rand48(param)` keeps its seed in the scalar `param` as 12 hex digits;
/// `rand48()` uses a fresh seed each call.
pub fn call_string(name: &str, arg: &str) -> Option<Result<Num, String>> {
    if !is_string_func(name) {
        return None;
    }
    let arg = arg.trim_matches(|c: char| c == ' ' || c == '\t');
    let mut seed = [0u16; 3];
    let mut do_init = true;
    if !arg.is_empty() {
        if let Some(s) = crate::shell::var_get_opt(arg) {
            let b = s.as_bytes();
            if b.len() >= 12 {
                do_init = false;
                'shorts: for (k, slot) in seed.iter_mut().enumerate() {
                    for &c in &b[k * 4..k * 4 + 4] {
                        match (c as char).to_digit(16) {
                            Some(d) => *slot = *slot * 16 + d as u16,
                            None => {
                                do_init = true;
                                break 'shorts;
                            }
                        }
                    }
                }
            }
        }
    }
    // zsh's default seed check never skips this (do_init starts true), so
    // rand48() without a parameter reseeds from rand() on every call.
    if do_init {
        // SAFETY: rand() has no preconditions; seed48() reads three
        // unsigned shorts from `seed`, which has exactly three.
        unsafe {
            for s in seed.iter_mut() {
                *s = libc::rand() as u16;
            }
            libc::seed48(seed.as_mut_ptr());
        }
    }
    // SAFETY: erand48() reads and updates the three shorts of `seed`.
    let d = unsafe { libc::erand48(seed.as_mut_ptr()) };
    if !arg.is_empty() {
        crate::shell::var_set(
            arg,
            &format!("{:04x}{:04x}{:04x}", seed[0], seed[1], seed[2]),
            false,
        );
    }
    Some(Ok(Num::F(d)))
}
