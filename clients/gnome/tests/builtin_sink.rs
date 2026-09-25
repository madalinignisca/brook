//! The GTK 4 video sink is linked in: with no system GStreamer plugins at all,
//! registering it makes `gtk4paintablesink` available (the Flatpak and tarball
//! rely on this, not on a distro package).

#[test]
fn the_linked_in_sink_registers_without_system_plugins() {
    let registry =
        std::env::temp_dir().join(format!("brook-gst-registry-{}.bin", std::process::id()));
    // Set before GStreamer initialises; this test binary has only this test.
    std::env::set_var("GST_PLUGIN_SYSTEM_PATH_1_0", "");
    std::env::set_var("GST_PLUGIN_PATH_1_0", "");
    std::env::set_var("GST_REGISTRY_1_0", &registry);
    gst::init().unwrap();
    assert!(
        gst::ElementFactory::find("gtk4paintablesink").is_none(),
        "system plugins leaked in"
    );
    gstgtk4::plugin_register_static().unwrap();
    assert!(gst::ElementFactory::find("gtk4paintablesink").is_some());
    let _ = std::fs::remove_file(registry);
}
