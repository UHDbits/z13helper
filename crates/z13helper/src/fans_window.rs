//! Fans + Power window: profile editor, power limits, undervolt, fan curve.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13helper_core::{Base, FanControlMode, Profile};

use crate::app::AppState;
use crate::services::worker;
use crate::ui::curve_editor::CurveEditor;
use crate::ui::sync::SyncGuard;

pub fn present(state: &Rc<AppState>, parent: &impl IsA<gtk::Window>) {
    let window = adw::Window::builder()
        .application(&state.app)
        .transient_for(parent)
        .title("Fans + Power")
        .default_width(920)
        .default_height(720)
        .build();

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    header.set_show_start_title_buttons(false);
    header.set_show_end_title_buttons(false);
    header.set_title_widget(Some(&gtk::Label::new(Some("Fans + Power"))));
    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("flat");
    close.set_tooltip_text(Some("Close Fans + Power"));
    close.update_property(&[gtk::accessible::Property::Label("Close Fans + Power")]);
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

    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.set_margin_top(12);
    root.set_margin_bottom(12);
    root.set_margin_start(12);
    root.set_margin_end(12);
    let toast_overlay = adw::ToastOverlay::new();
    state.register_toast_overlay(&toast_overlay);
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    body.set_vexpand(true);
    let left = gtk::Box::new(gtk::Orientation::Vertical, 8);
    left.set_size_request(300, -1);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.set_hexpand(true);

    let stack = gtk::Stack::new();
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));

    let profile = state
        .config
        .borrow()
        .active()
        .cloned()
        .unwrap_or_else(|| Profile::builtin("balanced", "Balanced", Base::Balanced));
    let editor = CurveEditor::new(
        profile.fan_curves[0],
        state.config.borrow().fan_clamp_to_grid,
        "Fan 1 curve",
    );
    let editor2 = CurveEditor::new(
        profile.fan_curves[1],
        state.config.borrow().fan_clamp_to_grid,
        "Fan 2 curve",
    );
    editor.set_floor_config(state.config.borrow().fan_floor);
    editor2.set_floor_config(state.config.borrow().fan_floor);

    // Shared "currently editing" profile id for the editor.
    let editing_id = Rc::new(RefCell::new(profile.id.clone()));
    let loading = SyncGuard::default();

    let cpu = build_cpu_page(state, &editing_id, &loading);
    let advanced = build_advanced_page(state, &editing_id, &loading);
    stack.add_titled(&cpu.0, Some("cpu"), "CPU");
    stack.add_titled(&advanced.0, Some("advanced"), "Advanced");

    left.append(&switcher);
    left.append(&stack);

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
    let plus = gtk::Button::with_label("Add");
    plus.set_tooltip_text(Some("Add profile"));
    plus.update_property(&[gtk::accessible::Property::Label("Add profile")]);
    let minus = gtk::Button::with_label("Remove");
    minus.set_tooltip_text(Some("Remove selected profile"));
    minus.update_property(&[gtk::accessible::Property::Label("Remove selected profile")]);
    let rename = gtk::Button::with_label("Rename");
    rename.set_tooltip_text(Some("Rename selected profile"));
    rename.update_property(&[gtk::accessible::Property::Label("Rename selected profile")]);
    set_profile_action_sensitivity(state, &profile.id, &rename, &minus);
    sel_row.append(&selector);
    sel_row.append(&plus);
    sel_row.append(&rename);
    sel_row.append(&minus);
    root.append(&sel_row);

    let restore = gtk::Button::with_label("Restore Factory Defaults");
    restore.set_tooltip_text(Some(
        "Reset Silent, Balanced, Turbo (or the selected custom) to stock power, fans, and undervolt.",
    ));

    let clamp = gtk::CheckButton::with_label("Clamp to grid");
    clamp.set_active(state.config.borrow().fan_clamp_to_grid);
    let editor_clamp = editor.clone();
    let editor2_clamp = editor2.clone();
    let state_clamp = state.clone();
    clamp.connect_toggled(move |t| {
        let on = t.is_active();
        state_clamp.config.borrow_mut().fan_clamp_to_grid = on;
        editor_clamp.set_clamp_to_grid(on);
        editor2_clamp.set_clamp_to_grid(on);
        state_clamp.save_config();
    });
    right.append(&clamp);

    let fan_toggle = gtk::CheckButton::with_label("Apply custom fan curve");
    fan_toggle.set_active(profile.apply_fan_curve);
    editor.set_muted(!fan_toggle.is_active());
    editor2.set_muted(!fan_toggle.is_active());
    right.append(&fan_toggle);

    let direct_toggle = gtk::CheckButton::with_label("Direct EC control");
    direct_toggle.set_active(profile.fan_control_mode == FanControlMode::Direct);
    direct_toggle.set_sensitive(false);
    direct_toggle.set_tooltip_text(Some("Handled by the privileged z13helperd system backend."));
    right.append(&direct_toggle);

    let direct_warning = gtk::Label::new(Some(
        "Direct mode bypasses the firmware fan-curve algorithm. z13helperd returns control \
         to firmware on sensor or EC errors.",
    ));
    direct_warning.add_css_class("warning");
    direct_warning.set_wrap(true);
    direct_warning.set_xalign(0.0);
    direct_warning.set_visible(direct_toggle.is_active());
    right.append(&direct_warning);

    let direct_status = gtk::Label::new(Some("Direct EC control: checking…"));
    direct_status.add_css_class("dim-label");
    direct_status.set_xalign(0.0);
    right.append(&direct_status);

    let manual_probe = state.client.clone();
    let direct_probe = direct_toggle.clone();
    let status_probe = direct_status.clone();
    let ppd_probe = cpu.6.clone();
    let state_probe = state.clone();
    let uv_probe = advanced.1.clone();
    let apply_uv_probe = advanced.2.clone();
    worker::blocking(
        move || manual_probe.get_state(),
        move |result| match result {
            Ok(status) => {
                state_probe
                    .undervolt_available
                    .set(Some(status.undervolt_available));
                uv_probe.set_sensitive(status.undervolt_available);
                apply_uv_probe.set_sensitive(status.undervolt_available);
                sync_ppd_choices(&ppd_probe, &status.capabilities.ppd_profiles, &state_probe);
                direct_probe.set_sensitive(status.capabilities.direct_fans);
                if !status.capabilities.direct_fans {
                    status_probe.set_label("Direct EC control is unavailable");
                    return;
                }
                let target = format!(
                    "{}%",
                    (i32::from(status.floor.effective_min_duty) * 100 + 127) / 255
                );
                let rpms = format!("{} / {}", status.fan_rpms[0], status.fan_rpms[1]);
                status_probe.set_label(&format!(
                    "Direct control: {:?} · floor {target} · RPM {rpms}",
                    status.floor.enforcement
                ));
            }
            Err(error) => {
                ppd_probe.set_sensitive(false);
                direct_probe.set_sensitive(false);
                status_probe.set_label(&format!("z13helperd unavailable: {error}"));
            }
        },
    );

    let chart_label = gtk::Label::new(Some("Fan 1 Curve — % vs °C"));
    chart_label.add_css_class("dim-label");
    chart_label.set_xalign(0.0);
    right.append(&chart_label);
    right.append(editor.widget());
    let chart2_label = gtk::Label::new(Some("Fan 2 Curve — % vs °C"));
    chart2_label.add_css_class("dim-label");
    chart2_label.set_xalign(0.0);
    right.append(&chart2_label);
    right.append(editor2.widget());

    let editor_muted = editor.clone();
    let editor2_muted = editor2.clone();
    let state_fan = state.clone();
    let editing_fan = editing_id.clone();
    let loading_fan = loading.clone();
    fan_toggle.connect_toggled(move |t| {
        if loading_fan.active() {
            return;
        }
        editor_muted.set_muted(!t.is_active());
        editor2_muted.set_muted(!t.is_active());
        let id = editing_fan.borrow().clone();
        if let Some(p) = state_fan.config.borrow_mut().find_mut(&id) {
            p.apply_fan_curve = t.is_active();
        }
    });

    let state_direct = state.clone();
    let editing_direct = editing_id.clone();
    let warning_direct = direct_warning.clone();
    let loading_direct = loading.clone();
    direct_toggle.connect_toggled(move |toggle| {
        if loading_direct.active() {
            return;
        }
        warning_direct.set_visible(toggle.is_active());
        let id = editing_direct.borrow().clone();
        if let Some(profile) = state_direct.config.borrow_mut().find_mut(&id) {
            profile.fan_control_mode = if toggle.is_active() {
                FanControlMode::Direct
            } else {
                FanControlMode::Firmware
            };
        }
    });

    // Persist curve edits into the profile currently selected in this window.
    let state_curve = state.clone();
    let editing_curve = editing_id.clone();
    let loading_curve = loading.clone();
    editor.set_changed(move |curve| {
        if loading_curve.active() {
            return;
        }
        let id = editing_curve.borrow().clone();
        if let Some(p) = state_curve.config.borrow_mut().find_mut(&id) {
            p.fan_curves[0] = curve;
        }
    });
    let state_curve = state.clone();
    let editing_curve = editing_id.clone();
    let loading_curve = loading.clone();
    editor2.set_changed(move |curve| {
        if loading_curve.active() {
            return;
        }
        let id = editing_curve.borrow().clone();
        if let Some(profile) = state_curve.config.borrow_mut().find_mut(&id) {
            profile.fan_curves[1] = curve;
        }
    });

    let base_drop = cpu.1.clone();
    let spl = cpu.2.clone();
    let sppt = cpu.3.clone();
    let fppt = cpu.4.clone();
    let apply_power = cpu.5.clone();
    let uv_scale = advanced.1.clone();
    let apply_uv = advanced.2.clone();
    let editors = Rc::new(ProfileEditorView {
        fans: FanEditorView {
            first: editor.clone(),
            second: editor2.clone(),
            enabled: fan_toggle.clone(),
            direct: direct_toggle.clone(),
            direct_explanation: direct_warning.clone(),
        },
        power: PowerEditorView {
            base: base_drop.clone(),
            spl: spl.clone(),
            sppt: sppt.clone(),
            fppt: fppt.clone(),
            enabled: apply_power.clone(),
            ppd: cpu.6.clone(),
        },
        undervolt: UndervoltEditorView {
            value: uv_scale.clone(),
            enabled: apply_uv.clone(),
        },
        loading: loading.clone(),
    });

    // Switching profiles in the dropdown: save previous, load next into editors.
    let state_sel = state.clone();
    let editor_sel = editor.clone();
    let editor2_sel = editor2.clone();
    let editing_sel = editing_id.clone();
    let fan_toggle_sel = fan_toggle.clone();
    let editors_sel = editors.clone();
    let loading_sel = loading.clone();
    let rename_sel = rename.clone();
    let remove_sel = minus.clone();
    selector.connect_selected_notify(move |drop| {
        if loading_sel.active() {
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
            let curve2 = editor2_sel.curve();
            if let Some(p) = state_sel.config.borrow_mut().find_mut(&prev) {
                p.fan_curves[0] = curve;
                p.fan_curves[1] = curve2;
                p.apply_fan_curve = fan_toggle_sel.is_active();
            }
        }
        *editing_sel.borrow_mut() = next.id.clone();
        state_sel.config.borrow_mut().active_profile = next.id.clone();
        editors_sel.load(next);
        set_profile_action_sensitivity(&state_sel, &next.id, &rename_sel, &remove_sel);
        state_sel.apply_active(false);
    });

    let state_def = state.clone();
    let editing_def = editing_id.clone();
    let editors_def = editors.clone();
    let restored_toast = toast_overlay.clone();
    let restore_factory = Rc::new(move || {
        let id = editing_def.borrow().clone();
        let restored = {
            let mut cfg = state_def.config.borrow_mut();
            if let Some(p) = cfg.find_mut(&id) {
                p.factory_defaults();
                Some(p.clone())
            } else {
                None
            }
        };
        if let Some(p) = restored {
            editors_def.load(&p);
            state_def.save_config();
            state_def.apply_active(false);
            restored_toast.add_toast(adw::Toast::new("Factory defaults restored"));
        }
    });
    let restore_click = restore_factory.clone();
    let restore_parent = window.clone();
    let restore_state = state.clone();
    restore.connect_clicked(move |_| {
        let name = restore_state
            .config
            .borrow()
            .active()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "selected profile".into());
        let body = format!("Reset “{name}” power, fan, and undervolt settings?");
        let dialog = adw::AlertDialog::new(Some("Restore Factory Defaults?"), Some(&body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("restore", "Restore");
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("restore", adw::ResponseAppearance::Destructive);
        let restore = restore_click.clone();
        dialog.connect_response(Some("restore"), move |_, _| restore());
        dialog.present(Some(&restore_parent));
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
        loading_add.run(|| {
            selector_add.set_model(Some(&model));
            selector_add.set_selected((names.len() - 1) as u32);
        });
        // Trigger load via notify by toggling selected (already at end).
        selector_add.notify("selected");
    });

    let state_rm = state.clone();
    let selector_rm = selector.clone();
    let editors_rm = editors.clone();
    let editing_rm = editing_id.clone();
    let loading_rm = loading.clone();
    let rename_rm = rename.clone();
    let remove_rm = minus.clone();
    let removed_toast = toast_overlay.clone();
    let remove_profile = Rc::new(move || {
        let id = state_rm.config.borrow().active_profile.clone();
        if state_rm.config.borrow_mut().remove(&id) {
            let profiles = state_rm.config.borrow().profiles.clone();
            let active = state_rm.config.borrow().active_profile.clone();
            let selected = profiles
                .iter()
                .position(|profile| profile.id == active)
                .unwrap_or(0);
            let names: Vec<String> = profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let model = gtk::StringList::new(&refs);
            loading_rm.run(|| {
                selector_rm.set_model(Some(&model));
                selector_rm.set_selected(selected as u32);
                if let Some(profile) = profiles.get(selected) {
                    *editing_rm.borrow_mut() = profile.id.clone();
                    editors_rm.load(profile);
                }
            });
            set_profile_action_sensitivity(&state_rm, &active, &rename_rm, &remove_rm);
            state_rm.save_config();
            state_rm.apply_active(false);
            removed_toast.add_toast(adw::Toast::new("Profile removed"));
        }
    });
    let remove_parent = window.clone();
    let remove_state = state.clone();
    minus.connect_clicked(move |_| {
        let name = remove_state
            .config
            .borrow()
            .active()
            .map(|profile| profile.name.clone())
            .unwrap_or_else(|| "selected profile".into());
        let body = format!("Permanently remove “{name}”?");
        let dialog = adw::AlertDialog::new(Some("Remove Profile?"), Some(&body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_close_response("cancel");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        let remove = remove_profile.clone();
        dialog.connect_response(Some("remove"), move |_, _| remove());
        dialog.present(Some(&remove_parent));
    });

    let state_ren = state.clone();
    let parent_ren = window.clone();
    let selector_ren = selector.clone();
    let loading_ren = loading.clone();
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
        let dialog = adw::AlertDialog::new(Some("Rename Profile"), None);
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("rename", "Rename");
        dialog.set_default_response(Some("rename"));
        let entry = gtk::Entry::new();
        entry.set_text(&name);
        entry.set_activates_default(true);
        entry.set_margin_top(12);
        entry.set_margin_bottom(12);
        entry.set_margin_start(12);
        entry.set_margin_end(12);
        dialog.set_extra_child(Some(&entry));
        let state = state_ren.clone();
        let selector = selector_ren.clone();
        let loading = loading_ren.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "rename" {
                let new_name = entry.text().trim().to_owned();
                if !new_name.is_empty() && state.config.borrow_mut().rename(&id, &new_name) {
                    state.save_config();
                    let names: Vec<String> = state
                        .config
                        .borrow()
                        .profiles
                        .iter()
                        .map(|profile| profile.name.clone())
                        .collect();
                    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
                    let model = gtk::StringList::new(&refs);
                    let selected = state
                        .config
                        .borrow()
                        .profiles
                        .iter()
                        .position(|profile| profile.id == id)
                        .unwrap_or(0) as u32;
                    loading.run(|| {
                        selector.set_model(Some(&model));
                        selector.set_selected(selected);
                    });
                }
            }
        });
        dialog.present(Some(&parent_ren));
    });

    let apply = gtk::Button::with_label("Apply Profile");
    apply.add_css_class("suggested-action");
    apply.set_hexpand(true);
    let state_apply = state.clone();
    apply.connect_clicked(move |_| state_apply.apply_active(false));
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(&restore);
    actions.append(&apply);
    body.append(&left);
    body.append(&right);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&body)
        .build();
    let adaptive_body = body.clone();
    let adaptive_left = left.clone();
    window.connect_notify_local(Some("width"), move |window, _| {
        let narrow = window.width() < 760;
        adaptive_body.set_orientation(if narrow {
            gtk::Orientation::Vertical
        } else {
            gtk::Orientation::Horizontal
        });
        adaptive_left.set_size_request(if narrow { -1 } else { 300 }, -1);
    });
    root.append(&scroller);
    root.append(&actions);
    toast_overlay.set_child(Some(&root));
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));
    window.present();
}

/// Returns (page, base_drop, spl, sppt, fppt, apply_power).
fn build_cpu_page(
    state: &Rc<AppState>,
    editing_id: &Rc<RefCell<String>>,
    loading: &SyncGuard,
) -> (
    gtk::Box,
    gtk::DropDown,
    gtk::Scale,
    gtk::Scale,
    gtk::Scale,
    gtk::CheckButton,
    gtk::DropDown,
) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let base_group = adw::PreferencesGroup::builder()
        .title("Power Profile")
        .description("Platform profile and power-profiles-daemon are independent.")
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
    base_drop.set_hexpand(true);
    let base_row = adw::ActionRow::builder().title("Base").build();
    base_row.add_suffix(&base_drop);
    base_group.add(&base_row);
    let ppd = gtk::DropDown::from_strings(&["power-saver", "balanced", "performance", "disabled"]);
    let ppd_selected = state
        .config
        .borrow()
        .active()
        .and_then(|profile| profile.ppd_profile.as_deref())
        .map(|profile| match profile {
            "power-saver" => 0,
            "performance" => 2,
            _ => 1,
        })
        .unwrap_or(3);
    ppd.set_selected(ppd_selected);
    let ppd_row = adw::ActionRow::builder()
        .title("Power Profiles Daemon")
        .build();
    ppd_row.add_suffix(&ppd);
    base_group.add(&ppd_row);
    page.append(&base_group);

    let power_group = adw::PreferencesGroup::builder()
        .title("Power Limits")
        .build();
    let apply_power = gtk::CheckButton::with_label("Apply power limits");
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

    let update = {
        let state = state.clone();
        let editing = editing_id.clone();
        let base = base_drop.clone();
        let ppd = ppd.clone();
        let power = apply_power.clone();
        let pl1 = spl.1.clone();
        let pl2 = sppt.1.clone();
        let pl3 = fppt.1.clone();
        let loading = loading.clone();
        Rc::new(move || {
            if loading.active() {
                return;
            }
            let id = editing.borrow().clone();
            if let Some(profile) = state.config.borrow_mut().find_mut(&id) {
                profile.base = match base.selected() {
                    0 => Base::Quiet,
                    2 => Base::Performance,
                    _ => Base::Balanced,
                };
                profile.ppd_profile = ppd
                    .selected_item()
                    .and_downcast::<gtk::StringObject>()
                    .map(|item| item.string().to_string())
                    .filter(|selection| selection != "disabled");
                profile.apply_power_limits = power.is_active();
                profile.pl1_spl = pl1.value() as u32;
                profile.pl2_sppt = pl2.value() as u32;
                profile.fppt = pl3.value() as u32;
            }
        })
    };
    for drop in [&base_drop, &ppd] {
        let update = update.clone();
        drop.connect_selected_notify(move |_| update());
    }
    let update_toggle = update.clone();
    apply_power.connect_toggled(move |_| update_toggle());
    for scale in [&spl.1, &sppt.1, &fppt.1] {
        let update = update.clone();
        scale.connect_value_changed(move |_| update());
    }

    (page, base_drop, spl.1, sppt.1, fppt.1, apply_power, ppd)
}

fn build_advanced_page(
    state: &Rc<AppState>,
    editing_id: &Rc<RefCell<String>>,
    loading: &SyncGuard,
) -> (gtk::Box, gtk::Scale, gtk::CheckButton) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
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

    let floor_group = adw::PreferencesGroup::builder()
        .title("High-Power Fan Floor")
        .description(
            "At sustained power above 75W, firmware curves are transformed on write. \
             Engage/release hysteresis and dwell are applied by direct EC control.",
        )
        .build();
    let floor = state.config.borrow().fan_floor;
    let engage = slider_row(
        "Engage temperature (°C)",
        floor.engage_temp_c as u32,
        60,
        70,
    );
    let release = slider_row(
        "Release temperature (°C)",
        floor.release_temp_c as u32,
        50,
        65,
    );
    let duty = slider_row("Minimum duty (PWM)", u32::from(floor.duty), 204, 255);
    let dwell = slider_row(
        "Minimum dwell (seconds)",
        floor.dwell_ms as u32 / 1000,
        5,
        30,
    );
    floor_group.add(&engage.0);
    floor_group.add(&release.0);
    floor_group.add(&duty.0);
    floor_group.add(&dwell.0);
    page.append(&floor_group);
    let update_floor = {
        let state = state.clone();
        let engage = engage.1.clone();
        let release = release.1.clone();
        let duty = duty.1.clone();
        let dwell = dwell.1.clone();
        Rc::new(move || {
            let engage_temp_c = engage.value() as i32;
            let max_release = engage_temp_c - 5;
            if release.value() as i32 > max_release {
                release.set_value(max_release as f64);
            }
            state.config.borrow_mut().fan_floor = z13helper_core::FanFloorConfig {
                engage_temp_c,
                release_temp_c: release.value() as i32,
                duty: duty.value() as u8,
                dwell_ms: dwell.value() as u64 * 1000,
            };
            state.save_config();
        })
    };
    for scale in [&engage.1, &release.1, &duty.1, &dwell.1] {
        let update = update_floor.clone();
        scale.connect_value_changed(move |_| update());
    }

    if let Some(available) = state.undervolt_available.get() {
        uv.set_sensitive(available);
        apply_uv.set_sensitive(available);
    }

    let state_uv = state.clone();
    let apply_uv_c = apply_uv.clone();
    let editing = editing_id.clone();
    let loading_uv = loading.clone();
    uv.connect_value_changed(move |scale| {
        if loading_uv.active() {
            return;
        }
        let id = editing.borrow().clone();
        if let Some(p) = state_uv.config.borrow_mut().find_mut(&id) {
            p.cpu_co = scale.value() as i32;
            p.apply_undervolt = apply_uv_c.is_active();
        }
    });
    let state_chk = state.clone();
    let editing = editing_id.clone();
    let loading_uv = loading.clone();
    apply_uv.connect_toggled(move |chk| {
        if loading_uv.active() {
            return;
        }
        let id = editing.borrow().clone();
        if let Some(p) = state_chk.config.borrow_mut().find_mut(&id) {
            p.apply_undervolt = chk.is_active();
        }
    });

    (page, uv, apply_uv)
}

struct ProfileEditorView {
    fans: FanEditorView,
    power: PowerEditorView,
    undervolt: UndervoltEditorView,
    loading: SyncGuard,
}

struct FanEditorView {
    first: CurveEditor,
    second: CurveEditor,
    enabled: gtk::CheckButton,
    direct: gtk::CheckButton,
    direct_explanation: gtk::Label,
}

struct PowerEditorView {
    base: gtk::DropDown,
    spl: gtk::Scale,
    sppt: gtk::Scale,
    fppt: gtk::Scale,
    enabled: gtk::CheckButton,
    ppd: gtk::DropDown,
}

struct UndervoltEditorView {
    value: gtk::Scale,
    enabled: gtk::CheckButton,
}

impl ProfileEditorView {
    fn load(&self, profile: &Profile) {
        self.loading.run(|| {
            self.fans.first.set_curve(profile.fan_curves[0]);
            self.fans.second.set_curve(profile.fan_curves[1]);
            self.fans.first.set_muted(!profile.apply_fan_curve);
            self.fans.second.set_muted(!profile.apply_fan_curve);
            self.fans.enabled.set_active(profile.apply_fan_curve);
            self.fans
                .direct
                .set_active(profile.fan_control_mode == FanControlMode::Direct);
            self.fans
                .direct_explanation
                .set_visible(profile.fan_control_mode == FanControlMode::Direct);
            self.power.base.set_selected(match profile.base {
                Base::Quiet => 0,
                Base::Balanced => 1,
                Base::Performance => 2,
            });
            self.power.spl.set_value(profile.pl1_spl as f64);
            self.power.sppt.set_value(profile.pl2_sppt as f64);
            self.power.fppt.set_value(profile.fppt as f64);
            self.power.enabled.set_active(profile.apply_power_limits);
            select_dropdown_string(
                &self.power.ppd,
                profile.ppd_profile.as_deref().unwrap_or("disabled"),
            );
            self.undervolt.value.set_value(profile.cpu_co as f64);
            self.undervolt.enabled.set_active(profile.apply_undervolt);
        });
    }
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

fn slider_row(title: &str, initial: u32, min: u32, max: u32) -> (gtk::Box, gtk::Scale) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    row.set_margin_start(12);
    row.set_margin_end(12);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    row.append(&label);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min as f64, max as f64, 1.0);
    scale.set_value(initial as f64);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(0);
    scale.set_hexpand(true);
    row.append(&scale);
    (row, scale)
}

fn sync_ppd_choices(dropdown: &gtk::DropDown, profiles: &[String], state: &AppState) {
    let mut choices = profiles.to_vec();
    choices.push("disabled".into());
    let references: Vec<&str> = choices.iter().map(String::as_str).collect();
    dropdown.set_model(Some(&gtk::StringList::new(&references)));
    let selected = state
        .config
        .borrow()
        .active()
        .and_then(|profile| profile.ppd_profile.as_ref())
        .and_then(|selected| profiles.iter().position(|profile| profile == selected))
        .unwrap_or(profiles.len());
    dropdown.set_selected(selected as u32);
    dropdown.set_sensitive(!profiles.is_empty());
}

fn select_dropdown_string(dropdown: &gtk::DropDown, target: &str) {
    let Some(model) = dropdown.model().and_downcast::<gtk::StringList>() else {
        return;
    };
    for index in 0..model.n_items() {
        if model.string(index).as_deref() == Some(target) {
            dropdown.set_selected(index);
            return;
        }
    }
}

fn set_profile_action_sensitivity(
    state: &AppState,
    profile_id: &str,
    rename: &gtk::Button,
    remove: &gtk::Button,
) {
    let editable = profile_actions_are_editable(&state.config.borrow(), profile_id);
    rename.set_sensitive(editable);
    remove.set_sensitive(editable);
}

fn profile_actions_are_editable(config: &z13helper_core::Config, profile_id: &str) -> bool {
    config
        .find(profile_id)
        .is_some_and(|profile| !profile.builtin)
}

#[cfg(test)]
mod tests {
    use super::profile_actions_are_editable;

    #[test]
    fn builtins_cannot_be_renamed_or_removed() {
        let mut config = z13helper_core::Config::default();
        assert!(!profile_actions_are_editable(&config, "silent"));
        let custom = config.add_custom().id.clone();
        assert!(profile_actions_are_editable(&config, &custom));
    }
}
