//! User settings, persisted as JSON in the app config dir
//! (`~/.config/io.olund.poe2overlay/settings.json`). Only the UI language for now.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::trade::Lang;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// UI language code (`"ru"`, `"de"`, …); `None`/`"auto"` = follow the game client.
    #[serde(default)]
    pub language: Option<String>,
    /// Trade league to price in; `None`/`"auto"` = the current challenge league.
    #[serde(default)]
    pub league: Option<String>,
    /// Gamepad button that runs a price check: `"share"`, `"select"`, `"l3"`, `"r3"`;
    /// `None`/`"none"` = off.
    #[serde(default)]
    pub pad_trigger: Option<String>,
}

impl Settings {
    /// The chosen UI language, `None` = auto.
    pub fn lang(&self) -> Option<Lang> {
        self.language.as_deref().and_then(Lang::from_code)
    }

    /// The chosen league, `None` = auto.
    pub fn league(&self) -> Option<String> {
        self.league.clone().filter(|l| l != "auto" && !l.is_empty())
    }
}

/// The league part of the settings panel.
#[derive(Debug, Clone, Serialize)]
pub struct LeagueView {
    /// `"auto"` or a league name.
    pub league: String,
    /// The league actually priced in.
    pub effective: String,
    /// Current leagues to choose from.
    pub leagues: Vec<String>,
}

/// What the settings panel shows: the choice and the language actually in effect.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    /// `"auto"` or a language code.
    pub language: String,
    /// The UI language in effect (auto resolves to the game client's).
    pub effective: String,
    /// Gamepad price-check button id, `"none"` when off.
    pub pad_trigger: String,
}

fn path(app: &AppHandle) -> Option<PathBuf> {
    app.path().app_config_dir().ok().map(|d| d.join("settings.json"))
}

/// Saved settings; defaults when missing or unreadable.
pub fn load(app: &AppHandle) -> Settings {
    path(app)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(app: &AppHandle, settings: &Settings) -> std::io::Result<()> {
    let p = path(app).ok_or_else(|| std::io::Error::other("no app config dir"))?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(std::io::Error::other)?;
    std::fs::write(p, json)
}
