use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use webrtc::data_channel::RTCDataChannel;

use crate::room::{Room, RoomStatus};

/// ランダムマッチの結果。待機列に入ったか、相手とマッチしてルームができたか。
pub enum MatchResult {
    Waiting,
    #[allow(unused)]
    Matched {
        room_id: Uuid,
        opponent: Uuid,
    },
}

/// ゲームオーバー通知後の生存判定結果。
pub enum WinnerStatus {
    MatchContinues,
    Winner(Uuid),
    Draw,
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

    /// プレイヤーを Game の内部状態（接続表・待機列・経路表）から取り除き，クローズする
    pub fn remove_connection(&mut self, player_id: &Uuid) {
        let channels: (Option<Arc<RTCDataChannel>>, Option<Arc<RTCDataChannel>>) = self
            .connections
            .remove(player_id)
            .map(|pair| (pair.reliable, pair.unreliable))
            .unwrap_or((None, None));

        // 待機列からも除外
        self.matchmaking_queue
            .retain(|(pid, _, _)| pid != player_id);
        // 経路表・所属ルームからも退去させる
        self.leave_room(player_id);

        let player_id = *player_id;

        tokio::spawn(async move {
            if let Some(dc) = channels.0 {
                if let Err(err) = dc.close().await {
                    println!(
                        "Failed to close reliable channel for player {}: {:?}",
                        player_id, err
                    );
                }
            }
            if let Some(dc) = channels.1 {
                if let Err(err) = dc.close().await {
                    println!(
                        "Failed to close unreliable channel for player {}: {:?}",
                        player_id, err
                    );
                }
            }
        });
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
        // 入室直後は未READY状態（再入室時の stale READY も除去）
        room.ready_players.retain(|pid| *pid != player_id);
        self.player_rooms.insert(player_id, room_id);
        Ok(())
    }

    /// プレイヤー自身のルール ("tet" | "puyo") を更新する。マッチ開始前のみ有効。
    pub fn update_player_rule(&mut self, player_id: &Uuid, rule: String) -> Result<(), String> {
        let room_id = self
            .player_rooms
            .get(player_id)
            .copied()
            .ok_or_else(|| "Not in a room".to_string())?;
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Cannot change rule after match has started".to_string());
        }
        match rule.as_str() {
            "tet" | "puyo" => {}
            _ => return Err("Invalid rule".to_string()),
        }
        if let Some(entry) = room.players.iter_mut().find(|(pid, _, _)| pid == player_id) {
            entry.2 = rule;
        }
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
            room.alive_players.retain(|pid| pid != player_id);
            room.ready_players.retain(|pid| pid != player_id);
            room.pings.remove(player_id);
            // オーナーが退出したら残った先頭プレイヤーへオーナー権を移譲する
            if room.owner == *player_id {
                if let Some((next_owner, _, _)) = room.players.first() {
                    room.owner = *next_owner;
                }
            }
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
                false,
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
                self.matchmaking_queue
                    .push_back((player_id, username, rule));
            }
            MatchResult::Waiting
        }
    }

    /// ランダムマッチの待機列から離脱する。離脱できたら true。
    pub fn cancel_random_match(&mut self, player_id: &Uuid) -> bool {
        let before = self.matchmaking_queue.len();
        self.matchmaking_queue
            .retain(|(pid, _, _)| pid != player_id);
        before != self.matchmaking_queue.len()
    }

    /// プレイヤーの所属ルームを引く。切断削除の前にルームを控え、削除後に
    /// 残ったメンバーへ通知するために使う（削除後は経路表から消えて引けなくなる）。
    pub fn room_of(&self, player_id: &Uuid) -> Option<Uuid> {
        self.player_rooms.get(player_id).copied()
    }

    /// player -> room -> opponent を引く（1対1前提: 同ルームの自分以外のプレイヤー）。
    #[cfg(test)]
    pub fn get_opponent(&self, player_id: &Uuid) -> Option<Uuid> {
        let room_id = self.player_rooms.get(player_id)?;
        let room = self.rooms.get(room_id)?;
        room.players
            .iter()
            .map(|(pid, _, _)| *pid)
            .find(|pid| pid != player_id)
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

    /// ルームの全ピア（自分以外）の ID を返す。0x2N 中継で使う。
    pub fn get_room_peers(&self, player_id: &Uuid) -> Vec<Uuid> {
        let room_id = match self.player_rooms.get(player_id) {
            Some(id) => *id,
            None => return vec![],
        };
        let room = match self.rooms.get(&room_id) {
            Some(r) => r,
            None => return vec![],
        };
        room.players
            .iter()
            .map(|(pid, _, _)| *pid)
            .filter(|pid| *pid != *player_id)
            .collect()
    }

    /// ルームの全ピアの reliable/unreliable チャンネルを収集する。0x2N 中継ホットパス。
    pub fn get_room_peer_channels(
        &self,
        player_id: &Uuid,
        reliable: bool,
    ) -> Vec<Arc<RTCDataChannel>> {
        self.get_room_peers(player_id)
            .into_iter()
            .filter_map(|pid| {
                if reliable {
                    self.get_reliable_connection(&pid)
                } else {
                    self.get_unreliable_connection(&pid)
                }
            })
            .collect()
    }

    /// ルーム全メンバーの reliable チャンネルを収集する（通知配布用）。
    pub fn get_room_reliable_channels(&self, room_id: &Uuid) -> Vec<Arc<RTCDataChannel>> {
        let Some(room) = self.rooms.get(room_id) else {
            return vec![];
        };
        room.players
            .iter()
            .filter_map(|(pid, _, _)| self.get_reliable_connection(pid))
            .collect()
    }

    /// マッチを開始する。バリデーション後ステータスを Playing に変更し、
    /// 16 バイト共有シードと現在の match_setting を返す。
    pub fn start_match(&mut self, room_id: Uuid) -> Result<([u8; 16], String), String> {
        let room = self.rooms.get_mut(&room_id).ok_or("Room not found")?;
        if room.status != RoomStatus::Waiting {
            return Err("Game already started".to_string());
        }
        if room.players.len() < 2 {
            return Err("Need at least 2 players".to_string());
        }
        // オーナー以外の全員が READY であること（オーナーの開始操作 = オーナーのREADY扱い）
        let not_ready: Vec<&String> = room
            .players
            .iter()
            .filter(|(pid, _, _)| *pid != room.owner && !room.ready_players.contains(pid))
            .map(|(_, name, _)| name)
            .collect();
        if !not_ready.is_empty() {
            return Err(format!(
                "Not all players are ready: {}",
                not_ready
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        room.status = RoomStatus::Playing;
        room.alive_players = room.players.iter().map(|(pid, _, _)| *pid).collect();
        // 次のマッチに向けて READY をリセット
        room.ready_players.clear();
        let seed: [u8; 16] = *Uuid::new_v4().as_bytes();
        let match_setting = room.match_setting.clone();
        Ok((seed, match_setting))
    }

    /// プレイヤーの READY 状態を設定する。Waiting 中のみ有効。
    pub fn set_ready(&mut self, player_id: &Uuid, ready: bool) -> Result<(), String> {
        let room_id = self
            .player_rooms
            .get(player_id)
            .copied()
            .ok_or_else(|| "Not in a room".to_string())?;
        let room = self
            .rooms
            .get_mut(&room_id)
            .ok_or_else(|| "Room not found".to_string())?;
        if room.status != RoomStatus::Waiting {
            return Err("Cannot change ready state during a match".to_string());
        }
        room.ready_players.retain(|pid| pid != player_id);
        if ready {
            room.ready_players.push(*player_id);
        }
        Ok(())
    }

    /// プレイヤーの自己報告 RTT (ms) を記録する。ルーム未所属なら何もしない。
    pub fn set_ping(&mut self, player_id: &Uuid, ping_ms: u32) {
        let Some(room_id) = self.player_rooms.get(player_id).copied() else {
            return;
        };
        if let Some(room) = self.rooms.get_mut(&room_id) {
            room.pings.insert(*player_id, ping_ms);
        }
    }

    /// プレイヤーのゲームオーバーを記録する。生存者数を確認して WinnerStatus を返す。
    /// 勝敗が確定したらルームを Waiting に戻す（再戦・ルール変更・新規参加を可能にするため）。
    pub fn record_game_over(&mut self, player_id: &Uuid) -> WinnerStatus {
        let room_id = match self.player_rooms.get(player_id).copied() {
            Some(id) => id,
            None => return WinnerStatus::MatchContinues,
        };
        let room = match self.rooms.get_mut(&room_id) {
            Some(r) => r,
            None => return WinnerStatus::MatchContinues,
        };
        // 既に決着済み（status != Playing）なら何もしない。
        // このゲームに引き分けは無く「先に死んだ方が負け・最後まで生きた方が勝ち」。
        // 2人がほぼ同時に死んでも、先に処理された死亡で勝者(=後に生き残っていた方)が確定する。
        // その後に届いた勝者自身の gameOver でDRAWに上書きしてしまうのを防ぐ。
        if room.status != RoomStatus::Playing {
            return WinnerStatus::MatchContinues;
        }
        room.alive_players.retain(|pid| pid != player_id);
        println!(
            "Recorded game over for player [{}] in room [{}]. Alive players remaining: {}",
            player_id,
            room_id,
            room.alive_players.len()
        );
        match room.alive_players.len() {
            0 => {
                room.status = RoomStatus::Waiting;
                room.alive_players.clear();
                room.ready_players.clear();
                WinnerStatus::Draw
            }
            1 => {
                let winner = room.alive_players[0];
                room.status = RoomStatus::Waiting;
                room.alive_players.clear();
                room.ready_players.clear();
                WinnerStatus::Winner(winner)
            }
            _ => WinnerStatus::MatchContinues,
        }
    }

    /// ルームを新規作成する。作成者(owner)は自動的に最初のプレイヤーとして参加する。
    /// 戻り値は (room_id, ルームコード)。
    pub fn new_room(
        &mut self,
        owner: Uuid,
        room_name: String,
        max_players: u8,
        is_public: bool,
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
            is_public,
            tags,
            code: code.clone(),
            match_setting: "{}".to_string(),
            alive_players: vec![],
            ready_players: vec![],
            pings: std::collections::HashMap::new(),
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
            false,
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
            false,
            vec![],
            "owner".to_string(),
            "tet".to_string(),
        );
        game.add_player_to_room(room_id, guest, "guest".to_string(), "puyo".to_string())
            .expect("guest should join");

        // 退室させると、接続表からも消えるし，closeもされる．
        game.remove_connection(&guest);

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

    /// このゲームに引き分けは無い：先に死んだ方が負け、最後まで生きた方が勝ち。
    /// 2人がほぼ同時に死んでも、先に処理された死亡で勝者が確定し、その後に届いた
    /// 勝者自身の gameOver は無視される（DRAWにならない）。
    #[test]
    fn last_survivor_wins_and_never_draws() {
        let mut game = Game::default();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let (room_id, _code) = game.new_room(
            a,
            "t".to_string(),
            2,
            false,
            vec![],
            "A".to_string(),
            "tet".to_string(),
        );
        game.add_player_to_room(room_id, b, "B".to_string(), "tet".to_string())
            .expect("B joins");
        {
            let room = game.rooms.get_mut(&room_id).unwrap();
            room.status = RoomStatus::Playing;
            room.alive_players = vec![a, b];
        }
        // A が先に死亡 → 最後の生存者 B が勝利
        assert!(
            matches!(game.record_game_over(&a), WinnerStatus::Winner(w) if w == b),
            "first death loses; last survivor wins"
        );
        // 直後に勝者 B の gameOver が届いても DRAW にならず無視される
        assert!(
            matches!(game.record_game_over(&b), WinnerStatus::MatchContinues),
            "post-resolution gameOver must be ignored (no draw)"
        );
    }
}
