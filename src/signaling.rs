use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMessage {
    // WebRTC用シグナリングメッセージ
    Offer {
        sdp: String,
        /// 再接続時にクライアントが既存の player_id を載せる。存在すれば再バインド、
        /// 無い/未知なら新規プレイヤーとして扱う（初回接続では省略=None）。
        #[serde(default)]
        player_id: Option<uuid::Uuid>,
    },
    Answer {
        sdp: String,
    },
    Candidate {
        candidate: String,
        sdp_mid: Option<String>,
        sdp_m_line_index: Option<u16>,
        user_id: uuid::Uuid,
    },

    // クライアント認証
    Auth {
        client_version: String,
    },
}
