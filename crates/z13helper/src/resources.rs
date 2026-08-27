use gtk4 as gtk;

pub fn register() {
    gio::resources_register_include!("z13helper.gresource")
        .expect("embedded z13helper resources must be valid");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_resource_path("/com/ashtonantila/z13helper/icons");
    }
}
