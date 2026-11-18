//! MonsGeek (Akko 系) Ry5088 ファミリ — app.monsgeek.com Web ドライバ互換プロトコル。
//!
//! 対象: **FUN60 PRO 有線 (3151:502D, デバイス ID 2304)** で実機検証 (2026-11-19)。
//! ほか PID 502D を共有する約 30 機種 (FUN60 Ultra 2381 / FUN60 PRO 2464 等) が
//! 同一プロトコル (cmd 0x8F のデバイス ID で機種確定)。公式の名前は
//! `ry5088_<model>` (fw: 実機 777)。
//!
//! ## トランスポート (docs/PROTOCOL_NOTES.md §7 参照)
//!
//! - 設定チャネル = **interface 2 の FFFF/02 コレクション、feature report**。
//!   reportId 0 / ペイロード 64B (Windows では 65B バッファの [1..65])。
//! - チェックサム **Bit7**: `byte[7] = 0xFF - sum(byte[0..7])` (SET_LEDPARAM だけ Bit8)。
//! - 単純 GET (`0x84`/`0x83`/`0x86`/`0x89`/`0x87`/`0x8F`): 応答 `[cmd echo][data…]`。
//! - チャンク GET (`0x8A` キーマップ / `0x90` Fn / `0xE5` 磁軸):
//!   応答は **エコー無しの生 64B データ**。[1]=profile/[2]=0xFF/[3]=chunk。
//! - 準備中は `[echo or 00][zeros][0xFF-cmd]` 形式の空フレームが返る —
//!   リトライでデータフレームを待つ (実機: オープン直後の最初の 1 コマンドで稀に発生)。
//! - SET は応答確認なし (公式も sendMsg のみ) + `vendorSleep` (書込後 ~100ms)。
//!
//! ## メモリマップ (磁軸 op — GET 0xE5 / SET 0x65、[1]=op [2]=bulk [3]=chunk)
//!
//! op0=押下行程 (u16 LE, mm×100) / op1=リリース行程 / op2=RT押下 / op3=RTリリース /
//! op6=底部死区 / op7=キーモード (bit7=RT 有効、本体 0/2/3/4/5/7) /
//! op9=Snap 相手 (0xFF=無効) / op251=頂部死区 (FW≥1024、実機 777 は未対応)。
//!
//! ## キーマトリクス (0x8A GET / 0x0A SET — 4B/キー × 128)
//!
//! `[0,0,usage,0]`=通常キー、`[0,mods,key,0]`=コンボ、`[b3,b2,b1,b0]`=32bit BE
//! (メディア等 usage>0xFF)、`[9,type,slot,0]`=マクロ、`[0,0,0,0]`/`[0,0,3,0]`=無効。
//! 物理配置は `registry::fun60_layout` (実機 defaultMatrix から抽出)。

use devhub_core::backend::*;
use devhub_core::model::*;
use devhub_core::registry;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::Duration;

// ---------------------------------------------------------------- コマンド

mod cmd {
    /// 工場リセット (応答なし、後 2s 待ち)。
    pub const RESET: u8 = 0x01;
    /// ポーリングレート設定: [2]=enum (0=8K..6=125)。GET 0x83 → resp[2]。
    pub const SET_REPORT: u8 = 0x03;
    /// プロファイル切替: [1]=profile。GET 0x84 → resp[1]。
    pub const SET_PROFILE: u8 = 0x04;
    /// デバウンス設定: [1]=値。GET 0x86 → resp[1]。
    pub const SET_DEBOUNCE: u8 = 0x06;
    /// LED 設定 (Bit8): [1]=灯効 [2]=4-速度 [3]=明るさ [4]=mode(7 炫彩/8 単色)
    ///   [5..7]=RGB。GET 0x87 → 同レイアウト。
    pub const SET_LEDPARAM: u8 = 0x07;
    /// KBOPTION 設定: [2]=fn層 [3]=誤入力防止 [4]=RTStab/25 [5]=WASD。
    ///   GET 0x89 → resp[1]=system [2]=fn [3]=防誤触 [4]=RTStab [5]=WASD。
    pub const SET_KBOPTION: u8 = 0x09;
    /// キーマトリクス単キー設定: [1]=profile [2]=keyIndex [5]=persist [6]=son
    ///   [8..11]=4B 設定値。GET 0x8A: [1]=profile [2]=0xFF [3]=chunk → 64B データ。
    pub const SET_KEYMATRIX: u8 = 0x0A;
    /// 磁軸設定: [1]=op [2]=0(単キー)/1(bulk) [3]=keyIndex/chunk [4]=last。
    ///   GET 0xE5 は同引数でデータ 64B。単キーはデータ @byte[5..] (u16 LE or u8)。
    pub const SET_MAGNETISM: u8 = 0x65;
    /// 磁軸 GET (=0xE5)。
    pub const GET_MAGNETISM: u8 = 0xE5;
    /// 磁軸モード op。
    pub const OP_ACTUATE: u8 = 0;
    pub const OP_RELEASE: u8 = 1;
    pub const OP_RT_PRESS: u8 = 2;
    pub const OP_RT_RELEASE: u8 = 3;
    pub const OP_DEADZONE: u8 = 6;
    pub const OP_MODE: u8 = 7;
    /// 最小行程キャリブレーション: [1]=0/1 (1 → 2s → 0 → 0x1E[1]=1)。
    pub const SET_MAG_CAL_MIN: u8 = 0x1C;
    /// 最大行程キャリブレーション: [1]=0/1。
    pub const SET_MAG_CAL_MAX: u8 = 0x1E;
    /// whoAmI: 応答 [1..5]=デバイス ID u32 LE、[7..9]=FW (low,high)、[9]=isBoot。
    pub const WHOAMI: u8 = 0x8F;
    // 0xE6 機能リスト (分解能 enum) は FW≥1024 系のみ — 実機 777 は未対応のため
    // 分解能は FW 番号から推定する (travel_mul)。
}

/// cmd 0x8F 応答のデバイス ID → (表示名, model)。CID ではなく ID 直読み。
/// PID 502D 共有機のうち FUN60 系のみ表登録 — 他は汎用名で開く。
fn model_of(id: u32) -> Option<(&'static str, &'static str)> {
    Some(match id {
        2304 => ("FUN60 PRO", "fun60-pro"),
        2381 => ("FUN60 Ultra", "fun60-ultra"),
        2464 => ("FUN60 PRO", "fun60-pro"),
        2600 => ("FUN60 PRO", "fun60-pro"),
        _ => return None,
    })
}

/// 行程値のワイヤ倍率 (FW 依存 — 実機 777 → ×100)。
fn travel_mul(fw: u16) -> u32 {
    match fw {
        f if f < 768 => 10,
        f if f < 1280 => 100,
        _ => 200,
    }
}

// ==================================================================== 構造

pub struct MonsDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    caps: Capabilities,
    /// FW から決まる行程倍率。
    mul: u32,
    /// layout index → matrix index (fun60_layout と DEFAULT_USAGE から構築)。
    matrix_of: Vec<u16>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// 直前のチャンク (要求 args, 応答 64B)。**stale 応答検出**に使う —
    /// 要求を変えたのに応答が直前とバイト一致なら、デバイスの準備が
    /// 間に合っておらず古い応答が返っている (実機: 長い連続読み出しの
    /// 末尾で観測 — op3 chunk0 が op2 chunk2 の内容を返した)。
    last_chunk: Mutex<Option<([u8; 6], [u8; 64])>>,
}

/// Bit7 チェックサム: `0xFF - sum(p[0..7])`。
fn ck7(p: &[u8]) -> u8 {
    0xFFu8.wrapping_sub(p[..7].iter().map(|&x| x as u32).sum::<u32>() as u8)
}
/// Bit8 チェックサム: `0xFF - sum(p[0..8])` (SET_LEDPARAM のみ)。
fn ck8(p: &[u8]) -> u8 {
    0xFFu8.wrapping_sub(p[..8].iter().map(|&x| x as u32).sum::<u32>() as u8)
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
) -> HubResult<MonsDriver> {
    let matrix_of = registry::fun60_layout()
        .iter()
        .map(|k| k.id)
        .collect::<Vec<_>>();
    let mut d = MonsDriver {
        dev: Mutex::new(dev),
        info: Mutex::new(info),
        caps: build_caps(),
        mul: 100,
        matrix_of,
        events,
        last_chunk: Mutex::new(None),
    };
    // ウォームアップ + 機種確定 (オープン直後の 1 発目は空フレームが返ることがある)。
    for _ in 0..5 {
        if let Some((id, fw)) = d.whoami() {
            d.mul = travel_mul(fw);
            let mut i = d.info.lock().unwrap_or_else(|p| p.into_inner());
            i.firmware = Some(format!("{fw}"));
            let mut dirty = false;
            if let Some((name, model)) = model_of(id) {
                if i.model != model {
                    i.model = model.into();
                    i.name = name.into();
                    dirty = true;
                }
            }
            let info = i.clone();
            drop(i);
            if dirty {
                // 機種確定を UI へ (サイドバーの表示名が変わる)。
                if let Some(ev) = &d.events {
                    ev(HubEvent::Info(info.id.clone(), Box::new(info)));
                }
            }
            break;
        }
        sleep(Duration::from_millis(60));
    }
    Ok(d)
}

impl MonsDriver {
    /// whoAmI → (デバイス ID, FW)。空/無効応答は None。
    fn whoami(&self) -> Option<(u32, u16)> {
        let p = self.cmd_simple(cmd::WHOAMI, &[]).ok()?;
        let id = u32::from_le_bytes([
            p[1], p[2], p[3], p[4],
        ]);
        let fw = ((p[8] as u16) << 8) | p[7] as u16;
        (id != 0 && id != 0xFFFF_FFFF).then_some((id, fw))
    }

    /// 単純 GET: 応答 payload 64B ([0]=cmd echo)。失敗/不一致は 1 回リトライ。
    fn cmd_simple(&self, c: u8, args: &[u8]) -> HubResult<[u8; 64]> {
        for _ in 0..3 {
            let mut p = [0u8; 64];
            p[0] = c;
            for (i, a) in args.iter().take(6).enumerate() {
                p[1 + i] = *a;
            }
            p[7] = ck7(&p);
            self.send(&p)?;
            sleep(Duration::from_millis(15));
            if let Some(r) = self.get_feature() {
                let mut payload = [0u8; 64];
                payload.copy_from_slice(&r[1..65]);
                if payload[0] == c {
                    return Ok(payload);
                }
            }
            sleep(Duration::from_millis(40));
        }
        Err(HubError::Protocol(format!("monsgeek: cmd 0x{c:02x} 応答なし")))
    }

    /// チャンク GET: 応答はエコー無しの 64B データ。応答が準備中の場合、
    /// デバイスは「準備中フレーム」か**直前の応答をそのまま**返す —
    /// 前者は idle 判定 (p[7]==0xFF-cmd + 実質ゼロ) で、後者は
    /// 「要求が変わったのに応答が直前と一致」で検出。いずれも待って読み直し、
    /// **stale が 1 サイクル続けば要求を再送**して仕切り直す
    /// (書き込みコミット中は GET が無視され続けるため — 実機確認)。
    /// 正味オールゼロの正当チャンク (p[7]==0) はどちらにも当てはまらない
    /// ので即座に通る。
    fn cmd_chunk(&self, c: u8, args: &[u8]) -> HubResult<[u8; 64]> {
        let mut p = [0u8; 64];
        p[0] = c;
        let mut a6 = [0u8; 6];
        for (i, a) in args.iter().take(6).enumerate() {
            p[1 + i] = *a;
            a6[i] = *a;
        }
        p[7] = ck7(&p);
        let mut wait = 15u64;
        let mut last_data: Option<[u8; 64]> = None;
        let mut since_send = 0usize;
        self.send(&p)?;
        for attempt in 0..14 {
            if attempt > 0 {
                sleep(Duration::from_millis(wait));
                wait = (wait * 2).min(120);
                since_send += 1;
                if since_send >= 5 {
                    // stale が 1 サイクル続いた → 再送して仕切り直す
                    // (コミット中に無視された要求の再発行)。
                    self.send(&p)?;
                    since_send = 0;
                    wait = 60;
                }
            }
            let Some(r) = self.get_feature() else {
                continue;
            };
            let mut payload = [0u8; 64];
            payload.copy_from_slice(&r[1..65]);
            let is_idle = payload[7] == 0xFFu8.wrapping_sub(c)
                && payload[1..7].iter().all(|&x| x == 0)
                && payload[8..].iter().all(|&x| x == 0);
            if is_idle {
                continue;
            }
            // stale 検出: 要求が前回と違うのに応答が一致。
            let stale = {
                let lc = self.last_chunk.lock().unwrap_or_else(|p| p.into_inner());
                matches!(&*lc, Some((prev_a, prev_r)) if *prev_a != a6 && *prev_r == payload)
            };
            if stale {
                last_data = Some(payload);
                continue;
            }
            *self.last_chunk.lock().unwrap_or_else(|p| p.into_inner()) = Some((a6, payload));
            return Ok(payload);
        }
        match last_data {
            // 要求を変えても同じ内容が返り続ける — 同内容の正当チャンク
            // (未編集プロファイル同士等) はこれで通る。
            Some(payload) => {
                *self.last_chunk.lock().unwrap_or_else(|p| p.into_inner()) = Some((a6, payload));
                Ok(payload)
            }
            None => Err(HubError::Protocol(format!(
                "monsgeek: chunk 0x{c:02x} の応答が準備中のまま"
            ))),
        }
    }

    /// SET (応答確認なし、公式と同じ) + vendorSleep。
    fn send_set(&self, c: u8, args: &[u8], data: &[u8], ck_at: usize) -> HubResult<()> {
        let mut p = [0u8; 64];
        p[0] = c;
        for (i, a) in args.iter().take(7).enumerate() {
            p[1 + i] = *a;
        }
        for (i, b) in data.iter().take(56).enumerate() {
            p[8 + i] = *b;
        }
        p[ck_at] = if ck_at == 8 { ck8(&p) } else { ck7(&p) };
        self.send(&p)?;
        sleep(Duration::from_millis(100));
        Ok(())
    }

    fn send(&self, p: &[u8; 64]) -> HubResult<()> {
        // reportId 0 を先頭に付けた 65B (hidapi の feature report は [rid, data…])。
        let mut buf = [0u8; 65];
        buf[1..].copy_from_slice(p);
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        dev.send_feature_report(&buf)
            .map_err(|e| HubError::Io(format!("monsgeek write: {e}")))
    }

    fn get_feature(&self) -> Option<[u8; 65]> {
        let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
        let mut buf = [0u8; 65];
        dev.get_feature_report(&mut buf).ok()?;
        Some(buf)
    }

    /// 磁軸 op の GET (bulk、[1]=op [2]=1 [3]=chunk)。chunks は呼び出し側で
    /// (u16 系=3 チャンクで keys 0..95、u8 系=2 チャンクで keys 0..127)。
    fn mag_get(&self, op: u8, chunks: usize) -> HubResult<Vec<u8>> {
        let mut out = Vec::with_capacity(chunks * 64);
        for c in 0..chunks {
            let p = self.cmd_chunk(cmd::GET_MAGNETISM, &[op, 1, c as u8])?;
            out.extend_from_slice(&p);
        }
        Ok(out)
    }

    /// 磁軸 単キー SET。公式 `_sendMagnetismInfoSimpleCMD` は **8B ヘッダ**
    /// (`[0x65, op, 0, keyIdx, last, 0, 0]` + Bit7 cksum が byte[7] を上書き) に
    /// **データを byte[8..] 以降に追加**する (u16 系は LE 2B、u8 系は 1B)。
    /// `last` はバッチ最終フレームのみ true (デバイス側の確定をトリガー)。
    /// **バッチ確定後は 1500ms 待つ** — コミット中のデバイスは GET を
    /// 無視して古い応答を返し続ける (公式の blockCommonData に相当。
    /// 500ms では足りない場合があった実測済み)。
    fn mag_set_one(&self, op: u8, key: u16, data: &[u8], last: bool) -> HubResult<()> {
        let args = [
            op,
            0, // simple (単キー)
            key as u8,
            u8::from(last),
        ];
        self.send_set(cmd::SET_MAGNETISM, &args, data, 7)?;
        if last {
            sleep(Duration::from_millis(1500));
        }
        Ok(())
    }

    /// キーマトリクス 1 プロファイル分 (keys 0..95 = 6 チャンク。レイアウトの
    /// 最大 matrix idx は 83 < 96)。
    fn matrix_get(&self, profile: u8) -> HubResult<Vec<u8>> {
        let mut out = Vec::with_capacity(384);
        for c in 0..6u8 {
            let p = self.cmd_chunk(0x8A, &[profile, 0xFF, c, 0])?;
            out.extend_from_slice(&p);
        }
        Ok(out)
    }

    fn u16_at(data: &[u8], key: usize) -> u32 {
        let o = key * 2;
        u32::from(data[o]) | (u32::from(data[o + 1]) << 8)
    }
}

// ---------------------------------------------------------------- マトリクス符号化

/// 4B レコード → KeyAction (matrixToHid / matrixToConfigs 相当)。
fn rec_to_action(r: &[u8]) -> KeyAction {
    if r[0] == 0 && r[1] == 0 && r[3] == 0 && r[2] != 0 {
        return KeyAction::Code(r[2] as u32);
    }
    if r == [0, 0, 0, 0] || r == [0, 0, 3, 0] {
        return KeyAction::None;
    }
    if r[0] == 0 && r[3] == 0 && r[1] != 0 {
        // コンボ [0, skey, key, 0]。
        return KeyAction::Combo {
            mods: r[1],
            code: r[2] as u32,
        };
    }
    // 32bit BE 合成 (メディア / マクロ / 機能キー)。往復保存可能な生コード。
    let v = (u32::from(r[0]) << 24) | (u32::from(r[1]) << 16) | (u32::from(r[2]) << 8)
        | u32::from(r[3]);
    KeyAction::Code(v)
}

/// KeyAction → 4B レコード (hidToMatrix 相当)。未対応は Err。
fn action_to_rec(a: &KeyAction) -> HubResult<[u8; 4]> {
    Ok(match a {
        KeyAction::None => [0, 0, 0, 0],
        KeyAction::Transparent => return Err(HubError::Unsupported("透過 (Fn 層は対象外)")),
        KeyAction::Code(c) => {
            if *c <= 0xFF {
                [0, 0, *c as u8, 0]
            } else {
                [(*c >> 24) as u8, (*c >> 16) as u8, (*c >> 8) as u8, *c as u8]
            }
        }
        KeyAction::Combo { mods, code } => {
            if *code > 0xFF {
                return Err(HubError::Invalid("コンボのキーコード".into()));
            }
            [0, *mods, *code as u8, 0]
        }
        // マクロ割当は Phase 2 (マクロ領域 0x0B/0x8B 未検証)。
        KeyAction::Macro(_) => return Err(HubError::Unsupported("マクロ割当")),
    })
}

// ==================================================================== caps

fn opts(v: &[(i64, &str)]) -> Vec<ChoiceOpt> {
    v.iter().map(|(a, b)| ChoiceOpt::new(*a, *b)).collect()
}

fn build_caps() -> Capabilities {
    use section::*;
    let effects: Vec<LightEffect> = [
        (0u8, "消灯", false, false),
        (1, "常時点灯", true, false),
        (2, "ブリージング", true, false),
        (4, "ウェーブ", false, true),
        (5, "リップル", false, true),
        (6, "雨滴", false, true),
        (8, "タップ発光", false, true),
        (10, "正弦波", false, true),
        (16, "流星", false, true),
        (18, "トレイン", false, true),
        (23, "花火", false, true),
    ]
    .iter()
    .map(|(id, name, color, speed)| {
        let mut e = LightEffect::new(*id, name).dir();
        if *color {
            e = e.color();
        }
        if !*speed {
            e = e.still();
        }
        e
    })
    .collect();
    Capabilities {
        settings: vec![
            SettingSpec::choice(
                "polling",
                PERFORMANCE,
                "ポーリングレート",
                opts(&[
                    (125, "125"),
                    (250, "250"),
                    (500, "500"),
                    (1000, "1K"),
                    (2000, "2K"),
                    (4000, "4K"),
                    (8000, "8K"),
                ]),
            )
            .help("キーボードの報告頻度 (Hz)"),
            SettingSpec::slider("debounce", CLICK, "デバウンス", 0., 30., 1., "ms")
                .help("キーの入力確定遅延 (公式 UI のデバウンス)"),
            SettingSpec::choice(
                "rt_stab",
                PERFORMANCE,
                "RT Stab",
                opts(&[
                    (0, "オフ"),
                    (25, "25%"),
                    (50, "50%"),
                    (75, "75%"),
                    (100, "100%"),
                    (125, "125%"),
                ]),
            )
            .help("RT 安定化アルゴリズムの介入率 — 高いほど安定・遅延増"),
            SettingSpec::toggle("mistouch", CLICK, "誤入力防止")
                .help("浅いタッチを間引きました (防誤触)"),
            SettingSpec::toggle("wasd_swap", KEYBOARD, "WASD ↔ 矢印").help("WASD キーを矢印に切替"),
        ],
        actions: vec![
            ActionSpec::new("calibrate", MAGNETIC, "キャリブレーション")
                .confirm("全キーを数回ゆっくり押し込み・離してください (約 10 秒)")
                .help("最小/最大行程を測定し直します"),
            ActionSpec::new("calibrate_finish", MAGNETIC, "キャリブレーション終了")
                .help("最大行程キャリブレーションを確定します"),
            ActionSpec::new("factory_reset", SYSTEM, "工場出荷状態に戻す")
                .danger("このデバイスの全設定が消去されます"),
        ],
        profiles: Some(ProfileCaps {
            count: 4,
            renamable: false,
        }),
        keyboard: Some(KeyboardCaps {
            layout: registry::fun60_layout(),
            layers: ["オンボード1", "オンボード2", "オンボード3", "オンボード4"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            catalog: monsgeek_catalog(),
            combos: true,
            perf: Some(PerfCaps {
                travel: 3.3,
                min: 0.1,
                step: 0.01,
                rapid_trigger: true,
                separate_release: true,
                rt_first: false,
                deadzones: true,
                continuous_rt: false,
                switch_types: vec![],
                switch_ids: vec![],
                per_key: true,
                // 0x1B 通知ストリーム (interface 1) は Phase 2 — 今は申告しない。
                live_travel: false,
            }),
            advanced: vec![],
            macros: None,
        }),
        lighting: vec![LightArea {
            id: 0,
            name: "バックライト".into(),
            effects,
            brightness_max: 4,
            speed_max: 4,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        }],
        battery: false,
        peq: None,
        mouse: None,
        firmware_feed: None,
    }
}

/// 割当候補。基本 = HID usage そのまま、メディア = Consumer usage (32bit 形式)。
fn monsgeek_catalog() -> Vec<KeyGroup> {
    let g = |name: &str, v: &[(u32, &str, &str)]| KeyGroup {
        name: name.into(),
        keys: v
            .iter()
            .map(|(c, l, s)| KeyChoice {
                code: *c,
                label: (*l).into(),
                short: Some((*s).into()),
            })
            .collect(),
    };
    vec![
        devhub_core::keys::basic_group(),
        g(
            "メディア",
            &[
                (0xCD, "再生/一時停止", "⏯"),
                (0xB5, "次の曲", "⏭"),
                (0xB6, "前の曲", "⏮"),
                (0xE9, "音量 +", "🔊"),
                (0xEA, "音量 −", "🔉"),
                (0xE2, "ミュート", "🔇"),
            ],
        ),
    ]
}

// ==================================================================== DeviceDriver

impl DeviceDriver for MonsDriver {
    fn info(&self) -> DeviceInfo {
        self.info.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn caps(&self) -> Capabilities {
        self.caps.clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        // ---- 現在プロファイル + スカラー設定
        let profile = self.cmd_simple(0x84, &[])?[1].min(3);
        let rate = self.cmd_simple(0x83, &[])?[2].min(6);
        let debounce = self.cmd_simple(0x86, &[])?[1];
        let kbopt = self.cmd_simple(0x89, &[])?;
        let light = self.cmd_simple(0x87, &[])?;

        let mut settings = BTreeMap::new();
        let hz = match rate {
            0 => 8000,
            1 => 4000,
            2 => 2000,
            3 => 1000,
            4 => 500,
            5 => 250,
            6 => 125,
            _ => 1000,
        };
        settings.insert("polling".into(), SettingValue::Choice(hz));
        settings.insert("debounce".into(), SettingValue::Num(debounce as f32));
        // KBOPTION: [1]=system [2]=fn [3]=防誤触 [4]=RTStab(×25) [5]=WASD。
        settings.insert(
            "mistouch".into(),
            SettingValue::Bool(kbopt[3] != 0),
        );
        let stab = kbopt[4].min(5) * 25;
        settings.insert("rt_stab".into(), SettingValue::Choice(stab as i64));
        settings.insert("wasd_swap".into(), SettingValue::Bool(kbopt[5] != 0));

        // ---- キーマップ 4 プロファイル分 (各 6 チャンク)。
        let mut keymap: Vec<Vec<KeyAction>> = Vec::with_capacity(4);
        for layer in 0..4u8 {
            let m = self.matrix_get(layer)?;
            keymap.push(
                self.matrix_of
                    .iter()
                    .map(|&idx| {
                        let o = idx as usize * 4;
                        if o + 4 > m.len() {
                            KeyAction::None
                        } else {
                            rec_to_action(&m[o..o + 4])
                        }
                    })
                    .collect(),
            );
        }

        // ---- 磁軸 (keys 0..95)。op7 (u8) → fire、op0/1/6/2/3 (u16 LE)。
        let mul = self.mul as f32;
        let modes = self.mag_get(cmd::OP_MODE, 2)?;
        let actuate = self.mag_get(cmd::OP_ACTUATE, 3)?;
        let release = self.mag_get(cmd::OP_RELEASE, 3)?;
        let deadzone = self.mag_get(cmd::OP_DEADZONE, 3)?;
        let rt_press = self.mag_get(cmd::OP_RT_PRESS, 3)?;
        let rt_release = self.mag_get(cmd::OP_RT_RELEASE, 3)?;

        let perf: Vec<KeyPerf> = self
            .matrix_of
            .iter()
            .map(|&idx| {
                let k = idx as usize;
                let mode = modes.get(k).copied().unwrap_or(0);
                let fire = mode & 0x80 != 0;
                let act = Self::u16_at(&actuate, k) as f32 / mul;
                KeyPerf {
                    mode: if fire { TriggerMode::Rapid } else { TriggerMode::Normal },
                    actuation: act,
                    release: Some(Self::u16_at(&release, k) as f32 / mul),
                    rt_first: act,
                    rt_press: Self::u16_at(&rt_press, k) as f32 / mul,
                    rt_release: Self::u16_at(&rt_release, k) as f32 / mul,
                    top_deadzone: 0.,
                    bottom_deadzone: Self::u16_at(&deadzone, k) as f32 / mul,
                    continuous: false,
                    switch_type: 0,
                }
            })
            .collect();

        // ---- ライティング: [1]=灯効 [2]=speed(4-値) [3]=明るさ [4]=mode [5..7]=RGB。
        let effect = light[1];
        let speed_ui = 4u8.saturating_sub(light[2].min(4));
        let lighting = LightingState {
            on: effect != 0,
            effect: if effect == 0 { 1 } else { effect },
            color: Rgb::new(light[5], light[6], light[7]),
            brightness: light[3].min(4),
            speed: speed_ui,
            direction: 0,
            per_key: BTreeMap::new(),
            zone_on: vec![],
            palette: vec![],
            palette_sel: 0,
            color_correction: None,
        };
        let mut lm = BTreeMap::new();
        lm.insert(0u8, lighting);

        Ok(DeviceState {
            profile: ProfileState {
                active: profile,
                names: vec![],
            },
            settings,
            mouse: None,
            keyboard: Some(KeyboardState {
                keymap,
                perf,
                advanced: vec![],
                macros: vec![],
            }),
            lighting: lm,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "polling" => {
                    let e = match value.as_choice() {
                        8000 => 0,
                        4000 => 1,
                        2000 => 2,
                        1000 => 3,
                        500 => 4,
                        250 => 5,
                        125 => 6,
                        _ => return Err(HubError::Invalid("ポーリングレート".into())),
                    };
                    self.send_set(cmd::SET_REPORT, &[0, e], &[], 7)
                }
                "debounce" => {
                    self.send_set(
                        cmd::SET_DEBOUNCE,
                        &[value.as_num().clamp(0., 30.) as u8],
                        &[],
                        7,
                    )
                }
                "rt_stab" => {
                    let v = (value.as_choice().clamp(0, 125) / 25).min(5) as u8;
                    // fnIndex は現在値を維持 (resp から再取得)。
                    let cur = self.cmd_simple(0x89, &[])?;
                    let args = [0, cur[2], cur[3], v, cur[5]];
                    self.send_set(cmd::SET_KBOPTION, &args, &[], 7)
                }
                "mistouch" => {
                    let cur = self.cmd_simple(0x89, &[])?;
                    let v = u8::from(value.as_bool());
                    let args = [0, cur[2], v, cur[4], cur[5]];
                    self.send_set(cmd::SET_KBOPTION, &args, &[], 7)
                }
                "wasd_swap" => {
                    let cur = self.cmd_simple(0x89, &[])?;
                    let v = u8::from(value.as_bool());
                    let args = [0, cur[2], cur[3], cur[4], v];
                    self.send_set(cmd::SET_KBOPTION, &args, &[], 7)
                }
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::KeyMap { layer, key, action } => {
                let idx = *self.matrix_of.get(*key).ok_or_else(|| {
                    HubError::Invalid("キー番号".into())
                })?;
                let rec = action_to_rec(action)?;
                let args = [
                    *layer,
                    idx as u8,
                    0,
                    0,
                    1, // persist
                    0, // sonProfile
                ];
                // [8..11]=4B 設定値 → send_set の data 領域 [8..] へ。
                self.send_set(cmd::SET_KEYMATRIX, &args, &rec, 7)
            }
            Change::KeyPerf { keys, perf } => {
                let mul = self.mul;
                let mut frames: Vec<(u8, u16, [u8; 2])> = Vec::new();
                for &li in keys {
                    let idx = *self.matrix_of.get(li).ok_or_else(|| {
                        HubError::Invalid("キー番号".into())
                    })?;
                    let mode = u8::from(perf.mode == TriggerMode::Rapid) << 7;
                    frames.push((cmd::OP_MODE, idx, [mode, 0]));
                    let a = (perf.actuation * mul as f32).round() as u32;
                    frames.push((
                        cmd::OP_ACTUATE,
                        idx,
                        [(a & 0xff) as u8, (a >> 8) as u8],
                    ));
                    if let Some(r) = perf.release {
                        let r = (r * mul as f32).round() as u32;
                        frames.push((cmd::OP_RELEASE, idx, [(r & 0xff) as u8, (r >> 8) as u8]));
                    }
                    if perf.mode == TriggerMode::Rapid {
                        let p = (perf.rt_press * mul as f32).round() as u32;
                        let r = (perf.rt_release * mul as f32).round() as u32;
                        frames.push((cmd::OP_RT_PRESS, idx, [(p & 0xff) as u8, (p >> 8) as u8]));
                        frames.push((
                            cmd::OP_RT_RELEASE,
                            idx,
                            [(r & 0xff) as u8, (r >> 8) as u8],
                        ));
                    }
                    let d = (perf.bottom_deadzone * mul as f32).round() as u32;
                    frames.push((cmd::OP_DEADZONE, idx, [(d & 0xff) as u8, (d >> 8) as u8]));
                }
                if frames.is_empty() {
                    return Ok(());
                }
                let n = frames.len();
                for (i, (op, idx, data)) in frames.iter().enumerate() {
                    self.mag_set_one(*op, *idx, data, i + 1 == n)?;
                }
                Ok(())
            }
            Change::Profile(i) => self.send_set(cmd::SET_PROFILE, &[*i], &[], 7),
            Change::Lighting { state, .. } => {
                let effect = if state.on {
                    if state.effect == 0 {
                        1
                    } else {
                        state.effect
                    }
                } else {
                    0
                };
                // 静的灯効 (1) は単色 + RGB、それ以外は炫彩 (公式の NORMAL=8/DAZZLE=7)。
                let mode = if effect == 1 { 8 } else { 7 };
                let args = [
                    effect,
                    4 - state.speed.clamp(0, 4),
                    state.brightness.clamp(0, 4),
                    mode,
                    state.color.r,
                    state.color.g,
                    state.color.b,
                ];
                // SET_LEDPARAM は Bit8 (cksum @byte[8])。
                self.send_set(cmd::SET_LEDPARAM, &args, &[], 8)
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "calibrate" => {
                // 公式 setJiaoZhunKaiGuan: 最小行程 → 2s → 最大行程 開始。
                self.send_set(cmd::SET_MAG_CAL_MIN, &[1], &[], 7)?;
                sleep(Duration::from_millis(2000));
                self.send_set(cmd::SET_MAG_CAL_MIN, &[0], &[], 7)?;
                self.send_set(cmd::SET_MAG_CAL_MAX, &[1], &[], 7)?;
                Ok(ActionOutcome {
                    message: Some("キャリブレーション開始 — 全キーを数回押し込み・離してください".into()),
                    reload: false,
                })
            }
            "calibrate_finish" => {
                self.send_set(cmd::SET_MAG_CAL_MAX, &[0], &[], 7)?;
                Ok(ActionOutcome {
                    message: Some("キャリブレーションを保存しました".into()),
                    reload: true,
                })
            }
            "factory_reset" => {
                self.send_set(cmd::RESET, &[], &[], 7)?;
                sleep(Duration::from_millis(2000));
                Ok(ActionOutcome {
                    message: Some("工場出荷状態に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }
}

// ==================================================================== テスト
//
// fixtures/monsgeek/ は実機 FUN60 PRO (ID 2304, fw 777) の 65B 生フレーム
// ([0]=report id 0、payload は [1..65])。

#[cfg(test)]
mod tests {
    use super::*;

    /// 実機 (FUN60 PRO, ID 2304) の defaultMatrix — matrix idx → 既定 HID usage。
    /// 0 = 未使用スロット (Fn キー k71 の既定値は関数レコード [10,1,0,0] のため 0)。
    /// レイアウト (registry::fun60_layout) の id と 1:1 — 公式 Web バンドルの
    /// `class x8` (ry5088_akko_fun60pro_1m_8k) から機械抽出したもの。
    #[rustfmt::skip]
    const DEFAULT_USAGE: [u16; 128] = [
        // idx 0..15
        0, 0x29, 0x2B, 0x39, 0xE1, 0xE0, 0, 0x1E, 0x14, 0x04, 0, 0, 0, 0x1F, 0x1A, 0x16,
        // 16..31
        0x1D, 0xE3, 0, 0x20, 0x08, 0x07, 0x1B, 0xE2, 0, 0x21, 0x15, 0x09, 0x06, 0, 0, 0x22,
        // 32..47
        0x17, 0x0A, 0x19, 0, 0, 0x23, 0x1C, 0x0B, 0x05, 0x2C, 0, 0x24, 0x18, 0x0D, 0x11, 0,
        // 48..63
        0, 0x25, 0x0C, 0x0E, 0x10, 0, 0, 0x26, 0x12, 0x0F, 0x36, 0, 0, 0x27, 0x13, 0x33,
        // 64..79
        0x37, 0xE6, 0, 0x2D, 0x2F, 0x34, 0x38, 0, 0, 0x2E, 0x30, 0, 0xE5, 0x65, 0, 0x2A,
        // 80..95
        0x31, 0x28, 0, 0xE4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        // 96..127
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    const WHOAMI_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/whoami.resp.bin");
    const MATRIX0_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/matrix0.resp.bin");
    const MAG_OP7_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/mag_op7_c0.resp.bin");
    const MAG_OP0_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/mag_op0_c0.resp.bin");
    const LIGHT_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/light.resp.bin");
    const RATE_RESP: &[u8] = include_bytes!("../fixtures/monsgeek/rate.resp.bin");

    #[test]
    fn checksum_bit7() {
        // whoami 要求フレーム: [0x8F, 0×6, 0xFF-0x8F=0x70] (fixtures と一致)。
        let mut p = [0u8; 64];
        p[0] = cmd::WHOAMI;
        p[7] = ck7(&p);
        assert_eq!(p[7], 0x70);
        // mag op0 GET: [E5, 0, 1, 0, 0, 0, 0, ck] → 0xFF-(E5+1)=0x19。
        // (「準備中」応答フレームの cksum 0x1A は 0xFF-0xE5 — 要求とは別物。)
        let mut q = [0u8; 64];
        q[0] = 0xE5;
        q[1] = 0;
        q[2] = 1;
        q[7] = ck7(&q);
        assert_eq!(q[7], 0x19);
        // 実機応答の echo (単純 GET は [0]=cmd echo)。
        let w = &WHOAMI_RESP[1..65];
        assert_eq!(w[0], cmd::WHOAMI);
    }

    #[test]
    fn whoami_parses() {
        let w = &WHOAMI_RESP[1..65];
        let id = u32::from_le_bytes([w[1], w[2], w[3], w[4]]);
        let fw = ((w[8] as u16) << 8) | w[7] as u16;
        assert_eq!(id, 2304);
        assert_eq!(fw, 777);
        assert_eq!(model_of(id), Some(("FUN60 PRO", "fun60-pro")));
        assert_eq!(travel_mul(fw), 100);
    }

    #[test]
    fn matrix_chunk_decodes_default_layout() {
        // chunk0 = matrix idx 0..15。実機は出荷値: k1=Esc, k2=Tab, k3=Caps,
        // k4=LShift, k5=LCtrl, k7='1', k8='q', k9='a'、未使用スロットは 0。
        let m = &MATRIX0_RESP[1..65];
        let rec = |k: usize| &m[k * 4..k * 4 + 4];
        assert_eq!(rec(0), &[0, 0, 0, 0]); // 未使用
        assert_eq!(rec(1), &[0, 0, 0x29, 0]); // Esc
        assert_eq!(rec(2), &[0, 0, 0x2b, 0]); // Tab
        assert_eq!(rec(3), &[0, 0, 0x39, 0]); // Caps
        assert_eq!(rec(4), &[0, 0, 0xe1, 0]); // LShift
        assert_eq!(rec(5), &[0, 0, 0xe0, 0]); // LCtrl
        assert_eq!(rec(7), &[0, 0, 0x1e, 0]); // 1
        assert_eq!(rec(8), &[0, 0, 0x14, 0]); // q
        assert_eq!(rec(9), &[0, 0, 0x04, 0]); // a
        // DEFAULT_USAGE と突合 (レイアウト ↔ マトリクスの対応づけの根拠)。
        assert_eq!(DEFAULT_USAGE[1], 0x29);
        assert_eq!(DEFAULT_USAGE[9], 0x04);
        assert_eq!(DEFAULT_USAGE[41], 0x2c); // Space
        assert_eq!(DEFAULT_USAGE[79], 0x2a); // Backspace
        // rec → action。
        assert_eq!(rec_to_action(rec(1)), KeyAction::Code(0x29));
        assert_eq!(rec_to_action(rec(0)), KeyAction::None);
        assert_eq!(
            rec_to_action(&[0, 0x01, 0x04, 0]),
            KeyAction::Combo { mods: 1, code: 4 } // Ctrl+A
        );
        assert_eq!(rec_to_action(&[0, 0, 0x00, 0xCD]), KeyAction::Code(0xCD)); // メディア 32bit
    }

    #[test]
    fn action_roundtrip() {
        for a in &[
            KeyAction::None,
            KeyAction::Code(0x04),
            KeyAction::Code(0x29),
            KeyAction::Code(0xCD),
            KeyAction::Combo { mods: 0x01, code: 0x04 },
        ] {
            let rec = action_to_rec(a).expect("encodable");
            assert_eq!(&rec_to_action(&rec), a, "{a:?}");
        }
        // 32bit 形式の往復 (メディア): BE 分割で [0,0,hi,lo]。
        let rec = action_to_rec(&KeyAction::Code(0x20CD)).unwrap();
        assert_eq!(rec, [0, 0x00, 0x20, 0xCD]);
        assert_eq!(rec_to_action(&rec), KeyAction::Code(0x20CD));
    }

    #[test]
    fn mag_ops_decode() {
        // op7 (u8/キー): 全キー 0x80 = RT 有効・モード通常。
        let m = &MAG_OP7_RESP[1..65];
        assert_eq!(m[0], 0); // 未使用スロット
        assert_eq!(m[1], 0x80);
        assert_eq!(m[9], 0x80);
        // op0 (u16 LE/キー): 0x00b4 = 180 → 1.80mm (×100)。
        let a = &MAG_OP0_RESP[1..65];
        assert_eq!(MonsDriver::u16_at(a, 0), 0); // 未使用
        assert_eq!(MonsDriver::u16_at(a, 1), 180);
        assert_eq!(MonsDriver::u16_at(a, 2), 180);
    }

    #[test]
    fn light_and_rate_decode() {
        let l = &LIGHT_RESP[1..65];
        assert_eq!((l[1], l[3], l[4]), (4, 1, 0x17)); // Wave, 明るさ1, 炫彩(1<<4|7)
        assert_eq!((l[5], l[6], l[7]), (0xb4, 0xb4, 0xb4));
        let r = &RATE_RESP[1..65];
        assert_eq!(r[2], 1); // 4000Hz
    }

    #[test]
    fn caps_shape() {
        let c = build_caps();
        let k = c.keyboard.as_ref().unwrap();
        assert_eq!(k.layout.len(), 61);
        assert_eq!(k.layers.len(), 4);
        assert!(k.perf.as_ref().unwrap().rapid_trigger);
        // レイアウト id はすべて DEFAULT_USAGE の使用スロットと一致。
        for key in &k.layout {
            assert!(key.id < 128);
            if key.label != "Fn" {
                assert_ne!(
                    DEFAULT_USAGE[key.id as usize],
                    0,
                    "レイアウトキー {} (id={}) が未使用スロットを指している",
                    key.label,
                    key.id
                );
            }
        }
    }
}
