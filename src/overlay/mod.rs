// Display logic is GTK-free so it's unit-tested in every build,
// but it's only used when the overlay itself is compiled in.
#[cfg_attr(
    not(all(feature = "layer-shell", target_os = "linux")),
    allow(dead_code)
)]
mod view;

#[cfg(all(feature = "layer-shell", target_os = "linux"))]
mod gtk_overlay;
#[cfg(all(feature = "layer-shell", target_os = "linux"))]
pub use gtk_overlay::spawn_overlay;
