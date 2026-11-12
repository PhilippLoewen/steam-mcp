//! How the original TypeScript tools turned JSON values into text and
//! numbers, shared by the tools that build output from web API JSON.

use serde_json::Value;

use crate::util::{js_num, to_fixed};

/// JS truthiness of a JSON value; a missing one is falsy.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `${v}` in a JS template string.
pub fn js_str(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js_num(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => {
            a.iter().map(|e| if e.is_null() { String::new() } else { js_str(Some(e)) }).collect::<Vec<_>>().join(",")
        }
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// JS `Number(v)`.
pub fn number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() { 0.0 } else { t.parse().unwrap_or(f64::NAN) }
        }
        _ => f64::NAN,
    }
}

/// `n.toFixed(digits)`, which prints infinities as JS does.
pub fn fixed(n: f64, digits: usize) -> String {
    if n.is_infinite() { js_num(n) } else { to_fixed(n, digits) }
}

/// JS `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn converts_like_js() {
        assert!(!truthy(None) && !truthy(Some(&json!(0))) && !truthy(Some(&json!(""))) && truthy(Some(&json!([]))));
        assert_eq!(js_str(Some(&json!([1, null, "a"]))), "1,,a");
        assert_eq!(js_str(Some(&json!({}))), "[object Object]");
        assert_eq!(number(Some(&json!(" 12 "))), 12.0);
        assert!(number(Some(&json!("x"))).is_nan());
        assert_eq!(fixed(f64::INFINITY, 2), "Infinity");
        assert_eq!(encode_uri_component("Half-Life 2: Ep. 1/ü"), "Half-Life%202%3A%20Ep.%201%2F%C3%BC");
    }
}
