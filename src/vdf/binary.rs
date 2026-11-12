//! Binary VDF, as used by shortcuts.vdf.
//!
//! Type tags: 0x00 nested object, 0x01 null-terminated string,
//! 0x02 little-endian uint32, 0x08 end of the current object.
//! Keys are null-terminated strings.

use anyhow::{Result, bail};
use indexmap::IndexMap;

use super::MAX_DEPTH;

const TYPE_OBJECT: u8 = 0x00;
const TYPE_STRING: u8 = 0x01;
const TYPE_UINT32: u8 = 0x02;
const TYPE_END: u8 = 0x08;

/// A binary VDF object. Values keep their wire type so an edited file is
/// written back with every untouched field exactly as it was.
pub type BinObject = IndexMap<String, BinValue>;

#[derive(Debug, Clone, PartialEq)]
pub enum BinValue {
    Obj(BinObject),
    Str(String),
    Uint(u32),
}

impl BinValue {
    /// A string or number as text; None for an object.
    pub fn text(&self) -> Option<String> {
        match self {
            BinValue::Str(s) => Some(s.clone()),
            BinValue::Uint(n) => Some(n.to_string()),
            BinValue::Obj(_) => None,
        }
    }

    pub fn as_obj(&self) -> Option<&BinObject> {
        match self {
            BinValue::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_obj_mut(&mut self) -> Option<&mut BinObject> {
        match self {
            BinValue::Obj(o) => Some(o),
            _ => None,
        }
    }
}

impl From<&str> for BinValue {
    fn from(s: &str) -> Self {
        BinValue::Str(s.to_string())
    }
}

impl From<String> for BinValue {
    fn from(s: String) -> Self {
        BinValue::Str(s)
    }
}

impl From<u32> for BinValue {
    fn from(n: u32) -> Self {
        BinValue::Uint(n)
    }
}

impl From<BinObject> for BinValue {
    fn from(o: BinObject) -> Self {
        BinValue::Obj(o)
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    offset: usize,
    /// Refuse anything a rewrite couldn't reproduce exactly: invalid UTF-8,
    /// duplicate keys and bytes after the root's end tag.
    strict: bool,
}

impl Reader<'_> {
    fn read_byte(&mut self) -> Result<u8> {
        let Some(&b) = self.buf.get(self.offset) else {
            bail!("Unexpected end of buffer at offset {}", self.offset);
        };
        self.offset += 1;
        Ok(b)
    }

    fn read_cstr(&mut self) -> Result<String> {
        let start = self.offset;
        let Some(len) = self.buf[start..].iter().position(|&b| b == 0) else {
            bail!("Unterminated string at offset {start}");
        };
        self.offset += len + 1;
        let bytes = &self.buf[start..start + len];
        match std::str::from_utf8(bytes) {
            Ok(s) => Ok(s.to_string()),
            Err(_) if self.strict => bail!("Invalid UTF-8 in string at offset {start}"),
            Err(_) => Ok(String::from_utf8_lossy(bytes).into_owned()),
        }
    }

    fn read_u32(&mut self) -> Result<u32> {
        let Some(bytes) = self.buf.get(self.offset..self.offset + 4) else {
            bail!("Not enough bytes for uint32 at offset {}", self.offset);
        };
        self.offset += 4;
        Ok(u32::from_le_bytes(bytes.try_into().expect("slice of 4")))
    }
}

/// Read entries up to the object's end tag. Only the root may instead end
/// with the buffer; a nested object without its end tag means a truncated file.
fn read_object(r: &mut Reader, depth: usize) -> Result<BinObject> {
    if depth > MAX_DEPTH {
        bail!("Binary VDF nested deeper than {MAX_DEPTH} levels at offset {}", r.offset);
    }
    let mut obj = BinObject::new();
    while r.offset < r.buf.len() {
        let tag = r.read_byte()?;
        if tag == TYPE_END {
            if depth == 0 && r.strict && r.offset < r.buf.len() {
                bail!("{} unexpected bytes after the end at offset {}", r.buf.len() - r.offset, r.offset);
            }
            return Ok(obj);
        }
        let key_offset = r.offset;
        let key = r.read_cstr()?;
        let value = match tag {
            TYPE_OBJECT => BinValue::Obj(read_object(r, depth + 1)?),
            TYPE_STRING => BinValue::Str(r.read_cstr()?),
            TYPE_UINT32 => BinValue::Uint(r.read_u32()?),
            other => bail!("Unknown binary VDF type 0x{other:02x} at offset {}", r.offset - 1),
        };
        if obj.insert(key, value).is_some() && r.strict {
            bail!("Duplicate key at offset {key_offset}");
        }
    }
    // Only the root may end with the buffer, and in strict mode only an empty one:
    // Steam always closes the root, so a missing end tag means a cut-off file.
    if depth > 0 || (r.strict && !r.buf.is_empty()) {
        bail!("Unexpected end of buffer at offset {}: object not closed", r.offset);
    }
    Ok(obj)
}

/// Parse a binary VDF buffer. Invalid UTF-8 is replaced, a repeated key keeps
/// its last value and anything after the root's end tag is ignored.
pub fn parse_binary_vdf(buf: &[u8]) -> Result<BinObject> {
    read_object(&mut Reader { buf, offset: 0, strict: false }, 0)
}

/// Parse a binary VDF buffer that is about to be edited and written back:
/// fail on anything `serialize_binary_vdf` couldn't reproduce byte for byte.
pub fn parse_binary_vdf_strict(buf: &[u8]) -> Result<BinObject> {
    read_object(&mut Reader { buf, offset: 0, strict: true }, 0)
}

fn push_cstr(out: &mut Vec<u8>, s: &str) -> Result<()> {
    if s.contains('\0') {
        bail!("Binary VDF strings can't contain a NUL character: {s:?}");
    }
    out.extend_from_slice(s.as_bytes());
    out.push(0);
    Ok(())
}

fn write_object(out: &mut Vec<u8>, obj: &BinObject) -> Result<()> {
    for (key, value) in obj {
        match value {
            BinValue::Obj(o) => {
                out.push(TYPE_OBJECT);
                push_cstr(out, key)?;
                write_object(out, o)?;
                out.push(TYPE_END);
            }
            BinValue::Str(s) => {
                out.push(TYPE_STRING);
                push_cstr(out, key)?;
                push_cstr(out, s)?;
            }
            BinValue::Uint(n) => {
                out.push(TYPE_UINT32);
                push_cstr(out, key)?;
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
    }
    Ok(())
}

/// Serialize to binary VDF. The root's entries are written directly and
/// closed by a trailing 0x08, the layout Steam expects for shortcuts.vdf.
/// Fails when a key or string contains NUL, which would break the framing.
pub fn serialize_binary_vdf(obj: &BinObject) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_object(&mut out, obj)?;
    out.push(TYPE_END);
    Ok(out)
}
