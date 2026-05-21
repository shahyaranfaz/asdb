/*
codec.rs: tiny binary encoding for document values.

Storage only understands bytes. This module is the bridge between "user-shaped
data" and "heap record bytes".

FORMAT
  Document:
    [field_count: u32]
    repeated field_count times:
      [key_len: u32][key utf-8 bytes][value]

  Value:
    [tag: u8][payload]

Strings and arrays are length-prefixed. Integers and floats are fixed-width
little-endian. This is BSON-inspired, but deliberately smaller: no field type
table, no object byte length, no cstrings. Just enough structure to round-trip
documents without pulling in serde yet.
*/

use super::{Document, Value};

use std::fmt;

const TAG_NULL: u8 = 0x00;
const TAG_BOOL_FALSE: u8 = 0x01;
const TAG_BOOL_TRUE: u8 = 0x02;
const TAG_INT: u8 = 0x03;
const TAG_FLOAT: u8 = 0x04;
const TAG_STRING: u8 = 0x05;
const TAG_ARRAY: u8 = 0x06;
const TAG_DOCUMENT: u8 = 0x07;

/*
DecodeError: things that can go wrong while turning bytes back into Values.

Encoding is infallible right now because every in-memory Value can be written
to a Vec<u8>. Decoding is fallible because bytes can be truncated, corrupted,
or simply not produced by this codec.
*/
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    UnexpectedEof,
    InvalidTag(u8),
    InvalidUtf8,
    TrailingBytes,
    LengthOverflow,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof => write!(f, "unexpected end of document bytes"),
            DecodeError::InvalidTag(tag) => write!(f, "invalid value tag: {tag}"),
            DecodeError::InvalidUtf8 => write!(f, "document contains invalid utf-8"),
            DecodeError::TrailingBytes => write!(f, "document has trailing bytes"),
            DecodeError::LengthOverflow => write!(f, "encoded length does not fit on this platform"),
        }
    }
}

impl std::error::Error for DecodeError {}

/*
serialize_document / deserialize_document: public top-level document codec.

Notice that a top-level document does NOT start with TAG_DOCUMENT. The bytes are
the document body directly: [field_count][fields...]. Nested documents DO use
TAG_DOCUMENT because they appear inside a Value stream and need a tag.
*/
pub fn serialize_document(doc: &Document) -> Vec<u8> {
    let mut out = Vec::new();
    write_document(&mut out, doc);
    out
}

pub fn deserialize_document(bytes: &[u8]) -> Result<Document, DecodeError> {
    let mut cursor = Cursor::new(bytes);
    let doc = cursor.read_document_body()?;
    if cursor.is_done() {
        Ok(doc)
    } else {
        Err(DecodeError::TrailingBytes)
    }
}

/*
serialize_value / deserialize_value: useful for expression tests later.

The query engine will eventually evaluate expressions to Values. Having a
standalone value codec lets us test nested pieces without always wrapping them
in a fake top-level document.
*/
pub fn serialize_value(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, value);
    out
}

pub fn deserialize_value(bytes: &[u8]) -> Result<Value, DecodeError> {
    let mut cursor = Cursor::new(bytes);
    let value = cursor.read_value()?;
    if cursor.is_done() {
        Ok(value)
    } else {
        Err(DecodeError::TrailingBytes)
    }
}

/*
write_value: tag first, payload second.

The tag byte tells the decoder how many bytes to read next and how to interpret
them. For arrays/documents, the payload recursively contains more encoded
Values, which is why this function calls itself.
*/
fn write_value(out: &mut Vec<u8>, value: &Value) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(false) => out.push(TAG_BOOL_FALSE),
        Value::Bool(true) => out.push(TAG_BOOL_TRUE),
        Value::Int(n) => {
            out.push(TAG_INT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(n) => {
            out.push(TAG_FLOAT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            write_bytes(out, s.as_bytes());
        }
        Value::Array(values) => {
            out.push(TAG_ARRAY);
            write_len(out, values.len());
            for value in values {
                write_value(out, value);
            }
        }
        Value::Document(doc) => {
            out.push(TAG_DOCUMENT);
            write_document(out, doc);
        }
    }
}

/*
write_document: encode field count, then key/value pairs.

HashMap iteration order is intentionally unspecified, so serialized bytes for
the same logical document may come out in different orders. That's okay for v1:
we assert document equality after decoding, not byte-for-byte canonical output.
If we later need stable bytes for hashing/indexing, switch to BTreeMap or sort
keys while writing.
*/
fn write_document(out: &mut Vec<u8>, doc: &Document) {
    write_len(out, doc.len());
    for (key, value) in doc {
        write_bytes(out, key.as_bytes());
        write_value(out, value);
    }
}

fn write_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    write_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn write_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&(len as u32).to_le_bytes());
}

/*
Cursor: tiny reader over a byte slice.

Instead of slicing manually in every decode function, Cursor owns the current
position and advances as values are read. This is the same idea as a file
cursor in DiskManager, just in memory.
*/
struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    fn is_done(&self) -> bool {
        self.pos == self.bytes.len()
    }

    fn read_value(&mut self) -> Result<Value, DecodeError> {
        let tag = self.read_u8()?;
        match tag {
            TAG_NULL => Ok(Value::Null),
            TAG_BOOL_FALSE => Ok(Value::Bool(false)),
            TAG_BOOL_TRUE => Ok(Value::Bool(true)),
            TAG_INT => Ok(Value::Int(self.read_i64()?)),
            TAG_FLOAT => Ok(Value::Float(self.read_f64()?)),
            TAG_STRING => Ok(Value::String(self.read_string()?)),
            TAG_ARRAY => {
                let len = self.read_len()?;
                let mut values = Vec::with_capacity(len);
                for _ in 0..len {
                    values.push(self.read_value()?);
                }
                Ok(Value::Array(values))
            }
            TAG_DOCUMENT => Ok(Value::Document(self.read_document_body()?)),
            other => Err(DecodeError::InvalidTag(other)),
        }
    }

    fn read_document_body(&mut self) -> Result<Document, DecodeError> {
        let len = self.read_len()?;
        let mut doc = Document::with_capacity(len);
        for _ in 0..len {
            let key = self.read_string()?;
            let value = self.read_value()?;
            doc.insert(key, value);
        }
        Ok(doc)
    }

    fn read_string(&mut self) -> Result<String, DecodeError> {
        let bytes = self.read_bytes()?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::InvalidUtf8)
    }

    fn read_bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let len = self.read_len()?;
        let end = self.pos.checked_add(len).ok_or(DecodeError::LengthOverflow)?;
        if end > self.bytes.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let bytes = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(bytes)
    }

    fn read_len(&mut self) -> Result<usize, DecodeError> {
        let len = self.read_u32()?;
        usize::try_from(len).map_err(|_| DecodeError::LengthOverflow)
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        if self.pos >= self.bytes.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let byte = self.bytes[self.pos];
        self.pos += 1;
        Ok(byte)
    }

    fn read_u32(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_exact::<4>()?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_i64(&mut self) -> Result<i64, DecodeError> {
        let bytes = self.read_exact::<8>()?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_f64(&mut self) -> Result<f64, DecodeError> {
        let bytes = self.read_exact::<8>()?;
        Ok(f64::from_le_bytes(bytes))
    }

    fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let end = self.pos.checked_add(N).ok_or(DecodeError::LengthOverflow)?;
        if end > self.bytes.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let bytes = self.bytes[self.pos..end].try_into().unwrap();
        self.pos = end;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_trip_nested_document() {
        let mut nested = Document::new();
        nested.insert("active".to_string(), Value::Bool(true));
        nested.insert("score".to_string(), Value::Float(9.5));

        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(42));
        doc.insert("name".to_string(), Value::String("alice".to_string()));
        doc.insert("tags".to_string(), Value::Array(vec![
            Value::String("admin".to_string()),
            Value::Null,
        ]));
        doc.insert("profile".to_string(), Value::Document(nested));

        let bytes = serialize_document(&doc);
        let decoded = deserialize_document(&bytes).unwrap();
        assert_eq!(decoded, doc);
    }

    #[test]
    fn test_rejects_trailing_bytes() {
        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(1));

        let mut bytes = serialize_document(&doc);
        bytes.push(0);

        assert_eq!(deserialize_document(&bytes), Err(DecodeError::TrailingBytes));
    }

    #[test]
    fn test_rejects_invalid_tag() {
        let bytes = [0xff];
        assert_eq!(deserialize_value(&bytes), Err(DecodeError::InvalidTag(0xff)));
    }

    #[test]
    #[ignore = "phase 2 stress test: round-trips 10k generated documents"]
    fn stress_round_trip_10k_documents() {
        for i in 0..10_000u32 {
            let doc = generated_doc(i);
            let bytes = serialize_document(&doc);
            let decoded = deserialize_document(&bytes).unwrap();
            assert_eq!(decoded, doc);
        }
    }

    fn generated_doc(i: u32) -> Document {
        let mut nested = Document::new();
        nested.insert("flag".to_string(), Value::Bool(i % 2 == 0));
        nested.insert("ratio".to_string(), Value::Float(i as f64 / 3.0));

        let mut doc = Document::new();
        doc.insert("id".to_string(), Value::Int(i as i64));
        doc.insert("name".to_string(), Value::String(format!("user-{i}")));
        doc.insert("deleted_at".to_string(), Value::Null);
        doc.insert("meta".to_string(), Value::Document(nested));
        doc.insert("scores".to_string(), Value::Array(vec![
            Value::Int(i as i64),
            Value::Int((i as i64) * 2),
            Value::String(format!("bucket-{}", i % 11)),
        ]));
        doc
    }
}
