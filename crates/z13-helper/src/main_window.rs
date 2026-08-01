use std::cell::Cell;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13_helper_core::label::mode_label;

use crate::{app::AppState, fans_window, worker};

pub fn build(state: &Rc<AppState>) -> adw::ApplicationWindow {
    let window = adw::ApplicationWindow::builder()
        .application(&state.app)
        .title("z13 Helper")
        .default_width(450)
        .default_height(720)
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

    let scroll = gtk::ScrolledWindow::new();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    scroll.set_child(Some(&content));
    toolbar.set_content(Some(&scroll));
    window.set_content(Some(&toolbar));

    let mode_label = gtk::Label::new(Some(&current_label(state)));
    mode_label.set_xalign(0.0);
    mode_label.add_css_class("title-3");
    content.append(&section("Performance Mode", &mode_label));
    let telemetry = gtk::Label::new(Some("APU: — °C    Fan: — RPM"));
    telemetry.set_xalign(0.0);
    content.append(&telemetry);
    let mode_grid = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .build();
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
        let state_click = state.clone();
        let label = mode_label.clone();
        button.connect_clicked(move |_| {
            select_profile(&state_click, id);
            label.set_label(&current_label(&state_click));
        });
        mode_grid.attach(&button, index as i32, 0, 1, 1);
    }
    let fans = gtk::Button::with_label("Fans + Power");
    let parent = window.clone();
    let state_fans = state.clone();
    fans.connect_clicked(move |_| fans_window::present(&state_fans, &parent));
    mode_grid.attach(&fans, 3, 0, 1, 1);
    content.append(&mode_grid);
    let custom = gtk::FlowBox::new();
    custom.set_selection_mode(gtk::SelectionMode::None);
    for profile in state.config.borrow().profiles.iter().filter(|p| !p.builtin) {
        let button = gtk::Button::with_label(&profile.name);
        let id = profile.id.clone();
        let state_click = state.clone();
        let label = mode_label.clone();
        button.connect_clicked(move |_| {
            select_profile(&state_click, &id);
            label.set_label(&current_label(&state_click));
        });
        custom.insert(&button, -1);
    }
    content.append(&custom);

    let display = adw::PreferencesGroup::builder().title("Display").build();
    let overdrive = gtk::Switch::new();
    let row = adw::ActionRow::builder().title("Panel overdrive").build();
    row.add_suffix(&overdrive);
    row.set_activatable_widget(Some(&overdrive));
    display.add(&row);
    let client = state.client.clone();
    overdrive.connect_state_set(move |_, enabled| {
        let c = client.clone();
        worker::blocking(move || c.panel_overdrive_set(i32::from(enabled)), |_| {});
        glib::Propagation::Proceed
    });
    content.append(&display);

    content.append(&lighting_group(state, "Lightbar", "lightbar"));
    content.append(&lighting_group(state, "Keyboard", "keyboard"));

    let battery = adw::PreferencesGroup::builder().title("Battery").build();
    let limit = gtk::Scale::with_range(gtk::Orientation::Horizontal, 40.0, 100.0, 5.0);
    limit.set_value(80.0);
    limit.set_hexpand(true);
    let battery_row = adw::ActionRow::builder().title("Charge limit").build();
    battery_row.add_suffix(&limit);
    battery.add(&battery_row);
    let full = gtk::Button::with_label("100%");
    battery.add(&full);
    install_battery_debounce(state, &limit);
    let limit_full = limit.clone();
    full.connect_clicked(move |_| limit_full.set_value(100.0));
    content.append(&battery);

    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let version = gtk::Label::new(Some(&format!("z13-helper {}", env!("CARGO_PKG_VERSION"))));
    version.set_hexpand(true);
    version.set_xalign(0.0);
    let boot = gtk::CheckButton::with_label("Boot sound");
    let client_boot = state.client.clone();
    boot.connect_toggled(move |b| {
        let c = client_boot.clone();
        let enabled = b.is_active();
        worker::blocking(move || c.boot_sound_set(i32::from(enabled)), |_| {});
    });
    let quit = gtk::Button::with_label("Quit");
    let app = state.app.clone();
    quit.connect_clicked(move |_| app.quit());
    footer.append(&version);
    footer.append(&boot);
    footer.append(&quit);
    content.append(&footer);

    install_telemetry(state, &window, &telemetry, &banner);
    window
}

fn section(title: &str, child: &impl IsA<gtk::Widget>) -> gtk::Box {
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 4);
    let label = gtk::Label::new(Some(title));
    label.add_css_class("heading");
    label.set_xalign(0.0);
    box_.append(&label);
    box_.append(child);
    box_
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
    // notify=false: button highlight is the feedback.
    state.apply_active(false);
}

fn lighting_group(
    state: &Rc<AppState>,
    title: &str,
    device: &'static str,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title(title).build();
    let modes = gtk::DropDown::from_strings(&["static", "breathe", "cycle", "rainbow", "strobe"]);
    #[allow(deprecated)]
    let color = gtk::ColorButton::new();
    let speed = gtk::DropDown::from_strings(&["slow", "normal", "fast"]);
    let enabled = gtk::Switch::new();
    let row = adw::ActionRow::builder().title("Effect").build();
    row.add_suffix(&enabled);
    row.add_suffix(&modes);
    row.add_suffix(&color);
    row.add_suffix(&speed);
    group.add(&row);
    let client = state.client.clone();
    let apply = move |on: bool, mode: String| {
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
    };
    let apply = Rc::new(apply);
    let effect = modes.clone();
    let callback = apply.clone();
    enabled.connect_state_set(move |switch, on| {
        callback(
            on,
            effect
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string())
                .unwrap_or_else(|| "static".into()),
        );
        switch.set_state(on);
        glib::Propagation::Stop
    });
    let color_vis = color.clone();
    let speed_vis = speed.clone();
    modes.connect_selected_notify(move |drop| {
        let mode = drop.selected();
        color_vis.set_visible(mode != 2 && mode != 3);
        speed_vis.set_visible(mode != 0);
    });
    group
}

fn install_battery_debounce(state: &Rc<AppState>, scale: &gtk::Scale) {
    let source = Rc::new(Cell::new(None::<glib::SourceId>));
    let client = state.client.clone();
    scale.connect_value_changed(move |scale| {
        if let Some(id) = source.take() {
            id.remove();
        }
        let client = client.clone();
        let value = scale.value() as i32;
        let source_done = source.clone();
        source.set(Some(glib::timeout_add_local_once(
            std::time::Duration::from_millis(200),
            move || {
                worker::blocking(move || client.battery_limit_set(value), |_| {});
                source_done.set(None);
            },
        )));
    });
}

fn install_telemetry(
    state: &Rc<AppState>,
    window: &adw::ApplicationWindow,
    label: &gtk::Label,
    banner: &adw::Banner,
) {
    let busy = Rc::new(Cell::new(false));
    let state = state.clone();
    let window = window.clone();
    let label = label.clone();
    let banner = banner.clone();
    glib::timeout_add_seconds_local(1, move || {
        if !window.is_visible() || busy.replace(true) {
            return glib::ControlFlow::Continue;
        }
        let client = state.client.clone();
        let busy = busy.clone();
        let label = label.clone();
        let banner = banner.clone();
        worker::blocking(
            move || client.get_state(),
            move |result| {
                busy.set(false);
                match result {
                    Ok(s) => {
                        banner.set_revealed(false);
                        label.set_label(&format!(
                            "APU: {} °C    Fan: {} RPM",
                            s.temperature.map_or("—".into(), |v| v.to_string()),
                            s.fan_rpm.map_or("—".into(), |v| v.to_string())
                        ));
                    }
                    Err(_) => banner.set_revealed(true),
                }
            },
        );
        glib::ControlFlow::Continue
    });
}
