//! Graphics acceleration preference for the map (desktop only).
//!
//! The web view's GPU settings can only be chosen before it starts, so the
//! choice lives in a one-word file (`graphics.txt`) in the app's data folder
//! that `main()` reads before anything else, and changing it needs a restart.
//!
//! * `auto`     – the app's defaults (see `apply_env`).
//! * `gpu`      – prefer the GPU, even where the web view would normally fall
//!                back to software because of a driver blocklist.
//! * `software` – no GPU: the web view draws on the CPU. For broken drivers.
//!
//! Only Linux (WebKitGTK) and Windows (WebView2) can be steered this way;
//! macOS and the mobile systems always use the system's GPU path.

use std::path::{Path, PathBuf};

/// Must match `identifier` in tauri.conf.json: Tauri's data folder is named
/// after it, and `setup()` checks the two agree.
const IDENTIFIER: &str = "app.vrcfriendmap";
const FILE: &str = "graphics.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto,
    Gpu,
    Software,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s.trim() {
            "auto" => Some(Mode::Auto),
            "gpu" => Some(Mode::Gpu),
            "software" => Some(Mode::Software),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::Gpu => "gpu",
            Mode::Software => "software",
        }
    }
}

/// Can the web view on this OS be told to use or avoid the GPU?
pub fn supported() -> bool {
    cfg!(any(target_os = "linux", target_os = "windows"))
}

/// The app data folder, found without Tauri (it isn't running yet).
/// Same folder Tauri's `app_data_dir()` gives on Linux and Windows.
pub fn early_data_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share")));
    base.map(|b| b.join(IDENTIFIER))
}

pub fn read(dir: &Path) -> Mode {
    std::fs::read_to_string(dir.join(FILE)).ok().and_then(|s| Mode::parse(&s)).unwrap_or(Mode::Auto)
}

pub fn write(dir: &Path, mode: Mode) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(dir.join(FILE), mode.as_str()).map_err(|e| e.to_string())
}

/// What `main()` calls before the web view exists.
pub fn apply_early() {
    let mode = early_data_dir().map(|d| read(&d)).unwrap_or(Mode::Auto);
    apply_env(mode);
}

/// Pure planning step: the environment changes for a mode, as
/// `(name, Some(value))` to set and `(name, None)` to remove.
/// `get` reads the current environment.
pub fn env_plan(mode: Mode, get: &dyn Fn(&str) -> Option<String>) -> Vec<(&'static str, Option<String>)> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
        // WebKitGTK's DMA-BUF renderer made the map's canvas several times
        // slower (17-22 fps vs 66-73 fps for the same drawing) and is known to
        // misbehave with NVIDIA drivers; the older path is still
        // GPU-composited. VRC_FRIEND_MAP_DMABUF=1 keeps WebKit's default.
        let keep_dmabuf = get("VRC_FRIEND_MAP_DMABUF").is_some_and(|v| v != "0");
        match mode {
            Mode::Auto => {
                // Anything the user set themselves is left alone.
                if !keep_dmabuf && get("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
                    out.push(("WEBKIT_DISABLE_DMABUF_RENDERER", Some("1".into())));
                }
            }
            Mode::Gpu => {
                if !keep_dmabuf {
                    out.push(("WEBKIT_DISABLE_DMABUF_RENDERER", Some("1".into())));
                }
                // An explicit choice in the app beats a leftover variable.
                out.push(("WEBKIT_DISABLE_COMPOSITING_MODE", None));
                out.push(("LIBGL_ALWAYS_SOFTWARE", None));
            }
            Mode::Software => {
                out.push(("WEBKIT_DISABLE_COMPOSITING_MODE", Some("1".into())));
                out.push(("LIBGL_ALWAYS_SOFTWARE", Some("1".into())));
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        // WebView2 is Chromium: these are Chromium switches. Added to anything
        // the user already put in this variable.
        let extra = match mode {
            Mode::Auto => "",
            Mode::Gpu => "--ignore-gpu-blocklist --enable-gpu-rasterization --enable-zero-copy",
            Mode::Software => "--disable-gpu",
        };
        if !extra.is_empty() {
            let name = "WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS";
            let merged = match get(name) {
                Some(old) if !old.trim().is_empty() => format!("{} {extra}", old.trim()),
                _ => extra.to_string(),
            };
            out.push((name, Some(merged)));
        }
    }
    let _ = (mode, get);
    out
}

fn apply_env(mode: Mode) {
    let get = |k: &str| std::env::var(k).ok();
    for (name, value) in env_plan(mode, &get) {
        // Single-threaded here: nothing else reads the environment yet.
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_roundtrip() {
        for m in [Mode::Auto, Mode::Gpu, Mode::Software] {
            assert_eq!(Mode::parse(m.as_str()), Some(m));
        }
        assert_eq!(Mode::parse(" gpu\n"), Some(Mode::Gpu));
        assert_eq!(Mode::parse("turbo"), None);
    }

    #[test]
    fn file_roundtrip_and_default() {
        let dir = std::env::temp_dir().join(format!("vfm-gfx-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(read(&dir), Mode::Auto);
        write(&dir, Mode::Software).unwrap();
        assert_eq!(read(&dir), Mode::Software);
        std::fs::write(dir.join(FILE), "garbage").unwrap();
        assert_eq!(read(&dir), Mode::Auto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_plans() {
        let none = |_: &str| None;
        let auto = env_plan(Mode::Auto, &none);
        assert_eq!(auto, vec![("WEBKIT_DISABLE_DMABUF_RENDERER", Some("1".to_string()))]);
        let user = |k: &str| (k == "WEBKIT_DISABLE_DMABUF_RENDERER").then(|| "0".to_string());
        assert!(env_plan(Mode::Auto, &user).is_empty());
        let sw = env_plan(Mode::Software, &none);
        assert!(sw.contains(&("WEBKIT_DISABLE_COMPOSITING_MODE", Some("1".to_string()))));
        let gpu = env_plan(Mode::Gpu, &none);
        assert!(gpu.contains(&("WEBKIT_DISABLE_COMPOSITING_MODE", None)));
    }
}
