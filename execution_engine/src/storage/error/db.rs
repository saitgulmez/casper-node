use std::sync;

use lmdb as lmdb_external;
use thiserror::Error;

use casper_types::bytesrepr;

use crate::storage::{error::in_memory, global_state::CommitError};

/// Error enum representing possible errors from database internals.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum DbError {
    /// LMDB error returned from underlying `lmdb` crate.
    #[error(transparent)]
    Lmdb(#[from] lmdb_external::Error),
}

/// Error enum representing possible error states in DB interactions.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum Error {
    /// Error from the underlying database.
    #[error(transparent)]
    Db(#[from] DbError),

    /// Error when we cannot open a column family.
    #[error("unable to open column family {0}")]
    UnableToOpenColumnFamily(String),

    /// (De)serialization error.
    #[error("{0}")]
    BytesRepr(bytesrepr::Error),

    /// Concurrency error.
    #[error("Another thread panicked while holding a lock")]
    Poison,

    /// Error committing to execution engine.
    #[error(transparent)]
    CommitError(#[from] CommitError),
}

impl wasmi::HostError for Error {}

impl From<lmdb_external::Error> for Error {
    fn from(error: lmdb_external::Error) -> Self {
        Error::Db(DbError::Lmdb(error))
    }
}

impl From<bytesrepr::Error> for Error {
    fn from(error: bytesrepr::Error) -> Self {
        Error::BytesRepr(error)
    }
}

impl<T> From<sync::PoisonError<T>> for Error {
    fn from(_error: sync::PoisonError<T>) -> Self {
        Error::Poison
    }
}

impl From<in_memory::Error> for Error {
    fn from(error: in_memory::Error) -> Self {
        match error {
            in_memory::Error::BytesRepr(error) => Error::BytesRepr(error),
            in_memory::Error::Poison => Error::Poison,
        }
    }
}