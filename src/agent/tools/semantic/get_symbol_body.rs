use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use rig::tool::PortableTool;
use serde::Deserialize;

use crate::agent::tools::{AskSender, PermCheck, ToolError, check_perm_path};
use crate::semantic::SymbolIndex;

pub struct GetSymbolBodyTool {
    pub permission: Option<PermCheck>,
    pub ask_tx: Option<AskSender>,
    index: Arc<RwLock<SymbolIndex>>,
}

impl GetSymbolBodyTool {
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
    path: String,
    name: String,
}

impl PortableTool for GetSymbolBodyTool {
    const NAME: &'static str = "get_symbol_body";

    type Error = ToolError;
    type Args = Args;
    type Output = String;

    fn description(&self) -> String {
        "Get the full source code of a named symbol (function, class, method, etc.) from a file. Uses tree-sitter to precisely extract by byte range.".to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file containing the symbol"
                },
                "name": {
                    "type": "string",
                    "description": "Name of the symbol to retrieve"
                }
            },
            "required": ["path", "name"]
        })
    }

    async fn call(&self, args: Args) -> Result<String, ToolError> {
        // Path-aware check so external_directory rules apply —
        // `args.path` is the real file path we'll read symbols from.
        check_perm_path(
            &self.permission,
            &self.ask_tx,
            "get_symbol_body",
            &args.path,
        )
        .await?;

        let file_path = PathBuf::from(&args.path);

        let body = {
            let idx = self
                .index
                .read()
                .map_err(|e| ToolError::Msg(format!("Index read-lock error: {e}")))?;
            idx.get_symbol_body(&file_path, &args.name)
                .map_err(ToolError::Msg)?
        };

        Ok(format!(
            "Symbol: {} in {}\n\n{}",
            args.name, args.path, body
        ))
    }
}
