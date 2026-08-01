//! Main window: compact G-Helper-style control panel.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13_helper_core::label::mode_label;
use z13ctl_client::State;

use crate::app::AppState;
use crate::{fans_window, worker};

pub fn build(state: &Rc<AppState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&state.app)
        .title("z13 Helper")
        .default_width(460)
        .default_height(640)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("z13 Helper"))));
    toolbar.add_top_bar(&header);

    let banner =
        adw::Banner::new("z13ctl daemon not running — systemctl --user start z13ctl.service");
    banner.set_revealed(false);
    toolbar.add_top_bar(&banner);
    *state.error_banner.borrow_mut() = Some(banner.clone());

    // Outer content is NOT a scrolled window — only the mode grid scrolls when
    // custom profiles overflow.
    let content = gtk::Box::new(gtk::Orientation::Vertical, 10);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(14);
    content.set_margin_end(14);
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

    let mode_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .propagate_natural_height(true)
        .max_content_height(160)
        .build();
    let mode_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    mode_scroll.set_child(Some(&mode_box));

    let mode_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .column_homogeneous(true)
        .build();
    mode_box.append(&mode_grid);

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

    let custom_box = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .max_children_per_line(4)
        .min_children_per_line(2)
        .column_spacing(8)
        .row_spacing(8)
        .build();
    for profile in state.config.borrow().profiles.iter().filter(|p| !p.builtin) {
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
    mode_box.append(&custom_box);
    content.append(&mode_scroll);
    refresh_active_buttons(&mode_buttons, &state.config.borrow().active_profile);

    // --- Display ---
    let display = adw::PreferencesGroup::builder().title("Display").build();
    let overdrive = make_switch();
    let od_row = adw::ActionRow::builder().title("Panel Overdrive").build();
    od_row.add_suffix(&overdrive);
    od_row.set_activatable_widget(Some(&overdrive));
    display.add(&od_row);
    content.append(&display);

    let syncing = Rc::new(Cell::new(false));
    {
        let client = state.client.clone();
        let syncing = syncing.clone();
        overdrive.connect_state_set(move |switch, enabled| {
            if syncing.get() {
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
    let (lightbar, lb_enabled, lb_modes) = lighting_group("Lightbar", "lightbar", state);
    let (keyboard, kb_enabled, kb_modes) = lighting_group("Laptop Keyboard", "keyboard", state);
    content.append(&lightbar);
    content.append(&keyboard);

    // --- Battery ---
    let battery = adw::PreferencesGroup::builder()
        .title("Battery Charge Limit")
        .build();
    let batt_header = adw::ActionRow::builder()
        .title("Limit")
        .subtitle("40–100%, steps of 5")
        .build();
    let full = gtk::Button::with_label("100%");
    full.add_css_class("flat");
    batt_header.add_suffix(&full);
    battery.add(&batt_header);

    let limit = gtk::Scale::with_range(gtk::Orientation::Horizontal, 40.0, 100.0, 5.0);
    limit.set_draw_value(true);
    limit.set_value_pos(gtk::PositionType::Right);
    limit.set_hexpand(true);
    limit.set_digits(0);
    limit.set_margin_start(12);
    limit.set_margin_end(12);
    limit.set_margin_bottom(8);
    // Prefer round-digits so the thumb lands on 5% steps.
    limit.set_round_digits(0);
    battery.add(&limit);
    content.append(&battery);

    let batt_syncing = Rc::new(Cell::new(false));
    install_battery_debounce(state, &limit, &batt_syncing);
    let limit_full = limit.clone();
    full.connect_clicked(move |_| limit_full.set_value(100.0));

    // --- Footer ---
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let version = gtk::Label::new(Some(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    version.add_css_class("dim-label");
    version.set_hexpand(true);
    version.set_xalign(0.0);
    let boot = gtk::CheckButton::with_label("Boot sound");
    let boot_syncing = Rc::new(Cell::new(false));
    {
        let client = state.client.clone();
        let boot_syncing = boot_syncing.clone();
        boot.connect_toggled(move |b| {
            if boot_syncing.get() {
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
    let widgets = SyncWidgets {
        overdrive,
        battery: limit,
        boot,
        lb_enabled,
        lb_modes,
        kb_enabled,
        kb_modes,
        mode_label: mode_label_w,
        telemetry,
        banner,
        mode_buttons,
        syncing,
        batt_syncing,
        boot_syncing,
    };
    sync_once(state, &widgets);
    install_telemetry(state, &window, widgets);

    window
}

struct SyncWidgets {
    overdrive: gtk::Switch,
    battery: gtk::Scale,
    boot: gtk::CheckButton,
    lb_enabled: gtk::Switch,
    lb_modes: gtk::DropDown,
    kb_enabled: gtk::Switch,
    kb_modes: gtk::DropDown,
    mode_label: gtk::Label,
    telemetry: gtk::Label,
    banner: adw::Banner,
    mode_buttons: Rc<RefCell<Vec<(String, gtk::Button)>>>,
    syncing: Rc<Cell<bool>>,
    batt_syncing: Rc<Cell<bool>>,
    boot_syncing: Rc<Cell<bool>>,
}

fn make_switch() -> gtk::Switch {
    let sw = gtk::Switch::new();
    // Prevent ActionRow from crushing the switch into a vertical ellipse.
    sw.set_valign(gtk::Align::Center);
    sw.set_halign(gtk::Align::End);
    sw.set_size_request(48, 24);
    sw
}

fn lighting_group(
    title: &str,
    device: &'static str,
    state: &Rc<AppState>,
) -> (adw::PreferencesGroup, gtk::Switch, gtk::DropDown) {
    let group = adw::PreferencesGroup::builder().title(title).build();

    let enabled = make_switch();
    let on_row = adw::ActionRow::builder().title("Power").build();
    on_row.add_suffix(&enabled);
    on_row.set_activatable_widget(Some(&enabled));
    group.add(&on_row);

    let modes = gtk::DropDown::from_strings(&["static", "breathe", "cycle", "rainbow", "strobe"]);
    modes.set_valign(gtk::Align::Center);
    modes.set_size_request(120, -1);
    let mode_row = adw::ActionRow::builder().title("Mode").build();
    mode_row.add_suffix(&modes);
    group.add(&mode_row);

    #[allow(deprecated)]
    let color = gtk::ColorButton::new();
    color.set_valign(gtk::Align::Center);
    let color_row = adw::ActionRow::builder().title("Color").build();
    color_row.add_suffix(&color);
    group.add(&color_row);

    let speed = gtk::DropDown::from_strings(&["slow", "normal", "fast"]);
    speed.set_valign(gtk::Align::Center);
    speed.set_size_request(120, -1);
    let speed_row = adw::ActionRow::builder().title("Speed").build();
    speed_row.add_suffix(&speed);
    group.add(&speed_row);

    let client = state.client.clone();
    let apply = Rc::new({
        let client = client.clone();
        move |on: bool, mode: String| {
            let c = client.clone();
            worker::blocking(
                move || {
                    if on {
                        c.apply_lighting(&mode, "FFFFFF", "FFFFFF", "normal", 3, device)
                    } else {
                        c.lighting_off(device)
                    }
                },
                |_| {},
            );
        }
    });

    let effect = modes.clone();
    let callback = apply.clone();
    enabled.connect_state_set(move |switch, on| {
        let mode = effect
            .selected_item()
            .and_downcast::<gtk::StringObject>()
            .map(|s| s.string().to_string())
            .unwrap_or_else(|| "static".into());
        callback(on, mode);
        switch.set_state(on);
        glib::Propagation::Stop
    });

    let color_row_c = color_row.clone();
    let speed_row_c = speed_row.clone();
    modes.connect_selected_notify(move |drop| {
        let mode = drop.selected();
        // cycle(2)/rainbow(3) ignore color; static(0) ignores speed.
        color_row_c.set_visible(mode != 2 && mode != 3);
        speed_row_c.set_visible(mode != 0);
    });
    // Initial visibility for static.
    color_row.set_visible(true);
    speed_row.set_visible(false);

    (group, enabled, modes)
}

fn install_battery_debounce(state: &Rc<AppState>, scale: &gtk::Scale, syncing: &Rc<Cell<bool>>) {
    let source = Rc::new(Cell::new(None::<glib::SourceId>));
    let client = state.client.clone();
    let syncing = syncing.clone();
    scale.connect_value_changed(move |scale| {
        if syncing.get() {
            return;
        }
        if let Some(id) = source.take() {
            id.remove();
        }
        // Snap to nearest 5 and clamp to [40, 100].
        let raw = scale.value().round() as i32;
        let value = ((raw + 2) / 5 * 5).clamp(40, 100);
        if (scale.value() - value as f64).abs() > 0.1 {
            syncing.set(true);
            scale.set_value(value as f64);
            syncing.set(false);
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
                            eprintln!("battery limit: {e}");
                        }
                    },
                );
                source_done.set(None);
            },
        )));
    });
}

fn apply_state_to_widgets(state: &AppState, s: &State, w: &SyncWidgets) {
    w.syncing.set(true);
    if let Some(v) = s.panel_overdrive {
        w.overdrive.set_active(v != 0);
        w.overdrive.set_state(v != 0);
    }
    w.syncing.set(false);

    w.batt_syncing.set(true);
    if let Some(v) = s.battery_limit {
        w.battery.set_value(v.clamp(40, 100) as f64);
    }
    w.batt_syncing.set(false);

    w.boot_syncing.set(true);
    if let Some(v) = s.boot_sound {
        w.boot.set_active(v != 0);
    }
    w.boot_syncing.set(false);

    apply_lighting_zone(
        &w.lb_enabled,
        &w.lb_modes,
        s.devices
            .as_ref()
            .and_then(|d| d.get("lightbar"))
            .unwrap_or(&s.lighting),
    );
    apply_lighting_zone(
        &w.kb_enabled,
        &w.kb_modes,
        s.devices
            .as_ref()
            .and_then(|d| d.get("keyboard"))
            .unwrap_or(&s.lighting),
    );

    w.mode_label.set_label(&current_label(state));
    refresh_active_buttons(&w.mode_buttons, &state.config.borrow().active_profile);
    w.telemetry.set_label(&format!(
        "APU: {}°C  Fan: {} RPM",
        s.temperature.map_or_else(|| "—".into(), |v| v.to_string()),
        s.fan_rpm.map_or_else(|| "—".into(), |v| v.to_string())
    ));
}

fn apply_lighting_zone(
    enabled: &gtk::Switch,
    modes: &gtk::DropDown,
    lighting: &z13ctl_client::LightingState,
) {
    enabled.set_active(lighting.enabled);
    enabled.set_state(lighting.enabled);
    let idx = match lighting.mode.as_str() {
        "breathe" => 1,
        "cycle" => 2,
        "rainbow" => 3,
        "strobe" => 4,
        _ => 0,
    };
    modes.set_selected(idx);
}

fn sync_once(state: &Rc<AppState>, widgets: &SyncWidgets) {
    let client = state.client.clone();
    // Clone widget handles for the callback.
    let overdrive = widgets.overdrive.clone();
    let battery = widgets.battery.clone();
    let boot = widgets.boot.clone();
    let lb_enabled = widgets.lb_enabled.clone();
    let lb_modes = widgets.lb_modes.clone();
    let kb_enabled = widgets.kb_enabled.clone();
    let kb_modes = widgets.kb_modes.clone();
    let mode_label = widgets.mode_label.clone();
    let telemetry = widgets.telemetry.clone();
    let banner = widgets.banner.clone();
    let mode_buttons = widgets.mode_buttons.clone();
    let syncing = widgets.syncing.clone();
    let batt_syncing = widgets.batt_syncing.clone();
    let boot_syncing = widgets.boot_syncing.clone();
    let state = state.clone();
    worker::blocking(
        move || client.get_state(),
        move |result| match result {
            Ok(s) => {
                banner.set_revealed(false);
                let w = SyncWidgets {
                    overdrive,
                    battery,
                    boot,
                    lb_enabled,
                    lb_modes,
                    kb_enabled,
                    kb_modes,
                    mode_label,
                    telemetry,
                    banner,
                    mode_buttons,
                    syncing,
                    batt_syncing,
                    boot_syncing,
                };
                apply_state_to_widgets(&state, &s, &w);
            }
            Err(_) => banner.set_revealed(true),
        },
    );
}

fn install_telemetry(state: &Rc<AppState>, window: &adw::ApplicationWindow, widgets: SyncWidgets) {
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
        let overdrive = widgets.overdrive.clone();
        let battery = widgets.battery.clone();
        let boot = widgets.boot.clone();
        let lb_enabled = widgets.lb_enabled.clone();
        let lb_modes = widgets.lb_modes.clone();
        let kb_enabled = widgets.kb_enabled.clone();
        let kb_modes = widgets.kb_modes.clone();
        let mode_label = widgets.mode_label.clone();
        let telemetry = widgets.telemetry.clone();
        let banner = widgets.banner.clone();
        let mode_buttons = widgets.mode_buttons.clone();
        let syncing = widgets.syncing.clone();
        let batt_syncing = widgets.batt_syncing.clone();
        let boot_syncing = widgets.boot_syncing.clone();
        worker::blocking(
            move || client.get_state(),
            move |result| {
                busy.set(false);
                match result {
                    Ok(s) => {
                        banner.set_revealed(false);
                        let w = SyncWidgets {
                            overdrive,
                            battery,
                            boot,
                            lb_enabled,
                            lb_modes,
                            kb_enabled,
                            kb_modes,
                            mode_label,
                            telemetry,
                            banner,
                            mode_buttons,
                            syncing,
                            batt_syncing,
                            boot_syncing,
                        };
                        apply_state_to_widgets(&state, &s, &w);
                    }
                    Err(_) => banner.set_revealed(true),
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
