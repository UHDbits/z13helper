//! z13-helper — G-Helper-style GUI for z13ctl.
fn main() {
    // GTK_A11Y=none avoids AT-SPI D-Bus timeouts that block GTK init (z13gui lesson).
    if std::env::var_os("GTK_A11Y").is_none() {
        // SAFETY: set before any GTK/GLib init.
        unsafe { std::env::set_var("GTK_A11Y", "none") };
    }
    println!("z13-helper {} — GUI not yet built", env!("CARGO_PKG_VERSION"));
}
