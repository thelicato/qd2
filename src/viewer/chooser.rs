use std::{cell::RefCell, rc::Rc};

use anyhow::{Context, Result};
use gtk::{glib, prelude::*};
use gtk4 as gtk;

use crate::qemu::VmSummary;

use super::{chrome, utils};

/// Show a small chooser window when multiple VMs are visible so `qd2 connect`
/// can stay as direct as virt-viewer instead of forcing the user back to `--vm`.
pub(super) fn choose_vm(vms: &[VmSummary]) -> Result<Option<VmSummary>> {
    gtk::init().context("failed to initialize GTK4 for the VM chooser")?;

    let main_loop = glib::MainLoop::new(None, false);
    let chosen_index = Rc::new(RefCell::new(None::<usize>));
    let app_icon = utils::load_app_icon().ok();

    let window = gtk::Window::builder()
        .title("Choose a VM - QD2")
        .default_width(760)
        .default_height(460)
        .modal(true)
        .build();
    if let Some(icon) = app_icon.clone() {
        window.connect_realize(move |window| {
            if let Err(error) = utils::apply_window_icon(window, &icon) {
                eprintln!("QD2 icon error: {error:#}");
            }
        });
    }

    let display = gtk::prelude::RootExt::display(&window);
    chrome::install_viewer_css(&display);

    let header_bar = gtk::HeaderBar::new();
    header_bar.set_show_title_buttons(true);
    let title = gtk::Label::new(Some("Choose a VM"));
    title.add_css_class("viewer-title");
    header_bar.set_title_widget(Some(&title));
    window.set_titlebar(Some(&header_bar));

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);

    let lead = gtk::Label::new(Some(
        "Multiple QEMU D-Bus virtual machines are available. Select one to open.",
    ));
    lead.set_xalign(0.0);
    lead.add_css_class("dim-label");
    lead.set_wrap(true);
    content.append(&lead);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .min_content_height(260)
        .build();

    let listbox = gtk::ListBox::new();
    listbox.set_selection_mode(gtk::SelectionMode::Single);
    listbox.add_css_class("boxed-list");
    listbox.set_activate_on_single_click(false);

    for vm in vms {
        let row = gtk::ListBoxRow::new();
        row.set_activatable(true);
        row.set_selectable(true);

        let card = gtk::Box::new(gtk::Orientation::Vertical, 6);
        card.set_margin_top(10);
        card.set_margin_bottom(10);
        card.set_margin_start(10);
        card.set_margin_end(10);

        let name = gtk::Label::new(Some(&vm.name));
        name.set_xalign(0.0);
        name.add_css_class("heading");
        card.append(&name);

        let uuid = gtk::Label::new(Some(&format!("UUID: {}", vm.uuid)));
        uuid.set_xalign(0.0);
        uuid.add_css_class("dim-label");
        uuid.set_selectable(true);
        card.append(&uuid);

        let source = gtk::Label::new(Some(&format!("Source: {}", vm.source_label)));
        source.set_xalign(0.0);
        source.add_css_class("dim-label");
        source.set_wrap(true);
        card.append(&source);

        let details = gtk::Label::new(Some(&format!(
            "Owner: {}   |   Consoles: {}",
            vm.owner,
            vm.console_ids.len()
        )));
        details.set_xalign(0.0);
        details.add_css_class("dim-label");
        details.set_wrap(true);
        card.append(&details);

        row.set_child(Some(&card));
        listbox.append(&row);
    }

    scrolled.set_child(Some(&listbox));
    content.append(&scrolled);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);

    let cancel_button = gtk::Button::with_label("Cancel");
    let connect_button = gtk::Button::with_label("Connect");
    connect_button.add_css_class("suggested-action");
    connect_button.set_sensitive(!vms.is_empty());

    actions.append(&cancel_button);
    actions.append(&connect_button);
    content.append(&actions);

    window.set_child(Some(&content));

    listbox.connect_row_selected({
        let connect_button = connect_button.clone();
        move |_, row| connect_button.set_sensitive(row.is_some())
    });

    listbox.connect_row_activated({
        let main_loop = main_loop.clone();
        let chosen_index = chosen_index.clone();
        move |_, row| {
            *chosen_index.borrow_mut() = usize::try_from(row.index()).ok();
            main_loop.quit();
        }
    });

    connect_button.connect_clicked({
        let main_loop = main_loop.clone();
        let chosen_index = chosen_index.clone();
        let listbox = listbox.clone();
        move |_| {
            if let Some(row) = listbox.selected_row() {
                *chosen_index.borrow_mut() = usize::try_from(row.index()).ok();
                main_loop.quit();
            }
        }
    });

    cancel_button.connect_clicked({
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    window.connect_close_request({
        let main_loop = main_loop.clone();
        move |_| {
            main_loop.quit();
            glib::Propagation::Proceed
        }
    });

    if let Some(first_row) = listbox.row_at_index(0) {
        listbox.select_row(Some(&first_row));
    }

    window.present();
    main_loop.run();

    Ok(chosen_index
        .borrow()
        .and_then(|index| vms.get(index).cloned()))
}
