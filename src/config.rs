//! Settings. `config.json` is created with defaults on first run so there is
//! always a file to edit; anything missing or malformed falls back to the
//! default rather than stopping the terminal from starting.
use crate::paths;

#[derive(Clone, Debug)]
pub struct Config {
    pub font_pt: f32,
    /// Text unfolds as it arrives.
    pub reveal: bool,
    /// Strength of the light arriving text casts: 0 off, 1 default.
    pub light: f32,
    /// Sounds for keys, arriving text, the bell and a session ending.
    pub sound: bool,
    /// Loudness of those sounds, 0 to 1 (the system volume still applies).
    pub volume: f32,
    /// Self-update: "auto" (apply signed releases when found; they take
    /// effect at the next launch), "check" (only say so in the log), "off".
    pub update: String,
    /// Program to start; empty means PowerShell 7, Windows PowerShell, then cmd.
    pub shell: String,
    pub shell_args: Vec<String>,
    /// Journals older than this, or beyond the size cap (oldest first), are deleted.
    pub journal_days: u64,
    pub journal_max_mb: u64,
    /// Ctrl+click on a file link opens it with this command, {path}, {line}
    /// and {col} filled in (e.g. `code --goto {path}:{line}:{col}`); empty
    /// means the system's own choice for the file, at its start.
    pub editor: String,
}

impl Default for Config {
    fn default() -> Self {
        Self { font_pt: 15.0, reveal: true, light: 1.0, sound: true, volume: 0.5, update: "auto".into(), shell: String::new(), shell_args: Vec::new(), journal_days: 14, journal_max_mb: 2048, editor: String::new() }
    }
}

impl Config {
    pub fn parse(bytes: &[u8]) -> Self {
        let d = Self::default();
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else { return d };
        Self {
            font_pt: v["font_pt"].as_f64().map(|x| x as f32).filter(|x| x.is_finite()).unwrap_or(d.font_pt).clamp(6.0, 72.0),
            reveal: v["reveal"].as_bool().unwrap_or(d.reveal),
            light: v["light"].as_f64().map(|x| x as f32).filter(|x| x.is_finite()).unwrap_or(d.light).clamp(0.0, 3.0),
            sound: v["sound"].as_bool().unwrap_or(d.sound),
            update: match v["update"].as_str() {
                Some(m @ ("auto" | "check" | "off")) => m.to_string(),
                _ => d.update.clone(),
            },
            volume: v["volume"].as_f64().map(|x| x as f32).filter(|x| x.is_finite()).unwrap_or(d.volume).clamp(0.0, 1.0),
            shell: v["shell"].as_str().unwrap_or("").trim().to_string(),
            shell_args: v["shell_args"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            journal_days: v["journal_days"].as_u64().unwrap_or(d.journal_days).clamp(1, 3650),
            journal_max_mb: v["journal_max_mb"].as_u64().unwrap_or(d.journal_max_mb).clamp(16, 1 << 20),
            editor: v["editor"].as_str().unwrap_or("").trim().to_string(),
        }
    }

    fn to_json(&self) -> String {
        let v = serde_json::json!({
            "_about": "PROTEUS settings. Edit and open a new window. Delete this file to restore defaults.",
            "font_pt": self.font_pt,
            "reveal": self.reveal,
            "light": self.light,
            "sound": self.sound,
            "update": self.update,
            "volume": self.volume,
            "shell": self.shell,
            "shell_args": self.shell_args,
            "journal_days": self.journal_days,
            "journal_max_mb": self.journal_max_mb,
            "editor": self.editor,
        });
        serde_json::to_string_pretty(&v).unwrap_or_default()
    }

    pub fn load() -> Self {
        let path = paths::data_dir().join("config.json");
        match std::fs::read(&path) {
            Ok(b) => Self::parse(&b),
            Err(_) => {
                let c = Self::default();
                let _ = std::fs::write(&path, c.to_json());
                c
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_input_never_breaks_startup() {
        let d = Config::default();
        assert_eq!(Config::parse(b"not json").font_pt, d.font_pt);
        let c = Config::parse(br#"{"font_pt": 900, "light": -4, "reveal": "yes", "shell": "  cmd.exe ", "journal_days": 0}"#);
        assert_eq!(c.font_pt, 72.0);
        assert_eq!(c.light, 0.0);
        assert!(c.reveal);
        assert_eq!(c.shell, "cmd.exe");
        assert_eq!(c.journal_days, 1);
        // A settings file written before sound existed still loads, with sound on.
        assert!(c.sound && c.volume == d.volume);
        let q = Config::parse(br#"{"sound": false, "volume": 7}"#);
        assert!(!q.sound && q.volume == 1.0);
        // What we write is what we read.
        let round = Config::parse(d.to_json().as_bytes());
        assert_eq!((round.font_pt, round.light, round.journal_max_mb), (d.font_pt, d.light, d.journal_max_mb));
    }
}
