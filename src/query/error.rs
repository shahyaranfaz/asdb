use std::fmt;

pub type QueryResult<T> = Result<T, QueryError>;

#[derive(Debug, PartialEq)]
pub enum QueryError {
    /*
    Arithmetic: the operands were the right TYPE but the operation has no
    representable answer. Separate from Type because the two want different
    fixes: Type means the query is wrong, Arithmetic means the data hit a
    limit. `total / 0` is a Type-correct query over int fields.
    */
    Arithmetic(String),
    /// A read or write below the query layer failed.
    Storage(String),
    InvalidPipeline(String),
    Type(String),
    Unsupported(String),
    Validation(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::Arithmetic(msg) => write!(f, "arithmetic error: {msg}"),
            QueryError::Storage(msg) => write!(f, "storage error: {msg}"),
            QueryError::InvalidPipeline(msg) => write!(f, "invalid pipeline: {msg}"),
            QueryError::Type(msg) => write!(f, "type error: {msg}"),
            QueryError::Unsupported(msg) => write!(f, "unsupported query: {msg}"),
            QueryError::Validation(msg) => write!(f, "validation error: {msg}"),
        }
    }
}

/*
A storage failure surfaces as a query failure. Stringified at the boundary
rather than nested, so QueryError stays comparable with PartialEq (which the
tests rely on) and does not have to depend on the database module's error
type structurally.
*/
impl From<crate::database::DatabaseError> for QueryError {
    fn from(err: crate::database::DatabaseError) -> Self {
        QueryError::Storage(format!("{err:?}"))
    }
}

impl std::error::Error for QueryError {}
