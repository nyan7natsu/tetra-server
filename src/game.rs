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

    /// プレイヤーを Game の内部状態（接続表・待機列・経路表）から取り除き、
    /// クローズすべき DataChannel を返す。
    ///
    /// `dc.close().await` は内部でネットワーク I/O を伴い時間がかかりうるため、
    /// ここでは close せずチャンネルを返すだけにする。Game は単一の `RwLock` で
    /// 共有されており、close を保持中の write ロック内で await すると
    /// その間ずっと全ルームの中継 read ロックまで含む Game 全操作がブロックされる。
    /// 呼び出し側は write ロックを解放してから返り値を `close().await` すること。
    #[must_use = "返された DataChannel はロック解放後に close すること"]
    pub fn remove_connection(&mut self, player_id: &Uuid) -> Vec<Arc<RTCDataChannel>> {
        let channels: Vec<Arc<RTCDataChannel>> = self
            .connections
            .remove(player_id)
            .map(|pair| pair.reliable.into_iter().chain(pair.unreliable).collect())
            .unwrap_or_default();
        // 待機列からも除外
        self.matchmaking_queue.retain(|(pid, _, _)| pid != player_id);
        // 経路表・所属ルームからも退去させる
        self.leave_room(player_id);
        channels
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

    /// プレイヤーの所属ルームを引く。切断削除の前にルームを控え、削除後に
    /// 残ったメンバーへ通知するために使う（削除後は経路表から消えて引けなくなる）。
    pub fn room_of(&self, player_id: &Uuid) -> Option<Uuid> {
        self.player_rooms.get(player_id).copied()
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

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::RwLock;

    /// 中継ホットパス(get_opponent_channel)が依存する経路引き get_opponent が
    /// 正しく相手を返すこと、退室で経路が消えることを確認する。
    #[test]
    fn get_opponent_routes_through_room() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();

        let (room_id, _code) = game.new_room(
            owner,
            "test".to_string(),
            2,
            None,
            vec![],
            "owner".to_string(),
            "tet".to_string(),
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");

        // 双方向に相手を引ける
        assert_eq!(game.get_opponent(&owner), Some(guest));
        assert_eq!(game.get_opponent(&guest), Some(owner));
        // 切断通知用に所属ルームも引ける
        assert_eq!(game.room_of(&guest), Some(room_id));

        // 退室すると経路が消える（中継先なし／所属ルームも消える）
        game.leave_room(&guest);
        assert_eq!(game.get_opponent(&owner), None);
        assert_eq!(game.room_of(&guest), None);
    }

    /// remove_connection は close すべきチャンネルを返すだけで、その場では await しない
    /// （ロックを保持したまま close.await すると全 Game 操作がブロックされるため）。
    /// 接続表・待機列・経路表からはちゃんと消えること、戻り値で close 対象を受け取れることを確認する。
    #[test]
    fn remove_connection_clears_state_and_returns_channels() {
        let mut game = Game::default();
        let owner = Uuid::new_v4();
        let guest = Uuid::new_v4();

        let (room_id, _code) = game.new_room(
            owner,
            "test".to_string(),
            2,
            None,
            vec![],
            "owner".to_string(),
            "tet".to_string(),
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");

        // DataChannel を持たない（reliable/unreliable とも None の）プレイヤーでも
        // 状態の掃除は行われ、閉じるチャンネルは無いので空ベクタが返る。
        let channels = game.remove_connection(&guest);
        assert!(channels.is_empty());

        // 経路表・ルームの players から消えている
        assert_eq!(game.room_of(&guest), None);
        assert_eq!(game.get_opponent(&owner), None);

        // owner を抜くと無人になりルームごと消える
        let _ = game.remove_connection(&owner);
        assert!(game.rooms.get(&room_id).is_none());
    }

    /// Issue #8 の核心: Game を RwLock で包むと「読み取りは並行・書き込みは排他」に
    /// なることを実体で確認する。中継(読み取り)が全ルーム並行に走れる根拠。
    #[test]
    fn rwlock_allows_concurrent_reads_but_exclusive_writes() {
        let lock = Arc::new(RwLock::new(Game::default()));

        // 複数の読み取りロックを同時に保持できる（= 複数ルームの中継が並行）
        let r1 = lock.try_read().expect("first read lock");
        let r2 = lock.try_read().expect("concurrent read lock");

        // 読み取り保持中は書き込みを取れない（排他）
        assert!(
            lock.try_write().is_err(),
            "writer must not acquire while readers hold the lock"
        );

        drop(r1);
        drop(r2);

        // 読み取りが全て解放されれば書き込みを取れる
        assert!(
            lock.try_write().is_ok(),
            "writer should acquire once all readers released"
        );
    }
}
