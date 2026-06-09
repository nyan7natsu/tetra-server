//! Payload 全体で共有するプリミティブ。
//! wincode のシリアライズ設定と、UUID をバイト列として扱うための `UuidBytes`。

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wincode::{SchemaRead, SchemaWrite};

pub fn wincode_config() -> impl wincode::config::Config {
    wincode::config::Configuration::default().with_varint_encoding()
}

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug, Clone, Copy)]
pub struct UuidBytes(pub [u8; 16]);

impl From<Uuid> for UuidBytes {
    fn from(value: Uuid) -> Self {
        Self(value.into_bytes())
    }
}

impl From<UuidBytes> for Uuid {
    fn from(value: UuidBytes) -> Self {
        Uuid::from_bytes(value.0)
    }
}
