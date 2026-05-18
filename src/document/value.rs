/*
value.rs: the document value tree.

This is the Phase 2 "document model" from plans.txt. A top-level Document is
just a HashMap<String, Value>, and nested documents reuse the same type through
Value::Document.

We keep numbers simple for v1:
  - Int is signed 64-bit
  - Float is f64

That gives the rest of the database one shared data model without making the
storage layer care about structured data.
*/

use std::collections::HashMap;

pub type Document = HashMap<String, Value>;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Int(i64),
    Float(f64),
    String(String),
    Bool(bool),
    Null,
    Array(Vec<Value>),
    Document(Document),
}
