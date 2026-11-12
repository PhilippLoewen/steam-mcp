//! Valve KeyValues (VDF) in its text and binary forms.

mod binary;
mod text;

pub use binary::{BinObject, BinValue, parse_binary_vdf, parse_binary_vdf_strict, serialize_binary_vdf};
pub use text::{parse_vdf, parse_vdf_strict, serialize_vdf};

use indexmap::IndexMap;

/// Deepest object nesting either parser accepts. Steam's files stay far
/// below it; the limit keeps hostile input from overflowing the stack.
const MAX_DEPTH: usize = 64;

/// A VDF object. Insertion order is kept so files round-trip in Steam's order.
pub type VdfObject = IndexMap<String, VdfValue>;

#[derive(Debug, Clone, PartialEq)]
pub enum VdfValue {
    Str(String),
    Obj(VdfObject),
}

impl VdfValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            VdfValue::Str(s) => Some(s),
            VdfValue::Obj(_) => None,
        }
    }

    pub fn as_obj(&self) -> Option<&VdfObject> {
        match self {
            VdfValue::Obj(o) => Some(o),
            VdfValue::Str(_) => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut VdfObject> {
        match self {
            VdfValue::Obj(o) => Some(o),
            VdfValue::Str(_) => None,
        }
    }
}

impl From<&str> for VdfValue {
    fn from(s: &str) -> Self {
        VdfValue::Str(s.to_string())
    }
}

impl From<String> for VdfValue {
    fn from(s: String) -> Self {
        VdfValue::Str(s)
    }
}

impl From<VdfObject> for VdfValue {
    fn from(o: VdfObject) -> Self {
        VdfValue::Obj(o)
    }
}

/// Lookup helpers shared by everything that reads VDF.
pub trait VdfExt {
    /// The first of `keys` that holds a string.
    fn str_of(&self, keys: &[&str]) -> Option<&str>;
    /// The first of `keys` that holds an object.
    fn obj_of(&self, keys: &[&str]) -> Option<&VdfObject>;
    /// The first of `keys` present, as a string; "" when absent or an object.
    fn str_or_empty(&self, keys: &[&str]) -> String {
        self.str_of(keys).unwrap_or("").to_string()
    }
    /// The first of `keys` present, parsed like JS parseInt; `default` when absent or unparsable.
    fn int_of(&self, keys: &[&str], default: i64) -> i64 {
        self.str_of(keys).and_then(parse_int).unwrap_or(default)
    }
    /// Walk nested objects by key, falling back to a case-insensitive match per step.
    fn navigate(&self, keys: &[&str]) -> Option<&VdfObject>;
}

impl VdfExt for VdfObject {
    fn str_of(&self, keys: &[&str]) -> Option<&str> {
        // JS `a ?? b` falls through only on a missing key, so the first key present wins.
        keys.iter().find_map(|k| self.get(*k)).and_then(VdfValue::as_str)
    }

    fn obj_of(&self, keys: &[&str]) -> Option<&VdfObject> {
        keys.iter().find_map(|k| self.get(*k)).and_then(VdfValue::as_obj)
    }

    fn navigate(&self, keys: &[&str]) -> Option<&VdfObject> {
        let mut current = self;
        for key in keys {
            current = match current.get(*key) {
                Some(VdfValue::Obj(o)) => o,
                _ => current.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).and_then(|(_, v)| v.as_obj())?,
            };
        }
        Some(current)
    }
}

/// Whether JS treats `key` as an array index, which objects enumerate first, ascending.
pub fn is_array_index(key: &str) -> bool {
    !key.is_empty()
        && key.bytes().all(|b| b.is_ascii_digit())
        && (key == "0" || !key.starts_with('0'))
        && key.parse::<u64>().is_ok_and(|n| n < 4_294_967_295)
}

/// Entries in JS `Object.entries` order: array-index keys ascending, then the
/// rest in insertion order. Use this wherever iteration order reaches output.
pub fn js_entries<V>(obj: &IndexMap<String, V>) -> Vec<(&str, &V)> {
    let mut index: Vec<(u64, &str, &V)> = Vec::new();
    let mut rest = Vec::new();
    for (k, v) in obj {
        if is_array_index(k) {
            index.push((k.parse().expect("checked"), k, v));
        } else {
            rest.push((k.as_str(), v));
        }
    }
    index.sort_by_key(|(n, _, _)| *n);
    index.into_iter().map(|(_, k, v)| (k, v)).chain(rest).collect()
}

/// JS `parseInt(s, 10)`: optional whitespace and sign, then the leading digits.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let end = digits.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(digits.len());
    if end == 0 {
        return None;
    }
    let value: i64 = digits[..end].parse().ok()?;
    Some(if neg { -value } else { value })
}

#[cfg(test)]
mod tests;
