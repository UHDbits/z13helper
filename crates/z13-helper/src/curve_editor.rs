use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk4 as gtk;
use z13_helper_core::curve::{self, Curve, POINT_COUNT};

type ChangedCallback = Box<dyn Fn(Curve)>;

#[derive(Clone)]
pub struct CurveEditor {
    area: gtk::DrawingArea,
    curve: Rc<RefCell<Curve>>,
    selected: Rc<Cell<usize>>,
    muted: Rc<Cell<bool>>,
    clamp_grid: Rc<Cell<bool>>,
    changed: Rc<RefCell<Option<ChangedCallback>>>,
}

impl CurveEditor {
    pub fn new(curve: Curve, clamp_grid: bool) -> Self {
        let area = gtk::DrawingArea::builder()
            .content_width(420)
            .content_height(300)
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

    #[allow(dead_code)]
    pub fn set_curve(&self, curve: Curve) {
        *self.curve.borrow_mut() = curve;
        self.area.queue_draw();
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.set(muted);
        self.area.queue_draw();
    }

    #[allow(dead_code)]
    pub fn set_changed(&self, callback: impl Fn(Curve) + 'static) {
        *self.changed.borrow_mut() = Some(Box::new(callback));
    }

    fn emit_changed(&self) {
        if let Some(callback) = self.changed.borrow().as_ref() {
            callback(*self.curve.borrow());
        }
        self.area.queue_draw();
    }

    fn install_draw(&self) {
        let curve = self.curve.clone();
        let selected = self.selected.clone();
        let muted = self.muted.clone();
        self.area.set_draw_func(move |_, cr, width, height| {
            let w = width as f64;
            let h = height as f64;
            let left = 42.0;
            let right = 16.0;
            let top = 16.0;
            let bottom = 30.0;
            let x = |temp: i32| left + (temp - 20) as f64 / 90.0 * (w - left - right);
            let y = |pwm: i32| top + (1.0 - pwm as f64 / 255.0) * (h - top - bottom);
            cr.set_source_rgb(0.16, 0.18, 0.21);
            let _ = cr.paint();
            cr.set_line_width(1.0);
            cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
            for t in (20..=110).step_by(10) {
                cr.move_to(x(t), top);
                cr.line_to(x(t), h - bottom);
            }
            for p in (0..=100).step_by(20) {
                cr.move_to(left, y(curve::percent_to_pwm(p)));
                cr.line_to(w - right, y(curve::percent_to_pwm(p)));
            }
            let _ = cr.stroke();
            // Required high-TDP floor reference.
            cr.set_source_rgba(1.0, 0.5, 0.0, 0.55);
            cr.set_dash(&[4.0, 4.0], 0.0);
            cr.move_to(left, y(curve::HIGH_TDP_MIN_PWM));
            cr.line_to(w - right, y(curve::HIGH_TDP_MIN_PWM));
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);
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
                } else {
                    cr.set_source_rgb(0.92, 0.95, 0.98);
                }
                cr.arc(x(pt[0]), y(pt[1]), 5.0, 0.0, std::f64::consts::TAU);
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
        keys.connect_key_pressed(move |_, key, _, _| {
            let mut idx = editor.selected.get();
            if key == gtk::gdk::Key::Tab {
                idx = (idx + 1) % POINT_COUNT;
                editor.selected.set(idx);
                editor.area.queue_draw();
                return glib::Propagation::Stop;
            }
            let (dt, dp) = match key {
                gtk::gdk::Key::Left => (-1, 0),
                gtk::gdk::Key::Right => (1, 0),
                gtk::gdk::Key::Up => (0, 5),
                gtk::gdk::Key::Down => (0, -5),
                _ => return glib::Propagation::Proceed,
            };
            let mut curve = editor.curve.borrow_mut();
            curve[idx][0] += dt;
            curve[idx][1] += curve::percent_to_pwm(dp) - curve::percent_to_pwm(0);
            curve::enforce_curve(&mut curve, idx, 0, editor.clamp_grid.get());
            drop(curve);
            editor.emit_changed();
            glib::Propagation::Stop
        });
        self.area.add_controller(keys);
    }

    fn closest_point(&self, px: f64, py: f64) -> usize {
        let w = self.area.width() as f64;
        let h = self.area.height() as f64;
        let x = |t: i32| 42.0 + (t - 20) as f64 / 90.0 * (w - 58.0);
        let y = |p: i32| 16.0 + (1.0 - p as f64 / 255.0) * (h - 46.0);
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
        let temp = (20.0 + (px - 42.0) / (w - 58.0) * 90.0).round() as i32;
        let pwm = ((1.0 - (py - 16.0) / (h - 46.0)) * 255.0).round() as i32;
        let mut curve = self.curve.borrow_mut();
        if !vertical_only {
            curve[idx][0] = temp;
        }
        curve[idx][1] = pwm;
        curve::enforce_curve(&mut curve, idx, 0, self.clamp_grid.get());
        drop(curve);
        self.emit_changed();
    }
}
