use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Debug, Copy)]
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
    /// ルームのオーナーID
    pub owner: Uuid,
    /// プレイヤー (ID, 名前, ルール"tet"|"puyo") のタプルの Vec。
    /// ルールは対戦前に相手へ伝える（ロビーで相手のテト/ぷよを表示する）ために保持する。
    pub players: Vec<(Uuid, String, String)>,
    /// 部屋の名前
    pub room_name: String,
    /// 最大プレイヤー数
    pub max_players: u8,
    /// ルームの公開設定（true=ルーム一覧に表示、false=非表示でコード参加のみ）
    pub is_public: bool,
    /// ルーム種別を表すタグ
    pub tags: Vec<RoomTag>,
    /// 友人参加用の短いルームコード（UUIDの代わりに共有する）
    pub code: String,
    /// マッチ設定（vs_settings.js の設定を JSON シリアライズした文字列）。デフォルト "{}"。
    pub match_setting: String,
    /// 対戦中の生存プレイヤー ID リスト。start_match 時に全員で初期化され、ゲームオーバーごとに削除。
    pub alive_players: Vec<Uuid>,
    /// READY 済みプレイヤー ID リスト。マッチ開始/終了・入退室でリセットされる。
    pub ready_players: Vec<Uuid>,
    /// 各プレイヤーの RTT (ms)。クライアントからの自己報告値。
    pub pings: HashMap<Uuid, u32>,
}
