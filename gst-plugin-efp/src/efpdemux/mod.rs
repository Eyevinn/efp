use gst::glib;
use gst::prelude::*;

mod imp;

glib::wrapper! {
    pub struct EfpDemux(ObjectSubclass<imp::EfpDemux>) @extends gst::Element, gst::Object;
}

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(
        Some(plugin),
        "efpdemux",
        gst::Rank::NONE,
        EfpDemux::static_type(),
    )
}
