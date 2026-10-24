//! Machine identity for multi-machine history (which host recorded an event).

use std::fmt;
use std::str::FromStr;

use crate::error::MemoryError;
use serde::{Deserialize, Serialize};

/// Unique identifier for a node in a cluster topology.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(String);

impl NodeId {
    /// Construct and validate a node identifier.
    ///
    /// Allowed characters: ASCII alphanumeric, hyphens, underscores, and dots.
    /// Length must be between 1 and 64 characters.
    pub fn new(raw: impl Into<String>) -> Result<Self, MemoryError> {
        let raw = raw.into();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(MemoryError::MalformedRecord(
                "node_id cannot be empty".into(),
            ));
        }
        if trimmed.len() > 64 {
            return Err(MemoryError::MalformedRecord(
                "node_id exceeds maximum length of 64".into(),
            ));
        }
        if !trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(MemoryError::MalformedRecord(format!(
                "invalid characters in node_id: {trimmed}"
            )));
        }
        Ok(Self(trimmed.to_string()))
    }

    /// Return the string slice for this node identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("NodeId").field(&self.0).finish()
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for NodeId {
    type Err = MemoryError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_id_valid() {
        let id = NodeId::new("sleeper-01").unwrap();
        assert_eq!(id.as_str(), "sleeper-01");
        assert_eq!(id.to_string(), "sleeper-01");

        let id2 = NodeId::new("brokentooth.node_2").unwrap();
        assert_eq!(id2.as_str(), "brokentooth.node_2");
    }

    #[test]
    fn test_node_id_invalid() {
        assert!(NodeId::new("").is_err());
        assert!(NodeId::new("   ").is_err());
        assert!(NodeId::new("bad space").is_err());
        assert!(NodeId::new("bad/slash").is_err());
        assert!(NodeId::new("a".repeat(65)).is_err());
    }
}
