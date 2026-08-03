//! Fans + Power window: profile editor, power limits, undervolt, fan curve.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13helper_core::{
    stock_fan_curves, stock_ppt, ApplyRequest, FanControlMode, Profile, HIGH_POWER_THRESHOLD_W,
};

use crate::app::AppState;
use crate::services::worker;
use crate::ui::curve_editor::CurveEditor;
use crate::ui::sync::SyncGuard;

type ApplySchedule = Rc<RefCell<Option<glib::SourceId>>>;

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
    let left = gtk::Box::new(gtk::Orientation::Vertical, 8);
    left.add_css_class("card");
    left.set_margin_top(12);
    left.set_margin_bottom(12);
    left.set_margin_start(12);
    left.set_margin_end(12);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 8);
    right.set_hexpand(true);
    right.set_margin_top(12);
    right.set_margin_bottom(12);
    right.set_margin_start(12);
    right.set_margin_end(12);

    let stack = gtk::Stack::new();
    stack.set_vhomogeneous(false);
    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_margin_top(12);
    switcher.set_margin_start(12);
    switcher.set_margin_end(12);

    let profile = state
        .config
        .borrow()
        .active()
        .cloned()
        .unwrap_or_else(|| Profile::builtin("balanced", "Balanced"));
    let editor = CurveEditor::new(profile.fan_curves[0], "Fan 1 curve");
    let editor2 = CurveEditor::new(profile.fan_curves[1], "Fan 2 curve");
    let protection_disabled = state.config.borrow().disable_high_power_fan_protection;
    let protection_active = profile.apply_power_limits
        && profile.pl1_spl >= HIGH_POWER_THRESHOLD_W
        && !protection_disabled;
    editor.set_high_power_protection(protection_active);
    editor2.set_high_power_protection(protection_active);

    // Shared "currently editing" profile id for the editor.
    let editing_id = Rc::new(RefCell::new(profile.id.clone()));
    let loading = SyncGuard::default();
    let apply_schedule: ApplySchedule = Rc::new(RefCell::new(None));

    let cpu = build_cpu_page(
        state,
        &editing_id,
        &loading,
        &apply_schedule,
        &editor,
        &editor2,
    );
    let advanced = build_advanced_page(
        state,
        &editing_id,
        &loading,
        &apply_schedule,
        &window,
        &editor,
        &editor2,
    );
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
        "Reset Silent, Balanced, Turbo (or the selected custom) to stock power, fans, APU temperature, and undervolt.",
    ));

    let fan_toggle = gtk::CheckButton::with_label("Apply custom fan curve");
    fan_toggle.set_active(profile.apply_fan_curve);
    let unified_toggle = gtk::CheckButton::with_label("Unified fan control");
    unified_toggle.set_active(profile.unified_fan_control);
    unified_toggle.set_tooltip_text(Some(
        "Use the Fan 1 graph to control both fans with the same curve.",
    ));
    editor.set_muted(!fan_toggle.is_active());
    editor2.set_muted(!fan_toggle.is_active());
    editor.set_editable(fan_toggle.is_active());
    editor2.set_editable(fan_toggle.is_active() && !profile.unified_fan_control);
    editor2.widget().set_visible(!profile.unified_fan_control);
    right.append(&fan_toggle);
    right.append(&unified_toggle);

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

    let hysteresis_group = adw::PreferencesGroup::builder()
        .title("Direct EC Hysteresis")
        .description(
            "Directional temperature deadbands for direct EC control. Values 1–5 mirror \
             G-Helper and default to 3/3; firmware mode manages its own hysteresis.",
        )
        .build();
    let hysteresis_up = slider_row(
        "Fan speed-up hysteresis",
        u32::from(profile.fan_hysteresis_up),
        1,
        5,
    );
    let hysteresis_down = slider_row(
        "Fan slow-down hysteresis",
        u32::from(profile.fan_hysteresis_down),
        1,
        5,
    );
    hysteresis_up.1.set_sensitive(direct_toggle.is_active());
    hysteresis_down.1.set_sensitive(direct_toggle.is_active());
    hysteresis_group.add(&hysteresis_up.0);
    hysteresis_group.add(&hysteresis_down.0);
    right.append(&hysteresis_group);

    let direct_status = gtk::Label::new(Some("Direct EC control: checking…"));
    direct_status.add_css_class("dim-label");
    direct_status.set_xalign(0.0);
    right.append(&direct_status);

    let manual_probe = state.client.clone();
    let direct_probe = direct_toggle.clone();
    let status_probe = direct_status.clone();
    let ppd_probe = cpu.5.clone();
    let state_probe = state.clone();
    let hysteresis_up_probe = hysteresis_up.1.clone();
    let hysteresis_down_probe = hysteresis_down.1.clone();
    let uv_probe = advanced.1.clone();
    let apply_uv_probe = advanced.2.clone();
    let manual_uv_probe = advanced.3.clone();
    let undervolt_note_probe = advanced.4.clone();
    let cpu_temp_probe = advanced.5.clone();
    let loading_probe = loading.clone();
    worker::blocking(
        move || manual_probe.get_state(),
        move |result| match result {
            Ok(status) => {
                state_probe
                    .undervolt_available
                    .set(Some(status.undervolt_available));
                uv_probe.set_sensitive(status.undervolt_available);
                apply_uv_probe.set_sensitive(status.undervolt_available);
                manual_uv_probe.set_sensitive(status.undervolt_available);
                cpu_temp_probe.set_sensitive(status.undervolt_available);
                undervolt_note_probe.set_visible(!status.undervolt_available);
                sync_ppd_choices(
                    &ppd_probe,
                    &status.capabilities.ppd_profiles,
                    &state_probe,
                    &loading_probe,
                );
                direct_probe.set_sensitive(status.capabilities.direct_fans);
                hysteresis_up_probe.set_sensitive(
                    status.capabilities.direct_fans
                        && status.fan_control_mode == FanControlMode::Direct,
                );
                hysteresis_down_probe.set_sensitive(
                    status.capabilities.direct_fans
                        && status.fan_control_mode == FanControlMode::Direct,
                );
                if !status.capabilities.direct_fans {
                    status_probe.set_label("Direct EC control is unavailable");
                    return;
                }
                let duties = status
                    .direct_fan_duties
                    .map(|duty| (i32::from(duty) * 100 + 127) / 255);
                let rpms = format!("{} / {}", status.fan_rpms[0], status.fan_rpms[1]);
                if status.fan_control_mode == FanControlMode::Direct {
                    status_probe.set_label(&format!(
                        "Direct EC PWM: {}% / {}% · RPM {rpms}",
                        duties[0], duties[1]
                    ));
                } else {
                    status_probe.set_label(&format!("Firmware fan control · RPM {rpms}"));
                }
            }
            Err(error) => {
                ppd_probe.set_sensitive(false);
                direct_probe.set_sensitive(false);
                hysteresis_up_probe.set_sensitive(false);
                hysteresis_down_probe.set_sensitive(false);
                uv_probe.set_sensitive(false);
                apply_uv_probe.set_sensitive(false);
                manual_uv_probe.set_sensitive(false);
                cpu_temp_probe.set_sensitive(false);
                undervolt_note_probe.set_visible(false);
                status_probe.set_label(&format!("z13helperd unavailable: {error}"));
            }
        },
    );

    let chart_label = gtk::Label::new(Some("Fan 1 Curve — % vs °C"));
    chart_label.add_css_class("dim-label");
    chart_label.set_xalign(0.0);
    if profile.unified_fan_control {
        chart_label.set_label("Unified Fan Curve — % vs °C");
    }
    right.append(&chart_label);
    right.append(editor.widget());
    let chart2_label = gtk::Label::new(Some("Fan 2 Curve — % vs °C"));
    chart2_label.add_css_class("dim-label");
    chart2_label.set_xalign(0.0);
    chart2_label.set_visible(!profile.unified_fan_control);
    right.append(&chart2_label);
    right.append(editor2.widget());

    let editor_muted = editor.clone();
    let editor2_muted = editor2.clone();
    let unified_fan_toggle = unified_toggle.clone();
    let state_fan = state.clone();
    let editing_fan = editing_id.clone();
    let loading_fan = loading.clone();
    let apply_schedule_fan = apply_schedule.clone();
    fan_toggle.connect_toggled(move |t| {
        if loading_fan.active() {
            return;
        }
        let enabled = t.is_active();
        editor_muted.set_muted(!enabled);
        editor2_muted.set_muted(!enabled);
        editor_muted.set_editable(enabled);
        editor2_muted.set_editable(enabled && !unified_fan_toggle.is_active());
        let id = editing_fan.borrow().clone();
        if let Some(p) = state_fan.config.borrow_mut().find_mut(&id) {
            p.apply_fan_curve = t.is_active();
        }
        schedule_apply(&state_fan, &apply_schedule_fan);
    });

    let state_unified = state.clone();
    let editing_unified = editing_id.clone();
    let loading_unified = loading.clone();
    let apply_schedule_unified = apply_schedule.clone();
    let editor2_unified = editor2.clone();
    let chart_label_unified = chart_label.clone();
    let chart2_label_unified = chart2_label.clone();
    let fan_toggle_unified = fan_toggle.clone();
    unified_toggle.connect_toggled(move |toggle| {
        if loading_unified.active() {
            return;
        }
        let unified = toggle.is_active();
        let id = editing_unified.borrow().clone();
        let curve = state_unified
            .config
            .borrow_mut()
            .find_mut(&id)
            .map(|profile| {
                profile.unified_fan_control = unified;
                if unified {
                    profile.fan_curves[1] = profile.fan_curves[0];
                }
                profile.fan_curves[0]
            });
        if let Some(curve) = curve {
            if unified {
                editor2_unified.set_curve(curve);
            }
        }
        editor2_unified.widget().set_visible(!unified);
        editor2_unified.set_editable(fan_toggle_unified.is_active() && !unified);
        chart_label_unified.set_label(if unified {
            "Unified Fan Curve — % vs °C"
        } else {
            "Fan 1 Curve — % vs °C"
        });
        chart2_label_unified.set_visible(!unified);
        schedule_apply(&state_unified, &apply_schedule_unified);
    });

    let state_direct = state.clone();
    let editing_direct = editing_id.clone();
    let warning_direct = direct_warning.clone();
    let loading_direct = loading.clone();
    let apply_schedule_direct = apply_schedule.clone();
    let hysteresis_up_direct = hysteresis_up.1.clone();
    let hysteresis_down_direct = hysteresis_down.1.clone();
    direct_toggle.connect_toggled(move |toggle| {
        if loading_direct.active() {
            return;
        }
        warning_direct.set_visible(toggle.is_active());
        hysteresis_up_direct.set_sensitive(toggle.is_active());
        hysteresis_down_direct.set_sensitive(toggle.is_active());
        let id = editing_direct.borrow().clone();
        if let Some(profile) = state_direct.config.borrow_mut().find_mut(&id) {
            profile.fan_control_mode = if toggle.is_active() {
                FanControlMode::Direct
            } else {
                FanControlMode::Firmware
            };
        }
        schedule_apply(&state_direct, &apply_schedule_direct);
    });

    for (scale, upwards) in [(&hysteresis_up.1, true), (&hysteresis_down.1, false)] {
        let state = state.clone();
        let editing = editing_id.clone();
        let loading = loading.clone();
        let apply_schedule = apply_schedule.clone();
        scale.connect_value_changed(move |scale| {
            if loading.active() {
                return;
            }
            let id = editing.borrow().clone();
            if let Some(profile) = state.config.borrow_mut().find_mut(&id) {
                if upwards {
                    profile.fan_hysteresis_up = scale.value() as u8;
                } else {
                    profile.fan_hysteresis_down = scale.value() as u8;
                }
            }
            schedule_apply(&state, &apply_schedule);
        });
    }

    // Persist curve edits into the profile currently selected in this window.
    let state_curve = state.clone();
    let editing_curve = editing_id.clone();
    let loading_curve = loading.clone();
    let apply_schedule_curve = apply_schedule.clone();
    let editor2_curve = editor2.clone();
    editor.set_changed(move |curve| {
        if loading_curve.active() {
            return;
        }
        let id = editing_curve.borrow().clone();
        if let Some(p) = state_curve.config.borrow_mut().find_mut(&id) {
            p.fan_curves[0] = curve;
            if p.unified_fan_control {
                p.fan_curves[1] = curve;
                editor2_curve.set_curve(curve);
            }
        }
        schedule_apply(&state_curve, &apply_schedule_curve);
    });
    let state_curve = state.clone();
    let editing_curve = editing_id.clone();
    let loading_curve = loading.clone();
    let apply_schedule_curve = apply_schedule.clone();
    editor2.set_changed(move |curve| {
        if loading_curve.active() {
            return;
        }
        let id = editing_curve.borrow().clone();
        if let Some(profile) = state_curve.config.borrow_mut().find_mut(&id) {
            if profile.unified_fan_control {
                profile.fan_curves[0] = curve;
            }
            profile.fan_curves[1] = curve;
        }
        schedule_apply(&state_curve, &apply_schedule_curve);
    });

    let spl = cpu.1.clone();
    let sppt = cpu.2.clone();
    let fppt = cpu.3.clone();
    let apply_power = cpu.4.clone();
    let uv_scale = advanced.1.clone();
    let apply_uv = advanced.2.clone();
    let cpu_temp_limit = advanced.5.clone();
    let editors = Rc::new(ProfileEditorView {
        fans: FanEditorView {
            first: editor.clone(),
            second: editor2.clone(),
            enabled: fan_toggle.clone(),
            unified: unified_toggle.clone(),
            direct: direct_toggle.clone(),
            direct_explanation: direct_warning.clone(),
            hysteresis_up: hysteresis_up.1.clone(),
            hysteresis_down: hysteresis_down.1.clone(),
            chart_label: chart_label.clone(),
            chart2_label: chart2_label.clone(),
        },
        power: PowerEditorView {
            spl: spl.clone(),
            sppt: sppt.clone(),
            fppt: fppt.clone(),
            enabled: apply_power.clone(),
            ppd: cpu.5.clone(),
        },
        undervolt: UndervoltEditorView {
            value: uv_scale.clone(),
            enabled: apply_uv.clone(),
            cpu_temp_limit,
        },
        loading: loading.clone(),
    });

    // Switching profiles in the dropdown: save previous, load next into editors.
    let state_sel = state.clone();
    let editor_sel = editor.clone();
    let editor2_sel = editor2.clone();
    let editing_sel = editing_id.clone();
    let fan_toggle_sel = fan_toggle.clone();
    let unified_toggle_sel = unified_toggle.clone();
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
            let unified = unified_toggle_sel.is_active();
            let curve2 = if unified { curve } else { editor2_sel.curve() };
            if let Some(p) = state_sel.config.borrow_mut().find_mut(&prev) {
                p.fan_curves[0] = curve;
                p.fan_curves[1] = curve2;
                p.apply_fan_curve = fan_toggle_sel.is_active();
                p.unified_fan_control = unified;
            }
        }
        *editing_sel.borrow_mut() = next.id.clone();
        state_sel.config.borrow_mut().active_profile = next.id.clone();
        editors_sel.load(
            next,
            state_sel.config.borrow().disable_high_power_fan_protection,
        );
        set_profile_action_sensitivity(&state_sel, &next.id, &rename_sel, &remove_sel);
        state_sel.apply_active(false);
    });

    let state_def = state.clone();
    let editing_def = editing_id.clone();
    let editors_def = editors.clone();
    let restored_toast = toast_overlay.clone();
    let restore_factory = Rc::new(move || {
        let id = editing_def.borrow().clone();
        let Some(mut restored) = state_def.config.borrow().find(&id).cloned() else {
            return;
        };
        restored.factory_defaults();
        let ppd_profile = restored.ppd_profile.clone();
        let request = ApplyRequest::from_profile(
            &restored,
            state_def.config.borrow().disable_high_power_fan_protection,
        );
        let client = state_def.client.clone();
        let state_done = state_def.clone();
        let editors_done = editors_def.clone();
        let toast_done = restored_toast.clone();
        worker::blocking(
            move || {
                let apply = client.apply(request)?;
                let factory = ppd_profile.map(|ppd| client.factory_fan_curves(vec![ppd]));
                Ok::<_, z13helper_core::DaemonError>((restored, apply.warnings, factory))
            },
            move |result| match result {
                Ok((mut restored, mut warnings, factory)) => {
                    if let Some(factory) = factory {
                        match factory {
                            Ok(curves) => {
                                if let Some(ppd) = restored.ppd_profile.as_ref() {
                                    if let Some(curve) = curves.get(ppd) {
                                        restored.fan_curves = *curve;
                                        restored.factory_fan_curves_loaded = true;
                                    } else {
                                        warnings.push(format!(
                                            "firmware returned no factory fan curves for {ppd}"
                                        ));
                                    }
                                }
                            }
                            Err(error) => warnings.push(format!(
                                "firmware factory fan curves were unavailable; using bundled defaults: {error}"
                            )),
                        }
                    }
                    if let Some(profile) = state_done.config.borrow_mut().find_mut(&id) {
                        *profile = restored.clone();
                    }
                    editors_done.load(
                        &restored,
                        state_done.config.borrow().disable_high_power_fan_protection,
                    );
                    state_done.save_config();
                    toast_done.add_toast(adw::Toast::new("Factory defaults restored"));
                    if !warnings.is_empty() {
                        state_done.report_error(&warnings.join(" · "));
                    }
                }
                Err(error) => {
                    state_done.report_error(&format!("Could not restore factory defaults: {error}"))
                }
            },
        );
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
        let body = format!("Reset “{name}” power, fan, APU temperature, and undervolt settings?");
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
                    editors_rm.load(
                        profile,
                        state_rm.config.borrow().disable_high_power_fan_protection,
                    );
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

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.append(&restore);
    let left_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(&left)
        .build();
    let right_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .hexpand(true)
        .vexpand(true)
        .child(&right)
        .build();
    let sidebar_page = adw::NavigationPage::new(&left_scroll, "Power & Undervolt");
    let content_page = adw::NavigationPage::new(&right_scroll, "Fan Curves & Controls");
    let split_view = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .content(&content_page)
        .min_sidebar_width(300.0)
        .max_sidebar_width(380.0)
        .sidebar_width_fraction(0.36)
        .build();
    split_view.add_css_class("fans-split-view");
    split_view.set_vexpand(true);
    root.append(&split_view);
    root.append(&actions);
    toast_overlay.set_child(Some(&root));
    toolbar.set_content(Some(&toast_overlay));
    window.set_content(Some(&toolbar));
    window.present();
}

/// Returns (page, spl, sppt, fppt, apply_power, ppd).
fn build_cpu_page(
    state: &Rc<AppState>,
    editing_id: &Rc<RefCell<String>>,
    loading: &SyncGuard,
    apply_schedule: &ApplySchedule,
    first_editor: &CurveEditor,
    second_editor: &CurveEditor,
) -> (
    gtk::Box,
    gtk::Scale,
    gtk::Scale,
    gtk::Scale,
    gtk::CheckButton,
    adw::ComboRow,
) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let ppd_group = adw::PreferencesGroup::builder()
        .title("Power Profile")
        .build();
    let ppd = adw::ComboRow::new();
    ppd.set_title("PPD Profile");
    ppd.set_model(Some(&gtk::StringList::new(&[
        "power-saver",
        "balanced",
        "performance",
        "disabled",
    ])));
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
    ppd_group.add(&ppd);
    page.append(&ppd_group);

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

    let spl = sidebar_slider_row("SPL (CPU sustained)", pl1, 5, 93);
    let sppt = sidebar_slider_row("sPPT (CPU long boost)", pl2, 5, 93);
    let fppt = sidebar_slider_row("fPPT (CPU short boost)", pl3, 5, 120);
    power_group.add(&spl.0);
    power_group.add(&sppt.0);
    power_group.add(&fppt.0);
    power_group.add(&apply_power);
    spl.1.set_sensitive(apply_power.is_active());
    sppt.1.set_sensitive(apply_power.is_active());
    fppt.1.set_sensitive(apply_power.is_active());
    page.append(&power_group);

    let warning = gtk::Label::new(Some(
        "At 80 W and above, the final points are protected at 80°C / at least 80% and 90°C / 100%.",
    ));
    warning.add_css_class("warning");
    warning.set_wrap(true);
    warning.set_visible(pl1 >= HIGH_POWER_THRESHOLD_W);
    page.append(&warning);

    let warning_vis = warning.clone();
    spl.1.connect_value_changed(move |s| {
        warning_vis.set_visible(s.value() >= f64::from(HIGH_POWER_THRESHOLD_W));
    });
    install_ordering(&spl.1, &sppt.1, &fppt.1, loading);

    let update = {
        let state = state.clone();
        let editing = editing_id.clone();
        let ppd = ppd.clone();
        let power = apply_power.clone();
        let pl1 = spl.1.clone();
        let pl2 = sppt.1.clone();
        let pl3 = fppt.1.clone();
        let loading = loading.clone();
        let apply_schedule = apply_schedule.clone();
        let first_editor = first_editor.clone();
        let second_editor = second_editor.clone();
        Rc::new(move || {
            if loading.active() {
                return;
            }
            let id = editing.borrow().clone();
            let high_power_disabled = state.config.borrow().disable_high_power_fan_protection;
            if let Some(profile) = state.config.borrow_mut().find_mut(&id) {
                let ppd_profile = ppd
                    .selected_item()
                    .and_downcast::<gtk::StringObject>()
                    .map(|item| item.string().to_string())
                    .filter(|selection| selection != "disabled");
                if profile.ppd_profile != ppd_profile {
                    profile.ppd_profile = ppd_profile;
                    profile.factory_fan_curves_loaded = false;
                    if !profile.apply_fan_curve {
                        profile.fan_curves = stock_fan_curves(profile.ppd_profile.as_deref());
                    }
                    if !profile.apply_power_limits {
                        let (stock_pl1, stock_pl2, stock_fppt) =
                            stock_ppt(profile.ppd_profile.as_deref());
                        profile.pl1_spl = stock_pl1;
                        profile.pl2_sppt = stock_pl2;
                        profile.fppt = stock_fppt;
                        loading.run(|| {
                            pl1.set_value(profile.pl1_spl as f64);
                            pl2.set_value(profile.pl2_sppt as f64);
                            pl3.set_value(profile.fppt as f64);
                        });
                    }
                }
                profile.apply_power_limits = power.is_active();
                profile.pl1_spl = pl1.value() as u32;
                profile.pl2_sppt = pl2.value() as u32;
                profile.fppt = pl3.value() as u32;
                let protection = profile.apply_power_limits
                    && profile.pl1_spl >= HIGH_POWER_THRESHOLD_W
                    && !high_power_disabled;
                first_editor.set_high_power_protection(protection);
                second_editor.set_high_power_protection(protection);
                schedule_apply(&state, &apply_schedule);
            }
        })
    };
    let update_ppd = update.clone();
    ppd.connect_selected_notify(move |_| update_ppd());
    let update_toggle = update.clone();
    let spl_toggle = spl.1.clone();
    let sppt_toggle = sppt.1.clone();
    let fppt_toggle = fppt.1.clone();
    apply_power.connect_toggled(move |toggle| {
        let enabled = toggle.is_active();
        if enabled {
            sppt_toggle.set_value(sppt_toggle.value().max(spl_toggle.value()));
            fppt_toggle.set_value(fppt_toggle.value().max(sppt_toggle.value()));
        }
        spl_toggle.set_sensitive(enabled);
        sppt_toggle.set_sensitive(enabled);
        fppt_toggle.set_sensitive(enabled);
        update_toggle();
    });
    for scale in [&spl.1, &sppt.1, &fppt.1] {
        let update = update.clone();
        scale.connect_value_changed(move |_| update());
    }

    (page, spl.1, sppt.1, fppt.1, apply_power, ppd)
}

fn build_advanced_page(
    state: &Rc<AppState>,
    editing_id: &Rc<RefCell<String>>,
    loading: &SyncGuard,
    apply_schedule: &ApplySchedule,
    parent: &adw::Window,
    first_editor: &CurveEditor,
    second_editor: &CurveEditor,
) -> (
    gtk::Box,
    gtk::Scale,
    gtk::CheckButton,
    gtk::Button,
    gtk::Label,
    gtk::Scale,
) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 12);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let group = adw::PreferencesGroup::builder()
        .title("CPU Undervolt (Curve Optimizer)")
        .build();

    let apply_uv = gtk::CheckButton::with_label("Auto Apply");
    apply_uv.set_active(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.apply_undervolt)
            .unwrap_or(false),
    );

    // Full-width slider below the title — ActionRow suffixes crush scales.
    let uv = gtk::Scale::with_range(gtk::Orientation::Horizontal, -40.0, 0.0, 1.0);
    uv.set_draw_value(true);
    uv.set_value_pos(gtk::PositionType::Right);
    uv.set_digits(0);
    uv.add_css_class("undervolt-scale");
    uv.set_hexpand(true);
    uv.set_margin_start(0);
    uv.set_margin_end(0);
    uv.set_value(
        state
            .config
            .borrow()
            .active()
            .map(|p| p.cpu_co)
            .unwrap_or(0) as f64,
    );
    page.append(&group);
    page.append(&uv);

    let uv_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    uv_actions.set_margin_start(12);
    uv_actions.set_margin_end(12);
    uv_actions.set_hexpand(true);
    uv_actions.set_homogeneous(true);
    let manual_apply = gtk::Button::with_label("Apply");
    manual_apply.set_halign(gtk::Align::Start);
    manual_apply.set_tooltip_text(Some(
        "Test this Curve Optimizer offset once without enabling Auto Apply.",
    ));
    manual_apply.update_property(&[gtk::accessible::Property::Label("Apply undervolt once")]);
    apply_uv.set_halign(gtk::Align::End);
    uv_actions.append(&manual_apply);
    uv_actions.append(&apply_uv);
    page.append(&uv_actions);

    let note = gtk::Label::new(Some("Disabled because ryzen_smu is not loaded."));
    note.set_wrap(true);
    note.set_xalign(0.0);
    note.set_visible(false);
    page.append(&note);

    let temperature_group = adw::PreferencesGroup::builder()
        .title("APU Temperature Limit")
        .build();
    let cpu_temp_limit = gtk::Scale::with_range(gtk::Orientation::Horizontal, 80.0, 99.0, 1.0);
    cpu_temp_limit.set_value(
        state
            .config
            .borrow()
            .active()
            .map(|profile| f64::from(profile.cpu_temp_limit))
            .unwrap_or(95.0),
    );
    cpu_temp_limit.set_draw_value(true);
    cpu_temp_limit.set_value_pos(gtk::PositionType::Right);
    cpu_temp_limit.set_digits(0);
    cpu_temp_limit.add_css_class("undervolt-scale");
    cpu_temp_limit.set_hexpand(true);
    cpu_temp_limit.set_margin_start(0);
    cpu_temp_limit.set_margin_end(0);
    page.append(&temperature_group);
    page.append(&cpu_temp_limit);

    let protection_group = adw::PreferencesGroup::builder()
        .title("High-Power Fan Protection")
        .description(
            "At 80 W and above, the fan is locked at higher speeds at 80°C or above. \
             Disabling this removes that safety constraint.",
        )
        .build();
    let disable_protection = gtk::CheckButton::with_label("Disable high-power fan protection");
    disable_protection.set_active(state.config.borrow().disable_high_power_fan_protection);
    protection_group.add(&disable_protection);
    page.append(&protection_group);

    let state_protection = state.clone();
    let loading_protection = loading.clone();
    let apply_schedule_protection = apply_schedule.clone();
    let parent_protection = parent.clone();
    let first_protection = first_editor.clone();
    let second_protection = second_editor.clone();
    disable_protection.connect_toggled(move |toggle| {
        if loading_protection.active() {
            return;
        }
        if toggle.is_active() {
            loading_protection.run(|| toggle.set_active(false));
            let dialog = adw::AlertDialog::new(
                Some("Disable High-Power Fan Protection?"),
                Some(
                    "This allows fan curves below the protected 80°C / 80% and 90°C / 100% \
                     endpoints while PL1 is 80 W or above. This can increase thermal risk.",
                ),
            );
            dialog.add_response("cancel", "Cancel");
            dialog.add_response("disable", "Disable Protection");
            dialog.set_close_response("cancel");
            dialog.set_response_appearance("disable", adw::ResponseAppearance::Destructive);
            let toggle = toggle.clone();
            let state = state_protection.clone();
            let schedule = apply_schedule_protection.clone();
            let loading = loading_protection.clone();
            let first = first_protection.clone();
            let second = second_protection.clone();
            dialog.connect_response(Some("disable"), move |_, _| {
                state.config.borrow_mut().disable_high_power_fan_protection = true;
                loading.run(|| toggle.set_active(true));
                first.set_high_power_protection(false);
                second.set_high_power_protection(false);
                schedule_apply(&state, &schedule);
            });
            dialog.present(Some(&parent_protection));
        } else {
            state_protection
                .config
                .borrow_mut()
                .disable_high_power_fan_protection = false;
            let active = state_protection
                .config
                .borrow()
                .active()
                .is_some_and(|profile| {
                    profile.apply_power_limits && profile.pl1_spl >= HIGH_POWER_THRESHOLD_W
                });
            first_protection.set_high_power_protection(active);
            second_protection.set_high_power_protection(active);
            schedule_apply(&state_protection, &apply_schedule_protection);
        }
    });

    if let Some(available) = state.undervolt_available.get() {
        uv.set_sensitive(available);
        apply_uv.set_sensitive(available);
        manual_apply.set_sensitive(available);
        cpu_temp_limit.set_sensitive(available);
        note.set_visible(!available);
    }

    let state_temperature = state.clone();
    let editing_temperature = editing_id.clone();
    let loading_temperature = loading.clone();
    let apply_schedule_temperature = apply_schedule.clone();
    cpu_temp_limit.connect_value_changed(move |scale| {
        if loading_temperature.active() {
            return;
        }
        let id = editing_temperature.borrow().clone();
        if let Some(profile) = state_temperature.config.borrow_mut().find_mut(&id) {
            profile.cpu_temp_limit = scale.value() as u8;
        }
        schedule_apply(&state_temperature, &apply_schedule_temperature);
    });

    let state_uv = state.clone();
    let apply_uv_c = apply_uv.clone();
    let editing = editing_id.clone();
    let loading_uv = loading.clone();
    let apply_schedule_uv = apply_schedule.clone();
    uv.connect_value_changed(move |scale| {
        if loading_uv.active() {
            return;
        }
        let id = editing.borrow().clone();
        if let Some(p) = state_uv.config.borrow_mut().find_mut(&id) {
            p.cpu_co = scale.value() as i32;
            p.apply_undervolt = apply_uv_c.is_active();
        }
        if apply_uv_c.is_active() {
            schedule_apply(&state_uv, &apply_schedule_uv);
        } else {
            state_uv.save_config();
        }
    });
    let state_chk = state.clone();
    let editing = editing_id.clone();
    let loading_uv = loading.clone();
    let apply_schedule_uv = apply_schedule.clone();
    apply_uv.connect_toggled(move |chk| {
        if loading_uv.active() {
            return;
        }
        let id = editing.borrow().clone();
        if let Some(p) = state_chk.config.borrow_mut().find_mut(&id) {
            p.apply_undervolt = chk.is_active();
        }
        schedule_apply(&state_chk, &apply_schedule_uv);
    });

    let state_manual = state.clone();
    let editing_manual = editing_id.clone();
    let loading_manual = loading.clone();
    manual_apply.connect_clicked(move |button| {
        if loading_manual.active() {
            return;
        }
        let id = editing_manual.borrow().clone();
        let Some(offset) = state_manual
            .config
            .borrow()
            .find(&id)
            .map(|profile| profile.cpu_co)
        else {
            return;
        };
        button.set_sensitive(false);
        let button_done = button.clone();
        let client = state_manual.client.clone();
        let feedback = state_manual.clone();
        worker::blocking(
            move || client.apply_undervolt_once(offset),
            move |result| {
                button_done.set_sensitive(feedback.undervolt_available.get().unwrap_or(true));
                if let Err(error) = result {
                    feedback.report_error(&format!("Manual undervolt apply failed: {error}"));
                }
            },
        );
    });

    (page, uv, apply_uv, manual_apply, note, cpu_temp_limit)
}

fn schedule_apply(state: &Rc<AppState>, schedule: &ApplySchedule) {
    if let Some(source) = schedule.borrow_mut().take() {
        source.remove();
    }
    let state = state.clone();
    let schedule_done = schedule.clone();
    *schedule.borrow_mut() = Some(glib::timeout_add_local_once(
        std::time::Duration::from_millis(200),
        move || {
            schedule_done.borrow_mut().take();
            state.save_config();
            state.apply_active(false);
        },
    ));
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
    unified: gtk::CheckButton,
    direct: gtk::CheckButton,
    direct_explanation: gtk::Label,
    hysteresis_up: gtk::Scale,
    hysteresis_down: gtk::Scale,
    chart_label: gtk::Label,
    chart2_label: gtk::Label,
}

struct PowerEditorView {
    spl: gtk::Scale,
    sppt: gtk::Scale,
    fppt: gtk::Scale,
    enabled: gtk::CheckButton,
    ppd: adw::ComboRow,
}

struct UndervoltEditorView {
    value: gtk::Scale,
    enabled: gtk::CheckButton,
    cpu_temp_limit: gtk::Scale,
}

impl ProfileEditorView {
    fn load(&self, profile: &Profile, disable_high_power_fan_protection: bool) {
        self.loading.run(|| {
            self.fans.first.set_curve(profile.fan_curves[0]);
            self.fans.second.set_curve(if profile.unified_fan_control {
                profile.fan_curves[0]
            } else {
                profile.fan_curves[1]
            });
            let high_power = profile.apply_power_limits
                && profile.pl1_spl >= HIGH_POWER_THRESHOLD_W
                && !disable_high_power_fan_protection;
            self.fans.first.set_high_power_protection(high_power);
            self.fans.second.set_high_power_protection(high_power);
            self.fans.first.set_muted(!profile.apply_fan_curve);
            self.fans.second.set_muted(!profile.apply_fan_curve);
            self.fans.first.set_editable(profile.apply_fan_curve);
            self.fans
                .second
                .set_editable(profile.apply_fan_curve && !profile.unified_fan_control);
            self.fans
                .second
                .widget()
                .set_visible(!profile.unified_fan_control);
            self.fans.enabled.set_active(profile.apply_fan_curve);
            self.fans.unified.set_active(profile.unified_fan_control);
            self.fans
                .chart_label
                .set_label(if profile.unified_fan_control {
                    "Unified Fan Curve — % vs °C"
                } else {
                    "Fan 1 Curve — % vs °C"
                });
            self.fans
                .chart2_label
                .set_visible(!profile.unified_fan_control);
            self.fans
                .direct
                .set_active(profile.fan_control_mode == FanControlMode::Direct);
            self.fans
                .direct_explanation
                .set_visible(profile.fan_control_mode == FanControlMode::Direct);
            self.fans
                .hysteresis_up
                .set_value(f64::from(profile.fan_hysteresis_up));
            self.fans
                .hysteresis_down
                .set_value(f64::from(profile.fan_hysteresis_down));
            self.fans
                .hysteresis_up
                .set_sensitive(profile.fan_control_mode == FanControlMode::Direct);
            self.fans
                .hysteresis_down
                .set_sensitive(profile.fan_control_mode == FanControlMode::Direct);
            self.power.spl.set_value(profile.pl1_spl as f64);
            self.power.sppt.set_value(profile.pl2_sppt as f64);
            self.power.fppt.set_value(profile.fppt as f64);
            self.power.enabled.set_active(profile.apply_power_limits);
            self.power.spl.set_sensitive(profile.apply_power_limits);
            self.power.sppt.set_sensitive(profile.apply_power_limits);
            self.power.fppt.set_sensitive(profile.apply_power_limits);
            select_dropdown_string(
                &self.power.ppd,
                profile.ppd_profile.as_deref().unwrap_or("disabled"),
            );
            self.undervolt.value.set_value(profile.cpu_co as f64);
            self.undervolt.enabled.set_active(profile.apply_undervolt);
            self.undervolt
                .cpu_temp_limit
                .set_value(f64::from(profile.cpu_temp_limit));
        });
    }
}

fn install_ordering(pl1: &gtk::Scale, pl2: &gtk::Scale, pl3: &gtk::Scale, loading: &SyncGuard) {
    let pl2c = pl2.clone();
    let pl3c = pl3.clone();
    let loading_pl1 = loading.clone();
    pl1.connect_value_changed(move |s| {
        if loading_pl1.active() {
            return;
        }
        if pl2c.value() < s.value() {
            pl2c.set_value(s.value());
        }
        if pl3c.value() < pl2c.value() {
            pl3c.set_value(pl2c.value());
        }
    });
    let pl1c = pl1.clone();
    let pl3c = pl3.clone();
    let loading_pl2 = loading.clone();
    pl2.connect_value_changed(move |s| {
        if loading_pl2.active() {
            return;
        }
        if s.value() < pl1c.value() {
            s.set_value(pl1c.value());
        }
        if pl3c.value() < s.value() {
            pl3c.set_value(s.value());
        }
    });
    let pl2c = pl2.clone();
    let loading_pl3 = loading.clone();
    pl3.connect_value_changed(move |s| {
        if loading_pl3.active() {
            return;
        }
        if s.value() < pl2c.value() {
            s.set_value(pl2c.value());
        }
    });
}

fn slider_row(title: &str, initial: u32, min: u32, max: u32) -> (gtk::Box, gtk::Scale) {
    slider_row_with_margin(title, initial, min, max, 12)
}

fn sidebar_slider_row(title: &str, initial: u32, min: u32, max: u32) -> (gtk::Box, gtk::Scale) {
    slider_row_with_margin(title, initial, min, max, 4)
}

fn slider_row_with_margin(
    title: &str,
    initial: u32,
    min: u32,
    max: u32,
    horizontal_margin: i32,
) -> (gtk::Box, gtk::Scale) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    row.set_margin_start(horizontal_margin);
    row.set_margin_end(horizontal_margin);
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

fn sync_ppd_choices(
    dropdown: &adw::ComboRow,
    profiles: &[String],
    state: &AppState,
    loading: &SyncGuard,
) {
    let mut choices = profiles.to_vec();
    choices.push("disabled".into());
    let references: Vec<&str> = choices.iter().map(String::as_str).collect();
    let selected = state
        .config
        .borrow()
        .active()
        .and_then(|profile| profile.ppd_profile.as_ref())
        .and_then(|selected| profiles.iter().position(|profile| profile == selected))
        .unwrap_or(profiles.len());
    loading.run(|| {
        dropdown.set_model(Some(&gtk::StringList::new(&references)));
        dropdown.set_selected(selected as u32);
        dropdown.set_sensitive(!profiles.is_empty());
    });
}

fn select_dropdown_string(dropdown: &adw::ComboRow, target: &str) {
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
