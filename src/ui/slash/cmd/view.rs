//! /swarm, /panel and /display: view commands, owned by the view seam
//! (`ui::view`). The UI loop routes them there before the agent's busy
//! gate; this forwards the ones that reach `handle_slash` another way.

use crate::ui::view::{self, ViewEvent};

pub(crate) fn cmd_view(parts: &[&str]) {
    let name = parts.first().map_or("", |p| p.trim_start_matches('/'));
    view::submit(ViewEvent::command(name, &parts[parts.len().min(1)..]));
}
