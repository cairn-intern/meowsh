//! Widgets zsh defines in zle_main.c, zle_refresh.c and zle_utils.c.

use crate::zle::keymap::{bindztrdup, nice, Bind};
use crate::zle::*;

pub const WIDGETS: &[(&str, WidgetFn)] = &[
    ("beep", utils::handlefeep),
    ("clear-screen", clearscreen),
    ("redisplay", redisplay),
    ("reset-prompt", resetprompt),
    ("undo", undo),
    ("redo", redo),
    ("split-undo", splitundo),
    ("read-command", readcommand),
    ("recursive-edit", recursiveedit),
    ("describe-key-briefly", describekeybriefly),
    ("where-is", whereis),
    ("auto-suffix-remove", handlesuffix),
    ("auto-suffix-retain", handlesuffix),
];

pub fn clearscreen(z: &mut Zle, _args: &[String]) -> i32 {
    z.clearscreen();
    0
}

pub fn redisplay(z: &mut Zle, _args: &[String]) -> i32 {
    z.redisplay();
    0
}

pub fn resetprompt(z: &mut Zle, args: &[String]) -> i32 {
    redisplay(z, args)
}

pub fn undo(z: &mut Zle, args: &[String]) -> i32 {
    z.undo(args)
}

pub fn redo(z: &mut Zle, _args: &[String]) -> i32 {
    z.redo()
}

pub fn splitundo(z: &mut Zle, _args: &[String]) -> i32 {
    z.splitundo()
}

pub fn handlesuffix(_z: &mut Zle, _args: &[String]) -> i32 {
    0
}

pub fn readcommand(z: &mut Zle, _args: &[String]) -> i32 {
    match z.getkeycmd() {
        Some(name) => {
            z.shell(|| crate::shell::var_set("REPLY", &name, false));
            0
        }
        None => 1,
    }
}

pub fn recursiveedit(z: &mut Zle, _args: &[String]) -> i32 {
    z.recursive += 1;
    z.zrefresh();
    z.run_core();
    z.recursive -= 1;
    let err = i32::from(z.errflag);
    z.errflag = false;
    z.done = false;
    z.eofsent = false;
    err
}

pub fn describekeybriefly(z: &mut Zle, _args: &[String]) -> i32 {
    if z.statusline.is_some() {
        return 1;
    }
    z.clearlist = true;
    z.statusline = Some("Describe key briefly: _".into());
    z.zrefresh();
    if z.invicmdmode() && z.region_active != 0 {
        z.selectlocalmap(keymap::open_keymap("visual"));
    }
    let km = z.curkeymap.clone();
    let (bind, empty) = z.getkeymapcmd(&km);
    z.selectlocalmap(None);
    z.statusline = None;
    if empty {
        return 1;
    }
    let is = match bind {
        Some(Bind::Str(s)) => bindztrdup(&s),
        Some(Bind::Widget(w)) => nice(&w),
        None => "undefined-key".into(),
    };
    let msg = format!("{} is {}", bindztrdup(&z.keybuf), is);
    z.showmsg(&msg);
    0
}

pub fn whereis(z: &mut Zle, _args: &[String]) -> i32 {
    let Some(func) = super::misc::executenamedcommand(z, "Where is: ") else {
        return 1;
    };
    let mut msg = nice(&func);
    let mut found = 0;
    for (seq, b) in z.curkeymap.borrow().map.iter() {
        if *b != Bind::Widget(func.clone()) {
            continue;
        }
        found += 1;
        if found == 1 {
            msg.push_str(" is on");
        }
        if found <= 4 {
            msg.push(' ');
            msg.push_str(&bindztrdup(seq));
        }
    }
    if found == 0 {
        msg.push_str(" is not bound to any key");
    } else if found > 4 {
        msg.push_str(" et al");
    }
    z.showmsg(&msg);
    0
}
