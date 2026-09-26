//! What the panel is allowed to see of niri's state.
//!
//! The event-stream reducer keeps a full [`EventStreamState`]; widgets get
//! these small, immutable, `PartialEq` projections of it instead. Two reasons:
//!
//! - `PartialEq` is what makes "publish after every event" cheap. Most niri
//!   events (window layout, focus timestamps, screencasts) change nothing the
//!   panel draws, so the watch channel simply does not fire.
//! - The projections carry no compositor vocabulary the widgets would have to
//!   re-derive. Occupancy and urgency are already folded in here.

use std::collections::{BTreeMap, HashSet};

use niri_ipc::state::EventStreamState;

use crate::compositor::{
    KeyboardLayoutSnapshot, WindowView, WindowsSnapshot, WorkspaceView, WorkspacesSnapshot,
};

/// Project the window map with workspace and output context.
pub(crate) fn windows(state: &EventStreamState, connected: bool) -> WindowsSnapshot {
    let mut windows: Vec<_> = state
        .windows
        .windows
        .values()
        .map(|window| {
            let workspace = window
                .workspace_id
                .and_then(|id| state.workspaces.workspaces.get(&id));
            WindowView {
                id: window.id,
                title: window.title.clone().unwrap_or_default(),
                app_id: window.app_id.clone().unwrap_or_default(),
                workspace: workspace.map(|workspace| {
                    workspace
                        .name
                        .clone()
                        .unwrap_or_else(|| workspace.idx.to_string())
                }),
                output: workspace.and_then(|workspace| workspace.output.clone()),
                focused_at_ms: window
                    .focus_timestamp
                    .map(|time| std::time::Duration::from(time).as_millis()),
            }
        })
        .collect();
    windows.sort_by_key(|window| window.id);
    WindowsSnapshot {
        connected,
        windows: if connected { windows } else { Vec::new() },
    }
}

/// Project the workspace half of `state`.
///
/// Occupancy comes from the window map rather than `active_window_id`: a
/// workspace can hold windows without any of them being active, and the two
/// parts of the state are explicitly allowed to disagree for an event or two.
pub(crate) fn workspaces(state: &EventStreamState, connected: bool) -> WorkspacesSnapshot {
    let mut occupied: HashSet<u64> = HashSet::new();
    let mut urgent_windows: HashSet<u64> = HashSet::new();
    for window in state.windows.windows.values() {
        let Some(workspace_id) = window.workspace_id else {
            continue;
        };
        occupied.insert(workspace_id);
        if window.is_urgent {
            urgent_windows.insert(workspace_id);
        }
    }

    let mut outputs: BTreeMap<String, Vec<WorkspaceView>> = BTreeMap::new();
    let mut focused_output = None;
    for workspace in state.workspaces.workspaces.values() {
        // A workspace with no output belongs to a monitor that is not
        // connected right now; no bar can draw it.
        let Some(output) = workspace.output.clone() else {
            continue;
        };
        if workspace.is_focused {
            focused_output = Some(output.clone());
        }
        outputs.entry(output).or_default().push(WorkspaceView {
            id: workspace.id,
            idx: usize::from(workspace.idx),
            name: workspace.name.clone(),
            is_active: workspace.is_active,
            is_focused: workspace.is_focused,
            is_urgent: workspace.is_urgent || urgent_windows.contains(&workspace.id),
            has_windows: occupied.contains(&workspace.id),
            is_persistent: workspace.name.is_some(),
        });
    }

    // The state stores workspaces in a hash map, so the order it hands them
    // back is arbitrary and unstable between events.
    for views in outputs.values_mut() {
        views.sort_by_key(|view| (view.idx, view.id));
    }

    WorkspacesSnapshot {
        connected,
        outputs,
        focused_output,
    }
}

/// Project the keyboard-layout half of `state`.
pub(crate) fn keyboard_layout(state: &EventStreamState, connected: bool) -> KeyboardLayoutSnapshot {
    let layouts = state.keyboard_layouts.keyboard_layouts.as_ref();
    KeyboardLayoutSnapshot {
        connected,
        current_name: layouts
            .and_then(|layouts| layouts.names.get(usize::from(layouts.current_idx)))
            .cloned(),
        layout_count: layouts.map_or(0, |layouts| layouts.names.len()),
    }
}
