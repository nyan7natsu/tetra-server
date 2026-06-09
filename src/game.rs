use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use webrtc::data_channel::RTCDataChannel;

use crate::room::{Room, RoomStatus};

/// ランダムマッチの結果。待機列に入ったか、相手とマッチしてルームができたか。
pub enum MatchResult {
    Waiting,
    Matched { room_id: Uuid, opponent: Uuid },
}

#[derive(Clone)]
pub enum ConnectionState {
    Establishing,
    Connected,
    Disconnected,
}

#[derive(Clone)]
pub struct ChannelPair {
    pub reliable: Option<Arc<RTCDataChannel>>,
    pub unreliable: Option<Arc<RTCDataChannel>>,
    pub state: Arc<Mutex<ConnectionState>>,
}

#[derive(Clone)]
pub struct Game {
    pub rooms: HashMap<Uuid, Room>,
    connections: HashMap<Uuid, ChannelPair>,
    /// 経路表: player_id -> room_id の逆引き。中継時に
    /// player -> room -> opponent -> channel を高頻度(unreliable 30〜60Hz)で引くため。
    player_rooms: HashMap<Uuid, Uuid>,
    /// 友人参加用: ルームコード -> room_id の逆引き。
    room_codes: HashMap<String, Uuid>,
    /// ランダムマッチの待機列（FIFO）。(player_id, username, rule)。
    matchmaking_queue: VecDeque<(Uuid, String, String)>,
}

impl Default for Game {
    fn default() -> Self {
        Self {
            rooms: HashMap::new(),
            connections: HashMap::new(),
            player_rooms: HashMap::new(),
            room_codes: HashMap::new(),
            matchmaking_queue: VecDeque::new(),
        }
    }
}

impl Game {
    pub fn add_reliable_connection(
        &mut self,
        player_id: Uuid,
        dc: Arc<webrtc::data_channel::RTCDataChannel>,
    ) {
        let pair = self.connections.entry(player_id).or_insert(ChannelPair {
            reliable: None,
            unreliable: None,
            state: Arc::new(Mutex::new(ConnectionState::Establishing)),
        });
        pair.reliable = Some(dc);

        // 2本揃ったかチェック
        self.check_if_ready(player_id);
    }

    pub fn add_unreliable_connection(&mut self, player_id: Uuid, dc: Arc<RTCDataChannel>) {
        let pair = self.connections.entry(player_id).or_insert(ChannelPair {
            reliable: None,
            unreliable: None,
            state: Arc::new(Mutex::new(ConnectionState::Establishing)),
        });
        pair.unreliable = Some(dc);

        // 2本揃ったかチェック
        self.check_if_ready(player_id);
    }

    pub fn check_if_ready(&self, id: Uuid) {
        if let Some(pair) = self.connections.get(&id) {
            if pair.reliable.is_some() && pair.unreliable.is_some() {
                let mut state = pair.state.lock().unwrap();
                *state = ConnectionState::Connected;
                println!(
                    "🎉 プレイヤー [{}] の Reliable と Unreliable が両方揃いました！通信準備完了！",
                    id
                );
            }
        }
    }

    pub async fn remove_connection(&mut self, player_id: &Uuid) {
        let reliable = self.get_reliable_connection(player_id);
        let unreliable = self.get_unreliable_connection(player_id);
        if let Some(dc) = reliable {
            if let Err(e) = dc.close().await {
                eprintln!("Failed to close reliable data channel: {e}");
            }
        }
        if let Some(dc) = unreliable {
            if let Err(e) = dc.close().await {
                eprintln!("Failed to close unreliable data channel: {e}");
            }
        }
        self.connections.remove(player_id);
        // 待機列からも除外
        self.matchmaking_queue.retain(|(pid, _, _)| pid != player_id);
        // 経路表・所属ルームからも退去させる
        self.leave_room(player_id);
    }

    /// プレイヤーをルームに参加させる（players への追加＋経路表更新）。
    /// 部屋の存在・満員・開始済みをチェックする。パスワード検証は呼び出し側の責務。
    pub fn add_player_to_room(
        &mut self,
        room_id: Uuid,
        player_id: Uuid,
        username: String,
        rule: String,
    ) -> Result<(), String> {
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Game has already started".to_string());
        }
        if room.players.iter().any(|(pid, _, _)| *pid == player_id) {
            // すでに参加済み。冪等に成功扱い。
            return Ok(());
        }
        if room.players.len() as u8 >= room.max_players {
            return Err("Room is full".to_string());
        }
        room.players.push((player_id, username, rule));
        self.player_rooms.insert(player_id, room_id);
        Ok(())
    }

    /// プレイヤーを所属ルームの players から除き、経路表からも削除する。
    /// ルームが無人になったら、ルーム本体とコード索引も破棄する。
    pub fn leave_room(&mut self, player_id: &Uuid) {
        let Some(room_id) = self.player_rooms.remove(player_id) else {
            return;
        };
        let mut empty_code: Option<String> = None;
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.players.retain(|(pid, _, _)| pid != player_id);
            if room.players.is_empty() {
                empty_code = Some(room.code.clone());
            }
        }
        if let Some(code) = empty_code {
            self.rooms.remove(&room_id);
            self.room_codes.remove(&code);
        }
    }

    /// ルームコードから room_id を引く。
    pub fn find_room_by_code(&self, code: &str) -> Option<Uuid> {
        self.room_codes.get(code).copied()
    }

    /// 既存コードと衝突しない6桁の数字ルームコードを生成する。
    /// 英字（特に大文字 O/0・I/1 など）は数字と紛らわしいため、数字のみ・ゼロ埋め6桁にする。
    fn generate_room_code(&self) -> String {
        loop {
            let n = (Uuid::new_v4().as_u128() % 1_000_000) as u32;
            let code = format!("{n:06}");
            if !self.room_codes.contains_key(&code) {
                return code;
            }
        }
    }

    /// ランダムマッチ。待機列に相手がいればルームを作って両者を入れる。
    /// いなければ自身を待機列へ追加する。
    pub fn random_match(&mut self, player_id: Uuid, username: String, rule: String) -> MatchResult {
        if let Some(pos) = self
            .matchmaking_queue
            .iter()
            .position(|(pid, _, _)| *pid != player_id)
        {
            let (opp_id, opp_name, opp_rule) = self.matchmaking_queue.remove(pos).unwrap();
            // 待機していた相手をオーナーにしてルームを作成（相手は player[0] として入る）
            let (room_id, _code) = self.new_room(
                opp_id,
                "Random Match".to_string(),
                2,
                None,
                vec![],
                opp_name,
                opp_rule,
            );
            let _ = self.add_player_to_room(room_id, player_id, username, rule);
            MatchResult::Matched {
                room_id,
                opponent: opp_id,
            }
        } else {
            if !self
                .matchmaking_queue
                .iter()
                .any(|(pid, _, _)| *pid == player_id)
            {
                self.matchmaking_queue.push_back((player_id, username, rule));
            }
            MatchResult::Waiting
        }
    }

    /// ランダムマッチの待機列から離脱する。離脱できたら true。
    pub fn cancel_random_match(&mut self, player_id: &Uuid) -> bool {
        let before = self.matchmaking_queue.len();
        self.matchmaking_queue.retain(|(pid, _, _)| pid != player_id);
        before != self.matchmaking_queue.len()
    }

    /// player -> room -> opponent を引く（1対1前提: 同ルームの自分以外のプレイヤー）。
    pub fn get_opponent(&self, player_id: &Uuid) -> Option<Uuid> {
        let room_id = self.player_rooms.get(player_id)?;
        let room = self.rooms.get(room_id)?;
        room.players
            .iter()
            .map(|(pid, _, _)| *pid)
            .find(|pid| pid != player_id)
    }

    /// 相手の同種チャンネル（reliable=true なら reliable、false なら unreliable）を引く。
    /// 中継時はこれで得たチャンネルへ受信フレームをそのまま送る。
    pub fn get_opponent_channel(
        &self,
        player_id: &Uuid,
        reliable: bool,
    ) -> Option<Arc<RTCDataChannel>> {
        let opponent = self.get_opponent(player_id)?;
        if reliable {
            self.get_reliable_connection(&opponent)
        } else {
            self.get_unreliable_connection(&opponent)
        }
    }

    pub fn get_reliable_connection(
        &self,
        player_id: &Uuid,
    ) -> Option<Arc<webrtc::data_channel::RTCDataChannel>> {
        self.connections
            .get(player_id)
            .and_then(|pair| pair.reliable.clone())
    }

    pub fn get_unreliable_connection(
        &self,
        player_id: &Uuid,
    ) -> Option<Arc<webrtc::data_channel::RTCDataChannel>> {
        self.connections
            .get(player_id)
            .and_then(|pair| pair.unreliable.clone())
    }

    pub fn get_connection_state(&self, player_id: &Uuid) -> Option<Arc<Mutex<ConnectionState>>> {
        self.connections
            .get(player_id)
            .map(|pair| Arc::clone(&pair.state))
    }

    /// ルームを新規作成する。作成者(owner)は自動的に最初のプレイヤーとして参加する。
    /// 戻り値は (room_id, ルームコード)。
    pub fn new_room(
        &mut self,
        owner: Uuid,
        room_name: String,
        max_players: u8,
        password: Option<String>,
        tags: Vec<crate::room::RoomTag>,
        owner_username: String,
        owner_rule: String,
    ) -> (Uuid, String) {
        let room_id = Uuid::new_v4();
        let code = self.generate_room_code();
        let room = Room {
            status: RoomStatus::Waiting,
            owner,
            players: vec![(owner, owner_username, owner_rule)],
            room_name,
            max_players,
            password,
            tags,
            code: code.clone(),
        };
        self.rooms.insert(room_id, room);
        self.room_codes.insert(code.clone(), room_id);
        self.player_rooms.insert(owner, room_id);
        (room_id, code)
    }
}
