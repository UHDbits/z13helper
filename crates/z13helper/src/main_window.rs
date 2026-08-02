//! Main window: compact G-Helper-style control panel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13helper_client::{Client, State};
use z13helper_core::label::mode_label;

use crate::app::AppState;
use crate::services::worker;
use crate::ui::fans_window;
use crate::ui::sync::SyncGuard;

pub fn build(state: &Rc<AppState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&state.app)
        .title("z13helper")
        .default_width(460)
        .default_height(640)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("z13helper"))));
    toolbar.add_top_bar(&header);

    let banner =
        adw::Banner::new("z13helperd is not running — sudo systemctl start z13helperd.service");
    banner.set_revealed(false);
    toolbar.add_top_bar(&banner);
    *state.error_banner.borrow_mut() = Some(banner.clone());

    // Outer content is NOT a scrolled window — only the mode grid scrolls when
    // custom profiles overflow.
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

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

    // Built-in row always visible; only custom profiles scroll if they overflow.
    let mode_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .column_homogeneous(true)
        .build();
    content.append(&mode_grid);

    let mode_buttons: Rc<RefCell<Vec<(String, gtk::Button)>>> = Rc::new(RefCell::new(Vec::new()));

    for (index, (name, id, class)) in [
        ("Silent", "silent", "silent"),
        ("Balanced", "balanced", "balanced"),
        ("Turbo", "turbo", "turbo"),
    ]
    .into_iter()
    .enumerate()
    {
        let button = gtk::Button::with_label(name);
        button.add_css_class("mode-button");
        button.add_css_class(class);
        button.set_hexpand(true);
        let state_click = state.clone();
        let label = mode_label_w.clone();
        let buttons = mode_buttons.clone();
        let id_owned = id.to_string();
        button.connect_clicked(move |_| {
            select_profile(&state_click, &id_owned);
            label.set_label(&current_label(&state_click));
            refresh_active_buttons(&buttons, &state_click.config.borrow().active_profile);
        });
        mode_grid.attach(&button, index as i32, 0, 1, 1);
        mode_buttons.borrow_mut().push((id.to_string(), button));
    }

    let fans = gtk::Button::with_label("Fans + Power");
    fans.add_css_class("mode-button");
    fans.add_css_class("custom");
    fans.set_hexpand(true);
    let parent = window.clone();
    let state_fans = state.clone();
    fans.connect_clicked(move |_| fans_window::present(&state_fans, &parent));
    mode_grid.attach(&fans, 3, 0, 1, 1);

    let custom_profiles: Vec<_> = state
        .config
        .borrow()
        .profiles
        .iter()
        .filter(|p| !p.builtin)
        .cloned()
        .collect();
    if !custom_profiles.is_empty() {
        let custom_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .max_content_height(120)
            .build();
        let custom_box = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .homogeneous(true)
            .max_children_per_line(4)
            .min_children_per_line(2)
            .column_spacing(8)
            .row_spacing(8)
            .build();
        for profile in custom_profiles {
            let button = gtk::Button::with_label(&profile.name);
            button.add_css_class("mode-button");
            button.add_css_class("custom");
            let id = profile.id.clone();
            let state_click = state.clone();
            let label = mode_label_w.clone();
            let buttons = mode_buttons.clone();
            button.connect_clicked(move |_| {
                select_profile(&state_click, &id);
                label.set_label(&current_label(&state_click));
                refresh_active_buttons(&buttons, &state_click.config.borrow().active_profile);
            });
            mode_buttons
                .borrow_mut()
                .push((profile.id.clone(), button.clone()));
            custom_box.append(&button);
        }
        custom_scroll.set_child(Some(&custom_box));
        content.append(&custom_scroll);
    }
    refresh_active_buttons(&mode_buttons, &state.config.borrow().active_profile);

    // --- Display ---
    let display = adw::PreferencesGroup::builder().title("Display").build();
    let overdrive = make_switch();
    let od_row = adw::ActionRow::builder().title("Panel Overdrive").build();
    od_row.add_suffix(&overdrive);
    od_row.set_activatable_widget(Some(&overdrive));
    display.add(&od_row);
    content.append(&display);

    let sync = SyncGuard::default();
    {
        let client = state.client.clone();
        let sync = sync.clone();
        overdrive.connect_state_set(move |switch, enabled| {
            if sync.active() {
                switch.set_state(enabled);
                return glib::Propagation::Stop;
            }
            let c = client.clone();
            worker::blocking(move || c.panel_overdrive_set(i32::from(enabled)), |_| {});
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
    let batt_icon = gtk::Image::from_icon_name("battery-symbolic");
    let batt_titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
    batt_titles.set_hexpand(true);
    let batt_title = gtk::Label::new(Some("Battery Charge Limit"));
    batt_title.add_css_class("heading");
    batt_title.set_xalign(0.0);
    let batt_hint = gtk::Label::new(Some("40–100% · 5% steps"));
    batt_hint.add_css_class("dim-label");
    batt_hint.add_css_class("caption");
    batt_hint.set_xalign(0.0);
    batt_titles.append(&batt_title);
    batt_titles.append(&batt_hint);
    let full = gtk::Button::with_label("100%");
    full.add_css_class("flat");
    full.set_tooltip_text(Some("Set charge limit to 100%"));
    batt_header.append(&batt_icon);
    batt_header.append(&batt_titles);
    batt_header.append(&full);
    battery.append(&batt_header);

    let limit = gtk::Scale::with_range(gtk::Orientation::Horizontal, 40.0, 100.0, 5.0);
    limit.set_draw_value(false);
    limit.set_hexpand(true);
    limit.set_digits(0);
    limit.set_round_digits(0);
    battery.append(&limit);
    content.append(&battery);

    install_battery_debounce(state, &limit, &sync);
    let full_label = full.clone();
    limit.connect_value_changed(move |scale| {
        full_label.set_label(&format!("{:.0}%", scale.value()));
    });
    let limit_full = limit.clone();
    full.connect_clicked(move |_| limit_full.set_value(100.0));

    // --- Footer ---
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let version = gtk::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("dim-label");
    version.set_hexpand(true);
    version.set_xalign(0.0);
    let boot = gtk::CheckButton::with_label("Boot sound");
    {
        let client = state.client.clone();
        let sync = sync.clone();
        boot.connect_toggled(move |b| {
            if sync.active() {
                return;
            }
            let c = client.clone();
            let enabled = b.is_active();
            worker::blocking(move || c.boot_sound_set(i32::from(enabled)), |_| {});
        });
    }
    let quit = gtk::Button::with_label("Quit");
    let app = state.app.clone();
    quit.connect_clicked(move |_| app.quit());
    footer.append(&version);
    footer.append(&boot);
    footer.append(&quit);
    content.append(&footer);

    // Initial + periodic sync from daemon.
    let view = MainView {
        settings: SettingsView {
            overdrive,
            battery: limit,
            boot,
            lightbar: lightbar.view,
            keyboard: keyboard.view,
        },
        mode: ModeView {
            label: mode_label_w,
            telemetry,
            buttons: mode_buttons,
        },
        banner,
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
    battery: gtk::Scale,
    boot: gtk::CheckButton,
    lightbar: LightingView,
    keyboard: LightingView,
}

#[derive(Clone)]
struct LightingView {
    enabled: gtk::Switch,
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
    buttons: Rc<RefCell<Vec<(String, gtk::Button)>>>,
}

impl MainView {
    fn sync_from(&self, state: &AppState, daemon: &State) {
        state
            .undervolt_available
            .set(Some(daemon.undervolt_available));
        self.sync.run(|| self.settings.sync_from(daemon));
        self.mode.sync_from(state, daemon);
    }
}

impl SettingsView {
    fn sync_from(&self, daemon: &State) {
        if let Some(value) = daemon.panel_overdrive {
            self.overdrive.set_active(value != 0);
            self.overdrive.set_state(value != 0);
        }
        if let Some(value) = daemon.battery_limit {
            self.battery.set_value(value.clamp(40, 100) as f64);
        }
        if let Some(value) = daemon.boot_sound {
            self.boot.set_active(value != 0);
        }
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
        self.enabled.set_active(lighting.enabled);
        self.enabled.set_state(lighting.enabled);
        let index = match lighting.mode.as_str() {
            "breathe" => 1,
            "cycle" => 2,
            "rainbow" => 3,
            "strobe" => 4,
            _ => 0,
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
    let enabled = make_switch();
    header.append(&icon);
    header.append(&heading);
    header.append(&enabled);
    root.append(&header);

    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let modes = gtk::DropDown::from_strings(&["static", "breathe", "cycle", "rainbow", "strobe"]);
    modes.set_valign(gtk::Align::Center);
    modes.set_hexpand(true);
    modes.set_tooltip_text(Some("Lighting mode"));
    controls.append(&modes);

    let color = gtk::ColorDialogButton::new(Some(gtk::ColorDialog::new()));
    color.set_valign(gtk::Align::Center);
    color.set_tooltip_text(Some("Lighting color"));
    let color_control = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let color_label = gtk::Label::new(Some("Color"));
    color_control.append(&color_label);
    color_control.append(&color);
    controls.append(&color_control);

    let speed = gtk::DropDown::from_strings(&["slow", "normal", "fast"]);
    speed.set_valign(gtk::Align::Center);
    speed.set_tooltip_text(Some("Animation speed"));
    controls.append(&speed);
    root.append(&controls);

    let view = LightingView {
        enabled: enabled.clone(),
        modes: modes.clone(),
        color: color.clone(),
        speed: speed.clone(),
    };

    let intent_view = view.clone();
    let intent_client = state.client.clone();
    let intent_sync = sync.clone();
    enabled.connect_state_set(move |switch, on| {
        if intent_sync.active() {
            switch.set_state(on);
            return glib::Propagation::Stop;
        }
        send_lighting_intent(&intent_client, &intent_view, device, Some(on));
        switch.set_state(on);
        glib::Propagation::Stop
    });

    let color_control_c = color_control.clone();
    let speed_c = speed.clone();
    let intent_view = view.clone();
    let intent_client = state.client.clone();
    let intent_sync = sync.clone();
    modes.connect_selected_notify(move |drop| {
        let mode = drop.selected();
        // cycle(2)/rainbow(3) ignore color; static(0) ignores speed.
        color_control_c.set_visible(mode != 2 && mode != 3);
        speed_c.set_visible(mode != 0);
        if !intent_sync.active() {
            send_lighting_intent(&intent_client, &intent_view, device, None);
        }
    });
    let intent_view = view.clone();
    let intent_client = state.client.clone();
    let intent_sync = sync.clone();
    color.connect_rgba_notify(move |_| {
        if !intent_sync.active() {
            send_lighting_intent(&intent_client, &intent_view, device, None);
        }
    });
    let intent_view = view.clone();
    let intent_client = state.client.clone();
    let intent_sync = sync.clone();
    speed.connect_selected_notify(move |_| {
        if !intent_sync.active() {
            send_lighting_intent(&intent_client, &intent_view, device, None);
        }
    });
    // Initial visibility for static.
    color_control.set_visible(true);
    speed.set_visible(false);

    LightingSection { root, view }
}

fn send_lighting_intent(
    client: &Client,
    view: &LightingView,
    device: &'static str,
    enabled: Option<bool>,
) {
    let enabled = enabled.unwrap_or_else(|| view.enabled.is_active());
    let mode = view
        .modes
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_else(|| "static".into());
    let speed = view
        .speed
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_else(|| "normal".into());
    let rgba = view.color.rgba();
    let color = format!(
        "{:02X}{:02X}{:02X}",
        (rgba.red() * 255.0).round() as u8,
        (rgba.green() * 255.0).round() as u8,
        (rgba.blue() * 255.0).round() as u8
    );
    let client = client.clone();
    worker::blocking(
        move || {
            if enabled {
                client.apply_lighting(&mode, &color, "000000", &speed, 3, device)
            } else {
                client.lighting_off(device)
            }
        },
        move |result| {
            if let Err(error) = result {
                tracing::error!(%error, %device, "lighting write failed");
            }
        },
    );
}

fn install_battery_debounce(state: &Rc<AppState>, scale: &gtk::Scale, sync: &SyncGuard) {
    let source = Rc::new(Cell::new(None::<glib::SourceId>));
    let client = state.client.clone();
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
        let source_done = source.clone();
        source.set(Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(200),
            move || {
                worker::blocking(
                    move || client.battery_limit_set(value),
                    |result| {
                        if let Err(e) = result {
                            tracing::error!(%e, "battery limit write failed");
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

fn refresh_active_buttons(buttons: &RefCell<Vec<(String, gtk::Button)>>, active_id: &str) {
    for (id, button) in buttons.borrow().iter() {
        if id == active_id {
            button.add_css_class("active");
        } else {
            button.remove_css_class("active");
        }
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
