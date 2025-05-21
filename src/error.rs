use std::time::Duration;
use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq)]
pub enum BufferError {
    #[error("Buffer is full")]
    Full,
    #[error("Buffer is empty")]
    Empty,
    #[error("Operation timed out after {0:?}")]
    Timeout(Duration),
    #[error("Maximum capacity of {0} reached")]
    MaxCapacityReached(usize),
    #[error("Failed to resize buffer: {0}")]
    ResizeError(String),
    #[error("Invalid operation: {0}")]
    InvalidOperation(String),
    #[error("Invalid configuration: {0}")]
    InvalidConfiguration(String),
    #[error("Other error: {0}")]
    Other(String),
}

impl BufferError {
    pub fn is_retriable(&self) -> bool {
        matches!(self, BufferError::Full | BufferError::Timeout(_))
    }

    pub fn is_capacity_error(&self) -> bool {
        matches!(self, BufferError::Full | BufferError::MaxCapacityReached(_))
    }
}

pub type BufferResult<T> = Result<T, BufferError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_retriable() {
        assert!(BufferError::Full.is_retriable());
        assert!(BufferError::Timeout(Duration::from_secs(1)).is_retriable());
        assert!(!BufferError::Empty.is_retriable());
        assert!(!BufferError::InvalidConfiguration("Test".into()).is_retriable());
    }

    #[test]
    fn test_is_capacity_error() {
        assert!(BufferError::Full.is_capacity_error());
        assert!(BufferError::MaxCapacityReached(1000).is_capacity_error());
        assert!(!BufferError::Empty.is_capacity_error());
        assert!(!BufferError::InvalidConfiguration("Test".into()).is_capacity_error());
    }

    #[test]
    fn test_error_messages() {
        assert_eq!(
            BufferError::Timeout(Duration::from_secs(5)).to_string(),
            "Operation timed out after 5s"
        );
        assert_eq!(
            BufferError::MaxCapacityReached(1000).to_string(),
            "Maximum capacity of 1000 reached"
        );
        assert_eq!(
            BufferError::InvalidConfiguration("Invalid min and max capacities".into()).to_string(),
            "Invalid configuration: Invalid min and max capacities"
        );
    }
}
