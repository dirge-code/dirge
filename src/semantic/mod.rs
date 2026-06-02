mod adapter;
pub mod adapters;
pub(crate) mod common;
mod index;
pub mod minify;
pub mod syntax_validator;
pub mod types;

use std::sync::Arc;
use std::sync::RwLock;

use crate::agent::tools::semantic;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

pub use adapter::LanguageAdapter;
pub use index::SymbolIndex;

pub struct SemanticManager {
    index: Arc<RwLock<SymbolIndex>>,
}

impl SemanticManager {
    // `mut` and the post-`Vec::new()` pushes are conditionally needed
    // depending on which language adapter features are active. The
    // `#[cfg]` gating doesn't compose with `vec![]` so we keep the
    // `Vec::new() + push` pattern and silence both lints at the fn
    // level (an item-level `#[allow]` on the local binding doesn't
    // suppress clippy here in practice).
    #[allow(unused_mut, clippy::vec_init_then_push)]
    pub fn new() -> Self {
        let mut adapters: Vec<Box<dyn LanguageAdapter>> = Vec::new();

        #[cfg(feature = "semantic-ts")]
        adapters.push(Box::new(adapters::TypescriptAdapter));

        #[cfg(feature = "semantic-python")]
        adapters.push(Box::new(adapters::PythonAdapter));

        #[cfg(feature = "semantic-clojure")]
        adapters.push(Box::new(adapters::ClojureAdapter));

        #[cfg(feature = "semantic-go")]
        adapters.push(Box::new(adapters::GoAdapter));

        #[cfg(feature = "semantic-ruby")]
        adapters.push(Box::new(adapters::RubyAdapter));

        #[cfg(feature = "semantic-rust")]
        adapters.push(Box::new(adapters::RustAdapter));

        #[cfg(feature = "semantic-java")]
        adapters.push(Box::new(adapters::JavaAdapter));

        // C registered before C++ so the C adapter wins for `.h`
        // (shared extension). C++ users with C++ headers should use
        // `.hpp`/`.hh`/`.hxx` to route through CppAdapter — see the
        // comment on `CppAdapter::extensions`.
        #[cfg(feature = "semantic-c")]
        adapters.push(Box::new(adapters::CAdapter));

        #[cfg(feature = "semantic-cpp")]
        adapters.push(Box::new(adapters::CppAdapter));

        #[cfg(feature = "semantic-elixir")]
        adapters.push(Box::new(adapters::ElixirAdapter));

        let registry = Arc::new(adapters::AdapterRegistry::new(adapters));
        let index = Arc::new(RwLock::new(SymbolIndex::new(registry)));

        Self { index }
    }

    pub fn tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<Box<dyn rig::tool::ToolDyn>> {
        let idx = self.index.clone();
        vec![
            Box::new(semantic::ListSymbolsTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::GetSymbolBodyTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindDefinitionTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindCallersTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
            Box::new(semantic::FindCalleesTool::new(
                idx.clone(),
                permission.clone(),
                ask_tx.clone(),
            )),
        ]
    }
}
