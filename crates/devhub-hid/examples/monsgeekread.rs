//! MonsGeek ドライバ経路の実機検証 — monsgeek::open → caps → read → apply(同値書き戻し)。
//! `cargo run -p devhub-hid --example monsgeekread`
//!
//! 安全な往復のみ: 全て「現在値と同じ値」を書き戻して ACK 経路と読み戻し一致を確認。

use devhub_core::backend::DeviceDriver;
use devhub_core::model::*;
use devhub_hid::monsgeek;
use devhub_hid::transport;

fn main() {
    let api = transport::open().expect("hidapi");
    let info_raw = api
        .device_list()
        .find(|d| {
            d.vendor_id() == 0x3151
                && d.product_id() == 0x502D
                && d.usage_page() == 0xFFFF
                && d.usage() == 2
        })
        .expect("MonsGeek FFFF/u2 collection (FUN60 を挿してください)");
    let dev = info_raw.open_device(&api).expect("open");
    let info = DeviceInfo {
        id: DeviceId("monsgeek-fun60".into()),
        kind: DeviceKind::Keyboard,
        name: "MonsGeek?".into(),
        vendor: "MonsGeek".into(),
        model: "ry5088".into(),
        vid: 0x3151,
        pid: 0x502D,
        transport: Transport::Wired,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Monsgeek,
        supported: false,
        asleep: false,
        battery: None,
        board_id: None,
    };
    let d = monsgeek::open(dev, info, None).expect("driver");

    let i = d.info();
    println!("== info: name={} model={} fw={:?}", i.name, i.model, i.firmware);

    let c = d.caps();
    println!(
        "== caps: settings={} keys={}",
        c.settings.len(),
        c.keyboard.as_ref().map(|k| k.layout.len()).unwrap_or(0)
    );

    let t0 = std::time::Instant::now();
    let s = d.read().expect("read");
    println!("== read took {:.1}s", t0.elapsed().as_secs_f32());
    let ks = s.keyboard.as_ref().unwrap();
    println!("== profile={} layers={}", s.profile.active, ks.keymap.len());
    println!("== settings:");
    for (k, v) in &s.settings {
        println!("   {k} = {v:?}");
    }
    // レイヤー 0 の一部キー。
    let layout = c.keyboard.as_ref().unwrap().layout.clone();
    for li in 0..ks.keymap.len() {
        let esc = ks.keymap[li]
            .iter()
            .position(|_| true)
            .map(|_| ()) // dummy
            .unwrap_or(());
        let _ = esc;
    }
    for (ki, key) in layout.iter().enumerate().take(8) {
        println!("   L0 {:?} = {:?}", key.label, ks.keymap[0][ki]);
    }
    // per-key 性能 (A キー)。
    let a_pos = layout.iter().position(|k| k.label == "A").unwrap();
    println!("== perf[A]: {:?}", ks.perf[a_pos]);
    let rt_count = ks.perf.iter().filter(|p| p.mode == TriggerMode::Rapid).count();
    println!("== RT 有効キー: {rt_count}/{}", ks.perf.len());
    if let Some(l) = s.lighting.get(&0) {
        println!("== lighting: effect={} on={} rgb={:?}", l.effect, l.on, l.color);
    }

    // ---- 安全な同値書き戻し往復 ----
    // 1) ポーリングレート。
    if let Some(SettingValue::Choice(cur)) = s.settings.get("polling") {
        println!("== apply polling {cur} (同値)");
        d.apply(&Change::Setting {
            id: "polling".into(),
            value: SettingValue::Choice(*cur),
        })
        .expect("apply polling");
        let s2 = d.read().expect("read2");
        assert_eq!(s2.settings.get("polling"), s.settings.get("polling"));
    }

    // 2) RT Stab (KBOPTION は現在値を全項目保持して書く)。
    if let Some(SettingValue::Choice(cur)) = s.settings.get("rt_stab") {
        println!("== apply rt_stab {cur} (同値)");
        d.apply(&Change::Setting {
            id: "rt_stab".into(),
            value: SettingValue::Choice(*cur),
        })
        .expect("apply rt_stab");
        let s2 = d.read().expect("read2");
        assert_eq!(s2.settings.get("rt_stab"), s.settings.get("rt_stab"));
    }

    // 3) キーマップ 1 キー (A) 同値書き戻し。
    let orig = ks.keymap[s.profile.active as usize][a_pos].clone();
    println!("== apply KeyMap A -> {orig:?} (同値)");
    d.apply(&Change::KeyMap {
        layer: s.profile.active,
        key: a_pos,
        action: orig.clone(),
    })
    .expect("apply keymap");
    let s2 = d.read().expect("read2");
    assert_eq!(
        s2.keyboard.as_ref().unwrap().keymap[s.profile.active as usize][a_pos],
        orig,
        "keymap roundtrip failed"
    );

    // 4) KeyPerf 1 キー (A) 同値書き戻し (op0/1/2/3/6/7)。
    let orig_perf = ks.perf[a_pos];
    println!("== apply KeyPerf A (同値) {orig_perf:?}");
    d.apply(&Change::KeyPerf {
        keys: vec![a_pos],
        perf: orig_perf,
    })
    .expect("apply keyperf");
    let s3 = d.read().expect("read3");
    let now = s3.keyboard.as_ref().unwrap().perf[a_pos];
    assert_eq!(now.actuation, orig_perf.actuation);
    assert_eq!(now.release, orig_perf.release);
    assert_eq!(now.mode, orig_perf.mode);
    assert_eq!(now.bottom_deadzone, orig_perf.bottom_deadzone);

    // 5) プロファイル同値再設定。
    let cur_profile = s.profile.active;
    println!("== apply Profile {cur_profile} (同値)");
    d.apply(&Change::Profile(cur_profile)).expect("apply profile");
    let s4 = d.read().expect("read4");
    assert_eq!(s4.profile.active, cur_profile);

    // 6) ライティング同値書き戻し。
    if let Some(l) = s.lighting.get(&0).cloned() {
        println!("== apply Lighting (同値)");
        d.apply(&Change::Lighting { area: 0, state: l }).expect("apply light");
        let s5 = d.read().expect("read5");
        assert_eq!(s5.lighting.get(&0), s.lighting.get(&0));
    }

    println!("OK");
}
