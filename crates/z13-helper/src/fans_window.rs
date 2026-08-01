//! Fans + Power window: profile editor, power limits, undervolt, fan curve.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13_helper_core::apply::{apply_profile, ClientDaemon};
use z13_helper_core::{Base, Profile};

use crate::app::AppState;
use crate::curve_editor::CurveEditor;
use crate::worker;

const SLIDER_WIDTH: i32 = 220;

pub fn present(state: &Rc<AppState>, parent: &impl IsA<gtk::Window>) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .title("Fans + Power")
        .default_width(780)
        .default_height(560)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&gtk::Label::new(Some("Fans + Power"))));
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    let win_close = window.clone();
    close.connect_clicked(move |_| win_close.close());
    header.pack_end(&close);
    toolbar.add_top_bar(&header);

    // Escape closes the window.
    let key = gtk::EventControllerKey::new();
    let win_esc = window.clone();
    key.connect_key_pressed(move |_, keyval, _, _| {
        if keyval == gtk::gdk::Key::Escape {
            win_esc.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.set_size_request(300, -1);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.set_hexpand(true);
    right.set_margin_top(8);
    right.set_margin_bottom(12);
    right.set_margin_start(12);
    right.set_margin_end(12);

    let stack = gtk::Stack::new();
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));

    let profile = state
        .config
        .borrow()
        .active()
        .cloned()
        .unwrap_or_else(|| Profile::builtin("balanced", "Balanced", Base::Balanced));
    let editor = CurveEditor::new(profile.fan_curve, state.config.borrow().fan_clamp_to_grid);

    // Shared "currently editing" profile id for the editor.
    let editing_id = Rc::new(RefCell::new(profile.id.clone()));

    let cpu = build_cpu_page(state, &editor, &editing_id);
    let advanced = build_advanced_page(state, &editing_id);
    stack.add_titled(&cpu.0, Some("cpu"), "CPU");
    stack.add_titled(&advanced, Some("advanced"), "Advanced");

    left.append(&switcher);
    left.append(&stack);
    left.set_margin_top(4);
    left.set_margin_start(8);
    left.set_margin_bottom(8);

    // Right: profile selector + chart
    let sel_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let names: Vec<String> = state
        .config
        .borrow()
        .profiles
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let selector = gtk::DropDown::from_strings(&name_refs);
    let active = state.config.borrow().active_profile.clone();
    let selected = state
        .config
        .borrow()
        .profiles
        .iter()
        .position(|p| p.id == active)
        .unwrap_or(0);
    selector.set_selected(selected as u32);
    selector.set_hexpand(true);
    let plus = gtk::Button::with_label("+");
    let minus = gtk::Button::with_label("−");
    let rename = gtk::Button::with_label("Rename");
    sel_row.append(&selector);
    sel_row.append(&plus);
    sel_row.append(&rename);
    sel_row.append(&minus);
    right.append(&sel_row);

    let clamp = gtk::CheckButton::with_label("Clamp to Grid");
    clamp.set_active(state.config.borrow().fan_clamp_to_grid);
    let editor_clamp = editor.clone();
    let state_clamp = state.clone();
    clamp.connect_toggled(move |t| {
        let on = t.is_active();
        state_clamp.config.borrow_mut().fan_clamp_to_grid = on;
        editor_clamp.set_clamp_to_grid(on);
        state_clamp.save_config();
    });
    right.append(&clamp);

    let fan_toggle = gtk::CheckButton::with_label("Apply Custom Fan Curve");
    fan_toggle.set_active(profile.apply_fan_curve);
    editor.set_muted(!fan_toggle.is_active());
    right.append(&fan_toggle);

    let chart_label = gtk::Label::new(Some("Fan Curve (both fans) — % vs °C"));
    chart_label.add_css_class("dim-label");
    chart_label.set_xalign(0.0);
    right.append(&chart_label);
    right.append(editor.widget());

    let editor_muted = editor.clone();
    let state_fan = state.clone();
    let editing_fan = editing_id.clone();
    fan_toggle.connect_toggled(move |t| {
        editor_muted.set_muted(!t.is_active());
        let id = editing_fan.borrow().clone();
        if let Some(p) = state_fan.config.borrow_mut().find_mut(&id) {
            p.apply_fan_curve = t.is_active();
        }
    });

    // Persist curve edits into the profile currently selected in this window.
    let state_curve = state.clone();
    let editing_curve = editing_id.clone();
    editor.set_changed(move |curve| {
        let id = editing_curve.borrow().clone();
        if let Some(p) = state_curve.config.borrow_mut().find_mut(&id) {
            p.fan_curve = curve;
        }
    });

    // Switching profiles in the dropdown: save previous, load next into editors.
    let state_sel = state.clone();
    let editor_sel = editor.clone();
    let editing_sel = editing_id.clone();
    let fan_toggle_sel = fan_toggle.clone();
    let base_drop = cpu.1.clone();
    let spl = cpu.2.clone();
    let sppt = cpu.3.clone();
    let fppt = cpu.4.clone();
    let apply_power = cpu.5.clone();
    let loading = Rc::new(Cell::new(false));
    let loading_sel = loading.clone();
    selector.connect_selected_notify(move |drop| {
        if loading_sel.get() {
            return;
        }
        let idx = drop.selected() as usize;
        let profiles = state_sel.config.borrow().profiles.clone();
        let Some(next) = profiles.get(idx) else {
            return;
        };
        // Commit current curve into previous profile.
        {
            let prev = editing_sel.borrow().clone();
            let curve = editor_sel.curve();
            if let Some(p) = state_sel.config.borrow_mut().find_mut(&prev) {
                p.fan_curve = curve;
                p.apply_fan_curve = fan_toggle_sel.is_active();
            }
        }
        *editing_sel.borrow_mut() = next.id.clone();
        state_sel.config.borrow_mut().active_profile = next.id.clone();
        editor_sel.set_curve(next.fan_curve);
        editor_sel.set_muted(!next.apply_fan_curve);
        fan_toggle_sel.set_active(next.apply_fan_curve);
        loading_sel.set(true);
        base_drop.set_selected(match next.base {
            Base::Quiet => 0,
            Base::Balanced => 1,
            Base::Performance => 2,
        });
        spl.set_value(next.pl1_spl as f64);
        sppt.set_value(next.pl2_sppt as f64);
        fppt.set_value(next.fppt as f64);
        apply_power.set_active(next.apply_power_limits);
        loading_sel.set(false);
        state_sel.apply_active(false);
    });

    let state_add = state.clone();
    let selector_add = selector.clone();
    let loading_add = loading.clone();
    plus.connect_clicked(move |_| {
        state_add.config.borrow_mut().add_custom();
        state_add.save_config();
        let names: Vec<String> = state_add
            .config
            .borrow()
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let model = gtk::StringList::new(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        loading_add.set(true);
        selector_add.set_model(Some(&model));
        selector_add.set_selected((names.len() - 1) as u32);
        loading_add.set(false);
        // Trigger load via notify by toggling selected (already at end).
        selector_add.notify("selected");
    });

    let state_rm = state.clone();
    minus.connect_clicked(move |_| {
        let id = state_rm.config.borrow().active_profile.clone();
        if state_rm.config.borrow_mut().remove(&id) {
            state_rm.save_config();
        }
    });

    let state_ren = state.clone();
    rename.connect_clicked(move |_| {
        let id = state_ren.config.borrow().active_profile.clone();
        if state_ren
            .config
            .borrow()
            .find(&id)
            .map(|p| p.builtin)
            .unwrap_or(true)
        {
            return;
        }
        let name = state_ren
            .config
            .borrow()
            .find(&id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let new_name = if name.ends_with(" (edit)") {
            name
        } else {
            format!("{name} (edit)")
        };
        state_ren.config.borrow_mut().rename(&id, &new_name);
        state_ren.save_config();
    });

    root.append(&left);
    root.append(&right);
    toolbar.set_content(Some(&root));
    window.set_content(Some(&toolbar));
    window.present();
}

/// Returns (page, base_drop, spl, sppt, fppt, apply_power).
fn build_cpu_page(
    state: &Rc<AppState>,
    editor: &CurveEditor,
    editing_id: &Rc<RefCell<String>>,
) -> (
    gtk::Box,
    gtk::DropDown,
    gtk::Scale,
    gtk::Scale,
    gtk::Scale,
    gtk::CheckButton,
) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let base_group = adw::PreferencesGroup::builder()
        .title("Power Profile")
        .description("Also sets the matching power-profiles-daemon profile.")
        .build();
    let base_drop = gtk::DropDown::from_strings(&[
        Base::Quiet.short_label(),
        Base::Balanced.short_label(),
        Base::Performance.short_label(),
    ]);
    let base_idx = match state.config.borrow().active().map(|p| p.base) {
        Some(Base::Quiet) => 0,
        Some(Base::Performance) => 2,
        _ => 1,
    };
    base_drop.set_selected(base_idx);
    base_drop.set_size_request(SLIDER_WIDTH, -1);
    let base_row = adw::ActionRow::builder().title("Base").build();
    base_row.add_suffix(&base_drop);
    base_group.add(&base_row);
    page.append(&base_group);

    let power_group = adw::PreferencesGroup::builder()
        .title("Power Limits")
        .build();
    let apply_power = gtk::CheckButton::with_label("Apply Power Limits");
    let (pl1, pl2, pl3) = state
        .config
        .borrow()
        .active()
        .map(|p| (p.pl1_spl, p.pl2_sppt, p.fppt))
        .unwrap_or((52, 71, 70));
    apply_power.set_active(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.apply_power_limits)
            .unwrap_or(false),
    );
    power_group.add(&apply_power);

    let spl = slider_row("SPL (CPU sustained)", pl1, 5, 93);
    let sppt = slider_row("sPPT (CPU long boost)", pl2, 5, 93);
    let fppt = slider_row("fPPT (CPU short boost)", pl3, 5, 93);
    power_group.add(&spl.0);
    power_group.add(&sppt.0);
    power_group.add(&fppt.0);
    page.append(&power_group);

    let warning = gtk::Label::new(Some(
        "PL1 above 75 W requires force and clamps fans to an 80% floor.",
    ));
    warning.add_css_class("warning");
    warning.set_wrap(true);
    warning.set_visible(pl1 > 75);
    page.append(&warning);

    let warning_vis = warning.clone();
    spl.1.connect_value_changed(move |s| {
        warning_vis.set_visible(s.value() > 75.0);
    });
    install_ordering(&spl.1, &sppt.1, &fppt.1);

    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    page.append(&apply);

    let state_apply = state.clone();
    let editor = editor.clone();
    let apply_power_c = apply_power.clone();
    let base_drop_c = base_drop.clone();
    let s1 = spl.1.clone();
    let s2 = sppt.1.clone();
    let s3 = fppt.1.clone();
    let editing = editing_id.clone();
    apply.connect_clicked(move |button| {
        if state_apply.applying.replace(true) {
            return;
        }
        button.set_sensitive(false);
        {
            let mut cfg = state_apply.config.borrow_mut();
            let id = editing.borrow().clone();
            if let Some(p) = cfg.find_mut(&id) {
                p.base = match base_drop_c.selected() {
                    0 => Base::Quiet,
                    2 => Base::Performance,
                    _ => Base::Balanced,
                };
                let pl1 = s1.value() as u32;
                let mut pl2 = s2.value() as u32;
                let mut pl3 = s3.value() as u32;
                if pl2 < pl1 {
                    pl2 = pl1;
                }
                if pl3 < pl2 {
                    pl3 = pl2;
                }
                p.pl1_spl = pl1;
                p.pl2_sppt = pl2;
                p.fppt = pl3;
                p.apply_power_limits = apply_power_c.is_active();
                p.fan_curve = editor.curve();
            }
            cfg.active_profile = id;
        }
        let button = button.clone();
        let profile = state_apply.config.borrow().active().cloned();
        let client = state_apply.client.clone();
        let state_done = state_apply.clone();
        worker::blocking(
            move || {
                let available = client
                    .get_state()
                    .map(|s| s.undervolt_available)
                    .unwrap_or(false);
                profile.map(|p| apply_profile(&ClientDaemon(&client), &p, available))
            },
            move |result| {
                state_done.applying.set(false);
                button.set_sensitive(true);
                match result {
                    Some(Ok(())) => {
                        state_done.save_config();
                        state_done.clear_error();
                    }
                    Some(Err(error)) => state_done.report_error(&error.to_string()),
                    None => {}
                }
            },
        );
    });

    (page, base_drop, spl.1, sppt.1, fppt.1, apply_power)
}

fn build_advanced_page(state: &Rc<AppState>, editing_id: &Rc<RefCell<String>>) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let group = adw::PreferencesGroup::builder()
        .title("CPU Undervolt (Curve Optimizer)")
        .description(
            "Offset only takes effect while custom overrides are active. \
             The daemon reapplies it after sleep. Range 0 to −40.",
        )
        .build();

    let apply_uv = gtk::CheckButton::with_label("Apply Undervolt");
    apply_uv.set_active(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.apply_undervolt)
            .unwrap_or(false),
    );
    group.add(&apply_uv);

    // Full-width slider below the title — ActionRow suffixes crush scales.
    let uv = gtk::Scale::with_range(gtk::Orientation::Horizontal, -40.0, 0.0, 1.0);
    uv.set_draw_value(true);
    uv.set_value_pos(gtk::PositionType::Right);
    uv.set_digits(0);
    uv.set_hexpand(true);
    uv.set_size_request(SLIDER_WIDTH, -1);
    uv.set_margin_start(12);
    uv.set_margin_end(12);
    uv.set_margin_bottom(8);
    uv.set_value(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.cpu_co)
            .unwrap_or(0) as f64,
    );
    let uv_row = adw::ActionRow::builder()
        .title("All-core Curve Optimizer")
        .subtitle("Drag the slider below")
        .build();
    group.add(&uv_row);
    page.append(&group);
    page.append(&uv);

    let note = gtk::Label::new(Some(
        "If undervolt controls stay disabled, ryzen_smu (amkillam fork) is not loaded.",
    ));
    note.set_wrap(true);
    note.set_xalign(0.0);
    page.append(&note);

    let client = state.client.clone();
    let uv_c = uv.clone();
    let apply_c = apply_uv.clone();
    worker::blocking(
        move || {
            client
                .get_state()
                .map(|s| s.undervolt_available)
                .unwrap_or(false)
        },
        move |available| {
            uv_c.set_sensitive(available);
            apply_c.set_sensitive(available);
        },
    );

    let defaults = gtk::Button::with_label("Factory Defaults");
    page.append(&defaults);
    let state_def = state.clone();
    let editing = editing_id.clone();
    defaults.connect_clicked(move |_| {
        let id = editing.borrow().clone();
        if let Some(p) = state_def.config.borrow_mut().find_mut(&id) {
            p.factory_defaults();
        }
        state_def.save_config();
    });

    let state_uv = state.clone();
    let apply_uv_c = apply_uv.clone();
    let editing = editing_id.clone();
    uv.connect_value_changed(move |scale| {
        let id = editing.borrow().clone();
        if let Some(p) = state_uv.config.borrow_mut().find_mut(&id) {
            p.cpu_co = scale.value() as i32;
            p.apply_undervolt = apply_uv_c.is_active();
        }
    });
    let state_chk = state.clone();
    let editing = editing_id.clone();
    apply_uv.connect_toggled(move |chk| {
        let id = editing.borrow().clone();
        if let Some(p) = state_chk.config.borrow_mut().find_mut(&id) {
            p.apply_undervolt = chk.is_active();
        }
    });

    page
}

fn install_ordering(pl1: &gtk::Scale, pl2: &gtk::Scale, pl3: &gtk::Scale) {
    let pl2c = pl2.clone();
    let pl3c = pl3.clone();
    pl1.connect_value_changed(move |s| {
        if pl2c.value() < s.value() {
            pl2c.set_value(s.value());
        }
        if pl3c.value() < pl2c.value() {
            pl3c.set_value(pl2c.value());
        }
    });
    let pl1c = pl1.clone();
    let pl3c = pl3.clone();
    pl2.connect_value_changed(move |s| {
        if s.value() < pl1c.value() {
            s.set_value(pl1c.value());
        }
        if pl3c.value() < s.value() {
            pl3c.set_value(s.value());
        }
    });
    let pl2c = pl2.clone();
    pl3.connect_value_changed(move |s| {
        if s.value() < pl2c.value() {
            s.set_value(pl2c.value());
        }
    });
}

fn slider_row(title: &str, initial: u32, min: u32, max: u32) -> (adw::ActionRow, gtk::Scale) {
    let row = adw::ActionRow::builder().title(title).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min as f64, max as f64, 1.0);
    scale.set_value(initial as f64);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(0);
    scale.set_size_request(SLIDER_WIDTH, -1);
    scale.set_hexpand(false);
    scale.set_halign(gtk::Align::End);
    scale.set_valign(gtk::Align::Center);
    row.add_suffix(&scale);
    (row, scale)
}
