//! Shared DPI update policy for startup and live native scale-factor events.
use crate::{Appearance, MainWindow};
use slint::ComponentHandle;

pub fn apply_scale(window: &MainWindow, scale: f64) -> bool {
    let scale = scale as f32;
    if !scale.is_finite() || scale <= 0.0 {
        return false;
    }
    let appearance = window.global::<Appearance>();
    if appearance.get_dpi() == scale {
        return false;
    }
    appearance.set_dpi(scale);
    true
}
