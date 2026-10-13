//! Fans + Power window: profile editor, power limits, undervolt, fan curve.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4 as gtk;
use libadwaita as adw;
use libadwaita::prelude::*;
use z13helper_core::{
    ApplyRequest, Command, DaemonError, FanControlMode, MAX_CPU_TEMP_LIMIT_C, MAX_FAN_HYSTERESIS,
    MAX_FAN_TEMPERATURE_AVERAGE_SECONDS, MAX_FPPT_WATTS, MAX_PPT_WATTS, MAX_UNDERVOLT,
    MIN_CPU_TEMP_LIMIT_C, MIN_FAN_HYSTERESIS, MIN_PPT_WATTS, MIN_UNDERVOLT, Profile, WireStatus,
    fan_protection_required, stock_fan_curves, stock_ppt,
};

use crate::app::AppState;
use crate::curve_editor::CurveEditor;
use crate::sync::SyncGuard;
use crate::worker;

struct PendingApply {
    source: glib::SourceId,
    edited_profile: String,
}

type ApplySchedule = Rc<RefCell<Option<PendingApply>>>;

pub fn present(state: &Rc<AppState>, parent: &impl IsA<gtk::Window>) {
    if let Some(window) = state.fans_window() {
        state.present_existing_auxiliary(&window);
        let _ = window.activate_action("fans.refresh", None);
        return;
    }
    let (window_width, window_height) = state
        .gamescope
        .as_ref()
        .map_or((920, 720), |gamescope| gamescope.panel_size(920, 720));
    let window = adw::Window::builder()
        .application(&state.app)
        .transient_for(parent)
        .title("Fans + Power")
        .default_width(window_width)
        .default_height(window_height)
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
    // The window opens on the active profile; edits must target what it shows.
    state.select_editing_profile(&profile.id);
    let editor = CurveEditor::new(profile.fan_curves[0], "Fan 1 curve");
    let editor2 = CurveEditor::new(profile.fan_curves[1], "Fan 2 curve");

    let loading = SyncGuard::default();
    let apply_schedule: ApplySchedule = Rc::new(RefCell::new(None));
    let state_close = state.clone();
    let schedule_close = apply_schedule.clone();
    window.connect_close_request(move |window| {
        flush_pending_edit(&state_close, &schedule_close);
        state_close.hide_auxiliary(window.upcast_ref());
        glib::Propagation::Stop
    });

    let (cpu_page, power) = build_cpu_page(state, &loading, &apply_schedule, &editor, &editor2);
    let (advanced_page, undervolt) = build_advanced_page(
        state,
        &loading,
        &apply_schedule,
        &window,
        &editor,
        &editor2,
        &power.protection_warning,
    );
    stack.add_titled(&cpu_page, Some("cpu"), "CPU");
    stack.add_titled(&advanced_page, Some("advanced"), "Advanced");

    left.append(&switcher);
    left.append(&stack);

    // Right: profile selector + chart
    let sel_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let (profile_names, selected) = {
        let config = state.config.borrow();
        profile_model(&config, &config.active_profile)
    };
    let selector = gtk::DropDown::new(Some(profile_names), None::<gtk::Expression>);
    selector.set_selected(selected);
    selector.set_hexpand(true);
    let selector_control: gtk::Widget = if state.gamescope.is_some() {
        gamescope_dropdown_button(&selector, "Profile").upcast()
    } else {
        selector.clone().upcast()
    };
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
    sel_row.append(&selector_control);
    sel_row.append(&plus);
    sel_row.append(&rename);
    sel_row.append(&minus);
    root.append(&sel_row);

    let restore = gtk::Button::with_label("Restore Factory Defaults");
    restore.set_tooltip_text(Some(
        "Reset Silent, Balanced, Turbo (or the selected custom) to stock power, fans, APU temperature, and undervolt.",
    ));

    let fan_toggle = gtk::CheckButton::with_label("Apply custom fan curve");
    let unified_toggle = gtk::CheckButton::with_label("Unified fan control");
    unified_toggle.set_tooltip_text(Some(
        "Use the Fan 1 graph to control both fans with the same curve.",
    ));
    right.append(&fan_toggle);
    right.append(&unified_toggle);

    let direct_toggle = gtk::CheckButton::with_label("Direct EC control");
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
    right.append(&direct_warning);

    let hysteresis_group = adw::PreferencesGroup::builder()
        .title("Direct EC Hysteresis")
        .build();
    let (hysteresis_up_row, hysteresis_up) = slider_row(
        "Fan speed-up hysteresis",
        u32::from(MIN_FAN_HYSTERESIS),
        u32::from(MAX_FAN_HYSTERESIS),
        12,
    );
    let (hysteresis_down_row, hysteresis_down) = slider_row(
        "Fan slow-down hysteresis",
        u32::from(MIN_FAN_HYSTERESIS),
        u32::from(MAX_FAN_HYSTERESIS),
        12,
    );
    let (temperature_average_row, temperature_average) = slider_row(
        "Temperature averaging (seconds)",
        0,
        u32::from(MAX_FAN_TEMPERATURE_AVERAGE_SECONDS),
        12,
    );
    hysteresis_group.add(&hysteresis_up_row);
    hysteresis_group.add(&hysteresis_down_row);
    hysteresis_group.add(&temperature_average_row);
    right.append(&hysteresis_group);

    let direct_status = gtk::Label::new(Some("Direct EC control: checking…"));
    direct_status.add_css_class("dim-label");
    direct_status.set_xalign(0.0);
    right.append(&direct_status);

    let chart_label = gtk::Label::new(None);
    chart_label.add_css_class("dim-label");
    chart_label.set_xalign(0.0);
    right.append(&chart_label);
    right.append(editor.widget());
    let chart2_label = gtk::Label::new(Some("Fan 2 Curve — % vs °C"));
    chart2_label.add_css_class("dim-label");
    chart2_label.set_xalign(0.0);
    right.append(&chart2_label);
    right.append(editor2.widget());

    let editors = Rc::new(ProfileEditorView {
        fans: FanEditorView {
            first: editor,
            second: editor2,
            enabled: fan_toggle,
            unified: unified_toggle,
            direct: direct_toggle,
            direct_explanation: direct_warning,
            hysteresis_up,
            hysteresis_down,
            temperature_average,
            chart_label,
            chart2_label,
        },
        power,
        undervolt,
        loading: loading.clone(),
    });
    editors.load(
        &profile,
        state.config.borrow().disable_high_power_fan_protection,
    );

    let editors_probe = editors.clone();
    let state_probe = state.clone();
    let show_status = Rc::new(move |result: Result<WireStatus, DaemonError>| {
        let fans = &editors_probe.fans;
        match result {
            Ok(status) => {
                let undervolt = status.capabilities.undervolt;
                state_probe.undervolt_available.set(Some(undervolt));
                editors_probe.undervolt.set_available(Some(undervolt));
                sync_ppd_choices(
                    &editors_probe.power.ppd,
                    &status.capabilities.ppd_profiles,
                    &state_probe,
                    &editors_probe.loading,
                );
                fans.direct.set_sensitive(status.capabilities.direct_fans);
                fans.sync_direct();
                if !status.capabilities.direct_fans {
                    direct_status.set_label("Direct EC control is unavailable");
                    return;
                }
                let duties = status
                    .direct_fan_duties
                    .map(|duty| z13helper_core::curve::pwm_to_percent(i32::from(duty)));
                let rpms = format!(
                    "{} / {}",
                    status.telemetry.fan_rpms[0], status.telemetry.fan_rpms[1]
                );
                if status.fan_control_mode == FanControlMode::Direct {
                    direct_status.set_label(&format!(
                        "Direct EC PWM: {}% / {}% · RPM {rpms}",
                        duties[0], duties[1]
                    ));
                } else {
                    direct_status.set_label(&format!("Firmware fan control · RPM {rpms}"));
                }
            }
            Err(error) => {
                editors_probe.power.ppd.set_sensitive(false);
                fans.direct.set_sensitive(false);
                fans.sync_direct();
                editors_probe.undervolt.set_available(None);
                direct_status.set_label(&format!("z13helperd unavailable: {error}"));
            }
        }
    });
    // Close only hides this window, so re-probe the daemon on every present.
    let refresh = gio::SimpleAction::new("refresh", None);
    let client_probe = state.client.clone();
    refresh.connect_activate(move |_, _| {
        let client = client_probe.clone();
        let show_status = show_status.clone();
        worker::blocking(
            move || client.get_state(),
            move |result| show_status(result),
        );
    });
    refresh.activate(None);
    let refresh_group = gio::SimpleActionGroup::new();
    refresh_group.add_action(&refresh);
    window.insert_action_group("fans", Some(&refresh_group));

    let state_fan = state.clone();
    let editors_fan = editors.clone();
    let apply_schedule_fan = apply_schedule.clone();
    editors.fans.enabled.connect_toggled(move |toggle| {
        if editors_fan.loading.active() {
            return;
        }
        let fans = &editors_fan.fans;
        fans.sync_curves(toggle.is_active(), fans.unified.is_active());
        let id = state_fan.editing_profile_id();
        state_fan.edit_profile(&id, |p| {
            p.apply_fan_curve = toggle.is_active();
        });
        schedule_apply(&state_fan, &apply_schedule_fan, id);
    });

    let state_unified = state.clone();
    let editors_unified = editors.clone();
    let apply_schedule_unified = apply_schedule.clone();
    editors.fans.unified.connect_toggled(move |toggle| {
        if editors_unified.loading.active() {
            return;
        }
        let unified = toggle.is_active();
        let id = state_unified.editing_profile_id();
        state_unified.edit_profile(&id, |profile| {
            profile.unified_fan_control = unified;
        });
        let fans = &editors_unified.fans;
        fans.sync_curves(fans.enabled.is_active(), unified);
        schedule_apply(&state_unified, &apply_schedule_unified, id);
    });

    let state_direct = state.clone();
    let editors_direct = editors.clone();
    let apply_schedule_direct = apply_schedule.clone();
    editors.fans.direct.connect_toggled(move |toggle| {
        if editors_direct.loading.active() {
            return;
        }
        editors_direct.fans.sync_direct();
        let id = state_direct.editing_profile_id();
        state_direct.edit_profile(&id, |profile| {
            profile.fan_control_mode = if toggle.is_active() {
                FanControlMode::Direct
            } else {
                FanControlMode::Firmware
            };
        });
        schedule_apply(&state_direct, &apply_schedule_direct, id);
    });

    type SetTuning = fn(&mut Profile, u8);
    let direct_tuning: [(_, SetTuning); 3] = [
        (&editors.fans.hysteresis_up, |p, value| {
            p.fan_hysteresis_up = value
        }),
        (&editors.fans.hysteresis_down, |p, value| {
            p.fan_hysteresis_down = value
        }),
        (&editors.fans.temperature_average, |p, value| {
            p.fan_temperature_average_seconds = value
        }),
    ];
    for (scale, set) in direct_tuning {
        let state = state.clone();
        let loading = loading.clone();
        let apply_schedule = apply_schedule.clone();
        scale.connect_value_changed(move |scale| {
            if loading.active() {
                return;
            }
            let id = state.editing_profile_id();
            state.edit_profile(&id, |profile| {
                set(profile, scale.value() as u8);
            });
            schedule_apply(&state, &apply_schedule, id);
        });
    }

    // Persist curve edits into the profile currently selected in this window.
    for (index, curve_editor) in [&editors.fans.first, &editors.fans.second]
        .into_iter()
        .enumerate()
    {
        let state = state.clone();
        let loading = loading.clone();
        let apply_schedule = apply_schedule.clone();
        curve_editor.set_changed(move |curve| {
            if loading.active() {
                return;
            }
            let id = state.editing_profile_id();
            state.edit_profile(&id, |profile| {
                profile.fan_curves[index] = curve;
            });
            schedule_apply(&state, &apply_schedule, id);
        });
    }

    // Switching profiles in the dropdown: save previous, load next into
    // editors, and make it the active profile.
    let state_sel = state.clone();
    let schedule_sel = apply_schedule.clone();
    let editors_sel = editors.clone();
    let rename_sel = rename.clone();
    let remove_sel = minus.clone();
    selector.connect_selected_notify(move |drop| {
        if editors_sel.loading.active() {
            return;
        }
        let idx = drop.selected() as usize;
        let profiles = state_sel.config.borrow().profiles.clone();
        let Some(next) = profiles.get(idx) else {
            return;
        };
        flush_pending_edit(&state_sel, &schedule_sel);
        state_sel.select_editing_profile(&next.id);
        editors_sel.load(
            next,
            state_sel.config.borrow().disable_high_power_fan_protection,
        );
        set_profile_action_sensitivity(&state_sel, &next.id, &rename_sel, &remove_sel);
        crate::power::select_profile(&state_sel, &next.id);
    });

    let state_def = state.clone();
    let editors_def = editors.clone();
    let restored_toast = toast_overlay.clone();
    let restore_factory = Rc::new(move || {
        let id = state_def.editing_profile_id();
        let Some(mut restored) = state_def.config.borrow().find(&id).cloned() else {
            return;
        };
        restored.factory_defaults();
        let Some(token) = state_def.begin_profile_operation(&id) else {
            return;
        };
        let ppd_profile = restored.ppd_profile.clone();
        // Only the active profile drives hardware. Applying it first also
        // lowers PL1 below the daemon's factory-curve read limit.
        let request = (id == state_def.active_profile_id()).then(|| {
            ApplyRequest::from_profile(
                &restored,
                state_def.config.borrow().disable_high_power_fan_protection,
            )
        });
        let client = state_def.client.clone();
        let state_done = state_def.clone();
        let editors_done = editors_def.clone();
        let toast_done = restored_toast.clone();
        let worker_token = token.clone();
        worker::blocking(
            move || {
                let warnings = match request {
                    Some(request) => client.apply(request)?.warnings,
                    None => Vec::new(),
                };
                let factory = ppd_profile.map(|ppd| client.factory_fan_curves(vec![ppd]));
                Ok::<_, DaemonError>((restored, warnings, factory))
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
                    let editing_current = state_done.operation_is_current_editor(&worker_token);
                    if state_done.replace_profile_if_current(&worker_token, restored.clone()) {
                        if editing_current {
                            editors_done.load(
                                &restored,
                                state_done.config.borrow().disable_high_power_fan_protection,
                            );
                        }
                        state_done.save_config();
                        toast_done.add_toast(adw::Toast::new("Factory defaults restored"));
                    }
                    // A stale restore the daemon already finished is never
                    // cancelled; either way converge to the current active profile.
                    if state_done.active_profile_id() == worker_token.id {
                        state_done.apply_active();
                    }
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
            .find(&restore_state.editing_profile_id())
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
        let id = state_add.add_custom_profile();
        state_add.save_config();
        let (model, selected) = profile_model(&state_add.config.borrow(), &id);
        loading_add.run(|| {
            selector_add.set_model(Some(&model));
            selector_add.set_selected(selected);
        });
        // Trigger load via notify by toggling selected (already at end).
        selector_add.notify("selected");
        state_add.apply_active();
    });

    let state_rm = state.clone();
    let selector_rm = selector.clone();
    let editors_rm = editors.clone();
    let loading_rm = loading.clone();
    let rename_rm = rename.clone();
    let remove_rm = minus.clone();
    let removed_toast = toast_overlay.clone();
    let remove_profile = Rc::new(move || {
        let id = state_rm.editing_profile_id();
        let old_active = state_rm.active_profile_id();
        if state_rm.remove_profile(&id) {
            let active = state_rm.config.borrow().active_profile.clone();
            let (model, selected) = profile_model(&state_rm.config.borrow(), &active);
            let profile = state_rm
                .config
                .borrow()
                .profiles
                .get(selected as usize)
                .cloned();
            loading_rm.run(|| {
                selector_rm.set_model(Some(&model));
                selector_rm.set_selected(selected);
                if let Some(profile) = profile {
                    state_rm.select_editing_profile(&profile.id);
                    editors_rm.load(
                        &profile,
                        state_rm.config.borrow().disable_high_power_fan_protection,
                    );
                }
            });
            set_profile_action_sensitivity(&state_rm, &active, &rename_rm, &remove_rm);
            state_rm.save_config();
            if old_active != state_rm.active_profile_id() {
                state_rm.apply_active();
            }
            removed_toast.add_toast(adw::Toast::new("Profile removed"));
        }
    });
    let remove_parent = window.clone();
    let remove_state = state.clone();
    minus.connect_clicked(move |_| {
        let name = remove_state
            .config
            .borrow()
            .find(&remove_state.editing_profile_id())
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
        let id = state_ren.editing_profile_id();
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
                if !new_name.is_empty() && state.rename_profile(&id, &new_name) {
                    state.save_config();
                    let (model, selected) = profile_model(&state.config.borrow(), &id);
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
    if let Some(gamescope) = state.gamescope.as_ref() {
        window.add_css_class("gamescope-overlay-window");
        window.set_decorated(false);
        window.fullscreen();
        let closing = window.clone();
        let wrapper = gamescope.wrap_panel(&toolbar, 920, Some(720), move || closing.close());
        window.set_content(Some(&wrapper));
    } else {
        window.set_content(Some(&toolbar));
    }
    state.set_fans_window(window.upcast_ref());
    state.present_auxiliary(&window);
}

fn build_cpu_page(
    state: &Rc<AppState>,
    loading: &SyncGuard,
    apply_schedule: &ApplySchedule,
    first_editor: &CurveEditor,
    second_editor: &CurveEditor,
) -> (gtk::Box, PowerEditorView) {
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
    if state.gamescope.is_some() {
        ppd_group.add(&gamescope_combo_row(&ppd, "PPD Profile"));
    } else {
        ppd_group.add(&ppd);
    }
    page.append(&ppd_group);

    let power_group = adw::PreferencesGroup::builder()
        .title("Power Limits")
        .build();
    let apply_power = gtk::CheckButton::with_label("Apply power limits");
    let (min_ppt, max_ppt, max_fppt) = (
        MIN_PPT_WATTS as u32,
        MAX_PPT_WATTS as u32,
        MAX_FPPT_WATTS as u32,
    );
    let (spl_row, spl) = slider_row("SPL (CPU sustained)", min_ppt, max_ppt, 4);
    let (sppt_row, sppt) = slider_row("sPPT (CPU long boost)", min_ppt, max_ppt, 4);
    let (fppt_row, fppt) = slider_row("fPPT (CPU short boost)", min_ppt, max_fppt, 4);
    power_group.add(&spl_row);
    power_group.add(&sppt_row);
    power_group.add(&fppt_row);
    power_group.add(&apply_power);
    page.append(&power_group);

    let warning = gtk::Label::new(Some(
        "At 80 W and above, the final points are protected at 80°C / at least 80% and 90°C / 100%.",
    ));
    warning.add_css_class("warning");
    warning.set_wrap(true);
    page.append(&warning);
    install_ordering(&spl, &sppt, &fppt, loading);

    let update = {
        let state = state.clone();
        let power = apply_power.clone();
        let pl1 = spl.clone();
        let pl2 = sppt.clone();
        let pl3 = fppt.clone();
        let loading = loading.clone();
        let apply_schedule = apply_schedule.clone();
        let first_editor = first_editor.clone();
        let second_editor = second_editor.clone();
        let warning = warning.clone();
        // `ppd_selection` is set only by the PPD row itself; other power
        // edits must not read the PPD back from a row that may not list it.
        Rc::new(move |ppd_selection: Option<Option<String>>| {
            if loading.active() {
                return;
            }
            let id = state.editing_profile_id();
            let high_power_disabled = state.config.borrow().disable_high_power_fan_protection;
            let changed = state.edit_profile(&id, |profile| {
                if let Some(ppd_profile) = ppd_selection
                    && profile.ppd_profile != ppd_profile
                {
                    profile.ppd_profile = ppd_profile;
                    profile.factory_fan_curves_loaded = false;
                    if !profile.apply_fan_curve {
                        profile.fan_curves = stock_fan_curves(profile.ppd_profile.as_deref());
                        first_editor.set_curve(profile.fan_curves[0]);
                        second_editor.set_curve(profile.fan_curves[1]);
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
                show_high_power_protection(
                    [&first_editor, &second_editor],
                    &warning,
                    high_power_protection(profile, high_power_disabled),
                );
            });
            if changed.is_some() {
                schedule_apply(&state, &apply_schedule, id);
            }
        })
    };
    let update_ppd = update.clone();
    ppd.connect_selected_notify(move |row| {
        update_ppd(Some(
            row.selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|item| item.string().to_string())
                .filter(|selection| selection != "disabled"),
        ))
    });
    let update_toggle = update.clone();
    let spl_toggle = spl.clone();
    let sppt_toggle = sppt.clone();
    let fppt_toggle = fppt.clone();
    apply_power.connect_toggled(move |toggle| {
        let enabled = toggle.is_active();
        if enabled {
            sppt_toggle.set_value(sppt_toggle.value().max(spl_toggle.value()));
            fppt_toggle.set_value(fppt_toggle.value().max(sppt_toggle.value()));
        }
        spl_toggle.set_sensitive(enabled);
        sppt_toggle.set_sensitive(enabled);
        fppt_toggle.set_sensitive(enabled);
        update_toggle(None);
    });
    for scale in [&spl, &sppt, &fppt] {
        let update = update.clone();
        scale.connect_value_changed(move |_| update(None));
    }

    let view = PowerEditorView {
        spl,
        sppt,
        fppt,
        enabled: apply_power,
        ppd,
        protection_warning: warning,
    };
    (page, view)
}

fn build_advanced_page(
    state: &Rc<AppState>,
    loading: &SyncGuard,
    apply_schedule: &ApplySchedule,
    parent: &adw::Window,
    first_editor: &CurveEditor,
    second_editor: &CurveEditor,
    protection_warning: &gtk::Label,
) -> (gtk::Box, UndervoltEditorView) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    page.set_margin_top(12);
    page.set_margin_bottom(12);
    page.set_margin_start(12);
    page.set_margin_end(12);

    let group = adw::PreferencesGroup::builder()
        .title("CPU Undervolt (Curve Optimizer)")
        .build();

    let apply_uv = gtk::CheckButton::with_label("Auto Apply");

    // Full-width slider below the title — ActionRow suffixes crush scales.
    let uv = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        f64::from(MIN_UNDERVOLT),
        f64::from(MAX_UNDERVOLT),
        1.0,
    );
    uv.set_draw_value(true);
    uv.set_value_pos(gtk::PositionType::Right);
    uv.set_digits(0);
    uv.add_css_class("undervolt-scale");
    uv.set_hexpand(true);
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
    let cpu_temp_limit = gtk::Scale::with_range(
        gtk::Orientation::Horizontal,
        f64::from(MIN_CPU_TEMP_LIMIT_C),
        f64::from(MAX_CPU_TEMP_LIMIT_C),
        1.0,
    );
    cpu_temp_limit.set_draw_value(true);
    cpu_temp_limit.set_value_pos(gtk::PositionType::Right);
    cpu_temp_limit.set_digits(0);
    cpu_temp_limit.add_css_class("undervolt-scale");
    cpu_temp_limit.set_hexpand(true);
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
    let warning_protection = protection_warning.clone();
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
            let warning = warning_protection.clone();
            dialog.connect_response(Some("disable"), move |_, _| {
                state.config.borrow_mut().disable_high_power_fan_protection = true;
                loading.run(|| toggle.set_active(true));
                show_high_power_protection([&first, &second], &warning, false);
                let profile_id = state.active_profile_id();
                schedule_apply(&state, &schedule, profile_id);
            });
            dialog.present(Some(&parent_protection));
        } else {
            state_protection
                .config
                .borrow_mut()
                .disable_high_power_fan_protection = false;
            let protected = state_protection
                .config
                .borrow()
                .find(&state_protection.editing_profile_id())
                .is_some_and(|profile| high_power_protection(profile, false));
            show_high_power_protection(
                [&first_protection, &second_protection],
                &warning_protection,
                protected,
            );
            let profile_id = state_protection.active_profile_id();
            schedule_apply(&state_protection, &apply_schedule_protection, profile_id);
        }
    });

    let state_temperature = state.clone();
    let loading_temperature = loading.clone();
    let apply_schedule_temperature = apply_schedule.clone();
    cpu_temp_limit.connect_value_changed(move |scale| {
        if loading_temperature.active() {
            return;
        }
        let id = state_temperature.editing_profile_id();
        state_temperature.edit_profile(&id, |profile| {
            profile.cpu_temp_limit = scale.value() as u8;
        });
        schedule_apply(&state_temperature, &apply_schedule_temperature, id);
    });

    let state_uv = state.clone();
    let apply_uv_c = apply_uv.clone();
    let loading_uv = loading.clone();
    let apply_schedule_uv = apply_schedule.clone();
    uv.connect_value_changed(move |scale| {
        if loading_uv.active() {
            return;
        }
        let id = state_uv.editing_profile_id();
        state_uv.edit_profile(&id, |p| {
            p.cpu_co = scale.value() as i32;
            p.apply_undervolt = apply_uv_c.is_active();
        });
        if apply_uv_c.is_active() {
            schedule_apply(&state_uv, &apply_schedule_uv, id);
        } else {
            state_uv.save_config();
        }
    });
    let state_chk = state.clone();
    let loading_uv = loading.clone();
    let apply_schedule_uv = apply_schedule.clone();
    apply_uv.connect_toggled(move |chk| {
        if loading_uv.active() {
            return;
        }
        let id = state_chk.editing_profile_id();
        state_chk.edit_profile(&id, |p| {
            p.apply_undervolt = chk.is_active();
        });
        schedule_apply(&state_chk, &apply_schedule_uv, id);
    });

    let state_manual = state.clone();
    manual_apply.connect_clicked(move |button| {
        let id = state_manual.editing_profile_id();
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
            move || client.send(Command::ApplyUndervoltOnce { offset }),
            move |result| {
                button_done.set_sensitive(feedback.undervolt_available.get().unwrap_or(true));
                if let Err(error) = result {
                    feedback.report_error(&format!("Manual undervolt apply failed: {error}"));
                }
            },
        );
    });

    let view = UndervoltEditorView {
        value: uv,
        enabled: apply_uv,
        manual_apply,
        unavailable_note: note,
        cpu_temp_limit,
    };
    let available = state.undervolt_available.get();
    if available.is_some() {
        view.set_available(available);
    }
    (page, view)
}

fn schedule_apply(state: &Rc<AppState>, schedule: &ApplySchedule, edited_profile: String) {
    if let Some(pending) = schedule.borrow_mut().take() {
        pending.source.remove();
    }
    let state = state.clone();
    let schedule_done = schedule.clone();
    let edited_profile_for_callback = edited_profile.clone();
    let source = glib::timeout_add_local_once(std::time::Duration::from_millis(200), move || {
        schedule_done.borrow_mut().take();
        commit_pending_edit(&state, &edited_profile_for_callback);
    });
    *schedule.borrow_mut() = Some(PendingApply {
        source,
        edited_profile,
    });
}

fn flush_pending_edit(state: &Rc<AppState>, schedule: &ApplySchedule) {
    let Some(pending) = schedule.borrow_mut().take() else {
        return;
    };
    pending.source.remove();
    commit_pending_edit(state, &pending.edited_profile);
}

fn commit_pending_edit(state: &Rc<AppState>, edited_profile: &str) {
    state.save_config();
    if state.active_profile_id() == edited_profile {
        state.apply_active();
    }
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
    temperature_average: gtk::Scale,
    chart_label: gtk::Label,
    chart2_label: gtk::Label,
}

struct PowerEditorView {
    spl: gtk::Scale,
    sppt: gtk::Scale,
    fppt: gtk::Scale,
    enabled: gtk::CheckButton,
    ppd: adw::ComboRow,
    protection_warning: gtk::Label,
}

struct UndervoltEditorView {
    value: gtk::Scale,
    enabled: gtk::CheckButton,
    manual_apply: gtk::Button,
    unavailable_note: gtk::Label,
    cpu_temp_limit: gtk::Scale,
}

impl FanEditorView {
    fn sync_curves(&self, apply: bool, unified: bool) {
        self.first.set_muted(!apply);
        self.second.set_muted(!apply);
        self.first.set_editable(apply);
        self.second.set_editable(apply && !unified);
        self.second.widget().set_visible(!unified);
        self.chart_label.set_label(if unified {
            "Unified Fan Curve — % vs °C"
        } else {
            "Fan 1 Curve — % vs °C"
        });
        self.chart2_label.set_visible(!unified);
    }

    /// Direct-mode tuning is editable only while Direct EC control is both
    /// available and selected for the edited profile.
    fn sync_direct(&self) {
        let selected = self.direct.is_active();
        self.direct_explanation.set_visible(selected);
        let enabled = self.direct.is_sensitive() && selected;
        for scale in [
            &self.hysteresis_up,
            &self.hysteresis_down,
            &self.temperature_average,
        ] {
            scale.set_sensitive(enabled);
        }
    }
}

impl UndervoltEditorView {
    /// `None` means the daemon is unreachable, so ryzen_smu is not blamed.
    fn set_available(&self, available: Option<bool>) {
        let enabled = available == Some(true);
        self.value.set_sensitive(enabled);
        self.enabled.set_sensitive(enabled);
        self.manual_apply.set_sensitive(enabled);
        self.cpu_temp_limit.set_sensitive(enabled);
        self.unavailable_note.set_visible(available == Some(false));
    }
}

impl ProfileEditorView {
    fn load(&self, profile: &Profile, disable_high_power_fan_protection: bool) {
        self.loading.run(|| {
            let fans = &self.fans;
            fans.first.set_curve(profile.fan_curves[0]);
            fans.second.set_curve(profile.fan_curves[1]);
            show_high_power_protection(
                [&fans.first, &fans.second],
                &self.power.protection_warning,
                high_power_protection(profile, disable_high_power_fan_protection),
            );
            fans.enabled.set_active(profile.apply_fan_curve);
            fans.unified.set_active(profile.unified_fan_control);
            fans.sync_curves(profile.apply_fan_curve, profile.unified_fan_control);
            fans.direct
                .set_active(profile.fan_control_mode == FanControlMode::Direct);
            fans.hysteresis_up
                .set_value(f64::from(profile.fan_hysteresis_up));
            fans.hysteresis_down
                .set_value(f64::from(profile.fan_hysteresis_down));
            fans.temperature_average
                .set_value(f64::from(profile.fan_temperature_average_seconds));
            fans.sync_direct();
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

/// The value is set by `ProfileEditorView::load`.
fn slider_row(title: &str, min: u32, max: u32, horizontal_margin: i32) -> (gtk::Box, gtk::Scale) {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    row.set_margin_start(horizontal_margin);
    row.set_margin_end(horizontal_margin);
    let label = gtk::Label::new(Some(title));
    label.set_xalign(0.0);
    row.append(&label);
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min as f64, max as f64, 1.0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(0);
    scale.set_hexpand(true);
    row.append(&scale);
    (row, scale)
}

fn gamescope_dropdown_button(dropdown: &gtk::DropDown, title: &'static str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_hexpand(true);
    set_choice_label(&button, Some(title), dropdown.selected_item());

    let button_selected = button.clone();
    dropdown.connect_selected_notify(move |dropdown| {
        set_choice_label(&button_selected, Some(title), dropdown.selected_item());
    });
    let button_model = button.clone();
    dropdown.connect_model_notify(move |dropdown| {
        set_choice_label(&button_model, Some(title), dropdown.selected_item());
    });

    let dropdown_select = dropdown.clone();
    let button_parent = button.clone();
    button.connect_clicked(move |_| {
        let Some(model) = dropdown_select.model() else {
            return;
        };
        let dropdown = dropdown_select.clone();
        show_selection_dialog(
            &model,
            title,
            &button_parent,
            dropdown.selected(),
            move |index| dropdown.set_selected(index),
        );
    });
    button
}

fn set_choice_label(button: &gtk::Button, title: Option<&str>, item: Option<glib::Object>) {
    let selected = item
        .and_downcast::<gtk::StringObject>()
        .map(|item| item.string().to_string())
        .unwrap_or_else(|| "None".into());
    button.set_label(&match title {
        Some(title) => format!("{title}: {selected}"),
        None => selected,
    });
}

fn gamescope_combo_row(combo: &adw::ComboRow, title: &'static str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).build();
    let button = gtk::Button::new();
    set_choice_label(&button, None, combo.selected_item());
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));
    row.set_sensitive(combo.is_sensitive());

    let button_selected = button.clone();
    combo.connect_selected_notify(move |combo| {
        set_choice_label(&button_selected, None, combo.selected_item())
    });
    let button_model = button.clone();
    combo.connect_model_notify(move |combo| {
        set_choice_label(&button_model, None, combo.selected_item())
    });
    let row_sensitive = row.clone();
    combo.connect_sensitive_notify(move |combo| row_sensitive.set_sensitive(combo.is_sensitive()));

    let combo_select = combo.clone();
    let button_parent = button.clone();
    button.connect_clicked(move |_| {
        let Some(model) = combo_select.model() else {
            return;
        };
        let combo = combo_select.clone();
        show_selection_dialog(
            &model,
            title,
            &button_parent,
            combo.selected(),
            move |index| combo.set_selected(index),
        );
    });
    row
}

fn show_selection_dialog(
    model: &gio::ListModel,
    title: &str,
    parent: &gtk::Button,
    selected: u32,
    on_selected: impl Fn(u32) + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(title), Some("Choose a selection"));
    dialog.add_response("cancel", "Cancel");
    dialog.set_close_response("cancel");
    for index in 0..model.n_items() {
        let Some(label) = model
            .item(index)
            .and_downcast::<gtk::StringObject>()
            .map(|item| item.string())
        else {
            continue;
        };
        let response = format!("choice-{index}");
        dialog.add_response(&response, &label);
        if selected == index {
            dialog.set_response_appearance(&response, adw::ResponseAppearance::Suggested);
        }
    }
    dialog.connect_response(None, move |_, response| {
        if let Some(index) = response
            .strip_prefix("choice-")
            .and_then(|index| index.parse::<u32>().ok())
        {
            on_selected(index);
        }
    });
    dialog.present(Some(parent));
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
        .find(&state.editing_profile_id())
        .and_then(|profile| profile.ppd_profile.as_ref())
        .and_then(|selected| profiles.iter().position(|profile| profile == selected))
        .unwrap_or(profiles.len());
    loading.run(|| {
        dropdown.set_model(Some(&gtk::StringList::new(&references)));
        dropdown.set_selected(selected as u32);
        dropdown.set_sensitive(!profiles.is_empty());
    });
}

/// Whether the daemon locks the hardware copy's final two points for this
/// profile; the same predicate as `apply_request`. A request that leaves PPT
/// untouched is decided from the live PL1, which the editor cannot see.
fn high_power_protection(profile: &Profile, disabled: bool) -> bool {
    ApplyRequest::from_profile(profile, disabled)
        .effective_pl1()
        .is_some_and(|pl1| fan_protection_required(pl1, disabled))
}

fn show_high_power_protection(editors: [&CurveEditor; 2], warning: &gtk::Label, protected: bool) {
    for editor in editors {
        editor.set_high_power_protection(protected);
    }
    warning.set_visible(protected);
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

/// Profile names in config order, and the index of `selected_id` (0 if absent).
fn profile_model(config: &z13helper_core::Config, selected_id: &str) -> (gtk::StringList, u32) {
    let names: Vec<&str> = config.profiles.iter().map(|p| p.name.as_str()).collect();
    let selected = config
        .profiles
        .iter()
        .position(|p| p.id == selected_id)
        .unwrap_or(0);
    (gtk::StringList::new(&names), selected as u32)
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
    use super::{high_power_protection, profile_actions_are_editable};
    use z13helper_core::Profile;

    #[test]
    fn editor_lock_and_warning_follow_the_daemon_predicate() {
        let mut profile = Profile::builtin("turbo", "Turbo");
        profile.pl1_spl = 85;
        // Unapplied slider values never reach hardware: stock PPT stays below 80 W.
        assert!(!high_power_protection(&profile, false));
        profile.apply_power_limits = true;
        assert!(high_power_protection(&profile, false));
        assert!(!high_power_protection(&profile, true));
        profile.apply_power_limits = false;
        profile.ppd_profile = None;
        assert!(!high_power_protection(&profile, false));
    }

    #[test]
    fn builtins_cannot_be_renamed_or_removed() {
        let mut config = z13helper_core::Config::default();
        assert!(!profile_actions_are_editable(&config, "silent"));
        let custom = config.add_custom().id.clone();
        assert!(profile_actions_are_editable(&config, &custom));
    }
}
