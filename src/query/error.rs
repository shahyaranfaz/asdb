use std::fmt;

pub type QueryResult<T> = Result<T, QueryError>;

#[derive(Debug, PartialEq)]
pub enum QueryError {
    InvalidPipeline(String),
    Type(String),
    Unsupported(String),
    Validation(String),
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryError::InvalidPipeline(msg) => write!(f, "invalid pipeline: {msg}"),
            QueryError::Type(msg) => write!(f, "type error: {msg}"),
            QueryError::Unsupported(msg) => write!(f, "unsupported query: {msg}"),
            QueryError::Validation(msg) => write!(f, "validation error: {msg}"),
        }
    }
}

impl std::error::Error for QueryError {}
