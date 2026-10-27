//! Stock Mango 0.18.2 replacement snapshots and panel projections.
//!
//! Wire keys follow `src/ipc/ipc.c` (`build_monitor_json`, `build_tags_json`,
//! `monitor_active_client`, and `build_client_json`) at:
//! <https://github.com/mangowm/mango/blob/0.18.2/src/ipc/ipc.c>.
//! Unknown fields are deliberately ignored; invalid identities never enter the
//! connector registry. The two watches are independent, so their client and
//! monitor lists are not required to agree across a transition.

use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use topbar_core::config::MangoConfig;

use crate::compositor::{
    KeyboardLayoutSnapshot, WindowView, WindowsSnapshot, WorkspaceView, WorkspacesSnapshot,
};
use crate::error::SvcError;

/// TAG0_MASK occupies bit 31; regular tags use the remaining bits.
const MAX_TAG: u32 = 31;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Tag {
    pub index: u32,
    pub is_active: bool,
    pub is_urgent: bool,
    pub client_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ActiveClient {
    #[serde(deserialize_with = "Option::deserialize")]
    pub id: Option<u32>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub title: Option<String>,
    #[serde(deserialize_with = "Option::deserialize")]
    pub appid: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Monitor {
    pub name: String,
    pub active: bool,
    /// Native geometry is retained even while an output is disabled.
    pub width: i32,
    pub height: i32,
    pub tags: Vec<Tag>,
    pub active_tags: Vec<u32>,
    pub active_client: ActiveClient,
    /// A missing keyboard description is not a broken workspace connection.
    pub keyboardlayout: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Client {
    pub id: u32,
    pub title: String,
    pub appid: String,
    pub monitor: String,
    pub tags: Vec<u32>,
    pub is_focused: bool,
    pub is_urgent: bool,
}

#[derive(Deserialize)]
struct MonitorsResponse {
    monitors: Vec<Monitor>,
}

#[derive(Deserialize)]
struct ClientsResponse {
    clients: Vec<Client>,
}

fn protocol(message: impl Into<String>) -> SvcError {
    SvcError::Protocol(message.into())
}

/// Decode a native reply once, giving its error envelope precedence over data.
pub(crate) fn decode<T: serde::de::DeserializeOwned>(line: &str) -> Result<T, SvcError> {
    let value: serde_json::Value = serde_json::from_str(line)
        .map_err(|error| protocol(format!("invalid Mango reply: {error}")))?;
    if let Some(error) = value.as_object().and_then(|object| object.get("error")) {
        return Err(match error.as_str() {
            Some(message) => SvcError::Rejected(message.to_owned()),
            None => protocol("invalid Mango error envelope"),
        });
    }
    serde_json::from_value(value).map_err(|error| protocol(format!("invalid Mango reply: {error}")))
}

/// Parse a complete replacement, never an incremental monitor event.
pub(crate) fn parse_monitors(line: &str) -> Result<Vec<Monitor>, SvcError> {
    let response: MonitorsResponse = decode(line)?;
    let mut names = HashSet::new();
    let mut active_clients = HashSet::new();
    let mut focused = false;
    for monitor in &response.monitors {
        validate_monitor(monitor)?;
        if !names.insert(monitor.name.as_str()) {
            return Err(protocol("duplicate Mango monitor connector"));
        }
        if monitor.active && std::mem::replace(&mut focused, true) {
            return Err(protocol("multiple focused Mango monitors"));
        }
        if let Some(id) = monitor.active_client.id
            && !active_clients.insert(id)
        {
            return Err(protocol("duplicate Mango active client ID"));
        }
    }
    Ok(response.monitors)
}

pub(crate) fn parse_monitor(line: &str) -> Result<Monitor, SvcError> {
    let monitor = decode(line)?;
    validate_monitor(&monitor)?;
    Ok(monitor)
}

fn validate_monitor(monitor: &Monitor) -> Result<(), SvcError> {
    if monitor.name.is_empty() {
        return Err(protocol("empty Mango monitor connector"));
    }
    let mut indices = HashSet::new();
    for tag in &monitor.tags {
        if !(1..=MAX_TAG).contains(&tag.index) || !indices.insert(tag.index) {
            return Err(protocol("invalid or duplicate Mango tag index"));
        }
    }
    let mut active = HashSet::new();
    for &tag in &monitor.active_tags {
        if !active.insert(tag) || (tag != 0 && !indices.contains(&tag)) {
            return Err(protocol("invalid or duplicate Mango active tag"));
        }
    }
    let overview = monitor.active_tags == [0];
    if active.contains(&0) && !overview {
        return Err(protocol(
            "Mango overview tag mixed with regular active tags",
        ));
    }
    if !overview
        && monitor
            .tags
            .iter()
            .any(|tag| tag.is_active != active.contains(&tag.index))
    {
        return Err(protocol("Mango active tag flags disagree with active_tags"));
    }
    match &monitor.active_client {
        ActiveClient {
            id: None,
            title: None,
            appid: None,
        } => {}
        ActiveClient {
            id: Some(id),
            title: Some(_),
            appid: Some(_),
        } if *id != 0 => {}
        _ => return Err(protocol("inconsistent Mango active client identity")),
    }
    Ok(())
}

pub(crate) fn parse_clients(line: &str) -> Result<Vec<Client>, SvcError> {
    let response: ClientsResponse = decode(line)?;
    let mut ids = HashSet::new();
    for client in &response.clients {
        validate_client(client)?;
        if !ids.insert(client.id) {
            return Err(protocol("duplicate Mango client ID"));
        }
    }
    Ok(response.clients)
}

pub(crate) fn parse_client(line: &str) -> Result<Client, SvcError> {
    let client = decode(line)?;
    validate_client(&client)?;
    Ok(client)
}

fn validate_client(client: &Client) -> Result<(), SvcError> {
    if client.id == 0 {
        return Err(protocol("invalid Mango client ID"));
    }
    let mut tags = HashSet::new();
    for &tag in &client.tags {
        // Native tags_mask_to_array can report tag zero alongside regular tags.
        if tag > MAX_TAG || !tags.insert(tag) {
            return Err(protocol("invalid or duplicate Mango client tag"));
        }
    }
    Ok(())
}

struct Connector {
    name: String,
    selected: Option<u32>,
}

/// Slots and overview memory outlive socket pairs and monitor removals.
#[derive(Default)]
pub(crate) struct Registry {
    connectors: Vec<Connector>,
}

impl Registry {
    /// Decode an assigned connector slot, not proof that its tag is still live.
    pub(crate) fn resolve(&self, id: u64) -> Option<(String, u32)> {
        let tag = id as u32;
        if !(1..=MAX_TAG).contains(&tag) {
            return None;
        }
        let slot = usize::try_from(id >> 32).ok()?;
        Some((self.connectors.get(slot)?.name.clone(), tag))
    }

    pub(crate) fn workspaces(
        &mut self,
        monitors: &[Monitor],
        config: &MangoConfig,
        connected: bool,
    ) -> Result<WorkspacesSnapshot, SvcError> {
        let mut outputs = BTreeMap::new();
        let mut focused_output = None;
        for monitor in monitors {
            // ponytail: output counts are tiny; keep one owned connector name
            // per lifetime slot instead of a second reverse-lookup map.
            let slot = match self
                .connectors
                .iter()
                .position(|connector| connector.name == monitor.name)
            {
                Some(slot) => slot,
                None => {
                    let slot = self.connectors.len();
                    u32::try_from(slot).map_err(|_| protocol("Mango connector slots exhausted"))?;
                    self.connectors.push(Connector {
                        name: monitor.name.clone(),
                        selected: None,
                    });
                    slot
                }
            };
            let connector = &mut self.connectors[slot];
            let overview = monitor.active_tags == [0];
            let selected = if overview {
                connector
                    .selected
                    .filter(|selected| monitor.tags.iter().any(|tag| tag.index == *selected))
                    .or_else(|| monitor.tags.iter().map(|tag| tag.index).min())
            } else {
                monitor.active_tags.iter().copied().min()
            };
            if !overview {
                connector.selected = selected;
            }
            if monitor.active {
                focused_output = Some(monitor.name.clone());
            }
            let mut views: Vec<_> = monitor
                .tags
                .iter()
                .map(|tag| {
                    let is_active = if overview {
                        Some(tag.index) == selected
                    } else {
                        tag.is_active
                    };
                    WorkspaceView {
                        id: ((slot as u64) << 32) | u64::from(tag.index),
                        idx: tag.index as usize,
                        name: None,
                        is_active,
                        is_focused: monitor.active && is_active,
                        is_urgent: tag.is_urgent,
                        has_windows: tag.client_count > 0,
                        is_persistent: config.persistent_tags.contains(&tag.index),
                    }
                })
                .collect();
            views.sort_by_key(|view| view.idx);
            outputs.insert(monitor.name.clone(), views);
        }
        Ok(WorkspacesSnapshot {
            connected,
            outputs,
            focused_output,
        })
    }
}

pub(crate) fn keyboard_layout(
    monitors: &[Monitor],
    config: &MangoConfig,
    connected: bool,
) -> KeyboardLayoutSnapshot {
    // The description is global in native build_monitor_json, even on inactive
    // outputs. Prefer the selected monitor, then any available description.
    let current_name = monitors
        .iter()
        .find(|monitor| monitor.active)
        .and_then(|monitor| monitor.keyboardlayout.as_ref())
        .or_else(|| {
            monitors
                .iter()
                .find_map(|monitor| monitor.keyboardlayout.as_ref())
        })
        .filter(|name| !name.is_empty())
        .cloned();
    KeyboardLayoutSnapshot {
        connected,
        current_name,
        layout_count: config.keyboard_layouts.len(),
    }
}

pub(crate) fn windows(clients: &[Client], connected: bool) -> WindowsSnapshot {
    let mut windows = if connected {
        clients
            .iter()
            .map(|client| {
                let mut tags: Vec<_> = client
                    .tags
                    .iter()
                    .copied()
                    .filter(|tag| *tag != 0)
                    .collect();
                tags.sort_unstable();
                WindowView {
                    id: u64::from(client.id),
                    title: client.title.clone(),
                    app_id: client.appid.clone(),
                    workspace: (!tags.is_empty()).then(|| {
                        tags.iter()
                            .map(u32::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    }),
                    output: (!client.monitor.is_empty()).then(|| client.monitor.clone()),
                    focused_at_ms: None,
                }
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    windows.sort_by_key(|window| window.id);
    WindowsSnapshot { connected, windows }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Source-derived stock JSON fixtures, not recordings: Mango 0.18.2
    // src/ipc/ipc.c build_monitor_json/build_tags_json/monitor_active_client
    // and build_client_json/build_all_clients_array. Unknown native fields
    // remain here to exercise forward-compatible typed deserialization.
    pub(super) const MONITORS: &str = r#"{"monitors":[{"name":"eDP-1","active":true,"is_hdr":false,"x":0,"width":1920,"height":1080,"tag_num":3,"tags":[{"index":1,"is_active":false,"is_urgent":false,"layout":"tile","client_count":2},{"index":2,"is_active":true,"is_urgent":false,"layout":"tile","client_count":0},{"index":3,"is_active":false,"is_urgent":true,"layout":"tile","client_count":1}],"active_tags":[2],"active_client":{"id":null,"title":null,"appid":null},"keyboardlayout":"English (US)"},{"name":"DP-2","active":false,"width":2560,"height":1440,"tags":[{"index":1,"is_active":true,"is_urgent":false,"client_count":0},{"index":2,"is_active":false,"is_urgent":false,"client_count":0}],"active_tags":[1],"active_client":{"id":null,"title":null,"appid":null},"keyboardlayout":"English (US)"}]}"#;
    pub(super) const CLIENTS: &str = r#"{"clients":[{"id":9,"pid":123,"title":"Editor","appid":"emacs","monitor":"DP-2","tags":[2,1],"is_focused":false,"is_urgent":true,"is_floating":false},{"id":2,"title":"Browser","appid":"firefox","monitor":"eDP-1","tags":[1],"is_focused":true,"is_urgent":false}]}"#;

    fn config() -> MangoConfig {
        MangoConfig {
            persistent_tags: vec![2, 8],
            keyboard_layouts: vec!["us".into(), "de".into()],
        }
    }

    #[test]
    fn stable_connector_ids_survive_reordering_removal_and_reconnect() {
        let mut monitors = parse_monitors(MONITORS).unwrap();
        let mut registry = Registry::default();
        let first = registry.workspaces(&monitors, &config(), true).unwrap();
        let internal_id = first.for_output("eDP-1")[0].id;
        let external_id = first.for_output("DP-2")[0].id;
        assert_ne!(internal_id, external_id);
        monitors.reverse();
        assert_eq!(
            registry.workspaces(&monitors, &config(), true).unwrap(),
            first
        );
        let removed = monitors.remove(0);
        registry.workspaces(&monitors, &config(), true).unwrap();
        let mut new_output = removed.clone();
        new_output.name = "HDMI-A-1".into();
        monitors.push(new_output);
        let replacement = registry.workspaces(&monitors, &config(), true).unwrap();
        assert_ne!(replacement.for_output("HDMI-A-1")[0].id, external_id);
        assert_eq!(registry.resolve(external_id), Some(("DP-2".into(), 1)));
        monitors.push(removed);
        let returned = registry.workspaces(&monitors, &config(), true).unwrap();
        assert_eq!(returned.for_output("DP-2")[0].id, external_id);
        assert_eq!(returned.for_output("eDP-1")[0].id, internal_id);
        assert!(registry.resolve(0).is_none());
        assert!(registry.resolve(u64::MAX).is_none());
    }

    #[test]
    fn native_occupancy_urgency_focus_and_configured_persistence() {
        let monitors = parse_monitors(MONITORS).unwrap();
        let snapshot = Registry::default()
            .workspaces(&monitors, &config(), true)
            .unwrap();
        assert_eq!(snapshot.focused_output.as_deref(), Some("eDP-1"));
        let internal = snapshot.for_output("eDP-1");
        assert!(internal[0].has_windows);
        assert!(!internal[1].has_windows);
        assert!(internal[1].is_active && internal[1].is_focused && internal[1].is_persistent);
        assert!(internal[2].is_urgent);
        assert!(internal.iter().all(|view| view.name.is_none()));
        assert!(snapshot.for_output("DP-2")[0].is_active);
        assert!(!snapshot.for_output("DP-2")[0].is_focused);
        assert_eq!(
            internal.len(),
            3,
            "configured tag eight must not be fabricated"
        );
    }

    #[test]
    fn overview_remembers_one_tag_per_connector_and_uses_lowest_reported_fallback() {
        let mut monitors = parse_monitors(MONITORS).unwrap();
        let mut registry = Registry::default();
        registry.workspaces(&monitors, &config(), true).unwrap();
        for monitor in &mut monitors {
            monitor.active_tags = vec![0];
            for tag in &mut monitor.tags {
                tag.is_active = true;
            }
        }
        registry.workspaces(&[], &config(), false).unwrap();
        monitors.reverse();
        let overview = registry.workspaces(&monitors, &config(), true).unwrap();
        for (name, selected) in [("eDP-1", 2), ("DP-2", 1)] {
            let active: Vec<_> = overview
                .for_output(name)
                .iter()
                .filter(|tag| tag.is_active)
                .map(|tag| tag.idx)
                .collect();
            assert_eq!(active, vec![selected]);
        }
        monitors.reverse();
        monitors[0].tags.retain(|tag| tag.index != 2);
        monitors[0].tags.reverse();
        let missing = registry.workspaces(&monitors, &config(), true).unwrap();
        assert!(missing.for_output("eDP-1")[0].is_active);
        let cold = Registry::default()
            .workspaces(&monitors, &config(), true)
            .unwrap();
        assert_eq!(cold, missing);
        monitors[0].tags.clear();
        assert!(
            registry
                .workspaces(&monitors, &config(), true)
                .unwrap()
                .for_output("eDP-1")
                .is_empty()
        );
    }

    #[test]
    fn keyboard_uses_live_description_and_only_configured_choice_count() {
        let mut monitors = parse_monitors(MONITORS).unwrap();
        let layout = keyboard_layout(&monitors, &config(), true);
        assert_eq!(layout.current(), Some("English (US)"));
        assert_eq!(layout.layout_count, 2);
        assert_eq!(
            keyboard_layout(&monitors, &MangoConfig::default(), true).layout_count,
            0
        );
        for monitor in &mut monitors {
            monitor.keyboardlayout = None;
        }
        let missing = keyboard_layout(&monitors, &MangoConfig::default(), true);
        assert!(missing.connected);
        assert_eq!(missing.current(), None);
        assert_eq!(missing.layout_count, 0);
    }

    #[test]
    fn clients_keep_numeric_tags_native_identity_and_no_fabricated_focus_time() {
        let mut clients = parse_clients(CLIENTS).unwrap();
        let snapshot = windows(&clients, true);
        assert_eq!(
            snapshot
                .windows
                .iter()
                .map(|window| window.id)
                .collect::<Vec<_>>(),
            vec![2, 9]
        );
        assert_eq!(snapshot.windows[1].title, "Editor");
        assert_eq!(snapshot.windows[1].app_id, "emacs");
        assert_eq!(snapshot.windows[1].output.as_deref(), Some("DP-2"));
        assert_eq!(snapshot.windows[1].workspace.as_deref(), Some("1, 2"));
        assert!(
            snapshot
                .windows
                .iter()
                .all(|window| window.focused_at_ms.is_none())
        );
        clients[0].monitor.clear();
        clients[0].tags = vec![0];
        let unassigned = windows(&clients, true);
        assert!(unassigned.windows[1].output.is_none());
        assert!(unassigned.windows[1].workspace.is_none());
        assert!(windows(&clients, false).windows.is_empty());
    }

    #[test]
    fn empty_replacements_and_null_active_client_are_valid() {
        let monitors = parse_monitors(r#"{"monitors":[]}"#).unwrap();
        let clients = parse_clients(r#"{"clients":[]}"#).unwrap();
        let workspace = Registry::default()
            .workspaces(&monitors, &config(), true)
            .unwrap();
        assert!(workspace.connected && workspace.outputs.is_empty());
        assert!(windows(&clients, true).windows.is_empty());
        assert!(
            parse_monitors(MONITORS).unwrap()[0]
                .active_client
                .id
                .is_none()
        );
        let monitor =
            serde_json::from_str::<serde_json::Value>(MONITORS).unwrap()["monitors"][0].to_string();
        assert!(parse_monitor(&monitor).is_ok());
        let client =
            serde_json::from_str::<serde_json::Value>(CLIENTS).unwrap()["clients"][0].to_string();
        assert!(parse_client(&client).is_ok());
    }

    #[test]
    fn monitor_geometry_is_required_but_disabled_outputs_retain_their_native_tags() {
        let original: serde_json::Value = serde_json::from_str(MONITORS).unwrap();
        for dimension in ["width", "height"] {
            let mut missing = original.clone();
            missing["monitors"][0]
                .as_object_mut()
                .unwrap()
                .remove(dimension);
            assert!(parse_monitor(&missing["monitors"][0].to_string()).is_err());
            assert!(parse_monitors(&missing.to_string()).is_err());
            for invalid in [
                serde_json::json!(null),
                serde_json::json!(1.5),
                serde_json::json!(i64::from(i32::MAX) + 1),
            ] {
                let mut malformed = original.clone();
                malformed["monitors"][0][dimension] = invalid;
                assert!(parse_monitors(&malformed.to_string()).is_err());
            }
        }
        for (width, height) in [(0, 1080), (1920, 0), (-1, -1)] {
            let mut disabled = original.clone();
            disabled["monitors"][0]["width"] = serde_json::json!(width);
            disabled["monitors"][0]["height"] = serde_json::json!(height);
            let monitors = parse_monitors(&disabled.to_string()).unwrap();
            assert_eq!((monitors[0].width, monitors[0].height), (width, height));
            let projected = Registry::default()
                .workspaces(&monitors, &config(), true)
                .unwrap();
            assert!(projected.connected);
            assert_eq!(projected.for_output("eDP-1").len(), 3);
        }
    }

    #[test]
    fn native_error_envelopes_take_precedence_over_other_fields() {
        let rejected = r#"{"error":"monitor not found","monitors":[]}"#;
        assert!(matches!(
            parse_monitors(rejected),
            Err(SvcError::Rejected(_))
        ));
        for line in [
            r#"{"error":null,"clients":[]}"#,
            r#"{"error":false,"clients":[]}"#,
        ] {
            assert!(matches!(parse_clients(line), Err(SvcError::Protocol(_))));
        }
    }

    #[test]
    fn malformed_inconsistent_and_duplicate_replacements_are_rejected() {
        for line in [
            "",
            "null",
            "{}",
            r#"{"monitors":null}"#,
            r#"{"monitors":[{}]}"#,
            "{\"monitors\":",
        ] {
            assert!(parse_monitors(line).is_err(), "{line}");
        }
        let original: serde_json::Value = serde_json::from_str(MONITORS).unwrap();
        for (path, value) in [
            ("name", serde_json::json!("")),
            ("active_tags", serde_json::json!([0, 2])),
            ("active_tags", serde_json::json!([2, 2])),
            ("active_tags", serde_json::json!([9])),
            ("active_tags", serde_json::json!([1])),
            (
                "active_client",
                serde_json::json!({"id":0,"title":"x","appid":"x"}),
            ),
            ("active_client", serde_json::json!({})),
            (
                "active_client",
                serde_json::json!({"id":null,"title":"x","appid":null}),
            ),
        ] {
            let mut bad = original.clone();
            bad["monitors"][0][path] = value;
            assert!(parse_monitors(&bad.to_string()).is_err(), "{bad}");
        }
        for index in [0, 2, 32, u32::MAX] {
            let mut bad = original.clone();
            bad["monitors"][0]["tags"][0]["index"] = serde_json::json!(index);
            assert!(parse_monitors(&bad.to_string()).is_err(), "{bad}");
        }
        let mut duplicate = original.clone();
        duplicate["monitors"][1]["name"] = duplicate["monitors"][0]["name"].clone();
        assert!(parse_monitors(&duplicate.to_string()).is_err());
        let mut focused = original;
        focused["monitors"][1]["active"] = serde_json::json!(true);
        assert!(parse_monitors(&focused.to_string()).is_err());
        for id in [
            serde_json::json!(0),
            serde_json::json!(-1),
            serde_json::json!(4294967296_u64),
            serde_json::json!(1.5),
        ] {
            let mut bad: serde_json::Value = serde_json::from_str(CLIENTS).unwrap();
            bad["clients"][0]["id"] = id;
            assert!(parse_clients(&bad.to_string()).is_err());
        }
        for tags in [
            serde_json::json!([1, 1]),
            serde_json::json!([32]),
            serde_json::json!([-1]),
        ] {
            let mut bad: serde_json::Value = serde_json::from_str(CLIENTS).unwrap();
            bad["clients"][0]["tags"] = tags;
            assert!(parse_clients(&bad.to_string()).is_err());
        }
        let mut duplicate: serde_json::Value = serde_json::from_str(CLIENTS).unwrap();
        duplicate["clients"][1]["id"] = duplicate["clients"][0]["id"].clone();
        assert!(parse_clients(&duplicate.to_string()).is_err());
        let mut special: serde_json::Value = serde_json::from_str(CLIENTS).unwrap();
        special["clients"][0]["tags"] = serde_json::json!([0, 1]);
        assert!(parse_clients(&special.to_string()).is_ok());
    }
}
