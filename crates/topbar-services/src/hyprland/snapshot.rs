//! Typed wire data and complete UI projections; no JSON walking in widgets.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;

use crate::SvcError;
use crate::compositor::{KeyboardLayoutSnapshot, WorkspaceId, WorkspaceView, WorkspacesSnapshot};

// Only native numeric IDs and booleans enter this JSON. Names/titles never enter Lua.
pub(super) const SNAPSHOT_REQUEST: &str = "[[BATCH]]j/monitors;j/workspaces;j/clients;j/devices;/repl local r={} for _,w in ipairs(hl.get_workspaces()) do table.insert(r,string.format('{\"id\":%d,\"urgent\":%s}',w.id,tostring(w.has_urgent))) end return '['..table.concat(r,',')..']'";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Monitor {
    name: String,
    active_workspace: WorkspaceRef,
    focused: bool,
}

#[derive(Debug, Deserialize)]
pub(super) struct WorkspaceRef {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct Workspace {
    id: i64,
    name: String,
    monitor: String,
    windows: usize,
    ispersistent: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Client {
    pub address: String,
    pub class: String,
    pub initial_class: String,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i64,
}

#[derive(Debug, Deserialize)]
pub(super) struct Devices {
    keyboards: Vec<Keyboard>,
}

#[derive(Debug, Deserialize)]
struct Keyboard {
    main: bool,
    active_keymap: String,
    layout: String,
}

#[derive(Debug, Deserialize)]
struct Urgency {
    id: i64,
    urgent: bool,
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(reply: &str) -> Result<T, SvcError> {
    serde_json::from_str(reply).map_err(|error| SvcError::Protocol(format!("Hyprland: {error}")))
}

pub(super) struct Projection {
    pub workspaces: WorkspacesSnapshot,
    pub keyboard: KeyboardLayoutSnapshot,
    pub clients: Vec<Client>,
}

pub(super) fn project(reply: &str) -> Result<Projection, SvcError> {
    let mut parts = reply.split("\n\n\n");
    let mut next = || {
        parts
            .next()
            .ok_or_else(|| SvcError::Protocol("incomplete Hyprland snapshot batch".into()))
    };
    let monitors: Vec<Monitor> = decode(next()?)?;
    let mut workspaces: Vec<Workspace> = decode(next()?)?;
    let clients: Vec<Client> = decode(next()?)?;
    let devices: Devices = decode(next()?)?;
    let urgency: Vec<Urgency> = decode(next()?)?;
    if parts.next().is_some() {
        return Err(SvcError::Protocol(
            "extra Hyprland snapshot batch reply".into(),
        ));
    }
    let urgent: HashMap<i64, bool> = urgency.into_iter().map(|w| (w.id, w.urgent)).collect();
    let mut monitor_map = HashMap::new();
    let mut focused_output = None;
    let mut outputs = BTreeMap::new();
    for monitor in &monitors {
        if monitor.name.is_empty() || monitor_map.insert(monitor.name.as_str(), monitor).is_some() {
            return Err(SvcError::Protocol(
                "duplicate/empty Hyprland connector".into(),
            ));
        }
        if monitor.focused && focused_output.replace(monitor.name.clone()).is_some() {
            return Err(SvcError::Protocol(
                "multiple focused Hyprland monitors".into(),
            ));
        }
        outputs.insert(monitor.name.clone(), Vec::new());
    }
    workspaces.retain(|workspace| !workspace.name.starts_with("special:"));
    workspaces.sort_by(|a, b| match (a.id > 0, b.id > 0) {
        (true, true) => a.id.cmp(&b.id),
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        (false, false) => (&a.name, a.id).cmp(&(&b.name, b.id)),
    });
    let mut ids = HashSet::new();
    for workspace in workspaces {
        if !ids.insert(workspace.id) {
            return Err(SvcError::Protocol("duplicate Hyprland workspace ID".into()));
        }
        // Like Niri's outputless workspaces, a persistent workspace can outlive
        // its connector. It belongs on no currently mapped bar.
        let Some(monitor) = monitor_map.get(workspace.monitor.as_str()) else {
            continue;
        };
        let is_urgent = *urgent.get(&workspace.id).ok_or_else(|| {
            SvcError::Protocol("Hyprland urgency query omitted a workspace".into())
        })?;
        let views = outputs
            .get_mut(&workspace.monitor)
            .expect("validated connector");
        let is_active = monitor.active_workspace.id == workspace.id;
        views.push(WorkspaceView {
            id: WorkspaceId::Hyprland(workspace.id),
            idx: views.len() + 1,
            name: (workspace.name != workspace.id.to_string()).then_some(workspace.name),
            is_active,
            is_focused: is_active && monitor.focused,
            is_urgent,
            has_windows: workspace.windows > 0,
            is_persistent: workspace.ispersistent,
        });
    }
    // A query batch is synchronous in the compositor. Missing active workspaces
    // therefore mean an incomplete projection, not a reason to publish half state.
    for monitor in &monitors {
        if monitor.active_workspace.id != 0
            && !outputs[&monitor.name]
                .iter()
                .any(|workspace| workspace.is_active)
        {
            return Err(SvcError::Protocol(
                "Hyprland active workspace missing from snapshot".into(),
            ));
        }
    }
    let mut mains = devices.keyboards.iter().filter(|keyboard| keyboard.main);
    let main = mains.next();
    if mains.next().is_some() {
        return Err(SvcError::Protocol(
            "multiple main Hyprland keyboards".into(),
        ));
    }
    let keyboard = KeyboardLayoutSnapshot {
        connected: true,
        current_name: main.map(|keyboard| keyboard.active_keymap.clone()),
        layout_count: main.map_or(0, |keyboard| {
            keyboard
                .layout
                .split(',')
                .filter(|layout| !layout.trim().is_empty())
                .count()
        }),
    };
    Ok(Projection {
        workspaces: WorkspacesSnapshot {
            connected: true,
            outputs,
            focused_output,
        },
        keyboard,
        clients,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn fixture() -> String {
        [
            r#"[{"name":"DP-1","activeWorkspace":{"id":2},"focused":false},{"name":"eDP-1","activeWorkspace":{"id":-9},"focused":true}]"#,
            r#"[{"id":3,"name":"3","monitor":"DP-1","windows":0,"ispersistent":true},{"id":-9,"name":"notes","monitor":"eDP-1","windows":1,"ispersistent":false},{"id":2,"name":"browser","monitor":"DP-1","windows":0,"ispersistent":true},{"id":-99,"name":"special:scratch","monitor":"DP-1","windows":1,"ispersistent":false}]"#,
            r#"[]"#,
            r#"{"keyboards":[{"main":false,"active_keymap":"Other","layout":"de"},{"main":true,"active_keymap":"Russian","layout":"us,ru"}]}"#,
            r#"[{"id":2,"urgent":false},{"id":3,"urgent":false},{"id":-9,"urgent":true},{"id":-99,"urgent":false}]"#,
        ].join("\n\n\n")
    }

    #[test]
    fn native_ids_connectors_main_keyboard_and_persistent_empty_workspaces() {
        let p = project(&fixture()).unwrap();
        let dp = p.workspaces.for_output("DP-1");
        assert_eq!(
            dp.iter().map(|w| w.id).collect::<Vec<_>>(),
            [WorkspaceId::Hyprland(2), WorkspaceId::Hyprland(3)]
        );
        assert!(dp[0].is_active && !dp[0].is_focused);
        assert!(dp[1].is_persistent && !dp[1].has_windows);
        assert_eq!(dp[1].name, None);
        assert_eq!(dp[1].idx, 2);
        assert_eq!(p.workspaces.focused_output.as_deref(), Some("eDP-1"));
        assert!(p.workspaces.for_output("eDP-1")[0].is_urgent);
        assert_eq!(p.keyboard.current(), Some("Russian"));
        assert!(p.keyboard.is_switchable());
        assert!(
            project(&fixture().replace("\"id\":-9,\"urgent\":true", "\"id\":-8,\"urgent\":true"))
                .is_err()
        );
        let unplugged = fixture().replace(
            r#"{"name":"DP-1","activeWorkspace":{"id":2},"focused":false},"#,
            "",
        );
        let unplugged = project(&unplugged).unwrap();
        assert_eq!(
            unplugged
                .workspaces
                .outputs
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["eDP-1"]
        );
        assert_eq!(
            unplugged.workspaces.for_output("eDP-1")[0].id,
            WorkspaceId::Hyprland(-9)
        );
    }
}
