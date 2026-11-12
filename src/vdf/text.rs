//! Recursive descent parser and serializer for text VDF.
//!
//! Handles quoted strings with backslash escapes, unquoted tokens, nested
//! objects, duplicate keys (last value wins, like Steam) and `//` comments.

use anyhow::{Result, bail};

use super::{MAX_DEPTH, VdfObject, VdfValue};

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Open,
    Close,
    Eof,
}

struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
    /// The first thing that shows the input isn't complete, well-formed VDF.
    /// The lenient parser carries on past it; the strict one fails on it.
    problem: Option<String>,
}

impl<'a> Lexer<'a> {
    fn skip_whitespace_and_comments(&mut self) {
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                b'/' if self.src.get(self.pos + 1) == Some(&b'/') => {
                    while self.pos < self.src.len() && self.src[self.pos] != b'\n' {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn read_quoted(&mut self) -> String {
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.src.len() {
            let ch = self.src[self.pos];
            if ch == b'"' {
                self.pos += 1;
                return bytes_to_string(out);
            }
            if ch == b'\\' && self.pos + 1 < self.src.len() {
                let next = self.src[self.pos + 1];
                match next {
                    b'n' => out.push(b'\n'),
                    b't' => out.push(b'\t'),
                    b'\\' => out.push(b'\\'),
                    b'"' => out.push(b'"'),
                    // Unknown escapes keep the backslash (paths like C:\Games).
                    _ => {
                        out.push(b'\\');
                        out.push(next);
                    }
                }
                self.pos += 2;
                continue;
            }
            out.push(ch);
            self.pos += 1;
        }
        self.flag("unterminated quoted string at end of input");
        bytes_to_string(out)
    }

    fn flag(&mut self, problem: &str) {
        if self.problem.is_none() {
            self.problem = Some(format!("{problem} (byte {})", self.pos));
        }
    }

    fn read_unquoted(&mut self) -> String {
        let start = self.pos;
        while self.pos < self.src.len() {
            match self.src[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' | b'{' | b'}' | b'"' => break,
                _ => self.pos += 1,
            }
        }
        bytes_to_string(self.src[start..self.pos].to_vec())
    }

    fn next(&mut self) -> Token {
        self.skip_whitespace_and_comments();
        match self.src.get(self.pos) {
            None => Token::Eof,
            Some(b'{') => {
                self.pos += 1;
                Token::Open
            }
            Some(b'}') => {
                self.pos += 1;
                Token::Close
            }
            Some(b'"') => Token::Str(self.read_quoted()),
            Some(_) => Token::Str(self.read_unquoted()),
        }
    }

    fn peek(&mut self) -> Token {
        let saved = self.pos;
        let tok = self.next();
        self.pos = saved;
        tok
    }
}

/// Only ASCII delimiters are ever split on, so slices stay valid UTF-8;
/// the lossy conversion only matters for files that were not UTF-8 to begin with.
fn bytes_to_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn parse_object(lex: &mut Lexer, depth: usize) -> VdfObject {
    let mut obj = VdfObject::new();
    loop {
        match lex.peek() {
            Token::Eof | Token::Close => break,
            _ => {}
        }
        let Token::Str(key) = lex.next() else {
            lex.flag("unexpected '{'");
            continue;
        };
        match lex.peek() {
            Token::Open if depth >= MAX_DEPTH => {
                // Stop here rather than recurse without bound on hostile input.
                lex.flag(&format!("nested deeper than {MAX_DEPTH} levels"));
                lex.pos = lex.src.len();
                break;
            }
            Token::Open => {
                lex.next();
                let child = parse_object(lex, depth + 1);
                if obj.insert(key, VdfValue::Obj(child)).is_some() {
                    lex.flag("duplicate key");
                }
                // Closing brace; a missing one is tolerated.
                if lex.next() != Token::Close {
                    lex.flag("missing '}' at end of input");
                }
            }
            Token::Str(value) => {
                lex.next();
                if obj.insert(key, VdfValue::Str(value)).is_some() {
                    lex.flag("duplicate key");
                }
            }
            _ => {
                lex.flag("key without a value");
                break;
            }
        }
    }
    obj
}

fn parse(text: &str) -> (VdfObject, Option<String>) {
    let mut lex = Lexer { src: text.as_bytes(), pos: 0, problem: None };
    let obj = parse_object(&mut lex, 0);
    if lex.next() != Token::Eof {
        lex.flag("unexpected '}'");
    }
    (obj, lex.problem)
}

/// Parse VDF text into an object, keeping whatever it can of malformed input.
pub fn parse_vdf(text: &str) -> VdfObject {
    parse(text).0
}

/// Parse VDF text, failing unless all of it is well-formed. Use this before
/// writing a file back, so a truncated read can't become a truncated file.
pub fn parse_vdf_strict(text: &str) -> Result<VdfObject> {
    match parse(text) {
        (obj, None) => Ok(obj),
        (_, Some(problem)) => bail!("malformed VDF: {problem}"),
    }
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n").replace('\t', "\\t")
}

fn serialize_value(out: &mut String, key: &str, value: &VdfValue, depth: usize) {
    let indent = "\t".repeat(depth);
    match value {
        VdfValue::Str(s) => {
            out.push_str(&format!("{indent}\"{}\"\t\t\"{}\"\n", escape(key), escape(s)));
        }
        VdfValue::Obj(o) => {
            out.push_str(&format!("{indent}\"{}\"\n{indent}{{\n", escape(key)));
            for (k, v) in o {
                serialize_value(out, k, v, depth + 1);
            }
            out.push_str(&format!("{indent}}}\n"));
        }
    }
}

/// Serialize an object back to VDF text, starting at `indent` tabs.
pub fn serialize_vdf(obj: &VdfObject, indent: usize) -> String {
    let mut out = String::new();
    for (k, v) in obj {
        serialize_value(&mut out, k, v, indent);
    }
    out
}
