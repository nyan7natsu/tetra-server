use serde::{Deserialize, Serialize};
use uuid::Uuid;
use wincode::{SchemaRead, SchemaWrite};

pub fn wincode_config() -> impl wincode::config::Config {
    wincode::config::Configuration::default().with_varint_encoding()
}

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug, Clone, Copy)]
pub struct UuidBytes(pub [u8; 16]);

impl From<Uuid> for UuidBytes {
    fn from(value: Uuid) -> Self {
        Self(value.into_bytes())
    }
}

impl From<UuidBytes> for Uuid {
    fn from(value: UuidBytes) -> Self {
        Uuid::from_bytes(value.0)
    }
}

macro_rules! payload {
    ($name:ident) => {
        pastey::paste! {
            #[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
            pub struct [<$name Payload>] { }

            impl [<$name Payload>] {
                #[allow(dead_code)]
                pub fn to_binary(&self) -> Result<Vec<u8>, wincode::WriteError> {
                    wincode::config::serialize(self, crate::payload::wincode_config())
                }
            }
        }
    };
    ($name:ident, $(($key:ident, $type:ty)),*) => {
        pastey::paste! {
            #[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
            pub struct [<$name Payload>] {
                $(pub $key: $type),*
            }

            impl [<$name Payload>] {
                #[allow(dead_code)]
                pub fn to_binary(&self) -> Result<Vec<u8>, wincode::WriteError> {
                    wincode::config::serialize(self, crate::payload::wincode_config())
                }
            }
        }
    };
}

payload!(JSONRequest, (data, String));
payload!(JSONResponse, (data, String));
payload!(Ping, (id, UuidBytes));
payload!(Pong, (id, UuidBytes));

macro_rules! jpayload {
    ($name:ident) => {
        pastey::paste! {
            /**
             * Payloadの`data`の中身をparseした構造体
             */
            #[derive(Serialize, Deserialize, Debug)]
            #[allow(dead_code)]
            pub struct [<JSON $name>] { }

            impl [<JSON $name>] {
                /**
                 * 自身をstringifyしてPayloadに変換する関数
                 */
                #[allow(dead_code)]
                pub fn to_payload(&self) -> Result<JSONResponsePayload, serde_json::Error> {
                    Ok(JSONResponsePaload { data: serde_json::to_string(self)? })
                }
            }
        }
    };
    ($name:ident, $(($key:ident, $type:ty)),*) => {
        pastey::paste! {
            /**
             * Payloadの`data`の中身をシリアライズした構造体
             */
            #[derive(Serialize, Deserialize, Debug)]
            #[allow(dead_code)]
            pub struct [<JSON $name>] {
                $(pub $key: $type),*
            }

            impl [<JSON $name>] {
                /**
                 * 自身をstringifyしてPayloadに変換する関数
                 */
                #[allow(dead_code)]
                pub fn to_payload(&self) -> Result<JSONResponsePayload, serde_json::Error> {
                    Ok(JSONResponsePayload { data: serde_json::to_string(self)? })
                }
            }
        }
    };
}

jpayload!(Ping, (id, Uuid));
jpayload!(Pong, (id, Uuid));

#[derive(Serialize, Deserialize, Debug)]
pub struct ListRoomInfo {
    pub id: Uuid,
    pub room_name: String,
    pub players: u8,
    pub max_players: u8,
    pub locked: bool,
}

jpayload!(GetRoomsRequest, (id, Uuid));
jpayload!(GetRoomsResponse, (id, Uuid), (rooms, Vec<ListRoomInfo>));

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum JsonMessage {
    JSONPing(JSONPing),
    JSONPong(JSONPong),
    JSONGetRoomsRequest(JSONGetRoomsRequest),
    JSONGetRoomsResponse(JSONGetRoomsResponse),
}

#[derive(Debug)]
#[allow(dead_code)]
pub enum JsonPayloadError {
    Json(serde_json::Error),
    Binary(wincode::WriteError),
}

impl From<serde_json::Error> for JsonPayloadError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<wincode::WriteError> for JsonPayloadError {
    fn from(value: wincode::WriteError) -> Self {
        Self::Binary(value)
    }
}

impl JsonMessage {
    pub fn to_response_body(&self) -> Result<Vec<u8>, JsonPayloadError> {
        let data = serde_json::to_string(self)?;
        let payload = JSONResponsePayload { data };
        Ok(payload.to_binary()?)
    }
}

#[allow(dead_code)]
pub fn wrap_with_opcode(op: Opcode, body: Vec<u8>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + body.len());
    buf.push(op as u8);
    buf.extend_from_slice(&body);
    buf
}

macro_rules! schemas {
        ($($name:ident = $val:expr),*) => {
            #[derive(Serialize, Deserialize, Debug)]
            #[allow(dead_code)]
            #[repr(u8)]
            pub enum GameMessage {
                $(
                    $name($name) = $val,
                )*
            }
        };
    }

#[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug, Clone, Copy)]
#[allow(dead_code)]
#[repr(u8)]
pub enum Opcode {
    PingPayload = 0x01,
    PongPayload = 0x02,
    JSONRequestPayload = 0x10,
    JSONResponsePayload = 0x11,
}

impl TryFrom<u8> for Opcode {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Opcode::PingPayload),
            0x02 => Ok(Opcode::PongPayload),
            0x10 => Ok(Opcode::JSONRequestPayload),
            0x11 => Ok(Opcode::JSONResponsePayload),
            _ => Err(()),
        }
    }
}

schemas! {
    PingPayload = 0x01,
    PongPayload = 0x02,
    JSONRequestPayload = 0x10,
    JSONResponsePayload = 0x11
}
