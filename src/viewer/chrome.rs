use std::{cell::RefCell, rc::Rc, time::Duration};

use gtk::{gdk, glib, prelude::*};
use gtk4 as gtk;

const FULLSCREEN_HOVER_HIDE_DELAY: Duration = Duration::from_millis(900);
const VIEWER_CSS: &str = r#"
.viewer-floating-controls {
    background: rgba(28, 28, 32, 0.92);
    border: 1px solid rgba(255, 255, 255, 0.1);
    border-radius: 999px;
    padding: 6px 8px;
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.35);
}

.viewer-floating-controls button,
.viewer-floating-controls menubutton {
    min-width: 34px;
    min-height: 34px;
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
) -> (gtk::Box, gtk::Button) {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);

    let fullscreen_button = gtk::Button::from_icon_name("view-fullscreen-symbolic");
    fullscreen_button.add_css_class("flat");
    fullscreen_button.add_css_class("circular");
    fullscreen_button.set_tooltip_text(Some("Fullscreen"));
    fullscreen_button.connect_clicked({
        let window = window.clone();
        move |_| toggle_fullscreen(&window)
    });

    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("viewer-popover");

    let popover_box = gtk::Box::new(gtk::Orientation::Vertical, 4);

    let shortcuts_button = gtk::Button::with_label("Keyboard shortcuts");
    shortcuts_button.set_halign(gtk::Align::Fill);
    shortcuts_button.add_css_class("flat");
    shortcuts_button.connect_clicked({
        let popover = popover.clone();
        let window = window.clone();
        move |_| {
            popover.popdown();
            show_keyboard_shortcuts(&window);
        }
    });
    popover_box.append(&shortcuts_button);

    let about_button = gtk::Button::with_label("About");
    about_button.set_halign(gtk::Align::Fill);
    about_button.add_css_class("flat");
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
    menu_button.set_popover(Some(&popover));

    controls.append(&fullscreen_button);
    controls.append(&menu_button);

    (controls, fullscreen_button)
}

fn show_keyboard_shortcuts(window: &gtk::Window) {
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

    for (title, accelerator) in [
        ("Toggle fullscreen", "F11"),
        ("Leave fullscreen", "Escape"),
        ("Rotate DMABUF view", "<Control><Alt>r"),
        ("Toggle DMABUF vertical flip", "<Control><Alt>f"),
        ("Reset DMABUF transform", "<Control><Alt>0"),
    ] {
        let shortcut = gtk::ShortcutsShortcut::builder()
            .title(title)
            .accelerator(accelerator)
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
    if let Some(icon) = app_icon.as_ref() {
        about.set_logo(Some(icon));
    }
    about.present();
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
        button.set_tooltip_text(Some("Leave fullscreen"));
    } else {
        button.set_icon_name("view-fullscreen-symbolic");
        button.set_tooltip_text(Some("Fullscreen"));
    }
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
        window.set_titlebar(Some(header_bar));
    }
}
