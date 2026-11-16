//! The static topology of a cluster: which shard nodes exist and where they listen.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ShardInfo {
    pub id: u32,
    /// `host:port` of the shard's Arrow Flight service; for a replicated shard the addresses of all its replicas, separated by
    /// commas (the control node finds the leader)
    pub addr: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct ClusterConfig {
    pub shards: Vec<ShardInfo>,
}

impl ClusterConfig {
    /// `{"shards": [{"id": 0, "addr": "127.0.0.1:50051"}, ...]}`; the position in the list is the shard's number, so ids
    /// must be 0, 1, 2, ... in order.
    pub fn from_json(text: &str) -> Result<ClusterConfig, String> {
        let c: ClusterConfig = serde_json::from_str(text).map_err(|e| e.to_string())?;
        c.validate()?;
        Ok(c)
    }

    pub fn load(path: impl AsRef<std::path::Path>) -> Result<ClusterConfig, String> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| format!("{}: {}", path.as_ref().display(), e))?;
        ClusterConfig::from_json(&text)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.shards.is_empty() {
            return Err("the cluster has no shards".into());
        }
        for (i, s) in self.shards.iter().enumerate() {
            if s.id as usize != i {
                return Err(format!("shard ids must be 0, 1, 2, ... in order; entry {} has id {}", i, s.id));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_is_read_and_checked() {
        let c = ClusterConfig::from_json(r#"{"shards":[{"id":0,"addr":"a:1"},{"id":1,"addr":"b:2"}]}"#).unwrap();
        assert_eq!(c.shards.len(), 2);
        assert!(ClusterConfig::from_json(r#"{"shards":[{"id":1,"addr":"a:1"}]}"#).is_err());
        assert!(ClusterConfig::from_json(r#"{"shards":[]}"#).is_err());
    }
}
