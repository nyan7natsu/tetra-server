use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Debug)]
#[repr(u32)]
#[allow(dead_code)]
pub enum RoomTag {
    PuyoTet = 0,
    PuyoOnly = 1,
    TetOnly = 2,
    Casual = 3,
    Competitive = 4,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
#[derive(PartialEq)]
pub enum RoomStatus {
    Waiting,
    Playing,
}

#[derive(Clone)]
pub struct Room {
    /// 部屋の状態
    pub status: RoomStatus,
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
    /// ルーム種別を表すタグ
    pub tags: Vec<RoomTag>,
}
