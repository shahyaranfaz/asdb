/*
json.rs: serialize a Value or a QueryOutput as JSON.

WRITER ONLY, NO PARSER. That asymmetry is deliberate and it is what keeps this
file small. The wire protocol (see server.rs) takes ASL TEXT as the request
body and returns JSON as the response, so nothing ever has to read JSON back
in. ASL is already a parser we own; adding a second input grammar to parse
would be work with no caller.

Hand-written because asdb has no dependencies and this does not justify
adding serde. The whole surface is six Value variants.

The output shape is fixed by asl.txt RESULT FORMAT:

    { "count": <n>, "documents": [ ... ] }     a query
    { "affected": <n> }                        a mutation
    { "error": "...", "stage": "..." }         a failure
*/

use crate::document::{Document, Value};
use crate::query::QueryOutput;

/*
write_output: a finished query as its JSON response body.

Documents keys are sorted. A Document is a HashMap, whose iteration order
varies between runs, and an HTTP response that reorders its own fields at
random is miserable to test against and to diff. Sorting costs nothing here
and makes the output reproducible.
*/
pub fn write_output(output: &QueryOutput) -> String {
    match output {
        QueryOutput::Documents(docs) => {
            let mut out = String::from("{\"count\":");
            out.push_str(&docs.len().to_string());
            out.push_str(",\"documents\":[");
            for (i, doc) in docs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_document(&mut out, doc);
            }
            out.push_str("]}");
            out
        }
        QueryOutput::Affected(n) => format!("{{\"affected\":{n}}}"),
    }
}

/*
write_error: a failure as its JSON response body.

`stage` is part of asl.txt's error shape. It is carried as a plain string
rather than an index because the caller is a Java backend logging it, not
something computing on it.
*/
pub fn write_error(message: &str, stage: &str) -> String {
    let mut out = String::from("{\"error\":");
    write_string(&mut out, message);
    out.push_str(",\"stage\":");
    write_string(&mut out, stage);
    out.push('}');
    out
}

pub fn write_document(out: &mut String, doc: &Document) {
    let mut keys: Vec<&String> = doc.keys().collect();
    keys.sort(); // deterministic output, see the note above
    out.push('{');
    for (i, key) in keys.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_string(out, key);
        out.push(':');
        write_value(out, &doc[*key]);
    }
    out.push('}');
}

pub fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int(n) => out.push_str(&n.to_string()),

        /*
        JSON has no way to write NaN or Infinity: the grammar simply does not
        include them, and emitting the bare words produces a document no
        conforming parser will accept. They become null, which every client
        can at least read. This is the same choice most JSON libraries make.

        A finite float is written via Rust's shortest round-trip formatting,
        except that a whole number needs an explicit ".0" or it would come
        back as an integer on the other side and lose its type.
        */
        Value::Float(x) => {
            if x.is_finite() {
                if x.fract() == 0.0 && x.abs() < 1e15 {
                    out.push_str(&format!("{x:.1}"));
                } else {
                    out.push_str(&x.to_string());
                }
            } else {
                out.push_str("null");
            }
        }

        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Document(doc) => write_document(out, doc),
    }
}

/*
write_string: a JSON string literal, escaped.

The control-character case matters more than it looks. JSON forbids raw bytes
below 0x20 inside a string, so a tab or a newline arriving in user data would
otherwise produce output that fails to parse on the client. Anything without a
short escape gets the \u00XX form.
*/
fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_of(pairs: &[(&str, Value)]) -> Document {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    fn rendered(value: Value) -> String {
        let mut out = String::new();
        write_value(&mut out, &value);
        out
    }

    #[test]
    fn test_scalars() {
        assert_eq!(rendered(Value::Null), "null");
        assert_eq!(rendered(Value::Bool(true)), "true");
        assert_eq!(rendered(Value::Int(-5)), "-5");
        assert_eq!(rendered(Value::String("hi".into())), "\"hi\"");
    }

    #[test]
    fn test_whole_floats_keep_their_type() {
        // without the .0 this would read back as an integer on the client.
        assert_eq!(rendered(Value::Float(3.0)), "3.0");
        assert_eq!(rendered(Value::Float(0.5)), "0.5");
    }

    #[test]
    fn test_non_finite_floats_become_null() {
        // JSON has no NaN or Infinity literal; emitting one produces output no
        // conforming parser accepts.
        assert_eq!(rendered(Value::Float(f64::NAN)), "null");
        assert_eq!(rendered(Value::Float(f64::INFINITY)), "null");
    }

    #[test]
    fn test_string_escaping() {
        assert_eq!(rendered(Value::String("a\"b".into())), r#""a\"b""#);
        assert_eq!(rendered(Value::String("a\\b".into())), r#""a\\b""#);
        assert_eq!(rendered(Value::String("a\nb".into())), r#""a\nb""#);
        // a raw control byte is illegal inside a JSON string, so it has to be
        // escaped rather than passed through
        assert_eq!(rendered(Value::String("a\u{1}b".into())), "\"a\\u0001b\"");
    }

    #[test]
    fn test_non_ascii_passes_through() {
        // valid UTF-8 needs no escaping, and escaping it would only bloat the
        // response.
        assert_eq!(rendered(Value::String("café".into())), "\"café\"");
    }

    #[test]
    fn test_document_keys_are_sorted() {
        let doc = doc_of(&[("b", Value::Int(2)), ("a", Value::Int(1))]);
        let mut out = String::new();
        write_document(&mut out, &doc);
        assert_eq!(out, r#"{"a":1,"b":2}"#);
    }

    #[test]
    fn test_nested_structures() {
        let inner = doc_of(&[("x", Value::Int(1))]);
        let value = Value::Array(vec![Value::Document(inner), Value::Int(2)]);
        assert_eq!(rendered(value), r#"[{"x":1},2]"#);
    }

    #[test]
    fn test_output_shapes_match_the_spec() {
        let docs = QueryOutput::Documents(vec![doc_of(&[("a", Value::Int(1))])]);
        assert_eq!(write_output(&docs), r#"{"count":1,"documents":[{"a":1}]}"#);

        assert_eq!(write_output(&QueryOutput::Affected(3)), r#"{"affected":3}"#);
        assert_eq!(
            write_error("bad", "where"),
            r#"{"error":"bad","stage":"where"}"#
        );
    }

    #[test]
    fn test_empty_result_is_still_well_formed() {
        assert_eq!(
            write_output(&QueryOutput::Documents(vec![])),
            r#"{"count":0,"documents":[]}"#
        );
    }
}
