use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
pub struct Room {
    /// 部屋の内部ID
    pub id: Uuid,
    /// プレイヤーIDリスト
    pub players: Vec<Uuid>,
    /// 部屋の名前
    pub room_name: String,
    /// 最大プレイヤー数
    pub max_players: u8,
    /// パスワードが設定されている場合はSome、そうでない場合はNone
    pub password: Option<String>,
}
