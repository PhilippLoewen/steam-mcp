//! Recursive descent parser and serializer for text VDF.
//!
//! Handles quoted strings with backslash escapes, unquoted tokens, nested
//! objects, duplicate keys (last value wins, like Steam) and `//` comments.

use super::{VdfObject, VdfValue};

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
        bytes_to_string(out)
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

fn parse_object(lex: &mut Lexer) -> VdfObject {
    let mut obj = VdfObject::new();
    loop {
        match lex.peek() {
            Token::Eof | Token::Close => break,
            _ => {}
        }
        let Token::Str(key) = lex.next() else {
            continue;
        };
        match lex.peek() {
            Token::Open => {
                lex.next();
                let child = parse_object(lex);
                obj.insert(key, VdfValue::Obj(child));
                // Closing brace; a missing one is tolerated.
                lex.next();
            }
            Token::Str(value) => {
                lex.next();
                obj.insert(key, VdfValue::Str(value));
            }
            _ => break,
        }
    }
    obj
}

/// Parse VDF text into an object.
pub fn parse_vdf(text: &str) -> VdfObject {
    let mut lex = Lexer { src: text.as_bytes(), pos: 0 };
    parse_object(&mut lex)
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
