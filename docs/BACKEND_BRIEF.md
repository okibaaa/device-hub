# バックエンド指示書 v3 — Web HUB の完全移植 + 上位互換

> 対象: **フロントエンド以外**を担当するエージェント (devhub-hid / devhub-core の契約・registry)。
> フロントエンド (`crates/device-hub`) は担当者 (Claude) が書く。UI の都合で契約を足したいときは §6。
>
> 目標: **mouse.fit と hub.xsyd.top でできることを全部このアプリでできるようにし、さらに使いやすくする。**
> ユーザーはブラウザを開かなくなる前提なので、Web 版にしか無い機能が 1 つでも残ったら失敗。

---

## 0. 完了の定義

1. §2 の機能表の **P0〜P2 がすべて「済」**。P3 は実装するか、できない理由を表に書く。
2. 各項目を「アプリで変更 → Web 版で同じ値 → 抜き差し/電源再投入でも保持」で確認し、表に検証日を書く。
3. `cargo test -p devhub-hid` に、保存したフレーム (fixtures) によるエンコード/デコードのテストがある。
4. 1 台あたり `read()` が **300ms 以内** (キーボードの全キー性能込みで 800ms 以内)。
5. `cargo clippy --workspace --all-targets` が警告 0。docs (§8) を更新。

## 1. 現状 (2026-11-15 時点)

| 部分 | 状態 |
|---|---|
| `mousefit8.rs` (8K ドングル 33E4:3854 の report-8/EEPROM 経路) | DPI・ボタン・マクロ・主要設定の読み書き、ペアリング、バッテリー、ポーリング 125–8K (修正コード表で書き戻し済み)、磁気ボタン校正・押下深さ通知 (op10)、休眠検知 (タイムアウト+沈黙 → `DeviceInfo.asleep`、覚醒で自動再読)。**実機検証済み** |
| `mousefit.rs` (有線 64B feature report 経路) | 実装あり・**実機未検証** (HTS Plus Pro 有線 5219 は report-8 で接続・検証済み) |
| `xsyd.rs` (SU66 Max) | キーマップ・性能・ポーリング・ストローク・構成・高度キー・マクロ・ライティング (メイン+デコ+キーごと色)・設定・リセット・キャリブレーション — **ほぼ全機能実機検証済み**。残: 連動ライト・ファーム更新 (配布元チェックのみ実装) |
| フロントエンド | 全画面 + テスト画面・元に戻す/やり直し・プリセット・コマンドパレット・キャリブレーション・ウィザード・進捗表示 + サイドバー電池/スリープ表示・磁気ボタン深さ・フォント選択・ファームウェア更新通知・操作完了トースト (実機で確認済み) |

一番大きいのは**キーボード**: ホール効果キーボードの肝 (キーごとのトリガー/RT) がまだ無い。最優先。

## 2. 機能表 (Web 版の全機能 → このアプリでの表し方)

凡例: **済** / **一部** / **未**。優先度 P0 = 最優先 (ゲームで毎日使う)、P1 = Web 版にある日常機能、
P2 = Web 版にあるたまに使う機能、P3 = 機種限定・リスクが高いもの。
「表し方」は契約上の入れ物。`spec:xxx` は `SettingSpec` の id、`act:xxx` は `ActionSpec` の id。

### 2.1 キーボード — hub.xsyd.top (SU66 Max, `xsyd.rs`)

根拠: [公式 Web HUB](https://hub.xsyd.top/keyboard) の挙動と実機検証。[PROTOCOL_NOTES.md](PROTOCOL_NOTES.md) §2 を参照。

| 機能 | SDK のメソッド | 表し方 | 現状 | 優先 |
|---|---|---|---|---|
| キーごとのトリガー (作動点・リリース点・RT 押下/戻し感度・First touch・上下デッドゾーン・連続 RT) | `getPerformance(row,col)` / `setPerformance{...}` (値は mm×1000) | `KeyboardState::perf` / `Change::KeyPerf` / `PerfCaps` | **済** (66キー読み書き実機検証 11-15。連続 RT は wire に無く caps=false) | **P0** |
| RT 精度・軸 (スイッチ種別) 一覧 | `getRtPrecision` `getAxisList` `getCurrentAxis` `setDefaultAxis` | `PerfCaps::step` / `switch_types`、`KeyPerf::switch_type` | 一部 (step/軸一覧を open 時取得。`setDefaultAxis` は未公開) | **P0** |
| ストロークテスト (押下深さのリアルタイム表示) | `getADC` `getRoute` `getKeyStatus` | `DeviceDriver::live_travel` + `PerfCaps::live_travel=true` | **済** (route → mm、実機検証) | **P0** |
| 磁軸キャリブレーション | `CalibrationSetting(6)` `getCalibrate` | `act:calibrate` (開始) + `act:calibrate_finish` (保存)。UI がウィザードを出す (§4) | **済** (実機で UI ウィザード走行・`[4,3,2,row]` status=2 で全行校正済みを確認 11-15) | **P0** |
| ポーリングレート (8K〜125) | `ReportRateSetting(5)` | `spec:polling` (Choice, Hz) | **済** (list/現在/設定、実機検証) | **P0** |
| 設定の保存 (永続化) | `SaveParam(2)` | 各 `apply` の後に必要なら送る | **済** (scope 別に全 apply 後送信) | **P0** |
| 構成 1〜4 の切替・名前 | `ConfigSwitch(3)` `getConfigList` `getConfigName/setConfigName` | `ProfileCaps{count:4, renamable:true}` / `Change::Profile` `ProfileName` | **済** (4 構成・名读写・active 取得、実機検証) | P1 |
| 高度キー DKS / MPT / MT / TGL / END / SOCD / RS | `setHigherKey*` `getHigherKeyData` (SOCD/RS は 2 パケット、解除は NONE) | `KeyboardState::advanced` / `Change::Advanced` / `KeyboardCaps::advanced` | **済** (TGL write/read/clear 実機検証 11-15。全種同一経路) | P1 |
| マクロ (記録・モード・最大 32768ms) | `setMacroData` `getMacroData` `setMacroMode` `getMacroSpaceInfo` | `MacroCaps` / `KeyboardState::macros` / `Change::Macro` | **済** (write/read/delete 実機検証) | P1 |
| マクロ / 組み合わせキーの割り当て | キーコード空間 (PROTOCOL_NOTES §2.4) | `KeyAction::Macro` `KeyAction::Combo`、`KeyboardCaps::combos=true` | 一部 (Combo=`0x1000\|mods<<8\|usage` 実装済・caps=true。Macro 割当はキーコード空間未確定) | P1 |
| デコレーションライト (ON/OFF・上/下・エフェクト) | `getLightingBase(area)` `getDecorate1Custom` | `LightArea{id:1..}` | **済** (領域1 実機读写検証) | P1 |
| キーごとの色 (カスタム) | `getLightingCustom` `saveLightingCustom` `setLightingDirectDrive` | `LightingState::per_key` (エフェクトに `per_key`) | **済** (write/read 実機検証 11-15。セル対応: 6×21 グリッド、行5 は実測テーブル — `xsyd.rs::led_cells`・PROTOCOL_NOTES §2.5) | P1 |
| OS (Win/Mac)・Win キーロック | `SystemSetting(4)` `setSystemType` | `spec:system_type` `spec:win_lock` | 一部 (system_type 実機検証済。win_lock はキーコード側) | P1 |
| 範囲指定リセット (全部/キャリブ/性能/ライト/配列/高度キー/マクロ/軸) | `ResetFactory(1)` + 範囲 | `act:reset_perf` `act:reset_keymap` `act:reset_lighting` `act:reset_advanced` `act:reset_macro` `act:factory_reset` (`reload:true`) | **済** (全スコープ公開) | P1 |
| ライティング自動消灯・連動 | `getLightingSleepTime/set` `getDoubleLighting` | `spec:light_sleep` `spec:light_link` | 一部 (light_sleep 実機検証済。連動は未) | P2 |
| 防チャタリング最適化 | `ShakeOptimizationSwitch(16)` | `spec:shake_opt` (confirm 付き) | **済** (実機検証) | P2 |
| USB モード (2.0 / 3.0 8K/16K/32K) | `USBModeSetting(18)` `getUSBModeStatus` | `spec:usb_mode` (confirm 付き) | N/A (実機 `[2,18,*]`→0xFF 未対応。SU66 は有線専用) | P2 |
| 互換モード | `openWebDriver` 系 / blackout | `spec:compat_mode` | 未 | P2 |
| スリープ時間 (浅い/深い、無線機) | `setSleepTime{shallow,deep}` | `spec:sleep_shallow` `spec:sleep_deep` | 未 (有線機は宣言しない) | P2 |
| ファームウェア更新 | `getAppToBoot` `getFlash` `upgrade` / Sparklink getFirmwarePack | `act:firmware` (`.file(&["bin"])`、Progress を送る) | 一部 (配布元チェック+通知は実装済。書き込みは未) | P3 |
| 画面 GIF / 音声 / 触覚 / ゲームパッド (ハンドル) モード | displayer / voice / haptic / handle | 対応機だけ。契約の追加が要る → §6 | 未 | P3 |

### 2.2 マウス — www.mouse.fit (`mousefit8.rs` ドングル, `mousefit.rs` 有線)

根拠: [mouse.fit](https://www.mouse.fit/) の挙動と実機検証。EEPROM アドレスは [PROTOCOL_NOTES.md](PROTOCOL_NOTES.md) §1.3。

| 機能 | 根拠 (JS / EEPROM) | 表し方 | 現状 | 優先 |
|---|---|---|---|---|
| 有線経路 (5219 等) | 64B feature report | `mousefit.rs` | ドライバ経路 **未検証** (5219 有線そのものは report-8 で検証済み) | **P0** |
| HTX Ultra (5608/5617/5618/5619) | env-models.json | registry + 機種表 | 未検証 | **P0** |
| DPI ステージ (数・値・XY・色・現在) | `DPIValue` `Sensor3955DPI` `DPIColor` `maxDpiStage` `CurrentDPI` | `MouseState::stages` / `Change::DpiStages` `ActiveStage` | 済 (ドングル) | P0 |
| ポーリングレート (有線 / 無線。無線はドングル種別で候補が変わる) | `ReportRate` / `getDonglePR` | `spec:polling_wired` `spec:polling_wireless` | 済 (無線 125–8K 実機書き戻し検証 11-15、コード表修正済。他ドングルの候補絞りは未検証) | **P0** |
| LOD | `LOD` | `spec:lod` | 済 | P0 |
| MotionSync / 直線補正 / リップル / センサー角度 | `MotionSync` `Angle` `Ripple` `AngleTune` | `spec:motion_sync` `angle_snap` `ripple` `sensor_angle` | 済 | P0 |
| クリックデバウンス (押下/解放) / ホイールデバウンス | `DebounceTime` `DebounceReleaseTime` `WheelDebounceTime` | `spec:debounce_press` `debounce_release` `wheel_debounce` | 済 | P0 |
| バッテリー (%・充電中) / 本体・ドングル FW | op 4 / 18 / 29 | `battery()` / `DeviceInfo::firmware` `dongle_firmware` | 済 (fw 2.05 / 85% 実機表示確認) | P0 |
| 本体側変更・休眠の追従 | op10 通知 (深さ/DPI/ポーリング/電池) + `04 xx` + コマンドタイムアウト検知 | `HubEvent::StateChanged` `Battery` `Info` / `DeviceInfo::asleep` | 済 (休眠→サイドバー減光・覚醒→自動再読を実機検証) | P0 |
| 競技モード / 高速モード (+ 性能時間) | `PerformanceState` `Performance` / `setHyperMode` `setHyperModePerformanceTime` | `spec:competitive` `spec:hyper_mode` (+ `spec:hyper_time`) | 一部 (高速モード未) | P1 |
| デバウンス DIY 4 値 + 低/中/高プリセット | `ButtonDebounceAllDiy` `ButtonDebounceLevel` | `spec:debounce_preset` (Choice) + Slider 4 本 (`visible_when` でカスタム時だけ表示) | 未 | P1 |
| SPDT (左/右) | `getSPDTStateNew/set` | `spec:spdt_left` `spdt_right` | 一部 (ドングル経路で Unsupported) | P1 |
| ボタン誤操作防止 (LiftOff debounce) | `getLiftOffDebounceNew/set` | `spec:liftoff_debounce` | 未 | P1 |
| スリープ時間 | `SleepTime` | `spec:sleep_min` | 済 | P1 |
| DPI インジケーター / LED 状態 / 効果 | `setDPIIndicator` `setLedState` `DPIEffectMode/State` | `spec:dpi_indicator` `spec:dpi_led_effect` | 一部 (有線のみ) | P1 |
| ボタン割り当て (全種類) | `Ya` (Disable/MouseKey/DPISwitch/FireKey/ShortcutKey/Macro/ReportRateSwitch/DPILock/UpDownRoll) | `Change::Button`、`MouseCaps::button_kinds` | 一部 (上下ロール・プロファイル切替が Unsupported) | P1 |
| マクロ (最大 3、名前、回数/離すまで/再度押すまで) | `Macro` 768〜 | `MouseState::macros` / `Change::Macro` | 済 (ドングル) | P1 |
| 磁気ボタン: トリガー / クイックトリガー / 振動 / 左右校正 / 押下深さ表示 | `Left/RightTrigger` `FastTrigger` `TactileFeedback` / `StartCalibr(46)` / op10 通知 | `spec:ms_*` / `act:calibrate_buttons` / `live_buttons` | 済 (校正 UI 走行・押下深さリアルタイム表示とも実機検証) | P1 |
| ペアリング | op 5/6 | `act:pair` | 済 | P1 |
| 設定リセット / 工場出荷 | `ClearSetting(9)` `resetProfile` `defaultDevice` | `act:reset_profile` `act:factory_reset` (`reload:true`) | 未 | P1 |
| 組み合わせキー機能 | `getButtonCombine/set` | `spec:combine_key` | 未 | P2 |
| 20K FPS / センサー仮想中心 / センサー切替 / トラッキングモード | `SensorFPS20K` `SensorVirtualPos*` `setSensorIndex` `setTrackingMode` | `spec:sensor_fps_20k` `sensor_center` `sensor_index` `tracking_mode` | 未 | P2 |
| 長距離モード | op 22/23 | `spec:long_range` | 済 | P2 |
| 本体ライト (単色/ブリージング/スペクトル/波 + 明るさ/速度) | `SetLightEffect` `SetLightness` / `LightEffects` | `LightArea{id:0}` | 未 (機種表で LightEnablePID のある機種だけ) | P2 |
| レシーバーのライト・バッテリー表示 | 「レシーバー灯効」「バッテリー指示」 | `LightArea{id:1, name:"レシーバー"}` | 未 | P2 |
| オンボードプロファイル | `getProfileList` `setProfileID` (op 14/15) | `ProfileCaps` / `Change::Profile` | 未 (ドングル Unsupported) | P2 |
| レシーバーのボタン割り当て | `ReceiverKeyBindingSeq` | 契約の追加が要る → §6 | 未 | P3 |
| ファームウェア更新 (DFU) | `enterBL` `erase` `program` `verify` `exitBL` | `act:firmware` + Progress | 一部 (配布元チェック+通知は devhub-update で実装済。書き込みは未) | P3 |

### 2.3 ATK F1 Ultimate

mouse.fit の機種表に無い。実機の VID/PID/usage を取り、`docs/ADD_DEVICE.md §7` で系統を決める。
既存プロトコルで話せれば registry に足すだけ。違えば `DriverKind` を足して新ドライバ。P1。
**→ 実装済み (`atk.rs`, 有線+ドングル実機検証、§1 現状表を参照)。**

### 2.4 Pulsar Nordic (Xlite V3 ほか 21 機種)

公式アプリはデスクトップの Pulsar Fusion (Web 版が無い) なので、既存オープンソース解析
(python-pulsar-mouse-tool / pulsar-mouse-linux / hid-pulsar.c) + 実機プローブで解析。
**→ 実装済み (`pulsar.rs`, Xlite V3 有線で全機能実機検証、PROTOCOL_NOTES §6 参照)。**

| 機能 | 根拠 | 表し方 | 現状 | 優先 |
|---|---|---|---|---|
| 機種確定 (21 機種共用 PID) | cmd1 handshake → CID/MID | `DeviceInfo` の model/name + KM で caps | **済** (実機: 6/7 = Xlite V3) | **P0** |
| DPI 段 (4 段・50-26000・50 刻み) + 段色 | メモリ 0x0C/0x2C | `MouseState::stages` / `Change::DpiStages` | **済** (同値往復検証) | **P0** |
| ポーリング (125-4K) | メモリ 0x00 | `spec:polling_wireless` | **済** (4K 保持を実機確認) | **P0** |
| LOD (1/2mm) / MotionSync / 直線補正 / リップル | 0x0A/0xAB/0xAF/0xB1 | `spec:lod` ほか Toggle | **済** (反転往復検証) | **P0** |
| デバウンス (0-30ms) | 0xA9 | `spec:debounce` | **済** | P0 |
| バッテリー (%・充電中・mV) / FW | cmd4 / cmd 0x12 (パックド10進) | `battery()` / `DeviceInfo::firmware` | **済** (95%→100% 充電、fw 1.21) | P0 |
| スリープ (10-600秒・10秒刻み) | 0xB7 | `spec:sleep_sec` | **済** (60↔120 往復) | P1 |
| ボタン割当 (6/8 ボタン・KM 別) | 0x60+slot*4 | `Change::Button`、`MouseCaps::buttons` | **済** (Key 往復+復元検証) | P1 |
| ショートカット/メディア | 0x100+slot*32 レコード | `ButtonAction::Key` / `Media` | **済** (#30 キャプチャとバイト一致) | P1 |
| プロファイル 4 | cmd 0x0E/0x0F | `ProfileCaps` / `Change::Profile` | **済** (同値再設定) | P1 |
| LED (静的/ブリージング・輝度・速度) | 0x4C/0x4E/0x50/0x52 | `LightArea{id:0}` | **済** (効果切替往復) | P1 |
| 工場リセット | cmd 0x09 | `act:factory_reset` (`danger`) | **済** (送信経路のみ — 破壊的のため未実行) | P2 |
| 本体側変更の追従 | cmd 0x0A 通知 (DPI/プロファイル) | `HubEvent::StateChanged` | **済** (echo 待ちで拾って通知) | P2 |
| ドングル休眠検知 | コマンドタイムアウト | `DeviceInfo::asleep` | **済** (ドングル挿し+有線併用で確認) | P2 |
| マクロ (mode 6) | レコード形式が未解析 | — | 未 | P3 |
| FW 更新 (DFU) | 不明 | — | 対象外 (Fusion で) | P3 |

### 2.5 MonsGeek Ry5088 (FUN60 PRO ほか PID 502D 共有 ~30 機種)

公式 Web ドライバ (app.monsgeek.com) のバンドル解析 + 実機プローブで解析。
**→ 実装済み (`monsgeek.rs`, FUN60 PRO 有線で実機検証、PROTOCOL_NOTES §7 参照)。**

| 機能 | 根拠 | 表し方 | 現状 | 優先 |
|---|---|---|---|---|
| 機種確定 (PID 502D 共有) | cmd 0x8F whoAmI → デバイス ID (u32 LE) | `DeviceInfo` の model/name | **済** (実機: 2304 = FUN60 PRO, fw 777) | **P0** |
| キーマップ (61 キー × 4 オンボード層) | GET 0x8A (4B/キー) / SET 0x0A | `KeyboardState::keymap` / `Change::KeyMap` | **済** (同値往復。コンボ・メディア (32bit) 往復保存) | **P0** |
| キーごとの磁軸 (押下/リリース/RT押下/RTリリース/死区/モード) | GET/SET 0xE5/0x65 op0-7 | `KeyboardState::perf` / `Change::KeyPerf` / `PerfCaps` | **済** (A キー 6 op 往復検証) | **P0** |
| ポーリングレート (125-8K) | GET/SET 0x83/0x03 | `spec:polling` | **済** (実機 4K) | **P0** |
| プロファイル 4 | GET/SET 0x84/0x04 | `ProfileCaps` / `Change::Profile` | **済** (同値再設定) | P1 |
| RT Stab (0-125%・25 刻み) / 誤入力防止 / WASD↔矢印 | KBOPTION 0x89/0x09 | `spec:rt_stab` / `spec:mistouch` / `spec:wasd_swap` | **済** (同値往復・全項目保持) | P1 |
| デバウンス | GET/SET 0x86/0x06 | `spec:debounce` | **済** (同値往復) | P1 |
| バックライト (灯効 11 種・明るさ/速度/RGB) | GET/SET 0x87/0x07 (**Bit8**) | `LightArea{id:0}` | **済** (同値往復) | P1 |
| 磁軸キャリブレーション | 0x1C (最小) → 2s → 0x1E (最大) | `act:calibrate` / `calibrate_finish` | **済** (経路のみ — 実機で未実行) | P1 |
| 工場リセット | cmd 0x01 | `act:factory_reset` (`danger`) | **済** (経路のみ — 破壊的のため未実行) | P2 |
| live_travel (行程ストリーム) | interface 1 (FFFF/01) の 0x1B 通知 | `PerfCaps::live_travel` | 未 (Phase 2 — 現在 false 申告) | P1 |
| Fn 層 (win/mac × 2) | GET/SET 0x90/0x10 | 契約の追加が必要 | 未 | P2 |
| マクロ | GET/SET 0x8B/0x0B | `MacroCaps` | 未 (レコード形式は解析済み) | P2 |
| 高度キー (DKS/MT/TGL/Snap) | op7 モード 2-7 + op4/5/9 | `Advanced` | 未 (読み出しのみ) | P2 |
| カスタム灯効レイヤ / GIF | 0x0C/0x8C/0x12 | `LightingState::per_key` | 未 | P2 |
| 軸体タイプ (op252) / 頂部死区 (op251) | FW≥1024 系のみ | — | N/A (実機 fw 777 は未対応) | P3 |
| FW 更新 | 0x7F / 0xBA (ヘッダ 20480B スキップ) | `act:firmware` | 未 (対象外) | P3 |


## 3. 作業の順番

1. **P0 キーボード**: `xsyd.rs` に performance (読み/書き/保存)・ポーリング・ストロークテスト・キャリブレーション。
   SDK の `F([...])` が作るバイト列を、1 文字の定数 (`Or` `kr` `Ar` …) を esm 先頭で解決して表にし、
   **フレームを fixtures に保存してからコードを書く**。
2. **P0 マウス**: 有線経路と HTX Ultra を実機で検証。ポーリング候補をドングル種別で正しく絞る。
3. **P1**: 構成 1〜4、高度キー、マクロ、組み合わせ/マクロ割り当て、デコライト、OS/Win ロック、
   範囲リセット、マウスの未実装設定、ボタン種類の残り、磁気ボタン校正、ATK F1。
4. **P2 / P3**: 残り。ファーム更新は**最後**で、実装するなら Web 版の手順を 1 バイトも変えずに。

## 4. 契約の約束 (フロントはこう動くので、こう返す)

- **UI は caps しか見ない**。機種に無い機能は spec/action/caps を**返さない**だけで UI から消える。
  「宣言したのに読めない」は最悪 (行が無効表示になる)。分からないものは宣言しない。
- 汎用設定の id と単位は demo.rs と §2 の表の名前に揃える (書き出しファイルの互換のため)。
  新しい id を作ったら §2 に追記。
- `apply` は細粒度。UI は同じ対象の連続変更を間引いてから送る。**書き込みに失敗したら必ず `Err`**
  (UI がトーストを出して読み直す)。成功を装わない。
- **元に戻す / プリセット適用**: UI は「目標の状態」との差分を `Change` の列にして順に送る。
  `KeyPerf` は同じ値のキーをまとめて 1 件で送るので、複数キーの一括書き込みを速く実装すること。
- **キャリブレーション**: `act:calibrate` (開始) → UI が `live_travel()` を 30Hz で呼び、各キーの最大深さを表示 →
  `act:calibrate_finish` (あれば) で保存。`live_travel` は **layout 順・mm** で返す。
- `*_finish` で終わる action は一覧に出ない (ウィザード専用)。
- **進捗**: 長い操作 (ファーム更新・大量書き込み・キャリブレーション) は `HubEvent::Progress { id, label,
  fraction, done }` を `watch` の callback で送る。UI のヘッダに進捗バーが出る。最後に `done: true`。
- **本体側での変更の通知**: 本体のボタンで DPI / プロファイルを変えた、充電を始めた、などが分かるなら
  `HubEvent::StateChanged(id)` / `HubEvent::Battery(..)` を送る。UI は読み直す (Web 版にはできない = 上位互換ポイント)。
- 無線で本体が寝ていてドングルだけ応答する → `DeviceInfo::asleep = true`。起きたら `scan` で `asleep=false` を返せば
  UI が開き直す。
- エラー文は日本語の `HubError` で (UI がそのままトーストに出す)。

## 5. 上位互換にするためにバックエンドでやること

フロントだけではできない部分:

1. **速さ**: read をパイプライン化 (要求をまとめて送ってから応答を集める)。キーボードの全キー性能も 1 秒未満。
2. **確実さ**: 書き込み後に読み戻して一致を確認 (`apply` の中で。ずれたら `Err(Protocol)`)。保存コマンドを忘れない。
3. **自動復帰**: 抜き差し・スリープ明け・ドングル付け替えで `scan` が新しい ID を作らない (シリアル / パスを安定させる)。
   `watch` でホットプラグを通知 (`hidapi` を 1 秒ごとに列挙して差分を見るだけでもよい)。
4. **本体側の変更を拾う**: input report を常時読むスレッドで DPI 変更・バッテリー変化を検知して `HubEvent` を送る。
5. **Web 版と同時起動の検知**: 他のプロセスがデバイスを占有していて開けないときは
   `HubError::Io("ブラウザの設定画面など、他のアプリがデバイスを使用中です")` のように原因が分かる文にする。
6. **安全**: ファーム更新・工場出荷は、バッテリー残量 (無線) と接続方式 (有線必須など) を事前に確認して拒否できるように。

## 6. 契約 (devhub-core) を広げるとき

- スカラー設定 → `SettingSpec` を足すだけ (契約変更なし、UI に自動で出る)。
  - 他の設定に従属するもの (例: LAMZU の 20K FPS は競技モード ON のときだけ) は
    `.enabled_when("hyper_mode", SettingValue::Bool(true), "先に競技モードを有効にしてください")`。
    UI は条件を満たさない間、行を薄くして理由を出し、操作させない。ドライバ側でも必ず拒否 (`Err(Invalid)`) すること。
  - 速度連動 DPI のカーブは `MouseCaps::adaptive_dpi: Option<AdaptiveCaps>` + `MouseState::adaptive` + `Change::Adaptive`
    (点 = 速度 u16 と倍率 f32)。宣言すれば性能タブにカーブ編集カードが出る。
- ボタン 1 つの操作 → `ActionSpec` を足すだけ。
- 構造を持つ新機能 (画面 GIF、音声、ハンドルモード、レシーバーのボタン割り当て等) → `model.rs` に型と `Change`
  の variant を**追加** (既存は変えない)。`demo.rs` にも実装し、**`docs/FRONTEND_TODO.md` に UI の要望を書く**
  (フロント担当が画面を作る)。書式: 何の機能か / 新しい型 / どの caps で出すか / Web 版の画面のどこにあるか。

## 7. 開発と検証の手順

```bash
cargo run -p device-hub                    # 実機 (HID が開けなければデモ)
DEVHUB_DEMO=1 cargo run -p device-hub      # デモ
cargo run -p devhub-hid --example enum     # HID 列挙
cargo run -p devhub-hid --example readback # 読み戻し確認 (既存)
cargo test -p devhub-hid
```

- 検証は 1 項目ずつ「読む → 変える → 読み戻す → Web 版で確認 → 元に戻す」。
- フレームは `crates/devhub-hid/fixtures/<model>/<名前>.req.bin` / `.resp.bin` + 説明 `.md`。
- **Web 版のタブを開いたままアプリで操作しない** (HID を取り合う)。Web 版で確認したらタブを閉じる。
- リセット系とファーム更新の検証は最後に、ユーザーに確認してから。
- `crates/device-hub` (フロントエンド) は編集しない。

## 8. 終わったら更新するもの

- この文書の §2 の「現状」列と検証日
- `docs/ADD_DEVICE.md` の対応表、`registry.rs` の `verified`
- `docs/PROTOCOL_NOTES.md` (新しく分かったバイト列・単位・注意点)
- `AGENTS.md` の「現状」
- フロントへの要望があれば `docs/FRONTEND_TODO.md`
