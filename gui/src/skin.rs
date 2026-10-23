//! Skins: a name, an author, eleven colours and a digit style. Built-ins live here; custom
//! skins are small `.json` files in `<preset folder>\skins\` that can be shared, loaded and
//! exported. App settings (chosen skin, alert levels) are a tiny text file next to them.

use std::fs;
use std::path::{Path, PathBuf};

use eframe::egui::Color32;
use peripheral_lite::config::write_atomic;

/// Colour slots, in the order the editor shows them: (JSON key, label).
pub const FIELDS: [(&str, &str); 11] = [
    ("frame", "Frame"),
    ("face", "Buttons"),
    ("hi", "Highlight"),
    ("lo", "Edge"),
    ("ink", "Frame text"),
    ("lcd", "Display"),
    ("lcdInk", "Display text"),
    ("lcdDim", "Display dim"),
    ("ok", "Battery good"),
    ("warn", "Battery mid"),
    ("bad", "Battery low"),
];
pub const FRAME: usize = 0;
pub const FACE: usize = 1;
pub const HI: usize = 2;
pub const LO: usize = 3;
pub const INK: usize = 4;
pub const LCD: usize = 5;
pub const LCD_INK: usize = 6;
pub const LCD_DIM: usize = 7;
pub const OK: usize = 8;
pub const WARN: usize = 9;
pub const BAD: usize = 10;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Digits {
    Grotesk,
    Mono,
    Pixel,
}

impl Digits {
    pub const ALL: [Digits; 3] = [Digits::Grotesk, Digits::Mono, Digits::Pixel];

    pub fn key(self) -> &'static str {
        match self {
            Digits::Grotesk => "grotesk",
            Digits::Mono => "mono",
            Digits::Pixel => "pixel",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Digits::Grotesk => "Grotesk",
            Digits::Mono => "Mono",
            Digits::Pixel => "Pixel",
        }
    }

    fn from_key(k: &str) -> Digits {
        Digits::ALL
            .into_iter()
            .find(|d| d.key() == k)
            .unwrap_or(Digits::Grotesk)
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Skin {
    /// Built-in id, or the file stem of a custom skin.
    pub id: String,
    pub name: String,
    pub author: String,
    pub colors: [[u8; 3]; 11],
    pub digits: Digits,
}

impl Skin {
    pub fn c(&self, i: usize) -> Color32 {
        let [r, g, b] = self.colors[i];
        Color32::from_rgb(r, g, b)
    }

    pub fn builtin(&self) -> bool {
        self.author == "built-in"
    }

    /// Colour for a battery level: low at or under the alert level, mid up to 20 above it.
    pub fn level(&self, pct: u8, alert: u8) -> Color32 {
        if pct <= alert {
            self.c(BAD)
        } else if pct <= alert.saturating_add(20) {
            self.c(WARN)
        } else {
            self.c(OK)
        }
    }

    pub fn to_json(&self) -> String {
        let mut s = format!(
            "{{\n  \"name\": \"{}\",\n  \"author\": \"{}\",\n",
            esc(&self.name),
            esc(&self.author)
        );
        for (i, (key, _)) in FIELDS.iter().enumerate() {
            s.push_str(&format!("  \"{key}\": \"{}\",\n", hex(self.colors[i])));
        }
        s.push_str(&format!("  \"font\": \"{}\"\n}}\n", self.digits.key()));
        s
    }

    /// Parse a skin file: every colour must be present as `#rrggbb`.
    pub fn from_json(text: &str, id: &str, fallback_name: &str) -> Result<Skin, String> {
        let kv = parse_flat_json(text)?;
        let get = |k: &str| kv.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str());
        let mut colors = [[0u8; 3]; 11];
        let mut missing = Vec::new();
        for (i, (key, _)) in FIELDS.iter().enumerate() {
            match get(key).and_then(parse_hex) {
                Some(c) => colors[i] = c,
                None => missing.push(*key),
            }
        }
        if !missing.is_empty() {
            return Err(format!("missing colours: {}", missing.join(", ")));
        }
        Ok(Skin {
            id: id.to_string(),
            name: get("name")
                .filter(|n| !n.trim().is_empty())
                .unwrap_or(fallback_name)
                .to_string(),
            author: get("author")
                .filter(|a| !a.trim().is_empty() && *a != "built-in")
                .unwrap_or("imported")
                .to_string(),
            colors,
            digits: Digits::from_key(get("font").unwrap_or("")),
        })
    }
}

fn hex([r, g, b]: [u8; 3]) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

pub fn hex_of(c: [u8; 3]) -> String {
    hex(c)
}

fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    Some([v(0)?, v(2)?, v(4)?])
}

fn esc(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            '"' => vec!['\\', '"'],
            '\\' => vec!['\\', '\\'],
            c if (c as u32) < 0x20 => vec![' '],
            c => vec![c],
        })
        .collect()
}

/// Parse a flat JSON object of string values (other value types are skipped). Enough for skin
/// files, without a JSON library.
fn parse_flat_json(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut it = text.trim_start_matches('\u{feff}').chars().peekable();
    let skip_ws = |it: &mut std::iter::Peekable<std::str::Chars>| {
        while it.peek().is_some_and(|c| c.is_whitespace()) {
            it.next();
        }
    };
    let string = |it: &mut std::iter::Peekable<std::str::Chars>| -> Result<String, String> {
        let mut s = String::new();
        loop {
            match it.next().ok_or("unterminated string")? {
                '"' => return Ok(s),
                '\\' => match it.next().ok_or("bad escape")? {
                    'n' | 't' | 'r' => s.push(' '),
                    'u' => {
                        let code: String = it.by_ref().take(4).collect();
                        let c = u32::from_str_radix(&code, 16).ok().and_then(char::from_u32);
                        s.push(c.unwrap_or('?'));
                    }
                    c => s.push(c),
                },
                c => s.push(c),
            }
        }
    };
    skip_ws(&mut it);
    if it.next() != Some('{') {
        return Err("not a JSON object".into());
    }
    let mut out = Vec::new();
    loop {
        skip_ws(&mut it);
        match it.next() {
            Some('}') => return Ok(out),
            Some(',') => continue,
            Some('"') => {}
            _ => return Err("expected a key".into()),
        }
        let key = string(&mut it)?;
        skip_ws(&mut it);
        if it.next() != Some(':') {
            return Err(format!("expected ':' after \"{key}\""));
        }
        skip_ws(&mut it);
        if it.peek() == Some(&'"') {
            it.next();
            out.push((key, string(&mut it)?));
        } else {
            // number, bool, null: skip to the next ',' or '}' (nested values are not expected)
            while it.peek().is_some_and(|&c| c != ',' && c != '}') {
                it.next();
            }
        }
    }
}

const fn rgb(v: u32) -> [u8; 3] {
    [(v >> 16) as u8, (v >> 8) as u8, v as u8]
}

/// Built-in skins: (id, name, colours in FIELDS order, digits).
const BUILTIN: [(&str, &str, [u32; 11], Digits); 5] = [
    (
        "gunmetal",
        "Gunmetal",
        [
            0x2c2f35, 0x383c43, 0x4a4e57, 0x16181b, 0xd6d9de, 0x0a0d0b, 0xbff5cf, 0x2f5a3e, 0x3ddc84, 0xf2c94c,
            0xff5a4d,
        ],
        Digits::Grotesk,
    ),
    (
        "amber",
        "Amber CRT",
        [
            0x2b231b, 0x3a2f24, 0x4d3f30, 0x120d08, 0xe8c9a0, 0x140b02, 0xffd59a, 0x6b4814, 0xffb43d, 0xffe08a,
            0xff5a2a,
        ],
        Digits::Pixel,
    ),
    (
        "arctic",
        "Arctic",
        [
            0xdfe3e8, 0xeef1f4, 0xffffff, 0xb4bac3, 0x2a2e35, 0xf7f9fa, 0x1d232b, 0x9aa3ae, 0x1f9d61, 0xb7791f,
            0xd03a2f,
        ],
        Digits::Grotesk,
    ),
    (
        "synth",
        "Synthline",
        [
            0x22163a, 0x30214f, 0x43306a, 0x0e0618, 0xf2d6ff, 0x0b0414, 0xd9f7ff, 0x4f2e70, 0x4fe3ff, 0xffd84f,
            0xff4fd8,
        ],
        Digits::Mono,
    ),
    (
        "olive",
        "Mil-Spec",
        [
            0x3f4430, 0x4b513a, 0x5f6648, 0x1c1f12, 0xe4e8c8, 0x14170b, 0xe6f2b8, 0x4d5a20, 0xc8eb5a, 0xf2b84b,
            0xf2624b,
        ],
        Digits::Mono,
    ),
];

pub fn builtins() -> Vec<Skin> {
    BUILTIN
        .iter()
        .map(|(id, name, cols, digits)| Skin {
            id: id.to_string(),
            name: name.to_string(),
            author: "built-in".into(),
            colors: cols.map(rgb),
            digits: *digits,
        })
        .collect()
}

/// Custom skins in `dir` (`*.json`, sorted by file name). Unreadable files are skipped.
pub fn load_custom(dir: &Path) -> Vec<Skin> {
    let Ok(rd) = fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|p| {
            let id = p.file_stem()?.to_string_lossy().into_owned();
            let text = fs::read_to_string(p).ok()?;
            Skin::from_json(&text, &id, &id).ok()
        })
        .collect()
}

/// A file-name-safe slug, e.g. "Amber CRT (mine)" -> "amber-crt-mine".
pub fn slug(name: &str) -> String {
    let mut s = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if !s.ends_with('-') && !s.is_empty() {
            s.push('-');
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { "skin".into() } else { s }
}

/// A free id (file stem) in `dir` based on `name`.
pub fn free_id(dir: &Path, name: &str, taken: &[Skin]) -> String {
    let base = slug(name);
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|s| s.id == id) || dir.join(format!("{id}.json")).exists() {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

pub fn save(dir: &Path, skin: &Skin) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    write_atomic(&dir.join(format!("{}.json", skin.id)), &skin.to_json())
}

/// Optional parts of the widget. Versions up to 0.4.4 always showed them; an update keeps them
/// on, a new install starts without them.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Show {
    /// ◀ ▶ buttons that switch device.
    pub nav: bool,
    /// The connection ("USB receiver", "Receiver · slot 1", …) in the display header.
    pub conn: bool,
    /// Chips under the percentage: voltage or HID++ version, curve source, selector.
    pub details: bool,
    /// The drain-rate line in the display.
    pub drain: bool,
}

impl Show {
    pub const ALL: Show = Show {
        nav: true,
        conn: true,
        details: true,
        drain: true,
    };

    fn parts(&mut self) -> [(&'static str, &mut bool); 4] {
        [
            ("nav", &mut self.nav),
            ("conn", &mut self.conn),
            ("details", &mut self.details),
            ("drain", &mut self.drain),
        ]
    }
}

/// App settings: the chosen skin, the alert level per device, the optional parts shown
/// (None until decided, see `Show`) and the release tag whose update was skipped.
#[derive(Default, Clone, PartialEq, Debug)]
pub struct Settings {
    pub skin: String,
    pub alerts: Vec<(String, u8)>,
    pub show: Option<Show>,
    pub skip: String,
}

impl Settings {
    pub fn load(path: &Path) -> Settings {
        let mut s = Settings::default();
        for line in fs::read_to_string(path).unwrap_or_default().lines() {
            let mut f = line.split_whitespace();
            match (f.next(), f.next(), f.next()) {
                (Some("skin"), Some(id), None) => s.skin = id.to_string(),
                (Some("skip"), Some(tag), None) => s.skip = tag.to_string(),
                (Some("alert"), Some(sel), Some(v)) => {
                    if let Ok(v) = v.parse::<u8>() {
                        s.alerts.push((sel.to_string(), v.min(100)));
                    }
                }
                (Some("show"), Some(part), Some(v @ ("on" | "off"))) => {
                    let show = s.show.get_or_insert_default();
                    if let Some((_, b)) = show.parts().into_iter().find(|(n, _)| *n == part) {
                        *b = v == "on";
                    }
                }
                _ => {}
            }
        }
        s
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut out = String::from("# peripheral-lite app settings\n");
        if !self.skin.is_empty() {
            out.push_str(&format!("skin {}\n", self.skin));
        }
        for (sel, v) in &self.alerts {
            out.push_str(&format!("alert {sel} {v}\n"));
        }
        if !self.skip.is_empty() {
            out.push_str(&format!("skip {}\n", self.skip));
        }
        if let Some(mut show) = self.show {
            for (name, on) in show.parts() {
                out.push_str(&format!("show {name} {}\n", if *on { "on" } else { "off" }));
            }
        }
        write_atomic(path, &out)
    }

    pub fn alert(&self, sel: &str) -> Option<u8> {
        self.alerts.iter().find(|a| a.0 == sel).map(|a| a.1)
    }

    pub fn set_alert(&mut self, sel: &str, v: u8) {
        match self.alerts.iter_mut().find(|a| a.0 == sel) {
            Some(a) => a.1 = v,
            None => self.alerts.push((sel.to_string(), v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_and_errors() {
        let mut s = builtins()[1].clone();
        s.name = "Amber \"CRT\" (mine)".into();
        s.author = "you".into();
        let back = Skin::from_json(&s.to_json(), "x", "x").unwrap();
        assert_eq!(
            (back.name.as_str(), back.colors, back.digits),
            (s.name.as_str(), s.colors, Digits::Pixel)
        );
        assert!(
            Skin::from_json("{\"name\": \"a\"}", "x", "x")
                .unwrap_err()
                .contains("frame")
        );
        assert!(Skin::from_json("[1]", "x", "x").is_err());
        // an extra number value is skipped; missing name falls back to the file name
        let mut j = s
            .to_json()
            .replace("\"name\": \"Amber \\\"CRT\\\" (mine)\",", "\"version\": 1,");
        j = j.replace("\"author\": \"you\"", "\"author\": \"built-in\"");
        let b = Skin::from_json(&j, "x", "from-file").unwrap();
        assert_eq!((b.name.as_str(), b.author.as_str()), ("from-file", "imported"));
    }

    #[test]
    fn slugs_settings_and_levels() {
        assert_eq!(slug("Amber CRT (mine)"), "amber-crt-mine");
        assert_eq!(slug("!!"), "skin");
        let mut st = Settings {
            skin: "amber".into(),
            ..Default::default()
        };
        st.set_alert("c54d:1", 20);
        st.set_alert("c54d:1", 25);
        let p = std::env::temp_dir().join(format!("peripheral-lite-app-{}.txt", std::process::id()));
        st.save(&p).unwrap();
        assert_eq!(Settings::load(&p), st);
        st.skip = "v0.5.0".into();
        st.show = Some(Show {
            nav: true,
            drain: true,
            ..Default::default()
        });
        st.save(&p).unwrap();
        assert_eq!(Settings::load(&p), st);
        std::fs::remove_file(&p).unwrap();
        let k = &builtins()[0];
        assert_eq!(k.level(15, 15), k.c(BAD));
        assert_eq!(k.level(35, 15), k.c(WARN));
        assert_eq!(k.level(36, 15), k.c(OK));
    }
}
