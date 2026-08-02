use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use z13helper_core::curve::{self, Curve, POINT_COUNT};
use z13helper_core::FanFloorConfig;

type ChangedCallback = Box<dyn Fn(Curve)>;

const PAD_LEFT: f64 = 48.0;
const PAD_RIGHT: f64 = 16.0;
const PAD_TOP: f64 = 20.0;
const PAD_BOTTOM: f64 = 36.0;

#[derive(Clone)]
pub struct CurveEditor {
    area: gtk::DrawingArea,
    curve: Rc<RefCell<Curve>>,
    selected: Rc<Cell<usize>>,
    muted: Rc<Cell<bool>>,
    clamp_grid: Rc<Cell<bool>>,
    floor: Rc<Cell<FanFloorConfig>>,
    changed: Rc<RefCell<Option<ChangedCallback>>>,
}

impl CurveEditor {
    pub fn new(curve: Curve, clamp_grid: bool) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(440)
            .content_height(320)
            .hexpand(true)
            .vexpand(true)
            .focusable(true)
            .build();
        let this = Self {
            area,
            curve: Rc::new(RefCell::new(curve)),
            selected: Rc::new(Cell::new(0)),
            muted: Rc::new(Cell::new(false)),
            clamp_grid: Rc::new(Cell::new(clamp_grid)),
            floor: Rc::new(Cell::new(FanFloorConfig::default())),
            changed: Rc::new(RefCell::new(None)),
        };
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
        self.area.queue_draw();
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.set(muted);
        self.area.queue_draw();
    }

    pub fn set_clamp_to_grid(&self, on: bool) {
        self.clamp_grid.set(on);
    }

    pub fn set_floor_config(&self, floor: FanFloorConfig) {
        self.floor.set(floor);
        self.area.queue_draw();
    }

    pub fn set_changed(&self, callback: impl Fn(Curve) + 'static) {
        *self.changed.borrow_mut() = Some(Box::new(callback));
    }

    fn emit_changed(&self) {
        if let Some(callback) = self.changed.borrow().as_ref() {
            callback(*self.curve.borrow());
        }
        self.area.queue_draw();
    }

    fn chart_geom(w: f64, h: f64) -> (f64, f64, f64, f64) {
        (
            PAD_LEFT,
            PAD_TOP,
            (w - PAD_LEFT - PAD_RIGHT).max(1.0),
            (h - PAD_TOP - PAD_BOTTOM).max(1.0),
        )
    }

    fn install_draw(&self) {
        let curve = self.curve.clone();
        let selected = self.selected.clone();
        let muted = self.muted.clone();
        let floor = self.floor.clone();
        self.area.set_draw_func(move |_, cr, width, height| {
            let w = width as f64;
            let h = height as f64;
            let (left, top, cw, ch) = Self::chart_geom(w, h);
            let x = |temp: i32| left + (temp - 20) as f64 / 90.0 * cw;
            let y = |pwm: i32| top + (1.0 - pwm as f64 / 255.0) * ch;

            cr.set_source_rgb(0.14, 0.15, 0.17);
            let _ = cr.paint();

            // Grid.
            cr.set_line_width(1.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
            for t in (20..=110).step_by(10) {
                cr.move_to(x(t), top);
                cr.line_to(x(t), top + ch);
            }
            for p in (0..=100).step_by(20) {
                cr.move_to(left, y(curve::percent_to_pwm(p)));
                cr.line_to(left + cw, y(curve::percent_to_pwm(p)));
            }
            let _ = cr.stroke();

            // Axis labels.
            cr.set_source_rgba(0.85, 0.88, 0.92, 0.9);
            cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
            cr.set_font_size(11.0);
            for t in (20..=110).step_by(10) {
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
            cr.set_font_size(10.0);
            cr.set_source_rgba(0.7, 0.74, 0.8, 0.85);
            cr.move_to(left + cw / 2.0 - 12.0, h - 4.0);
            let _ = cr.show_text("°C");

            // High-TDP floor.
            let floor = floor.get();
            {
                cr.set_source_rgba(1.0, 0.5, 0.0, 0.55);
                cr.set_dash(&[4.0, 4.0], 0.0);
                cr.move_to(x(floor.engage_temp_c), y(i32::from(floor.duty)));
                cr.line_to(left + cw, y(i32::from(floor.duty)));
                let _ = cr.stroke();
                cr.set_dash(&[], 0.0);
                cr.set_font_size(10.0);
                cr.move_to(x(floor.engage_temp_c) + 4.0, y(i32::from(floor.duty)) - 4.0);
                let _ = cr.show_text(&format!(
                    "{}% floor above {}°C at >75W",
                    curve::pwm_to_percent(i32::from(floor.duty)),
                    floor.engage_temp_c
                ));
            }

            let points = *curve.borrow();
            if muted.get() {
                cr.set_source_rgba(0.55, 0.55, 0.55, 0.5);
            } else {
                cr.set_source_rgb(0.23, 0.68, 0.94);
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
                    cr.set_source_rgb(1.0, 0.5, 0.0);
                    // Hover-style tooltip near the point.
                    cr.set_font_size(11.0);
                    let tip = format!("{}°C, {}%", pt[0], curve::pwm_to_percent(pt[1]));
                    cr.move_to(x(pt[0]) + 8.0, y(pt[1]) - 8.0);
                    let _ = cr.show_text(&tip);
                    cr.set_source_rgb(1.0, 0.5, 0.0);
                } else {
                    cr.set_source_rgb(0.92, 0.95, 0.98);
                }
                cr.arc(x(pt[0]), y(pt[1]), 5.5, 0.0, std::f64::consts::TAU);
                let _ = cr.fill();
            }
        });
    }

    fn install_input(&self) {
        let gesture = gtk::GestureDrag::new();
        let editor = self.clone();
        // Track whether this is the first update of a drag so we don't
        // surprise-snap into a clamp-to-grid band mid-gesture start.
        let drag_origin = Rc::new(Cell::new((0.0_f64, 0.0_f64)));
        let origin = drag_origin.clone();
        gesture.connect_drag_begin(move |_, px, py| {
            editor.area.grab_focus();
            let idx = editor.closest_point(px, py);
            editor.selected.set(idx);
            origin.set((px, py));
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
            curve[idx][0] += dt;
            curve[idx][1] += curve::percent_to_pwm(dp) - curve::percent_to_pwm(0);
            curve::enforce_curve(&mut curve, idx, editor.clamp_grid.get());
            drop(curve);
            editor.emit_changed();
            glib::Propagation::Stop
        });
        self.area.add_controller(keys);
    }

    fn closest_point(&self, px: f64, py: f64) -> usize {
        let w = self.area.width() as f64;
        let h = self.area.height() as f64;
        let (left, top, cw, ch) = Self::chart_geom(w, h);
        let x = |t: i32| left + (t - 20) as f64 / 90.0 * cw;
        let y = |p: i32| top + (1.0 - p as f64 / 255.0) * ch;
        self.curve
            .borrow()
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
        let (left, top, cw, ch) = Self::chart_geom(w, h);
        let temp = (20.0 + (px - left) / cw * 90.0).round() as i32;
        let pwm = ((1.0 - (py - top) / ch) * 255.0).round() as i32;
        let mut curve = self.curve.borrow_mut();
        if vertical_only {
            let delta = pwm - curve[idx][1];
            curve::shift_curve_vertical(&mut curve, delta);
        } else {
            curve[idx][0] = temp;
            curve[idx][1] = pwm;
            curve::enforce_curve(&mut curve, idx, self.clamp_grid.get());
        }
        drop(curve);
        self.emit_changed();
    }
}
