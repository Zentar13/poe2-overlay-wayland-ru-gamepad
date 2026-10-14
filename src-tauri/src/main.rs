// Prevents an extra console window on Windows in release (irrelevant on Linux,
// but the Tauri convention — leave it).
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // Critical for the overlay on native Wayland (see ADR-0001):
    // - GDK_BACKEND=wayland forces GTK onto a real Wayland display so the
    //   gtk-layer-shell surface works (otherwise GTK may attach via XWayland
    //   and the layer-shell promotion silently no-ops).
    // - WEBKIT_DISABLE_DMABUF_RENDERER=1 avoids a WebKitGTK crash on KDE Wayland.
    // Set before any GTK/WebKit init. The launcher/.desktop should also export
    // these as a belt-and-suspenders. Linux-only: WebView2 on Windows needs neither.
    #[cfg(target_os = "linux")]
    {
        std::env::set_var("GDK_BACKEND", "wayland");
        // Default only — an explicit value from the launcher wins, so the renderer can
        // be switched per machine (`=0` re-enables DMA-BUF) without a rebuild.
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    poe2_overlay_lib::run()
}
