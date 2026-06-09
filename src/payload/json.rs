//! Reliable な JSON 制御メッセージ層。
//! ロビー〜ルーム操作のリクエスト/レスポンス/通知を表す。
//! `json_message!` マクロが `JSON*` 構造体と、それらを束ねる [`JsonMessage`] enum を生成する。
//! 実際の送信時は [`JsonMessage::to_response_body`] で JSON 文字列化したのち
//! [`JSONResponsePayload`] に詰めてバイナリ化する。

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::schema::JSONResponsePayload;

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
        CreateRoomRequest, (id, Uuid), (room_name, String), (max_players, u8), (password, Option<String>), (tags, Vec<u32>), (username, String), (rule, String);
        CreateRoomResponse, (id, Uuid), (room_id, Uuid), (code, String);
        JoinRoomRequest, (id, Uuid), (room_id, Uuid), (password, Option<String>), (username, String), (rule, String);
        JoinRoomResponse, (id, Uuid), (success, bool), (message, Option<String>);
        JoinByCodeRequest, (id, Uuid), (code, String), (password, Option<String>), (username, String), (rule, String);
        JoinByCodeResponse, (id, Uuid), (success, bool), (message, Option<String>), (room_id, Option<Uuid>);
        JoinRandomMatchRequest, (id, Uuid), (username, String), (rule, String);
        JoinRandomMatchResponse, (id, Uuid), (matched, bool), (room_id, Option<Uuid>);
        CancelRandomMatchRequest, (id, Uuid);
        CancelRandomMatchResponse, (id, Uuid), (success, bool);
        LeaveRoomRequest, (id, Uuid), (room_id, Uuid);
        LeaveRoomResponse, (id, Uuid), (success, bool), (message, Option<String>);
        RoomUpdateRequest, (id, Uuid), (room_id, Uuid), (room_name, String), (max_players, u8), (password, Option<String>), (tags, Vec<u32>);
        RoomUpdateResponse, (id, Uuid), (success, bool), (message, Option<String>);
        RoomInfoNotification, (room_id, Uuid), (room_name, String), (players, Vec<(Uuid, String, String)>), (max_players, u8), (tags, Vec<u32>);
        RoomLeaveNotification, (message, String);
    }
);

impl JsonMessage {
    pub fn to_response_body(&self) -> Result<Vec<u8>, JsonPayloadError> {
        let data = serde_json::to_string(self)?;
        let payload = JSONResponsePayload { data };
        Ok(payload.to_binary()?)
    }
}
