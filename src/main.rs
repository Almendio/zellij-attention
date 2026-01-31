mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;
use zellij_tile::shim::pipe_message_to_plugin;

use crate::state::{load_state, save_state, NotificationType, PersistedState};

/// JSON payload structure for pipe messages.
/// External processes send: `zellij pipe --name notification -- '{"event_type":"waiting","pane_id":42}'`
#[derive(Debug, serde::Deserialize)]
struct PipeEvent {
    event_type: String,
    pane_id: u32,
}

struct State {
    permissions_granted: bool,
    tabs: Vec<TabInfo>,
    panes: PaneManifest,
    notification_state: HashMap<u32, HashSet<NotificationType>>,
    zjstatus_url: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            permissions_granted: false,
            tabs: Vec::new(),
            panes: PaneManifest::default(),
            notification_state: HashMap::new(),
            zjstatus_url: "file:~/.config/zellij/plugins/zjstatus.wasm".to_string(),
        }
    }
}

impl State {
    /// Determines which pane is currently focused, accounting for floating pane visibility.
    /// Returns None if no pane is focused or active tab cannot be determined.
    fn determine_focused_pane(&self) -> Option<u32> {
        // Find active tab
        let active_tab = self.tabs.iter().find(|t| t.active)?;

        // Get panes for active tab
        let panes = self.panes.panes.get(&active_tab.position)?;

        // Find focused pane in the correct layer (floating vs tiled)
        // When floating panes are visible, only floating panes can be focused
        // When floating panes are hidden, only tiled panes can be focused
        let focused = panes.iter().find(|p| {
            p.is_focused && (p.is_floating == active_tab.are_floating_panes_visible)
        })?;

        Some(focused.id)
    }

    /// Checks if focused pane has notifications and clears them.
    /// Persists state to disk and notifies zjstatus if any notifications were cleared.
    fn check_and_clear_focus(&mut self) {
        if let Some(focused_pane_id) = self.determine_focused_pane() {
            if self.notification_state.remove(&focused_pane_id).is_some() {
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: Cleared notifications for focused pane {}",
                    focused_pane_id
                );

                // Persist state change
                let persisted = PersistedState {
                    notifications: self.notification_state.clone(),
                };
                if let Err(e) = save_state(&persisted) {
                    eprintln!("zellij-attention: Failed to save state: {}", e);
                }

                // Notify zjstatus
                self.send_to_zjstatus();
            }
        }
    }

    /// Determines the notification state for a tab by checking all panes in that tab.
    /// Returns the highest priority notification: Waiting > Completed > None.
    /// Priority: Waiting is attention-seeking, so it takes precedence.
    fn get_tab_notification_state(&self, tab_position: usize) -> Option<NotificationType> {
        // Get panes for this tab position
        let panes = self.panes.panes.get(&tab_position)?;

        // Check if any pane in this tab has notifications
        // Priority: Waiting > Completed (attention-seeking state first)
        let mut has_completed = false;

        for pane in panes {
            if let Some(notifications) = self.notification_state.get(&pane.id) {
                if notifications.contains(&NotificationType::Waiting) {
                    return Some(NotificationType::Waiting);
                }
                if notifications.contains(&NotificationType::Completed) {
                    has_completed = true;
                }
            }
        }

        if has_completed {
            Some(NotificationType::Completed)
        } else {
            None
        }
    }

    /// Removes notification entries for panes that no longer exist.
    /// Called on every PaneUpdate to handle pane closures.
    fn cleanup_stale_panes(&mut self) {
        // Collect all current pane IDs from all tabs
        let current_pane_ids: HashSet<u32> = self
            .panes
            .panes
            .values()
            .flat_map(|panes| panes.iter().map(|p| p.id))
            .collect();

        // Track if any notifications were removed
        let initial_count = self.notification_state.len();

        // Remove notifications for panes that no longer exist
        self.notification_state.retain(|pane_id, _| {
            let exists = current_pane_ids.contains(pane_id);
            if !exists {
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: Removing notification for closed pane {}",
                    pane_id
                );
            }
            exists
        });

        // Persist and notify zjstatus if any notifications were removed
        if self.notification_state.len() != initial_count {
            let persisted = PersistedState {
                notifications: self.notification_state.clone(),
            };
            if let Err(e) = save_state(&persisted) {
                eprintln!("zellij-attention: Failed to save state: {}", e);
            }

            // Notify zjstatus
            self.send_to_zjstatus();
        }
    }

    /// Formats the current notification state as a summary string for zjstatus.
    /// Returns format like "! 2, 4" for waiting tabs, "* 3" for completed, or combined.
    fn format_notification_summary(&self) -> String {
        let mut waiting_tabs: Vec<usize> = Vec::new();
        let mut completed_tabs: Vec<usize> = Vec::new();

        for tab in &self.tabs {
            if let Some(notification) = self.get_tab_notification_state(tab.position) {
                match notification {
                    NotificationType::Waiting => waiting_tabs.push(tab.position + 1),
                    NotificationType::Completed => completed_tabs.push(tab.position + 1),
                }
            }
        }

        // Format output: "! 2, 4 * 3" means tabs 2,4 waiting, tab 3 completed
        let mut parts: Vec<String> = Vec::new();
        if !waiting_tabs.is_empty() {
            let tabs = waiting_tabs
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("! {}", tabs));
        }
        if !completed_tabs.is_empty() {
            let tabs = completed_tabs
                .iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("* {}", tabs));
        }

        parts.join(" ")
    }

    /// Sends the current notification state to zjstatus via pipe.
    fn send_to_zjstatus(&self) {
        let summary = self.format_notification_summary();
        let message_name = format!("zjstatus::pipe::claude::{}", summary);

        #[cfg(debug_assertions)]
        eprintln!("zellij-attention: Sending to zjstatus: {}", message_name);

        pipe_message_to_plugin(
            MessageToPlugin::new(&message_name).with_plugin_url(&self.zjstatus_url),
        );
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // Request permissions needed for tab/pane state
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);

        // Subscribe to events (no Mouse needed anymore)
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            EventType::PaneUpdate,
        ]);

        // Load persisted state
        self.notification_state = load_state().notifications;

        // Allow override of zjstatus URL via configuration
        if let Some(url) = configuration.get("zjstatus_url") {
            self.zjstatus_url = url.clone();
        }

        eprintln!("zellij-attention: loaded, zjstatus_url={}\n", self.zjstatus_url);
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(status) => {
                self.permissions_granted = status == PermissionStatus::Granted;
                // Plugin should not be selectable (runs as background)
                set_selectable(false);
                eprintln!(
                    "zellij-attention: permissions={}, selectable=false\n",
                    self.permissions_granted
                );

                // Send initial state to zjstatus
                self.send_to_zjstatus();
                true
            }
            Event::TabUpdate(tab_info) => {
                self.tabs = tab_info;
                self.check_and_clear_focus();

                // Tab list changed, update zjstatus
                self.send_to_zjstatus();

                #[cfg(debug_assertions)]
                eprintln!("zellij-attention: TabUpdate - {} tabs", self.tabs.len());
                false // No need to render, we're invisible
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes = pane_manifest;
                self.cleanup_stale_panes();
                self.check_and_clear_focus();
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: PaneUpdate - {} tabs with panes",
                    self.panes.panes.len()
                );
                false // No need to render, we're invisible
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        // Plugin runs as backend for zjstatus, no visible UI needed
    }

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        eprintln!(
            "zellij-attention: pipe name={} payload={:?} args={:?}\n",
            pipe_message.name, pipe_message.payload, pipe_message.args
        );

        // Only handle messages to "notification" pipe, silently ignore others
        if pipe_message.name != "notification" {
            return false;
        }

        // Parse event_type and pane_id from either payload (JSON) or args
        let (event_type, pane_id) = if let Some(payload) = &pipe_message.payload {
            // JSON payload format: {"event_type":"waiting","pane_id":0}
            match serde_json::from_str::<PipeEvent>(payload) {
                Ok(e) => (e.event_type, e.pane_id),
                Err(e) => {
                    eprintln!("zellij-attention: Failed to parse JSON: {}\n", e);
                    return false;
                }
            }
        } else {
            // Args format: --args "event_type=waiting,pane_id=0"
            let event_type = match pipe_message.args.get("event_type") {
                Some(t) => t.clone(),
                None => {
                    eprintln!("zellij-attention: Missing event_type in args\n");
                    return false;
                }
            };
            let pane_id: u32 = match pipe_message.args.get("pane_id") {
                Some(id) => match id.parse() {
                    Ok(n) => n,
                    Err(_) => {
                        eprintln!("zellij-attention: Invalid pane_id: {}\n", id);
                        return false;
                    }
                },
                None => {
                    eprintln!("zellij-attention: Missing pane_id in args\n");
                    return false;
                }
            };
            (event_type, pane_id)
        };

        // Normalize event_type to lowercase and match
        let notification_type = match event_type.to_lowercase().as_str() {
            "waiting" => NotificationType::Waiting,
            "completed" => NotificationType::Completed,
            unknown => {
                eprintln!("zellij-attention: Unknown event type: {}\n", unknown);
                return false;
            }
        };

        // Latest wins: create new HashSet with single entry, replacing any existing
        let mut notifications = HashSet::new();
        notifications.insert(notification_type);
        self.notification_state.insert(pane_id, notifications);

        eprintln!(
            "zellij-attention: Set pane {} to {:?}\n",
            pane_id, notification_type
        );

        // Persist state change
        let persisted = PersistedState {
            notifications: self.notification_state.clone(),
        };
        if let Err(e) = save_state(&persisted) {
            eprintln!("zellij-attention: Failed to save state: {}", e);
        }

        // Notify zjstatus
        self.send_to_zjstatus();

        false // No need to render, we're invisible
    }
}

register_plugin!(State);
