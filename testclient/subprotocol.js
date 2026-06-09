// ─────────────────────────────────────────────────────────────
// 内部サブプロトコル v1 コーデック（クライアント間合意）
// 仕様: tetra-server/docs/internal-subprotocol.md
//
// このファイルは「GameEvent(0x06) / PieceState(0x07) の data 部（=opcode を除いた中身）」
// のみを encode/decode する純粋ロジック。トランスポート（先頭 opcode 付与・チャンネル選択）
// は呼び出し側の責務。
//
// クラシック script として読み込み、グローバル `Subprotocol` を公開する。
// 本体(TETLABO)・testclient の双方からそのまま流用できる（依存なし）。
// ─────────────────────────────────────────────────────────────
(function (root) {
  "use strict";

  // ── 盤面寸法（仕様 §5） ──
  // テト: 可視 20 行に加え、上方バッファ 20 行を含める。
  //   おじゃまがせり上がると既存スタックは applyGarbage の `block.y -= 1` で y<0（上端より上）へ
  //   押し上げられ、後のライン消去で降りてくると再び見える。これらを取りこぼさないため上方も含める。
  //   盤面行 r=0 が最上段（y = −TET_BUFFER_ROWS）、r=TET_BUFFER_ROWS が可視最上段（y=0）。
  const TET_COLS = 10, TET_ROWS = 20;          // 可視行数
  const TET_BUFFER_ROWS = 20;                  // 上方バッファ行数
  const TET_TOTAL_ROWS = TET_ROWS + TET_BUFFER_ROWS; // 40 行
  const PUYO_COLS = 6, PUYO_ROWS = 17;         // cols6 × (rows12 + hidden5) = 102 セル（隠し行を内包）
  const TET_EMPTY = 0xff;                       // テト盤面の空セル番兵
  const PUYO_EMPTY = 0;                         // ぷよ盤面の空（field 生値 0）

  // ── GameEvent サブタグ（仕様 §4） ──
  const EV = Object.freeze({
    SPAWN: 0x01,
    LOCK: 0x02,
    CLEAR: 0x03,
    GARBAGE: 0x04, // ★ゲーム影響
    HOLD: 0x05,
    PENDING: 0x06,
    GAMEOVER: 0x07,
    CONTROL: 0x08, // 対戦の開始/再戦合意（ロビー段のクライアント間ハンドシェイク）
  });

  // ── CONTROL アクション（仕様 §4・サブタグ 0x08） ──
  //   READY   = 開始/再戦の準備完了（seed を持つ。両者の seed を XOR して共有シードにする）
  //   UNREADY = 準備をキャンセル（seed は未使用）
  const CTRL = Object.freeze({
    READY: 0x01,
    UNREADY: 0x02,
  });

  // ──────────────────────────────────────────
  // 低レベル: ByteWriter / ByteReader（little-endian）
  // ──────────────────────────────────────────
  class Writer {
    constructor(cap = 64) {
      this._buf = new Uint8Array(cap);
      this._len = 0;
    }
    _ensure(n) {
      if (this._len + n <= this._buf.length) return;
      let cap = this._buf.length * 2;
      while (cap < this._len + n) cap *= 2;
      const nb = new Uint8Array(cap);
      nb.set(this._buf.subarray(0, this._len));
      this._buf = nb;
    }
    u8(v) { this._ensure(1); this._buf[this._len++] = v & 0xff; return this; }
    i8(v) { return this.u8(v < 0 ? v + 256 : v); }
    u16(v) {
      this._ensure(2);
      this._buf[this._len++] = v & 0xff;
      this._buf[this._len++] = (v >> 8) & 0xff;
      return this;
    }
    u32(v) {
      this._ensure(4);
      this._buf[this._len++] = v & 0xff;
      this._buf[this._len++] = (v >>> 8) & 0xff;
      this._buf[this._len++] = (v >>> 16) & 0xff;
      this._buf[this._len++] = (v >>> 24) & 0xff;
      return this;
    }
    bytes(arr) {
      this._ensure(arr.length);
      this._buf.set(arr, this._len);
      this._len += arr.length;
      return this;
    }
    finish() { return this._buf.slice(0, this._len); }
  }

  class Reader {
    constructor(data) {
      this._d = data instanceof Uint8Array ? data : new Uint8Array(data);
      this._off = 0;
    }
    get remaining() { return this._d.length - this._off; }
    u8() { return this._d[this._off++]; }
    i8() { const b = this._d[this._off++]; return b > 127 ? b - 256 : b; }
    u16() {
      const v = this._d[this._off] | (this._d[this._off + 1] << 8);
      this._off += 2;
      return v;
    }
    u32() {
      const v =
        (this._d[this._off] |
          (this._d[this._off + 1] << 8) |
          (this._d[this._off + 2] << 16) |
          (this._d[this._off + 3] << 24)) >>> 0;
      this._off += 4;
      return v;
    }
    bytes(n) {
      const s = this._d.subarray(this._off, this._off + n);
      this._off += n;
      return s;
    }
  }

  // ──────────────────────────────────────────
  // PieceState（0x07・unreliable・サブタグ無し・仕様 §3）
  //   テト: t:u32, type:u8, x:i8, y:i8, rot:u8                → 8B
  //   ぷよ: t:u32, pivotX:i8, pivotY2:i8(=pivotY*2), orient:u8 → 7B
  // 色は載せない（受信側は直近 Spawn で既知）。
  // ──────────────────────────────────────────
  function encodePieceStateTet({ t, type, x, y, rot }) {
    return new Writer(8).u32(t).u8(type).i8(x).i8(y).u8(rot).finish();
  }
  function encodePieceStatePuyo({ t, pivotX, pivotY, orient }) {
    return new Writer(7).u32(t).i8(pivotX).i8(Math.round(pivotY * 2)).u8(orient).finish();
  }
  // rule: 'tet' | 'puyo'（受信側が相手の rule を文脈から知っている前提）
  function decodePieceState(data, rule) {
    const r = new Reader(data);
    const t = r.u32();
    if (rule === "puyo") {
      const pivotX = r.i8();
      const pivotY = r.i8() / 2;
      const orient = r.u8();
      return { kind: "piece", rule, t, pivotX, pivotY, orient };
    }
    const type = r.u8();
    const x = r.i8();
    const y = r.i8();
    const rot = r.u8();
    return { kind: "piece", rule: "tet", t, type, x, y, rot };
  }

  // ──────────────────────────────────────────
  // GameEvent（0x06・reliable・先頭1B=サブタグ + t:u32 + ボディ・仕様 §4）
  // ──────────────────────────────────────────

  // Spawn ── テト: type, nextCount, next[] / ぷよ: pivotColor, childColor, nextCount, next[(a,b)]
  function encodeSpawnTet({ t, type, next = [] }) {
    const w = new Writer(8 + next.length).u8(EV.SPAWN).u32(t).u8(type).u8(next.length);
    for (const n of next) w.u8(n);
    return w.finish();
  }
  function encodeSpawnPuyo({ t, pivotColor, childColor, next = [] }) {
    const w = new Writer(9 + next.length * 2)
      .u8(EV.SPAWN).u32(t).u8(pivotColor).u8(childColor).u8(next.length);
    for (const pair of next) { w.u8(pair[0]); w.u8(pair[1]); }
    return w.finish();
  }

  // Lock ── 盤面スナップショット（行優先・1セル1バイト・仕様 §5）。board は長さ cols*rows の配列/Uint8Array。
  function encodeLockTet({ t, board }) {
    _assertLen(board, TET_COLS * TET_TOTAL_ROWS, "tet board");
    return new Writer(5 + board.length).u8(EV.LOCK).u32(t).bytes(board).finish();
  }
  function encodeLockPuyo({ t, board }) {
    _assertLen(board, PUYO_COLS * PUYO_ROWS, "puyo board");
    return new Writer(5 + board.length).u8(EV.LOCK).u32(t).bytes(board).finish();
  }

  // Clear ── 演出用・任意（盤面は Lock が権威）
  //   テト: rows:u8, rowIdx:u8[rows], flags:u8(bit0 B2B / bit1 PC / bit2 T-spin)
  //   ぷよ: chain:u8, clearedCells:u8
  function encodeClearTet({ t, rowIdx = [], flags = 0 }) {
    const w = new Writer(7 + rowIdx.length).u8(EV.CLEAR).u32(t).u8(rowIdx.length);
    for (const i of rowIdx) w.u8(i);
    w.u8(flags);
    return w.finish();
  }
  function encodeClearPuyo({ t, chain, clearedCells }) {
    return new Writer(7).u8(EV.CLEAR).u32(t).u8(chain).u8(clearedCells).finish();
  }

  // GarbageSend ── ★ゲーム影響: amount:u16, holes:u8[amount]
  function encodeGarbage({ t, amount, holes = [] }) {
    const w = new Writer(7 + holes.length).u8(EV.GARBAGE).u32(t).u16(amount);
    for (const h of holes) w.u8(h);
    return w.finish();
  }

  // Hold ── テトのみ: heldType:u8
  function encodeHold({ t, heldType }) {
    return new Writer(6).u8(EV.HOLD).u32(t).u8(heldType).finish();
  }

  // PendingUpdate ── ready:u16, unready:u16（相手の予告ゲージ表示用・フェーズ別）
  //   ready=確定(降下可・赤/点滅)段の合計 / unready=猶予(青)段の合計（いずれも internal は除外）
  function encodePending({ t, ready = 0, unready = 0 }) {
    return new Writer(9).u8(EV.PENDING).u32(t).u16(ready).u16(unready).finish();
  }

  // GameOver ── result:u8（0=topout/負, 1=clear/勝）
  function encodeGameOver({ t, result }) {
    return new Writer(6).u8(EV.GAMEOVER).u32(t).u8(result).finish();
  }

  // Control ── action:u8 + seed:u32（開始/再戦合意。seed は READY 時の共有シード素材）
  //   サーバーは中身を解釈しないため、CONTROL も既存の中継経路で相手へそのまま届く。
  function encodeControl({ t = 0, action, seed = 0 }) {
    return new Writer(10).u8(EV.CONTROL).u32(t).u8(action).u32(seed >>> 0).finish();
  }

  // ── 統合デコーダ（rule: 相手の 'tet' | 'puyo'） ──
  function decodeGameEvent(data, rule) {
    const r = new Reader(data);
    const tag = r.u8();
    const t = r.u32();
    switch (tag) {
      case EV.SPAWN: {
        if (rule === "puyo") {
          const pivotColor = r.u8();
          const childColor = r.u8();
          const count = r.u8();
          const next = [];
          for (let i = 0; i < count; i++) next.push([r.u8(), r.u8()]);
          return { kind: "spawn", rule: "puyo", t, pivotColor, childColor, next };
        }
        const type = r.u8();
        const count = r.u8();
        const next = [];
        for (let i = 0; i < count; i++) next.push(r.u8());
        return { kind: "spawn", rule: "tet", t, type, next };
      }
      case EV.LOCK: {
        const cells = rule === "puyo" ? PUYO_COLS * PUYO_ROWS : TET_COLS * TET_TOTAL_ROWS;
        const board = r.bytes(cells);
        return { kind: "lock", rule, t, board };
      }
      case EV.CLEAR: {
        if (rule === "puyo") {
          const chain = r.u8();
          const clearedCells = r.u8();
          return { kind: "clear", rule: "puyo", t, chain, clearedCells };
        }
        const rows = r.u8();
        const rowIdx = [];
        for (let i = 0; i < rows; i++) rowIdx.push(r.u8());
        const flags = r.u8();
        return {
          kind: "clear", rule: "tet", t, rowIdx, flags,
          b2b: !!(flags & 1), pc: !!(flags & 2), tspin: !!(flags & 4),
        };
      }
      case EV.GARBAGE: {
        const amount = r.u16();
        const holes = [];
        for (let i = 0; i < amount; i++) holes.push(r.u8());
        return { kind: "garbage", t, amount, holes };
      }
      case EV.HOLD:
        return { kind: "hold", t, heldType: r.u8() };
      case EV.PENDING: {
        const ready = r.u16();
        const unready = r.u16();
        return { kind: "pending", t, ready, unready, pending: ready + unready };
      }
      case EV.GAMEOVER:
        return { kind: "gameover", t, result: r.u8() };
      case EV.CONTROL: {
        const action = r.u8();
        const seed = r.u32();
        return { kind: "control", t, action, seed };
      }
      default:
        return { kind: "unknown", t, tag };
    }
  }

  // ──────────────────────────────────────────
  // 盤面ビルダ（本体エンジン構造 → スナップショット）
  // ──────────────────────────────────────────
  // テト: Game.field.blocks（[{x,y,type}]、type 0-6色 / 7灰）→ 1セル1バイト（空=0xFF）。
  //   盤面行 = y + TET_BUFFER_ROWS（上方バッファ含む）。範囲外（極端なオーバーフロー=実質ゲームオーバー時）は破棄。
  function buildTetBoard(blocks) {
    const board = new Uint8Array(TET_COLS * TET_TOTAL_ROWS).fill(TET_EMPTY);
    for (const b of blocks) {
      const row = b.y + TET_BUFFER_ROWS;
      if (b.x < 0 || b.x >= TET_COLS || row < 0 || row >= TET_TOTAL_ROWS) continue;
      board[row * TET_COLS + b.x] = b.type & 0xff;
    }
    return board;
  }
  // 逆変換: テト盤面 → [{x,y,type}]（y は上方バッファを考慮した実座標。空=0xFF は除外）。受信側パペット用。
  function tetBoardToBlocks(board) {
    const out = [];
    for (let row = 0; row < TET_TOTAL_ROWS; row++) {
      for (let c = 0; c < TET_COLS; c++) {
        const v = board[row * TET_COLS + c];
        if (v !== TET_EMPTY) out.push({ x: c, y: row - TET_BUFFER_ROWS, type: v });
      }
    }
    return out;
  }
  // ぷよ: PuyoGame.field（[r][c]、0空/1-5色/6おじゃま、先頭 hiddenRows 含む 17 行）→ 行優先フラット
  function buildPuyoBoard(field) {
    const board = new Uint8Array(PUYO_COLS * PUYO_ROWS).fill(PUYO_EMPTY);
    for (let r = 0; r < PUYO_ROWS; r++) {
      const row = field[r];
      if (!row) continue;
      for (let c = 0; c < PUYO_COLS; c++) board[r * PUYO_COLS + c] = (row[c] || 0) & 0xff;
    }
    return board;
  }
  // 逆変換: ぷよ盤面 → 17×6 の 2D 配列。受信側パペット用。
  function puyoBoardToField(board) {
    const field = Array.from({ length: PUYO_ROWS }, () => new Array(PUYO_COLS).fill(0));
    for (let r = 0; r < PUYO_ROWS; r++)
      for (let c = 0; c < PUYO_COLS; c++) field[r][c] = board[r * PUYO_COLS + c];
    return field;
  }

  function _assertLen(arr, n, label) {
    if (arr.length !== n) throw new Error(`${label}: expected ${n} cells, got ${arr.length}`);
  }

  root.Subprotocol = {
    VERSION: 1,
    TET_COLS, TET_ROWS, TET_BUFFER_ROWS, TET_TOTAL_ROWS,
    PUYO_COLS, PUYO_ROWS, TET_EMPTY, PUYO_EMPTY,
    EV, CTRL,
    // PieceState
    encodePieceStateTet, encodePieceStatePuyo, decodePieceState,
    // GameEvent encoders
    encodeSpawnTet, encodeSpawnPuyo,
    encodeLockTet, encodeLockPuyo,
    encodeClearTet, encodeClearPuyo,
    encodeGarbage, encodeHold, encodePending, encodeGameOver, encodeControl,
    // GameEvent decoder
    decodeGameEvent,
    // board builders / inverse
    buildTetBoard, buildPuyoBoard, tetBoardToBlocks, puyoBoardToField,
    // low-level (テスト/拡張用)
    Writer, Reader,
  };

  if (typeof module !== "undefined" && module.exports) module.exports = root.Subprotocol;
})(typeof window !== "undefined" ? window : globalThis);
