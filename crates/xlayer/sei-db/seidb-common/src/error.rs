use thiserror::Error;

#[derive(Error, Debug)]
pub enum SeiDbError {
    #[error("key empty")]
    KeyEmpty,
    #[error("record not found")]
    RecordNotFound,
    #[error("start key after end key")]
    StartAfterEnd,
    #[error("export is complete")]
    ExportDone,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("malformed EVM key: {0}")]
    MalformedEvmKey(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("rocksdb error: {0}")]
    RocksDb(String),
    #[error(transparent)]
    Proto(#[from] prost::DecodeError),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, SeiDbError>;

pub fn is_not_found(err: &SeiDbError) -> bool {
    matches!(err, SeiDbError::NotFound(_) | SeiDbError::RecordNotFound)
}

/// Aggregates multiple errors into a single error. Returns None if all errors are absent.
pub fn join_errors(errs: Vec<SeiDbError>) -> Option<SeiDbError> {
    let msgs: Vec<String> = errs.into_iter().map(|e| e.to_string()).collect();
    if msgs.is_empty() {
        None
    } else if msgs.len() == 1 {
        Some(SeiDbError::Other(msgs.into_iter().next().unwrap()))
    } else {
        Some(SeiDbError::Other(msgs.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        assert_eq!(SeiDbError::KeyEmpty.to_string(), "key empty");
        assert_eq!(SeiDbError::RecordNotFound.to_string(), "record not found");
        assert_eq!(SeiDbError::StartAfterEnd.to_string(), "start key after end key");
        assert_eq!(SeiDbError::ExportDone.to_string(), "export is complete");
        assert_eq!(SeiDbError::NotFound("foo".into()).to_string(), "not found: foo");
        assert_eq!(SeiDbError::MalformedEvmKey("bad".into()).to_string(), "malformed EVM key: bad");
        assert_eq!(SeiDbError::RocksDb("oops".into()).to_string(), "rocksdb error: oops");
        assert_eq!(SeiDbError::Other("misc".into()).to_string(), "misc");
    }

    #[test]
    fn test_is_not_found() {
        assert!(is_not_found(&SeiDbError::NotFound("x".into())));
        assert!(is_not_found(&SeiDbError::RecordNotFound));
        assert!(!is_not_found(&SeiDbError::KeyEmpty));
        assert!(!is_not_found(&SeiDbError::Other("not found: y".into())));
    }

    #[test]
    fn test_join_errors_empty() {
        assert!(join_errors(vec![]).is_none());
    }

    #[test]
    fn test_join_errors_single() {
        let result = join_errors(vec![SeiDbError::KeyEmpty]);
        assert!(result.is_some());
        assert_eq!(result.unwrap().to_string(), "key empty");
    }

    #[test]
    fn test_join_errors_multiple() {
        let result =
            join_errors(vec![SeiDbError::KeyEmpty, SeiDbError::NotFound("bar".into())]).unwrap();
        assert_eq!(result.to_string(), "key empty\nnot found: bar");
    }
}
