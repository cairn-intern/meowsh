//! Port of zsh's deltochar.c (the zsh/deltochar module).

use crate::zle::*;

const NAMES: [&str; 2] = ["delete-to-char", "zap-to-char"];

/// Adds the module's widgets (boot_).
pub fn register() {
    for name in NAMES {
        thingy::add_internal(name, deltochar, ZLE_KILL | ZLE_KEEPSUFFIX);
    }
}

/// Removes the module's widgets (cleanup_).
pub fn unregister() {
    for name in NAMES {
        thingy::delete_internal(name);
    }
}

fn deltochar(z: &mut Zle, _args: &[String]) -> i32 {
    let c = z.getfullchar(false);
    let mut dest = z.cs;
    let mut ok = 0;
    let mut n = z.zmod.mult;
    let zap = match (
        z.bindk.as_deref().and_then(thingy::lookup),
        thingy::lookup("zap-to-char"),
    ) {
        (Some(b), Some(w)) => std::rc::Rc::ptr_eq(&b, &w),
        _ => false,
    };

    if n > 0 {
        while n != 0 {
            n -= 1;
            if dest == z.ll() {
                break;
            }
            while dest != z.ll() && Some(z.line[dest]) != c {
                dest += 1;
            }
            if dest != z.ll() {
                if !zap || n > 0 {
                    dest += 1;
                }
                if n == 0 {
                    let ct = dest - z.cs;
                    z.forekill(ct, CUT_RAW);
                    ok += 1;
                }
            }
        }
    } else {
        // Ignore the character the cursor is on when scanning backwards.
        dest = dest.saturating_sub(1);
        loop {
            let t = n;
            n += 1;
            if t == 0 || dest == 0 {
                break;
            }
            while dest != 0 && Some(z.line[dest]) != c {
                dest -= 1;
            }
            if Some(z.line[dest]) == c {
                if n == 0 {
                    let ct = z.cs - dest - usize::from(zap);
                    z.backkill(ct, CUT_RAW | CUT_FRONT);
                    ok += 1;
                }
                dest = dest.saturating_sub(1);
            }
        }
    }
    i32::from(ok == 0)
}
