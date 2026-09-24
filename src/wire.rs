/*
wire.rs: the ABP/1 binary encoding. Pure functions, no IO.

Split from the server for the same reason AsdbEntityMapper is split from
AsdbClient on the Java side: encoding is testable without a socket, and a
socket bug cannot masquerade as an encoding bug.

FRAMING

Every message, both directions:

    [u32 length LE][u8 opcode][payload ...]
     \___________/
      byte count of opcode + payload, NOT including the 4 length bytes

Length-prefixed rather than delimiter-terminated because document values are
arbitrary bytes: any delimiter would need escaping, and escaping is what the
ASL text path has to do (see AsdbEntityMapper.quote). A length prefix cannot
be spoofed by content, which removes a whole class of injection by
construction rather than by careful escaping.

Little-endian throughout, matching storage/heap.rs. x86 and ARM are both LE,
so it is a no-op on every platform this runs on.

VALUE ENCODING

One tag byte then the payload, mirroring document/key.rs's tagging so the two
stay mentally aligned:

    0x00 Null          (no payload)
    0x01 Bool false    (no payload)
    0x02 Bool true     (no payload)
    0x03 Int           i64 LE
    0x04 Float         f64 LE
    0x05 String        u32 len + UTF-8 bytes
    0x06 Array         u32 count + values
    0x07 Document      u32 count + (u32 keylen + key + value) pairs

Bool is two tags rather than a tag plus a byte: it saves a byte and removes
the "what does 0x07 mean in a bool payload" question entirely.

Int stays i64 and Float stays f64 with no varint compression. That is a
deliberate non-optimisation and the reasoning is in PROTOCOL.txt section 3:
varints would shrink small integers but epoch-millis timestamps, the single
most common integer in this workload, are ~1.7e12 and would cost 7 bytes
instead of 8 while adding branchy decode to every field.
*/

use crate::document::{Document, Value};

pub const TAG_NULL: u8 = 0x00;
pub const TAG_FALSE: u8 = 0x01;
pub const TAG_TRUE: u8 = 0x02;
pub const TAG_INT: u8 = 0x03;
pub const TAG_FLOAT: u8 = 0x04;
pub const TAG_STRING: u8 = 0x05;
pub const TAG_ARRAY: u8 = 0x06;
pub const TAG_DOCUMENT: u8 = 0x07;

/*
Op: the opcodes, as a type rather than as loose bytes.

A frame's first byte is the only place the number matters, so it is parsed on
read and written back on send; nothing in between carries a bare u8. The Java
client mirrors this as AbpCodec.Op, and the numbers are the contract between
the two.

An unknown opcode is a real case, not a bug: a mismatched build on either side,
or a desynchronised stream. TryFrom gives the caller that case to answer rather
than a number nothing matches.
*/
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    // requests
    Exec = 1,
    Insert = 2,
    Ping = 3,
    Close = 4,
    Upsert = 5,
    // responses
    Affected = 6,
    Documents = 7,
    Error = 8,
    Pong = 9,
}

impl Op {
    pub fn code(self) -> u8 {
        self as u8
    }
}

impl TryFrom<u8> for Op {
    type Error = u8;

    fn try_from(code: u8) -> Result<Self, u8> {
        Ok(match code {
            1 => Op::Exec,
            2 => Op::Insert,
            3 => Op::Ping,
            4 => Op::Close,
            5 => Op::Upsert,
            6 => Op::Affected,
            7 => Op::Documents,
            8 => Op::Error,
            9 => Op::Pong,
            other => return Err(other),
        })
    }
}

pub const MAX_FRAME: usize = 64 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub enum WireError {
    Truncated,
    BadTag(u8),
    BadUtf8,
    TooLarge(usize),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated => write!(f, "frame ended mid-value"),
            WireError::BadTag(t) => write!(f, "unknown value tag 0x{t:02x}"),
            WireError::BadUtf8 => write!(f, "string was not valid UTF-8"),
            WireError::TooLarge(n) => write!(f, "frame of {n} bytes exceeds the limit"),
        }
    }
}

pub type WireResult<T> = Result<T, WireError>;

/* ---------- writing ---------- */

pub fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

pub fn put_value(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(false) => out.push(TAG_FALSE),
        Value::Bool(true) => out.push(TAG_TRUE),
        Value::Int(n) => {
            out.push(TAG_INT);
            out.extend_from_slice(&n.to_le_bytes());
        }
        Value::Float(x) => {
            out.push(TAG_FLOAT);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::String(s) => {
            out.push(TAG_STRING);
            put_str(out, s);
        }
        Value::Array(items) => {
            out.push(TAG_ARRAY);
            put_u32(out, items.len() as u32);
            for item in items {
                put_value(out, item);
            }
        }
        Value::Document(doc) => {
            out.push(TAG_DOCUMENT);
            put_document_body(out, doc);
        }
    }
}

/*
The document body without its tag byte.

Keys are written in SORTED order. A Document is a HashMap, whose iteration
order varies between runs, and a wire format that reorders its own fields is
miserable to diff, to test against, and to checksum. Sorting costs one small
sort per document and makes the encoding deterministic, which is the same
reason json.rs sorts its keys.
*/
pub fn put_document_body(out: &mut Vec<u8>, doc: &Document) {
    put_u32(out, doc.len() as u32);
    let mut keys: Vec<&String> = doc.keys().collect();
    keys.sort();
    for k in keys {
        put_str(out, k);
        put_value(out, &doc[k]);
    }
}

/* ---------- reading ---------- */

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> WireResult<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn u8(&mut self) -> WireResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32(&mut self) -> WireResult<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub fn i64(&mut self) -> WireResult<i64> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub fn f64(&mut self) -> WireResult<f64> {
        Ok(f64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    /*
    A length-prefixed string.

    The length is checked against what is ACTUALLY left in the frame before
    allocating, so a corrupt or hostile length field cannot make the server
    reserve gigabytes. take() does that check; this never pre-allocates from
    an untrusted number.
    */
    pub fn str(&mut self) -> WireResult<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        std::str::from_utf8(bytes)
            .map(|s| s.to_string())
            .map_err(|_| WireError::BadUtf8)
    }

    pub fn value(&mut self) -> WireResult<Value> {
        let tag = self.u8()?;
        Ok(match tag {
            TAG_NULL => Value::Null,
            TAG_FALSE => Value::Bool(false),
            TAG_TRUE => Value::Bool(true),
            TAG_INT => Value::Int(self.i64()?),
            TAG_FLOAT => Value::Float(self.f64()?),
            TAG_STRING => Value::String(self.str()?),
            TAG_ARRAY => {
                let n = self.u32()? as usize;
                // capacity is bounded by what is left in the frame: each value
                // costs at least one tag byte, so n cannot exceed remaining().
                if n > self.remaining() {
                    return Err(WireError::Truncated);
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.value()?);
                }
                Value::Array(items)
            }
            TAG_DOCUMENT => Value::Document(self.document_body()?),
            other => return Err(WireError::BadTag(other)),
        })
    }

    pub fn document_body(&mut self) -> WireResult<Document> {
        let n = self.u32()? as usize;
        if n > self.remaining() {
            return Err(WireError::Truncated);
        }
        let mut doc = Document::with_capacity(n);
        for _ in 0..n {
            let k = self.str()?;
            let v = self.value()?;
            doc.insert(k, v);
        }
        Ok(doc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) -> Value {
        let mut buf = Vec::new();
        put_value(&mut buf, &v);
        let got = Reader::new(&buf).value().unwrap();
        assert_eq!(got, v);
        got
    }

    #[test]
    fn test_every_value_variant_round_trips() {
        roundtrip(Value::Null);
        roundtrip(Value::Bool(true));
        roundtrip(Value::Bool(false));
        roundtrip(Value::Int(0));
        roundtrip(Value::Int(i64::MIN));
        roundtrip(Value::Int(i64::MAX));
        roundtrip(Value::Float(0.5));
        roundtrip(Value::Float(-1.0e300));
        roundtrip(Value::String(String::new()));
        roundtrip(Value::String("café ☕".into()));
        roundtrip(Value::Array(vec![Value::Int(1), Value::Null]));
    }

    #[test]
    fn test_large_i64_survives_exactly() {
        // the precision the JSON/text path has to be careful about is free
        // here: eight bytes in, eight bytes out, no decimal rendering.
        let big = (1i64 << 53) + 1;
        assert_eq!(roundtrip(Value::Int(big)), Value::Int(big));
    }

    #[test]
    fn test_document_encoding_is_deterministic() {
        let mut a = Document::new();
        a.insert("b".into(), Value::Int(2));
        a.insert("a".into(), Value::Int(1));
        let mut b = Document::new();
        b.insert("a".into(), Value::Int(1));
        b.insert("b".into(), Value::Int(2));

        let (mut ba, mut bb) = (Vec::new(), Vec::new());
        put_document_body(&mut ba, &a);
        put_document_body(&mut bb, &b);
        assert_eq!(ba, bb, "insertion order must not change the bytes");
    }

    #[test]
    fn test_nested_document_round_trips() {
        let mut inner = Document::new();
        inner.insert("kills".into(), Value::Int(7));
        let mut outer = Document::new();
        outer.insert("customMetrics".into(), Value::Document(inner));
        outer.insert("placeId".into(), Value::String("place-1".into()));
        roundtrip(Value::Document(outer));
    }

    #[test]
    fn test_truncated_frame_is_an_error_not_a_panic() {
        let mut buf = Vec::new();
        put_value(&mut buf, &Value::Int(42));
        for cut in 0..buf.len() {
            assert_eq!(Reader::new(&buf[..cut]).value(), Err(WireError::Truncated));
        }
    }

    #[test]
    fn test_hostile_length_does_not_allocate() {
        // a string claiming 4 GB inside a 9-byte frame must be rejected by the
        // bounds check, not by running out of memory.
        let mut buf = vec![TAG_STRING];
        put_u32(&mut buf, u32::MAX);
        buf.extend_from_slice(b"abcd");
        assert_eq!(Reader::new(&buf).value(), Err(WireError::Truncated));

        let mut buf = vec![TAG_ARRAY];
        put_u32(&mut buf, u32::MAX);
        assert_eq!(Reader::new(&buf).value(), Err(WireError::Truncated));
    }

    /*
    THE CROSS-IMPLEMENTATION FIXTURE.

    The other half of this protocol is Java, in SHAYVERI's AbpCodec, and there
    is no shared schema, no code generation, and nothing that fails at compile
    time if the two drift apart. Round-trip tests cannot catch that: an encoder
    and decoder written together will agree with each other even when both are
    wrong.

    These exact bytes are asserted on BOTH sides. AbpCodecTest has the same hex
    string in encodingMatchesTheRustSideByteForByte. Either implementation
    changing its mind fails a test instead of quietly corrupting documents.

    If this fails, do not paste in the new bytes until you know which side is
    right.
    */
    #[test]
    fn test_encoding_matches_the_java_side_byte_for_byte() {
        let mut metrics = Document::new();
        metrics.insert("kills".into(), Value::Int(7));

        let mut doc = Document::new();
        doc.insert("placeId".into(), Value::String("place-1".into()));
        doc.insert("playerCount".into(), Value::Int(42));
        doc.insert("serverFps".into(), Value::Float(58.5));
        doc.insert("customMetrics".into(), Value::Document(metrics));
        doc.insert("receivedAt".into(), Value::Int(1_754_500_000_000));

        let mut out = Vec::new();
        put_document_body(&mut out, &doc);
        let hex: String = out.iter().map(|b| format!("{b:02x}")).collect();

        assert_eq!(
            hex,
            "050000000d000000637573746f6d4d657472696373070100000005000000\
             6b696c6c73030700000000000000\
             07000000706c61636549640507000000706c6163652d31\
             0b000000706c61796572436f756e74032a00000000000000\
             0a0000007265636569766564417403006959809801000009000000\
             736572766572467073040000000000404d40"
                .replace([' ', '\n'], "")
        );
    }

    #[test]
    fn test_unknown_tag_is_reported() {
        assert_eq!(Reader::new(&[0x7f]).value(), Err(WireError::BadTag(0x7f)));
    }

    #[test]
    fn test_invalid_utf8_is_rejected() {
        let mut buf = vec![TAG_STRING];
        put_u32(&mut buf, 2);
        buf.extend_from_slice(&[0xff, 0xfe]);
        assert_eq!(Reader::new(&buf).value(), Err(WireError::BadUtf8));
    }
}
