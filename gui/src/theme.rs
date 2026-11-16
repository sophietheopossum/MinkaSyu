//! Proustite tokens and the few controls the window draws itself. The
//! values mirror the shared Proustite QML singleton (MinkaSyu's
//! ui/services/Theme.qml facade); keep them in sync with
//! ../../Proustite/Proustite.qml, as MinkaMon's and MinkaShell's theme.rs do.

use anti_webui::prelude::*;
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

pub const FONT_FAMILY: &str = "Noto Sans";
pub const MONO_FAMILY: &str = "monospace";
pub const FONT_SIZE: f32 = 13.0_f32;

pub fn color(hex: u32) -> Color {
    Color::from_rgb(hex)
}

pub fn color_alpha(hex: u32, alpha: f32) -> Color {
    Color::from_rgb(hex).with_alpha(alpha.clamp(0.0_f32, 1.0_f32))
}

/// The toolkit's own widgets (text fields) default to a light theme.
pub fn install_widget_theme() {
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

/// The QML PushButton: quiet, toggled (purple), or primary (red).
pub fn push_button(
    label: impl Fn() -> String + 'static,
    primary: bool,
    toggled: impl Fn() -> bool + 'static,
    enabled: impl Fn() -> bool + 'static,
    on_click: impl Fn() + 'static,
) -> Div {
    let enabled = Rc::new(enabled);
    let for_click = Rc::clone(&enabled);
    let hovered = signal(false);
    div()
        .h(30.0_f32)
        .px(13.0_f32)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .rounded(5.0_f32)
        .border(1.0_f32)
        .select_none()
        .on_hover(move |on| hovered.set_if_changed(on))
        .dyn_style(move |s| {
            let on = enabled();
            let hover = on && hovered.get();
            let s = s.opacity(if on { 1.0_f32 } else { 0.4_f32 }).cursor(if on {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            });
            if !on {
                s.bg(color(SURFACE))
                    .border_color(color(if primary { RED } else { LINE }))
            } else if primary {
                s.bg(color(if hover { RED } else { RED_DIM }))
                    .border_color(color(RED))
            } else if toggled() {
                s.bg(color(PURPLE_DIM)).border_color(color(PURPLE))
            } else {
                s.bg(color(if hover { SURFACE_RAISED } else { SURFACE }))
                    .border_color(color(LINE))
            }
        })
        .on_click(move |e: &PointerEvent| {
            e.stop_propagation();
            if for_click() {
                on_click();
            }
        })
        .child(ui_text(label, FONT_SIZE - 1.0_f32, TEXT).whitespace_nowrap())
}

/// The QML CheckBox: 18 px box, purple when ticked, faded when disabled.
pub fn check_box(
    checked: impl Fn() -> bool + 'static,
    enabled: impl Fn() -> bool + 'static,
    on_toggle: impl Fn() + 'static,
) -> Div {
    let checked = Rc::new(checked);
    let for_mark = Rc::clone(&checked);
    let enabled = Rc::new(enabled);
    let for_click = Rc::clone(&enabled);
    let hovered = signal(false);
    div()
        .w(26.0_f32)
        .h(26.0_f32)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .select_none()
        .on_hover(move |on| hovered.set_if_changed(on))
        .dyn_style(move |s| {
            let on = enabled();
            s.opacity(if on { 1.0_f32 } else { 0.35_f32 })
                .cursor(if on {
                    CursorIcon::Pointer
                } else {
                    CursorIcon::Default
                })
        })
        .on_click(move |e: &PointerEvent| {
            e.stop_propagation();
            if for_click() {
                on_toggle();
            }
        })
        .child(
            div()
                .w(18.0_f32)
                .h(18.0_f32)
                .rounded(4.0_f32)
                .border(1.0_f32)
                .flex()
                .items_center()
                .justify_center()
                .dyn_style(move |s| {
                    if checked() {
                        s.bg(color(PURPLE_DIM)).border_color(color(PURPLE))
                    } else {
                        s.bg(color(SURFACE)).border_color(color(if hovered.get() {
                            TEXT_MUTED
                        } else {
                            LINE
                        }))
                    }
                })
                .child(
                    ui_text(
                        move || if for_mark() { "✓" } else { "" },
                        FONT_SIZE - 2.0_f32,
                        TEXT,
                    )
                    .bold(),
                ),
        )
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
