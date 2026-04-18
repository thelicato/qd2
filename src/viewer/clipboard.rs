use std::{
    cell::RefCell,
    env,
    rc::Rc,
    sync::OnceLock,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use gtk::{
    gdk,
    gio::{self, prelude::*},
    glib::{self, prelude::ToValue},
    prelude::*,
};
use gtk4 as gtk;
use qemu_display::{
    Clipboard as RemoteClipboard, ClipboardHandler, ClipboardSelection, Display,
    Error as RemoteError,
};
use tokio::sync::mpsc as tokio_mpsc;
use zbus::Connection;

use crate::diagnostics;

use super::{InputEvent, ViewerEvent, events::EventSender};

const TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
const TEXT_PLAIN: &str = "text/plain";
const UTF8_STRING: &str = "UTF8_STRING";
const TEXT: &str = "TEXT";
const STRING: &str = "STRING";
const TEXT_HTML: &str = "text/html";
const TEXT_URI_LIST: &str = "text/uri-list";
const IMAGE_PNG: &str = "image/png";
const CLIPBOARD_DEBUG_ENV: &str = "QD2_CLIPBOARD_DEBUG";

const TEXT_MIME_PREFERENCE: [&str; 5] = [TEXT_PLAIN_UTF8, TEXT_PLAIN, UTF8_STRING, TEXT, STRING];
const RICH_MIME_PREFERENCE: [&str; 3] = [TEXT_HTML, TEXT_URI_LIST, IMAGE_PNG];

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct ClipboardContent {
    plain_text: Option<String>,
    html: Option<Vec<u8>>,
    uri_list: Option<Vec<u8>>,
    png: Option<Vec<u8>>,
}

impl ClipboardContent {
    fn is_empty(&self) -> bool {
        self.plain_text.is_none()
            && self.html.is_none()
            && self.uri_list.is_none()
            && self.png.is_none()
    }

    pub(super) fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(text) = self.plain_text.as_deref() {
            parts.push(format!("text {}", describe_optional_text(Some(text))));
        }
        if let Some(html) = &self.html {
            parts.push(format!("html bytes={}", html.len()));
        }
        if let Some(uri_list) = &self.uri_list {
            parts.push(format!("uri-list bytes={}", uri_list.len()));
        }
        if let Some(png) = &self.png {
            parts.push(format!("png bytes={}", png.len()));
        }
        if parts.is_empty() {
            "empty".to_owned()
        } else {
            parts.join(", ")
        }
    }

    fn advertised_mimes(&self) -> Vec<&'static str> {
        let mut mimes = Vec::new();
        if self.plain_text.is_some() {
            mimes.extend(TEXT_MIME_PREFERENCE);
        }
        if self.html.is_some() {
            mimes.push(TEXT_HTML);
        }
        if self.uri_list.is_some() {
            mimes.push(TEXT_URI_LIST);
        }
        if self.png.is_some() {
            mimes.push(IMAGE_PNG);
        }
        mimes
    }

    fn merge_text(&mut self, text: String) {
        self.plain_text = Some(text);
    }

    fn merge_mime_bytes(&mut self, mime: &str, data: Vec<u8>) {
        if is_supported_text_mime(mime) {
            self.plain_text = Some(String::from_utf8_lossy(&data).into_owned());
            return;
        }

        match canonical_rich_mime(mime) {
            Some(TEXT_HTML) => self.html = Some(data),
            Some(TEXT_URI_LIST) => self.uri_list = Some(data),
            Some(IMAGE_PNG) => self.png = Some(data),
            _ => {}
        }
    }

    fn content_provider(&self) -> Option<gdk::ContentProvider> {
        let mut providers = Vec::new();

        if let Some(text) = self.plain_text.as_deref() {
            providers.push(gdk::ContentProvider::for_value(&text.to_value()));
        }
        if let Some(html) = &self.html {
            providers.push(gdk::ContentProvider::for_bytes(
                TEXT_HTML,
                &glib::Bytes::from_owned(html.clone()),
            ));
        }
        if let Some(uri_list) = &self.uri_list {
            providers.push(gdk::ContentProvider::for_bytes(
                TEXT_URI_LIST,
                &glib::Bytes::from_owned(uri_list.clone()),
            ));
        }
        if let Some(png) = &self.png {
            providers.push(gdk::ContentProvider::for_bytes(
                IMAGE_PNG,
                &glib::Bytes::from_owned(png.clone()),
            ));
        }

        match providers.len() {
            0 => None,
            1 => providers.into_iter().next(),
            _ => Some(gdk::ContentProvider::new_union(&providers)),
        }
    }

    fn reply_for_requested_mimes(&self, mimes: &[String]) -> Option<(String, Vec<u8>)> {
        for requested in mimes {
            if is_supported_text_mime(requested) {
                if let Some(text) = self.plain_text.as_ref() {
                    return Some((requested.clone(), text.as_bytes().to_vec()));
                }
                continue;
            }

            match canonical_rich_mime(requested) {
                Some(TEXT_HTML) => {
                    if let Some(html) = &self.html {
                        return Some((TEXT_HTML.to_owned(), html.clone()));
                    }
                }
                Some(TEXT_URI_LIST) => {
                    if let Some(uri_list) = &self.uri_list {
                        return Some((TEXT_URI_LIST.to_owned(), uri_list.clone()));
                    }
                }
                Some(IMAGE_PNG) => {
                    if let Some(png) = &self.png {
                        return Some((IMAGE_PNG.to_owned(), png.clone()));
                    }
                }
                _ => {}
            }
        }

        None
    }
}

#[derive(Default)]
pub(super) struct ClipboardUiState {
    clipboard: SelectionUiState,
    primary: SelectionUiState,
}

#[derive(Default)]
struct SelectionUiState {
    read_generation: u64,
    ignored_remote_content: Option<ClipboardContent>,
    last_seen_content: Option<ClipboardContent>,
    pending_guest_content: Option<ClipboardContent>,
    awaiting_remote_echo: bool,
}

impl ClipboardUiState {
    fn selection(&self, selection: ClipboardSelection) -> Option<&SelectionUiState> {
        match selection {
            ClipboardSelection::Clipboard => Some(&self.clipboard),
            ClipboardSelection::Primary => Some(&self.primary),
            ClipboardSelection::Secondary => None,
        }
    }

    fn selection_mut(&mut self, selection: ClipboardSelection) -> Option<&mut SelectionUiState> {
        match selection {
            ClipboardSelection::Clipboard => Some(&mut self.clipboard),
            ClipboardSelection::Primary => Some(&mut self.primary),
            ClipboardSelection::Secondary => None,
        }
    }
}

struct HostClipboardRead {
    selection: ClipboardSelection,
    generation: u64,
    pending_parts: usize,
    content: ClipboardContent,
}

struct RemoteFetchPlan {
    requested_mimes: Vec<&'static str>,
}

pub(super) fn debug(message: impl AsRef<str>) {
    if clipboard_debug_enabled() || diagnostics::verbose_enabled() {
        eprintln!("[qd2-clipboard] {}", message.as_ref());
    }
}

/// Bridge GTK clipboard notifications into the listener thread, preserving the
/// richer MIME variants that QEMU can forward to the guest.
pub(super) fn install_host_clipboard_bridge(
    picture: &gtk::Picture,
    window: &gtk::Window,
    ui_state: Rc<RefCell<ClipboardUiState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) {
    debug(format!(
        "install ui bridge: session_type={:?} wayland_display={:?} display={:?} sudo_user={:?}",
        env::var_os("XDG_SESSION_TYPE"),
        env::var_os("WAYLAND_DISPLAY"),
        env::var_os("DISPLAY"),
        env::var_os("SUDO_USER"),
    ));

    install_selection_bridge(
        picture,
        ClipboardSelection::Clipboard,
        picture.clipboard(),
        ui_state.clone(),
        input_tx.clone(),
    );
    install_selection_bridge(
        picture,
        ClipboardSelection::Primary,
        picture.primary_clipboard(),
        ui_state.clone(),
        input_tx.clone(),
    );

    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |_| {
            debug("viewer focus entered");
            let _ = input_tx.send(InputEvent::ClipboardViewerFocused(true));
            refresh_clipboard_state(&picture, &ui_state, &input_tx)
        }
    });
    focus.connect_leave({
        let input_tx = input_tx.clone();
        move |_| {
            debug("viewer focus left");
            let _ = input_tx.send(InputEvent::ClipboardViewerFocused(false));
        }
    });
    picture.add_controller(focus);

    let paste_shortcuts = gtk::EventControllerKey::new();
    paste_shortcuts.set_propagation_phase(gtk::PropagationPhase::Capture);
    paste_shortcuts.connect_key_pressed({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |_, keyval, _, state| {
            if is_paste_shortcut(keyval, state) {
                debug(format!(
                    "paste shortcut detected: key={keyval:?} state={state:?}"
                ));
                refresh_clipboard_state(&picture, &ui_state, &input_tx);
            }
            gtk::glib::Propagation::Proceed
        }
    });
    picture.add_controller(paste_shortcuts);

    window.connect_is_active_notify({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |window| {
            if window.is_active() {
                debug("window became active");
                refresh_clipboard_state(&picture, &ui_state, &input_tx);
            } else {
                debug("window became inactive");
                let _ = input_tx.send(InputEvent::ClipboardViewerFocused(false));
            }
        }
    });
}

/// Apply guest clipboard content to the selected GTK clipboard and keep enough
/// state to suppress the echo that GTK immediately emits back to us.
pub(super) fn apply_guest_clipboard(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    selection: ClipboardSelection,
    content: &ClipboardContent,
) -> anyhow::Result<()> {
    debug(format!(
        "apply guest clipboard to GTK selection={selection:?}: {}",
        content.describe()
    ));

    let Some(clipboard) = gtk_clipboard_for_selection(picture, selection) else {
        return Ok(());
    };

    let provider = content.content_provider();
    if let Err(error) = clipboard.set_content(provider.as_ref()) {
        if let Some(selection_state) = ui_state.borrow_mut().selection_mut(selection) {
            selection_state.pending_guest_content = Some(content.clone());
        }
        debug(format!("gtk set_content failed for {selection:?}: {error}"));
        return Err(error).with_context(
            || "failed to claim the host clipboard; Wayland may require a recent local input event",
        );
    }

    if let Some(selection_state) = ui_state.borrow_mut().selection_mut(selection) {
        selection_state.ignored_remote_content = Some(content.clone());
        selection_state.last_seen_content = (!content.is_empty()).then_some(content.clone());
        selection_state.pending_guest_content = None;
        selection_state.awaiting_remote_echo = true;
    }
    debug(format!("gtk set_content succeeded for {selection:?}"));
    Ok(())
}

pub(super) async fn register_clipboard_bridge(
    connection: &Connection,
    owner: &str,
    event_tx: EventSender,
) -> anyhow::Result<Option<ClipboardSession>> {
    debug(format!("register clipboard bridge for owner={owner}"));
    let display = Display::new(connection, Some(owner.to_owned()))
        .await
        .context("failed to open the QEMU display object for clipboard sharing")?;
    let Some(clipboard) = display
        .clipboard()
        .await
        .context("failed to inspect the QEMU clipboard interface")?
    else {
        debug("qemu display has no clipboard object");
        return Ok(None);
    };

    let shared = Arc::new(Mutex::new(ClipboardBridgeState::default()));
    clipboard
        .register(ClipboardListener {
            clipboard: clipboard.clone(),
            event_tx: event_tx.clone(),
            shared: shared.clone(),
        })
        .await
        .context("failed to register the clipboard bridge with QEMU")?;
    debug("registered clipboard bridge with QEMU");

    Ok(Some(ClipboardSession { clipboard, shared }))
}

pub(super) struct ClipboardSession {
    clipboard: RemoteClipboard,
    shared: Arc<Mutex<ClipboardBridgeState>>,
}

impl ClipboardSession {
    pub(super) fn set_viewer_focused(&self, focused: bool) {
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        if shared.viewer_focused != focused {
            shared.viewer_focused = focused;
            debug(format!("viewer clipboard focus -> {focused}"));
        }
    }

    /// Publish the current host clipboard to the guest or release ownership if
    /// the host selection no longer contains any MIME types QD2 can forward.
    pub(super) async fn update_host_content(
        &self,
        selection: ClipboardSelection,
        content: Option<ClipboardContent>,
    ) -> anyhow::Result<()> {
        debug(format!(
            "host clipboard update requested selection={selection:?}: {}",
            content
                .as_ref()
                .map(ClipboardContent::describe)
                .unwrap_or_else(|| "empty".to_owned())
        ));

        enum Action {
            Grab {
                selection: ClipboardSelection,
                serial: u32,
                mimes: Vec<&'static str>,
            },
            Release {
                selection: ClipboardSelection,
            },
            None,
        }

        let action = {
            let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
            let Some(selection_state) = shared.selection_mut(selection) else {
                debug(format!(
                    "ignoring unsupported host clipboard selection {selection:?}"
                ));
                return Ok(());
            };

            match content.filter(|content| !content.is_empty()) {
                Some(content) => {
                    if selection_state.local_owner
                        && selection_state.host_content.as_ref() == Some(&content)
                    {
                        debug("host clipboard unchanged while QD2 already owns it");
                        Action::None
                    } else {
                        let mimes = content.advertised_mimes();
                        selection_state.host_content = Some(content);
                        selection_state.local_owner = true;
                        Action::Grab {
                            selection,
                            serial: selection_state.next_serial(),
                            mimes,
                        }
                    }
                }
                None => {
                    if selection_state.host_content.take().is_some() || selection_state.local_owner
                    {
                        selection_state.local_owner = false;
                        Action::Release { selection }
                    } else {
                        debug("host clipboard already released");
                        Action::None
                    }
                }
            }
        };

        match action {
            Action::Grab {
                selection,
                serial,
                mimes,
            } => {
                debug(format!(
                    "sending QEMU Grab selection={selection:?} serial={serial} mimes={mimes:?}"
                ));
                self.clipboard
                    .proxy
                    .grab(selection, serial, &mimes)
                    .await
                    .context("failed to advertise the host clipboard to QEMU")
            }
            Action::Release { selection } => {
                debug(format!("sending QEMU Release selection={selection:?}"));
                self.clipboard
                    .proxy
                    .release(selection)
                    .await
                    .context("failed to release the host clipboard in QEMU")
            }
            Action::None => Ok(()),
        }
    }
}

fn should_accept_remote_grab(
    current_serial: u32,
    local_owner: bool,
    viewer_focused: bool,
    incoming_serial: u32,
) -> bool {
    if viewer_focused && local_owner {
        return true;
    }

    incoming_serial > current_serial || (incoming_serial == current_serial && !local_owner)
}

#[derive(Default)]
struct ClipboardBridgeState {
    clipboard: SelectionBridgeState,
    primary: SelectionBridgeState,
    viewer_focused: bool,
}

#[derive(Default)]
struct SelectionBridgeState {
    current_serial: u32,
    local_owner: bool,
    host_content: Option<ClipboardContent>,
}

impl SelectionBridgeState {
    fn next_serial(&mut self) -> u32 {
        self.current_serial = self.current_serial.wrapping_add(1);
        if self.current_serial == 0 {
            self.current_serial = 1;
        }
        self.current_serial
    }
}

impl ClipboardBridgeState {
    fn selection(&self, selection: ClipboardSelection) -> Option<&SelectionBridgeState> {
        match selection {
            ClipboardSelection::Clipboard => Some(&self.clipboard),
            ClipboardSelection::Primary => Some(&self.primary),
            ClipboardSelection::Secondary => None,
        }
    }

    fn selection_mut(
        &mut self,
        selection: ClipboardSelection,
    ) -> Option<&mut SelectionBridgeState> {
        match selection {
            ClipboardSelection::Clipboard => Some(&mut self.clipboard),
            ClipboardSelection::Primary => Some(&mut self.primary),
            ClipboardSelection::Secondary => None,
        }
    }
}

struct ClipboardListener {
    clipboard: RemoteClipboard,
    event_tx: EventSender,
    shared: Arc<Mutex<ClipboardBridgeState>>,
}

#[async_trait::async_trait]
impl ClipboardHandler for ClipboardListener {
    async fn register(&mut self) {
        debug("QEMU -> register");
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        shared.clipboard = SelectionBridgeState::default();
        shared.primary = SelectionBridgeState::default();
    }

    async fn unregister(&mut self) {
        debug("QEMU -> unregister");
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        shared.clipboard = SelectionBridgeState::default();
        shared.primary = SelectionBridgeState::default();
    }

    async fn grab(&mut self, selection: ClipboardSelection, serial: u32, mimes: Vec<String>) {
        debug(format!(
            "QEMU -> grab selection={selection:?} serial={serial} mimes={mimes:?}"
        ));

        let fetch_plan = remote_fetch_plan(&mimes);
        if fetch_plan.is_empty() {
            debug("no supported MIME offered by QEMU");
            return;
        }

        {
            let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
            let viewer_focused = shared.viewer_focused;
            let Some(selection_state) = shared.selection_mut(selection) else {
                debug(format!(
                    "ignoring unsupported guest clipboard selection {selection:?}"
                ));
                return;
            };

            if !should_accept_remote_grab(
                selection_state.current_serial,
                selection_state.local_owner,
                viewer_focused,
                serial,
            ) {
                debug(format!(
                    "ignoring stale/conflicting grab: current_serial={} local_owner={}",
                    selection_state.current_serial, selection_state.local_owner
                ));
                return;
            }
            if viewer_focused
                && selection_state.local_owner
                && serial <= selection_state.current_serial
            {
                debug(format!(
                    "accepting guest grab while viewer is focused despite local serial {}",
                    selection_state.current_serial
                ));
            }
            selection_state.current_serial = serial;
            selection_state.local_owner = false;
        }

        let mut content = ClipboardContent::default();
        let mut first_error = None;
        for request in fetch_plan {
            debug(format!(
                "sending QEMU Request selection={selection:?} serial={serial} mimes={:?}",
                request.requested_mimes
            ));
            match self
                .clipboard
                .proxy
                .request(selection, &request.requested_mimes)
                .await
            {
                Ok((mime, data)) => {
                    debug(format!(
                        "QEMU Request reply mime={mime} bytes={}",
                        data.len()
                    ));
                    content.merge_mime_bytes(&mime, data);
                }
                Err(error) => {
                    debug(format!(
                        "QEMU Request failed for selection={selection:?} mimes={:?}: {error:#}",
                        request.requested_mimes
                    ));
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }

        if content.is_empty() {
            if let Some(error) = first_error {
                let _ = self.event_tx.send(ViewerEvent::Status(format!(
                    "Clipboard fetch failed: {error:#}"
                )));
            }
            return;
        }

        let still_current = {
            let shared = self.shared.lock().expect("clipboard mutex was poisoned");
            let Some(selection_state) = shared.selection(selection) else {
                return;
            };
            !selection_state.local_owner && selection_state.current_serial == serial
        };
        if still_current {
            debug(format!(
                "forwarding guest clipboard to UI selection={selection:?}: {}",
                content.describe()
            ));
            let _ = self
                .event_tx
                .send(ViewerEvent::ClipboardGuestChanged(selection, content));
        } else {
            debug("discarding QEMU Request reply because ownership changed");
        }
    }

    async fn release(&mut self, selection: ClipboardSelection) {
        debug(format!("QEMU -> release selection={selection:?}"));
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        let Some(selection_state) = shared.selection_mut(selection) else {
            return;
        };

        if !selection_state.local_owner {
            selection_state.host_content = None;
            debug("cleared cached host clipboard because QEMU released remote ownership");
        }
    }

    async fn request(
        &mut self,
        selection: ClipboardSelection,
        mimes: Vec<String>,
    ) -> qemu_display::Result<(String, Vec<u8>)> {
        debug(format!(
            "QEMU -> request selection={selection:?} mimes={mimes:?}"
        ));

        let shared = self.shared.lock().expect("clipboard mutex was poisoned");
        let Some(selection_state) = shared.selection(selection) else {
            return Err(RemoteError::Failed(format!(
                "clipboard selection {selection:?} is not supported yet"
            )));
        };
        if !selection_state.local_owner {
            return Err(RemoteError::Failed(
                "the host clipboard is not currently owned by QD2".to_owned(),
            ));
        }

        let Some(content) = selection_state.host_content.clone() else {
            return Err(RemoteError::Failed(
                "the host clipboard does not currently contain a supported MIME type".to_owned(),
            ));
        };
        let Some((reply_mime, data)) = content.reply_for_requested_mimes(&mimes) else {
            return Err(RemoteError::Failed(
                "the guest requested a clipboard MIME type that QD2 does not support".to_owned(),
            ));
        };
        debug(format!(
            "serving host clipboard to QEMU selection={selection:?}: mime={reply_mime} {}",
            content.describe()
        ));
        Ok((reply_mime, data))
    }
}

fn install_selection_bridge(
    picture: &gtk::Picture,
    selection: ClipboardSelection,
    clipboard: gdk::Clipboard,
    ui_state: Rc<RefCell<ClipboardUiState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) {
    clipboard.connect_changed({
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        let picture = picture.clone();
        move |clipboard| {
            debug(format!("gtk clipboard changed for {selection:?}"));
            read_host_clipboard(selection, clipboard, &ui_state, &input_tx);
            retry_pending_guest_clipboard(&picture, &ui_state, selection);
        }
    });

    read_host_clipboard(selection, &clipboard, &ui_state, &input_tx);
}

fn read_host_clipboard(
    selection: ClipboardSelection,
    clipboard: &gdk::Clipboard,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    let Some(generation) = next_read_generation(ui_state, selection) else {
        return;
    };

    let formats = clipboard.formats();
    let offered_mimes = formats
        .mime_types()
        .into_iter()
        .map(|mime| mime.to_string())
        .collect::<Vec<_>>();
    let has_string_type = formats.contains_type(String::static_type());
    debug(format!(
        "snapshot host clipboard selection={selection:?} generation={generation} mimes={offered_mimes:?} has_string_type={has_string_type}",
    ));

    let read_text = has_string_type
        || offered_mimes.is_empty()
        || offered_mimes
            .iter()
            .any(|mime| is_supported_text_mime(mime));
    let read_html = offered_mimes
        .iter()
        .any(|mime| canonical_rich_mime(mime) == Some(TEXT_HTML));
    let read_uri_list = offered_mimes
        .iter()
        .any(|mime| canonical_rich_mime(mime) == Some(TEXT_URI_LIST));
    let read_png = offered_mimes
        .iter()
        .any(|mime| canonical_rich_mime(mime) == Some(IMAGE_PNG));

    let pending_parts = usize::from(read_text)
        + usize::from(read_html)
        + usize::from(read_uri_list)
        + usize::from(read_png);

    if pending_parts == 0 {
        finish_host_snapshot(
            selection,
            generation,
            ClipboardContent::default(),
            ui_state,
            input_tx,
        );
        return;
    }

    let collector = Rc::new(RefCell::new(HostClipboardRead {
        selection,
        generation,
        pending_parts,
        content: ClipboardContent::default(),
    }));

    if read_text {
        debug(format!("gtk read_text_async requested for {selection:?}"));
        clipboard.read_text_async(None::<&gio::Cancellable>, {
            let collector = collector.clone();
            let ui_state = ui_state.clone();
            let input_tx = input_tx.clone();
            move |result| {
                let part = match result {
                    Ok(Some(text)) => {
                        let text = text.to_string();
                        debug(format!(
                            "gtk read_text_async -> selection={selection:?} {}",
                            describe_optional_text(Some(&text))
                        ));
                        ClipboardReadPart::Text(text)
                    }
                    Ok(None) => {
                        debug(format!(
                            "gtk read_text_async -> selection={selection:?} no text"
                        ));
                        ClipboardReadPart::Empty
                    }
                    Err(error) => {
                        debug(format!(
                            "gtk read_text_async failed for {selection:?}: {error}"
                        ));
                        ClipboardReadPart::Empty
                    }
                };
                complete_host_read_part(&collector, part, &ui_state, &input_tx);
            }
        });
    }

    if read_html {
        read_host_mime(
            clipboard,
            selection,
            TEXT_HTML,
            collector.clone(),
            ui_state.clone(),
            input_tx.clone(),
        );
    }
    if read_uri_list {
        read_host_mime(
            clipboard,
            selection,
            TEXT_URI_LIST,
            collector.clone(),
            ui_state.clone(),
            input_tx.clone(),
        );
    }
    if read_png {
        read_host_mime(
            clipboard,
            selection,
            IMAGE_PNG,
            collector,
            ui_state.clone(),
            input_tx.clone(),
        );
    }
}

fn read_host_mime(
    clipboard: &gdk::Clipboard,
    selection: ClipboardSelection,
    mime: &'static str,
    collector: Rc<RefCell<HostClipboardRead>>,
    ui_state: Rc<RefCell<ClipboardUiState>>,
    input_tx: tokio_mpsc::UnboundedSender<InputEvent>,
) {
    debug(format!(
        "gtk read_async requested for selection={selection:?} mime={mime}"
    ));
    clipboard.read_async(
        &[mime],
        glib::Priority::DEFAULT,
        None::<&gio::Cancellable>,
        move |result| match result {
            Ok((stream, returned_mime)) => {
                let stream = stream;
                let sink = gio::MemoryOutputStream::new_resizable();
                let sink_result = sink.clone();
                sink.splice_async(
                    &stream,
                    gio::OutputStreamSpliceFlags::CLOSE_SOURCE
                        | gio::OutputStreamSpliceFlags::CLOSE_TARGET,
                    glib::Priority::DEFAULT,
                    None::<&gio::Cancellable>,
                    move |splice_result| {
                        let part = match splice_result {
                            Ok(_) => {
                                let bytes = sink_result.steal_as_bytes();
                                debug(format!(
                                    "gtk read_async -> selection={selection:?} mime={} bytes={}",
                                    returned_mime,
                                    bytes.len()
                                ));
                                ClipboardReadPart::Bytes {
                                    mime: returned_mime.to_string(),
                                    data: bytes.as_ref().to_vec(),
                                }
                            }
                            Err(error) => {
                                debug(format!(
                                    "gtk splice_async failed for selection={selection:?} mime={mime}: {error}"
                                ));
                                ClipboardReadPart::Empty
                            }
                        };
                        complete_host_read_part(&collector, part, &ui_state, &input_tx);
                    },
                );
            }
            Err(error) => {
                debug(format!(
                    "gtk read_async failed for selection={selection:?} mime={mime}: {error}"
                ));
                complete_host_read_part(&collector, ClipboardReadPart::Empty, &ui_state, &input_tx);
            }
        },
    );
}

fn complete_host_read_part(
    collector: &Rc<RefCell<HostClipboardRead>>,
    part: ClipboardReadPart,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    let (selection, generation, content, completed) = {
        let mut collector = collector.borrow_mut();
        match part {
            ClipboardReadPart::Text(text) => collector.content.merge_text(text),
            ClipboardReadPart::Bytes { mime, data } => {
                collector.content.merge_mime_bytes(&mime, data);
            }
            ClipboardReadPart::Empty => {}
        }
        collector.pending_parts -= 1;
        (
            collector.selection,
            collector.generation,
            collector.content.clone(),
            collector.pending_parts == 0,
        )
    };

    if completed {
        finish_host_snapshot(selection, generation, content, ui_state, input_tx);
    }
}

fn finish_host_snapshot(
    selection: ClipboardSelection,
    generation: u64,
    content: ClipboardContent,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    let mut ui_state = ui_state.borrow_mut();
    let Some(selection_state) = ui_state.selection_mut(selection) else {
        return;
    };
    if selection_state.read_generation != generation {
        debug(format!(
            "dropping stale clipboard snapshot selection={selection:?} generation={generation}",
        ));
        return;
    }

    if selection_state.ignored_remote_content.as_ref() == Some(&content) {
        debug(format!(
            "ignoring GTK clipboard echo from remote-set content for {selection:?}"
        ));
        selection_state.ignored_remote_content = None;
        selection_state.awaiting_remote_echo = false;
        selection_state.last_seen_content = (!content.is_empty()).then_some(content);
        return;
    }

    if selection_state.awaiting_remote_echo && content.is_empty() {
        debug(format!(
            "ignoring transient empty GTK clipboard snapshot after remote set for {selection:?}"
        ));
        return;
    }

    if selection_state.awaiting_remote_echo {
        selection_state.awaiting_remote_echo = false;
        selection_state.ignored_remote_content = None;
    }

    if selection_state.last_seen_content.as_ref() == Some(&content) {
        debug(format!("gtk clipboard content unchanged for {selection:?}"));
        return;
    }

    selection_state.last_seen_content = (!content.is_empty()).then_some(content.clone());
    drop(ui_state);

    debug(format!(
        "sending ClipboardHostChanged selection={selection:?}: {}",
        content.describe()
    ));
    let _ = input_tx.send(InputEvent::ClipboardHostChanged(
        selection,
        (!content.is_empty()).then_some(content),
    ));
}

fn next_read_generation(
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    selection: ClipboardSelection,
) -> Option<u64> {
    let mut ui_state = ui_state.borrow_mut();
    let selection_state = ui_state.selection_mut(selection)?;
    selection_state.read_generation = selection_state.read_generation.wrapping_add(1);
    if selection_state.read_generation == 0 {
        selection_state.read_generation = 1;
    }
    Some(selection_state.read_generation)
}

fn refresh_clipboard_state(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    debug("refresh clipboard state");
    for selection in [ClipboardSelection::Clipboard, ClipboardSelection::Primary] {
        if let Some(clipboard) = gtk_clipboard_for_selection(picture, selection) {
            read_host_clipboard(selection, &clipboard, ui_state, input_tx);
        }
        retry_pending_guest_clipboard(picture, ui_state, selection);
    }
}

fn retry_pending_guest_clipboard(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    selection: ClipboardSelection,
) {
    let pending = ui_state
        .borrow()
        .selection(selection)
        .and_then(|selection_state| selection_state.pending_guest_content.clone());
    if let Some(content) = pending {
        debug(format!(
            "retry pending guest clipboard selection={selection:?}: {}",
            content.describe()
        ));
        let _ = apply_guest_clipboard(picture, ui_state, selection, &content);
    }
}

fn is_paste_shortcut(keyval: gdk::Key, state: gdk::ModifierType) -> bool {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
    matches!(
        (keyval, ctrl, shift),
        (gdk::Key::V | gdk::Key::v, true, false) | (gdk::Key::Insert, false, true)
    )
}

fn remote_fetch_plan(mimes: &[String]) -> Vec<RemoteFetchPlan> {
    let mut plan = Vec::new();
    let text_mimes = preferred_text_request_mimes(mimes);
    if !text_mimes.is_empty() {
        plan.push(RemoteFetchPlan {
            requested_mimes: text_mimes,
        });
    }
    for mime in RICH_MIME_PREFERENCE {
        if mimes
            .iter()
            .any(|offered| canonical_rich_mime(offered) == Some(mime))
        {
            plan.push(RemoteFetchPlan {
                requested_mimes: vec![mime],
            });
        }
    }
    plan
}

fn preferred_text_request_mimes(mimes: &[String]) -> Vec<&'static str> {
    TEXT_MIME_PREFERENCE
        .into_iter()
        .filter(|supported| mimes.iter().any(|offered| offered == supported))
        .collect()
}

fn is_supported_text_mime(mime: &str) -> bool {
    TEXT_MIME_PREFERENCE.contains(&mime)
}

fn canonical_rich_mime(mime: &str) -> Option<&'static str> {
    match mime {
        TEXT_HTML => Some(TEXT_HTML),
        TEXT_URI_LIST => Some(TEXT_URI_LIST),
        IMAGE_PNG => Some(IMAGE_PNG),
        _ => None,
    }
}

fn gtk_clipboard_for_selection(
    picture: &gtk::Picture,
    selection: ClipboardSelection,
) -> Option<gdk::Clipboard> {
    match selection {
        ClipboardSelection::Clipboard => Some(picture.clipboard()),
        ClipboardSelection::Primary => Some(picture.primary_clipboard()),
        ClipboardSelection::Secondary => None,
    }
}

fn clipboard_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        env::var_os(CLIPBOARD_DEBUG_ENV)
            .map(|value| {
                let normalized = value.to_string_lossy().trim().to_ascii_lowercase();
                !normalized.is_empty() && normalized != "0" && normalized != "false"
            })
            .unwrap_or(false)
    })
}

fn describe_optional_text(text: Option<&str>) -> String {
    match text {
        Some(text) => format!("len={} preview={:?}", text.len(), preview_text(text)),
        None => "empty".to_owned(),
    }
}

fn preview_text(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 48;
    let mut preview = text.chars().take(MAX_PREVIEW_CHARS).collect::<String>();
    if text.chars().count() > MAX_PREVIEW_CHARS {
        preview.push_str("...");
    }
    preview
}

enum ClipboardReadPart {
    Text(String),
    Bytes { mime: String, data: Vec<u8> },
    Empty,
}

#[cfg(test)]
mod tests {
    use super::{
        ClipboardContent, ClipboardSelection, IMAGE_PNG, STRING, TEXT, TEXT_HTML, TEXT_PLAIN,
        TEXT_PLAIN_UTF8, TEXT_URI_LIST, UTF8_STRING, canonical_rich_mime,
        preferred_text_request_mimes, remote_fetch_plan, should_accept_remote_grab,
    };

    #[test]
    fn advertised_mimes_include_text_aliases_and_rich_content() {
        let mut content = ClipboardContent::default();
        content.merge_text("hello".to_owned());
        content.merge_mime_bytes(TEXT_HTML, b"<b>hello</b>".to_vec());
        content.merge_mime_bytes(IMAGE_PNG, vec![1, 2, 3]);

        assert_eq!(
            content.advertised_mimes(),
            vec![
                TEXT_PLAIN_UTF8,
                TEXT_PLAIN,
                UTF8_STRING,
                TEXT,
                STRING,
                TEXT_HTML,
                IMAGE_PNG
            ]
        );
    }

    #[test]
    fn request_reply_follows_requester_order() {
        let mut content = ClipboardContent::default();
        content.merge_text("hello".to_owned());
        content.merge_mime_bytes(TEXT_HTML, b"<b>hello</b>".to_vec());

        let requested = vec![TEXT_HTML.to_owned(), TEXT_PLAIN.to_owned()];
        let (mime, data) = content
            .reply_for_requested_mimes(&requested)
            .expect("reply should be available");

        assert_eq!(mime, TEXT_HTML);
        assert_eq!(data, b"<b>hello</b>");
    }

    #[test]
    fn remote_fetch_plan_requests_text_once_and_rich_formats_individually() {
        let offered = vec![
            TEXT_PLAIN.to_owned(),
            TEXT_HTML.to_owned(),
            IMAGE_PNG.to_owned(),
        ];

        let plan = remote_fetch_plan(&offered);
        assert_eq!(plan.len(), 3);
        assert_eq!(plan[0].requested_mimes, vec![TEXT_PLAIN]);
        assert_eq!(plan[1].requested_mimes, vec![TEXT_HTML]);
        assert_eq!(plan[2].requested_mimes, vec![IMAGE_PNG]);
    }

    #[test]
    fn preferred_text_request_mimes_keep_qd2_preference_order() {
        let offered = vec![
            STRING.to_owned(),
            TEXT_PLAIN.to_owned(),
            UTF8_STRING.to_owned(),
        ];

        assert_eq!(
            preferred_text_request_mimes(&offered),
            vec![TEXT_PLAIN, UTF8_STRING, STRING]
        );
    }

    #[test]
    fn canonical_rich_mimes_match_exact_supported_values() {
        assert_eq!(canonical_rich_mime(TEXT_HTML), Some(TEXT_HTML));
        assert_eq!(canonical_rich_mime(TEXT_URI_LIST), Some(TEXT_URI_LIST));
        assert_eq!(canonical_rich_mime(IMAGE_PNG), Some(IMAGE_PNG));
        assert_eq!(canonical_rich_mime("application/json"), None);
    }

    #[test]
    fn primary_selection_is_treated_as_supported() {
        let plan = remote_fetch_plan(&[TEXT_PLAIN_UTF8.to_owned()]);
        assert_eq!(ClipboardSelection::Primary as u32, 1);
        assert_eq!(plan[0].requested_mimes, vec![TEXT_PLAIN_UTF8]);
    }

    #[test]
    fn focused_viewer_allows_guest_grab_to_override_local_owner() {
        assert!(should_accept_remote_grab(5, true, true, 0));
        assert!(!should_accept_remote_grab(5, true, false, 0));
        assert!(should_accept_remote_grab(5, false, false, 6));
    }
}
