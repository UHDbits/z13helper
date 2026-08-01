//! Fans + Power window: profile editor, power limits, undervolt, fan curve.

use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13_helper_core::apply::{apply_profile, ClientDaemon};
use z13_helper_core::{Base, Profile};

use crate::app::AppState;
use crate::curve_editor::CurveEditor;
use crate::worker;

pub fn present(state: &Rc<AppState>, parent: &impl IsA<gtk::Window>) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .title("Fans + Power")
        .default_width(720)
        .default_height(580)
        .build();

    let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.set_size_request(280, -1);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.set_hexpand(true);
    right.set_margin_top(12);
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

    let cpu = build_cpu_page(state, &editor);
    let advanced = build_advanced_page(state);
    stack.add_titled(&cpu, Some("cpu"), "CPU");
    stack.add_titled(&advanced, Some("advanced"), "Advanced");

    left.append(&switcher);
    left.append(&stack);
    left.set_margin_top(8);
    left.set_margin_start(8);
    left.set_margin_bottom(8);

    // Right: profile selector + chart
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
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
    header.append(&selector);
    header.append(&plus);
    header.append(&rename);
    header.append(&minus);
    right.append(&header);

    let clamp = gtk::CheckButton::with_label("Clamp to Grid");
    clamp.set_active(state.config.borrow().fan_clamp_to_grid);
    right.append(&clamp);

    let fan_toggle = gtk::CheckButton::with_label("Apply Custom Fan Curve");
    fan_toggle.set_active(profile.apply_fan_curve);
    editor.set_muted(!fan_toggle.is_active());
    right.append(&fan_toggle);
    right.append(editor.widget());

    let editor_muted = editor.clone();
    fan_toggle.connect_toggled(move |t| {
        editor_muted.set_muted(!t.is_active());
    });

    let state_add = state.clone();
    let selector_add = selector.clone();
    plus.connect_clicked(move |_| {
        state_add.config.borrow_mut().add_custom();
        state_add.save_config();
        // Refresh is best-effort; user can reopen for a full refresh.
        let names: Vec<String> = state_add
            .config
            .borrow()
            .profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let model = gtk::StringList::new(&names.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        selector_add.set_model(Some(&model));
        selector_add.set_selected((names.len() - 1) as u32);
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
        // Simple rename: append " *" if not already customized — a dialog would
        // be nicer; keep it lightweight for now.
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
    window.set_content(Some(&root));
    window.present();
}

fn build_cpu_page(state: &Rc<AppState>, editor: &CurveEditor) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 10);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let base_group = adw::PreferencesGroup::builder()
        .title("Power Profile")
        .description("Sets both the ASUS platform profile and power-profiles-daemon.")
        .build();
    let base_drop = gtk::DropDown::from_strings(&[
        Base::Quiet.ppd_label(),
        Base::Balanced.ppd_label(),
        Base::Performance.ppd_label(),
    ]);
    let base_idx = match state.config.borrow().active().map(|p| p.base) {
        Some(Base::Quiet) => 0,
        Some(Base::Performance) => 2,
        _ => 1,
    };
    base_drop.set_selected(base_idx);
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

    // Enforce PL1 ≤ PL2 ≤ PL3 on release via button_release-ish change_value end.
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
    apply.connect_clicked(move |button| {
        if state_apply.applying.replace(true) {
            return;
        }
        button.set_sensitive(false);
        {
            let mut cfg = state_apply.config.borrow_mut();
            let id = cfg.active_profile.clone();
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

    page
}

fn build_advanced_page(state: &Rc<AppState>) -> gtk::Box {
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

    let uv = gtk::Scale::with_range(gtk::Orientation::Horizontal, -40.0, 0.0, 1.0);
    uv.set_draw_value(true);
    uv.set_value(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.cpu_co)
            .unwrap_or(0) as f64,
    );
    let row = adw::ActionRow::builder()
        .title("All-core Curve Optimizer")
        .build();
    row.add_suffix(&uv);
    group.add(&row);
    page.append(&group);

    let note = gtk::Label::new(Some(
        "If undervolt controls stay disabled, ryzen_smu (amkillam fork) is not loaded. \
         Ask the daemon via get-state — never probe the SMU yourself.",
    ));
    note.set_wrap(true);
    note.set_xalign(0.0);
    page.append(&note);

    // Probe availability once and gate the controls.
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
    defaults.connect_clicked(move |_| {
        let id = state_def.config.borrow().active_profile.clone();
        if let Some(p) = state_def.config.borrow_mut().find_mut(&id) {
            p.factory_defaults();
        }
        state_def.save_config();
    });

    // Persist undervolt edits into the active profile on change.
    let state_uv = state.clone();
    let apply_uv_c = apply_uv.clone();
    uv.connect_value_changed(move |scale| {
        let id = state_uv.config.borrow().active_profile.clone();
        if let Some(p) = state_uv.config.borrow_mut().find_mut(&id) {
            p.cpu_co = scale.value() as i32;
            p.apply_undervolt = apply_uv_c.is_active();
        }
    });
    let state_chk = state.clone();
    apply_uv.connect_toggled(move |chk| {
        let id = state_chk.config.borrow().active_profile.clone();
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
    scale.set_hexpand(true);
    // Avoid AddMark — causes GtkGizmo warnings (z13gui lesson).
    row.add_suffix(&scale);
    (row, scale)
}
