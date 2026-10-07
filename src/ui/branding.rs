//! Render every native icon from the same SVG used by QML and desktop entries.

pub const LOGO_SVG: &[u8] = include_bytes!("../../assets/com.unboundmark.Celestial-regular.svg");

#[cxx::bridge(namespace = "celestial::branding")]
mod ffi {
    unsafe extern "C++" {
        include!("src/ui/branding.h");
        fn logo_argb(svg: &[u8], size: i32) -> Result<Vec<u8>>;
        fn set_window_icon(svg: &[u8]) -> Result<()>;
    }
}

pub fn logo_argb(size: i32) -> Result<Vec<u8>, String> {
    ffi::logo_argb(LOGO_SVG, size).map_err(|err| err.to_string())
}

/// Call on the GUI thread, after constructing QGuiApplication.
pub fn set_window_icon() -> Result<(), String> {
    ffi::set_window_icon(LOGO_SVG).map_err(|err| err.to_string())
}
