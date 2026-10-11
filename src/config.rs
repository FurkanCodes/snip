use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShotAction {
    Clipboard,
    Save,
    Both,
}

impl ShotAction {
    pub fn copies(self) -> bool {
        matches!(self, ShotAction::Clipboard | ShotAction::Both)
    }
    pub fn saves(self) -> bool {
        matches!(self, ShotAction::Save | ShotAction::Both)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Hotkey strings use the `global-hotkey` format, e.g. `super+shift+Digit2`.
    pub screenshot_hotkey: String,
    pub record_hotkey: String,
    pub screenshot_action: ShotAction,
    pub screenshot_dir: PathBuf,
    pub video_dir: PathBuf,
    pub video_copy_to_clipboard: bool,
    pub video_show_clicks: bool,
    pub video_record_audio: bool,
    pub reveal_after_save: bool,
    pub play_sound: bool,
    /// Thumbnail card after each capture (a small toast when off).
    #[serde(alias = "show_toast")]
    pub show_preview: bool,
    pub highlight_windows: bool,
}

impl Default for Config {
    fn default() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        let desktop = dirs::desktop_dir().unwrap_or_else(|| home.clone());
        let movies = dirs::video_dir().unwrap_or_else(|| home.clone());
        Config {
            screenshot_hotkey: "super+shift+Digit2".into(),
            record_hotkey: "super+shift+Digit6".into(),
            screenshot_action: ShotAction::Clipboard,
            screenshot_dir: desktop,
            video_dir: movies,
            video_copy_to_clipboard: true,
            video_show_clicks: true,
            video_record_audio: false,
            reveal_after_save: false,
            play_sound: true,
            show_preview: true,
            highlight_windows: true,
        }
    }
}

fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Snip")
        .join("config.json")
}

impl Config {
    pub fn load() -> Config {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let p = path();
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(p, serde_json::to_vec_pretty(self).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let c: Config = serde_json::from_str(r#"{"play_sound": false}"#).unwrap();
        assert!(!c.play_sound);
        assert_eq!(c.screenshot_hotkey, Config::default().screenshot_hotkey);
        assert_eq!(c.screenshot_action, ShotAction::Clipboard);
    }

    #[test]
    fn old_show_toast_key_still_loads() {
        let c: Config = serde_json::from_str(r#"{"show_toast": false}"#).unwrap();
        assert!(!c.show_preview);
    }

    #[test]
    fn shot_actions() {
        assert_eq!(serde_json::to_string(&ShotAction::Both).unwrap(), r#""both""#);
        assert!(ShotAction::Clipboard.copies() && !ShotAction::Clipboard.saves());
        assert!(!ShotAction::Save.copies() && ShotAction::Save.saves());
        assert!(ShotAction::Both.copies() && ShotAction::Both.saves());
    }
}
