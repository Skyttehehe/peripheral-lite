//! Updates: at start the app asks GitHub for the newest release (once, not on a timer) and
//! offers a newer one in a bar. On Windows, "Update" downloads the release zip, checks it against its
//! `.sha256`, swaps the exes next to this one (a running exe can be renamed, not overwritten:
//! the old ones become `*.old`, removed at the next start) and the app restarts.
//! Elsewhere it opens the release page. HTTPS goes through the system's `curl` (shipped with
//! Windows 10 1803+, macOS and most Linux systems), so the app needs no TLS stack of its own.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// https://github.com/<owner>/<repo>
const REPO_URL: &str = env!("CARGO_PKG_REPOSITORY");
/// Whether `install` can replace this copy (else "Update" opens the release page).
pub const CAN_INSTALL: bool = cfg!(windows);
/// The files an update replaces, as packaged by packaging/package-windows.ps1.
#[cfg(windows)]
const EXES: [&str; 3] = [
    "peripheral-lite-gui.exe",
    "peripheral-lite.exe",
    "peripheral-lite-bg.exe",
];

#[derive(Clone, Debug)]
pub struct Release {
    pub tag: String,
}

impl Release {
    pub fn page(&self) -> String {
        format!("{REPO_URL}/releases/tag/{}", self.tag)
    }
}

/// `v1.2.3` or `1.2.3`; anything else (e.g. `v1.2.3-rc1`) is None.
fn version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim();
    let mut it = s.strip_prefix('v').unwrap_or(s).split('.').map(|p| p.parse().ok());
    let v = (it.next()??, it.next()??, it.next()??);
    it.next().is_none().then_some(v)
}

/// The highest `"tag_name"` in a GitHub releases listing (all releases so far are marked
/// pre-release, which `/releases/latest` would skip).
fn newest_tag(json: &str) -> Option<String> {
    json.split("\"tag_name\"")
        .skip(1)
        .filter_map(|rest| {
            let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
            let tag = &rest[..rest.find('"')?];
            Some((version(tag)?, tag.to_string()))
        })
        .max()
        .map(|(_, t)| t)
}

/// The newest release if it is newer than this build.
pub fn check() -> Result<Option<Release>, String> {
    let repo = REPO_URL.trim_start_matches("https://github.com/");
    let body = run(curl().arg(format!("https://api.github.com/repos/{repo}/releases?per_page=30")))?;
    let tag = newest_tag(&String::from_utf8_lossy(&body)).ok_or("no releases found on GitHub")?;
    Ok((version(&tag) > version(CURRENT)).then_some(Release { tag }))
}

/// Download `r`, verify it and put its exes in place of those in `dir` (normally
/// `install_dir()`). The caller restarts the app.
#[cfg(windows)]
pub fn install(r: &Release, dir: &Path) -> Result<(), String> {
    let name = format!("peripheral-lite-{}-windows-x64", r.tag);
    let url = format!("{REPO_URL}/releases/download/{}/{name}.zip", r.tag);
    let work = work_dir();
    let _ = std::fs::remove_dir_all(&work);
    let out = work.join("files");
    std::fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let zip = work.join(format!("{name}.zip"));

    run(curl().arg("--output").arg(&zip).arg(&url))?;
    let sum = run(curl().arg(format!("{url}.sha256")))?;
    let want = String::from_utf8_lossy(&sum)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let data = std::fs::read(&zip).map_err(|e| format!("{}: {e}", zip.display()))?;
    if want.len() != 64 || sha256_hex(&data) != want {
        return Err("the download is damaged (checksum mismatch); nothing was changed".into());
    }
    run(system_tool("tar").arg("-xf").arg(&zip).arg("-C").arg(&out))?;
    if !out.join(EXES[0]).is_file() {
        return Err(format!("{} is not in the release zip; nothing was changed", EXES[0]));
    }

    // Swap one file at a time; on a failure put back what was already swapped.
    let mut done: Vec<&str> = Vec::new();
    let undo = |done: &[&str]| {
        for f in done {
            let _ = std::fs::remove_file(dir.join(f));
            let _ = std::fs::rename(dir.join(format!("{f}.old")), dir.join(f));
        }
    };
    for f in EXES {
        let (new, cur, old) = (out.join(f), dir.join(f), dir.join(format!("{f}.old")));
        if !new.is_file() {
            continue;
        }
        let _ = std::fs::remove_file(&old);
        let had = cur.exists();
        let r = (if had { std::fs::rename(&cur, &old) } else { Ok(()) }).and_then(|_| std::fs::copy(&new, &cur));
        if let Err(e) = r {
            if had && !cur.exists() {
                let _ = std::fs::rename(&old, &cur);
            }
            undo(&done);
            return Err(format!("cannot replace {}: {e}; nothing was changed", cur.display()));
        }
        done.push(f);
    }
    let _ = std::fs::remove_dir_all(&work);
    Ok(())
}

#[cfg(not(windows))]
pub fn install(_: &Release, _: &Path) -> Result<(), String> {
    Err("updating in place is only supported on Windows".into())
}

/// Remove what the last update left behind: the old exes (in use until the old copy exited)
/// and the download folder.
pub fn clean_up() {
    #[cfg(windows)]
    if let Ok(dir) = install_dir() {
        for f in EXES {
            let _ = std::fs::remove_file(dir.join(format!("{f}.old")));
        }
    }
    let _ = std::fs::remove_dir_all(work_dir());
}

/// The folder of this exe.
pub fn install_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "cannot find the program folder".to_string())
}

/// Open the release page in the browser (the file manager's opener also takes URLs).
pub fn open_page(r: &Release) {
    peripheral_lite::platform::open_folder(Path::new(&r.page()));
}

fn work_dir() -> PathBuf {
    std::env::temp_dir().join("peripheral-lite-update")
}

/// A program from System32 on Windows (not whatever `curl.exe` is first in PATH), without a
/// console window; from PATH elsewhere.
fn system_tool(name: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let root = std::env::var_os("SystemRoot").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        let mut c = Command::new(root.join("System32").join(format!("{name}.exe")));
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }
    #[cfg(not(windows))]
    Command::new(name)
}

fn curl() -> Command {
    let mut c = system_tool("curl");
    c.args(["--silent", "--show-error", "--fail", "--location", "--max-time", "300"])
        .args([
            "--user-agent",
            concat!("peripheral-lite-gui/", env!("CARGO_PKG_VERSION")),
        ]);
    c
}

/// Run to the end; stdout on success, else the tool's own error text.
fn run(c: &mut Command) -> Result<Vec<u8>, String> {
    let prog = PathBuf::from(c.get_program());
    let prog = prog.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let out = c
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {prog}: {e}"))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim().trim_start_matches("curl: ");
    Err(if err.is_empty() {
        format!("{prog} failed ({})", out.status)
    } else {
        err.to_string()
    })
}

/// SHA-256 (FIPS 180-4) as lowercase hex.
#[cfg(any(windows, test))]
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
        0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
        0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
        0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
        0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
        0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
        0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (i, b) in block.as_chunks::<4>().0.iter().enumerate() {
            w[i] = u32::from_be_bytes(*b);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            (hh, g, f, e, d, c, b, a) = (g, f, e, d.wrapping_add(t1), c, b, a, t1.wrapping_add(t2));
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(version("v0.4.2"), Some((0, 4, 2)));
        assert_eq!(version("1.10.0"), Some((1, 10, 0)));
        assert_eq!(version("v0.4"), None);
        assert_eq!(version("v0.4.2-rc1"), None);
        assert!(version("v0.10.0") > version("v0.9.9"));
        assert!(version(CURRENT).is_some());
    }

    #[test]
    fn newest_tag_is_the_highest_version() {
        let json = r#"[{"tag_name": "v0.4.2","x":1},{"tag_name":"v0.10.0"},{"tag_name": "v0.9.1"},
            {"tag_name": "nightly"}]"#;
        assert_eq!(newest_tag(json).as_deref(), Some("v0.10.0"));
        assert_eq!(newest_tag("[]"), None);
    }

    #[test]
    fn sha256_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let long = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(
            sha256_hex(long),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    /// Talks to GitHub: `cargo test -- --ignored`. Needs a release published under the
    /// peripheral-lite name (v0.4.2 and v0.4.3 only have lghub-lite assets): change the tag below.
    #[test]
    #[ignore]
    #[cfg(windows)]
    fn check_and_install_from_github() {
        assert!(check().is_ok());
        let dir = std::env::temp_dir().join("peripheral-lite-update-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(EXES[0]), b"old").unwrap();
        let r = Release { tag: "v0.4.2".into() };
        install(&r, &dir).unwrap();
        for f in EXES {
            assert!(std::fs::metadata(dir.join(f)).unwrap().len() > 100_000, "{f}");
        }
        assert_eq!(std::fs::read(dir.join(format!("{}.old", EXES[0]))).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
