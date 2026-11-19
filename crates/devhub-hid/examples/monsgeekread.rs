//! MonsGeek live_travel test - enable the 0x1B stream and verify per-key travel.
//! `cargo run -p devhub-hid --example monsgeekread`

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

    // ---- live_travel test ----
    // Check if stream was already enabled externally (via PS).
    let ext = std::env::args().any(|a| a == "--vendor-only");
    if ext {
        println!("== vendor-only mode: skipping 0x1B enable (already on via PS)");
        // Force live_on = true so live_travel doesn't try to enable again.
        d.force_live_on_for_test();
    }
    println!("== live_travel: reading...");
    let t0 = std::time::Instant::now();
    let v1 = d.live_travel().expect("live_travel 1");
    println!(
        "   first call took {:.1}s, {} keys",
        t0.elapsed().as_secs_f32(),
        v1.len()
    );
    println!("   waiting 2s for stream data...");
    std::thread::sleep(std::time::Duration::from_secs(2));
    let v2 = d.live_travel().expect("live_travel 2");
    let non_zero = v2.iter().filter(|v| **v > 0.01).count();
    println!("   after 2s: {} non-zero keys", non_zero);
    {
        let (raw, _max) = monsgeek::MonsDriver::debug_live_buffers(&d);
        let raw_nz: Vec<(usize, u16)> = raw
            .iter()
            .enumerate()
            .filter(|(_, &v)| v != 0)
            .map(|(i, &v)| (i, v))
            .collect();
        println!(
            "   raw buf: {} entries: {:?}",
            raw_nz.len(),
            &raw_nz[..raw_nz.len().min(8)]
        );
    }
    // Print keys.
    let c = d.caps();
    let layout = c.keyboard.as_ref().unwrap().layout.clone();
    for (ki, key) in layout.iter().enumerate().take(8) {
        println!("   {:?}: {:.2}mm", key.label, v2[ki]);
    }
    // Read more and print pressed keys.
    println!("   (3s間キーを押してみてください)");
    std::thread::sleep(std::time::Duration::from_secs(3));
    let v3 = d.live_travel().expect("live_travel 3");
    for (ki, key) in layout.iter().enumerate() {
        if v3[ki] > 0.05 {
            println!("   PRESSED: {:?} = {:.2}mm", key.label, v3[ki]);
        }
    }
    // Cleanup: disable the stream.
    let _ = d.live_stream_set_public(false);
    println!("OK");
}
