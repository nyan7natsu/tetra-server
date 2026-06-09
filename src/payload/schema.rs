//! バイナリフレーム層。
//! 各フレームは先頭1バイトの [`Opcode`] と、それに続く `*Payload` 本体で構成される。
//! `schemas!` マクロが `*Payload` 構造体・`Opcode`・`GameMessage`・`TryFrom<u8>` を一括生成する。

use serde::{Deserialize, Serialize};
use wincode::{SchemaRead, SchemaWrite};

use super::common::UuidBytes;

/// フレーム先頭に [`Opcode`] を1バイト付与する。
#[allow(dead_code)]
pub fn wrap_with_opcode(op: Opcode, body: Vec<u8>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + body.len());
    buf.push(op as u8);
    buf.extend_from_slice(&body);
    buf
}

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
        // 対戦中の中継用。サーバーは中身を解釈せず、相手の同種チャンネルへフレームを素通しする。
        // GameEvent  : reliable 経由（spawn/lock/clear/garbage/gameover/start など離散イベント）
        // PieceState : unreliable 経由（落下ミノ座標・回転など高頻度ストリーム）
        GameEvent, 0x06, (data, Vec<u8>);
        PieceState, 0x07, (data, Vec<u8>);
    }
}
