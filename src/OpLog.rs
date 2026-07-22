pub mod oplog {
    use std::collections::HashSet;
    use std::fmt::{Display, Formatter};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    pub struct OpLog {
        pub type_: String,
        pub idempotency_key: String,
        pub timestamp: SystemTime,
        pub payload: serde_json::Value,
        pub source_partition_id: String,
    }

    impl Display for OpLog {
        fn fmt(&self, f: Formatter<'_>) -> Self::RuntimeError {
            format!("OpLog({:?}, {}, {}, {}, {})", 
                self.type_, 
                self.idempotency_key, 
                self.timestamp.into_std(), 
                self.payload, 
                self.source_partition_id)
        }
    }

    impl TryFrom<serde_json::Value> for OpLog {
        type Error = serde_json::Error;
        fn try_from(value: serde_json::Value) -> Result<Self, Self::Error> {
            Ok(OpLog {
                type_: value["type"].str().unwrap_or_default().to_string(),
                idempotency_key: value["idempotency_key"].str().unwrap_or_default().to_string(),
                timestamp: SystemTime::now().into(),
                payload: value,
                source_partition_id: value["source_partition_id"].str().unwrap_or_default().to_string(),
            })
        }
    }

    impl OpLog {
        pub fn new(
            type_: String,
            idempotency_key: String,
            payload: serde_json::Value,
            source_partition_id: String,
        ) -> Self {
            OpLog {
                type_,
                idempotency_key,
                timestamp: SystemTime::now().into(),
                payload,
                source_partition_id,
            }
        }

        pub fn validate_idempotency_key(key: &str) -> Result<(), String> {
            // Simple uniqueness check (in practice, use a database)
            if key.is_empty() {
                return Err("IDempotency key cannot be empty".to_string());
            }
            Ok(())
        }

        pub fn serialize(&self) -> serde_json::Value {
            self.payload.clone()
        }
    }
