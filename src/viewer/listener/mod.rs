mod remote;
mod session;

use std::sync::mpsc::{Sender, SyncSender};

use anyhow::{Context, Result};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::qemu::ConnectTarget;

use super::{InputEvent, ViewerEvent, ViewerReady};
use session::{SessionOutcome, listener_session};

pub(super) fn run_listener_thread(
    initial_target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    shutdown_rx: oneshot::Receiver<()>,
) {
    let result = tokio::runtime::Runtime::new()
        .context("failed to create the async runtime for the display listener")
        .and_then(|runtime| {
            runtime.block_on(listener_supervisor_main(
                initial_target,
                event_tx,
                ready_tx,
                input_rx,
                shutdown_rx,
            ))
        });

    if let Err(error) = result {
        eprintln!("QD2 listener error: {error:#}");
    }
}

async fn listener_supervisor_main(
    initial_target: ConnectTarget,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    match listener_session(
        initial_target,
        &event_tx,
        &ready_tx,
        &mut input_rx,
        &mut shutdown_rx,
    )
    .await
    {
        Ok(SessionOutcome::Shutdown) => return Ok(()),
        Ok(SessionOutcome::Disconnected) => return Ok(()),
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return Ok(());
        }
    }
}
