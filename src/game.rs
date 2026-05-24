use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;
use webrtc::data_channel::RTCDataChannel;

use crate::room::{Room, RoomStatus};

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
    pub rooms: Vec<Room>,
    connections: HashMap<Uuid, ChannelPair>,
}

impl Default for Game {
    fn default() -> Self {
        Self {
            rooms: vec![],
            connections: HashMap::new(),
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
        let reliable = self
            .get_reliable_connection(player_id);
        let unreliable = self
            .get_unreliable_connection(player_id);
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

    pub fn new_room(
        &mut self,
        room_name: String,
        max_players: u8,
        password: Option<String>,
        tags: Vec<crate::room::RoomTag>,
    ) -> Uuid {
        let room = Room {
            status: RoomStatus::Waiting,
            id: Uuid::new_v4(),
            players: vec![],
            room_name,
            max_players,
            password,
            tags: tags,
        };
        let room_id = room.id;
        self.rooms.push(room);
        room_id
    }
}
