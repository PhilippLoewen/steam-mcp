//! A small declarative layer over MCP tools: parameter specs that produce the
//! JSON Schema, validate and default the arguments, and typed accessors.

use std::future::Future;
use std::pin::Pin;

use serde_json::{Map, Value, json};

pub type ToolFuture = Pin<Box<dyn Future<Output = ToolResult> + Send>>;

/// The text a tool returns, and whether it is an error.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: false }
    }

    /// `JSON.stringify(value, null, 2)`.
    pub fn json(value: &Value) -> Self {
        Self::text(serde_json::to_string_pretty(value).expect("JSON values always serialize"))
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self { text: text.into(), is_error: true }
    }

    /// Ok results pass through; errors become `"{prefix}: {message}"` error results.
    pub fn or_error(result: anyhow::Result<ToolResult>, prefix: &str) -> Self {
        result.unwrap_or_else(|e| Self::error(format!("{prefix}: {e}")))
    }
}

#[derive(Debug, Clone, Copy)]
pub enum ParamType {
    String,
    Number,
    Boolean,
    Enum(&'static [&'static str]),
    ArrayOfStrings,
    ArrayOfNumbers,
    ArrayOfIntegers,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: &'static str,
    pub ty: ParamType,
    pub required: bool,
    pub default: Option<Value>,
    pub description: Option<&'static str>,
}

impl Param {
    pub fn new(name: &'static str, ty: ParamType) -> Self {
        Self { name, ty, required: false, default: None, description: None }
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    pub fn describe(mut self, text: &'static str) -> Self {
        self.description = Some(text);
        self
    }

    fn schema(&self) -> Value {
        let mut s = match self.ty {
            ParamType::String => json!({ "type": "string" }),
            ParamType::Number => json!({ "type": "number" }),
            ParamType::Boolean => json!({ "type": "boolean" }),
            ParamType::Enum(values) => json!({ "type": "string", "enum": values }),
            ParamType::ArrayOfStrings => json!({ "type": "array", "items": { "type": "string" } }),
            ParamType::ArrayOfNumbers => json!({ "type": "array", "items": { "type": "number" } }),
            ParamType::ArrayOfIntegers => json!({ "type": "array", "items": { "type": "integer" } }),
        };
        let obj = s.as_object_mut().expect("object");
        if let Some(d) = &self.default {
            obj.insert("default".into(), d.clone());
        }
        if let Some(d) = self.description {
            obj.insert("description".into(), d.into());
        }
        s
    }

    fn check(&self, value: &Value) -> Result<(), String> {
        let is_int = |v: &Value| v.as_f64().is_some_and(|f| f.fract() == 0.0);
        let ok = match self.ty {
            ParamType::String => value.is_string(),
            ParamType::Number => value.is_number(),
            ParamType::Boolean => value.is_boolean(),
            ParamType::Enum(values) => {
                return match value.as_str() {
                    Some(s) if values.contains(&s) => Ok(()),
                    _ => Err(format!(
                        "{}: Invalid enum value. Expected {}, received {}",
                        self.name,
                        values.iter().map(|v| format!("'{v}'")).collect::<Vec<_>>().join(" | "),
                        value.as_str().map_or_else(|| js_type(value).to_string(), |s| format!("'{s}'"))
                    )),
                };
            }
            ParamType::ArrayOfStrings => value.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            ParamType::ArrayOfNumbers => value.as_array().is_some_and(|a| a.iter().all(Value::is_number)),
            ParamType::ArrayOfIntegers => value.as_array().is_some_and(|a| a.iter().all(is_int)),
        };
        if ok {
            Ok(())
        } else {
            Err(format!("{}: Expected {}, received {}", self.name, self.type_name(), js_type(value)))
        }
    }

    fn type_name(&self) -> &'static str {
        match self.ty {
            ParamType::String | ParamType::Enum(_) => "string",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
            ParamType::ArrayOfStrings => "array of strings",
            ParamType::ArrayOfNumbers => "array of numbers",
            ParamType::ArrayOfIntegers => "array of integers",
        }
    }
}

/// The type name zod reports for a received value.
fn js_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    pub params: Vec<Param>,
    pub handler: fn(Args) -> ToolFuture,
}

impl Tool {
    pub fn input_schema(&self) -> Map<String, Value> {
        let properties: Map<String, Value> = self.params.iter().map(|p| (p.name.to_string(), p.schema())).collect();
        let required: Vec<&str> = self.params.iter().filter(|p| p.required).map(|p| p.name).collect();
        let mut schema = Map::new();
        schema.insert("type".into(), "object".into());
        schema.insert("properties".into(), properties.into());
        if !required.is_empty() {
            schema.insert("required".into(), json!(required));
        }
        schema.insert("additionalProperties".into(), false.into());
        schema.insert("$schema".into(), "http://json-schema.org/draft-07/schema#".into());
        schema
    }

    /// Validate `arguments` against the params and fill in defaults.
    /// Unknown keys are dropped, as zod does.
    pub fn validate(&self, arguments: Option<Map<String, Value>>) -> Result<Args, String> {
        let input = arguments.unwrap_or_default();
        let mut out = Map::new();
        let mut errors = Vec::new();
        for p in &self.params {
            match input.get(p.name) {
                None | Some(Value::Null) if p.default.is_some() => {
                    out.insert(p.name.into(), p.default.clone().expect("checked"));
                }
                None => {
                    if p.required {
                        errors.push(format!("{}: Required", p.name));
                    }
                }
                Some(v) => match p.check(v) {
                    Ok(()) => {
                        out.insert(p.name.into(), v.clone());
                    }
                    Err(e) => errors.push(e),
                },
            }
        }
        if errors.is_empty() { Ok(Args(out)) } else { Err(errors.join("; ")) }
    }
}

/// Validated tool arguments.
#[derive(Debug, Clone, Default)]
pub struct Args(pub Map<String, Value>);

impl Args {
    pub fn str(&self, name: &str) -> Option<String> {
        self.0.get(name).and_then(Value::as_str).map(str::to_string)
    }

    pub fn f64(&self, name: &str) -> Option<f64> {
        self.0.get(name).and_then(Value::as_f64)
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.0.get(name).and_then(Value::as_bool)
    }

    pub fn strings(&self, name: &str) -> Option<Vec<String>> {
        self.0
            .get(name)
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
    }

    pub fn numbers(&self, name: &str) -> Option<Vec<f64>> {
        self.0.get(name).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect())
    }

    pub fn ints(&self, name: &str) -> Option<Vec<i64>> {
        self.numbers(name).map(|v| v.into_iter().map(|f| f as i64).collect())
    }

    pub fn value(&self, name: &str) -> Option<&Value> {
        self.0.get(name)
    }
}

/// Box an async handler body into a [`ToolFuture`].
pub fn boxed<F: Future<Output = ToolResult> + Send + 'static>(f: F) -> ToolFuture {
    Box::pin(f)
}
