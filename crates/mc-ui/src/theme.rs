//! Colours, sized for a phone held in one hand in variable light.

/// Page background.
pub const GROUND: (u8, u8, u8) = (12, 19, 21);
/// Raised surfaces: tool cards, the composer.
pub const SURFACE: (u8, u8, u8) = (22, 34, 38);
/// Hairline between the app bar and the content below it.
pub const DIVIDER: (u8, u8, u8) = (36, 52, 57);
/// Primary text.
pub const INK: (u8, u8, u8) = (223, 232, 234);
/// Secondary text: status, notices, tool output.
pub const MUTED: (u8, u8, u8) = (133, 150, 155);
/// Accent, used for the agent's own activity.
pub const ACCENT: (u8, u8, u8) = (91, 187, 176);
/// The user's own messages - the accent, dimmed so text on it stays legible.
pub const ACCENT_DIM: (u8, u8, u8) = (22, 64, 60);
/// Errors and failed tool calls.
pub const DANGER: (u8, u8, u8) = (219, 124, 109);
pub const DANGER_DIM: (u8, u8, u8) = (52, 28, 25);

/// Comfortable tap target. Below roughly this, one-handed use starts to miss.
pub const TAP_TARGET: f32 = 48.0;

/// Freya's built-in component theme, set to match the colours above.
///
/// Without this Freya renders its own components (buttons, inputs) with the
/// *light* theme on top of this app's dark surfaces: flat buttons draw dark text
/// on a dark bar, and filled ones use Freya's default blue. It also decides the
/// Android status bar: `freya-android` picks dark status-bar icons whenever the
/// theme is named "light", which left the clock unreadable on the dark app bar.
pub fn freya_theme() -> freya::prelude::Theme {
    use freya::prelude::{Color, dark_theme};

    let rgb = |(r, g, b): (u8, u8, u8)| Color::from_rgb(r, g, b);
    let mut theme = dark_theme();
    let colors = &mut theme.colors;
    colors.primary = rgb(ACCENT);
    colors.secondary = rgb(ACCENT_DIM);
    colors.background = rgb(GROUND);
    colors.surface_primary = rgb(SURFACE);
    colors.text_primary = rgb(INK);
    colors.text_secondary = rgb(MUTED);
    colors.text_placeholder = rgb(MUTED);
    colors.border = rgb(DIVIDER);
    colors.border_focus = rgb(ACCENT);
    colors.error = rgb(DANGER);
    // Text drawn on the accent (a filled button) needs the dark ground colour
    // to stay legible on teal.
    colors.text_inverse = rgb(GROUND);
    theme
}
