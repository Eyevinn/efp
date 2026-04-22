use gst::glib;

pub mod efpdemux;
pub mod efpmux;

fn plugin_init(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    efpmux::register(plugin)?;
    efpdemux::register(plugin)?;
    Ok(())
}

gst::plugin_define!(
    efp,
    env!("CARGO_PKG_DESCRIPTION"),
    plugin_init,
    concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
    "MIT",
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_NAME"),
    env!("CARGO_PKG_REPOSITORY"),
    env!("BUILD_REL_DATE")
);
