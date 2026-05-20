use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
pub struct JoinRoomPayload {
    op: u8,
    room_id: u32,
    user_name: String,
}

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
#[serde(tag = "op")]
pub enum GameMessage {
    #[serde(rename = "16")]
    JoinRoom(JoinRoomPayload),
    #[serde(rename = "32")]
    Attack { power: u8 },
}
