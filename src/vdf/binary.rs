//! Binary VDF, as used by shortcuts.vdf.
//!
//! Type tags: 0x00 nested object, 0x01 null-terminated string,
//! 0x02 little-endian uint32, 0x08 end of the current object.
//! Keys are null-terminated strings.

use super::{VdfObject, VdfValue};
use anyhow::{Result, bail};

const TYPE_OBJECT: u8 = 0x00;
const TYPE_STRING: u8 = 0x01;
const TYPE_UINT32: u8 = 0x02;
const TYPE_END: u8 = 0x08;

struct Reader<'a> {
    buf: &'a [u8],
    offset: usize,
}

impl Reader<'_> {
    fn read_byte(&mut self) -> Result<u8> {
        let Some(&b) = self.buf.get(self.offset) else {
            bail!("Unexpected end of buffer at offset {}", self.offset);
        };
        self.offset += 1;
        Ok(b)
    }

    fn read_cstr(&mut self) -> String {
        let start = self.offset;
        while self.offset < self.buf.len() && self.buf[self.offset] != 0 {
            self.offset += 1;
        }
        let s = String::from_utf8_lossy(&self.buf[start..self.offset]).into_owned();
        if self.offset < self.buf.len() {
            self.offset += 1;
        }
        s
    }

    fn read_u32(&mut self) -> Result<u32> {
        let Some(bytes) = self.buf.get(self.offset..self.offset + 4) else {
            bail!("Not enough bytes for uint32 at offset {}", self.offset);
        };
        self.offset += 4;
        Ok(u32::from_le_bytes(bytes.try_into().expect("slice of 4")))
    }
}

fn read_object(r: &mut Reader) -> Result<VdfObject> {
    let mut obj = VdfObject::new();
    while r.offset < r.buf.len() {
        let tag = r.read_byte()?;
        if tag == TYPE_END {
            return Ok(obj);
        }
        let key = r.read_cstr();
        let value = match tag {
            TYPE_OBJECT => VdfValue::Obj(read_object(r)?),
            TYPE_STRING => VdfValue::Str(r.read_cstr()),
            TYPE_UINT32 => VdfValue::Str(r.read_u32()?.to_string()),
            other => bail!("Unknown binary VDF type 0x{other:02x} at offset {}", r.offset - 1),
        };
        obj.insert(key, value);
    }
    Ok(obj)
}

/// Parse a binary VDF buffer. uint32 values come back as decimal strings.
pub fn parse_binary_vdf(buf: &[u8]) -> Result<VdfObject> {
    if buf.is_empty() {
        return Ok(VdfObject::new());
    }
    read_object(&mut Reader { buf, offset: 0 })
}

fn write_object(out: &mut Vec<u8>, obj: &VdfObject) {
    for (key, value) in obj {
        match value {
            VdfValue::Obj(o) => {
                out.push(TYPE_OBJECT);
                push_cstr(out, key);
                write_object(out, o);
                out.push(TYPE_END);
            }
            VdfValue::Str(s) => {
                // Decimal strings that fit a u32 round-trip as uint32, matching the parser.
                match s.parse::<u32>().ok().filter(|_| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) {
                    Some(n) => {
                        out.push(TYPE_UINT32);
                        push_cstr(out, key);
                        out.extend_from_slice(&n.to_le_bytes());
                    }
                    None => {
                        out.push(TYPE_STRING);
                        push_cstr(out, key);
                        push_cstr(out, s);
                    }
                }
            }
        }
    }
}

fn push_cstr(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
    out.push(0);
}

/// Serialize to binary VDF. The root's entries are written directly and
/// closed by a trailing 0x08, the layout Steam expects for shortcuts.vdf.
pub fn serialize_binary_vdf(obj: &VdfObject) -> Vec<u8> {
    let mut out = Vec::new();
    write_object(&mut out, obj);
    out.push(TYPE_END);
    out
}
