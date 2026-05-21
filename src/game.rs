use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

use crate::room::Room;

#[derive(Clone)]
pub struct Game {
    pub rooms: Vec<Room>,
    connections: HashMap<Uuid, Arc<webrtc::data_channel::RTCDataChannel>>,
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
    pub fn add_connection(
        &mut self,
        player_id: Uuid,
        dc: Arc<webrtc::data_channel::RTCDataChannel>,
    ) {
        self.connections.insert(player_id, dc);
    }

    pub fn remove_connection(&mut self, player_id: &Uuid) {
        self.connections.remove(player_id);
    }

    pub fn get_connection(
        &self,
        player_id: &Uuid,
    ) -> Option<Arc<webrtc::data_channel::RTCDataChannel>> {
        self.connections.get(player_id).cloned()
    }

    pub fn new_room(
        &mut self,
        room_name: String,
        max_players: u8,
        password: Option<String>,
    ) -> Uuid {
        let room = Room {
            id: Uuid::new_v4(),
            players: vec![],
            room_name,
            max_players,
            password,
        };
        let room_id = room.id;
        self.rooms.push(room);
        room_id
    }
}
