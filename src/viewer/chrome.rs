use std::{cell::RefCell, rc::Rc, time::Duration};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;

use super::{hotkeys::ViewerHotkeys, keyboard::GuestShortcut};

const FULLSCREEN_HOVER_HIDE_DELAY: Duration = Duration::from_millis(900);
const VIEWER_CSS: &str = r#"
.viewer-floating-controls {
    background: rgba(28, 28, 32, 0.92);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 999px;
    padding: 6px 8px;
    box-shadow:
        inset 0 1px 0 rgba(255, 255, 255, 0.08),
        inset 0 -1px 0 rgba(0, 0, 0, 0.28);
}

.viewer-floating-controls button,
.viewer-floating-controls menubutton > button {
    min-width: 42px;
    min-height: 42px;
    padding: 0;
    border-radius: 999px;
    background: transparent;
    border-color: transparent;
    box-shadow: none;
}

.viewer-floating-controls menubutton {
    min-width: 42px;
    min-height: 42px;
}

.viewer-floating-controls button:hover,
.viewer-floating-controls button:active,
.viewer-floating-controls button:checked,
.viewer-floating-controls menubutton > button:hover,
.viewer-floating-controls menubutton > button:active,
.viewer-floating-controls menubutton > button:checked {
    background: rgba(255, 255, 255, 0.1);
}

.viewer-title {
    font-weight: 600;
}

.viewer-popover {
    padding: 6px;
}

.viewer-popover button {
    padding: 8px 12px;
    border-radius: 10px;
}
"#;

#[derive(Default)]
pub(super) struct FullscreenChromeState {
    hide_source: Option<glib::SourceId>,
}

pub(super) fn install_viewer_css(display: &gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(VIEWER_CSS);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

pub(super) fn build_viewer_controls(
    window: &gtk::Window,
    app_icon: Option<&gdk::Texture>,
    hotkeys: ViewerHotkeys,
    keyboard_available: bool,
    take_screenshot: Rc<dyn Fn()>,
    send_guest_shortcut: Rc<dyn Fn(GuestShortcut)>,
) -> (gtk::Box, gtk::Button) {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let hotkeys = Rc::new(hotkeys);

    let fullscreen_button = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    fullscreen_button.add_css_class("flat");
    fullscreen_button.add_css_class("circular");
    fullscreen_button.set_tooltip_text(Some("Fullscreen"));
    fullscreen_button.connect_clicked({
        let window = window.clone();
        move |_| toggle_fullscreen(&window)
    });

    let keyboard_popover = gtk::Popover::new();
    keyboard_popover.set_has_arrow(false);
    keyboard_popover.add_css_class("viewer-popover");

    let keyboard_popover_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for shortcut in GuestShortcut::all() {
        let button = gtk::Button::with_label(shortcut.label());
        button.set_halign(gtk::Align::Fill);
        button.add_css_class("flat");
        button.set_can_focus(false);
        button.set_focus_on_click(false);
        button.set_receives_default(false);
        button.connect_clicked({
            let keyboard_popover = keyboard_popover.clone();
            let send_guest_shortcut = send_guest_shortcut.clone();
            let shortcut = *shortcut;
            move |_| {
                keyboard_popover.popdown();
                send_guest_shortcut(shortcut);
            }
        });
        keyboard_popover_box.append(&button);
    }
    keyboard_popover.set_child(Some(&keyboard_popover_box));

    let keyboard_menu_button = gtk::MenuButton::new();
    keyboard_menu_button.set_icon_name("input-keyboard-symbolic");
    keyboard_menu_button.set_tooltip_text(Some("Send guest shortcut"));
    keyboard_menu_button.add_css_class("flat");
    keyboard_menu_button.add_css_class("circular");
    keyboard_menu_button.set_can_focus(false);
    keyboard_menu_button.set_focus_on_click(false);
    keyboard_menu_button.set_receives_default(false);
    keyboard_menu_button.set_visible(keyboard_available);
    keyboard_menu_button.set_popover(Some(&keyboard_popover));

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("viewer-popover");

    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 4);

    let screenshot_button = gtk::Button::with_label("Take Screenshot");
    screenshot_button.set_halign(gtk::Align::Fill);
    screenshot_button.add_css_class("flat");
    screenshot_button.set_can_focus(false);
    screenshot_button.set_focus_on_click(false);
    screenshot_button.set_receives_default(false);
    screenshot_button.connect_clicked({
        let popover = popover.clone();
        let take_screenshot = take_screenshot.clone();
        move |_| {
            popover.popdown();
            take_screenshot();
        }
    });
    popover_box.append(&screenshot_button);

    let shortcuts_button = gtk::Button::with_label("Keyboard Shortcuts");
    shortcuts_button.set_halign(gtk::Align::Fill);
    shortcuts_button.add_css_class("flat");
    shortcuts_button.set_can_focus(false);
    shortcuts_button.set_focus_on_click(false);
    shortcuts_button.set_receives_default(false);
    shortcuts_button.connect_clicked({
        let popover = popover.clone();
        let window = window.clone();
        let hotkeys = hotkeys.clone();
        move |_| {
            popover.popdown();
            show_keyboard_shortcuts(&window, hotkeys.as_ref());
        }
    });
    popover_box.append(&shortcuts_button);

    let about_button = gtk::Button::with_label("About");
    about_button.set_halign(gtk::Align::Fill);
    about_button.add_css_class("flat");
    about_button.set_can_focus(false);
    about_button.set_focus_on_click(false);
    about_button.set_receives_default(false);
    about_button.connect_clicked({
        let popover = popover.clone();
        let window = window.clone();
        let app_icon = app_icon.cloned();
        move |_| {
            popover.popdown();
            show_about_dialog(&window, app_icon.clone());
        }
    });
    popover_box.append(&about_button);

    popover.set_child(Some(&popover_box));

    let menu_button = gtk::MenuButton::new();
    menu_button.set_icon_name("view-more-symbolic");
    menu_button.set_tooltip_text(Some("More"));
    menu_button.add_css_class("flat");
    menu_button.add_css_class("circular");
    menu_button.set_can_focus(false);
    menu_button.set_focus_on_click(false);
    menu_button.set_receives_default(false);
    menu_button.set_popover(Some(&popover));

    controls.append(&fullscreen_button);
    controls.append(&keyboard_menu_button);
    controls.append(&menu_button);

    (controls, fullscreen_button)
}

fn show_keyboard_shortcuts(window: &gtk::Window, hotkeys: &ViewerHotkeys) {
    let shortcuts = gtk::ShortcutsWindow::builder()
        .title("Keyboard Shortcuts")
        .modal(true)
        .transient_for(window)
        .build();

    let section = gtk::ShortcutsSection::builder()
        .title("Viewer")
        .section_name("viewer")
        .build();
    let group = gtk::ShortcutsGroup::builder().title("Display").build();

    for (title, accelerator) in hotkeys.shortcuts_for_dialog() {
        let shortcut = gtk::ShortcutsShortcut::builder()
            .title(title)
            .accelerator(&accelerator)
            .build();
        group.add_shortcut(&shortcut);
    }

    section.add_group(&group);
    shortcuts.add_section(&section);
    shortcuts.present();
}

fn show_about_dialog(window: &gtk::Window, app_icon: Option<gdk::Texture>) {
    let about = gtk::AboutDialog::new();
    about.set_transient_for(Some(window));
    about.set_modal(true);
    about.set_program_name(Some("QD2"));
    about.set_version(Some(env!("CARGO_PKG_VERSION")));
    about.set_comments(Some("QEMU D-Bus Display client"));
    about.set_website(Some(env!("CARGO_PKG_REPOSITORY")));
    about.set_website_label("GitHub");
    if let Some(icon) = app_icon.as_ref() {
        about.set_logo(Some(icon));
    }
    about.connect_map(|dialog| disable_text_selection(dialog.upcast_ref()));
    about.present();
}

fn disable_text_selection(widget: &gtk::Widget) {
    if let Ok(label) = widget.clone().downcast::<gtk::Label>() {
        label.set_selectable(false);
    }

    if let Ok(text_view) = widget.clone().downcast::<gtk::TextView>() {
        text_view.set_editable(false);
        text_view.set_cursor_visible(false);
        text_view.set_focusable(false);
    }

    let mut child = widget.first_child();
    while let Some(current) = child {
        disable_text_selection(&current);
        child = current.next_sibling();
    }
}

pub(super) fn toggle_fullscreen(window: &gtk::Window) {
    if window.is_fullscreen() {
        window.unfullscreen();
    } else {
        window.fullscreen();
    }
}

fn update_fullscreen_button(button: &gtk::Button, is_fullscreen: bool) {
    if is_fullscreen {
        button.set_icon_name("view-restore-symbolic");
    } else {
        button.set_icon_name("view-fullscreen-symbolic");
    }
    button.set_tooltip_text(Some("Toggle fullscreen"));
}

fn cancel_fullscreen_bar_hide(state: &Rc<RefCell<FullscreenChromeState>>) {
    if let Some(source) = state.borrow_mut().hide_source.take() {
        source.remove();
    }
}

pub(super) fn reveal_fullscreen_bar(
    revealer: &gtk::Revealer,
    state: &Rc<RefCell<FullscreenChromeState>>,
) {
    cancel_fullscreen_bar_hide(state);
    revealer.set_visible(true);
    revealer.set_reveal_child(true);
}

pub(super) fn schedule_hide_fullscreen_bar(
    window: &gtk::Window,
    revealer: &gtk::Revealer,
    state: &Rc<RefCell<FullscreenChromeState>>,
) {
    if !window.is_fullscreen() {
        return;
    }

    cancel_fullscreen_bar_hide(state);

    let revealer = revealer.clone();
    let state_handle = state.clone();
    let source = glib::timeout_add_local_once(FULLSCREEN_HOVER_HIDE_DELAY, move || {
        revealer.set_reveal_child(false);
        revealer.set_visible(false);
        state_handle.borrow_mut().hide_source = None;
    });
    state.borrow_mut().hide_source = Some(source);
}

/// Swap between the native titlebar and the floating fullscreen controls so the
/// fullscreen window behaves like virt-viewer instead of a stock GTK header bar.
pub(super) fn sync_fullscreen_chrome(
    window: &gtk::Window,
    header_bar: &gtk::Widget,
    fullscreen_revealer: &gtk::Revealer,
    fullscreen_hotspot: &gtk::Box,
    fullscreen_buttons: &[gtk::Button],
    fullscreen_state: &Rc<RefCell<FullscreenChromeState>>,
    decorated_window: bool,
) {
    let is_fullscreen = window.is_fullscreen();
    for button in fullscreen_buttons {
        update_fullscreen_button(button, is_fullscreen);
    }

    if is_fullscreen {
        window.set_titlebar(None::<&gtk::Widget>);
        fullscreen_hotspot.set_visible(true);
        reveal_fullscreen_bar(fullscreen_revealer, fullscreen_state);
        schedule_hide_fullscreen_bar(window, fullscreen_revealer, fullscreen_state);
    } else {
        cancel_fullscreen_bar_hide(fullscreen_state);
        fullscreen_revealer.set_reveal_child(false);
        fullscreen_revealer.set_visible(false);
        fullscreen_hotspot.set_visible(false);
        if decorated_window {
            window.set_titlebar(Some(header_bar));
        } else {
            window.set_titlebar(None::<&gtk::Widget>);
        }
    }
}
