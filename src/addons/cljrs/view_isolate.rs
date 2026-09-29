//! Adapter: the cljrs `dirge.view` reducer behind the view seam's
//! [`Reducer`] port. It gets a runtime of its own on a thread of its own
//! (the view seam's [`ThreadEngine`]), apart from the addon isolate: an
//! addon hook running during an agent turn never delays a view change,
//! and a view change never queues behind one.

use std::cell::RefCell;
use std::rc::Rc;

use cljrs_runtime::tiered::Env;
use cljrs_runtime::{ExecutionMode, Runtime};
use serde_json::Value as Json;

use super::isolate::{ISOLATE_STACK_BYTES, eval_source, install_args};
use crate::ui::view::engine::ThreadEngine;
use crate::ui::view::port::{Reducer, UpdateSink};
use crate::ui::view::{ViewEvent, ViewModel, ViewUpdate, wire};

/// The reducer's namespace, embedded in the binary.
const VIEW_NS: &str = "dirge.view";
const VIEW_SRC: &str = include_str!("view.cljc");

/// Private namespace through which an event reaches Clojure.
const ARGS_NS: &str = "dirge.view.bridge";

/// `dirge.view` in a runtime of its own. Not `Send`: built on the
/// engine thread.
pub struct CljrsReducer {
    env: Env,
    inbox: Rc<RefCell<Vec<Json>>>,
}

impl CljrsReducer {
    pub fn boot() -> Result<Self, String> {
        let runtime = Runtime::builder()
            .execution_mode(ExecutionMode::Tiered)
            .builtin_source(VIEW_NS, VIEW_SRC)
            .build()
            .map_err(|e| format!("cannot build the view runtime: {e}"))?;
        cljrs_stdlib::install(&runtime);
        let inbox = Rc::new(RefCell::new(Vec::new()));
        install_args(runtime.globals(), ARGS_NS, inbox.clone());
        let mut reducer = Self {
            env: runtime.env("user"),
            inbox,
        };
        eval_source(&mut reducer.env, &format!("(require '{VIEW_NS})"))
            .map_err(|e| format!("cannot load {VIEW_NS}: {e}"))?;
        Ok(reducer)
    }
}

impl Reducer for CljrsReducer {
    fn step(&mut self, event: &ViewEvent) -> Result<ViewUpdate, String> {
        *self.inbox.borrow_mut() = vec![wire::encode_event(event)];
        let answer = eval_source(
            &mut self.env,
            &format!("(apply {VIEW_NS}/dispatch! ({ARGS_NS}/args))"),
        );
        self.inbox.borrow_mut().clear();
        wire::decode_update(&answer?)
    }
}

/// The cljrs view engine, for the view seam's engine table.
pub fn engine(sink: UpdateSink) -> Result<(ThreadEngine, ViewModel), String> {
    ThreadEngine::spawn(
        "dirge-view-cljrs",
        ISOLATE_STACK_BYTES,
        CljrsReducer::boot,
        sink,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::view::native::NativeReducer;
    use crate::ui::view::tests::parity_script;

    /// Run `f` on a thread with the isolate's stack: the tree-walking
    /// evaluator recurses deeper than a test thread's default.
    fn on_isolate_stack(f: impl FnOnce() + Send + 'static) {
        std::thread::Builder::new()
            .stack_size(ISOLATE_STACK_BYTES)
            .spawn(f)
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn cljrs_and_native_answer_every_event_alike() {
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            let mut native = NativeReducer::default();
            for event in parity_script() {
                let want = native.step(&event).unwrap();
                let got = cljrs.step(&event).unwrap();
                assert_eq!(got, want, "diverged on {event:?}");
            }
        });
    }

    #[test]
    fn unknown_event_types_are_refused_by_the_reducer() {
        on_isolate_stack(|| {
            let mut cljrs = CljrsReducer::boot().unwrap();
            *cljrs.inbox.borrow_mut() = vec![serde_json::json!({"type": "teleport"})];
            let answer = eval_source(
                &mut cljrs.env,
                &format!("(apply {VIEW_NS}/dispatch! ({ARGS_NS}/args))"),
            )
            .unwrap();
            let update = wire::decode_update(&answer).unwrap();
            assert_eq!(update.effects.len(), 1);
            assert!(format!("{:?}", update.effects[0]).contains("unknown view event: teleport"));
        });
    }

    #[test]
    fn the_engine_boots_and_answers_off_thread() {
        let (sink, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (engine, model) = engine(sink).unwrap();
        assert!(model.owns_command("swarm"));
        use crate::ui::view::port::ViewEngine;
        engine.submit(ViewEvent::command("swarm", &["on"]));
        assert!(rx.blocking_recv().unwrap().model.swarm_open());
    }
}
