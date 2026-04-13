mod reconnect;
mod remote;
mod session;

use std::sync::mpsc::{Sender, SyncSender};

use anyhow::{Context, Result};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

use crate::qemu::{self, ConnectTarget};

use super::{InputEvent, ViewerEvent, ViewerReady};
use reconnect::{ReconnectPlan, send_status_if_changed, wait_for_reconnect_retry};
use session::{SessionOutcome, listener_session};

pub(super) fn run_listener_thread(
    initial_target: ConnectTarget,
    requested_address: Option<String>,
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
                requested_address,
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
    requested_address: Option<String>,
    event_tx: Sender<ViewerEvent>,
    ready_tx: SyncSender<Result<ViewerReady>>,
    mut input_rx: tokio_mpsc::UnboundedReceiver<InputEvent>,
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let reconnect_plan = ReconnectPlan::new(&initial_target, requested_address);
    let mut last_status = None::<String>;

    match listener_session(
        initial_target,
        &event_tx,
        Some(&ready_tx),
        &mut input_rx,
        &mut shutdown_rx,
    )
    .await
    {
        Ok(SessionOutcome::Shutdown) => return Ok(()),
        Ok(SessionOutcome::Disconnected) => {
            send_status_if_changed(
                &event_tx,
                &mut last_status,
                reconnect_plan.waiting_message(),
            );
        }
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return Ok(());
        }
    }

    loop {
        if wait_for_reconnect_retry(&mut input_rx, &mut shutdown_rx).await {
            return Ok(());
        }

        match qemu::resolve_connect_target(
            reconnect_plan.requested_address.as_deref(),
            Some(&reconnect_plan.vm_uuid),
            Some(reconnect_plan.console_id),
        )
        .await
        {
            Ok(target) => {
                match listener_session(target, &event_tx, None, &mut input_rx, &mut shutdown_rx)
                    .await
                {
                    Ok(SessionOutcome::Shutdown) => return Ok(()),
                    Ok(SessionOutcome::Disconnected) => {
                        last_status = None;
                        send_status_if_changed(
                            &event_tx,
                            &mut last_status,
                            reconnect_plan.waiting_message(),
                        );
                    }
                    Err(error) => {
                        last_status = None;
                        send_status_if_changed(
                            &event_tx,
                            &mut last_status,
                            reconnect_plan.error_message(&error),
                        );
                    }
                }
            }
            Err(error) => {
                send_status_if_changed(
                    &event_tx,
                    &mut last_status,
                    reconnect_plan.error_message(&error),
                );
            }
        }
    }
}
