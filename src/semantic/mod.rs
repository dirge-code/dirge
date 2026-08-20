mod adapter;
pub mod adapters;
pub(crate) mod common;
mod index;
pub mod minify;
#[cfg(feature = "semantic-mojo")]
pub mod mojo_grammar;
pub mod syntax_validator;
pub mod types;

use std::sync::Arc;
use std::sync::RwLock;

use crate::agent::tools::semantic;
use crate::permission::ask::AskSender;
use crate::permission::checker::PermCheck;

// Used by src/tests/semantic_tests.rs; unused in a non-test build.
#[allow(unused_imports)]
pub use adapter::LanguageAdapter;
pub use index::SymbolIndex;

pub struct SemanticManager {
    index: Arc<RwLock<SymbolIndex>>,
}

impl SemanticManager {
    pub fn new() -> Self {
        let registry = Arc::new(adapters::AdapterRegistry::new(adapters::default_adapters()));
        let index = Arc::new(RwLock::new(SymbolIndex::new(registry)));

        Self { index }
    }

    pub fn tools(
        &self,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Vec<Box<dyn crate::agent::agent_loop::rig_tool::DynTool>> {
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
