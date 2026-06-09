# 内部サブプロトコル v1（クライアント間合意）

オンライン対戦（B方式：ライブ状態ストリーミング）で、**クライアント同士が `GameEvent` / `PieceState` の `data` バイト列に載せる中身**の仕様。
サーバー（VPS WebRTC リレー）は `data` を**解釈せず相手の同種チャンネルへ素通し**するため、この仕様は Rust 側の変更なしに改訂できる（バージョンは開始ハンドシェイクで交渉。後述）。

設計の前提・経緯は本体メモリ `project-online-design` / `project-tetra-server` を参照。

---

## 0. 確定した方針（2026-06-08）

- **符号化**: 手書き little-endian バイナリ（`DataView`）。固定レイアウト直接読み書き。wincode/varint 非依存・JSON 不使用。
- **盤面補正**: 設置（Lock）ごとに**毎回フル盤面**を送る（自己修復的・デシンク不能）。別途 snapshot 不要。
- **バージョン管理**: 開始ハンドシェイク（JSON 層の GameStart）で `protocolVersion` を**1回だけ**交渉。各フレームにバージョンバイトは持たせない。
- 相手の rule（テト/ぷよどちらか）は対戦開始時に既知 → PieceState/GameEvent に rule 判別バイト不要。

## 1. トランスポート（既存 / Rust 側）

| Opcode | 名前 | チャンネル | 用途 |
|---|---|---|---|
| `0x06` | `GameEvent` | reliable | 離散イベント（順序保証・欠落不可） |
| `0x07` | `PieceState` | unreliable | 落下ピース座標の高頻度ストリーム（最新優先・欠落OK） |

サーバーは受信した `0x06`/`0x07` フレームを相手の同種チャンネルへ中身解釈せず転送する。
開始/ルーム/再戦/rule通知などサーバーが発番する性質のものは **JSON 層（`payload/json.rs`）**で扱い、本サブプロトコルには含めない。

## 2. 共通エンコード規約

- すべて little-endian。
- 整数型表記: `u8 / i8 / u16 / u32`。
- `t:u32` = GameStart 相対ミリ秒。受信側は初回到着で `offset = localArrival − t` を確定し、以後 `t + offset + buffer` で再生（buffer ≈ 片道遅延 + 30〜60ms ジッタ）。
- 盤面セルは 1 セル 1 バイト（後述）。

## 3. PieceState（`0x07`・unreliable・30〜60Hz）

サブタグ無し（単一用途）。色は載せない（テトは type で既知、ぷよは直近 Spawn で既知）。

```
共通先頭: t:u32

テト本体（4B）:  type:u8(0–6)  x:i8  y:i8  rot:u8(0–3)
  → フレーム計 8B

ぷよ本体（3B）:  pivotX:i8  pivotY2:i8(= pivotY*2、x.5 刻みを整数化)  orient:u8(targetRot 0–3)
  → フレーム計 7B
```

受信側は前後サンプルを線形補間して滑らかに描画する。

## 4. GameEvent（`0x06`・reliable）

先頭 1 バイト = **サブタグ**、その直後に共通 `t:u32`、以降ボディ。

| サブタグ | 名前 | ボディ |
|---|---|---|
| `0x01` | Spawn | テト: `type:u8`, `nextCount:u8`, `next:u8×nextCount` ／ ぷよ: `pivotColor:u8`, `childColor:u8`, `nextCount:u8`, `next:(u8,u8)×nextCount` |
| `0x02` | Lock | 盤面スナップショット（§5）。**設置ごと=定期補正を兼ねる** |
| `0x03` | Clear | テト: `rows:u8`, `rowIdx:u8×rows`, `flags:u8`(bit0 B2B / bit1 PC / bit2 T-spin) ／ ぷよ: `chain:u8`, `clearedCells:u8`（演出キュー用・**任意**。盤面は Lock が権威） |
| `0x04` | **GarbageSend ★ゲーム影響** | `amount:u16`, `holes:u8×amount`（各おじゃま単位の穴/列。テト=各行の穴列 0–9、ぷよ=各おじゃまの列） |
| `0x05` | Hold | `heldType:u8`（テトのみ） |
| `0x06` | PendingUpdate | `ready:u16`, `unready:u16`（相手の予告ゲージ表示用・フェーズ別。ready=確定/降下可段, unready=猶予段。いずれも internal 非表示段は除外） |
| `0x07` | GameOver | `result:u8`（0=topout/負, 1=clear/勝） |
| `0x08` | **Control（開始/再戦合意）** | `action:u8`, `seed:u32`。`action`=`0x01 READY`（開始/再戦の準備完了。`seed`=共有シード素材）/`0x02 UNREADY`（準備取消・`seed` 未使用） |

> **表示同期 vs ゲーム影響**: `0x04 GarbageSend` のみ受信側の自分のゲームに反映（予告に積む。相殺/着弾は受信側の既存ロジック）。他はすべて相手ミニ盤面への描画のみで自分のゲームに影響しない。

> **Control（`0x08`）= ロビー段のクライアント間ハンドシェイク**: 両プレイヤーが在室中、各自「対戦開始 / REMATCH」で `READY` を送る。`READY` には乱数 `seed` を載せ、受信側は **自分の seed と相手の seed を XOR**（0 のときは 1）して共有シードを得る。両者 READY がそろった瞬間に各自カウントダウン開始＝**ずれ≒片道遅延**で同時開始。共有シードは本体エンジンの「同ツモ」（テト `getNextType` / ぷよ `_initActiveColors`・`_makePair` の専用乱数 `tumoRng`）に注入する。サーバーは中身を解釈せず中継するのみ（Rust 変更不要）。

## 5. 盤面スナップショット（Lock `0x02` ボディ）

行優先（row 0 が最上段）で固定サイズの密配列。1 セル 1 バイト。

- **テト**: `10 × 40` = **400B**。可視 20 行（ROWS_COUNT）＋**上方バッファ 20 行**。
  - 行マッピング: 盤面行 `r` ↔ エンジン座標 `y = r − 20`。`r=0` が最上段（`y=−20`）、`r=20` が可視最上段（`y=0`）。
  - 値: `0xFF`=空 / `0–7`=Block.type（`0–6`=色, `7`=灰=おじゃま）。
  - **上方バッファを含める理由**: おじゃまがせり上がると `applyGarbage`（tet/garbage.js）が既存ブロック全てを `block.y -= 1` で押し上げ、スタックが `y<0`（フィールド上端より上）へはみ出す。これらは消去で降りてくると再び見える＆描画も上端半行（`VISIBLE_EXTRA_ROW_RATIO=0.5`）を表示するため、可視 20 行のみでは取りこぼす。バッファ外（`y<−20`、実質トップアウト＝GameOver）に達したブロックのみ破棄。
- **ぷよ**: `6 × 17`（cols × (rows 12 + hiddenRows 5)）= 102B
  - 値: `0`=空 / `1–5`=色 / `6`=おじゃま（`field[r][c]` の生値そのまま。隠し 5 行を内包するので追加バッファ不要）。

設置は概ね 1 秒に 1 回なので 400B/102B でも帯域は問題なし。これが定期補正を兼ねるため、別途スナップショット用イベントは設けない。

## 6. バージョン交渉

GameStart（JSON 層・サーバー発番）に `protocolVersion`（この文書のメジャー = 1）を載せ、双方が一致を確認してから対戦開始。不一致なら互換なしとして対戦不可（将来：下位互換ネゴ）。

## 7. 送信フック箇所（本体・ロジック不変の 1 行追加）

- PieceState = `dropMino`/移動/回転後（throttle 30〜60Hz）
- Spawn = `popMino`（tet）/ ぷよペア生成
- Lock = `secureMino`（tet/board.js）/ ぷよ設置確定
- Clear = `Scoring`/`checkLine`（tet）/ ぷよ連鎖
- GarbageSend = `sendGarbage`（tet/garbage.js・puyo/ojama.js）送信時
- GameOver = `gameOver()`

受信側は相手用 Game/PuyoGame を「パペットモード」（重力/入力ループ無効）で保持し、受信データをフィールドへ代入して既存 `drawAll()` を呼ぶ。

## 8. リファレンス実装

- **コーデック**: `tetra-server/testclient/subprotocol.js`（クラシック script・依存なし・グローバル `Subprotocol`）。
  この仕様の encode/decode を全フレーム分実装した正準実装。本体(TETLABO)へはこのファイルをそのまま流用して `src/online/` 等へ配置する想定（step5）。
  - PieceState: `encodePieceStateTet/Puyo`, `decodePieceState(data, rule)`
  - GameEvent: `encodeSpawnTet/Puyo`, `encodeLockTet/Puyo`, `encodeClearTet/Puyo`, `encodeGarbage`, `encodeHold`, `encodePending`, `encodeGameOver`, `decodeGameEvent(data, rule)`
  - 盤面ビルダ: `buildTetBoard(blocks)`（field.blocks → 400B、上方バッファ込み）, `buildPuyoBoard(field)`（17×6 → 102B）／逆変換 `tetBoardToBlocks(board)`（y は実座標で復元・負yブロック保持）, `puyoBoardToField(board)`
  - 各 encode は **opcode を除いた data 部**を返す。呼び出し側が先頭に `0x06`/`0x07` を付けて該当チャンネルへ送る。
- **テストクライアント**: `tetra-server/testclient/index.html` がこのコーデックを使用。2タブで接続→同室化後、tet/ぷよを切替えて各フレームを送信し、相手タブが復号して人間可読表示する（リレー越しの実バイト送受信を検証可能）。「自分のrule」=送信エンコード、「相手のrule」=受信デコード。
