/*
document/mod.rs: Phase 2 document layer.

The folder is split by responsibility:
  - value.rs      in-memory document model
  - codec.rs      bytes <-> Document
  - collection.rs document API over HeapFile
  - catalog.rs    page-0 name -> collection root metadata

The pub use lines below make the common names available as crate::document::X
while still letting the files stay organized underneath.
*/

mod catalog;
mod codec;
mod collection;
mod key;
mod value;

pub use catalog::{Catalog, CatalogError, CatalogResult};
pub use codec::{
    deserialize_document, deserialize_value, serialize_document, serialize_value, DecodeError,
};
pub use collection::{Collection, CollectionError, CollectionResult};
pub use key::serialize_key;
pub use value::{Document, Value};
