//! Payload 定義。Issue #1（Payload処理コードの分割化）に沿ってファイル分割している。
//!
//! - [`common`] : Payload 共通のプリミティブ（wincode 設定 / `UuidBytes`）
//! - [`schema`] : バイナリフレーム層（`Opcode` / `*Payload` / `GameMessage`）
//! - [`json`]   : Reliable な JSON 制御メッセージ層（`JsonMessage` / `JSON*` / `ListRoomInfo`）
//!
//! 各サブモジュールの公開要素はここで re-export しているため、利用側は従来どおり
//! `payload::Opcode` や `payload::JsonMessage` のようにフラットに参照できる。

mod common;
mod json;
mod schema;

pub use common::*;
pub use json::*;
pub use schema::*;
