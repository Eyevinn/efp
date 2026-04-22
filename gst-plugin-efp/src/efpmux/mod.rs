use gst::glib;
use gst::prelude::*;

mod imp;

pub use imp::TimestampMode;

glib::wrapper! {
    pub struct EfpMux(ObjectSubclass<imp::EfpMux>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "efpmux",
        gst::Rank::NONE,
        EfpMux::static_type(),
    )
}
