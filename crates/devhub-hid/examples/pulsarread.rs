//! Pulsar ドライバ経路の実機検証 — pulsar::open → caps → read → apply(読み戻し) → 復元。
//! `cargo run -p devhub-hid --example pulsarread`
//!
//! 安全な往復のみ行う: トグル系は反転→読み戻し→復元、スカラは同値書き戻し or ±1→復元。
//! プロファイルは「現在の番号を再設定」するだけ (実質変更なし)。

use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use devhub_hid::pulsar;
use devhub_hid::transport;

fn main() {
    let api = transport::open().expect("hidapi");
    // 有線 F507 優先、なければドングル F508/F509 (同じ 17B 中継プロトコル)。
    let info_raw = api
        .device_list()
        .find(|d| {
            d.vendor_id() == 0x3554
                && matches!(d.product_id(), 0xF507 | 0xF508 | 0xF509)
                && d.usage_page() == 0xff02
                && d.usage() == 2
        })
        .expect("Pulsar ff02/u2 collection (マウスかドングルを挿してください)");
    let transport_kind = match info_raw.product_id() {
        0xF508 | 0xF509 => Transport::Wireless24G,
        _ => Transport::Wired,
    };
    let dev = info_raw.open_device(&api).expect("open");
    let info = DeviceInfo {
        id: DeviceId(format!("pulsar-{:04x}", info_raw.product_id())),
        kind: DeviceKind::Mouse,
        name: "Pulsar?".into(),
        vendor: "Pulsar".into(),
        model: "pulsar-nordic".into(),
        vid: 0x3554,
        pid: info_raw.product_id(),
        transport: transport_kind,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Pulsar,
        supported: false,
        asleep: false,
        battery: None,
        board_id: None,
    };
    let detected = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let d = pulsar::open(dev, info, None, detected).expect("driver");

    let mut i = d.info();
    for _ in 0..15 {
        if i.firmware.is_some() || i.asleep {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
        i = d.info();
    }
    println!(
        "== info: name={} model={} fw={:?} bat={:?} asleep={}",
        i.name, i.model, i.firmware, i.battery, i.asleep
    );
    if i.asleep {
        println!("   (ドングル経由でマウス休眠中 — マウスを動かしてから再実行)");
        return;
    }

    let c = d.caps();
    println!(
        "== caps: settings={} buttons={}",
        c.settings.len(),
        c.mouse.as_ref().map(|m| m.buttons.len()).unwrap_or(0)
    );

    let s = d.read().expect("read");
    let ms = s.mouse.as_ref().unwrap();
    println!("== profile={} stages: active={}", s.profile.active, ms.active_stage);
    for (k, st) in ms.stages.iter().enumerate() {
        println!("   {k}: {} rgb={:?}", st.x, st.color);
    }
    println!("== buttons:");
    for (k, b) in ms.buttons.iter().enumerate() {
        println!("   {k}: {b:?}");
    }
    println!("== settings:");
    for (k, v) in &s.settings {
        println!("   {k} = {v:?}");
    }
    if let Some(l) = s.lighting.get(&0) {
        println!("== lighting: {l:?}");
    }

    // ---- 安全な書き込み往復 ----
    // 1) トグル系: 反転 → 読み戻し → 復元。
    for id in ["motion_sync", "angle_snap", "ripple"] {
        let cur = matches!(s.settings.get(id), Some(SettingValue::Bool(true)));
        println!("== apply {id} -> {}", !cur);
        d.apply(&Change::Setting {
            id: id.into(),
            value: SettingValue::Bool(!cur),
        })
        .expect("apply");
        let s2 = d.read().expect("read2");
        let now = matches!(s2.settings.get(id), Some(SettingValue::Bool(true)));
        println!("   readback: {now} (expect {})", !cur);
        assert_eq!(now, !cur, "{id} roundtrip failed");
        d.apply(&Change::Setting {
            id: id.into(),
            value: SettingValue::Bool(cur),
        })
        .expect("restore");
    }

    // 2) 数値系: debounce を +1 → 読み戻し → 復元。
    if let Some(SettingValue::Num(cur)) = s.settings.get("debounce") {
        let t = if *cur < 30.0 { *cur + 1.0 } else { *cur - 1.0 };
        println!("== apply debounce {cur} -> {t}");
        d.apply(&Change::Setting {
            id: "debounce".into(),
            value: SettingValue::Num(t),
        })
        .expect("apply");
        let s2 = d.read().expect("read2");
        let now = match s2.settings.get("debounce") {
            Some(SettingValue::Num(v)) => *v,
            _ => -1.0,
        };
        println!("   readback: {now}");
        assert_eq!(now, t);
        d.apply(&Change::Setting {
            id: "debounce".into(),
            value: SettingValue::Num(*cur),
        })
        .expect("restore");
    }

    // 3) スリープ: 120秒 にして読み戻し → 復元。
    if let Some(SettingValue::Num(cur)) = s.settings.get("sleep_sec") {
        let cur = *cur;
        println!("== apply sleep_sec {cur} -> 120");
        d.apply(&Change::Setting {
            id: "sleep_sec".into(),
            value: SettingValue::Num(120.),
        })
        .expect("apply");
        let s2 = d.read().expect("read2");
        let now = match s2.settings.get("sleep_sec") {
            Some(SettingValue::Num(v)) => *v,
            _ => -1.0,
        };
        println!("   readback: {now}");
        assert_eq!(now, 120.);
        d.apply(&Change::Setting {
            id: "sleep_sec".into(),
            value: SettingValue::Num(cur),
        })
        .expect("restore");
    }

    // 4) LOD: 反対側 (1<->2) にして読み戻し → 復元。
    if let Some(SettingValue::Choice(cur)) = s.settings.get("lod") {
        let other = if *cur == 1 { 2 } else { 1 };
        println!("== apply lod {cur} -> {other}");
        d.apply(&Change::Setting {
            id: "lod".into(),
            value: SettingValue::Choice(other),
        })
        .expect("apply");
        let s2 = d.read().expect("read2");
        let now = match s2.settings.get("lod") {
            Some(SettingValue::Choice(v)) => *v,
            _ => -1,
        };
        println!("   readback: {now}");
        assert_eq!(now, other);
        d.apply(&Change::Setting {
            id: "lod".into(),
            value: SettingValue::Choice(*cur),
        })
        .expect("restore");
    }

    // 5) ポーリング: 現在値をそのまま書き戻す (ACK 経路の確認だけ)。
    if let Some(SettingValue::Choice(cur)) = s.settings.get("polling_wireless") {
        println!("== apply polling_wireless {cur} (同値書き戻し)");
        d.apply(&Change::Setting {
            id: "polling_wireless".into(),
            value: SettingValue::Choice(*cur),
        })
        .expect("apply");
    }

    // 6) DPI ステージ: 読んだ値をそのまま書き戻す (レコード書き込み経路の確認)。
    let stages = ms.stages.clone();
    let active = ms.active_stage;
    println!("== apply DpiStages (同値書き戻し, active={active})");
    d.apply(&Change::DpiStages {
        stages: stages.clone(),
        active,
    })
    .expect("apply");
    let s2 = d.read().expect("read2");
    let ms2 = s2.mouse.as_ref().unwrap();
    assert_eq!(ms2.stages.len(), stages.len());
    for (a, b) in ms2.stages.iter().zip(stages.iter()) {
        assert_eq!((a.x, a.color), (b.x, b.color), "DPI stage roundtrip failed");
    }

    // 7) プロファイル: 現在の番号を再設定 (メモリマップは同じ内容のまま)。
    let cur_profile = s.profile.active;
    println!("== apply Profile {cur_profile} (同値再設定)");
    d.apply(&Change::Profile(cur_profile)).expect("apply");
    let s3 = d.read().expect("read3");
    assert_eq!(s3.profile.active, cur_profile);

    // 8) サイドボタン (戻る) に Key('a') を書いて読み戻し → 元のアクションに復元。
    let orig = ms.buttons.get(3).cloned();
    if let Some(orig) = orig {
        let a = ButtonAction::Key(KeyStroke { mods: 0, usage: 4 });
        println!("== apply button3 -> {a:?}");
        d.apply(&Change::Button {
            slot: 3,
            action: a.clone(),
        })
        .expect("apply");
        let s4 = d.read().expect("read4");
        println!("   readback: {:?}", s4.mouse.as_ref().unwrap().buttons[3]);
        assert_eq!(s4.mouse.as_ref().unwrap().buttons[3], a);
        d.apply(&Change::Button {
            slot: 3,
            action: orig,
        })
        .expect("restore");
        let s5 = d.read().expect("read5");
        println!("   restored: {:?}", s5.mouse.as_ref().unwrap().buttons[3]);
    }

    // 9) ライティング: 効果を もう一方 (静的<->ブリージング) に切替 → 読み戻し → 復元。
    if let Some(l) = s.lighting.get(&0).cloned() {
        let other_eff = if l.effect == 2 { 1 } else { 2 };
        let mut n = l.clone();
        n.effect = other_eff;
        println!("== apply Lighting effect {} -> {}", l.effect, other_eff);
        d.apply(&Change::Lighting { area: 0, state: n.clone() }).expect("apply");
        let s6 = d.read().expect("read6");
        let now = s6.lighting.get(&0).map(|x| x.effect).unwrap_or(0);
        println!("   readback: {now}");
        assert_eq!(now, other_eff);
        d.apply(&Change::Lighting { area: 0, state: l }).expect("restore");
    }

    // 10) バッテリー。
    let b = d.battery().expect("battery");
    println!("== battery: {b:?}");
    println!("OK");
}
