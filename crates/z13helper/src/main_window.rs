//! Main window: compact G-Helper-style control panel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13helper_client::State;
use z13helper_core::label::mode_label;

use crate::app::AppState;
use crate::services::worker;
use crate::ui::fans_window;
use crate::ui::sync::SyncGuard;

const MAIN_WINDOW_WIDTH: i32 = 368;

pub fn build(state: &Rc<AppState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&state.app)
        .title("z13helper")
        .default_width(MAIN_WINDOW_WIDTH)
        .resizable(false)
        .build();

    // Keep the UI process resident for the hardware toggle button. Closing the
    // main window only hides it; AppState retains it for the next presentation.
    window.connect_close_request(|window| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_decoration_layout(Some(":close"));
    header.set_title_widget(Some(&gtk::Label::new(Some("z13helper"))));
    toolbar.add_top_bar(&header);

    let daemon_banner =
        adw::Banner::new("z13helperd is not running — sudo systemctl start z13helperd.service");
    daemon_banner.set_revealed(false);
    toolbar.add_top_bar(&daemon_banner);
    let persistent_banner = adw::Banner::new("");
    persistent_banner.set_revealed(false);
    toolbar.add_top_bar(&persistent_banner);
    *state.persistent_banner.borrow_mut() = Some(persistent_banner);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 16);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    let sync = SyncGuard::default();

    // --- Performance Mode ---
    let mode_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let mode_label_w = gtk::Label::new(Some(&current_label(state)));
    mode_label_w.set_xalign(0.0);
    mode_label_w.set_hexpand(true);
    mode_label_w.add_css_class("title-4");
    let telemetry = gtk::Label::new(Some("APU: —°C  Fan: — RPM"));
    telemetry.add_css_class("dim-label");
    telemetry.set_xalign(1.0);
    mode_header.append(&mode_label_w);
    mode_header.append(&telemetry);
    content.append(&mode_header);

    // Keep quick mode switching focused on the three built-ins. Custom profiles
    // are created, selected, and edited in Fans + Power.
    let mode_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .column_homogeneous(true)
        .build();
    content.append(&mode_grid);

    let mode_buttons: Rc<RefCell<Vec<(String, gtk::ToggleButton)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let mut mode_group: Option<gtk::ToggleButton> = None;

    for (index, (name, id, class)) in [
        ("Silent", "silent", "silent"),
        ("Balanced", "balanced", "balanced"),
        ("Turbo", "turbo", "turbo"),
    ]
    .into_iter()
    .enumerate()
    {
        let button = gtk::ToggleButton::with_label(name);
        button.add_css_class("mode-button");
        button.add_css_class(class);
        button.set_hexpand(true);
        if let Some(group) = mode_group.as_ref() {
            button.set_group(Some(group));
        } else {
            mode_group = Some(button.clone());
        }
        let state_click = state.clone();
        let label = mode_label_w.clone();
        let buttons = mode_buttons.clone();
        let mode_sync = sync.clone();
        let id_owned = id.to_string();
        button.connect_toggled(move |button| {
            if mode_sync.active() || !button.is_active() {
                return;
            }
            select_profile(&state_click, &id_owned);
            label.set_label(&current_label(&state_click));
            mode_sync.run(|| {
                refresh_active_buttons(&buttons, &state_click.config.borrow().active_profile)
            });
        });
        mode_grid.attach(&button, index as i32, 0, 1, 1);
        mode_buttons.borrow_mut().push((id.to_string(), button));
    }

    let fans = gtk::Button::with_label("Fans +\nPower");
    fans.add_css_class("editor-button");
    fans.set_hexpand(true);
    let parent = window.clone();
    let state_fans = state.clone();
    fans.connect_clicked(move |_| fans_window::present(&state_fans, &parent));
    mode_grid.attach(&fans, 3, 0, 1, 1);

    sync.run(|| refresh_active_buttons(&mode_buttons, &state.config.borrow().active_profile));

    // --- Display ---
    let display = adw::PreferencesGroup::builder().title("Display").build();
    display.set_margin_top(-4);
    let overdrive = make_switch();
    let od_row = adw::ActionRow::builder().title("Panel Overdrive").build();
    od_row.add_suffix(&overdrive);
    od_row.set_activatable_widget(Some(&overdrive));
    display.add(&od_row);
    content.append(&display);

    {
        let client = state.client.clone();
        let sync = sync.clone();
        let feedback = state.clone();
        overdrive.connect_state_set(move |switch, enabled| {
            if sync.active() {
                switch.set_state(enabled);
                return glib::Propagation::Stop;
            }
            let c = client.clone();
            let feedback = feedback.clone();
            worker::blocking(
                move || c.panel_overdrive_set(i32::from(enabled)),
                move |result| {
                    if let Err(error) = result {
                        feedback.report_error(&format!("Panel overdrive failed: {error}"));
                    }
                },
            );
            switch.set_state(enabled);
            glib::Propagation::Stop
        });
    }

    // --- Lighting ---
    let lightbar = lighting_section(
        "Lightbar",
        "display-brightness-symbolic",
        "lightbar",
        state,
        &sync,
    );
    let keyboard = lighting_section(
        "Laptop Keyboard",
        "input-keyboard-symbolic",
        "keyboard",
        state,
        &sync,
    );
    content.append(&lightbar.root);
    content.append(&keyboard.root);

    // --- Battery ---
    let battery = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let batt_header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let batt_icon = gtk::Image::from_icon_name("z13helper-battery-limit-symbolic");
    let batt_titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
    batt_titles.set_hexpand(true);
    let batt_title = gtk::Label::new(Some("Battery Charge Limit: —%"));
    batt_title.add_css_class("heading");
    batt_title.set_xalign(0.0);
    batt_titles.append(&batt_title);
    let battery_status = gtk::Label::new(Some("—"));
    battery_status.add_css_class("dim-label");
    battery_status.set_xalign(1.0);
    batt_header.append(&batt_icon);
    batt_header.append(&batt_titles);
    batt_header.append(&battery_status);
    battery.append(&batt_header);

    let limit_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let limit = gtk::Scale::with_range(gtk::Orientation::Horizontal, 40.0, 100.0, 5.0);
    limit.set_draw_value(false);
    limit.set_hexpand(true);
    // Preserve the visual left alignment without reducing GtkScale's
    // effective minimum width below the size required by its theme.
    limit.set_size_request(42, -1);
    limit.set_margin_start(-8);
    limit.set_digits(0);
    limit.set_round_digits(0);
    let full = gtk::ToggleButton::with_label("100%");
    full.set_valign(gtk::Align::Center);
    full.set_size_request(64, -1);
    full.set_tooltip_text(Some("Charge once to 100%"));
    limit_row.append(&limit);
    limit_row.append(&full);
    battery.append(&limit_row);

    let battery_charge = gtk::Label::new(Some("Charge: —%"));
    battery_charge.add_css_class("dim-label");
    battery_charge.set_halign(gtk::Align::End);
    battery.append(&battery_charge);
    content.append(&battery);

    install_battery_debounce(state, &limit, &sync);
    let title_for_limit = batt_title.clone();
    let full_for_limit = full.clone();
    limit.connect_value_changed(move |scale| {
        if !full_for_limit.is_active() {
            title_for_limit.set_label(&battery_limit_title(
                false,
                Some(scale.value().round() as i32),
            ));
        }
    });
    let client = state.client.clone();
    let feedback = state.clone();
    let one_time_sync = sync.clone();
    let title_for_toggle = batt_title.clone();
    let limit_for_toggle = limit.clone();
    full.connect_toggled(move |button| {
        if one_time_sync.active() {
            return;
        }
        let enabled = button.is_active();
        let normal_limit = Some(limit_for_toggle.value().round() as i32);
        sync_one_time_charge_button(button, &title_for_toggle, enabled, normal_limit);
        let client = client.clone();
        let feedback = feedback.clone();
        let button_done = button.clone();
        let title_done = title_for_toggle.clone();
        let sync_done = one_time_sync.clone();
        worker::blocking(
            move || client.battery_one_time_charge_set(enabled),
            move |result| {
                if let Err(error) = result {
                    sync_done.run(|| {
                        sync_one_time_charge_button(
                            &button_done,
                            &title_done,
                            !enabled,
                            normal_limit,
                        )
                    });
                    feedback.report_error(&format!("One-time charge failed: {error}"));
                }
            },
        );
    });

    // --- Footer ---
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let version = gtk::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("dim-label");
    version.set_hexpand(true);
    version.set_valign(gtk::Align::End);
    version.set_xalign(0.0);
    let hide = gtk::Button::with_label("Hide");
    hide.set_tooltip_text(Some("Hide z13helper"));
    let window_hide = window.clone();
    hide.connect_clicked(move |_| window_hide.set_visible(false));
    footer.append(&version);
    footer.append(&hide);
    content.append(&footer);

    let clamp = adw::Clamp::new();
    clamp.set_maximum_size(600);
    clamp.set_tightening_threshold(500);
    clamp.set_child(Some(&content));
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .child(&clamp)
        .build();
    let toast_overlay = adw::ToastOverlay::new();
    toast_overlay.set_child(Some(&scroller));
    state.register_toast_overlay(&toast_overlay);
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));

    // Initial + periodic sync from daemon.
    let view = MainView {
        settings: SettingsView {
            overdrive,
            battery_limit: limit,
            battery_title: batt_title,
            battery_one_time_charge: full,
            battery_charge,
            battery_status,
            lightbar: lightbar.view,
            keyboard: keyboard.view,
        },
        mode: ModeView {
            label: mode_label_w,
            telemetry,
            buttons: mode_buttons,
        },
        banner: daemon_banner,
        sync,
    };
    sync_once(state, &view);
    install_telemetry(state, &window, view);

    window
}

#[derive(Clone)]
struct MainView {
    settings: SettingsView,
    mode: ModeView,
    banner: adw::Banner,
    sync: SyncGuard,
}

#[derive(Clone)]
struct SettingsView {
    overdrive: gtk::Switch,
    battery_limit: gtk::Scale,
    battery_title: gtk::Label,
    battery_one_time_charge: gtk::ToggleButton,
    battery_charge: gtk::Label,
    battery_status: gtk::Label,
    lightbar: LightingView,
    keyboard: LightingView,
}

#[derive(Clone)]
struct LightingView {
    modes: gtk::DropDown,
    color: gtk::ColorDialogButton,
    speed: gtk::DropDown,
}

struct LightingSection {
    root: gtk::Box,
    view: LightingView,
}

#[derive(Clone)]
struct ModeView {
    label: gtk::Label,
    telemetry: gtk::Label,
    buttons: Rc<RefCell<Vec<(String, gtk::ToggleButton)>>>,
}

impl MainView {
    fn sync_from(&self, state: &AppState, daemon: &State) {
        state
            .undervolt_available
            .set(Some(daemon.undervolt_available));
        self.sync.run(|| {
            self.settings.sync_from(daemon);
            self.mode.sync_from(state, daemon);
        });
    }
}

impl SettingsView {
    fn sync_from(&self, daemon: &State) {
        if let Some(value) = daemon.panel_overdrive {
            self.overdrive.set_active(value != 0);
            self.overdrive.set_state(value != 0);
        }
        if let Some(value) = daemon.battery_limit {
            self.battery_limit.set_value(value.clamp(40, 100) as f64);
        }
        sync_one_time_charge_button(
            &self.battery_one_time_charge,
            &self.battery_title,
            daemon.battery_one_time_charge,
            daemon.battery_limit,
        );
        self.battery_charge.set_label(
            &daemon
                .battery
                .charge_percent
                .map_or_else(|| "Charge: —%".into(), |value| format!("Charge: {value}%")),
        );
        self.battery_charge
            .set_tooltip_text(Some(&daemon.battery.health_percent.map_or_else(
                || "Battery health: unavailable".into(),
                |value| format!("Battery health: {value}%"),
            )));
        self.battery_status.set_label(&battery_status_label(
            daemon.battery.status.as_deref(),
            daemon.battery.power_microwatts,
        ));
        self.lightbar.sync_from(
            daemon
                .devices
                .as_ref()
                .and_then(|devices| devices.get("lightbar"))
                .unwrap_or(&daemon.lighting),
        );
        self.keyboard.sync_from(
            daemon
                .devices
                .as_ref()
                .and_then(|devices| devices.get("keyboard"))
                .unwrap_or(&daemon.lighting),
        );
    }
}

impl LightingView {
    fn sync_from(&self, lighting: &z13helper_client::LightingState) {
        let index = if !lighting.enabled {
            0
        } else {
            match lighting.mode.as_str() {
                "breathe" => 2,
                "cycle" => 3,
                "rainbow" => 4,
                "strobe" => 5,
                _ => 1,
            }
        };
        self.modes.set_selected(index);
        if let Ok(color) = gtk::gdk::RGBA::parse(format!("#{}", lighting.color)) {
            self.color.set_rgba(&color);
        }
        self.speed.set_selected(match lighting.speed.as_str() {
            "slow" => 0,
            "fast" => 2,
            _ => 1,
        });
    }
}

impl ModeView {
    fn sync_from(&self, state: &AppState, daemon: &State) {
        self.label.set_label(&current_label(state));
        refresh_active_buttons(&self.buttons, &state.config.borrow().active_profile);
        self.telemetry.set_label(&format!(
            "APU: {}°C  Fans: {} / {} RPM",
            daemon
                .temperature
                .map_or_else(|| "—".into(), |value| value.to_string()),
            daemon.fan_rpms[0],
            daemon.fan_rpms[1]
        ));
    }
}

fn battery_status_label(status: Option<&str>, power_microwatts: Option<u64>) -> String {
    let status = status.filter(|status| !status.is_empty()).unwrap_or("—");
    if status == "Full" {
        return status.into();
    }
    power_microwatts.map_or_else(
        || status.into(),
        |power| format!("{status}: {:.1} W", power as f64 / 1_000_000.0),
    )
}

fn sync_one_time_charge_button(
    button: &gtk::ToggleButton,
    title: &gtk::Label,
    active: bool,
    normal_limit: Option<i32>,
) {
    button.set_active(active);
    if active {
        button.add_css_class("suggested-action");
        button.set_tooltip_text(Some("Cancel one-time charge"));
    } else {
        button.remove_css_class("suggested-action");
        button.set_tooltip_text(Some("Charge once to 100%"));
    }
    title.set_label(&battery_limit_title(active, normal_limit));
}

fn battery_limit_title(one_time_charge: bool, normal_limit: Option<i32>) -> String {
    if one_time_charge {
        "Battery Charge Limit: One time to 100%".into()
    } else {
        normal_limit.map_or_else(
            || "Battery Charge Limit: —%".into(),
            |limit| format!("Battery Charge Limit: {limit}%"),
        )
    }
}

fn make_switch() -> gtk::Switch {
    let sw = gtk::Switch::new();
    // Prevent ActionRow from crushing the switch into a vertical ellipse.
    sw.set_valign(gtk::Align::Center);
    sw.set_halign(gtk::Align::End);
    sw.set_size_request(48, 24);
    sw
}

fn lighting_section(
    title: &str,
    icon_name: &str,
    device: &'static str,
    state: &Rc<AppState>,
    sync: &SyncGuard,
) -> LightingSection {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let icon = gtk::Image::from_icon_name(icon_name);
    let heading = gtk::Label::new(Some(title));
    heading.add_css_class("heading");
    heading.set_xalign(0.0);
    heading.set_hexpand(true);
    header.append(&icon);
    header.append(&heading);
    root.append(&header);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let modes =
        gtk::DropDown::from_strings(&["Off", "Static", "Breathe", "Cycle", "Rainbow", "Strobe"]);
    modes.set_valign(gtk::Align::Center);
    modes.set_hexpand(true);
    modes.set_tooltip_text(Some("Lighting mode"));
    controls.append(&modes);

    let color = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
    color.set_valign(gtk::Align::Center);
    color.set_size_request(64, -1);
    color.set_tooltip_text(Some("Lighting color"));
    let color_control = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    color_control.append(&color);
    controls.append(&color_control);

    let speed = gtk::DropDown::from_strings(&["Slow", "Normal", "Fast"]);
    speed.set_valign(gtk::Align::Center);
    speed.set_tooltip_text(Some("Animation speed"));
    controls.append(&speed);
    root.append(&controls);

    let view = LightingView {
        modes: modes.clone(),
        color: color.clone(),
        speed: speed.clone(),
    };

    let color_control_c = color_control.clone();
    let speed_c = speed.clone();
    let intent_view = view.clone();
    let intent_state = state.clone();
    let intent_sync = sync.clone();
    modes.connect_selected_notify(move |drop| {
        let mode = drop.selected();
        // Off/cycle/rainbow hide color; Off/static hide speed.
        color_control_c.set_visible(mode != 0 && mode != 3 && mode != 4);
        speed_c.set_visible(mode != 0 && mode != 1);
        if !intent_sync.active() {
            send_lighting_intent(&intent_state, &intent_view, device);
        }
    });
    let intent_view = view.clone();
    let intent_state = state.clone();
    let intent_sync = sync.clone();
    color.connect_rgba_notify(move |_| {
        if !intent_sync.active() {
            send_lighting_intent(&intent_state, &intent_view, device);
        }
    });
    let intent_view = view.clone();
    let intent_state = state.clone();
    let intent_sync = sync.clone();
    speed.connect_selected_notify(move |_| {
        if !intent_sync.active() {
            send_lighting_intent(&intent_state, &intent_view, device);
        }
    });
    // Initial visibility for Off.
    color_control.set_visible(false);
    speed.set_visible(false);

    LightingSection { root, view }
}

fn send_lighting_intent(state: &Rc<AppState>, view: &LightingView, device: &'static str) {
    let mode_label = view
        .modes
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_else(|| "Off".into());
    let (enabled, mode) = match mode_label.as_str() {
        "Static" => (true, "static"),
        "Breathe" => (true, "breathe"),
        "Cycle" => (true, "cycle"),
        "Rainbow" => (true, "rainbow"),
        "Strobe" => (true, "strobe"),
        _ => (false, "static"),
    };
    let speed = view
        .speed
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .map(|speed| speed.to_ascii_lowercase())
        .unwrap_or_else(|| "normal".into());
    let rgba = view.color.rgba();
    let color = format!(
        "{:02X}{:02X}{:02X}",
        (rgba.red() * 255.0).round() as u8,
        (rgba.green() * 255.0).round() as u8,
        (rgba.blue() * 255.0).round() as u8
    );
    let client = state.client.clone();
    let feedback = state.clone();
    worker::blocking(
        move || {
            if enabled {
                client.apply_lighting(mode, &color, "000000", &speed, 3, device)
            } else {
                client.lighting_off(device)
            }
        },
        move |result| {
            if let Err(error) = result {
                tracing::error!(%error, %device, "lighting write failed");
                feedback.report_error(&format!("{device} lighting failed: {error}"));
            }
        },
    );
}

fn install_battery_debounce(state: &Rc<AppState>, scale: &gtk::Scale, sync: &SyncGuard) {
    let source = Rc::new(Cell::new(None::<glib::SourceId>));
    let client = state.client.clone();
    let feedback = state.clone();
    let sync = sync.clone();
    scale.connect_value_changed(move |scale| {
        if sync.active() {
            return;
        }
        if let Some(id) = source.take() {
            id.remove();
        }
        // Snap to nearest 5 and clamp to [40, 100].
        let raw = scale.value().round() as i32;
        let value = ((raw + 2) / 5 * 5).clamp(40, 100);
        if (scale.value() - value as f64).abs() > 0.1 {
            sync.run(|| scale.set_value(value as f64));
        }
        let client = client.clone();
        let feedback = feedback.clone();
        let source_done = source.clone();
        source.set(Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(200),
            move || {
                worker::blocking(
                    move || client.battery_limit_set(value),
                    move |result| {
                        if let Err(e) = result {
                            tracing::error!(%e, "battery limit write failed");
                            feedback.report_error(&format!("Battery limit failed: {e}"));
                        }
                    },
                );
                source_done.set(None);
            },
        )));
    });
}

fn sync_once(state: &Rc<AppState>, view: &MainView) {
    let client = state.client.clone();
    let view = view.clone();
    let state = state.clone();
    worker::blocking(
        move || client.get_state(),
        move |result| match result {
            Ok(s) => {
                view.banner.set_revealed(false);
                view.sync_from(&state, &s);
            }
            Err(_) => view.banner.set_revealed(true),
        },
    );
}

fn install_telemetry(state: &Rc<AppState>, window: &adw::ApplicationWindow, view: MainView) {
    let busy = Rc::new(Cell::new(false));
    let state = state.clone();
    let window = window.clone();
    glib::timeout_add_seconds_local(1, move || {
        if !window.is_visible() || busy.replace(true) {
            return glib::ControlFlow::Continue;
        }
        let client = state.client.clone();
        let busy = busy.clone();
        let state = state.clone();
        let view = view.clone();
        worker::blocking(
            move || client.get_state(),
            move |result| {
                busy.set(false);
                match result {
                    Ok(s) => {
                        view.banner.set_revealed(false);
                        view.sync_from(&state, &s);
                    }
                    Err(_) => view.banner.set_revealed(true),
                }
            },
        );
        glib::ControlFlow::Continue
    });
}

fn refresh_active_buttons(buttons: &RefCell<Vec<(String, gtk::ToggleButton)>>, active_id: &str) {
    for (id, button) in buttons.borrow().iter() {
        button.set_active(id == active_id);
    }
}

fn current_label(state: &AppState) -> String {
    state
        .config
        .borrow()
        .active()
        .map(mode_label)
        .unwrap_or_else(|| "Mode: Balanced".into())
}

fn select_profile(state: &Rc<AppState>, id: &str) {
    state.config.borrow_mut().active_profile = id.into();
    state.apply_active(false);
}

#[cfg(test)]
mod battery_tests {
    use super::{battery_limit_title, battery_status_label};

    #[test]
    fn formats_status_with_live_power() {
        assert_eq!(
            battery_status_label(Some("Charging"), Some(14_500_000)),
            "Charging: 14.5 W"
        );
        assert_eq!(battery_status_label(Some("Full"), Some(0)), "Full");
        assert_eq!(battery_status_label(Some("Full"), Some(12_300_000)), "Full");
        assert_eq!(battery_status_label(None, None), "—");
    }

    #[test]
    fn formats_normal_and_one_time_charge_limits() {
        assert_eq!(
            battery_limit_title(false, Some(80)),
            "Battery Charge Limit: 80%"
        );
        assert_eq!(
            battery_limit_title(true, Some(80)),
            "Battery Charge Limit: One time to 100%"
        );
    }
}
