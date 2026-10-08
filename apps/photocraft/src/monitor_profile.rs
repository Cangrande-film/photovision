//! The display's ICC profile, for the colour-managed canvas (Edit › Color Settings ›
//! Monitor Profile = `auto`).
//!
//! macOS: `NSScreen.mainScreen.colorSpace.ICCProfileData` (the screen holding the key window), a
//! documented AppKit API, read through `osascript` (AppleScriptObjC) so the app needs no `unsafe`
//! FFI. About 0.4 s.
//!
//! Windows: the profile associated with the monitor under a screen point. `powershell` runs
//! `monitor_profile.ps1` (Add-Type P/Invoke, again so we need no `unsafe`): `MonitorFromPoint` →
//! `GetMonitorInfoW` → `EnumDisplayDevicesW` (device interface name) → `mscms`
//! `WcsGetDefaultColorProfile` (per-user then system scope, per `WcsGetUsePerUserProfiles`),
//! falling back to `GetICMProfileW` on the display's DC. It prints the profile's path, which we
//! read here. About 0.7 s, killed after [`TIMEOUT`].
//!
//! Queries run on a background thread, at launch and whenever the window settles on a new spot
//! (`photocraft_ui_egui::monitor_follow`). Any failure gives `None`: sRGB, or the profile chosen
//! in Color Settings. Other platforms always return `None`.

use std::sync::mpsc::Receiver;

/// How long a platform query may take before it is abandoned.
#[cfg_attr(not(windows), allow(dead_code))]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Larger "profiles" are not monitor profiles (real ones are a few KB to a few hundred KB).
#[cfg_attr(not(windows), allow(dead_code))]
const MAX_PROFILE_BYTES: u64 = 16 << 20;

/// Starts reading the profile of the display under `point` (physical pixels; `None`: the
/// main display) in the background.
pub fn detect_async(point: Option<[i32; 2]>) -> Receiver<Option<Vec<u8>>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("monitor-profile".into()).spawn(move || {
        let _ = tx.send(detect(point));
    });
    if let Err(e) = spawned {
        log::warn!("monitor profile: can't start the query: {e}");
    }
    rx
}

/// The `Services::detect_monitor_profile` hook: where the platform can tell displays apart.
pub fn follow_hook() -> Option<photocraft_ui_egui::monitor_follow::DetectMonitorFn> {
    if cfg!(any(windows, target_os = "macos")) { Some(Box::new(|p: [f32; 2]| detect_async(Some([p[0].round() as i32, p[1].round() as i32])))) } else { None }
}

#[cfg(target_os = "macos")]
fn detect(_point: Option<[i32; 2]>) -> Option<Vec<u8>> {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "use framework \"AppKit\"",
            "-e",
            "return ((current application's NSScreen's mainScreen()'s colorSpace()'s ICCProfileData()'s base64EncodedStringWithOptions:0) as text)",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    validate(base64_decode(std::str::from_utf8(&out.stdout).ok()?.trim())?)
}

#[cfg(windows)]
fn detect(point: Option<[i32; 2]>) -> Option<Vec<u8>> {
    // The primary display's top-left corner is the origin.
    let [x, y] = point.unwrap_or([0, 0]);
    let out = match run_powershell(&format!("$x = {x}; $y = {y}\n{}", include_str!("monitor_profile.ps1"))) {
        Ok(out) => out,
        Err(e) => {
            log::warn!("monitor profile: {e}");
            return None;
        }
    };
    let Some(path) = parse_profile_path(&out) else {
        log::info!("monitor profile: none reported for ({x}, {y})");
        return None;
    };
    let bytes = match read_capped(std::path::Path::new(&path)) {
        Ok(b) => b,
        Err(e) => {
            log::warn!("monitor profile `{path}`: {e}");
            return None;
        }
    };
    let valid = validate(bytes);
    if valid.is_some() {
        log::info!("monitor profile at ({x}, {y}): {path}");
    } else {
        log::warn!("monitor profile `{path}` is not an RGB ICC profile");
    }
    valid
}

#[cfg(not(any(target_os = "macos", windows)))]
fn detect(_point: Option<[i32; 2]>) -> Option<Vec<u8>> {
    None
}

/// Runs a PowerShell script without a console window; its stdout, or an error on failure or
/// after [`TIMEOUT`].
#[cfg(windows)]
fn run_powershell(script: &str) -> Result<String, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    // The system's PowerShell, not whatever `powershell` is first on PATH.
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    let exe = std::path::Path::new(&root).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
    let mut child = Command::new(exe)
        .args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_command(script)])
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("can't run PowerShell: {e}"))?;
    // Read on another thread: the compiler Add-Type starts may hold the pipe open after a kill.
    let (tx, rx) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stdout.read_to_string(&mut s);
            let _ = tx.send(s);
        });
    }
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(status)) => return Err(format!("PowerShell failed ({status})")),
            Ok(None) if start.elapsed() >= TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("PowerShell took over {} s; stopped", TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
            Err(e) => return Err(format!("PowerShell: {e}")),
        }
    }
    rx.recv_timeout(std::time::Duration::from_secs(1)).map_err(|_| "PowerShell printed nothing".to_string())
}

/// The profile path from the script's output (the `PROFILE=` line).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_profile_path(out: &str) -> Option<String> {
    out.lines().rev().find_map(|l| l.trim().strip_prefix("PROFILE=")).map(str::trim).filter(|p| !p.is_empty()).map(str::to_string)
}

/// A file's bytes, refusing ones too large to be a monitor profile.
#[cfg_attr(not(windows), allow(dead_code))]
fn read_capped(path: &std::path::Path) -> Result<Vec<u8>, String> {
    let len = std::fs::metadata(path).map_err(|e| e.to_string())?.len();
    if len > MAX_PROFILE_BYTES {
        return Err(format!("{len} bytes is too large for a profile"));
    }
    std::fs::read(path).map_err(|e| e.to_string())
}

/// `bytes` when they are an RGB ICC profile we can use.
#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
fn validate(bytes: Vec<u8>) -> Option<Vec<u8>> {
    // An ICC profile starts with its size and carries `acsp` at offset 36.
    if bytes.len() < 132 || bytes.get(36..40) != Some(b"acsp") {
        return None;
    }
    let p = photocraft_cms::Profile::parse(&bytes).ok()?;
    (p.color_space == photocraft_cms::ColorSpace::Rgb).then_some(bytes)
}

/// `powershell -EncodedCommand` text: base64 of the script in UTF-16LE (no quoting to get wrong).
#[cfg_attr(not(windows), allow(dead_code))]
fn encode_command(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64_encode(&bytes)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 (RFC 4648, with padding).
#[cfg_attr(not(windows), allow(dead_code))]
fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk.first().copied().unwrap_or(0), chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let acc = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(B64[((acc >> (18 - 6 * i)) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Standard base64 (RFC 4648, with padding) → bytes; `None` on any invalid character.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> { B64.iter().position(|&b| b == c).map(|i| i as u32) };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGVsbG8h").as_deref(), Some(&b"hello!"[..]));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a$==").is_none());
        assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
        assert_eq!(base64_encode(b"hello!"), "aGVsbG8h");
        assert_eq!(base64_encode(b"hi"), "aGk=");
        assert_eq!(base64_encode(b""), "");
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(base64_decode(&base64_encode(&all)), Some(all));
        // `powershell -EncodedCommand` wants UTF-16LE.
        assert_eq!(encode_command("$x"), base64_encode(&[b'$', 0, b'x', 0]));
    }

    #[test]
    fn profile_path_from_script_output() {
        let out = "\r\nPROFILE=C:\\Windows\\system32\\spool\\drivers\\color\\sRGB Color Space Profile.icm\r\n";
        assert_eq!(parse_profile_path(out).as_deref(), Some("C:\\Windows\\system32\\spool\\drivers\\color\\sRGB Color Space Profile.icm"));
        assert_eq!(parse_profile_path("noise\nPROFILE=a.icc\nPROFILE=b.icm\n").as_deref(), Some("b.icm"), "the last answer wins");
        assert_eq!(parse_profile_path(""), None);
        assert_eq!(parse_profile_path("PROFILE=   \n"), None);
        assert_eq!(parse_profile_path("Add-Type : error\nexit"), None);
    }

    #[test]
    fn validate_rejects_non_profiles() {
        assert_eq!(validate(Vec::new()), None);
        assert_eq!(validate(vec![0; 200]), None);
        let mut fake = vec![0u8; 200];
        fake[36..40].copy_from_slice(b"acsp");
        assert_eq!(validate(fake), None, "a header with no tags doesn't parse");
        let srgb = photocraft_cms::Builtin::Srgb.profile().to_bytes().to_vec();
        assert_eq!(validate(srgb.clone()), Some(srgb));
    }

    #[test]
    fn read_capped_reports_missing_files() {
        assert!(read_capped(std::path::Path::new("definitely/not/here.icc")).is_err());
    }

    /// The detected profile (when there is one) parses as an RGB profile. On Windows this runs
    /// the real PowerShell query.
    #[test]
    fn detected_profile_parses() {
        if let Some(bytes) = detect(None) {
            let p = photocraft_engine::color_cmds::profile_from_bytes(&std::sync::Arc::new(bytes)).expect("parses");
            assert_eq!(format!("{:?}", p.color_space), "Rgb", "{}", p.description);
            eprintln!("detected monitor profile: {} ({} bytes)", p.description, p.to_bytes().len());
        }
    }
}
