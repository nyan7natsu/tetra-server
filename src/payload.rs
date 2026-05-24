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

macro_rules! json_message {
    (
        enum $enum_name:ident {
            $(
                // 各行のパターン： 構造体名, [ (フィールド, 型), (フィールド, 型), ... ]
                // ※ フィールドが0個の場合も考慮して `*`（0回以上の繰り返し）にします
                $struct_name:ident $(, ( $field_name:ident, $field_type:ty ) )*
            );* $(;)? // 各メッセージの区切りはセミコロン（;）にします
        }
    ) => {
        pastey::paste! {
            $(
                /**
                 * Payloadの`data`の中身をシリアライズした構造体
                 */
                #[derive(Serialize, Deserialize, Debug)]
                #[serde(rename_all = "camelCase")]
                #[allow(dead_code)]
                pub struct [<JSON $struct_name>] {
                    $( pub $field_name: $field_type ),*
                }

                impl [<JSON $struct_name>] {
                    /**
                     * 自身をstringifyしてPayloadに変換する関数
                     */
                    #[allow(dead_code)]
                    pub fn to_payload(&self) -> Result<JSONResponsePayload, serde_json::Error> {
                        Ok(JSONResponsePayload { data: serde_json::to_string(self)? })
                    }
                }
            )*

            #[derive(Serialize, Deserialize, Debug)]
            #[serde(tag = "type")]
            pub enum $enum_name {
                $(
                    [<JSON $struct_name>]([<JSON $struct_name>]),
                )*
            }
        }
    };
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ListRoomInfo {
    pub id: Uuid,
    pub room_name: String,
    pub players: u8,
    pub max_players: u8,
    pub locked: bool,
    pub tags: Vec<u32>,
}

json_message!(
    enum JsonMessage {
        Ping, (id, Uuid);
        Pong, (id, Uuid);
        GetRoomsRequest, (id, Uuid);
        GetRoomsResponse, (id, Uuid), (rooms, Vec<ListRoomInfo>);
    }
);

macro_rules! schemas {
    (
        enum $enum_name:ident {
            $(
                // 各行のパターン： 構造体名, [ (フィールド, 型), (フィールド, 型), ... ]
                // ※ フィールドが0個の場合も考慮して `*`（0回以上の繰り返し）にします
                $struct_name:ident, $opcode:expr $(, ( $field_name:ident, $field_type:ty ) )*
            );* $(;)? // 各メッセージの区切りはセミコロン（;）にします
        }
    ) => {
        pastey::paste! {
            $(
                #[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug)]
                pub struct [<$struct_name Payload>] {
                    $(pub $field_name: $field_type),*
                }

                impl [<$struct_name Payload>] {
                    #[allow(dead_code)]
                    pub fn to_binary(&self) -> Result<Vec<u8>, wincode::WriteError> {
                        wincode::config::serialize(self, crate::payload::wincode_config())
                    }
                }
            )*

            #[derive(Serialize, Deserialize, SchemaRead, SchemaWrite, Debug, Clone, Copy)]
            #[allow(dead_code)]
            #[repr(u8)]
            pub enum Opcode {
                $(
                    [<$struct_name Payload>] = $opcode,
                )*
            }

            #[derive(Serialize, Deserialize, Debug)]
            #[allow(dead_code)]
            #[repr(u8)]
            pub enum $enum_name {
                $(
                    [<$struct_name Payload>]([<$struct_name Payload>]) = $opcode,
                )*
            }

            impl TryFrom<u8> for Opcode {
                type Error = ();
                fn try_from(value: u8) -> Result<Self, Self::Error> {
                    match value {
                        $(
                            $opcode => Ok(Opcode::[<$struct_name Payload>]),
                        )*
                        _ => Err(()),
                    }
                }
            }
        }
    };
}

schemas! {
    enum GameMessage {
        Ping, 0x01, (id, UuidBytes);
        Pong, 0x02, (id, UuidBytes);
        JSONRequest, 0x03, (data, String);
        JSONResponse, 0x04, (data, String);
        Close, 0x05;
    }
}
