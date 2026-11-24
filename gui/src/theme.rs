//! Proustite (the `proustite` crate, generated from Proustite.qml; the QML
//! window's ui/services/Theme.qml is a facade over the same singleton) and
//! the window's controls: AntiWebUI's md3e components in its scheme.

use anti_webui::prelude::*;
pub use anti_webui_md3e::components::CheckState;
use anti_webui_md3e::components::{
    ButtonSize, ButtonStyle, CheckboxStyle, connected_button_group, filter_chip,
    indeterminate_wavy_progress, outlined_card, styled_button, styled_checkbox,
    styled_tri_state_checkbox,
};
pub use proustite::*;
use std::rc::Rc;

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
/// the tabs. `is_on` and `pick` go by position. While `disabled` holds, all
/// of them are greyed (the one that is on keeps its shape and a faint fill)
/// and take no press, as md3e drops their handlers: it should hold exactly
/// while a press would do nothing.
pub fn segmented(
    labels: Vec<&'static str>,
    is_on: impl Fn(usize) -> bool + 'static,
    pick: impl Fn(usize) + 'static,
    disabled: impl Fn() -> bool + 'static,
) -> Div {
    connected_button_group(
        labels,
        ButtonStyle::tonal().size(ButtonSize::ExtraSmall).disabled_when(disabled),
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
