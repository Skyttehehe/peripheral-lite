//! The window: a skinned battery widget and, next to it on demand, the skin manager.
//! The widget follows the "Battery Tracker v3" design one to one: every part is painted at the
//! design's measured position and size (see the coordinates below), with its fonts, radii,
//! inset shadows and glows.

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Painter, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder,
    pos2, vec2,
};
use eframe::epaint::Shadow;
use peripheral_lite::device::{MODE_HOST, MODE_ONBOARD};
use peripheral_lite::watch::Cmd;

use crate::fonts::{GROTESK, MONO, MONO_SEMIBOLD, PIXEL, SANS, SANS_MEDIUM, SANS_SEMIBOLD, font};
use crate::history::{self, BARS};
use crate::skin::{self, BAD, Digits, FACE, FIELDS, FRAME, HI, INK, LCD, LCD_DIM, LCD_INK, LO, OK, Skin};
use crate::{App, UpdateState, os, update};

pub const WIDGET_W: f32 = 420.0;
const MANAGER_W: f32 = 420.0;
const BOARD_GAP: f32 = 12.0;
/// The settings drawer: the device's settings, then a row of app settings.
const DRAWER_H: f32 = 156.0;
const DEVICE_SETTINGS_H: f32 = 118.0;

// Vertical layout, y from the widget's top: display, history strip, drawer (when open), devices.
const DISPLAY_Y: f32 = 41.0;
const DISPLAY_H: f32 = 100.0;
const HISTORY_Y: f32 = DISPLAY_Y + DISPLAY_H + 8.0;
const DRAWER_Y: f32 = HISTORY_Y + 44.0;
const DEVICES_Y: f32 = DRAWER_Y + 8.0;
/// How far the open drawer pushes the device list down.
const DRAWER_SHIFT: f32 = DRAWER_H + 8.0;

// Skin manager colours (fixed, not skinned).
const M_BG: Color32 = Color32::from_rgb(0x0d, 0x0e, 0x10);
const M_CARD: Color32 = Color32::from_rgb(0x16, 0x17, 0x1a);
const M_CARD_HOVER: Color32 = Color32::from_rgb(0x1d, 0x1f, 0x23);
const M_EDGE: Color32 = Color32::from_rgb(0x26, 0x28, 0x2d);
const M_EDGE2: Color32 = Color32::from_rgb(0x33, 0x36, 0x3b);
const M_TEXT: Color32 = Color32::from_rgb(0xe7, 0xe8, 0xea);
const M_SOFT: Color32 = Color32::from_rgb(0xc9, 0xcc, 0xd1);
const M_SUB: Color32 = Color32::from_rgb(0x8a, 0x8d, 0x94);
const M_DIM: Color32 = Color32::from_rgb(0x7d, 0x80, 0x87);
const M_FAINT: Color32 = Color32::from_rgb(0x6b, 0x6e, 0x75);
const M_ACCENT: Color32 = Color32::from_rgb(0x7f, 0xd3, 0xde);
const M_TEAL: Color32 = Color32::from_rgb(0x2f, 0x8e, 0x9b);
const M_TEAL_HOVER: Color32 = Color32::from_rgb(0x38, 0xa2, 0xb0);
const M_ERR: Color32 = Color32::from_rgb(0xff, 0x8a, 0x7a);

// ---------------------------------------------------------------------- painting helpers

fn alpha(c: Color32, a: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a)
}

/// CSS `filter: brightness(f)` on an opaque colour.
fn bright(c: Color32, f: f32) -> Color32 {
    let m = |x: u8| (x as f32 * f).min(255.0) as u8;
    Color32::from_rgb(m(c.r()), m(c.g()), m(c.b()))
}

fn text_w(ui: &Ui, s: &str, family: &str, size: f32) -> f32 {
    ui.painter()
        .layout_no_wrap(s.to_string(), font(family, size), Color32::WHITE)
        .size()
        .x
}

/// Shorten `s` with "…" to fit `max_w`.
fn elide(ui: &Ui, s: &str, family: &str, size: f32, max_w: f32) -> String {
    if text_w(ui, s, family, size) <= max_w {
        return s.to_string();
    }
    let mut chars: Vec<char> = s.chars().collect();
    while chars.pop().is_some() {
        let t: String = chars.iter().collect::<String>() + "…";
        if text_w(ui, &t, family, size) <= max_w {
            return t;
        }
    }
    "…".into()
}

/// Vertical text correction against the browser's placement (measured: none needed).
const TEXT_DY: f32 = 0.0;

fn text(p: &Painter, pos: Pos2, align: Align2, s: &str, family: &str, size: f32, color: Color32) -> Rect {
    p.text(pos + vec2(0.0, TEXT_DY), align, s, font(family, size), color)
}

/// Text with CSS `letter-spacing` (added after every character, as the browser does).
#[allow(clippy::too_many_arguments)]
fn spaced(p: &Painter, pos: Pos2, align: Align2, s: &str, family: &str, size: f32, color: Color32, ls: f32) -> Rect {
    let mut job = egui::text::LayoutJob::default();
    job.append(
        s,
        0.0,
        egui::TextFormat {
            font_id: font(family, size),
            color,
            extra_letter_spacing: ls,
            ..Default::default()
        },
    );
    let galley = p.layout_job(job);
    let rect = align.anchor_size(pos + vec2(0.0, TEXT_DY), galley.size());
    p.galley(rect.min, galley, color);
    rect
}

/// CSS `box-shadow: 0 0 <blur>px <color>` around a rounded rect. egui's blur reaches about
/// 60 % as far as the browser's for the same number, hence the scaling.
fn glow(p: &Painter, rect: Rect, radius: f32, blur: u8, color: Color32) {
    p.add(
        Shadow {
            offset: [0, 0],
            blur: (blur as f32 * 1.7) as u8,
            spread: 0,
            color,
        }
        .as_shape(rect, radius),
    );
}

/// CSS `box-shadow: inset 0 2px 6px rgba(0,0,0,.45)`: darker at the top edge, lighter at the
/// sides, faint at the bottom.
fn inset_shadow(p: &Painter, rect: Rect, radius: f32) {
    // one 1 px ring per step inwards, fading
    for (i, a) in [58u8, 44, 32, 22, 13, 6].into_iter().enumerate() {
        let s = 0.5 + i as f32;
        let r = rect.shrink(s);
        p.rect_stroke(
            r,
            (radius - s).max(0.0),
            Stroke::new(1.0, Color32::from_black_alpha(a)),
            StrokeKind::Middle,
        );
    }
    // the 2 px downward offset: extra darkness along the top edge
    for (i, a) in [40u8, 38, 30, 20, 10, 4].into_iter().enumerate() {
        let y = rect.top() + 0.5 + i as f32;
        let inset = radius * 0.45 + (radius - i as f32).max(0.0) * 0.4;
        p.line_segment(
            [pos2(rect.left() + inset, y), pos2(rect.right() - inset, y)],
            Stroke::new(1.0, Color32::from_black_alpha(a)),
        );
    }
}

/// The recessed display panel: LCD fill, optional inner shadow, 1 px edge ring.
fn lcd_box(p: &Painter, rect: Rect, radius: f32, k: &Skin, shadow: bool) {
    p.rect_filled(rect, radius, k.c(LCD));
    if shadow {
        inset_shadow(p, rect, radius);
    }
    p.rect_stroke(rect, radius, Stroke::new(1.0, k.c(LO)), StrokeKind::Inside);
}

/// A raised button: `box-shadow: inset 0 1px 0 hi, 0 0 0 1px lo` (the ring is optional);
/// `f` is the hover brightness.
fn raised(p: &Painter, rect: Rect, radius: f32, fill: Color32, k: &Skin, ring: bool, f: f32) {
    if ring {
        p.rect_filled(rect.expand(1.0), radius + 1.0, bright(k.c(LO), f));
    }
    p.rect_filled(rect, radius, bright(k.c(HI), f));
    p.rect_filled(
        Rect::from_min_max(rect.min + vec2(0.0, 1.0), rect.max),
        radius,
        bright(fill, f),
    );
}

fn click(ui: &mut Ui, rect: Rect, salt: impl std::hash::Hash + std::fmt::Debug, tip: &str) -> Response {
    let r = ui
        .interact(rect, ui.scope_id().with(salt), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand);
    if tip.is_empty() { r } else { r.on_hover_text(tip) }
}

/// An outlined chip (rate options, drawer actions), optionally filled.
#[allow(clippy::too_many_arguments)]
fn chip_at(
    ui: &mut Ui,
    rect: Rect,
    label: &str,
    family: &str,
    size: f32,
    k: &Skin,
    filled: bool,
    salt: &str,
) -> Response {
    let resp = click(ui, rect, (salt, label), "");
    let p = ui.painter();
    let fill = if filled {
        k.c(LCD_DIM)
    } else if resp.hovered() {
        k.c(LO)
    } else {
        Color32::TRANSPARENT
    };
    p.rect_filled(rect, 5.0, fill);
    p.rect_stroke(rect, 5.0, Stroke::new(1.0, k.c(LCD_DIM)), StrokeKind::Inside);
    text(
        p,
        rect.center(),
        Align2::CENTER_CENTER,
        label,
        family,
        size,
        k.c(LCD_INK),
    );
    resp
}

/// Text with the design's `text-shadow: 0 0 14px <color>55` glow (a real blur, see glow.rs).
#[allow(clippy::too_many_arguments)]
fn glow_text(p: &Painter, pos: Pos2, align: Align2, s: &str, family: &str, size: f32, color: Color32, bold: bool) {
    // the design's letter-spacing: -.02em
    let mut job = egui::text::LayoutJob::default();
    job.append(
        s,
        0.0,
        egui::TextFormat {
            font_id: font(family, size),
            color,
            extra_letter_spacing: -0.017 * size,
            ..Default::default()
        },
    );
    let galley = p.layout_job(job);
    let rect = align.anchor_size(pos, galley.size());
    crate::glow::paint(p, rect.min, &galley, 14.0, alpha(color, 0x55), bold);
    p.galley(rect.min, galley.clone(), color);
    if bold {
        // VT323 has one weight; the browser fakes the design's 600 by smearing it sideways
        p.galley(rect.min + vec2(1.0, 0.0), galley, color);
    }
}

// ---------------------------------------------------------------------- the data shown

/// What the display shows for the selected device.
struct Cur {
    name: String,
    pct: Option<u8>,
    alert: u8,
    color: Color32,
    time_left: String,
    status: String,
    bars: [Option<u8>; BARS],
}

impl App {
    fn current(&self, k: &Skin) -> Cur {
        let row = self
            .selected
            .as_ref()
            .and_then(|s| self.rows.iter().find(|r| &r.sel == s));
        let Some(r) = row else {
            return Cur {
                name: "peripheral-lite".into(),
                pct: None,
                alert: 20,
                color: k.c(LCD_DIM),
                time_left: String::new(),
                status: if self.fatal.is_some() {
                    "Device access failed"
                } else {
                    "Looking for devices…"
                }
                .into(),
                bars: [None; BARS],
            };
        };
        let now = history::now();
        let snap = self.snap.as_ref().filter(|s| s.sel == r.sel);
        let alert = self.alert_level(&r.sel);
        let pct = r.percent.filter(|_| r.online);
        let charging = r.charging();
        let drain = r.history.drain_tenths(now).filter(|_| !charging);
        let voltage = snap.and_then(|s| s.voltage_mv);
        let status = if !r.online {
            "Offline".to_string()
        } else if let Some(b) = r.battery.as_deref() {
            if let Some(state) = crate::charge_state(b) {
                state.into()
            } else if voltage.is_some() || b.contains(" mV") {
                "Discharging".into() // a voltage-only battery: charging is not detected yet
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        let time_left = if !r.online {
            String::new()
        } else if charging {
            "charging".into()
        } else if let Some(t) = snap.and_then(|s| s.tenths_left) {
            history::hours_text(t)
        } else if let (Some(p), Some(d)) = (pct, drain) {
            history::hours_text(p as u32 * 100 / d.max(1))
        } else {
            "— h left".into()
        };
        let (time_left, status) = match r.demo {
            Some([t, s]) => (t.to_string(), s.to_string()),
            None => (time_left, status),
        };
        Cur {
            name: r.name.clone(),
            pct,
            alert,
            color: pct.map_or(k.c(LCD_DIM), |p| k.level(p, alert)),
            time_left,
            status,
            bars: r.history.bars(now),
        }
    }

    pub fn draw(&mut self, ui: &mut Ui) {
        self.take_dropped_files(ui.ctx());
        let k = self.skin().clone();
        ui.style_mut().interaction.selectable_labels = false;
        let t = ui
            .ctx()
            .animate_bool_with_time(ui.scope_id().with("drawer"), self.drawer, 0.28);
        let rows = self.rows.len().max(1) as f32;
        let list_h = 6.0 + 32.0 * rows;
        let h = DEVICES_Y + 24.0 + DRAWER_SHIFT * t + list_h + 11.0;
        let mut size = vec2(WIDGET_W, h);
        egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
            let o = ui.max_rect().min;
            self.widget(ui, o, &k, t, h, list_h);
            if self.show_skins {
                // the manager sits on the design's page background, in its own rounded panel
                let panel = Rect::from_min_size(
                    o + vec2(WIDGET_W + BOARD_GAP, 0.0),
                    vec2(MANAGER_W + 40.0, self.manager_h),
                );
                ui.painter().rect_filled(panel, 14.0, M_BG);
                let mh = self.manager(ui, panel.min + vec2(20.0, 20.0), &k) + 40.0;
                self.manager_h = mh;
                size = vec2(WIDGET_W + BOARD_GAP + MANAGER_W + 40.0, h.max(mh));
            }
        });
        // The window always fits the content.
        let want = vec2(size.x.ceil(), size.y.ceil());
        let have = ui.ctx().input(|i| i.viewport().inner_rect.map(|r| r.size()));
        if have.is_none_or(|h| (h - want).length() > 0.5) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::InnerSize(want));
        }
    }

    // ------------------------------------------------------------------ the widget

    #[allow(clippy::too_many_arguments)]
    fn widget(&mut self, ui: &mut Ui, o: Pos2, k: &Skin, t: f32, h: f32, list_h: f32) {
        let cur = self.current(k);
        let at = |x: f32, y: f32| o + vec2(x, y);
        let r = |x: f32, y: f32, w: f32, hh: f32| Rect::from_min_size(o + vec2(x, y), vec2(w, hh));
        let p = ui.painter().clone();

        // frame: 1 px edge, 1 px top highlight, body
        let outer = r(0.0, 0.0, WIDGET_W, h);
        p.rect_filled(outer, 14.0, k.c(LO));
        let inner = outer.shrink(1.0);
        p.rect_filled(inner, 13.0, k.c(HI));
        p.rect_filled(
            Rect::from_min_max(inner.min + vec2(0.0, 1.0), inner.max),
            13.0,
            k.c(FRAME),
        );

        // title bar: drag to move; LED, name (click: skins), window buttons
        let drag = ui.interact(
            r(11.0, 11.0, 398.0, 22.0),
            ui.scope_id().with("title"),
            Sense::click_and_drag(),
        );
        if drag.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        let led = Rect::from_center_size(at(19.0, 22.0), vec2(8.0, 8.0));
        glow(&p, led, 4.0, 6, alpha(cur.color, 190));
        p.circle_filled(led.center(), 4.0, cur.color);
        let name = spaced(
            &p,
            at(31.0, 21.5),
            Align2::LEFT_CENTER,
            "peripheral-lite",
            SANS_SEMIBOLD,
            12.0,
            k.c(INK),
            0.24,
        );
        if click(ui, name.expand(3.0), "title-name", "Skins").clicked() {
            self.show_skins = !self.show_skins;
        }
        // settings: opens the drawer
        let c = at(360.0, 22.0);
        let set = Rect::from_center_size(c, vec2(18.0, 18.0));
        let resp = click(ui, set, "settings", "Settings");
        let lit = self.drawer || resp.hovered();
        if lit {
            p.rect_filled(set, 5.0, k.c(LO));
        }
        let ink = if lit { k.c(INK) } else { alpha(k.c(INK), 179) };
        let ic = c - vec2(6.0, 6.0); // 12×12 icon box
        for (y, cx) in [(3.0, 4.0), (9.0, 8.0)] {
            p.line_segment([ic + vec2(1.5, y), ic + vec2(10.5, y)], Stroke::new(1.4, ink));
            p.circle_filled(ic + vec2(cx, y), 2.2, ink);
        }
        if resp.clicked() {
            self.drawer = !self.drawer;
        }
        let close = if self.hides_to_tray() { "Hide to tray" } else { "Quit" };
        for (i, (x, hover, tip)) in [(381.0, HI, "Minimise"), (399.0, BAD, close)].into_iter().enumerate() {
            let c = at(x, 22.0);
            let resp = click(ui, Rect::from_center_size(c, vec2(14.0, 14.0)), ("winbtn", i), tip);
            p.circle_filled(c, 6.0, k.c(LO));
            p.circle_filled(c, 5.0, if resp.hovered() { k.c(hover) } else { k.c(FACE) });
            if resp.clicked() {
                let cmd = if i == 0 {
                    egui::ViewportCommand::Minimized(true)
                } else {
                    egui::ViewportCommand::Close
                };
                ui.ctx().send_viewport_cmd(cmd);
            }
        }

        self.display(ui, &p, o, k, &cur);
        history_strip(ui, &p, o, k, &cur);
        self.drawer(ui, o, k, t);

        // devices
        let top = DEVICES_Y + DRAWER_SHIFT * t;
        spaced(
            &p,
            at(15.0, top + 9.0),
            Align2::LEFT_CENTER,
            "DEVICES",
            SANS_SEMIBOLD,
            10.5,
            alpha(k.c(INK), 179),
            0.84,
        );
        p.line_segment([at(73.0, top + 9.5), at(405.0, top + 9.5)], Stroke::new(1.0, k.c(LO)));
        let list = r(11.0, top + 24.0, 398.0, list_h);
        self.device_list(ui, &p, list, k);

        // transient message over the bottom of the list
        if let Some((err, msg, _)) = self.message.clone() {
            let bar = Rect::from_min_max(pos2(list.left() + 4.0, list.bottom() - 30.0), list.max - vec2(4.0, 4.0));
            p.rect_filled(bar, 6.0, k.c(LCD));
            p.rect_stroke(
                bar,
                6.0,
                Stroke::new(1.0, if err { k.c(BAD) } else { k.c(LCD_DIM) }),
                StrokeKind::Inside,
            );
            let m = elide(ui, &msg, SANS, 11.5, bar.width() - 20.0);
            let c = if err { k.c(BAD) } else { k.c(LCD_INK) };
            text(
                &p,
                bar.left_center() + vec2(10.0, 0.0),
                Align2::LEFT_CENTER,
                &m,
                SANS,
                11.5,
                c,
            );
            click(ui, bar, "message", &msg);
        }
    }

    /// "Check for updates" in the settings drawer, right-aligned at `right` (its vertical
    /// centre). Only a click asks GitHub; a newer release turns it into "Update to vX".
    fn update_button(&mut self, ui: &mut Ui, p: &Painter, right: Pos2, k: &Skin) {
        let (label, tip, ready) = match &self.update {
            UpdateState::Idle => (
                "Check for updates".to_string(),
                format!("This is v{}. Look for a newer release on GitHub.", update::CURRENT),
                false,
            ),
            UpdateState::Checking => ("Checking…".to_string(), String::new(), false),
            UpdateState::Available(r) if update::CAN_INSTALL => (
                format!("Update to {}", r.tag),
                "Download it from GitHub, replace the program files and restart".to_string(),
                true,
            ),
            UpdateState::Available(r) => (format!("Get {}", r.tag), r.page(), true),
            UpdateState::Installing(r) => (format!("Updating to {}…", r.tag), String::new(), false),
        };
        let w = text_w(ui, &label, SANS, 11.0) + 18.0;
        let rect = Rect::from_min_size(right - vec2(w, 11.0), vec2(w, 22.0));
        let resp = click(ui, rect, "update", &tip);
        let edge = if ready { alpha(k.c(OK), 160) } else { k.c(LCD_DIM) };
        if resp.hovered() {
            p.rect_filled(rect, 5.0, k.c(LO));
        }
        p.rect_stroke(rect, 5.0, Stroke::new(1.0, edge), StrokeKind::Inside);
        let ink = if ready { k.c(OK) } else { k.c(LCD_INK) };
        text(p, rect.center(), Align2::CENTER_CENTER, &label, SANS, 11.0, ink);
        if resp.clicked() {
            self.update_clicked(ui.ctx());
        }
    }

    fn display(&mut self, ui: &mut Ui, p: &Painter, o: Pos2, k: &Skin, cur: &Cur) {
        let at = |x: f32, y: f32| o + vec2(x, y);
        lcd_box(
            p,
            Rect::from_min_size(at(11.0, DISPLAY_Y), vec2(398.0, DISPLAY_H)),
            10.0,
            k,
            true,
        );
        // header
        let name = elide(
            ui,
            &cur.name.to_uppercase(),
            MONO,
            10.5,
            370.0 - 0.63 * cur.name.len() as f32,
        );
        spaced(
            p,
            at(25.0, 60.0),
            Align2::LEFT_CENTER,
            &name,
            MONO,
            10.5,
            k.c(LCD_INK),
            0.63,
        );
        gauge(p, Rect::from_min_max(at(25.0, 74.0), at(39.0, 128.0)), k, cur);
        // the percentage, right of the gauge: its 54 px line box ends at y 129
        let label = cur.pct.map_or_else(|| "--".to_string(), |v| format!("{v}%"));
        match k.digits {
            Digits::Grotesk => glow_text(
                p,
                at(52.0, 140.0),
                Align2::LEFT_BOTTOM,
                &label,
                GROTESK,
                60.0,
                cur.color,
                false,
            ),
            Digits::Mono => glow_text(
                p,
                at(52.0, 140.0),
                Align2::LEFT_BOTTOM,
                &label,
                MONO_SEMIBOLD,
                60.0,
                cur.color,
                false,
            ),
            Digits::Pixel => glow_text(
                p,
                at(50.5, 132.5),
                Align2::LEFT_BOTTOM,
                &label,
                PIXEL,
                60.0,
                cur.color,
                true,
            ),
        }
        // detail lines, right-aligned, centred on the digits
        for (y, s, c) in [(93.0, &cur.time_left, k.c(LCD_INK)), (110.0, &cur.status, k.c(LCD_DIM))] {
            text(p, at(395.0, y), Align2::RIGHT_CENTER, s, MONO, 11.5, c);
        }
    }

    fn drawer(&mut self, ui: &mut Ui, o: Pos2, k: &Skin, t: f32) {
        if t <= 0.0 {
            return;
        }
        let top = o.y + DRAWER_Y + 8.0 * t;
        let full = Rect::from_min_size(pos2(o.x + 11.0, top), vec2(398.0, DRAWER_H));
        let visible = Rect::from_min_size(full.min, vec2(398.0, DRAWER_H * t));
        let mut c = ui.new_child(UiBuilder::new().max_rect(full));
        c.set_clip_rect(visible.intersect(ui.clip_rect()));
        lcd_box(c.painter(), full, 10.0, k, false);
        let (device, app) = full.split_top_bottom_at_y(top + DEVICE_SETTINGS_H);
        self.drawer_content(&mut c, device, k);
        self.app_settings(&mut c, app, k);
    }

    /// The drawer's bottom row: the selected device's alert level, Tray (start at login, in
    /// the tray) and Check for updates.
    fn app_settings(&mut self, ui: &mut Ui, d: Rect, k: &Skin) {
        let p = ui.painter().clone();
        let at = |x: f32| pos2(d.left() + x - 11.0, d.center().y);
        let (ink, dim) = (k.c(LCD_INK), k.c(LCD_DIM));
        p.line_segment(
            [pos2(d.left() + 12.0, d.top()), pos2(d.right() - 12.0, d.top())],
            Stroke::new(1.0, k.c(LO)),
        );
        // alert level of the selected device
        let sel = self.selected.clone();
        let alert = sel.as_ref().map_or(20, |s| self.alert_level(s));
        text(&p, at(23.0), Align2::LEFT_CENTER, "Alert at", SANS, 11.5, dim);
        let step = Rect::from_center_size(at(111.0), vec2(78.0, 22.0));
        p.rect_stroke(step, 6.0, Stroke::new(1.0, dim), StrokeKind::Inside);
        for (label, delta, zone) in [
            ("−", -5i16, Rect::from_min_size(step.min, vec2(22.0, 22.0))),
            (
                "+",
                5,
                Rect::from_min_size(step.right_top() - vec2(22.0, 0.0), vec2(22.0, 22.0)),
            ),
        ] {
            let resp = click(ui, zone, ("alert", label), "Low-battery alert for this device");
            if resp.hovered() {
                p.rect_filled(zone.shrink(1.0), 5.0, k.c(LO));
            }
            text(&p, zone.center(), Align2::CENTER_CENTER, label, SANS_MEDIUM, 11.0, ink);
            if resp.clicked()
                && let Some(sel) = &sel
            {
                let v = (alert as i16 + delta).clamp(5, 50) as u8;
                self.settings.set_alert(sel, v);
                self.save_settings();
            }
        }
        let value = if sel.is_some() {
            format!("{alert}%")
        } else {
            "—".into()
        };
        text(
            &p,
            step.center() + vec2(0.0, 0.5),
            Align2::CENTER_CENTER,
            &value,
            MONO,
            11.5,
            k.c(BAD),
        );
        // tray = start at login
        let tray = Rect::from_min_size(at(162.0) - vec2(0.0, 11.0), vec2(56.0, 22.0));
        let resp = click(ui, tray, "tray", "Start at login, in the tray");
        if resp.hovered() {
            p.rect_filled(tray, 5.0, k.c(LO));
        }
        p.rect_stroke(tray, 5.0, Stroke::new(1.0, dim), StrokeKind::Inside);
        let on = self.autostart_gui;
        let led = Rect::from_center_size(tray.left_center() + vec2(12.0, 0.0), vec2(6.0, 6.0));
        if on {
            glow(&p, led, 3.0, 5, k.c(OK));
        }
        p.circle_filled(led.center(), 3.0, if on { k.c(OK) } else { dim });
        text(
            &p,
            tray.left_center() + vec2(21.0, 0.0),
            Align2::LEFT_CENTER,
            "Tray",
            SANS,
            11.0,
            ink,
        );
        if resp.clicked() {
            self.set_autostart(!on);
        }
        self.update_button(ui, &p, at(397.0), k);
    }

    fn drawer_content(&mut self, ui: &mut Ui, d: Rect, k: &Skin) {
        let p = ui.painter().clone();
        // design coordinates: x from the widget's left edge, y from the drawer's top
        let at = |x: f32, y: f32| pos2(d.left() + x - 11.0, d.top() + y);
        let center_lines = |lines: &[(&str, Color32)]| {
            let n = lines.len() as f32;
            for (i, (s, c)) in lines.iter().enumerate() {
                let y = d.center().y + (i as f32 - (n - 1.0) / 2.0) * 19.0;
                text(&p, pos2(d.center().x, y), Align2::CENTER_CENTER, s, SANS, 11.5, *c);
            }
        };
        let Some(sel) = self.selected.clone() else {
            center_lines(&[("No device selected", k.c(LCD_INK))]);
            return;
        };
        let Some(s) = self.snap.clone().filter(|s| s.sel == sel) else {
            let online = self.rows.iter().any(|r| r.sel == sel && r.online);
            if online {
                center_lines(&[("Reading device…", k.c(LCD_INK))]);
            } else {
                center_lines(&[("Offline", k.c(LCD_INK)), ("Move it or switch it on.", k.c(LCD_DIM))]);
            }
            return;
        };
        let busy = self.pending > 0;
        let mut cmd: Option<Cmd> = None;
        let mut import = None;
        let dim = k.c(LCD_DIM);
        let ink = k.c(LCD_INK);
        if s.dpi.is_some() || s.rate.is_some() || s.mode.is_some() {
            let onboard = s.mode == Some(MODE_ONBOARD);
            let locked = onboard || busy;
            if let Some(dpi) = &s.dpi {
                text(&p, at(23.0, 29.5), Align2::LEFT_CENTER, "DPI", SANS, 11.5, dim);
                let field = Rect::from_min_size(at(64.0, 17.0), vec2(70.0, 24.0));
                p.rect_stroke(field, 6.0, Stroke::new(1.0, dim), StrokeKind::Inside);
                let mut child = ui.new_child(UiBuilder::new().max_rect(field.shrink2(vec2(8.0, 4.0))));
                let v = child.visuals_mut();
                v.extreme_bg_color = Color32::TRANSPARENT;
                v.selection.stroke = Stroke::NONE;
                for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
                    w.bg_stroke = Stroke::NONE;
                }
                let edit = egui::TextEdit::singleline(&mut self.dpi_input)
                    .frame(egui::Frame::NONE)
                    .desired_width(54.0)
                    .font(font(MONO, 11.5))
                    .text_color(if locked { dim } else { ink })
                    .align(egui::Align2::LEFT_CENTER);
                let resp = child.add_enabled(!locked, edit);
                let enter = resp.lost_focus() && child.input(|i| i.key_pressed(egui::Key::Enter));
                let apply = Rect::from_min_size(at(140.0, 17.0), vec2(49.0, 24.0));
                let ar = click(ui, apply, "apply", "");
                raised(
                    &p,
                    apply,
                    6.0,
                    k.c(FACE),
                    k,
                    false,
                    if ar.hovered() { 1.15 } else { 1.0 },
                );
                text(&p, apply.center(), Align2::CENTER_CENTER, "Apply", SANS, 11.5, k.c(INK));
                if (ar.clicked() || enter) && !locked {
                    match self.dpi_input.trim().parse::<u16>() {
                        Ok(v) if dpi.caps.allows(v) => cmd = Some(Cmd::SetDpi(v)),
                        _ => self.say(true, format!("DPI must be one of: {}", dpi.caps)),
                    }
                } else if ar.clicked() && onboard {
                    self.say(true, "Switch to host mode to change DPI.".into());
                }
                text(
                    &p,
                    at(195.0, 29.0),
                    Align2::LEFT_CENTER,
                    &format!("now {}", dpi.current),
                    MONO,
                    10.5,
                    dim,
                );
            }
            if let Some((current, allowed)) = &s.rate {
                text(&p, at(23.0, 60.5), Align2::LEFT_CENTER, "Rate", SANS, 11.5, dim);
                let mut x = 64.0;
                for hz in allowed {
                    let label = if *hz >= 1000 {
                        format!("{}k", hz / 1000)
                    } else {
                        hz.to_string()
                    };
                    let w = text_w(ui, &label, MONO, 10.5) + 16.0;
                    let rect = Rect::from_min_size(at(x, 49.0), vec2(w, 22.0));
                    let resp = chip_at(ui, rect, &label, MONO, 10.5, k, hz == current, "rate")
                        .on_hover_text(format!("{hz} Hz"));
                    if resp.clicked() && hz != current {
                        if locked {
                            self.say(true, "Switch to host mode to change the report rate.".into());
                        } else {
                            cmd = Some(Cmd::SetRate(*hz));
                        }
                    }
                    x += w + 3.0;
                }
            }
            if let Some(m) = s.mode {
                text(&p, at(23.0, 90.5), Align2::LEFT_CENTER, "Mode", SANS, 11.5, dim);
                let (label, action, to, tip) = if m == MODE_ONBOARD {
                    (
                        "Onboard profile",
                        "Use host",
                        MODE_HOST,
                        "Let this PC set DPI and report rate (kept as this mouse's preset)",
                    )
                } else {
                    (
                        "Host — settings from this PC",
                        "Use onboard",
                        MODE_ONBOARD,
                        "Go back to the profile stored on the mouse",
                    )
                };
                text(&p, at(64.0, 90.5), Align2::LEFT_CENTER, label, SANS, 11.5, ink);
                let w = text_w(ui, action, SANS, 11.5) + 20.0;
                let rect = Rect::from_min_size(at(397.0 - w, 79.0), vec2(w, 22.0));
                if chip_at(ui, rect, action, SANS, 11.5, k, false, "mode")
                    .on_hover_text(tip)
                    .clicked()
                    && !busy
                {
                    cmd = Some(Cmd::SetMode(to));
                }
            }
            if let Some(c) = &cmd {
                match c {
                    Cmd::SetDpi(_) | Cmd::SetRate(_) => self.save_preset = true,
                    // Back to onboard: drop the preset, or it would switch to host again on connect.
                    Cmd::SetMode(m) if *m == MODE_ONBOARD && s.preset.is_some() => {
                        self.send(&sel, Cmd::SavePreset { dpi: None, rate: None });
                    }
                    _ => {}
                }
            }
        } else if let Some(h) = s.headset {
            // Corsair headset: one row of chips per setting it has
            let mut row = |y: f32, name: &str, chips: &[(String, Cmd, bool)], tip: &str| {
                text(&p, at(23.0, y + 11.5), Align2::LEFT_CENTER, name, SANS, 11.5, dim);
                let mut x = 86.0;
                for (label, c, on) in chips {
                    let w = text_w(ui, label, MONO, 10.5) + 16.0;
                    let rect = Rect::from_min_size(at(x, y), vec2(w, 22.0));
                    if chip_at(ui, rect, label, MONO, 10.5, k, *on, name)
                        .on_hover_text(tip)
                        .clicked()
                        && !on
                        && !busy
                    {
                        cmd = Some(c.clone());
                    }
                    x += w + 3.0;
                }
            };
            if let Some(b) = h.brightness {
                let chips = [
                    ("On".to_string(), Cmd::SetLights(true), b > 0),
                    ("Off".to_string(), Cmd::SetLights(false), b == 0),
                ];
                row(18.0, "Lighting", &chips, "Headset LEDs on or off");
            }
            if let Some(m) = h.auto_off_minutes() {
                let chips: Vec<_> = [0u32, 5, 10, 15, 30, 60]
                    .iter()
                    .map(|&v| {
                        let label = if v == 0 { "Never".to_string() } else { format!("{v}m") };
                        (label, Cmd::SetAutoOff(v), m == v)
                    })
                    .collect();
                row(
                    49.0,
                    "Auto-off",
                    &chips,
                    "Switch the headset off after this long without sound",
                );
            }
            if let Some(muted) = h.mic_muted {
                // read-only: the headset refuses a mute command; its boom or button decides
                text(&p, at(23.0, 90.5), Align2::LEFT_CENTER, "Mic", SANS, 11.5, dim);
                let state = if muted { "Muted" } else { "On" };
                text(&p, at(86.0, 90.5), Align2::LEFT_CENTER, state, SANS, 11.5, ink);
                text(
                    &p,
                    at(397.0, 90.5),
                    Align2::RIGHT_CENTER,
                    "change it on the headset",
                    SANS,
                    10.5,
                    dim,
                );
            }
        } else if let Some(label) = s.curve_label.clone() {
            let note = if s.needs_curve {
                "an estimate until one full discharge is recorded"
            } else if label.starts_with("your") {
                "measured on this headset"
            } else {
                "from the curve file in the peripheral-lite folder"
            };
            let cy = d.center().y;
            text(
                &p,
                pos2(d.center().x, cy - 26.0),
                Align2::CENTER_CENTER,
                "No adjustable settings",
                SANS,
                11.5,
                ink,
            );
            text(
                &p,
                pos2(d.center().x, cy - 7.0),
                Align2::CENTER_CENTER,
                &format!("Battery: {note}."),
                SANS,
                11.5,
                dim,
            );
            let mut actions = vec![("Build from recording", s.recording.clone().unwrap_or_default())];
            let ghub = self
                .ghub_curve
                .as_ref()
                .and_then(|g| g.1.clone())
                .filter(|_| s.needs_curve);
            if let Some(g) = &ghub {
                actions.push(("Import from G HUB", g.display().to_string()));
            }
            actions.push(("Open folder", self.dir.display().to_string()));
            let widths: Vec<f32> = actions.iter().map(|(a, _)| text_w(ui, a, SANS, 11.0) + 18.0).collect();
            let total: f32 = widths.iter().sum::<f32>() + 6.0 * (widths.len() as f32 - 1.0);
            let mut x = d.center().x - total / 2.0;
            for ((a, tip), w) in actions.iter().zip(widths) {
                let rect = Rect::from_min_size(pos2(x, cy + 14.0), vec2(w, 22.0));
                if chip_at(ui, rect, a, SANS, 11.0, k, false, "curve")
                    .on_hover_text(tip)
                    .clicked()
                {
                    match *a {
                        "Build from recording" if !busy => cmd = Some(Cmd::BuildCurve),
                        "Import from G HUB" => import = ghub.clone(),
                        "Open folder" => {
                            let _ = std::fs::create_dir_all(&self.dir);
                            peripheral_lite::platform::open_folder(&self.dir);
                        }
                        _ => {}
                    }
                }
                x += w + 6.0;
            }
        } else {
            center_lines(&[
                ("No adjustable settings", ink),
                ("This device has no DPI, report rate or battery curve.", dim),
            ]);
        }
        if let Some(c) = cmd {
            self.message = None;
            self.send(&sel, c);
        }
        if let Some(src) = import {
            match peripheral_lite::battery::import_curve(&src, &self.dir) {
                Ok(dest) => {
                    self.say(false, format!("Imported {}", dest.display()));
                    self.ghub_curve = None;
                    self.send(&sel, Cmd::Refresh);
                }
                Err(e) => self.say(true, format!("Import failed: {e}")),
            }
        }
    }

    fn device_list(&mut self, ui: &mut Ui, p: &Painter, list: Rect, k: &Skin) {
        p.rect_filled(list, 9.0, k.c(LCD));
        p.rect_stroke(list, 9.0, Stroke::new(1.0, k.c(LO)), StrokeKind::Inside);
        if self.rows.is_empty() {
            let msg = if self.fatal.is_some() {
                "Device access failed"
            } else {
                "Looking for devices…"
            };
            text(p, list.center(), Align2::CENTER_CENTER, msg, SANS, 12.5, k.c(LCD_DIM));
            return;
        }
        let mut clicked = None;
        for (i, r) in self.rows.iter().enumerate() {
            let rr = Rect::from_min_size(list.min + vec2(4.0, 4.0 + i as f32 * 32.0), vec2(390.0, 30.0));
            let resp = click(ui, rr, ("row", &r.sel), "");
            let selected = self.selected.as_deref() == Some(r.sel.as_str());
            if selected || resp.hovered() {
                p.rect_filled(rr, 6.0, k.c(LO));
            }
            let alert = self.alert_level(&r.sel);
            let pct = r.percent.filter(|_| r.online);
            let color = pct.map_or(k.c(LCD_DIM), |v| k.level(v, alert));
            let y = rr.top() + 15.0;
            text(
                p,
                pos2(rr.left() + 10.0, y),
                Align2::LEFT_CENTER,
                &(i + 1).to_string(),
                MONO,
                10.5,
                k.c(LCD_DIM),
            );
            let name = elide(ui, &r.name, SANS, 12.5, 240.0);
            let ink = if r.online { k.c(LCD_INK) } else { k.c(LCD_DIM) };
            text(
                p,
                pos2(rr.left() + 26.0, y),
                Align2::LEFT_CENTER,
                &name,
                SANS,
                12.5,
                ink,
            );
            let bar = Rect::from_min_size(pos2(rr.left() + 284.0, rr.top() + 13.0), vec2(48.0, 4.0));
            p.rect_filled(bar, 2.0, k.c(LO));
            if let Some(v) = pct {
                let fill = Rect::from_min_size(bar.min, vec2(48.0 * v as f32 / 100.0, 4.0));
                p.rect_filled(fill, 0.0, color);
            }
            let label = pct.map_or_else(|| if r.online { "—".into() } else { "off".into() }, |v| format!("{v}%"));
            text(
                p,
                pos2(rr.right() - 10.0, y),
                Align2::RIGHT_CENTER,
                &label,
                MONO,
                11.5,
                color,
            );
            if resp.clicked() {
                clicked = Some(r.sel.clone());
            }
        }
        if let Some(sel) = clicked {
            self.select(sel);
        }
    }

    // ------------------------------------------------------------------ skin manager

    /// The skin manager, painted at the design's measured positions; returns its height.
    fn manager(&mut self, ui: &mut Ui, o: Pos2, k: &Skin) -> f32 {
        const W: f32 = MANAGER_W;
        let p = ui.painter().clone();
        let at = |x: f32, y: f32| o + vec2(x, y);
        text(
            &p,
            at(0.0, 13.5),
            Align2::LEFT_CENTER,
            "Skins",
            SANS_SEMIBOLD,
            20.0,
            M_TEXT,
        );
        text(
            &p,
            at(0.0, 39.5),
            Align2::LEFT_CENTER,
            "Pick one, tweak the colours, or load a skin file someone shared.",
            SANS,
            13.0,
            M_SUB,
        );

        // cards, three per row
        let pct = self.current(k).pct.unwrap_or(53);
        let cw = (W - 20.0) / 3.0;
        let mut pick = None;
        for (i, s) in self.skins.iter().enumerate() {
            let card = Rect::from_min_size(
                at((i % 3) as f32 * (cw + 10.0), 70.0 + (i / 3) as f32 * 106.0),
                vec2(cw, 96.0),
            );
            let resp = click(ui, card, ("skin", &s.id), "");
            p.rect_filled(card, 10.0, if resp.hovered() { M_CARD_HOVER } else { M_CARD });
            let border = if s.id == k.id { M_ACCENT } else { M_EDGE };
            p.rect_stroke(card, 10.0, Stroke::new(1.0, border), StrokeKind::Inside);
            let frame = Rect::from_min_size(card.min + vec2(7.0, 7.0), vec2(cw - 14.0, 46.0));
            p.rect_filled(frame, 7.0, s.c(HI));
            p.rect_filled(
                Rect::from_min_max(frame.min + vec2(0.0, 1.0), frame.max),
                7.0,
                s.c(FRAME),
            );
            let lcd = frame.shrink(4.0);
            p.rect_filled(lcd, 5.0, s.c(LCD));
            text(
                &p,
                lcd.center(),
                Align2::CENTER_CENTER,
                &format!("{pct}%"),
                GROTESK,
                22.0,
                s.c(OK),
            );
            let name = elide(ui, &s.name, SANS_MEDIUM, 12.0, cw - 14.0);
            let author = elide(ui, &s.author, SANS, 10.5, cw - 14.0);
            text(
                &p,
                card.min + vec2(7.0, 66.5),
                Align2::LEFT_CENTER,
                &name,
                SANS_MEDIUM,
                12.0,
                M_TEXT,
            );
            text(
                &p,
                card.min + vec2(7.0, 82.0),
                Align2::LEFT_CENTER,
                &author,
                SANS,
                10.5,
                M_DIM,
            );
            if resp.clicked() {
                pick = Some(s.id.clone());
            }
        }
        if let Some(id) = pick {
            self.settings.skin = id;
            self.skin_msg = None;
            self.save_settings();
        }
        let rows = self.skins.len().div_ceil(3) as f32;
        let mut y = 70.0 + rows * 106.0 - 10.0 + 22.0;

        // load / export
        let bw = (W - 8.0) / 2.0;
        let load = Rect::from_min_size(at(0.0, y), vec2(bw, 36.0));
        let resp = click(
            ui,
            load,
            "load",
            "Load a skin file (you can also drop one on the window)",
        );
        p.rect_filled(load, 8.0, if resp.hovered() { M_TEAL_HOVER } else { M_TEAL });
        text(
            &p,
            load.center(),
            Align2::CENTER_CENTER,
            "Load skin file…",
            SANS_MEDIUM,
            13.0,
            Color32::WHITE,
        );
        if resp.clicked()
            && let Some(path) = os::skin_file_dialog(false, "")
        {
            self.import_skin(&path);
        }
        let export = Rect::from_min_size(at(bw + 8.0, y), vec2(W - bw - 8.0, 36.0));
        let resp = click(ui, export, "export", "");
        if resp.hovered() {
            p.rect_filled(export, 8.0, M_CARD_HOVER);
        }
        p.rect_stroke(export, 8.0, Stroke::new(1.0, M_EDGE2), StrokeKind::Inside);
        text(
            &p,
            export.center(),
            Align2::CENTER_CENTER,
            "Export current",
            SANS,
            13.0,
            M_TEXT,
        );
        if resp.clicked() {
            let mut out = k.clone();
            if out.builtin() {
                out.author = "you".into();
            }
            let name = format!("{}.skin.json", skin::slug(&out.name));
            if let Some(path) = os::skin_file_dialog(true, &name) {
                self.skin_msg = Some(match peripheral_lite::config::write_atomic(&path, &out.to_json()) {
                    Ok(()) => (
                        true,
                        format!("Exported {}", path.file_name().unwrap_or_default().to_string_lossy()),
                    ),
                    Err(e) => (false, format!("Export failed: {e}")),
                });
            }
        }
        y += 36.0 + 22.0;
        if let Some((ok, msg)) = &self.skin_msg {
            // the design pulls the message up 12 px towards the buttons
            text(
                &p,
                at(0.0, y - 12.0 + 8.0),
                Align2::LEFT_CENTER,
                msg,
                SANS,
                12.0,
                if *ok { M_ACCENT } else { M_ERR },
            );
            y += 16.0 - 12.0 + 22.0;
        }

        // editor
        let ed = Rect::from_min_size(at(0.0, y), vec2(W, 272.0));
        p.rect_filled(ed, 12.0, M_CARD);
        p.rect_stroke(ed, 12.0, Stroke::new(1.0, M_EDGE), StrokeKind::Inside);
        let e = |x: f32, yy: f32| ed.min + vec2(x, yy);
        text(
            &p,
            e(17.0, 25.5),
            Align2::LEFT_CENTER,
            &format!("Edit “{}”", k.name),
            SANS_SEMIBOLD,
            13.0,
            M_TEXT,
        );
        let note = if k.builtin() {
            "Edits save as a new custom skin"
        } else {
            "Saved automatically"
        };
        text(&p, e(W - 17.0, 26.0), Align2::RIGHT_CENTER, note, SANS, 11.0, M_DIM);
        let col = (W - 34.0 - 14.0) / 2.0;
        let mut change: Option<(usize, [u8; 3])> = None;
        for (i, (_, label)) in FIELDS.iter().enumerate() {
            let x = 17.0 + (i % 2) as f32 * (col + 14.0);
            let row = 46.0 + (i / 2) as f32 * 30.0;
            let swatch = Rect::from_min_size(e(x, row), vec2(26.0, 22.0));
            let mut child = ui.new_child(UiBuilder::new().max_rect(swatch));
            child.spacing_mut().interact_size = vec2(26.0, 22.0);
            let v = child.visuals_mut();
            for w in [
                &mut v.widgets.inactive,
                &mut v.widgets.hovered,
                &mut v.widgets.active,
                &mut v.widgets.open,
            ] {
                w.bg_stroke = Stroke::new(1.0, M_EDGE2);
                w.corner_radius = 4.0.into();
                w.expansion = 0.0;
            }
            let mut c = k.colors[i];
            let resp = egui::color_picker::color_edit_button_srgb(&mut child, &mut c);
            if resp.changed() {
                change = Some((i, c));
            }
            // the browser's colour input: a framed well with the colour inset
            p.rect_filled(swatch, 4.0, if resp.hovered() { M_CARD_HOVER } else { M_CARD });
            p.rect_stroke(swatch, 4.0, Stroke::new(1.0, M_EDGE2), StrokeKind::Inside);
            let [r, g, b] = k.colors[i];
            let well = Rect::from_min_max(swatch.min + vec2(4.0, 4.0), swatch.max - vec2(4.0, 4.0));
            p.rect_filled(well, 0.0, Color32::from_rgb(r, g, b));
            p.rect_stroke(
                well,
                0.0,
                Stroke::new(1.0, Color32::from_gray(0x77)),
                StrokeKind::Inside,
            );
            text(
                &p,
                e(x + 34.0, row + 11.5),
                Align2::LEFT_CENTER,
                label,
                SANS,
                12.0,
                M_SOFT,
            );
            text(
                &p,
                e(x + col, row + 11.0),
                Align2::RIGHT_CENTER,
                &skin::hex_of(k.colors[i]),
                MONO,
                10.5,
                M_FAINT,
            );
        }
        let dy = 230.0;
        text(
            &p,
            e(17.0, dy + 12.5),
            Align2::LEFT_CENTER,
            "Digits",
            SANS,
            12.0,
            M_SOFT,
        );
        let mut x = 55.0;
        let mut digits = None;
        for d in Digits::ALL {
            let on = k.digits == d;
            let w = text_w(ui, d.label(), SANS, 11.5) + 20.0;
            let rect = Rect::from_min_size(e(x, dy), vec2(w, 25.0));
            let resp = click(ui, rect, ("digits", d.key()), "");
            if on {
                p.rect_filled(rect, 5.0, Color32::from_rgb(0x1f, 0x34, 0x38));
            }
            p.rect_stroke(
                rect,
                5.0,
                Stroke::new(1.0, if on { M_ACCENT } else { M_EDGE2 }),
                StrokeKind::Inside,
            );
            text(&p, rect.center(), Align2::CENTER_CENTER, d.label(), SANS, 11.5, M_SOFT);
            if resp.clicked() && !on {
                digits = Some(d);
            }
            x += w + 4.0;
        }
        if let Some((i, c)) = change {
            self.edit_skin(|s| s.colors[i] = c);
        }
        if let Some(d) = digits {
            self.edit_skin(|s| s.digits = d);
        }
        y += 272.0 + 22.0;

        // footnote, with ".json" in the mono face
        let mut job = egui::text::LayoutJob::default();
        let fmt = |family: &str, color: Color32| egui::TextFormat {
            font_id: font(family, 11.5),
            color,
            line_height: Some(11.5 * 1.6),
            ..Default::default()
        };
        job.append("A skin is a small ", 0.0, fmt(SANS, M_FAINT));
        job.append(".json", 0.0, fmt(MONO, Color32::from_rgb(0xa9, 0xac, 0xb2)));
        job.append(
            " file with a name, author, eleven colours and a digit font. Export one to see the format, edit it in any text editor, and share it.",
            0.0,
            fmt(SANS, M_FAINT),
        );
        job.wrap.max_width = W;
        let galley = p.layout_job(job);
        let h = galley.size().y;
        p.galley(at(0.0, y), galley, M_FAINT);
        y + h
    }

    /// Change the current skin; a built-in one is first copied to a new custom skin.
    fn edit_skin(&mut self, f: impl FnOnce(&mut Skin)) {
        let dir = self.dir.join("skins");
        let cur = self.skin().clone();
        let id = if cur.builtin() {
            let mut n = cur.clone();
            n.name = format!("{} (mine)", cur.name);
            n.author = "you".into();
            n.id = skin::free_id(&dir, &n.name, &self.skins);
            let id = n.id.clone();
            self.skins.push(n);
            self.settings.skin = id.clone();
            self.save_settings();
            id
        } else {
            cur.id
        };
        if let Some(s) = self.skins.iter_mut().find(|s| s.id == id) {
            f(s);
        }
        self.skin_dirty = Some(std::time::Instant::now());
    }

    pub fn import_skin(&mut self, path: &std::path::Path) {
        let stem = path
            .file_name()
            .map(|n| {
                n.to_string_lossy()
                    .trim_end_matches(".json")
                    .trim_end_matches(".skin")
                    .to_string()
            })
            .unwrap_or_default();
        let dir = self.dir.join("skins");
        let r = std::fs::read_to_string(path)
            .map_err(|e| e.to_string())
            .and_then(|t| skin::Skin::from_json(&t, "", &stem))
            .and_then(|mut s| {
                s.id = skin::free_id(&dir, &s.name, &self.skins);
                skin::save(&dir, &s).map(|_| s)
            });
        self.skin_msg = Some(match r {
            Ok(s) => {
                let msg = format!("Loaded “{}”", s.name);
                self.settings.skin = s.id.clone();
                self.skins.push(s);
                self.save_settings();
                (true, msg)
            }
            Err(e) => (false, format!("Couldn’t read that skin: {e}")),
        });
        self.show_skins = true;
    }

    fn take_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for p in dropped {
            if p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")) {
                self.import_skin(&p);
            }
        }
    }
}

// ---------------------------------------------------------------------- painted parts

/// The segment gauge left of the percentage: one segment per 10 %, the alert level as a red
/// line across it.
fn gauge(p: &Painter, rect: Rect, k: &Skin, cur: &Cur) {
    let gap = 1.5;
    let seg_h = (rect.height() - gap * 9.0) / 10.0;
    let lit = cur.pct.map_or(0, |v| ((v as u32 + 5) / 10) as usize);
    for i in 0..10 {
        let y = rect.bottom() - (i as f32 + 1.0) * seg_h - i as f32 * gap;
        let r = Rect::from_min_size(pos2(rect.left(), y), vec2(rect.width(), seg_h));
        if i < lit {
            glow(p, r, 1.5, 5, alpha(cur.color, 102));
            p.rect_filled(r, 1.5, cur.color);
        } else {
            p.rect_filled(r, 1.5, k.c(LO));
        }
    }
    let y = rect.bottom() - rect.height() * cur.alert as f32 / 100.0 - 1.0;
    let line = Rect::from_min_size(pos2(rect.left() - 3.0, y), vec2(rect.width() + 6.0, 2.0));
    glow(p, line, 1.0, 6, k.c(BAD));
    p.rect_filled(line, 1.0, k.c(BAD));
}

fn history_strip(ui: &mut Ui, p: &Painter, o: Pos2, k: &Skin, cur: &Cur) {
    let rect = Rect::from_min_size(o + vec2(11.0, HISTORY_Y), vec2(398.0, 44.0));
    lcd_box(p, rect, 8.0, k, true);
    let inner = Rect::from_min_max(rect.min + vec2(6.0, 5.0), rect.max - vec2(6.0, 5.0));
    let gap = 2.0;
    let bw = (inner.width() - gap * (BARS as f32 - 1.0)) / BARS as f32;
    for (i, v) in cur.bars.iter().enumerate() {
        let Some(v) = v else { continue };
        let h = inner.height() * *v as f32 / 100.0;
        let x = inner.left() + i as f32 * (bw + gap);
        let r = Rect::from_min_max(pos2(x, inner.bottom() - h), pos2(x + bw, inner.bottom()));
        p.rect_filled(r, 1.5, alpha(k.level(*v, cur.alert), 230));
    }
    let y = rect.bottom() - (5.0 + rect.height() * cur.alert as f32 / 100.0 * 0.78);
    p.extend(Shape::dashed_line(
        &[pos2(inner.left(), y), pos2(inner.right(), y)],
        Stroke::new(1.0, alpha(k.c(BAD), 153)),
        3.0,
        3.0,
    ));
    ui.interact(rect, ui.scope_id().with("history"), Sense::hover())
        .on_hover_text("Battery over the last 24 hours, one bar per hour");
}
