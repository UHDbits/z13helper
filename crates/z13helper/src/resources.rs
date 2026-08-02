use gtk4 as gtk;

pub fn register() {
    gio::resources_register_include!("z13helper.gresource")
        .expect("embedded z13helper resources must be valid");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::IconTheme::for_display(&display)
            .add_resource_path("/com/ashtonantila/z13helper/icons");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn bundled_battery_icon_is_present() {
        let bytes = glib::Bytes::from_static(include_bytes!(concat!(
            env!("OUT_DIR"),
            "/z13helper.gresource"
        )));
        let resource = gio::Resource::from_data(&bytes).unwrap();
        let icon = resource
            .lookup_data(
                "/com/ashtonantila/z13helper/icons/scalable/status/z13helper-battery-limit-symbolic.svg",
                gio::ResourceLookupFlags::NONE,
            )
            .unwrap();
        assert!(icon.as_ref().starts_with(b"<?xml"));
    }
}
