use std::{
    cell::RefCell,
    env,
    rc::Rc,
    sync::OnceLock,
    sync::{Arc, Mutex, mpsc::Sender},
};

use anyhow::Context;
use gtk::{gdk, glib::prelude::ToValue, prelude::*};
use gtk4 as gtk;
use qemu_display::{
    Clipboard as RemoteClipboard, ClipboardHandler, ClipboardSelection, Display,
    Error as RemoteError,
};
use tokio::sync::mpsc as tokio_mpsc;
use zbus::Connection;

use super::{InputEvent, ViewerEvent};

const TEXT_PLAIN_UTF8: &str = "text/plain;charset=utf-8";
const TEXT_PLAIN: &str = "text/plain";
const CLIPBOARD_DEBUG_ENV: &str = "QD2_CLIPBOARD_DEBUG";

#[derive(Default)]
pub(super) struct ClipboardUiState {
    ignored_remote_text: Option<String>,
    last_seen_text: Option<String>,
    pending_guest_text: Option<String>,
}

pub(super) fn debug(message: impl AsRef<str>) {
    if clipboard_debug_enabled() {
        eprintln!("[qd2-clipboard] {}", message.as_ref());
    }
}

/// Bridge GTK's clipboard notifications into the listener thread so host-side
/// copy operations can be offered to the guest through QEMU's clipboard API.
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

    let clipboard = picture.clipboard();
    clipboard.connect_changed({
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        let picture = picture.clone();
        move |clipboard| {
            debug("gtk clipboard changed");
            read_host_clipboard(clipboard, &ui_state, &input_tx);
            retry_pending_guest_clipboard(&picture, &ui_state);
        }
    });

    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter({
        let picture = picture.clone();
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |_| {
            debug("viewer focus entered");
            refresh_clipboard_state(&picture, &ui_state, &input_tx)
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
            }
        }
    });

    read_host_clipboard(&clipboard, &ui_state, &input_tx);
}

pub(super) fn apply_guest_text_clipboard(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    text: &str,
) -> anyhow::Result<()> {
    debug(format!(
        "apply guest clipboard to GTK: {}",
        describe_optional_text(Some(text))
    ));
    let provider = gdk::ContentProvider::for_value(&text.to_value());
    if let Err(error) = picture.clipboard().set_content(Some(&provider)) {
        ui_state.borrow_mut().pending_guest_text = Some(text.to_owned());
        debug(format!("gtk set_content failed: {error}"));
        return Err(error).with_context(
            || "failed to claim the host clipboard; Wayland may require a recent local input event",
        );
    }

    let mut ui_state = ui_state.borrow_mut();
    ui_state.ignored_remote_text = Some(text.to_owned());
    ui_state.last_seen_text = Some(text.to_owned());
    ui_state.pending_guest_text = None;
    debug("gtk set_content succeeded");
    Ok(())
}

fn read_host_clipboard(
    clipboard: &gdk::Clipboard,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    debug("gtk read_text_async requested");
    clipboard.read_text_async(None::<&gtk::gio::Cancellable>, {
        let ui_state = ui_state.clone();
        let input_tx = input_tx.clone();
        move |result| match result {
            Ok(Some(text)) => {
                let text = text.to_string();
                debug(format!(
                    "gtk read_text_async -> text available: {}",
                    describe_optional_text(Some(&text))
                ));
                let mut ui_state = ui_state.borrow_mut();

                if ui_state.ignored_remote_text.as_deref() == Some(text.as_str()) {
                    debug("ignoring GTK clipboard echo from remote-set text");
                    ui_state.ignored_remote_text = None;
                    ui_state.last_seen_text = Some(text);
                    return;
                }

                if ui_state.last_seen_text.as_deref() == Some(text.as_str()) {
                    debug("gtk clipboard text unchanged");
                    return;
                }

                ui_state.last_seen_text = Some(text.clone());
                drop(ui_state);
                debug("sending ClipboardHostChanged(Some)");
                let _ = input_tx.send(InputEvent::ClipboardHostChanged(Some(text)));
            }
            Ok(None) => {
                debug("gtk read_text_async -> clipboard has no text");
                let mut ui_state = ui_state.borrow_mut();
                ui_state.ignored_remote_text = None;
                if ui_state.last_seen_text.take().is_none() {
                    debug("host clipboard already known empty");
                    return;
                }

                drop(ui_state);
                debug("sending ClipboardHostChanged(None)");
                let _ = input_tx.send(InputEvent::ClipboardHostChanged(None));
            }
            Err(error) => {
                debug(format!("gtk read_text_async failed: {error}"));
                let mut ui_state = ui_state.borrow_mut();
                ui_state.ignored_remote_text = None;
                if ui_state.last_seen_text.take().is_none() {
                    debug("clipboard read error but no cached host text to clear");
                    return;
                }

                drop(ui_state);
                debug("sending ClipboardHostChanged(None) after read failure");
                let _ = input_tx.send(InputEvent::ClipboardHostChanged(None));
            }
        }
    });
}

fn refresh_clipboard_state(
    picture: &gtk::Picture,
    ui_state: &Rc<RefCell<ClipboardUiState>>,
    input_tx: &tokio_mpsc::UnboundedSender<InputEvent>,
) {
    debug("refresh clipboard state");
    read_host_clipboard(&picture.clipboard(), ui_state, input_tx);
    retry_pending_guest_clipboard(picture, ui_state);
}

fn retry_pending_guest_clipboard(picture: &gtk::Picture, ui_state: &Rc<RefCell<ClipboardUiState>>) {
    let pending_text = ui_state.borrow().pending_guest_text.clone();
    if let Some(text) = pending_text {
        debug(format!(
            "retry pending guest clipboard: {}",
            describe_optional_text(Some(&text))
        ));
        let _ = apply_guest_text_clipboard(picture, ui_state, &text);
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

pub(super) async fn register_clipboard_bridge(
    connection: &Connection,
    owner: &str,
    event_tx: Sender<ViewerEvent>,
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
    /// Publish the current host clipboard to the guest or release ownership if
    /// the host clipboard no longer contains plain text.
    pub(super) async fn update_host_text(&self, text: Option<String>) -> anyhow::Result<()> {
        debug(format!(
            "host clipboard update requested: {}",
            describe_optional_text(text.as_deref())
        ));
        enum Action {
            Grab { serial: u32 },
            Release,
            None,
        }

        let action = {
            let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
            match text {
                Some(text) => {
                    if shared.local_owner && shared.host_text.as_deref() == Some(text.as_str()) {
                        debug("host clipboard unchanged while QD2 already owns it");
                        Action::None
                    } else {
                        shared.host_text = Some(text);
                        shared.local_owner = true;
                        Action::Grab {
                            serial: shared.next_serial(),
                        }
                    }
                }
                None => {
                    if shared.host_text.take().is_some() || shared.local_owner {
                        shared.local_owner = false;
                        Action::Release
                    } else {
                        debug("host clipboard already released");
                        Action::None
                    }
                }
            }
        };

        match action {
            Action::Grab { serial } => {
                debug(format!(
                    "sending QEMU Grab selection=Clipboard serial={serial} mimes={:?}",
                    [TEXT_PLAIN_UTF8, TEXT_PLAIN]
                ));
                self.clipboard
                    .proxy
                    .grab(
                        ClipboardSelection::Clipboard,
                        serial,
                        &[TEXT_PLAIN_UTF8, TEXT_PLAIN],
                    )
                    .await
                    .context("failed to advertise the host clipboard to QEMU")
            }
            Action::Release => {
                debug("sending QEMU Release selection=Clipboard");
                self.clipboard
                    .proxy
                    .release(ClipboardSelection::Clipboard)
                    .await
                    .context("failed to release the host clipboard in QEMU")
            }
            Action::None => Ok(()),
        }
    }
}

#[derive(Default)]
struct ClipboardBridgeState {
    current_serial: u32,
    local_owner: bool,
    host_text: Option<String>,
}

impl ClipboardBridgeState {
    fn next_serial(&mut self) -> u32 {
        self.current_serial = self.current_serial.wrapping_add(1);
        if self.current_serial == 0 {
            self.current_serial = 1;
        }
        self.current_serial
    }
}

struct ClipboardListener {
    clipboard: RemoteClipboard,
    event_tx: Sender<ViewerEvent>,
    shared: Arc<Mutex<ClipboardBridgeState>>,
}

#[async_trait::async_trait]
impl ClipboardHandler for ClipboardListener {
    async fn register(&mut self) {
        debug("QEMU -> register");
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        shared.current_serial = 0;
        shared.local_owner = false;
    }

    async fn unregister(&mut self) {
        debug("QEMU -> unregister");
        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        shared.current_serial = 0;
        shared.local_owner = false;
    }

    async fn grab(&mut self, selection: ClipboardSelection, serial: u32, mimes: Vec<String>) {
        debug(format!(
            "QEMU -> grab selection={selection:?} serial={serial} mimes={mimes:?}"
        ));
        if selection != ClipboardSelection::Clipboard {
            debug("ignoring non-Clipboard selection");
            return;
        }

        let requested_mimes = supported_text_mimes(&mimes);
        if requested_mimes.is_empty() {
            debug("no supported text MIME offered by QEMU");
            return;
        }

        {
            let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
            if serial < shared.current_serial
                || (serial == shared.current_serial && shared.local_owner)
            {
                debug(format!(
                    "ignoring stale/conflicting grab: current_serial={} local_owner={}",
                    shared.current_serial, shared.local_owner
                ));
                return;
            }
            shared.current_serial = serial;
            shared.local_owner = false;
        }

        debug(format!(
            "sending QEMU Request selection=Clipboard serial={serial} mimes={requested_mimes:?}"
        ));
        match self
            .clipboard
            .proxy
            .request(ClipboardSelection::Clipboard, &requested_mimes)
            .await
        {
            Ok((mime, data)) if is_supported_text_mime(&mime) => {
                debug(format!(
                    "QEMU Request reply mime={mime} bytes={}",
                    data.len()
                ));
                let still_current = {
                    let shared = self.shared.lock().expect("clipboard mutex was poisoned");
                    !shared.local_owner && shared.current_serial == serial
                };
                if still_current {
                    let text = String::from_utf8_lossy(&data).into_owned();
                    debug(format!(
                        "forwarding guest clipboard text to UI: {}",
                        describe_optional_text(Some(&text))
                    ));
                    let _ = self.event_tx.send(ViewerEvent::ClipboardGuestText(text));
                } else {
                    debug("discarding QEMU Request reply because ownership changed");
                }
            }
            Ok((mime, data)) => {
                debug(format!(
                    "ignoring unsupported QEMU Request reply mime={mime} bytes={}",
                    data.len()
                ));
            }
            Err(error) => {
                debug(format!("QEMU Request failed: {error:#}"));
                let _ = self.event_tx.send(ViewerEvent::Status(format!(
                    "Clipboard fetch failed: {error:#}"
                )));
            }
        }
    }

    async fn release(&mut self, selection: ClipboardSelection) {
        debug(format!("QEMU -> release selection={selection:?}"));
        if selection != ClipboardSelection::Clipboard {
            return;
        }

        let mut shared = self.shared.lock().expect("clipboard mutex was poisoned");
        if !shared.local_owner {
            shared.host_text = None;
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
        if selection != ClipboardSelection::Clipboard {
            return Err(RemoteError::Failed(format!(
                "clipboard selection {selection:?} is not supported yet"
            )));
        }

        let Some(reply_mime) = preferred_text_mime(&mimes) else {
            return Err(RemoteError::Failed(
                "the guest requested a clipboard MIME type that QD2 does not support".to_owned(),
            ));
        };
        let shared = self.shared.lock().expect("clipboard mutex was poisoned");
        if !shared.local_owner {
            return Err(RemoteError::Failed(
                "the host clipboard is not currently owned by QD2".to_owned(),
            ));
        }

        let Some(text) = shared.host_text.clone() else {
            return Err(RemoteError::Failed(
                "the host clipboard does not currently contain plain text".to_owned(),
            ));
        };
        debug(format!(
            "serving host clipboard to QEMU: mime={reply_mime} {}",
            describe_optional_text(Some(&text))
        ));
        Ok((reply_mime.to_owned(), text.into_bytes()))
    }
}

fn supported_text_mimes(mimes: &[String]) -> Vec<&'static str> {
    let mut supported = Vec::new();
    if mimes.iter().any(|mime| mime == TEXT_PLAIN_UTF8) {
        supported.push(TEXT_PLAIN_UTF8);
    }
    if mimes.iter().any(|mime| mime == TEXT_PLAIN) {
        supported.push(TEXT_PLAIN);
    }
    supported
}

fn preferred_text_mime(mimes: &[String]) -> Option<&'static str> {
    supported_text_mimes(mimes).into_iter().next()
}

fn is_supported_text_mime(mime: &str) -> bool {
    mime == TEXT_PLAIN_UTF8 || mime == TEXT_PLAIN
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

#[cfg(test)]
mod tests {
    use super::{TEXT_PLAIN, TEXT_PLAIN_UTF8, supported_text_mimes};

    #[test]
    fn supported_text_mimes_keep_client_preference_order() {
        let mimes = vec![
            "image/png".to_owned(),
            TEXT_PLAIN.to_owned(),
            TEXT_PLAIN_UTF8.to_owned(),
        ];

        assert_eq!(
            supported_text_mimes(&mimes),
            vec![TEXT_PLAIN_UTF8, TEXT_PLAIN]
        );
    }

    #[test]
    fn unsupported_mimes_are_ignored() {
        let mimes = vec!["text/html".to_owned(), "image/png".to_owned()];
        assert!(supported_text_mimes(&mimes).is_empty());
    }
}
