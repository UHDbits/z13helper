use gtk4 as gtk;

const STYLE: &str = r#"
.mode-button { min-width: 72px; min-height: 72px; border: 3px solid transparent; border-radius: 6px; }
.editor-button { min-width: 72px; min-height: 72px; }
.mode-button.silent { color: #06B48A; }
.mode-button.balanced { color: #3AAEEF; }
.mode-button.turbo { color: #FF2020; }
.mode-button:checked {
  border-color: currentColor;
  background-image: linear-gradient(to bottom, alpha(currentColor, 0.22), transparent 45%);
  background-color: alpha(currentColor, 0.10);
}
switch {
  min-width: 48px;
  min-height: 24px;
}
.warning { color: #FF8000; }
.fans-split-view > .sidebar-pane { background-color: transparent; }
scale.undervolt-scale > value.right { margin-left: 6px; }
.hud {
  background-color: rgba(0, 0, 0, 0.72);
  border-radius: 12px;
}
.hud-label {
  font-size: 28px;
  font-weight: 700;
  color: white;
}
.gamescope-overlay-window,
.gamescope-wrapper,
.gamescope-backdrop { background: transparent; }
.gamescope-panel {
  background: @window_bg_color;
  border-radius: 12px;
}
"#;

pub fn install() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

pub fn install_gamescope_scale(scale: f64) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&format!(
        r#"
.gamescope-ui {{ font-size: {:.0}px; }}
.gamescope-ui .mode-button,
.gamescope-ui .editor-button {{ min-width: {:.0}px; min-height: {:.0}px; }}
.gamescope-ui button {{ min-height: {:.0}px; }}
.gamescope-ui .gamescope-choice {{ padding: {:.0}px {:.0}px; }}
.gamescope-ui switch {{ min-width: {:.0}px; min-height: {:.0}px; }}
.gamescope-ui scale slider {{ min-width: {:.0}px; min-height: {:.0}px; }}
.gamescope-ui .hud-label {{ font-size: {:.0}px; }}
"#,
        14.0 * scale,
        56.0 * scale,
        56.0 * scale,
        30.0 * scale,
        3.0 * scale,
        5.0 * scale,
        48.0 * scale,
        24.0 * scale,
        20.0 * scale,
        20.0 * scale,
        28.0 * scale,
    ));
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        );
    }
}
