//! Pulsar Nordic チップセット系マウス (Xlite V3 / X2V2 / X2A 等) — 17B report-8 + メモリマップ。
//!
//! 対応機種は VID `3554` の 3 つの PID (有線 F507 / 1K ドングル F508 / 4K 系 F509) を
//! 共用する 21 機種。プロトコルは共通で、機種は cmd1 handshake の CID/MID で確定する
//! (実機 Xlite V3: CID=6, MID=7)。ドングルは同じ 17B プロトコルを中継する。
//!
//! 出典 (docs/PROTOCOL_NOTES.md §3 参照):
//! - 実機 Xlite V3 (3554:F507, ff02/u2) の直接プローブ (2026-11-19) —
//!   handshake/battery/FW/profile/MEM_GET と全メモリマップ 0x00-0xBD を取得、
//!   スカラの (v, 0x55-v) ペアを全項目で検証。
//! - python-pulsar-mouse-tool (andrewrabert, MIT) — コマンド体系・DPI/LED エンコード。
//! - pulsar-mouse-linux の nordic ドライバ (packerlschupfer) — メモリマップ詳細・
//!   ショートカットレコード形式・MID 表 (Fusion Cfg.ini 読み出し)。
//! - Linux hid-pulsar.c (Nikolas Koesling) — 応答長・チェックサム・バッテリー形式。
//!
//! ## ワイヤ形式 (mouse.fit / ATK と同じ系列)
//!
//! - 設定コレクション usage_page 0xFF02 / usage 2、output/input report id 8 (17B)。
//! - 要求  `[8, cmd, b2, b3, b4, b5, data…]`、cksum = `0x55 - sum(frame[0..16])`
//!   (17B 全体の和 ≡ 0x55)。
//! - 応答  `[8, cmd echo, status, …, data@resp[6..], cksum]`。
//!   MEM_GET は要求の addr/len エコー (resp[3..6]) で他コマンドの応答と区別する。
//! - 設定は 16bit アドレスのフラットなメモリマップ = **アクティブプロファイルの中身**。
//!   cmd8 MEM_GET (≤10B) / cmd7 MEM_SET (≤10B)。スカラは (v, 0x55-v) ペア。
//! - 自発通知 cmd 0x0A (b6: 1=DPI 変更, 4=プロファイル変更) — コマンド応答と
//!   同じ report に混ざるので、cmd echo が一致するまで読み飛ばす。
//! - 休眠中 (ドングル経由) はコマンドがタイムアウトする → `asleep`。
//!
//! ## 実機メモリマップ (Xlite V3, アクティブプロファイル)
//!
//! ```text
//! 0x00 ポーリング (0x20=4K 実測)      0x02 DPI 段数      0x04 現在段 (0 始まり)
//! 0x0A LOD {1=1mm, 2=2mm}
//! 0x0C + i*4  DPI 段 i [lo, lo, high, crc]  (dpi=(lo+1+256*page)*50)
//! 0x2C + i*4  段色 i [R, G, B, crc(0x55-(r+g+b))]
//! 0x4C LED 効果 {1:静的, 2:ブリージング}   0x4E 輝度 0-255
//! 0x50 ブリージング速度 1-5               0x52 LED 有効 0/1
//! 0x60 + slot*4 ボタン [mode, a1, a2, crc]  (KM=10: 6 スロット, 0x78/0x7C は別用途)
//! 0xA9 デバウンス ms (0-30)   0xAB MotionSync   0xAF 直線補正   0xB1 リップル
//! 0xB7 スリープ (10 秒単位)
//! 0x100 + slot*32 ショートカットレコード [count][押下…][解放(逆順)][crc]
//! ```
//!
//! ボタン mode: 0=無効 1=マウス(bitmask) 2=DPI(a1: 1=循環 2=+ 3=-) 5=ショートカット
//! (a1a2=レコードポインタ) 9=プロファイル切替 0xA=DPI ロック (a1 = dpi/50-1)。

use devhub_core::backend::*;
use devhub_core::model::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

const RID: u8 = 8;
const CHUNK: u16 = 10;
/// メモリマップの読み出し範囲: 0x00..=0xB8 を 10B チャンクで (0xC1 まで)。
/// 0xB7 のスリープ値 + チェックサムペアまで含む。
const MAP_END: u16 = 0xB8;
/// 1 コマンドの応答待ち (1 試行あたり)。
const RESP_MS: u64 = 300;
/// 通常コマンドの再試行回数。
const TRIES: u8 = 2;

// ---------------------------------------------------------------- コマンド

mod cmd {
    /// handshake — b5=8 + nonce4 → CID=r[10], MID=r[11] (r[2]=status, 0 が正常)。
    pub const HANDSHAKE: u8 = 0x01;
    /// 電源 — r[6]=%, r[7]=充電中, r[8..10]=mV (BE)。
    pub const POWER: u8 = 0x04;
    /// メモリ書き込み (b3=addrHi, b4=addrLo, b5=len, data≤10)。
    pub const MEM_SET: u8 = 0x07;
    /// メモリ読み出し (同上。データは r[6..6+len])。
    pub const MEM_GET: u8 = 0x08;
    /// 工場出荷リセット。
    pub const RESTORE: u8 = 0x09;
    /// 自発通知 (r[6]: 1=DPI 変更, 4=プロファイル変更)。
    pub const EVENT: u8 = 0x0A;
    /// アクティブプロファイル取得 — r[6] = 0 始まりの番号。
    pub const PROFILE_GET: u8 = 0x0E;
    /// アクティブプロファイル切替 — b5=1, b6=番号 (0 始まり)。
    /// 応答 r[2]=status (0 が正常)。直後に 0x0A イベントが 1 通来る。
    pub const PROFILE_SET: u8 = 0x0F;
    /// FW バージョン — r[6], r[7] はパックド 10 進 ("01 21" → 1.21)。
    pub const FW: u8 = 0x12;
}

// ---------------------------------------------------------------- メモリマップ

#[allow(dead_code)]
mod ee {
    pub const POLLING: u16 = 0x00;
    pub const STAGE_COUNT: u16 = 0x02;
    pub const ACTIVE_STAGE: u16 = 0x04;
    pub const LOD: u16 = 0x0A;
    /// DPI 段 4B レコード [lo, lo, high, crc] × 4。
    pub const DPI_BASE: u16 = 0x0C;
    /// 段色 4B レコード [R, G, B, crc] × 4。
    pub const COLOR_BASE: u16 = 0x2C;
    /// LED 効果 {1: 静的, 2: ブリージング}。
    pub const LED_EFFECT: u16 = 0x4C;
    pub const LED_BRIGHTNESS: u16 = 0x4E;
    pub const LED_BREATHE_SPEED: u16 = 0x50;
    pub const LED_ENABLED: u16 = 0x52;
    /// ボタン 4B レコード [mode, a1, a2, crc] × スロット。
    pub const BUTTON_BASE: u16 = 0x60;
    pub const DEBOUNCE: u16 = 0xA9;
    pub const MOTION_SYNC: u16 = 0xAB;
    pub const ANGLE_SNAP: u16 = 0xAF;
    pub const RIPPLE: u16 = 0xB1;
    /// スリープ (10 秒単位、1..=60)。
    pub const AUTOSLEEP: u16 = 0xB7;
    /// ショートカットレコード 32B × ボタンスロット。
    pub const SHORTCUT_BASE: u16 = 0x100;
}

/// cmd1 応答の MID → (model, 表示名, KM=ボタンマトリクス)。
/// CID は全 Nordic 機で 6。KM=10 は 6 ボタン (右側面ペアは FW が別用途に使う —
/// 読み書きしない)、KM=14 は X2A 系の 8 ボタン。
/// 出典: Pulsar Fusion 同梱 Cfg.ini (pulsar-mouse-linux #30 で全 21 機種を読み出し)。
fn mid_lookup(mid: u8) -> Option<(&'static str, &'static str, u8)> {
    Some(match mid {
        0x01 => ("xlite", "Pulsar Xlite", 10),
        0x02 => ("xlite-mini", "Pulsar Xlite Mini", 10),
        0x03 => ("x2-mini", "Pulsar X2 Mini", 10),
        0x04 => ("x2", "Pulsar X2", 10),
        0x05 => ("x2-mini-bruce-lee", "Pulsar X2 Mini Bruce Lee", 10),
        0x06 => ("x2-bruce-lee", "Pulsar X2 Bruce Lee", 10),
        0x07 => ("xlite-v3", "Pulsar Xlite V3", 10),
        0x08 => ("x2a", "Pulsar X2A", 14),
        0x09 => ("x2a-mini", "Pulsar X2A Mini", 14),
        0x0A => ("x2v2", "Pulsar X2V2", 10),
        0x0B => ("x2v2-mini", "Pulsar X2V2 Mini", 10),
        0x0C => ("x2h", "Pulsar X2H", 10),
        0x0D => ("x2h-mini", "Pulsar X2H Mini", 10),
        0x0E => ("xlite-v3-mini", "Pulsar Xlite V3 Mini", 10),
        0x0F => ("xlite-v3-large", "Pulsar Xlite V3 Large", 10),
        0x10 => ("xlite-v3-custom-optical", "Pulsar Xlite V3 Custom Optical", 10),
        0x11 => ("xlite-v3-custom-mech", "Pulsar Xlite V3 Custom Mech", 10),
        0x12 => ("x2-custom-optical", "Pulsar X2 Custom Optical", 14),
        0x13 => ("x2-custom-mech", "Pulsar X2 Custom Mech", 14),
        0x14 => ("x2-custom-optical-km10", "Pulsar X2 Custom Optical", 10),
        0x15 => ("x2-custom-mech-km10", "Pulsar X2 Custom Mech", 10),
        _ => return None,
    })
}

/// ポーリングレート ↔ wire コード。>1K は bit4/5/6 の one-hot
/// (X2 CrazyLight の Fusion キャプチャで確認、本実機も 4K=0x20 を保持)。
fn poll_code(hz: i64) -> Option<u8> {
    Some(match hz {
        125 => 8,
        250 => 4,
        500 => 2,
        1000 => 1,
        2000 => 0x10,
        4000 => 0x20,
        8000 => 0x40,
        _ => return None,
    })
}
fn poll_hz(code: u8) -> Option<i64> {
    Some(match code {
        8 => 125,
        4 => 250,
        2 => 500,
        1 => 1000,
        0x10 => 2000,
        0x20 => 4000,
        0x40 => 8000,
        _ => return None,
    })
}

// ==================================================================== 構造

pub struct PulsarDriver {
    dev: Mutex<hidapi::HidDevice>,
    info: Mutex<DeviceInfo>,
    caps: Mutex<Capabilities>,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    /// CID/MID で機種確定済みか (ドングル + 休眠中は応答が取れないので後で再試行)。
    detected: AtomicBool,
    /// 確定した (model, 表示名) を Hub 側へ書き戻す共有マップ —
    /// ドライバ再生成時に registry 既定へ戻らないようにする (mousefit8 と同じ仕組み)。
    detect_sink: Arc<Mutex<HashMap<String, (String, String)>>>,
}

pub fn open(
    dev: hidapi::HidDevice,
    info: DeviceInfo,
    events: Option<Arc<dyn Fn(HubEvent) + Send + Sync>>,
    detect_sink: Arc<Mutex<HashMap<String, (String, String)>>>,
) -> HubResult<PulsarDriver> {
    dev.set_blocking_mode(false).ok();
    let km = 10; // 確定前の既定 (21 機種中 17 が KM=10)
    let d = PulsarDriver {
        dev: Mutex::new(dev),
        caps: Mutex::new(build_caps(km)),
        info: Mutex::new(info),
        events,
        detected: AtomicBool::new(false),
        detect_sink,
    };
    // 有線は即応答、ドングル + 休眠中は失敗して後で再試行。
    d.maybe_detect();
    Ok(d)
}

impl PulsarDriver {
    /// `info` の変更を UI へ通知。
    fn emit_info(&self, i: DeviceInfo) {
        if let Some(ev) = &self.events {
            ev(HubEvent::Info(i.id.clone(), Box::new(i)));
        }
    }

    /// 本体側で設定が変わったことを UI へ通知 (cmd 0x0A を受けて)。
    fn emit_changed(&self) {
        if let Some(ev) = &self.events {
            let id = self.info.lock().unwrap_or_else(|p| p.into_inner()).id.clone();
            ev(HubEvent::StateChanged(id));
        }
    }

    /// 休眠フラグの更新 (ドングル経由のみ意味を持つ)。
    fn set_asleep(&self, asleep: bool) {
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
        if i.transport != Transport::Wireless24G || i.asleep == asleep {
            return;
        }
        i.asleep = asleep;
        let info = i.clone();
        drop(i);
        self.emit_info(info);
    }

    /// cmd1 handshake → CID/MID で機種確定。
    /// MID=0 は休眠中の値なので確定に使わない (nordic.py と同じ扱い)。
    fn maybe_detect(&self) {
        if self.detected.load(Ordering::Relaxed) {
            return;
        }
        // nonce はランダムでよい (応答の nonce 変換は検証不要 — #30 の 2 キャプチャで
        // 何を送っても CID/MID が返ることが確認済み)。
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0xA5);
        let nonce = [
            (nanos & 0xff) as u8,
            ((nanos >> 8) & 0xff) as u8,
            ((nanos >> 16) & 0xff) as u8,
            ((nanos >> 24) & 0xff) as u8,
        ];
        let Ok(r) = self.cmd_raw(cmd::HANDSHAKE, 0, 0, 8, &nonce, TRIES) else {
            return;
        };
        if r[2] != 0 || r[11] == 0 {
            return; // status 非 0 / MID 不明 — 次の機会に再試行
        }
        self.detected.store(true, Ordering::Relaxed);
        let (cid, mid) = (r[10], r[11]);
        let Some((model, name, km)) = mid_lookup(mid) else {
            return; // 表に無い新機種 — 既定のまま使う
        };
        let _ = cid; // CID=6 は全機種共通
        // Hub 側にも記録 (ドライバ再生成時に復元される)。
        {
            let id = self
                .info
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .id
                .0
                .clone();
            self.detect_sink
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, (model.into(), name.into()));
        }
        let info = {
            let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner());
            if i.model != model {
                i.model = model.into();
                i.name = name.into();
            }
            i.clone()
        };
        *self.caps.lock().unwrap_or_else(|p| p.into_inner()) = build_caps(km);
        self.emit_info(info);
    }

    /// 17B コマンド → 17B 応答。cmd echo (MEM_GET は addr/len エコーも) が
    /// 一致するまで読み飛ばし、途中の 0x0A イベントは StateChanged に変換する。
    fn cmd_raw(
        &self,
        c: u8,
        b3: u8,
        b4: u8,
        b5: u8,
        data: &[u8],
        tries: u8,
    ) -> HubResult<Vec<u8>> {
        let mut f = [0u8; 17];
        f[0] = RID;
        f[1] = c;
        f[3] = b3;
        f[4] = b4;
        f[5] = b5;
        let n = data.len().min(10);
        f[6..6 + n].copy_from_slice(&data[..n]);
        f[16] = cksum(&f);
        let mut evs: Vec<u8> = Vec::new();
        let mut resp: Option<Vec<u8>> = None;
        {
            let dev = self.dev.lock().unwrap_or_else(|p| p.into_inner());
            'attempt: for _ in 0..tries {
                dev.write(&f)
                    .map_err(|e| HubError::Io(format!("pulsar write: {e}")))?;
                let deadline = Instant::now() + Duration::from_millis(RESP_MS);
                loop {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let mut buf = [0u8; 64];
                    match dev.read_timeout(&mut buf, left.as_millis().clamp(1, 120) as i32) {
                        Ok(n) if n >= 17 && buf[0] == RID => {
                            let r = &buf[..17];
                            if cksum(r) != r[16] {
                                continue; // 壊れたフレームは読み捨て
                            }
                            if r[1] == cmd::EVENT {
                                evs.push(r[6]);
                                continue;
                            }
                            if r[1] != c {
                                continue;
                            }
                            if c == cmd::MEM_GET && (r[3] != b3 || r[4] != b4 || r[5] != b5) {
                                continue;
                            }
                            resp = Some(r.to_vec());
                            break 'attempt;
                        }
                        _ => {
                            sleep(Duration::from_millis(1));
                            continue;
                        }
                    }
                }
                sleep(Duration::from_millis(10));
            }
        }
        match resp {
            Some(r) => {
                self.set_asleep(false);
                // 本体側変更 (DPI/プロファイル) はコマンド完了後に通知 —
                // cmd echo 待ちの途中で UI に呼び戻させないため。
                for k in evs {
                    if k == 1 || k == 4 {
                        self.emit_changed();
                    }
                }
                Ok(r)
            }
            None => {
                // ドングル経由で応答が無い = マウス休眠中とみなす。
                self.set_asleep(true);
                Err(HubError::Protocol(format!(
                    "pulsar: cmd 0x{c:02x} 応答なし"
                )))
            }
        }
    }

    /// メモリ読み出し (10B チャンク、内部結合)。データは resp[6..]。
    fn mem_read(&self, addr: u16, len: u16) -> HubResult<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        let mut a = addr;
        while (out.len() as u16) < len {
            let want = (len - out.len() as u16).min(CHUNK) as u8;
            let r = self.cmd_raw(
                cmd::MEM_GET,
                (a >> 8) as u8,
                a as u8,
                want,
                &[],
                TRIES,
            )?;
            let n = (r[5] as usize).min(want as usize).min(10);
            if n == 0 {
                return Err(HubError::Protocol(format!("mem_read @{addr:#06x}: 短い応答")));
            }
            out.extend_from_slice(&r[6..6 + n]);
            if n < want as usize {
                return Err(HubError::Protocol(format!("mem_read @{addr:#06x}: {n}B < {want}B")));
            }
            a += want as u16;
        }
        Ok(out)
    }

    /// メモリ書き込み (cmd7、10B チャンク)。
    fn mem_write(&self, addr: u16, data: &[u8]) -> HubResult<()> {
        for (i, chunk) in data.chunks(CHUNK as usize).enumerate() {
            let a = addr + (i as u16) * CHUNK;
            self.cmd_raw(
                cmd::MEM_SET,
                (a >> 8) as u8,
                a as u8,
                chunk.len() as u8,
                chunk,
                TRIES,
            )?;
            sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// スカラ (v, 0x55-v) ペア書き込み。
    fn write_pair(&self, addr: u16, v: u8) -> HubResult<()> {
        self.mem_write(addr, &[v, 85u8.wrapping_sub(v)])
    }

    /// アクティブプロファイルのメモリマップ全体 (0x00..0xC1) を読む。
    fn dump_map(&self) -> HubResult<Vec<u8>> {
        let mut m = Vec::with_capacity((MAP_END + CHUNK) as usize);
        let mut a: u16 = 0;
        while a <= MAP_END {
            m.extend(self.mem_read(a, CHUNK)?);
            a += CHUNK;
        }
        Ok(m)
    }

    /// ショートカットレコード (0x100 + slot*32) を読んで KeyStroke/MediaFn に復元。
    /// 形式: [count][押下 (0x80|type, val LE16)…][解放 (逆順, 0x40|type)…][crc]。
    /// type: 0=修飾 (val=bit), 1=キー (HID usage), 2=コンシューマ (メディア)。
    fn read_shortcut(&self, slot: usize) -> HubResult<Option<ButtonAction>> {
        let base = ee::SHORTCUT_BASE + (slot as u16) * 32;
        let b = self.mem_read(base, 32)?;
        Ok(parse_shortcut(&b))
    }

    /// ボタンスロット 4B レコード → ButtonAction (mode 5 はレコード領域を別途読む)。
    fn decode_button(&self, slot: usize, rec: &[u8]) -> HubResult<ButtonAction> {
        if rec.len() < 3 {
            return Ok(ButtonAction::Disabled);
        }
        let (mode, a1, a2) = (rec[0], rec[1], rec[2]);
        Ok(match mode {
            0 => ButtonAction::Disabled,
            1 => match a1 {
                1 => ButtonAction::Mouse(MouseFn::Left),
                2 => ButtonAction::Mouse(MouseFn::Right),
                4 => ButtonAction::Mouse(MouseFn::Middle),
                8 => ButtonAction::Mouse(MouseFn::Back),
                16 => ButtonAction::Mouse(MouseFn::Forward),
                _ => ButtonAction::Unsupported {
                    ty: mode,
                    params: vec![a1, a2],
                },
            },
            2 => ButtonAction::Dpi(match a1 {
                2 => DpiFn::Up,
                3 => DpiFn::Down,
                _ => DpiFn::Cycle,
            }),
            5 => self.read_shortcut(slot)?.unwrap_or(ButtonAction::Disabled),
            9 => ButtonAction::ProfileCycle,
            0xA => ButtonAction::Dpi(DpiFn::Shift(((a1 as u32) + 1) * 50)),
            _ => ButtonAction::Unsupported {
                ty: mode,
                params: vec![a1, a2],
            },
        })
    }

    /// ButtonAction → 4B レコード + (ショートカット系は) レコード領域への書き込み。
    fn encode_button(
        &self,
        _slot: usize,
        a: &ButtonAction,
    ) -> HubResult<([u8; 4], Option<Vec<u8>>)> {
        let rec = |ty: u8, a1: u8, a2: u8| {
            let mut r = [ty, a1, a2, 0];
            r[3] = 85u8.wrapping_sub(r[..3].iter().map(|&x| x as u32).sum::<u32>() as u8);
            r
        };
        use ButtonAction::*;
        Ok(match a {
            Disabled => (rec(0, 0, 0), None),
            Mouse(MouseFn::Left) => (rec(1, 1, 0), None),
            Mouse(MouseFn::Right) => (rec(1, 2, 0), None),
            Mouse(MouseFn::Middle) => (rec(1, 4, 0), None),
            Mouse(MouseFn::Back) => (rec(1, 8, 0), None),
            Mouse(MouseFn::Forward) => (rec(1, 0x10, 0), None),
            Mouse(_) => return Err(HubError::Unsupported("このマウスのボタン動作")),
            Dpi(DpiFn::Cycle) => (rec(2, 1, 0), None),
            Dpi(DpiFn::Up) => (rec(2, 2, 0), None),
            Dpi(DpiFn::Down) => (rec(2, 3, 0), None),
            // DPI ロック: a1 = dpi/50 - 1 (50..=1100)。
            Dpi(DpiFn::Shift(d)) => {
                let v = *d;
                if v % 50 != 0 || !(50..=1100).contains(&v) {
                    return Err(HubError::Invalid("DPI ロックは 50-1100 (50 刻み) です".into()));
                }
                (rec(0xA, (v / 50 - 1) as u8, 0), None)
            }
            Key(k) => {
                let blob = pack_key_shortcut(k);
                (rec(5, 0, 0), Some(blob))
            }
            Media(m) => {
                let blob = pack_media_shortcut(*m);
                (rec(5, 0, 0), Some(blob))
            }
            ProfileCycle => (rec(9, 0, 0), None),
            Unsupported { ty, params } => {
                if params.len() >= 2 {
                    (rec(*ty, params[0], params[1]), None)
                } else {
                    return Err(HubError::Unsupported("raw action"));
                }
            }
            _ => return Err(HubError::Unsupported("このマウスのボタン動作")),
        })
    }

    /// ショートカットレコード領域 (32B、0xFF パディング) を書いてからスロットを指す。
    /// a1/a2 にはレコード先頭のポインタを入れる (公式 Fusion は 00 00 を残すが、
    /// 読み側はポインタ優先・領域アドレスにフォールバックするのでどちらでも可)。
    fn write_button(&self, slot: usize, action: &ButtonAction) -> HubResult<()> {
        let (rec, shortcut) = self.encode_button(slot, action)?;
        let addr = ee::BUTTON_BASE + (slot as u16) * 4;
        if let Some(blob) = shortcut {
            let sc = ee::SHORTCUT_BASE + (slot as u16) * 32;
            let mut full = blob;
            full.resize(32, 0xff);
            self.mem_write(sc, &full)?;
        }
        self.mem_write(addr, &rec)
    }
}

// ---------------------------------------------------------------- フレーム

/// 17B フレームのチェックサム: `0x55 - sum(frame[0..16])`。
/// 17B 全体 (report id 込み) の和が 0x55 になる規約 (atk/mousefit8 と同じ)。
fn cksum(f: &[u8]) -> u8 {
    let s: u32 = f[..16].iter().map(|&b| b as u32).sum();
    85u32.wrapping_sub(s) as u8
}

// ---------------------------------------------------------------- DPI 段

/// 4bit を左に 2 回転 (自己逆変換)。
fn rot2(n: u8) -> u8 {
    ((n << 2) | (n >> 2)) & 0x0F
}

/// DPI → 段レコード 3B [lo, lo, high]。
/// dpi = (lo + 1 + 256*page) * 50。high の 4bit ニブルは同じ値を 2 つ並べ、
/// rot2 すると [mode:2][page:2] になる (mode=0 = 50 刻み)。
fn dpi_to_raw(dpi: u32) -> Option<[u8; 3]> {
    if !dpi.is_multiple_of(50) || !(50..=26000).contains(&dpi) {
        return None;
    }
    let index = dpi / 50 - 1; // 0..=519 → 10bit
    let page = (index >> 8) as u8;
    let low = (index & 0xff) as u8;
    let nib = rot2(page);
    Some([low, low, (nib << 4) | nib])
}

/// 段レコード 3B → DPI。
fn raw_to_dpi(raw: &[u8]) -> u32 {
    let nib = raw[2] & 0x0F;
    let high = rot2(nib);
    let page = (high & 0x03) as u32;
    // mode = high >> 2 — 実機は常に 0。未知 mode は 50 刻みとして解釈する。
    (raw[0] as u32 + 1 + 256 * page) * 50
}

// ---------------------------------------------------------------- ショートカット

/// ショートカットイベントの種別。
const HID_EVT_MOD: u8 = 0;
const HID_EVT_KEY: u8 = 1;
const HID_EVT_CONSUMER: u8 = 2;

/// `[(type, val16)]` → レコード本体 (crc 付き、パディング無し)。
/// count = 押下 + 解放のエントリ総数。解放は押しの逆順 (公式 Bibimbap 互換)。
fn pack_shortcut(events: &[(u8, u16)]) -> Vec<u8> {
    let mut b = vec![(events.len() * 2) as u8];
    for &(t, v) in events {
        b.extend_from_slice(&[0x80 | t, (v & 0xff) as u8, (v >> 8) as u8]);
    }
    for &(t, v) in events.iter().rev() {
        b.extend_from_slice(&[0x40 | t, (v & 0xff) as u8, (v >> 8) as u8]);
    }
    let crc = 85u8.wrapping_sub(b.iter().map(|&x| x as u32).sum::<u32>() as u8);
    b.push(crc);
    b
}

/// KeyStroke → レコード (修飾ビットを押してからキー)。
fn pack_key_shortcut(k: &KeyStroke) -> Vec<u8> {
    let mut ev: Vec<(u8, u16)> = Vec::new();
    for bit in [1u8, 2, 4, 8, 0x10, 0x20, 0x40, 0x80] {
        if k.mods & bit != 0 {
            ev.push((HID_EVT_MOD, bit as u16));
        }
    }
    if k.usage != 0 {
        ev.push((HID_EVT_KEY, k.usage));
    }
    pack_shortcut(&ev)
}

/// MediaFn → レコード (コンシューマ usage 1 個)。
/// 使用コードは mousefit8 と同じ標準 HID Consumer usage (同系列で実機確認済み)。
fn pack_media_shortcut(m: MediaFn) -> Vec<u8> {
    pack_shortcut(&[(HID_EVT_CONSUMER, media_code(m))])
}

/// レコード 32B → KeyStroke / MediaFn。
fn parse_shortcut(b: &[u8]) -> Option<ButtonAction> {
    if b.is_empty() {
        return None;
    }
    let n = (b[0] / 2) as usize; // 押下エントリ数
    let mut mods = 0u8;
    let mut usage = 0u16;
    let mut media: Option<MediaFn> = None;
    for i in 0..n {
        let o = 1 + i * 3;
        if o + 2 >= b.len() {
            break;
        }
        let flags = b[o];
        if flags & 0x80 == 0 {
            continue; // 押下エントリだけ見る
        }
        let t = flags & 0x0f;
        let v = b[o + 1] as u16 | ((b[o + 2] as u16) << 8);
        match t {
            HID_EVT_MOD => mods |= v as u8,
            HID_EVT_KEY => usage = v,
            HID_EVT_CONSUMER => media = media_of_code(v),
            _ => {}
        }
    }
    if let Some(m) = media {
        return Some(ButtonAction::Media(m));
    }
    if usage != 0 || mods != 0 {
        return Some(ButtonAction::Key(KeyStroke { mods, usage }));
    }
    None
}

fn media_code(m: MediaFn) -> u16 {
    match m {
        MediaFn::PlayPause => 0xCD,
        MediaFn::Stop => 0xB7,
        MediaFn::Next => 0xB5,
        MediaFn::Prev => 0xB6,
        MediaFn::VolumeUp => 0xE9,
        MediaFn::VolumeDown => 0xEA,
        MediaFn::Mute => 0xE2,
        MediaFn::Calculator => 0x192,
        MediaFn::MyComputer => 0x194,
        MediaFn::Browser => 0x196,
        MediaFn::Mail => 0x18A,
        MediaFn::BrightnessUp => 0x6F,
        MediaFn::BrightnessDown => 0x70,
        MediaFn::Refresh => 0x7A,
    }
}
fn media_of_code(c: u16) -> Option<MediaFn> {
    Some(match c {
        0xCD => MediaFn::PlayPause,
        0xB7 => MediaFn::Stop,
        0xB5 => MediaFn::Next,
        0xB6 => MediaFn::Prev,
        0xE9 => MediaFn::VolumeUp,
        0xEA => MediaFn::VolumeDown,
        0xE2 => MediaFn::Mute,
        0x192 => MediaFn::Calculator,
        0x194 => MediaFn::MyComputer,
        0x196 => MediaFn::Browser,
        0x18A => MediaFn::Mail,
        _ => return None,
    })
}

// ---------------------------------------------------------------- メモリ解釈

/// ペア検証付きスカラ取得。
fn pair_at(m: &[u8], addr: u16) -> Option<u8> {
    let i = addr as usize;
    if i + 1 >= m.len() {
        return None;
    }
    let (v, c) = (m[i], m[i + 1]);
    ((v as u16 + c as u16) & 0xff == 0x55).then_some(v)
}

// ==================================================================== caps

fn opts(v: &[(i64, &str)]) -> Vec<ChoiceOpt> {
    v.iter().map(|(a, b)| ChoiceOpt::new(*a, *b)).collect()
}

/// KM (ボタンマトリクス) から caps を組む。KM=10 は 6 ボタン、14 は 8 ボタン。
fn build_caps(km: u8) -> Capabilities {
    use section::*;
    let mut buttons: Vec<ButtonSlot> = [
        "左クリック",
        "右クリック",
        "ホイールクリック",
        "サイド (戻る)",
        "サイド (進む)",
        "DPI ボタン",
    ]
    .iter()
    .map(|n| ButtonSlot { name: (*n).into() })
    .collect();
    if km == 14 {
        for n in ["サイド右 (戻る)", "サイド右 (進む)"] {
            buttons.push(ButtonSlot { name: n.into() });
        }
    }
    Capabilities {
        settings: vec![
            SettingSpec::choice(
                "polling_wireless",
                PERFORMANCE,
                "ポーリングレート",
                opts(&[
                    (125, "125"),
                    (250, "250"),
                    (500, "500"),
                    (1000, "1K"),
                    (2000, "2K"),
                    (4000, "4K"),
                ]),
            )
            .help("マウスの報告頻度 (Hz)。2K/4K は 4K ドングル使用時"),
            SettingSpec::choice("lod", SENSOR, "リフトオフ距離", opts(&[(1, "1mm"), (2, "2mm")]))
                .help("持ち上げて反応しなくなる高さ"),
            SettingSpec::toggle("motion_sync", SENSOR, "モーション同期")
                .help("ポインタとマウスの動きを同期させ、軌跡を滑らかにします"),
            SettingSpec::toggle("angle_snap", SENSOR, "直線補正")
                .help("移動のブレを直線に補正します"),
            SettingSpec::toggle("ripple", SENSOR, "リップル制御")
                .help("高DPI時の細かい揺れを抑えます"),
            SettingSpec::slider("debounce", CLICK, "クリックデバウンス", 0., 30., 1., "ms")
                .help("ボタンのチャタリング防止 (@0xA9)"),
            SettingSpec::slider("sleep_sec", POWER, "スリープまでの時間", 10., 600., 10., "秒")
                .help("無操作でスリープするまで (@0xB7、10秒単位)"),
        ],
        actions: vec![
            ActionSpec::new("factory_reset", SYSTEM, "工場出荷状態に戻す")
                .help("全プロファイルの設定を初期値に戻します (cmd9)")
                .danger("このデバイスの全設定が消去されます"),
        ],
        profiles: Some(ProfileCaps {
            count: 4,
            renamable: false,
        }),
        mouse: Some(MouseCaps {
            dpi_min: 50,
            dpi_max: 26000,
            dpi_step: 50,
            min_stages: 1,
            max_stages: 4,
            xy_split: false,
            stage_colors: true,
            stage_toggle: false,
            buttons,
            button_kinds: vec![
                ButtonActionKind::Mouse,
                ButtonActionKind::Dpi,
                ButtonActionKind::Key,
                ButtonActionKind::Media,
                ButtonActionKind::ProfileCycle,
                ButtonActionKind::Disabled,
            ],
            require_left_click: false,
            macros: None,
            live_buttons: vec![],
            adaptive_dpi: None,
        }),
        keyboard: None,
        lighting: vec![LightArea {
            id: 0,
            name: "本体 LED".into(),
            effects: vec![
                LightEffect::new(1, "静的点灯").color().still(),
                LightEffect::new(2, "ブリージング").color(),
            ],
            brightness_max: 255,
            speed_max: 5,
            directions: vec![],
            switchable: true,
            zones: vec![],
            per_key: false,
            direct_drive: false,
            live_readback: false,
        }],
        battery: true,
        peq: None,
        firmware_feed: None,
    }
}

// ==================================================================== DeviceDriver

impl DeviceDriver for PulsarDriver {
    fn info(&self) -> DeviceInfo {
        self.maybe_detect();
        let mut i = self.info.lock().unwrap_or_else(|p| p.into_inner()).clone();
        // FW (cmd 0x12) — パックド 10 進 ("01 21" → 1.21)。
        match self.cmd_raw(cmd::FW, 0, 0, 0, &[], TRIES) {
            Ok(r) => {
                i.asleep = false;
                if r[6] != 0 || r[7] != 0 {
                    i.firmware = Some(format!("{}.{:02x}", r[6], r[7]));
                }
            }
            Err(_) => {
                i.asleep = i.transport == Transport::Wireless24G;
            }
        }
        if let Ok(b) = self.battery() {
            i.battery = Some(b);
            i.asleep = false;
        }
        i.supported = true;
        i
    }

    fn caps(&self) -> Capabilities {
        self.caps.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn read(&self) -> HubResult<DeviceState> {
        self.maybe_detect();
        // ドングル + 休眠判定済みなら探査 1 発だけ打って、応答が無ければ早抜け
        // (全メモリ掃射で数十秒待たせない)。
        if self.info.lock().unwrap_or_else(|p| p.into_inner()).asleep
            && self.cmd_raw(cmd::PROFILE_GET, 0, 0, 0, &[], 1).is_err()
        {
            return Err(HubError::Disconnected);
        }
        let m = self.dump_map()?;
        let n_buttons = self
            .caps()
            .mouse
            .as_ref()
            .map(|c| c.buttons.len())
            .unwrap_or(6);

        // ---- DPI 段
        let stage_count = pair_at(&m, ee::STAGE_COUNT).unwrap_or(4).clamp(1, 4) as usize;
        let active = (pair_at(&m, ee::ACTIVE_STAGE).unwrap_or(0) as usize).min(stage_count - 1);
        let mut stages = Vec::with_capacity(stage_count);
        for si in 0..stage_count {
            let dpi_at = (ee::DPI_BASE + (si as u16) * 4) as usize;
            let col_at = (ee::COLOR_BASE + (si as u16) * 4) as usize;
            if dpi_at + 3 >= m.len() || col_at + 3 >= m.len() {
                break;
            }
            stages.push(DpiStage {
                x: raw_to_dpi(&m[dpi_at..dpi_at + 3]),
                y: None,
                color: Rgb {
                    r: m[col_at],
                    g: m[col_at + 1],
                    b: m[col_at + 2],
                },
                enabled: true,
            });
        }

        // ---- ボタン
        let mut buttons = Vec::with_capacity(n_buttons);
        for slot in 0..n_buttons {
            let at = (ee::BUTTON_BASE + (slot as u16) * 4) as usize;
            if at + 3 >= m.len() {
                buttons.push(ButtonAction::Disabled);
                continue;
            }
            buttons.push(self.decode_button(slot, &m[at..at + 4])?);
        }

        // ---- 汎用設定 (読めたものだけ入れる)
        let mut settings = BTreeMap::new();
        if let Some(c) = pair_at(&m, ee::POLLING).and_then(poll_hz) {
            settings.insert("polling_wireless".into(), SettingValue::Choice(c));
        }
        if let Some(v) = pair_at(&m, ee::LOD) {
            if v == 1 || v == 2 {
                settings.insert("lod".into(), SettingValue::Choice(v as i64));
            }
        }
        for (addr, id) in [
            (ee::MOTION_SYNC, "motion_sync"),
            (ee::ANGLE_SNAP, "angle_snap"),
            (ee::RIPPLE, "ripple"),
        ] {
            if let Some(v) = pair_at(&m, addr) {
                settings.insert(id.into(), SettingValue::Bool(v != 0));
            }
        }
        if let Some(v) = pair_at(&m, ee::DEBOUNCE) {
            settings.insert("debounce".into(), SettingValue::Num(v as f32));
        }
        if let Some(v) = pair_at(&m, ee::AUTOSLEEP) {
            // 10 秒単位 → 秒。
            settings.insert("sleep_sec".into(), SettingValue::Num((v as u32 * 10) as f32));
        }

        // ---- ライティング (本体 LED)。色はアクティブ段の色 (@0x2C + active*4)。
        let effect = match pair_at(&m, ee::LED_EFFECT) {
            Some(2) => 2,
            _ => 1,
        };
        let lighting = LightingState {
            on: pair_at(&m, ee::LED_ENABLED).unwrap_or(0) != 0,
            effect,
            color: stages.get(active).map(|s| s.color).unwrap_or_default(),
            brightness: pair_at(&m, ee::LED_BRIGHTNESS).unwrap_or(255),
            speed: pair_at(&m, ee::LED_BREATHE_SPEED).unwrap_or(1).clamp(1, 5),
            direction: 0,
            per_key: BTreeMap::new(),
            zone_on: vec![],
            palette: stages.iter().map(|s| s.color).collect(),
            palette_sel: active as u8,
            color_correction: None,
        };
        let mut lm = BTreeMap::new();
        lm.insert(0u8, lighting);

        // ---- プロファイル (cmd 0x0E、0 始まり)
        let profile = self
            .cmd_raw(cmd::PROFILE_GET, 0, 0, 0, &[], TRIES)
            .ok()
            .and_then(|r| r.get(6).copied())
            .unwrap_or(0)
            .min(3);
        Ok(DeviceState {
            profile: ProfileState {
                active: profile,
                names: vec![],
            },
            settings,
            mouse: Some(MouseState {
                stages,
                active_stage: active,
                buttons,
                macros: vec![],
                adaptive: None,
            }),
            keyboard: None,
            lighting: lm,
            peq: None,
        })
    }

    fn apply(&self, change: &Change) -> HubResult<()> {
        match change {
            Change::Setting { id, value } => match id.as_str() {
                "polling_wireless" => {
                    let code = poll_code(value.as_choice())
                        .ok_or_else(|| HubError::Invalid("ポーリングレートの値".into()))?;
                    self.write_pair(ee::POLLING, code)
                }
                "lod" => {
                    let v = value.as_choice();
                    if v != 1 && v != 2 {
                        return Err(HubError::Invalid("LOD は 1mm / 2mm".into()));
                    }
                    self.write_pair(ee::LOD, v as u8)
                }
                "motion_sync" => self.write_pair(ee::MOTION_SYNC, value.as_bool() as u8),
                "angle_snap" => self.write_pair(ee::ANGLE_SNAP, value.as_bool() as u8),
                "ripple" => self.write_pair(ee::RIPPLE, value.as_bool() as u8),
                "debounce" => {
                    self.write_pair(ee::DEBOUNCE, value.as_num().clamp(0., 30.) as u8)
                }
                "sleep_sec" => {
                    let sec = value.as_num();
                    if !(10. ..=600.).contains(&sec) || sec % 10. != 0. {
                        return Err(HubError::Invalid("スリープは 10-600秒・10秒刻み".into()));
                    }
                    self.write_pair(ee::AUTOSLEEP, (sec / 10.) as u8)
                }
                _ => Err(HubError::Unsupported("setting")),
            },
            Change::DpiStages { stages, active } => {
                let n = stages.len().clamp(1, 4);
                self.write_pair(ee::STAGE_COUNT, n as u8)?;
                self.write_pair(ee::ACTIVE_STAGE, (*active).min(n - 1) as u8)?;
                for (i, s) in stages[..n].iter().enumerate() {
                    let raw = dpi_to_raw(s.x)
                        .ok_or_else(|| HubError::Invalid(format!("DPI {} は 50 刻みで 50-26000", s.x)))?;
                    let mut rec = [raw[0], raw[1], raw[2], 0];
                    rec[3] = 85u8.wrapping_sub(raw.iter().map(|&x| x as u32).sum::<u32>() as u8);
                    self.mem_write(ee::DPI_BASE + (i as u16) * 4, &rec)?;
                    let mut col = [s.color.r, s.color.g, s.color.b, 0];
                    col[3] = 85u8.wrapping_sub(
                        col[..3].iter().map(|&x| x as u32).sum::<u32>() as u8,
                    );
                    self.mem_write(ee::COLOR_BASE + (i as u16) * 4, &col)?;
                }
                Ok(())
            }
            Change::ActiveStage(a) => self.write_pair(ee::ACTIVE_STAGE, *a as u8),
            Change::Button { slot, action } => {
                let n = self
                    .caps()
                    .mouse
                    .as_ref()
                    .map(|c| c.buttons.len())
                    .unwrap_or(6);
                if *slot >= n {
                    return Err(HubError::Invalid("ボタン番号".into()));
                }
                self.write_button(*slot, action)
            }
            Change::Profile(i) => {
                let idx = (*i).min(3);
                let r = self.cmd_raw(cmd::PROFILE_SET, 0, 0, 1, &[idx], TRIES)?;
                if r[2] != 0 {
                    return Err(HubError::Protocol(format!(
                        "プロファイル {idx} へ切替できません (status {})",
                        r[2]
                    )));
                }
                // 切替直後の 0x0A (プロファイル変更) は読み捨てる
                // (呼び出し側は read() で全設定を読み直す)。
                let _ = self.cmd_raw(cmd::PROFILE_GET, 0, 0, 0, &[], 1);
                Ok(())
            }
            Change::Lighting { state, .. } => {
                // on / effect / brightness / speed はスカラペア。
                if state.on {
                    let eff = if state.effect == 2 { 2 } else { 1 };
                    self.write_pair(ee::LED_EFFECT, eff)?;
                    self.write_pair(ee::LED_ENABLED, 1)?;
                } else {
                    self.write_pair(ee::LED_ENABLED, 0)?;
                }
                self.write_pair(ee::LED_BRIGHTNESS, state.brightness)?;
                self.write_pair(ee::LED_BREATHE_SPEED, state.speed.clamp(1, 5))?;
                // color / palette は段色 (@0x2C + i*4) と同じ領域。
                let sel = (state.palette_sel as usize).min(3);
                let c = state.color;
                let mut col = [c.r, c.g, c.b, 0];
                col[3] = 85u8.wrapping_sub(col[..3].iter().map(|&x| x as u32).sum::<u32>() as u8);
                self.mem_write(ee::COLOR_BASE + (sel as u16) * 4, &col)?;
                for (pi, pc) in state.palette.iter().enumerate().take(4) {
                    if pi == sel {
                        continue; // 選択スロットは上で書いた
                    }
                    let mut col = [pc.r, pc.g, pc.b, 0];
                    col[3] =
                        85u8.wrapping_sub(col[..3].iter().map(|&x| x as u32).sum::<u32>() as u8);
                    self.mem_write(ee::COLOR_BASE + (pi as u16) * 4, &col)?;
                }
                Ok(())
            }
            _ => Err(HubError::Unsupported("change")),
        }
    }

    fn run_action(&self, id: &str, _file: Option<&std::path::Path>) -> HubResult<ActionOutcome> {
        match id {
            "factory_reset" => {
                self.cmd_raw(cmd::RESTORE, 0, 0, 0, &[], TRIES)?;
                Ok(ActionOutcome {
                    message: Some("工場出荷状態に戻しました".into()),
                    reload: true,
                })
            }
            _ => Err(HubError::Unsupported("action")),
        }
    }

    fn battery(&self) -> HubResult<Battery> {
        // cmd4: r[6]=%, r[7]=充電中 (有線接続でも USB 給電なら 1)。
        let r = self.cmd_raw(cmd::POWER, 0, 0, 0, &[], TRIES)?;
        Ok(Battery {
            percent: Some(r.get(6).copied().unwrap_or(0).min(100)),
            charging: r.get(7).copied().unwrap_or(0) != 0,
        })
    }
}

// ==================================================================== テスト
//
// 実機 Xlite V3 (3554:F507) から 2026-11-19 に取得したフレーム (fixtures/pulsar/)。
// dump はメモリマップ 0x00..0xBD の生コピー。

#[cfg(test)]
mod tests {
    use super::*;

    const HS_REQ: &[u8] = include_bytes!("../fixtures/pulsar/handshake.req.bin");
    const HS_RESP: &[u8] = include_bytes!("../fixtures/pulsar/handshake.resp.bin");
    const BAT_RESP: &[u8] = include_bytes!("../fixtures/pulsar/battery.resp.bin");
    const FW_RESP: &[u8] = include_bytes!("../fixtures/pulsar/fw.resp.bin");
    const MEM00_REQ: &[u8] = include_bytes!("../fixtures/pulsar/mem00.req.bin");
    const MEM00_RESP: &[u8] = include_bytes!("../fixtures/pulsar/mem00.resp.bin");

    /// 実機ダンプ 0x00..0xBD (プロファイル 1、DPI 400/800/1600/3200)。
    const DUMP: &[u8] = &[
        0x20, 0x35, 0x04, 0x51, 0x01, 0x54, 0x00, 0x55, 0x00, 0x55, 0x01, 0x54, 0x07, 0x07, 0x00,
        0x47, // 0x00: polling 4K, 段数4, 現在1, LOD 1mm, 段0=400
        0x0f, 0x0f, 0x00, 0x37, 0x1f, 0x1f, 0x00, 0x17, 0x3f, 0x3f, 0x00, 0xd7, 0x3f, 0x3f, 0x00,
        0xd7, // 0x10: 段1=800, 段2=1600, 段3=3200, 予備
        0x3f, 0x3f, 0x00, 0xd7, 0x3f, 0x3f, 0x00, 0xd7, 0x3f, 0x3f, 0x00, 0xd7, 0x00, 0xff, 0xff,
        0x57, // 0x20: 予備段, 段0色 (0,255,255)
        0x00, 0x00, 0xff, 0x56, 0xff, 0x00, 0xff, 0x57, 0xff, 0xff, 0xff, 0x58, 0xff, 0xff, 0xff,
        0x58, // 0x30: 段1色 (0,0,255), 段2色 (255,0,255)
        0xff, 0xff, 0xff, 0x58, 0xff, 0xff, 0xff, 0x58, 0xff, 0xff, 0xff, 0x58, 0x02, 0x53, 0xff,
        0x56, // 0x40: 段3色 (255,255,255), 予備, LED 効果=2 (ブリージング)
        0x03, 0x52, 0x01, 0x54, 0xff, 0x00, 0xff, 0x57, 0x00, 0x55, 0x80, 0xd5, 0x03, 0x52, 0x00,
        0x55, // 0x50: ブリージング速度3, LED有効(1), 不明レコード, 不明ペア
        0x01, 0x01, 0x00, 0x53, 0x01, 0x02, 0x00, 0x52, 0x01, 0x04, 0x00, 0x50, 0x01, 0x08, 0x00,
        0x4c, // 0x60: 左/右/中央/戻る
        0x01, 0x10, 0x00, 0x44, 0x02, 0x01, 0x00, 0x52, 0x01, 0x08, 0x00, 0x4c, 0x01, 0x10, 0x00,
        0x44, // 0x70: 進む, DPI循環, (0x78/0x7C は FW 別用途 — 触らない)
        0x07, 0x00, 0x00, 0x4e, 0x08, 0x04, 0x00, 0x49, 0x04, 0x0a, 0x03, 0x44, 0x00, 0x00, 0x00,
        0x55, // 0x80: スロット外
        0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00, 0x55, 0x00, 0x00, 0x00,
        0x55, // 0x90
        0x01, 0xff, 0x00, 0xff, 0x07, 0x09, 0x46, 0x00, 0x55, 0x00, 0x55, 0x01, 0x54, 0x06, 0x4f,
        0x01, // 0xa0: デバウンス0, MotionSync ON, (0xAD=06 不明)
        0x54, 0x01, 0x54, 0x00, 0x55, 0x00, 0x55, 0x06, 0x4f, 0x01, 0x54, 0x0a, 0x4b,
        0xff, // 0xb0: 直線補正ON, リップルON, スリープ6 (=60秒)
    ];

    #[test]
    fn frame_checksum_matches_live_capture() {
        // handshake 要求: 08 01 00 00 00 08 5f 5f 32 5d 00*6 f7
        let mut f = [0u8; 17];
        f[0] = RID;
        f[1] = cmd::HANDSHAKE;
        f[5] = 8;
        f[6..10].copy_from_slice(&[0x5f, 0x5f, 0x32, 0x5d]);
        f[16] = cksum(&f);
        assert_eq!(&f, HS_REQ);
        // MEM_GET (0x00, 10B): 08 08 00 00 00 0a …
        let mut g = [0u8; 17];
        g[0] = RID;
        g[1] = cmd::MEM_GET;
        g[5] = 10;
        g[16] = cksum(&g);
        assert_eq!(&g, MEM00_REQ);
        // 実機応答のチェックサム検証。
        assert_eq!(cksum(HS_RESP), HS_RESP[16]);
        assert_eq!(cksum(BAT_RESP), BAT_RESP[16]);
        assert_eq!(cksum(FW_RESP), FW_RESP[16]);
        assert_eq!(cksum(MEM00_RESP), MEM00_RESP[16]);
    }

    #[test]
    fn parse_handshake() {
        // Xlite V3: CID=6, MID=7, status=0, len エコー=8。
        assert_eq!(HS_RESP[2], 0);
        assert_eq!((HS_RESP[10], HS_RESP[11]), (6, 7));
        assert_eq!(mid_lookup(HS_RESP[11]), Some(("xlite-v3", "Pulsar Xlite V3", 10)));
    }

    #[test]
    fn parse_battery() {
        // r[6]=%, r[7]=充電中, r[8..10]=mV (BE)。実機: 95% 充電中 4247mV。
        let mv = ((BAT_RESP[8] as u16) << 8) | BAT_RESP[9] as u16;
        assert_eq!(BAT_RESP[6], 95);
        assert_eq!(BAT_RESP[7], 1);
        assert_eq!(mv, 4247);
    }

    #[test]
    fn parse_fw() {
        // パックド 10 進: 01 21 → "1.21" (実機)。
        assert_eq!(format!("{}.{:02x}", FW_RESP[6], FW_RESP[7]), "1.21");
    }

    #[test]
    fn mem_get_echo_and_extract() {
        // 要求の addr/len が resp[3..6] にエコーされる。
        assert_eq!(&MEM00_RESP[3..6], &MEM00_REQ[3..6]);
        // データは resp[6..16]。0x00=0x20 (4K), 0x01=0x35 (ペア OK)。
        let data = &MEM00_RESP[6..16];
        assert_eq!(data[0], 0x20);
        assert_eq!(((data[0] as u16 + data[1] as u16) & 0xff), 0x55);
    }

    #[test]
    fn dump_pairs_and_settings() {
        assert_eq!(pair_at(DUMP, ee::POLLING), Some(0x20)); // 4K
        assert_eq!(pair_at(DUMP, ee::STAGE_COUNT), Some(4));
        assert_eq!(pair_at(DUMP, ee::ACTIVE_STAGE), Some(1));
        assert_eq!(pair_at(DUMP, ee::LOD), Some(1)); // 1mm
        assert_eq!(pair_at(DUMP, ee::DEBOUNCE), Some(0));
        assert_eq!(pair_at(DUMP, ee::MOTION_SYNC), Some(1));
        assert_eq!(pair_at(DUMP, ee::ANGLE_SNAP), Some(1));
        assert_eq!(pair_at(DUMP, ee::RIPPLE), Some(1));
        assert_eq!(pair_at(DUMP, ee::LED_EFFECT), Some(2)); // ブリージング
        assert_eq!(pair_at(DUMP, ee::LED_BRIGHTNESS), Some(255)); // 最大輝度
        assert_eq!(pair_at(DUMP, ee::LED_BREATHE_SPEED), Some(3));
        assert_eq!(pair_at(DUMP, ee::LED_ENABLED), Some(1)); // 点灯
        assert_eq!(pair_at(DUMP, ee::AUTOSLEEP), Some(6)); // 60 秒
        assert_eq!(poll_hz(pair_at(DUMP, ee::POLLING).unwrap()), Some(4000));
    }

    #[test]
    fn dump_stages() {
        let expect = [
            (400u32, Rgb::new(0x00, 0xff, 0xff)),
            (800, Rgb::new(0x00, 0x00, 0xff)),
            (1600, Rgb::new(0xff, 0x00, 0xff)),
            (3200, Rgb::new(0xff, 0xff, 0xff)),
        ];
        for (i, (dpi, color)) in expect.iter().enumerate() {
            let at = (ee::DPI_BASE + (i as u16) * 4) as usize;
            let rec = &DUMP[at..at + 4];
            // レコード crc (0x55 - sum(3B))。
            assert_eq!(
                rec[3],
                85u8.wrapping_sub(rec[..3].iter().map(|&x| x as u32).sum::<u32>() as u8)
            );
            assert_eq!(raw_to_dpi(&rec[..3]), *dpi);
            let cat = (ee::COLOR_BASE + (i as u16) * 4) as usize;
            let col = &DUMP[cat..cat + 4];
            assert_eq!(
                col[3],
                85u8.wrapping_sub(col[..3].iter().map(|&x| x as u32).sum::<u32>() as u8)
            );
            assert_eq!((col[0], col[1], col[2]), (color.r, color.g, color.b));
        }
    }

    #[test]
    fn dpi_roundtrip() {
        for &dpi in &[50u32, 400, 800, 1600, 3200, 6400, 12800, 12850, 25600, 26000] {
            let raw = dpi_to_raw(dpi).expect("encodable");
            assert_eq!(raw[0], raw[1]); // XY 同値
            assert_eq!(raw_to_dpi(&raw), dpi);
        }
        // 範囲外・刻み外。
        assert!(dpi_to_raw(0).is_none());
        assert!(dpi_to_raw(26050).is_none());
        assert!(dpi_to_raw(123).is_none());
    }

    #[test]
    fn dpi_pages() {
        // page 拡張: 26000 → index 519 → page 2, lo 7, nib 8 → high 0x88。
        assert_eq!(dpi_to_raw(26000), Some([7, 7, 0x88]));
        // 実機の 400 → lo 7, high 0。
        assert_eq!(dpi_to_raw(400), Some([7, 7, 0x00]));
    }

    #[test]
    fn shortcut_media_matches_live_capture() {
        // #30 の実機キャプチャ (Xlite V3 サイドボタン = 音量±):
        //   vol+ : 02 82 e9 00 42 e9 00 bd
        //   vol- : 02 82 ea 00 42 ea 00 bb
        let up = pack_media_shortcut(MediaFn::VolumeUp);
        assert_eq!(up, vec![0x02, 0x82, 0xe9, 0x00, 0x42, 0xe9, 0x00, 0xbd]);
        let down = pack_media_shortcut(MediaFn::VolumeDown);
        assert_eq!(down, vec![0x02, 0x82, 0xea, 0x00, 0x42, 0xea, 0x00, 0xbb]);
        // 復帰も確認。
        let mut slot = [0xffu8; 32];
        slot[..up.len()].copy_from_slice(&up);
        assert_eq!(parse_shortcut(&slot), Some(ButtonAction::Media(MediaFn::VolumeUp)));
    }

    #[test]
    fn shortcut_key_roundtrip() {
        // Ctrl+A → 修飾 + キー (押下 2 + 解放 2 の count=4)。
        let blob = pack_key_shortcut(&KeyStroke {
            mods: devhub_core::model::mods::LCTRL,
            usage: 0x04, // A
        });
        assert_eq!(blob[0], 4);
        let mut slot = [0xffu8; 32];
        slot[..blob.len()].copy_from_slice(&blob);
        assert_eq!(
            parse_shortcut(&slot),
            Some(ButtonAction::Key(KeyStroke {
                mods: 0x01,
                usage: 0x04
            }))
        );
    }

    #[test]
    fn polling_codes() {
        assert_eq!(poll_code(125), Some(8));
        assert_eq!(poll_code(1000), Some(1));
        assert_eq!(poll_code(2000), Some(0x10));
        assert_eq!(poll_code(4000), Some(0x20));
        assert_eq!(poll_hz(0x20), Some(4000));
        assert!(poll_code(3000).is_none());
    }

    #[test]
    fn caps_per_km() {
        let c10 = build_caps(10);
        assert_eq!(c10.mouse.as_ref().unwrap().buttons.len(), 6);
        let c14 = build_caps(14);
        assert_eq!(c14.mouse.as_ref().unwrap().buttons.len(), 8);
        // 実機の caps 要件: DPI 50-26000・50 刻み・4 段・段色あり。
        let m = c10.mouse.as_ref().unwrap();
        assert_eq!((m.dpi_min, m.dpi_max, m.dpi_step, m.max_stages), (50, 26000, 50, 4));
        assert!(m.stage_colors && !m.xy_split);
        // ライティング領域: 静的 + ブリージング、スイッチあり。
        let l = &c10.lighting[0];
        assert_eq!(l.effects.len(), 2);
        assert!(l.switchable);
        // プロファイル 4。
        assert_eq!(c10.profiles.as_ref().unwrap().count, 4);
    }

    /// ダンプからボタン 6 個をデコード (ショートカットなしのため driver 経由不要の
    /// 純粋関数部分だけ検証 — mode 1/2 のパス)。
    #[test]
    fn dump_button_records() {
        let recs: Vec<[u8; 4]> = (0..6)
            .map(|s| {
                let at = (ee::BUTTON_BASE + (s as u16) * 4) as usize;
                [
                    DUMP[at],
                    DUMP[at + 1],
                    DUMP[at + 2],
                    DUMP[at + 3],
                ]
            })
            .collect();
        assert_eq!(recs[0], [1, 1, 0, 0x53]); // 左
        assert_eq!(recs[1], [1, 2, 0, 0x52]); // 右
        assert_eq!(recs[2], [1, 4, 0, 0x50]); // 中央
        assert_eq!(recs[3], [1, 8, 0, 0x4c]); // 戻る
        assert_eq!(recs[4], [1, 0x10, 0, 0x44]); // 進む
        assert_eq!(recs[5], [2, 1, 0, 0x52]); // DPI 循環
    }

    /// スカラ (v, 0x55-v) ペア生成。
    #[test]
    fn pair_write_bytes() {
        let v = 0x20u8; // 4K
        assert_eq!([v, 85u8.wrapping_sub(v)], [0x20, 0x35]);
        assert_eq!([6u8, 85u8.wrapping_sub(6)], [0x06, 0x4f]); // 実機スリープ
    }
}
