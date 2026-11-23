//! Proustite tokens, the Material 3 Expressive color scheme made from them,
//! and the window's controls: AntiWebUI's md3e components in that scheme.
//! The token values mirror the shared Proustite QML singleton (MinkaSyu's
//! ui/services/Theme.qml facade); keep them in sync with
//! ../../Proustite/Proustite.qml, as MinkaMon's and MinkaShell's theme.rs do.

use anti_webui::prelude::*;
pub use anti_webui_md3e::components::CheckState;
use anti_webui_md3e::components::{
    ButtonSize, ButtonStyle, CheckboxStyle, connected_button_group, filter_chip,
    indeterminate_wavy_progress, outlined_card, styled_button, styled_checkbox,
    styled_tri_state_checkbox,
};
use anti_webui_md3e::{ColorScheme, set_color_scheme};
use std::rc::Rc;

pub const GROUND: u32 = 0x0a0709;
pub const SURFACE: u32 = 0x161013;
pub const SURFACE_RAISED: u32 = 0x1e161a;
pub const LINE: u32 = 0x2e2228;
pub const TEXT: u32 = 0xece5e7;
pub const TEXT_MUTED: u32 = 0xa3959b;
pub const TEXT_FAINT: u32 = 0x6e6167;
pub const RED: u32 = 0xff0000;
pub const RED_DIM: u32 = 0x8f1e2d;
pub const PURPLE: u32 = 0xa488c9;
pub const PURPLE_DIM: u32 = 0x4c3a63;
pub const GAUGE_DIM: u32 = 0x3a1219;
pub const OK_GREEN: u32 = 0x7dc98c;
pub const WARN_AMBER: u32 = 0xe0a026;

/// The md3e components set their own labels in this family (Roboto Flex,
/// which comes with the crate), so the rest of the window uses it too.
pub const FONT_FAMILY: &str = anti_webui_md3e::typography::FONT_FAMILY;
pub const MONO_FAMILY: &str = "monospace";
pub const FONT_SIZE: f32 = 13.0_f32;

pub fn color(hex: u32) -> Color {
    Color::from_rgb(hex)
}

pub fn color_alpha(hex: u32, alpha: f32) -> Color {
    Color::from_rgb(hex).with_alpha(alpha.clamp(0.0_f32, 1.0_f32))
}

/// Proustite's tokens in Material's color roles: what the md3e components
/// draw with. The roles Proustite has no token for (the error ones and the
/// scrim) are the ones Material grows from Proustite's red.
pub fn proustite_scheme() -> ColorScheme {
    ColorScheme {
        primary: color(RED),
        on_primary: color(GROUND),
        primary_container: color(RED_DIM),
        on_primary_container: color(TEXT),
        secondary: color(PURPLE),
        on_secondary: color(GROUND),
        secondary_container: color(PURPLE_DIM),
        on_secondary_container: color(TEXT),
        tertiary: color(WARN_AMBER),
        on_tertiary: color(GROUND),
        tertiary_container: color(GAUGE_DIM),
        on_tertiary_container: color(TEXT),
        surface: color(GROUND),
        surface_dim: color(GROUND),
        surface_bright: color(LINE),
        surface_variant: color(SURFACE_RAISED),
        on_surface: color(TEXT),
        on_surface_variant: color(TEXT_MUTED),
        inverse_surface: color(TEXT),
        inverse_on_surface: color(GROUND),
        inverse_primary: color(RED_DIM),
        surface_container_lowest: color(GROUND),
        surface_container_low: color(SURFACE),
        surface_container: color(SURFACE),
        surface_container_high: color(SURFACE_RAISED),
        surface_container_highest: color(LINE),
        outline: color(TEXT_FAINT),
        outline_variant: color(LINE),
        ..ColorScheme::from_seed(color(RED), true)
    }
}

/// The colors of both widget sets: the md3e components take the scheme, and
/// the toolkit's own widgets (text fields), which default to a light theme,
/// take the theme below.
pub fn install_widget_theme() {
    set_color_scheme(proustite_scheme());
    set_theme(Theme {
        page: color(GROUND),
        surface: color(SURFACE),
        surface_muted: color(SURFACE),
        overlay: color(SURFACE_RAISED),
        hover: color(SURFACE_RAISED),
        pressed: color(LINE),
        selected: color(PURPLE_DIM),
        text: color(TEXT),
        text_muted: color(TEXT_MUTED),
        text_disabled: color(TEXT_FAINT),
        on_accent: color(TEXT),
        border: color(LINE),
        border_strong: color(TEXT_FAINT),
        accent: color(PURPLE_DIM),
        accent_hover: color(PURPLE),
        accent_pressed: color(PURPLE_DIM),
        accent_text: color(PURPLE),
        accent_muted: color(GAUGE_DIM),
        warning: color(WARN_AMBER),
        success: color(OK_GREEN),
        danger: color(RED),
        tooltip: color(SURFACE_RAISED),
        on_tooltip: color(TEXT),
        scrollbar: color_alpha(TEXT_FAINT, 0.6_f32),
        text_selection: color_alpha(RED_DIM, 0.5_f32),
        ..Theme::dark()
    });
}

pub fn ui_text(source: impl IntoTextSource, size: f32, hex: u32) -> Text {
    text(source)
        .font_family(FONT_FAMILY)
        .text_size(size)
        .text_color(color(hex))
}

pub fn mono_text(source: impl IntoTextSource, size: f32, hex: u32) -> Text {
    text(source)
        .font_family(MONO_FAMILY)
        .text_size(size)
        .text_color(color(hex))
}

/// Small bold caps over a list section ("REPOSITORIES", "AUR").
pub fn section_label(label: &str) -> Text {
    ui_text(label.to_string(), FONT_SIZE - 3.0_f32, TEXT_FAINT).bold()
}

/// A button: outlined, or filled in the primary color for the main action.
/// The smallest md3e size (32 px), since the window is dense.
pub fn push_button(
    label: impl Fn() -> String + 'static,
    primary: bool,
    enabled: impl Fn() -> bool + 'static,
    on_click: impl Fn() + 'static,
) -> Div {
    let enabled = Rc::new(enabled);
    let off = Rc::clone(&enabled);
    let style = if primary {
        ButtonStyle::filled()
    } else {
        ButtonStyle::outlined()
    };
    styled_button(
        label,
        style
            .size(ButtonSize::ExtraSmall)
            .disabled_when(move || !off()),
    )
    .flex_none()
    .on_click(move |e: &PointerEvent| {
        e.stop_propagation();
        if enabled() {
            on_click();
        }
    })
}

/// Buttons that touch, of which the one that is on takes a shape of its own:
/// the tabs. `is_on` and `pick` go by position.
pub fn segmented(
    labels: Vec<&'static str>,
    is_on: impl Fn(usize) -> bool + 'static,
    pick: impl Fn(usize) + 'static,
) -> Div {
    connected_button_group(
        labels,
        ButtonStyle::tonal().size(ButtonSize::ExtraSmall),
        is_on,
        pick,
    )
}

/// A chip that is on or off: a filter over a list.
pub fn filter_toggle(label: &'static str, on: Signal<bool>) -> Div {
    filter_chip(label, on).flex_none()
}

/// A filter chip whose press does `toggle` and nothing else: a clickable
/// row it sits in does not also take the press as its own.
pub fn toggle_chip(
    label: impl Fn() -> &'static str + 'static,
    on: impl Fn() -> bool + 'static,
    toggle: impl Fn() + 'static,
) -> Div {
    filter_chip(label, Binding::new(on, move |_| toggle()))
        .flex_none()
        .on_click(|e: &PointerEvent| e.stop_propagation())
}

/// The wavy line of something that takes as long as it takes, across the
/// width it is given. It is drawn into the gap under it (the bottom margin
/// takes its height back), so it costs the layout nothing and what is below
/// does not move when it comes and goes.
pub fn busy_strip(busy: impl Fn() -> bool + 'static) -> Div {
    div()
        .w_full()
        .h(BUSY_H)
        .mb(-BUSY_H)
        .flex_none()
        .child(show(busy, || indeterminate_wavy_progress().w_full()))
}

/// The height of md3e's wavy progress bar.
const BUSY_H: f32 = 10.0_f32;

/// md3e's checkbox keeps a 48 px box for touch, with its 20 px mark and its
/// hover circle centred in it. A list row should stay as high as its text,
/// so the whole 48 px box sits in a 26 px slot and hangs over it evenly:
/// shrinking the box itself would leave the hover circle off-centre.
const CHECK_TOUCH: f32 = 48.0_f32;
const CHECK_SLOT: f32 = 26.0_f32;
pub fn check_box(
    checked: impl Fn() -> bool + 'static,
    enabled: impl Fn() -> bool + 'static,
    on_toggle: impl Fn() + 'static,
) -> Div {
    let enabled = Rc::new(enabled);
    let off = Rc::clone(&enabled);
    styled_checkbox(
        Binding::new(checked, move |_| {
            if enabled() {
                on_toggle();
            }
        }),
        CheckboxStyle::new().disabled_when(move || !off()),
    )
    .flex_none()
    .m((CHECK_SLOT - CHECK_TOUCH) / 2.0_f32)
}

/// check_box for something that can be partly on (a group): md3e's
/// tri-state box, in the same 26 px slot. A press on a mix or on off turns
/// it on, on on turns it off: `set` gets which.
pub fn tri_check_box(state: impl Fn() -> CheckState + 'static, set: impl Fn(bool) + 'static) -> Div {
    styled_tri_state_checkbox(
        Binding::new(state, move |next: CheckState| set(next == CheckState::On)),
        CheckboxStyle::new(),
    )
    .flex_none()
    .m((CHECK_SLOT - CHECK_TOUCH) / 2.0_f32)
}

/// md3e's outlined card, its outline in `hex`: something to read before it
/// is dismissed. The card sets its outline in a dyn_style of its own, which
/// a plain style loses to, so the colour goes in one appended after it.
pub fn outlined_card_in(hex: u32) -> Div {
    outlined_card().dyn_style(move |s| s.border_color(color(hex)))
}

/// A filled marker: what in a list deserves attention at a glance.
pub fn chip(label: &str, bg: u32, fg: u32) -> Div {
    div()
        .flex_none()
        .px(6.0_f32)
        .py(3.0_f32)
        .rounded(4.0_f32)
        .bg(color(bg))
        .child(
            ui_text(label.to_string(), FONT_SIZE - 3.0_f32, fg)
                .bold()
                .whitespace_nowrap(),
        )
}

/// A text link-like control: muted, brighter on hover.
pub fn text_button(
    label: impl Fn() -> String + 'static,
    hex: u32,
    on_click: impl Fn() + 'static,
) -> Div {
    div()
        .flex_none()
        .cursor(CursorIcon::Pointer)
        .select_none()
        .opacity(0.85_f32)
        .hover(|s| s.opacity(1.0_f32))
        .on_click(move |e: &PointerEvent| {
            e.stop_propagation();
            on_click();
        })
        .child(
            ui_text(label, FONT_SIZE - 2.0_f32, hex)
                .underline()
                .whitespace_nowrap(),
        )
}
