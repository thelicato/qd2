use std::{
    sync::mpsc::{Sender, SyncSender},
    time::Duration,
};

use anyhow::{Context, Result};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::qemu::{self, ConnectTarget};

use super::super::{
    InputEvent, ViewerEvent, ViewerReady, audio, clipboard,
    mouse::{self, MouseMode},
};
use super::remote::RemoteConsole;

const DISCONNECT_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionOutcome {
    Shutdown,
    Disconnected,
}

pub(super) async fn listener_session(
    target: ConnectTarget,
    event_tx: &Sender<ViewerEvent>,
    ready_tx: &SyncSender<Result<ViewerReady>>,
    input_rx: &mut tokio_mpsc::UnboundedReceiver<InputEvent>,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> Result<SessionOutcome> {
    let connection = qemu::connect(target.source_address.as_deref()).await?;
    let mut console = RemoteConsole::new(&connection, &target.owner, target.console_id)
        .await
        .with_context(|| format!("failed to open console {}", target.console_id))?;

    console
        .register_listener(event_tx.clone())
        .await
        .context("failed to register the QEMU display listener")?;
    let clipboard =
        clipboard::register_clipboard_bridge(&connection, &target.owner, event_tx.clone())
            .await
            .context("failed to initialize clipboard sharing")?;
    let _audio =
        match audio::register_audio_output(&connection, &target.owner, &target.vm_name).await {
            Ok(audio) => audio,
            Err(error) => {
                eprintln!("QD2 audio error: {error:#}");
                None
            }
        };
    clipboard::debug(format!(
        "listener clipboard availability: {}",
        if clipboard.is_some() {
            "present"
        } else {
            "absent"
        }
    ));

    let title = format!("{} - QD2", target.vm_name);
    let keyboard_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Keyboard");
    let mouse_available = target
        .console_interfaces
        .iter()
        .any(|interface| interface == "org.qemu.Display1.Mouse");
    let mut mouse_mode = if mouse_available {
        match console.mouse_is_absolute().await {
            Ok(is_absolute) => MouseMode::from_is_absolute(is_absolute),
            Err(error) => {
                let _ = event_tx.send(ViewerEvent::Status(format!(
                    "Could not detect mouse mode: {error:#}"
                )));
                MouseMode::Relative
            }
        }
    } else {
        MouseMode::Disabled
    };
    let mut disconnect_probe = tokio::time::interval(DISCONNECT_POLL_INTERVAL);

    let ready = ViewerReady {
        title,
        width: target.width,
        height: target.height,
        keyboard_available,
        clipboard_available: clipboard.is_some(),
        mouse_mode,
    };
    let _ = ready_tx.send(Ok(ready));

    loop {
        tokio::select! {
            _ = &mut *shutdown_rx => return Ok(SessionOutcome::Shutdown),
            _ = disconnect_probe.tick() => {
                if console.check_alive().await.is_err() {
                    break;
                }
            }
            maybe_input = input_rx.recv() => match maybe_input {
                Some(input) => {
                    if let InputEvent::ClipboardHostChanged(selection, content) = &input {
                        clipboard::debug(format!(
                            "listener received ClipboardHostChanged selection={selection:?}: {}",
                            content
                                .as_ref()
                                .map(|content| content.describe())
                                .unwrap_or_else(|| "empty".to_owned())
                        ));
                        if let Some(clipboard) = &clipboard {
                            if let Err(error) = clipboard
                                .update_host_content(*selection, content.clone())
                                .await
                            {
                                super::super::clipboard::debug(format!(
                                    "update_host_content failed: {error:#}"
                                ));
                                let _ = event_tx.send(ViewerEvent::Status(format!(
                                    "Clipboard sharing failed: {error:#}"
                                )));
                            }
                        } else {
                            super::super::clipboard::debug(
                                "dropping ClipboardHostChanged because no QEMU clipboard is available",
                            );
                        }
                        continue;
                    }

                    let needs_mouse_mode = mouse::input_needs_mouse_mode(&input);
                    if let Err(error) = console.handle_input(input).await {
                        let recovered = if needs_mouse_mode {
                            match console.mouse_is_absolute().await {
                                Ok(is_absolute) => {
                                    let detected_mode = MouseMode::from_is_absolute(is_absolute);
                                    if detected_mode != mouse_mode {
                                        mouse_mode = detected_mode;
                                        let _ = event_tx.send(ViewerEvent::MouseModeChanged(detected_mode));
                                        true
                                    } else {
                                        false
                                    }
                                }
                                Err(mode_error) => {
                                    let _ = event_tx.send(ViewerEvent::Status(format!(
                                        "Input forwarding failed: {error:#}\n\nCould not re-check the mouse mode: {mode_error:#}"
                                    )));
                                    true
                                }
                            }
                        } else {
                            false
                        };

                        if !recovered {
                            let _ = event_tx.send(ViewerEvent::Status(format!(
                                "Input forwarding failed: {error:#}"
                            )));
                        }
                    }
                }
                None => return Ok(SessionOutcome::Shutdown),
            }
        }
    }

    drop(console);
    drop(connection);
    Ok(SessionOutcome::Disconnected)
}
