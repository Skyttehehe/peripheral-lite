//! `--demo`: shows sample devices (the design's two, plus a Corsair headset) without touching any hardware, for
//! screenshots and for comparing the widget with the design. Extra flags open parts of it:
//! `--demo-select N`, `--demo-drawer`, `--demo-skins`, `--demo-skin ID`, `--demo-all` (every
//! optional part shown, as for users updating from 0.4.4).

use peripheral_lite::corsair::Settings;
use peripheral_lite::device::{Dpi, DpiCaps, MODE_HOST};
use peripheral_lite::watch::Snapshot;

use crate::history::{self, History};
use crate::skin::Show;
use crate::{App, Row};

pub struct Options {
    select: usize,
    drawer: bool,
    skins: bool,
    skin: Option<String>,
    all: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> Option<Options> {
        if !args.iter().any(|a| a == "--demo") {
            return None;
        }
        let value = |flag: &str| {
            args.iter()
                .position(|a| a == flag)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        Some(Options {
            select: value("--demo-select")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(1)
                .max(1)
                - 1,
            drawer: args.iter().any(|a| a == "--demo-drawer"),
            skins: args.iter().any(|a| a == "--demo-skins"),
            skin: value("--demo-skin"),
            all: args.iter().any(|a| a == "--demo-all"),
        })
    }

    pub fn apply(self, app: &mut App) {
        let now = history::now();
        let hourly = |v: &[u8]| -> History {
            History::memory(
                v.iter()
                    .enumerate()
                    .map(|(i, p)| (now - (v.len() - 1 - i) as u64 * 3600 - 60, *p))
                    .collect(),
            )
        };
        let mouse = [
            88, 86, 85, 84, 82, 81, 80, 78, 77, 75, 74, 72, 71, 69, 67, 66, 64, 62, 61, 59, 58, 56, 55, 53,
        ];
        let headset = [
            100, 98, 97, 95, 93, 92, 90, 88, 87, 86, 84, 83, 82, 80, 78, 76, 74, 72, 70, 69, 67, 65, 64, 62,
        ];
        app.rows = vec![
            Row {
                sel: "c54d:1".into(),
                name: "PRO X 2".into(),
                battery: Some("53% level=good discharging external=none".into()),
                percent: Some(53),
                online: true,
                protocol: Some((4, 2)),
                history: hourly(&mouse),
                alerted: false,
                demo: Some(["≈ 41 h left", "−1.3% / h", "Discharging"]),
            },
            Row {
                sel: "0aba".into(),
                name: "PRO X Wireless Headset".into(),
                battery: Some("≈62%  (3858 mV, flags=0x01, generic Li-ion estimate)".into()),
                percent: Some(62),
                online: true,
                protocol: Some((4, 2)),
                history: hourly(&headset),
                alerted: false,
                demo: Some(["≈ 9 h left", "−4.1% / h", "Discharging"]),
            },
            Row {
                sel: "1b1c:0a73".into(),
                name: "CORSAIR HS80 RGB Wireless Gaming Receiver".into(),
                battery: Some("31% discharging (3657 mV)".into()),
                percent: Some(31),
                online: true,
                protocol: None,
                history: hourly(&headset[8..]),
                alerted: false,
                demo: Some(["≈ 6 h left", "−4.8% / h", "Discharging"]),
            },
        ];
        let base = |sel: &str, name: &str| Snapshot {
            sel: sel.into(),
            name: name.into(),
            battery: None,
            battery_percent: None,
            voltage_mv: None,
            needs_curve: false,
            curve_label: None,
            recording: None,
            tenths_left: None,
            dpi: None,
            rate: None,
            mode: None,
            model: None,
            preset: None,
            headset: None,
        };
        app.demo = Some(vec![
            Snapshot {
                battery_percent: Some(53),
                dpi: Some(Dpi {
                    feature: 0x2202,
                    index: 0,
                    version: 0,
                    current: 800,
                    default: 800,
                    y: None,
                    lod: 0,
                    caps: DpiCaps::Ranges {
                        points: vec![100, 32000],
                        steps: vec![50],
                    },
                }),
                rate: Some((8000, vec![125, 500, 1000, 4000, 8000])),
                mode: Some(MODE_HOST),
                model: Some(0x40a9),
                ..base("c54d:1", "PRO X 2")
            },
            Snapshot {
                battery_percent: Some(62),
                voltage_mv: Some(3858),
                needs_curve: true,
                curve_label: Some("generic Li-ion estimate".into()),
                recording: Some("demo".into()),
                model: Some(0x0aba),
                ..base("0aba", "PRO X Wireless Headset")
            },
            Snapshot {
                battery: Some("31% discharging (3657 mV)".into()),
                battery_percent: Some(31),
                headset: Some(Settings {
                    brightness: Some(330),
                    sleep_on: Some(true),
                    sleep_ms: Some(900_000),
                    mic_muted: Some(false),
                }),
                ..base("1b1c:0a73", "CORSAIR HS80 RGB Wireless Gaming Receiver")
            },
        ]);
        let sel = app.rows[self.select.min(app.rows.len() - 1)].sel.clone();
        app.select(sel);
        app.dpi_input = "800".into();
        app.drawer = self.drawer;
        app.show_skins = self.skins;
        app.settings.show = Some(if self.all { Show::ALL } else { Show::default() });
        if let Some(id) = self.skin {
            app.settings.skin = id;
        }
    }
}
