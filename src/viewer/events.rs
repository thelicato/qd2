use std::sync::mpsc::{self, Receiver, SendError, Sender, TryRecvError};

use anyhow::{Context, Result};

use super::ViewerEvent;

#[cfg(unix)]
use std::{
    io::ErrorKind,
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixDatagram,
    sync::Arc,
};

#[derive(Clone)]
pub(super) struct EventSender {
    tx: Sender<ViewerEvent>,
    #[cfg(unix)]
    wake_tx: Arc<UnixDatagram>,
}

pub(super) struct EventReceiver {
    rx: Receiver<ViewerEvent>,
    #[cfg(unix)]
    wake_rx: UnixDatagram,
}

/// Keep the viewer event queue and its GTK wakeup source together so the UI
/// can react as soon as the listener thread publishes a new frame.
pub(super) fn channel() -> Result<(EventSender, EventReceiver)> {
    let (tx, rx) = mpsc::channel();

    #[cfg(unix)]
    {
        let (wake_rx, wake_tx) =
            UnixDatagram::pair().context("failed to allocate the viewer wakeup socket pair")?;
        wake_rx
            .set_nonblocking(true)
            .context("failed to configure the viewer wakeup reader")?;
        wake_tx
            .set_nonblocking(true)
            .context("failed to configure the viewer wakeup writer")?;

        Ok((
            EventSender {
                tx,
                wake_tx: Arc::new(wake_tx),
            },
            EventReceiver { rx, wake_rx },
        ))
    }

    #[cfg(not(unix))]
    {
        Ok((EventSender { tx }, EventReceiver { rx }))
    }
}

impl EventSender {
    pub(super) fn send(&self, event: ViewerEvent) -> Result<(), SendError<ViewerEvent>> {
        self.tx.send(event)?;
        self.signal_wakeup();
        Ok(())
    }

    #[cfg(unix)]
    fn signal_wakeup(&self) {
        match self.wake_tx.send(&[1]) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::WouldBlock => {}
            Err(_) => {}
        }
    }

    #[cfg(not(unix))]
    fn signal_wakeup(&self) {}
}

impl EventReceiver {
    pub(super) fn try_recv(&mut self) -> Result<ViewerEvent, TryRecvError> {
        self.rx.try_recv()
    }

    #[cfg(unix)]
    pub(super) fn wake_fd(&self) -> RawFd {
        self.wake_rx.as_raw_fd()
    }

    pub(super) fn drain_wakeup(&mut self) {
        #[cfg(unix)]
        {
            let mut buffer = [0u8; 128];
            loop {
                match self.wake_rx.recv(&mut buffer) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                    Err(_) => break,
                }
            }
        }
    }
}
