use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::tool::PortableTool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm};
use crate::semantic::SymbolIndex;

pub struct FindDefinitionTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl FindDefinitionTool {
    pub fn new(
        index: Arc<RwLock<SymbolIndex>>,
        permission: Option<PermCheck>,
        ask_tx: Option<AskSender>,
    ) -> Self {
        Self {
            permission,
            ask_tx,
            index,
        }
    }
}

#[derive(Deserialize)]
pub struct Args {
    name: String,
}

impl PortableTool for FindDefinitionTool {
    const NAME: &'static str = "find_definition";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    fn description(&self) -> String {
        "Find where a SYMBOL (function, class, type, etc.) is DEFINED across the project. Uses tree-sitter for precise structural matching. NOT for finding files by name — use `find_files` for that. NOT for content search — use `grep`.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Name of the symbol to find"
                }
            },
            "required": ["name"]
        })
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        check_perm(
            &self.permission,
            &self.ask_tx,
            "find_definition",
            &args.name,
        )
        .await?;

        let results = {
            let idx = self
                .index
                .read()
                .map_err(|e| ToolError::Msg(format!("Index read-lock error: {e}")))?;
            idx.ensure_all(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            )
            .map_err(ToolError::Msg)?;
            idx.find_definition(&args.name).map_err(ToolError::Msg)?
        };

        if results.is_empty() {
            return Ok(format!("No definition found for '{}'", args.name));
        }

        let mut output = format!(
            "Found {} definition(s) for '{}':\n",
            results.len(),
            args.name
        );
        for (path, sym) in &results {
            output.push_str(&format!(
                "  {}:{} [{}] {}\n",
                path.display(),
                sym.range.start_line,
                sym.kind,
                sym.signature
            ));
        }

        Ok(output)
    }
}
