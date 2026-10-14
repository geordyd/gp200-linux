use crate::{AssetStore, data_dir};
use crossbeam_channel::{Receiver, SendTimeoutError, Sender, bounded};
use gp200_domain::*;
use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;
/// Bounded file/conversion worker. Cancellation bypasses a full job queue.
pub struct AssetWorker {
    commands: Sender<AssetCommand>,
    events: Receiver<AssetEvent>,
    cancelled: Arc<Mutex<BTreeSet<JobId>>>,
    shutdown: ShutdownToken,
    thread: Option<JoinHandle<()>>,
}
impl AssetWorker {
    pub fn open_xdg(wake: WakeUi) -> Result<Self, ServiceError> {
        Self::open(data_dir()?, wake)
    }
    pub fn new(wake: WakeUi) -> Result<Self, ServiceError> {
        Self::open_xdg(wake)
    }
    pub fn open(path: impl AsRef<Path>, wake: WakeUi) -> Result<Self, ServiceError> {
        let mut store = AssetStore::open(path)?;
        let (commands, rx) = bounded(COMMAND_CAPACITY);
        let (tx, events) = bounded(EVENT_CAPACITY);
        let shutdown = ShutdownToken::default();
        let thread_shutdown = shutdown.clone();
        let cancelled = Arc::new(Mutex::new(BTreeSet::new()));
        let thread_cancelled = cancelled.clone();
        let session = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let thread = thread::Builder::new()
            .name("gp200-assets".into())
            .spawn(move || {
                let emit = |mut event: AssetEvent| -> bool {
                    loop {
                        if thread_shutdown.cancelled() {
                            return false;
                        }
                        match tx.send_timeout(event, Duration::from_millis(50)) {
                            Ok(()) => {
                                wake();
                                return true;
                            }
                            Err(SendTimeoutError::Timeout(returned)) => event = returned,
                            Err(SendTimeoutError::Disconnected(_)) => return false,
                        }
                    }
                };
                if !emit(AssetEvent::Inventory(store.inventory())) {
                    return;
                }
                while !thread_shutdown.cancelled() {
                    let command = match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(command) => command,
                        Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                        Err(_) => break,
                    };
                    let prepare = !matches!(&command, AssetCommand::Inspect(_));
                    match command {
                        AssetCommand::List => {
                            if !emit(AssetEvent::Inventory(store.inventory())) {
                                break;
                            }
                        }
                        AssetCommand::Shutdown => break,
                        AssetCommand::Cancel(_) => {}
                        AssetCommand::Inspect(spec)
                        | AssetCommand::Prepare(spec)
                        | AssetCommand::Convert(spec) => {
                            let token = format!("{session}-{}", spec.id.0);
                            let is_cancelled = || {
                                thread_shutdown.cancelled()
                                    || thread_cancelled
                                        .lock()
                                        .unwrap_or_else(|poison| poison.into_inner())
                                        .contains(&spec.id)
                            };
                            let result = (|| {
                                store.journal(&token, &spec, "queued", None)?;
                                if is_cancelled() {
                                    return Err(crate::cancelled());
                                }
                                store.journal(&token, &spec, "validating", None)?;
                                if !emit(AssetEvent::JobProgress {
                                    job_id: spec.id,
                                    progress: JobProgress {
                                        completed: 0,
                                        total: None,
                                        message: "Validating and retaining original source".into(),
                                        cancellable: true,
                                    },
                                }) {
                                    return Err(crate::cancelled());
                                }
                                if prepare {
                                    store.journal(&token, &spec, "converting_locally", None)?;
                                let mut last_progress=0;
                                let result = store.prepare_with_progress(&spec, is_cancelled, |done,total| {
                                    if done == total || done/16384 != last_progress/16384 {
                                        last_progress=done;
                                        emit(AssetEvent::JobProgress {job_id:spec.id, progress:JobProgress { completed:done,total:Some(total),message:"Rendering local NAM audio; no pedal transfer".into(),cancellable:true }});
                                    }
                                })?;
                                    if let Some(info) = result.derived {
                                        Ok(AssetEvent::Prepared { job: spec.id, info })
                                    } else {
                                        Ok(AssetEvent::Inspected {
                                            job: spec.id,
                                            info: result.original,
                                        })
                                    }
                                } else {
                                    let info = store.inspect(&spec)?;
                                    if is_cancelled() {
                                        Err(crate::cancelled())
                                    } else {
                                        Ok(AssetEvent::Inspected { job: spec.id, info })
                                    }
                                }
                            })();
                            let state = match &result {
                                Ok(_) => "complete_local",
                                Err(error) if error.kind == ErrorKind::Cancelled => "cancelled",
                                Err(_) => "failed",
                            };
                            let journal = store.journal(
                                &token,
                                &spec,
                                state,
                                result.as_ref().err().map(|error| error.message.as_str()),
                            );
                            let event = match journal {
                                Err(error) => AssetEvent::Failed {
                                    job: spec.id,
                                    error,
                                },
                                Ok(()) => result.unwrap_or_else(|error| AssetEvent::Failed {
                                    job: spec.id,
                                    error,
                                }),
                            };
                            thread_cancelled
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner())
                                .remove(&spec.id);
                            if !emit(event) || !emit(AssetEvent::Inventory(store.inventory())) {
                                break;
                            }
                        }
                    }
                }
            })
            .map_err(|error| {
                ServiceError::new(ErrorKind::Io, format!("Cannot start asset worker: {error}"))
            })?;
        Ok(Self {
            commands,
            events,
            cancelled,
            shutdown,
            thread: Some(thread),
        })
    }
}
impl AssetService for AssetWorker {
    fn try_submit(&self, command: AssetCommand) -> Result<(), SubmitError<AssetCommand>> {
        if self.shutdown.cancelled() {
            return Err(SubmitError::Disconnected(command));
        }
        match command {
            AssetCommand::Shutdown => {
                self.shutdown.cancel();
                Ok(())
            }
            AssetCommand::Cancel(id) => {
                let mut cancelled = self
                    .cancelled
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if cancelled.len() >= 512 {
                    Err(SubmitError::Busy(AssetCommand::Cancel(id)))
                } else {
                    cancelled.insert(id);
                    Ok(())
                }
            }
            command => self
                .commands
                .try_send(command)
                .map_err(|error| match error {
                    crossbeam_channel::TrySendError::Full(command) => SubmitError::Busy(command),
                    crossbeam_channel::TrySendError::Disconnected(command) => {
                        SubmitError::Disconnected(command)
                    }
                }),
        }
    }
    fn try_next_event(&self) -> Option<AssetEvent> {
        self.events.try_recv().ok()
    }
    fn shutdown(&self) {
        self.shutdown.cancel();
    }
}
impl Drop for AssetWorker {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
