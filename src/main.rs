mod config;
mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;
use zellij_tile::shim::{rename_tab, unblock_cli_pipe_input};

use crate::config::NotificationConfig;
use crate::state::{load_state, save_state, NotificationType, PersistedState};

struct State {
    permissions_granted: bool,
    tabs: Vec<TabInfo>,
    panes: PaneManifest,
    notification_state: HashMap<u32, HashSet<NotificationType>>,
    original_tab_names: HashMap<usize, String>,
    config: NotificationConfig,
}

impl Default for State {
    fn default() -> Self {
        Self {
            permissions_granted: false,
            tabs: Vec::new(),
            panes: PaneManifest::default(),
            notification_state: HashMap::new(),
            original_tab_names: HashMap::new(),
            config: NotificationConfig::default(),
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
    /// Persists state to disk and updates tab names if any notifications were cleared.
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
                    original_tab_names: self.original_tab_names.clone(),
                };
                if let Err(e) = save_state(&persisted) {
                    eprintln!("zellij-attention: Failed to save state: {}", e);
                }

                // Update tab names
                self.update_tab_names();
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

        // Persist and update tab names if any notifications were removed
        if self.notification_state.len() != initial_count {
            let persisted = PersistedState {
                notifications: self.notification_state.clone(),
                original_tab_names: self.original_tab_names.clone(),
            };
            if let Err(e) = save_state(&persisted) {
                eprintln!("zellij-attention: Failed to save state: {}", e);
            }

            // Update tab names
            self.update_tab_names();
        }
    }

    /// Updates tab names to show notification icons or restore original names.
    /// Directly renames tabs via Zellij API instead of using zjstatus file communication.
    ///
    /// Note: Multiple plugin instances may exist. We re-read persisted state
    /// to get the latest truth before renaming, avoiding race conditions.
    fn update_tab_names(&mut self) {
        // Re-read persisted state for multi-instance coordination
        let persisted = load_state();
        self.notification_state = persisted.notifications;
        self.original_tab_names = persisted.original_tab_names;

        // Early return if disabled
        if !self.config.enabled {
            return;
        }

        // Track which tab positions currently have notifications
        // so we know which ones to restore
        let mut notified_positions: HashSet<usize> = HashSet::new();

        // Apply notification icons to tabs that need them
        for tab in &self.tabs {
            if let Some(notification) = self.get_tab_notification_state(tab.position) {
                notified_positions.insert(tab.position);

                // Cache original name if not already cached
                if !self.original_tab_names.contains_key(&tab.position) {
                    let original = if tab.name.is_empty() {
                        format!("Tab #{}", tab.position + 1)
                    } else {
                        tab.name.clone()
                    };
                    self.original_tab_names.insert(tab.position, original);
                }

                // Get the icon for this notification type
                let icon = match notification {
                    NotificationType::Waiting => &self.config.waiting_icon,
                    NotificationType::Completed => &self.config.completed_icon,
                };

                // Build new name: "icon original_name"
                let original = self.original_tab_names.get(&tab.position)
                    .cloned()
                    .unwrap_or_else(|| format!("Tab #{}", tab.position + 1));
                let new_name = format!("{} {}", icon, original);

                rename_tab(tab.position as u32, &new_name);
            }
        }

        // Restore original names for tabs that NO LONGER have notifications
        // (i.e., they were previously renamed but notification was cleared)
        let positions_to_restore: Vec<usize> = self.original_tab_names.keys()
            .filter(|pos| !notified_positions.contains(pos))
            .cloned()
            .collect();

        for pos in positions_to_restore {
            if let Some(original_name) = self.original_tab_names.remove(&pos) {
                // Only rename if tab still exists
                if self.tabs.iter().any(|t| t.position == pos) {
                    rename_tab(pos as u32, &original_name);
                }
            }
        }

        // Clean up cached names for tabs that no longer exist
        let valid_positions: HashSet<usize> = self.tabs.iter().map(|t| t.position).collect();
        self.original_tab_names.retain(|pos, _| valid_positions.contains(pos));

        // Persist updated state (includes original_tab_names)
        let persisted = PersistedState {
            notifications: self.notification_state.clone(),
            original_tab_names: self.original_tab_names.clone(),
        };
        if let Err(e) = save_state(&persisted) {
            eprintln!("zellij-attention: Failed to save state: {}", e);
        }
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        // Request permissions needed for tab/pane state and tab renaming
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::MessageAndLaunchOtherPlugins,
            PermissionType::ReadCliPipes,
        ]);

        // Subscribe to events (no Mouse needed anymore)
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            EventType::PaneUpdate,
        ]);

        // Load persisted state
        let persisted = load_state();
        self.notification_state = persisted.notifications;
        self.original_tab_names = persisted.original_tab_names;

        // Parse configuration
        self.config = NotificationConfig::from_configuration(&configuration);

        #[cfg(debug_assertions)]
        eprintln!("zellij-attention: config loaded: {:?}", self.config);

        eprintln!("zellij-attention: loaded\n");
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

                // Update tab names with initial state
                self.update_tab_names();
                true
            }
            Event::TabUpdate(tab_info) => {
                self.tabs = tab_info;
                self.check_and_clear_focus();

                // Tab list changed, update tab names
                self.update_tab_names();

                #[cfg(debug_assertions)]
                eprintln!("zellij-attention: TabUpdate - {} tabs", self.tabs.len());
                false // No need to render, we're invisible
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes = pane_manifest;
                // Note: cleanup_stale_panes() disabled - was causing notifications to
                // disappear during tab transitions when pane manifest temporarily
                // doesn't include all panes. Notifications clear on focus instead.
                // self.cleanup_stale_panes();
                self.check_and_clear_focus();

                // Always update tab names to reflect current state
                self.update_tab_names();

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

        // Try to parse message from name first, then fall back to payload
        // When using `zellij pipe "msg"`, zellij puts a UUID as name and msg in payload
        // When using `zellij pipe --name "msg"`, the msg is in name
        let message = if pipe_message.name.starts_with("zellij-attention::") {
            pipe_message.name.clone()
        } else if let Some(ref payload) = pipe_message.payload {
            if payload.starts_with("zellij-attention::") {
                payload.clone()
            } else {
                // Not for us
                return false;
            }
        } else {
            // Not for us
            return false;
        };

        // Parse broadcast pipe format: "zellij-attention::EVENT_TYPE::PANE_ID"
        let parts: Vec<&str> = message.split("::").collect();

        // Parse event_type and pane_id
        let (event_type, pane_id) = if parts.len() >= 3 {
            let event_type = parts[1].to_string();
            let pane_id: u32 = match parts[2].parse() {
                Ok(n) => n,
                Err(_) => {
                    eprintln!("zellij-attention: Invalid pane_id: {}\n", parts[2]);
                    unblock_cli_pipe_input(&pipe_message.name);
                    return false;
                }
            };
            (event_type, pane_id)
        } else {
            eprintln!("zellij-attention: Invalid format. Use: zellij-attention::EVENT_TYPE::PANE_ID\n");
            unblock_cli_pipe_input(&pipe_message.name);
            return false;
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
            original_tab_names: self.original_tab_names.clone(),
        };
        if let Err(e) = save_state(&persisted) {
            eprintln!("zellij-attention: Failed to save state: {}", e);
        }

        // Update tab names
        self.update_tab_names();

        // Unblock the CLI pipe so the command returns
        unblock_cli_pipe_input(&pipe_message.name);

        false // No need to render, we're invisible
    }
}

register_plugin!(State);
