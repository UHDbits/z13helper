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
.hud {
  background-color: rgba(0, 0, 0, 0.72);
  border-radius: 12px;
}
.hud-label {
  font-size: 28px;
  font-weight: 700;
  color: white;
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
