//! `~/.config/herdr-core/config.toml`. A missing file means defaults.

use crate::rpc::RpcError;
use serde::Deserialize;
use std::path::Path;

#[derive(Deserialize, Default)]
pub struct Config {
    /// Executable and arguments for new shells (default: this machine's shell).
    pub shell: Option<Vec<String>>,
    /// The chord that detaches `hc attach`: `ctrl+<key>`, default `ctrl+\`.
    pub detach_key: Option<String>,
    /// Seconds a `wait` runs before it checks in (default 180).
    pub wait_timeout: Option<f64>,
    /// Notes to parent agents and `<hc>` tags on messages between agents, on
    /// or off (default on). Off: hc types nothing on its own.
    pub notes: Option<bool>,
    /// Seconds without a person typing before a note goes in (default 3).
    pub notes_quiet: Option<f64>,
    /// Size of a terminal nobody is typing in, `COLSxROWS` (default 200x120).
    pub headless: Option<String>,
    /// Scrollback kept per live terminal, in MB (default 5).
    pub history_mb: Option<f64>,
    /// Where the core listens (default under $XDG_RUNTIME_DIR). `HC_SOCKET` wins.
    pub socket: Option<String>,
    /// Seconds another machine gets to answer a call (a wait gets its own
    /// timeout on top), and to send anything at all before its line is
    /// given up and dialed again (default 30).
    pub machine_timeout: Option<f64>,
    /// Output kept for a viewer that stops reading before it gets a repaint
    /// instead, in MB (default 4).
    pub viewer_queue_mb: Option<f64>,
    /// Kitty images: what each terminal keeps, and how viewers get them.
    #[serde(default)]
    pub images: Images,
    /// Lines to other machines.
    #[serde(default)]
    pub link: Link,
}

/// The zstd level a line to another machine uses unless `link.level` says.
pub const LINK_LEVEL: i32 = 3;

#[derive(Deserialize, Default)]
pub struct Link {
    /// "zstd" (default): what goes either way on a line to another machine is
    /// compressed, when the core there can. "none": sent as it is.
    pub compression: Option<String>,
    /// zstd level, 1 (fastest) to 19 (smallest); default 3.
    pub level: Option<i32>,
}

#[derive(Deserialize, Default)]
pub struct Images {
    /// Image memory each screen keeps, in MB (default 320, as Ghostty).
    pub storage_mb: Option<f64>,
    /// "display" (default): a repaint sends each image no larger than it is
    /// drawn, in the viewer's pixels. "original": always full resolution.
    pub scale: Option<String>,
    /// PNG compression of images sent to viewers: "fastest", "fast"
    /// (default), "balanced" or "high".
    pub png: Option<String>,
}

impl Config {
    pub fn shell(&self) -> Result<Vec<String>, RpcError> {
        match &self.shell {
            None => Ok(crate::platform::own_shell()),
            Some(argv) if argv.first().is_some_and(|exe| !exe.is_empty()) => Ok(argv.clone()),
            Some(_) => Err(invalid("shell: give an executable and optional arguments, like [\"nu\"]".into())),
        }
    }

    pub fn headless(&self) -> Result<(u16, u16), RpcError> {
        let Some(spec) = &self.headless else { return Ok((200, 120)) };
        spec.split_once('x')
            .and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)))
            .filter(|(c, r): &(u16, u16)| *c > 0 && *r > 0)
            .ok_or_else(|| invalid(format!("headless {spec:?}: use COLSxROWS, like \"200x120\"")))
    }

    pub fn machine_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(self.machine_timeout.filter(|s| s.is_finite() && *s > 0.0).unwrap_or(30.0))
    }

    pub fn history_bytes(&self) -> usize {
        (self.history_mb.unwrap_or(5.0).max(0.0) * 1024.0 * 1024.0) as usize
    }

    pub fn viewer_queue_bytes(&self) -> usize {
        let mb = self.viewer_queue_mb.filter(|mb| mb.is_finite() && *mb > 0.0).unwrap_or(4.0);
        (mb * 1024.0 * 1024.0) as usize
    }

    pub fn images(&self) -> Result<ghostty_vt::ImageSettings, RpcError> {
        let defaults = ghostty_vt::ImageSettings::default();
        let i = &self.images;
        let storage_bytes = match i.storage_mb {
            None => defaults.storage_bytes,
            Some(mb) if mb.is_finite() && mb > 0.0 => (mb * 1024.0 * 1024.0) as u64,
            Some(mb) => return Err(invalid(format!("images.storage_mb {mb}: give a size above 0, like 320"))),
        };
        let scale_to_display = match i.scale.as_deref() {
            None | Some("display") => true,
            Some("original") => false,
            Some(other) => return Err(invalid(format!("images.scale {other:?}: use \"display\" or \"original\""))),
        };
        let png = match i.png.as_deref() {
            None | Some("fast") => ghostty_vt::PngLevel::Fast,
            Some("fastest") => ghostty_vt::PngLevel::Fastest,
            Some("balanced") => ghostty_vt::PngLevel::Balanced,
            Some("high") => ghostty_vt::PngLevel::High,
            Some(other) => {
                return Err(invalid(format!(
                    "images.png {other:?}: use \"fastest\", \"fast\", \"balanced\" or \"high\""
                )));
            }
        };
        Ok(ghostty_vt::ImageSettings { storage_bytes, scale_to_display, png })
    }

    /// The zstd level lines to other machines use, or `None` for none.
    pub fn link(&self) -> Result<Option<i32>, RpcError> {
        match (self.link.compression.as_deref(), self.link.level) {
            (Some("none"), _) => Ok(None),
            (None | Some("zstd"), None) => Ok(Some(LINK_LEVEL)),
            (None | Some("zstd"), Some(level @ 1..=19)) => Ok(Some(level)),
            (None | Some("zstd"), Some(level)) => Err(invalid(format!("link.level {level}: use 1 to 19"))),
            (Some(other), _) => Err(invalid(format!("link.compression {other:?}: use \"zstd\" or \"none\""))),
        }
    }
}

fn invalid(message: String) -> RpcError {
    RpcError::new("config", message, "fix ~/.config/herdr-core/config.toml")
}

pub fn load(path: &Path) -> Result<Config, RpcError> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).map_err(|e| invalid(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(invalid(format!("{}: {e}", path.display()))),
    }
}

/// A detach chord: the control byte it sends, and its key code for the
/// kitty and modifyOtherKeys encodings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetachKey {
    pub byte: u8,
    pub code: u32,
}

impl Config {
    pub fn detach_key(&self) -> Result<DetachKey, RpcError> {
        let spec = self.detach_key.as_deref().unwrap_or("ctrl+\\");
        let key = spec.strip_prefix("ctrl+").and_then(|k| {
            let mut chars = k.chars();
            let c = chars.next()?.to_ascii_lowercase();
            (chars.next().is_none() && (c.is_ascii_lowercase() || "@[\\]^_".contains(c))).then_some(c)
        });
        let c = key.ok_or_else(|| {
            invalid(format!("detach_key {spec:?}: use a ctrl chord like \"ctrl+\\\\\" or \"ctrl+]\""))
        })?;
        Ok(DetachKey { byte: c as u8 & 0x1f, code: c as u32 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(spec: &str) -> Result<DetachKey, RpcError> {
        Config { detach_key: Some(spec.into()), ..Default::default() }.detach_key()
    }

    #[test]
    fn detach_keys_are_ctrl_chords_only() {
        assert_eq!(Config::default().detach_key().unwrap(), DetachKey { byte: 0x1c, code: 92 });
        assert_eq!(key("ctrl+]").unwrap(), DetachKey { byte: 0x1d, code: 93 });
        assert_eq!(key("ctrl+A").unwrap(), DetachKey { byte: 0x01, code: 97 });
        for bad in ["alt+x", "ctrl+", "ctrl+ab", "ctrl+1", "x"] {
            assert_eq!(key(bad).unwrap_err().kind, "config", "{bad}");
        }
    }
}
