//! Harbinger trigger — listens on a TCP or Unix socket for external wake-up
//! signals. Each accepted connection is read (with a 5s timeout), parsed as JSON,
//! and pushed as an event into the vigil's channel.
//!
//! Security: only binds to loopback (127.0.0.1) for TCP; `commands` mode requires
//! a non-empty command map.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{error, warn};

use crate::config::VigilCommand;

use super::dispatch::dispatch_commands;
use super::types::{HookDispatchRequest, TriggerKind, VigilEvent};

/// Spawn a harbinger trigger listening on `port` (TCP, loopback-only).
/// `commands_map` must be non-empty when `socket_mode` is Commands.
/// Dispatches `on-vigil-event` hook before pushing each accepted connection.
pub fn spawn_harbinger(
    vigil_name: String,
    port: u16,
    commands_map: HashMap<String, VigilCommand>,
    has_commands: bool,
    tx: mpsc::Sender<VigilEvent>,
    hook_tx: mpsc::Sender<HookDispatchRequest>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let listener = std::net::TcpListener::bind(addr)
        .map_err(|e| format!("bind {addr} for {vigil_name}: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set nonblocking for {vigil_name}: {e}"))?;
    let listener = TcpListener::from_std(listener)
        .map_err(|e| format!("convert listener for {vigil_name}: {e}"))?;

    Ok(tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    let vigil = vigil_name.clone();
                    let cmds = commands_map.clone();
                    let tx = tx.clone();
                    let hook_tx = hook_tx.clone();
                    tokio::spawn(async move {
                        match tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            handle_connection(stream, &vigil, &cmds, &tx, &hook_tx, has_commands),
                        )
                        .await
                        {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                error!(%vigil, %peer, "harbinger connection error: {e}");
                            }
                            Err(_) => {
                                warn!(%vigil, %peer, "harbinger connection timed out");
                            }
                        }
                    });
                }
                Err(e) => {
                    error!(%vigil_name, "accept error: {e}");
                    // Transient errors (ECONNABORTED, EMFILE under fd pressure)
                    // must not kill the listener. Back off briefly and retry.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
    }))
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    vigil_name: &str,
    commands_map: &HashMap<String, VigilCommand>,
    tx: &mpsc::Sender<VigilEvent>,
    hook_tx: &mpsc::Sender<HookDispatchRequest>,
    has_commands: bool,
) -> Result<(), String> {
    let peer = stream.peer_addr().map_err(|e| format!("peer addr: {e}"))?;
    let reader = BufReader::new(stream);
    let mut lines = reader.lines();

    let line = lines
        .next_line()
        .await
        .map_err(|e| format!("read from {peer}: {e}"))?
        .unwrap_or_default();

    let payload: serde_json::Value =
        serde_json::from_str(&line).map_err(|e| format!("parse json from {peer}: {e}"))?;

    // In `commands` mode, validate and resolve the command.
    let mut context = payload.clone();
    if has_commands {
        if let Some(command_name) = payload.get("command").and_then(|v| v.as_str()) {
            let (tool, args) = dispatch_commands(commands_map, command_name, &payload)
                .map_err(|e| format!("dispatch command '{command_name}': {e}"))?;
            // Enrich context with resolved tool dispatch.
            if let serde_json::Value::Object(ref mut map) = context {
                map.insert(
                    "_resolved_tool".to_string(),
                    serde_json::Value::String(tool),
                );
                map.insert(
                    "_resolved_args".to_string(),
                    serde_json::Value::Object(args),
                );
            }
        } else {
            return Err("commands mode requires 'command' field in payload".to_string());
        }
    }

    // Store raw payload for {harbinger_data} template substitution
    if let serde_json::Value::Object(ref mut map) = context {
        map.insert(
            "harbinger_data".to_string(),
            serde_json::Value::String(line.clone()),
        );
    }

    let event = VigilEvent {
        vigil_name: vigil_name.to_string(),
        trigger: TriggerKind::Harbinger,
        context,
        timestamp: chrono::Utc::now(),
    };

    let hook_ctx = format!("@{{:vigil \"{}\" :trigger :harbinger}}", vigil_name);
    let _ = hook_tx.try_send(HookDispatchRequest {
        hook_name: "on-vigil-event".into(),
        context: hook_ctx,
    });

    if let Err(mpsc::error::TrySendError::Full(_)) = tx.try_send(event) {
        warn!(
            vigil = %vigil_name,
            "harbinger queue full, dropping event"
        );
    }

    Ok(())
}
