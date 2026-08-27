use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use z13helper_core::curve::{self, Curve, POINT_COUNT};

type ChangedCallback = Box<dyn Fn(Curve)>;

const CHART_TEMP_MIN: i32 = 20;
const CHART_TEMP_MAX: i32 = 100;
const CHART_TEMP_RANGE: f64 = (CHART_TEMP_MAX - CHART_TEMP_MIN) as f64;

#[derive(Clone)]
pub struct CurveEditor {
    area: gtk::DrawingArea,
    accessible_label: Rc<String>,
    curve: Rc<RefCell<Curve>>,
    selected: Rc<Cell<usize>>,
    muted: Rc<Cell<bool>>,
    high_power_protection: Rc<Cell<bool>>,
    changed: Rc<RefCell<Option<ChangedCallback>>>,
}

impl CurveEditor {
    pub fn new(curve: Curve, accessible_label: &str) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(440)
            .content_height(320)
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .accessible_role(gtk::AccessibleRole::Slider)
            .build();
        area.add_css_class("card");
        let this = Self {
            area,
            accessible_label: Rc::new(accessible_label.into()),
            curve: Rc::new(RefCell::new(curve)),
            selected: Rc::new(Cell::new(0)),
            muted: Rc::new(Cell::new(false)),
            high_power_protection: Rc::new(Cell::new(false)),
            changed: Rc::new(RefCell::new(None)),
        };
        this.update_accessibility();
        this.install_draw();
        this.install_input();
        this
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    pub fn curve(&self) -> Curve {
        *self.curve.borrow()
    }

    pub fn set_curve(&self, curve: Curve) {
        *self.curve.borrow_mut() = curve;
        self.update_accessibility();
        self.area.queue_draw();
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.set(muted);
        self.area.queue_draw();
    }

    pub fn set_editable(&self, editable: bool) {
        self.area.set_sensitive(editable);
    }

    pub fn set_high_power_protection(&self, enabled: bool) {
        self.high_power_protection.set(enabled);
        self.update_accessibility();
        self.area.queue_draw();
    }

    pub fn set_changed(&self, callback: impl Fn(Curve) + 'static) {
        *self.changed.borrow_mut() = Some(Box::new(callback));
    }

    fn emit_changed(&self) {
        if let Some(callback) = self.changed.borrow().as_ref() {
            callback(self.curve());
        }
        self.update_accessibility();
        self.area.queue_draw();
    }

    fn display_curve(&self) -> Curve {
        curve_for_display(&self.curve(), self.high_power_protection.get())
    }

    fn font_size(area: &gtk::DrawingArea) -> f64 {
        area.pango_context()
            .font_description()
            .map(|description| f64::from(description.size()) / f64::from(gtk::pango::SCALE))
            .filter(|size| *size > 0.0)
            .unwrap_or(11.0)
    }

    fn chart_geom(w: f64, h: f64, font_size: f64) -> (f64, f64, f64, f64) {
        let pad_left = (font_size * 4.4).max(48.0);
        let pad_right = 16.0;
        let pad_top = (font_size * 1.8).max(20.0);
        let pad_bottom = (font_size * 3.2).max(36.0);
        (
            pad_left,
            pad_top,
            (w - pad_left - pad_right).max(1.0),
            (h - pad_top - pad_bottom).max(1.0),
        )
    }

    fn update_accessibility(&self) {
        let index = self.selected.get();
        let point = self.display_curve()[index];
        let value = format!(
            "Point {} of {POINT_COUNT}: {} degrees Celsius, {} percent fan speed",
            index + 1,
            point[0],
            curve::pwm_to_percent(point[1])
        );
        self.area.update_property(&[
            gtk::accessible::Property::Label(&self.accessible_label),
            gtk::accessible::Property::Description(
                "Press Tab to select a point. Use arrow keys to adjust it; hold Shift for larger steps.",
            ),
            gtk::accessible::Property::ValueText(&value),
        ]);
    }

    fn install_draw(&self) {
        let curve = self.curve.clone();
        let selected = self.selected.clone();
        let muted = self.muted.clone();
        let high_power_protection = self.high_power_protection.clone();
        self.area.set_draw_func(move |area, cr, width, height| {
            let w = width as f64;
            let h = height as f64;
            let font_size = Self::font_size(area);
            let (left, top, cw, ch) = Self::chart_geom(w, h, font_size);
            let x = |temp: i32| left + (temp - CHART_TEMP_MIN) as f64 / CHART_TEMP_RANGE * cw;
            let y = |pwm: i32| top + (1.0 - pwm as f64 / 255.0) * ch;
            let foreground = area.color();
            let set_color = |color: &gtk::gdk::RGBA, opacity: f64| {
                cr.set_source_rgba(
                    f64::from(color.red()),
                    f64::from(color.green()),
                    f64::from(color.blue()),
                    f64::from(color.alpha()) * opacity,
                );
            };

            // Grid.
            cr.set_line_width(1.0);
            set_color(&foreground, 0.18);
            for t in (CHART_TEMP_MIN..=CHART_TEMP_MAX).step_by(10) {
                cr.move_to(x(t), top);
                cr.line_to(x(t), top + ch);
            }
            for p in (0..=100).step_by(20) {
                cr.move_to(left, y(curve::percent_to_pwm(p)));
                cr.line_to(left + cw, y(curve::percent_to_pwm(p)));
            }
            let _ = cr.stroke();

            // Axis labels.
            set_color(&foreground, 0.88);
            let font = area.pango_context().font_description();
            let family = font
                .as_ref()
                .and_then(gtk::pango::FontDescription::family)
                .unwrap_or_else(|| "Sans".into());
            cr.select_font_face(
                &family,
                gtk::cairo::FontSlant::Normal,
                gtk::cairo::FontWeight::Normal,
            );
            cr.set_font_size(font_size);
            for t in (CHART_TEMP_MIN..=CHART_TEMP_MAX).step_by(10) {
                let label = format!("{t}");
                if let Ok(ext) = cr.text_extents(&label) {
                    cr.move_to(x(t) - ext.width() / 2.0, top + ch + 16.0);
                    let _ = cr.show_text(&label);
                }
            }
            for p in [0, 20, 40, 60, 80, 100] {
                let label = if p == 0 {
                    "OFF".to_string()
                } else {
                    format!("{p}%")
                };
                if let Ok(ext) = cr.text_extents(&label) {
                    cr.move_to(left - ext.width() - 6.0, y(curve::percent_to_pwm(p)) + 4.0);
                    let _ = cr.show_text(&label);
                }
            }
            // Axis titles.
            cr.set_font_size(font_size * 0.9);
            set_color(&foreground, 0.72);
            cr.move_to(left + cw / 2.0 - 12.0, h - 4.0);
            let _ = cr.show_text("°C");

            let authored = *curve.borrow();
            let points = curve_for_display(&authored, high_power_protection.get());
            if muted.get() {
                set_color(&foreground, 0.45);
            } else {
                set_color(&foreground, 1.0);
            }
            cr.set_line_width(2.5);
            for (i, pt) in points.iter().enumerate() {
                if i == 0 {
                    cr.move_to(x(pt[0]), y(pt[1]));
                } else {
                    cr.line_to(x(pt[0]), y(pt[1]));
                }
            }
            let _ = cr.stroke();

            for (i, pt) in points.iter().enumerate() {
                if i == selected.get() {
                    set_color(&foreground, 1.0);
                    // Hover-style tooltip near the point.
                    cr.set_font_size(font_size);
                    let tip = format!("{}°C, {}%", pt[0], curve::pwm_to_percent(pt[1]));
                    cr.move_to(x(pt[0]) + 8.0, y(pt[1]) - 8.0);
                    let _ = cr.show_text(&tip);
                    set_color(&foreground, 1.0);
                } else {
                    set_color(&foreground, 0.92);
                }
                cr.arc(x(pt[0]), y(pt[1]), 5.5, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            }
        });
    }

    fn install_input(&self) {
        let gesture = gtk::GestureDrag::new();
        let editor = self.clone();
        gesture.connect_drag_begin(move |_, px, py| {
            editor.area.grab_focus();
            let idx = editor.closest_point(px, py);
            editor.selected.set(idx);
            editor.update_accessibility();
            editor.area.queue_draw();
        });
        let editor = self.clone();
        gesture.connect_drag_update(move |gesture, dx, dy| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let idx = editor.selected.get();
            let shift = gesture
                .current_event_state()
                .contains(gtk::gdk::ModifierType::SHIFT_MASK);
            editor.move_point(idx, start_x + dx, start_y + dy, shift);
        });
        self.area.add_controller(gesture);

        let keys = gtk::EventControllerKey::new();
        let editor = self.clone();
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            let mut idx = editor.selected.get();
            if key == gtk::gdk::Key::Tab {
                idx = (idx + 1) % POINT_COUNT;
                editor.selected.set(idx);
                editor.update_accessibility();
                editor.area.queue_draw();
                return glib::Propagation::Stop;
            }
            let step = if modifiers.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                5
            } else {
                1
            };
            let (dt, dp) = match key {
                gtk::gdk::Key::Left => (-step, 0),
                gtk::gdk::Key::Right => (step, 0),
                gtk::gdk::Key::Up => (0, step),
                gtk::gdk::Key::Down => (0, -step),
                _ => return glib::Propagation::Proceed,
            };
            let mut curve = editor.curve.borrow_mut();
            if editor.high_power_protection.get() && idx >= 6 {
                return glib::Propagation::Stop;
            }
            let changed = adjust_authored_point(&mut curve, idx, dt, dp);
            drop(curve);
            if changed {
                editor.emit_changed();
            }
            glib::Propagation::Stop
        });
        self.area.add_controller(keys);
    }

    fn closest_point(&self, px: f64, py: f64) -> usize {
        let w = self.area.width() as f64;
        let h = self.area.height() as f64;
        let (left, top, cw, ch) = Self::chart_geom(w, h, Self::font_size(&self.area));
        let x = |t: i32| left + (t - CHART_TEMP_MIN) as f64 / CHART_TEMP_RANGE * cw;
        let y = |p: i32| top + (1.0 - p as f64 / 255.0) * ch;
        let curve = self.display_curve();
        curve
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let da = (x(a[0]) - px).powi(2) + (y(a[1]) - py).powi(2);
                let db = (x(b[0]) - px).powi(2) + (y(b[1]) - py).powi(2);
                da.partial_cmp(&db).unwrap()
            })
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn move_point(&self, idx: usize, px: f64, py: f64, vertical_only: bool) {
        let w = self.area.width() as f64;
        let h = self.area.height() as f64;
        let (left, top, cw, ch) = Self::chart_geom(w, h, Self::font_size(&self.area));
        let temp = (CHART_TEMP_MIN as f64 + (px - left) / cw * CHART_TEMP_RANGE)
            .round()
            .clamp(CHART_TEMP_MIN as f64, CHART_TEMP_MAX as f64) as i32;
        let pwm = ((1.0 - (py - top) / ch) * 255.0).round() as i32;
        let mut curve = self.curve.borrow_mut();
        if self.high_power_protection.get() && idx >= 6 {
            return;
        }
        if vertical_only {
            let delta = pwm - curve[idx][1];
            curve::shift_curve_vertical(&mut curve, delta);
        } else {
            curve[idx][0] = temp;
            curve[idx][1] = pwm;
            curve::enforce_curve(&mut curve, idx);
        }
        drop(curve);
        self.emit_changed();
    }
}

fn adjust_authored_point(
    curve: &mut Curve,
    idx: usize,
    temp_delta: i32,
    pwm_delta_percent: i32,
) -> bool {
    if idx >= POINT_COUNT {
        return false;
    }

    let before = *curve;
    curve[idx][0] = curve[idx][0]
        .saturating_add(temp_delta)
        .clamp(curve::TEMP_MIN, curve::TEMP_MAX);
    let pwm_step = if pwm_delta_percent >= 0 {
        curve::percent_to_pwm(pwm_delta_percent)
    } else {
        curve::percent_to_pwm(pwm_delta_percent.saturating_neg())
    };
    curve[idx][1] = if pwm_delta_percent >= 0 {
        curve[idx][1].saturating_add(pwm_step)
    } else {
        curve[idx][1].saturating_sub(pwm_step)
    }
    .clamp(curve::PWM_MIN, curve::PWM_MAX);
    curve::enforce_curve(curve, idx);
    *curve != before
}

fn curve_for_display(authored: &Curve, protection: bool) -> Curve {
    if protection {
        curve::high_power_curve(authored)
    } else {
        *authored
    }
}

#[cfg(test)]
mod tests {
    use super::{adjust_authored_point, curve_for_display};
    use z13helper_core::curve::{self, Curve};

    fn test_curve() -> Curve {
        [
            [20, 0],
            [30, 32],
            [40, 64],
            [50, 96],
            [60, 128],
            [70, 160],
            [80, 192],
            [90, 224],
        ]
    }

    #[test]
    fn keyboard_pwm_adjustment_uses_saturating_authored_deltas() {
        let mut curve = test_curve();
        curve[0][1] = 0;
        assert!(!adjust_authored_point(&mut curve, 0, 0, -1));
        assert_eq!(curve[0][1], 0);

        curve[0][1] = 1;
        assert!(adjust_authored_point(&mut curve, 0, 0, -1));
        assert_eq!(curve[0][1], 0);

        curve[0][1] = 0;
        assert!(adjust_authored_point(&mut curve, 0, 0, 1));
        assert_eq!(curve[0][1], 2);

        curve[0][1] = 254;
        assert!(adjust_authored_point(&mut curve, 0, 0, 1));
        assert_eq!(curve[0][1], 255);

        curve[0][1] = 255;
        assert!(adjust_authored_point(&mut curve, 0, 0, -1));
        assert_eq!(curve[0][1], 253);
        assert!(curve::validate(&curve).is_ok());
    }

    #[test]
    fn keyboard_adjustment_preserves_authored_curve_invariants() {
        let mut curve = test_curve();
        assert!(adjust_authored_point(&mut curve, 4, 0, -5));
        assert!(curve::validate(&curve).is_ok());
        for pair in curve.windows(2) {
            assert!(pair[1][0] > pair[0][0]);
            assert!(pair[1][1] >= pair[0][1]);
        }
    }

    #[test]
    fn protection_only_changes_the_display_copy() {
        let mut authored = Curve::default();
        authored[6] = [95, 10];
        authored[7] = [100, 20];
        assert_eq!(curve_for_display(&authored, false), authored);
        assert_eq!(
            curve_for_display(&authored, true),
            curve::high_power_curve(&authored)
        );
        assert_eq!(authored[6], [95, 10]);
        assert_eq!(authored[7], [100, 20]);
    }
}
