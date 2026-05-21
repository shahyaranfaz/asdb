use crate::document::{CatalogError, CollectionError};

pub type DatabaseResult<T> = Result<T, DatabaseError>;

#[derive(Debug)]
pub enum DatabaseError {
    IO(std::io::Error),
    Catalog(CatalogError),
    Collection(CollectionError),
}

impl From<std::io::Error> for DatabaseError {
    fn from(err: std::io::Error) -> Self {
        DatabaseError::IO(err)
    }
}

impl From<CatalogError> for DatabaseError {
    fn from(err: CatalogError) -> Self {
        DatabaseError::Catalog(err)
    }
}

impl From<CollectionError> for DatabaseError {
    fn from(err: CollectionError) -> Self {
        DatabaseError::Collection(err)
    }
}
