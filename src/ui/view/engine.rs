//! Adapter: a [`ViewEngine`] that runs any [`Reducer`] on its own
//! thread. The reducer is built on that thread (a cljrs runtime is not
//! `Send`), events arrive over a channel that never blocks the sender,
//! and updates leave on the [`UpdateSink`]. The engine does not know
//! which reducer it runs.

use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use super::domain::{NoticeLevel, ViewEffect, ViewEvent, ViewModel, ViewUpdate};
use super::port::{Reducer, UpdateSink, ViewEngine};

/// Longest dirge waits at startup for a reducer to boot and answer the
/// initial model.
const BOOT_WAIT: Duration = Duration::from_secs(10);

pub struct ThreadEngine {
    tx: Sender<ViewEvent>,
}

impl ThreadEngine {
    /// Start thread `name` with `stack` bytes, build the reducer there
    /// with `make`, fold [`ViewEvent::Init`] and answer the initial model.
    pub fn spawn<R, F>(
        name: &str,
        stack: usize,
        make: F,
        sink: UpdateSink,
    ) -> Result<(Self, ViewModel), String>
    where
        R: Reducer + 'static,
        F: FnOnce() -> Result<R, String> + Send + 'static,
    {
        let (tx, rx) = channel::<ViewEvent>();
        let (ready_tx, ready_rx) = channel::<Result<ViewModel, String>>();
        std::thread::Builder::new()
            .name(name.to_string())
            .stack_size(stack)
            .spawn(move || {
                let booted = make().and_then(|mut reducer| {
                    let initial = reducer.step(&ViewEvent::Init)?;
                    Ok((reducer, initial.model))
                });
                let (mut reducer, mut last) = match booted {
                    Ok(booted) => booted,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let _ = ready_tx.send(Ok(last.clone()));
                for event in rx {
                    let update = fold(&mut reducer, &event, &last);
                    last = update.model.clone();
                    if sink.send(update).is_err() {
                        return;
                    }
                }
            })
            .map_err(|e| format!("cannot start view thread {name}: {e}"))?;
        let model = ready_rx.recv_timeout(BOOT_WAIT).map_err(|_| {
            format!(
                "view thread {name} did not boot within {}s",
                BOOT_WAIT.as_secs()
            )
        })??;
        Ok((Self { tx }, model))
    }
}

/// One event through `reducer`; a failed step keeps the `last` model
/// and reports why.
fn fold<R: Reducer>(reducer: &mut R, event: &ViewEvent, last: &ViewModel) -> ViewUpdate {
    reducer.step(event).unwrap_or_else(|e| ViewUpdate {
        model: last.clone(),
        effects: vec![ViewEffect::notify(NoticeLevel::Error, format!("view: {e}"))],
    })
}

impl ViewEngine for ThreadEngine {
    fn submit(&self, event: ViewEvent) {
        if self.tx.send(event).is_err() {
            tracing::warn!(target: "dirge::view", "view engine stopped; event dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::native::NativeReducer;
    use tokio::sync::mpsc::unbounded_channel;

    struct Failing;

    impl Reducer for Failing {
        fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
            match event {
                ViewEvent::Init => Ok(ViewUpdate::default()),
                _ => Err("boom".into()),
            }
        }
    }

    #[test]
    fn submit_returns_at_once_and_the_update_arrives_on_the_sink() {
        let (sink, mut rx) = unbounded_channel();
        let (engine, model) =
            ThreadEngine::spawn("t-view", 1 << 20, || Ok(NativeReducer::default()), sink).unwrap();
        assert!(!model.swarm_open());
        assert!(model.owns_command("swarm"));
        engine.submit(ViewEvent::command("swarm", &["on"]));
        let update = rx.blocking_recv().unwrap();
        assert!(update.model.swarm_open());
    }

    #[test]
    fn a_failed_step_keeps_the_last_model_and_says_why() {
        let (sink, mut rx) = unbounded_channel();
        let (engine, _) = ThreadEngine::spawn("t-fail", 1 << 20, || Ok(Failing), sink).unwrap();
        engine.submit(ViewEvent::command("swarm", &[]));
        let update = rx.blocking_recv().unwrap();
        assert_eq!(update.model, ViewModel::default());
        assert!(
            matches!(&update.effects[0], ViewEffect::Notify { text, .. } if text.contains("boom"))
        );
    }

    #[test]
    fn a_reducer_that_cannot_boot_fails_the_spawn() {
        let (sink, _rx) = unbounded_channel();
        let err = ThreadEngine::spawn::<NativeReducer, _>(
            "t-dead",
            1 << 20,
            || Err("no runtime".into()),
            sink,
        );
        assert_eq!(err.err().as_deref(), Some("no runtime"));
    }
}
