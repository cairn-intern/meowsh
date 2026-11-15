//! Shell parameters: one store for scalars, arrays and associative
//! arrays, each carrying zsh attributes (`typeset` flags), plus the
//! function-local scopes that `local` and `typeset` create.

use crate::types::Shell;
use std::collections::{BTreeMap, HashMap};

pub const EXPORT: u32 = 1 << 0;
pub const READONLY: u32 = 1 << 1;
pub const INTEGER: u32 = 1 << 3; // -i: value is an integer, `base` its base
pub const FLOAT_E: u32 = 1 << 4; // -E: float, scientific notation
pub const FLOAT_F: u32 = 1 << 5; // -F: float, fixed point
pub const LOWER: u32 = 1 << 6; // -l
pub const UPPER: u32 = 1 << 7; // -u
pub const LEFT: u32 = 1 << 8; // -L: left justify to `width`
pub const RIGHT_B: u32 = 1 << 9; // -R: right justify with blanks
pub const RIGHT_Z: u32 = 1 << 10; // -Z: right justify with zeros
pub const UNIQUE: u32 = 1 << 11; // -U: arrays keep the first of duplicates
pub const HIDE: u32 = 1 << 12; // -h
pub const HIDEVAL: u32 = 1 << 13; // -H: typeset does not show the value
pub const TAG: u32 = 1 << 14; // -t
pub const NAMEREF: u32 = 1 << 15; // -n: the value names another parameter
pub const SPECIAL: u32 = 1 << 16;
pub const UNSET: u32 = 1 << 17; // declared but without a value
pub const DECLARED: u32 = 1 << 18;

pub const NUMERIC: u32 = INTEGER | FLOAT_E | FLOAT_F;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Scalar(String),
    Array(Vec<String>),
    Assoc(BTreeMap<String, String>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub value: Value,
    pub attrs: u32,
    // -L/-R/-Z width; 0 means "from the first value assigned".
    pub width: usize,
    // -i base or -E/-F digits; 0 means the default.
    pub base: u32,
    // A tied scalar/array pair (`typeset -T`, PATH/path): the other name
    // and the separator.
    pub tie: Option<(String, String)>,
    // Function depth that created it; 0 for globals.
    pub level: usize,
}

impl Param {
    pub fn scalar(v: impl Into<String>) -> Param {
        Param::with(Value::Scalar(v.into()))
    }

    pub fn with(value: Value) -> Param {
        Param {
            value,
            attrs: 0,
            width: 0,
            base: 0,
            tie: None,
            level: 0,
        }
    }

    pub fn is_array(&self) -> bool {
        matches!(self.value, Value::Array(_))
    }

    pub fn is_assoc(&self) -> bool {
        matches!(self.value, Value::Assoc(_))
    }

    /// The value as `$name` gives it, arrays joined with spaces. Scalars
    /// are shown through their attributes: integers in their base, floats
    /// in their format and precision, case conversion and justification.
    pub fn text(&self) -> String {
        match &self.value {
            Value::Scalar(s) => self.display(s),
            Value::Array(a) => a.join(" "),
            Value::Assoc(m) => m.values().cloned().collect::<Vec<_>>().join(" "),
        }
    }

    pub fn is_set(&self) -> bool {
        self.attrs & UNSET == 0
    }

    fn display(&self, raw: &str) -> String {
        let a = self.attrs;
        let mut v = if a & INTEGER != 0 {
            match raw.trim().parse::<i64>() {
                Ok(n) => crate::shell::format_int(n, self.base),
                Err(_) => raw.to_string(),
            }
        } else if a & (FLOAT_E | FLOAT_F) != 0 {
            match raw.trim().parse::<f64>() {
                Ok(f) => crate::shell::format_float(f, a, self.base),
                Err(_) => raw.to_string(),
            }
        } else {
            raw.to_string()
        };
        if a & LOWER != 0 {
            v = v.to_lowercase();
        }
        if a & UPPER != 0 {
            v = v.to_uppercase();
        }
        if a & (LEFT | RIGHT_B | RIGHT_Z) != 0 {
            let width = if self.width == 0 {
                raw.chars().count()
            } else {
                self.width
            };
            v = crate::shell::justify(&v, a, width);
        }
        v
    }
}

/// Built-in tied pairs: (scalar, array).
pub const DEFAULT_TIES: &[(&str, &str)] = &[
    ("PATH", "path"),
    ("FPATH", "fpath"),
    ("CDPATH", "cdpath"),
    ("MANPATH", "manpath"),
    ("MODULE_PATH", "module_path"),
    ("MAILPATH", "mailpath"),
    ("FIGNORE", "fignore"),
];

pub fn split_tied(value: &str, sep: &str) -> Vec<String> {
    if value.is_empty() {
        Vec::new()
    } else {
        value.split(sep).map(|s| s.to_string()).collect()
    }
}

impl Shell {
    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.get(name)
    }

    pub fn param_mut(&mut self, name: &str) -> Option<&mut Param> {
        self.params.get_mut(name)
    }

    pub fn scalar(&self, name: &str) -> Option<&str> {
        match &self.params.get(name)?.value {
            Value::Scalar(s) => Some(s),
            _ => None,
        }
    }

    pub fn array(&self, name: &str) -> Option<&Vec<String>> {
        match &self.params.get(name)?.value {
            Value::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn assoc(&self, name: &str) -> Option<&BTreeMap<String, String>> {
        match &self.params.get(name)?.value {
            Value::Assoc(m) => Some(m),
            _ => None,
        }
    }

    pub fn assoc_mut(&mut self, name: &str) -> Option<&mut BTreeMap<String, String>> {
        match &mut self.params.get_mut(name)?.value {
            Value::Assoc(m) => Some(m),
            _ => None,
        }
    }

    /// The associative array `name`, created (replacing any other kind of
    /// value, keeping attributes) if needed.
    pub fn assoc_entry(&mut self, name: &str) -> &mut BTreeMap<String, String> {
        let p = self
            .params
            .entry(name.to_string())
            .or_insert_with(|| Param::with(Value::Assoc(BTreeMap::new())));
        if !p.is_assoc() {
            p.value = Value::Assoc(BTreeMap::new());
        }
        match &mut p.value {
            Value::Assoc(m) => m,
            _ => unreachable!(),
        }
    }

    /// Follows namerefs (`typeset -n`) from `name` to the parameter they
    /// finally refer to. The result may carry a subscript (`arr[2]`).
    pub fn resolve_name(&self, name: &str) -> String {
        let mut cur = name.to_string();
        for _ in 0..64 {
            match self.params.get(&cur) {
                Some(p) if p.attrs & NAMEREF != 0 && p.attrs & UNSET == 0 => {
                    let Value::Scalar(target) = &p.value else {
                        return cur;
                    };
                    if target.is_empty() || *target == cur {
                        return cur;
                    }
                    cur = target.clone();
                }
                _ => return cur,
            }
        }
        cur
    }

    pub fn has_param(&self, name: &str) -> bool {
        self.params.contains_key(name)
    }

    /// Stores a value without attribute processing or readonly checks,
    /// keeping the parameter's attributes, and updating a tied partner.
    pub fn store(&mut self, name: &str, value: Value) {
        let p = self
            .params
            .entry(name.to_string())
            .or_insert_with(|| Param::with(Value::Scalar(String::new())));
        p.value = value;
        self.sync_tie(name);
    }

    pub fn remove_param(&mut self, name: &str) -> Option<Param> {
        let p = self.params.remove(name)?;
        // Unsetting one side of a tie unsets the other (zsh unties them).
        if let Some((other, _)) = &p.tie {
            if let Some(o) = self.params.get_mut(other) {
                o.tie = None;
            }
        }
        Some(p)
    }

    /// Re-derives the tied partner of `name` from its value.
    pub fn sync_tie(&mut self, name: &str) {
        let Some(p) = self.params.get(name) else {
            return;
        };
        let Some((other, sep)) = p.tie.clone() else {
            return;
        };
        let unique = p.attrs & UNIQUE != 0
            || self
                .params
                .get(&other)
                .is_some_and(|o| o.attrs & UNIQUE != 0);
        let dedup = |mut a: Vec<String>| {
            if unique {
                let mut seen = std::collections::HashSet::new();
                a.retain(|x| seen.insert(x.clone()));
            }
            a
        };
        let (mine, derived) = match &p.value {
            Value::Scalar(s) => {
                let a = dedup(split_tied(s, &sep));
                (Value::Scalar(a.join(&sep)), Value::Array(a))
            }
            Value::Array(a) => {
                let a = dedup(a.clone());
                (Value::Array(a.clone()), Value::Scalar(a.join(&sep)))
            }
            Value::Assoc(_) => return,
        };
        if let Some(me) = self.params.get_mut(name) {
            me.value = mine;
        }
        if let Some(o) = self.params.get_mut(&other) {
            o.value = derived;
        }
    }

    /// Ties scalar `scalar` and array `array` with separator `sep`,
    /// keeping the scalar's current value if it has one.
    pub fn tie(&mut self, scalar: &str, array: &str, sep: &str) {
        let svalue = self.scalar(scalar).map(|s| s.to_string());
        let avalue = self.array(array).cloned();
        let s_attrs = self.params.get(scalar).map(|p| p.attrs).unwrap_or(0);
        let a_attrs = self.params.get(array).map(|p| p.attrs).unwrap_or(0);
        let (sv, av) = match (svalue, avalue) {
            (Some(s), _) => (s.clone(), split_tied(&s, sep)),
            (None, Some(a)) => (a.join(sep), a),
            (None, None) => (String::new(), Vec::new()),
        };
        let mut sp = Param::scalar(sv);
        sp.attrs = s_attrs;
        sp.tie = Some((array.to_string(), sep.to_string()));
        let mut ap = Param::with(Value::Array(av));
        ap.attrs = a_attrs;
        ap.tie = Some((scalar.to_string(), sep.to_string()));
        self.params.insert(scalar.to_string(), sp);
        self.params.insert(array.to_string(), ap);
    }

    /// Makes `name` local to the innermost function scope: saves its
    /// current state (and its tied partner's) to restore when the
    /// function returns, and replaces it with an empty scalar at this
    /// level. Already local here: nothing to do. False outside functions.
    pub fn make_local(&mut self, name: &str) -> bool {
        let depth = self.local_scopes.len();
        if depth == 0 {
            return false;
        }
        if self.params.get(name).is_some_and(|p| p.level == depth) {
            return true;
        }
        let prev = self.params.get(name).cloned();
        let partner = prev
            .as_ref()
            .and_then(|p| p.tie.as_ref())
            .map(|(o, _)| (o.clone(), self.params.get(o).cloned()));
        let Some(scope) = self.local_scopes.last_mut() else {
            return false;
        };
        scope.entry(name.to_string()).or_insert(prev);
        if let Some((o, op)) = partner {
            scope.entry(o.clone()).or_insert(op);
            self.params.remove(&o);
        }
        let mut fresh = Param::scalar("");
        fresh.level = depth;
        self.params.insert(name.to_string(), fresh);
        true
    }

    pub fn is_local_here(&self, name: &str) -> bool {
        self.local_scopes
            .last()
            .is_some_and(|s| s.contains_key(name))
    }

    pub fn push_scope(&mut self) {
        self.local_scopes.push(HashMap::new());
    }

    pub fn pop_scope(&mut self) {
        let Some(scope) = self.local_scopes.pop() else {
            return;
        };
        for (name, prev) in scope {
            match prev {
                Some(p) => {
                    self.params.insert(name, p);
                }
                None => {
                    self.params.remove(&name);
                }
            }
        }
    }
}
