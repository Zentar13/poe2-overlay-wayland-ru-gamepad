//! PoE2 Overlay — a Wayland-native Path of Exile 2 trade overlay for KDE Plasma.
//!
//! Architecture (see `docs/adr/0001-*.md`): the overlay draws over a fullscreen
//! Proton game by promoting the Tauri GTK window to a `wlr-layer-shell` surface
//! on the OVERLAY layer (`overlay`), reads the hovered item by synthesizing
//! Ctrl+C into the game and reading the clipboard (`hotkey` + clipboard), and
//! prices it against the GGG trade2 API + poe.ninja (`trade`). The pieces are
//! stubbed here and wired per the active plan (T2-T4).

#[cfg(target_os = "linux")]
mod gamepad;
mod cheatsheet;
mod clipboard;
mod danger;
mod danger_i18n;
mod hotkey;
mod overlay;
mod settings;
mod trade;

use tauri::{Emitter, Manager, State, WebviewWindow};

/// Hide the overlay surface. The card's ✕ control (and Esc) invoke this; the price
/// check shows it again on the next Ctrl+Alt+D.
#[tauri::command]
fn hide_overlay(window: WebviewWindow) {
    let _ = window.hide();
}

/// Re-price the last-checked item with the overlay's edited filters + selected league
/// (T5 toggles + league selector). Always `Ok` — pricing failures arrive as a
/// `PriceResult` with an error/rate-limited status, not a command error.
#[tauri::command]
async fn requery(
    pricing: State<'_, trade::Pricing>,
    league: Option<String>,
    parsed_stats: Vec<trade::ParsedStat>,
    base_properties: Vec<trade::BaseProp>,
    need: Option<usize>,
) -> Result<trade::PriceResult, ()> {
    Ok(pricing.requery(league, parsed_stats, base_properties, need).await)
}

/// The regex cheat-sheet content for the overlay panel (T8, ADR-0006).
#[tauri::command]
fn get_cheatsheet() -> cheatsheet::Cheatsheet {
    cheatsheet::cheatsheet()
}

/// One category of the price sheet for the overlay panel (T9): poe.ninja prices for
/// the active league. Async — one poe.ninja round-trip (zero GGG quota). Unknown
/// categories fall back to the default (Runes).
#[tauri::command]
async fn get_price_sheet(
    pricing: State<'_, trade::Pricing>,
    category: String,
) -> Result<trade::PriceSheet, ()> {
    Ok(pricing.price_sheet(&category).await)
}

/// Write a cheat-sheet pattern to the X11 clipboard so the user can paste it into the
/// game's Ctrl-F box. `Err` if the X11 clipboard isn't available (regex copy disabled).
#[tauri::command]
fn copy_to_clipboard(clip: State<'_, clipboard::Clip>, text: String) -> Result<(), String> {
    clip.copy(&text)
}

/// Current settings for the settings panel.
#[tauri::command]
fn get_settings(app: tauri::AppHandle, pricing: State<'_, trade::Pricing>) -> settings::SettingsView {
    settings_view(&app, &pricing)
}

/// Set the UI language (`"auto"` or a code), persist it, and tell the panel which
/// language is now in effect.
#[tauri::command]
fn set_language(
    app: tauri::AppHandle,
    pricing: State<'_, trade::Pricing>,
    language: String,
) -> settings::SettingsView {
    let lang = trade::Lang::from_code(&language);
    pricing.set_ui_setting(lang);
    let mut saved = settings::load(&app);
    saved.language = Some(lang.map_or("auto", |l| l.code()).to_string());
    if let Err(e) = settings::save(&app, &saved) {
        eprintln!("[settings] save failed: {e}");
    }
    let _ = app.emit("ui-lang", pricing.ui_lang().code());
    settings_view(&app, &pricing)
}

/// Leagues for the settings panel.
#[tauri::command]
async fn get_leagues(pricing: State<'_, trade::Pricing>) -> Result<settings::LeagueView, ()> {
    Ok(league_view(&pricing).await)
}

/// Choose the league (`"auto"` or a name) for every following check, and persist it.
#[tauri::command]
async fn set_league(
    app: tauri::AppHandle,
    pricing: State<'_, trade::Pricing>,
    league: String,
) -> Result<settings::LeagueView, ()> {
    let mut saved = settings::load(&app);
    saved.league = Some(league);
    pricing.set_league(saved.league());
    if let Err(e) = settings::save(&app, &saved) {
        eprintln!("[settings] save failed: {e}");
    }
    Ok(league_view(&pricing).await)
}

async fn league_view(pricing: &trade::Pricing) -> settings::LeagueView {
    let (leagues, effective) = pricing.leagues().await;
    settings::LeagueView {
        league: pricing.league_setting().unwrap_or_else(|| "auto".to_string()),
        effective,
        leagues,
    }
}

/// Choose the gamepad button that runs a price check (`"none"` to turn it off).
#[cfg(target_os = "linux")]
#[tauri::command]
fn set_pad_trigger(
    app: tauri::AppHandle,
    pricing: State<'_, trade::Pricing>,
    button: String,
) -> settings::SettingsView {
    let mut saved = settings::load(&app);
    saved.pad_trigger = Some(button);
    gamepad::set_trigger(saved.pad_trigger.as_deref());
    if let Err(e) = settings::save(&app, &saved) {
        eprintln!("[settings] save failed: {e}");
    }
    settings::SettingsView {
        pad_trigger: saved.pad_trigger.clone().unwrap_or_else(|| "none".into()),
        ..settings_view_base(&pricing)
    }
}

/// Non-Linux stub (no gamepad support there yet).
#[cfg(not(target_os = "linux"))]
#[tauri::command]
fn set_pad_trigger(pricing: State<'_, trade::Pricing>, button: String) -> settings::SettingsView {
    let _ = button;
    settings_view_base(&pricing)
}

fn settings_view(app: &tauri::AppHandle, pricing: &trade::Pricing) -> settings::SettingsView {
    settings::SettingsView {
        pad_trigger: settings::load(app).pad_trigger.unwrap_or_else(|| "none".into()),
        ..settings_view_base(pricing)
    }
}

fn settings_view_base(pricing: &trade::Pricing) -> settings::SettingsView {
    settings::SettingsView {
        language: pricing.ui_setting().map_or("auto", |l| l.code()).to_string(),
        effective: pricing.ui_lang().code().to_string(),
        pad_trigger: "none".into(),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be the FIRST plugin. KDE's Ctrl+Alt+D shortcut launches
        // `poe2-overlay --price-check`; this forwards that to the running
        // instance and exits the second one (ADR-0002).
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if argv.iter().any(|a| a == "--price-check") {
                let app = app.clone();
                // Off the event-loop thread — price_check sleeps.
                std::thread::spawn(move || hotkey::price_check(&app));
            } else if argv.iter().any(|a| a == "--hide") {
                // Compositor-level dismiss (ADR-0003): KDE forwards `--hide` here.
                // KWin owns the shortcut, so it always reaches us even if the
                // OVERLAY surface were grabbing all pointer/keyboard input — the
                // guaranteed escape from any input-trap, plus the normal close key.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.hide();
                }
            } else if argv.iter().any(|a| a == "--runes") {
                // Category price sheet (T9): not item-driven — the game offers no
                // clipboard copy on reward tooltips, so this is opened deliberately via
                // Ctrl+Alt+F (the binding freed by the dormant regex sheet). Opens on
                // the default category (Runes); the panel's tabs switch categories.
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.emit("show-runes", ());
                    if !w.is_visible().unwrap_or(false) {
                        let _ = w.show();
                    }
                }
            }
            // The `--regex` cheat-sheet trigger (T8, ADR-0006) stays disabled: its
            // Ctrl+Alt+F binding now opens the rune sheet. The backend command + Vue
            // panel are retained, dormant, for an easy restore — re-add a `--regex`
            // branch here and an installer shortcut to re-enable.
        }))
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            hide_overlay,
            requery,
            get_cheatsheet,
            get_price_sheet,
            copy_to_clipboard,
            get_settings,
            set_language,
            get_leagues,
            set_league,
            set_pad_trigger
        ])
        .setup(|app| {
            let window = app
                .get_webview_window("main")
                .expect("main window missing from tauri.conf.json");
            // Promote to a layer-shell OVERLAY surface while still hidden. The
            // window stays hidden until a price check shows it; ✕/Esc hides it.
            overlay::init_layer_shell(&window)?;
            // Build the input-synth device once and keep it warm in state (Linux:
            // uinput; other platforms stub until plan 0002 T3).
            match hotkey::build_synth() {
                Ok(synth) => {
                    app.manage(synth);
                }
                Err(e) => eprintln!("[hotkey] input synth unavailable ({e}); item copy disabled"),
            }
            // Warm pricing client + caches kept in state across checks (T4, ADR-0004).
            let pricing = trade::Pricing::new();
            let saved = settings::load(app.handle());
            pricing.set_ui_setting(saved.lang());
            pricing.set_league(saved.league());
            #[cfg(target_os = "linux")]
            gamepad::set_trigger(saved.pad_trigger.as_deref());
            app.manage(pricing);
            // Controller players: right stick scrolls / B closes the visible card.
            #[cfg(target_os = "linux")]
            gamepad::spawn(app.handle().clone());
            // Persistent X11 clipboard owner for the regex cheat-sheet write (T8, ADR-0006).
            match clipboard::Clip::build() {
                Ok(clip) => {
                    app.manage(clip);
                }
                Err(e) => eprintln!("[clipboard] cannot open X11 clipboard ({e}); regex copy disabled"),
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running PoE2 Overlay");
}
