pub mod config;
pub mod state;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;
use zellij_tile::shim::{rename_tab, unblock_cli_pipe_input};

use crate::config::NotificationConfig;
use crate::state::NotificationType;

#[derive(Default)]
pub struct State {
    permissions_granted: bool,
    pub(crate) tabs: Vec<TabInfo>,
    pub(crate) panes: PaneManifest,
    pub(crate) notification_state: HashMap<u32, HashSet<NotificationType>>,
    pub(crate) config: NotificationConfig,
    updating_tabs: bool,
    /// Tab positions where we've issued a rename to strip stale icons.
    /// Prevents re-stripping on the bounced TabUpdate before Zellij catches up.
    pub(crate) pending_strips: HashSet<usize>,
}

impl State {
    /// Checks if any focused pane (across all clients) has notifications and clears them.
    /// With multiple clients attached, each has its own active tab, so we must
    /// iterate ALL active tabs — not just the first one found.
    /// Returns true if any notification was cleared.
    pub(crate) fn check_and_clear_focus(&mut self) -> bool {
        let focused_panes: Vec<u32> = self.tabs.iter()
            .filter(|t| t.active)
            .filter_map(|active_tab| {
                let panes = self.panes.panes.get(&active_tab.position)?;
                panes.iter().find(|p| {
                    !p.is_plugin
                        && p.is_focused
                        && (p.is_floating == active_tab.are_floating_panes_visible)
                }).map(|p| p.id)
            })
            .collect();

        let mut cleared = false;
        for pane_id in focused_panes {
            if self.notification_state.remove(&pane_id).is_some() {
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: Cleared notifications for focused pane {}",
                    pane_id
                );
                cleared = true;
            }
        }
        cleared
    }

    /// Removes notification entries for pane IDs that no longer exist.
    /// Returns true if any stale entries were removed.
    pub(crate) fn clean_stale_notifications(&mut self) -> bool {
        if self.notification_state.is_empty() || self.panes.panes.is_empty() {
            return false;
        }

        let current_pane_ids: HashSet<u32> = self
            .panes
            .panes
            .values()
            .flat_map(|panes| panes.iter().filter(|p| !p.is_plugin).map(|p| p.id))
            .collect();

        let stale_ids: Vec<u32> = self
            .notification_state
            .keys()
            .filter(|id| !current_pane_ids.contains(id))
            .copied()
            .collect();

        if stale_ids.is_empty() {
            return false;
        }

        for id in &stale_ids {
            self.notification_state.remove(id);
            #[cfg(debug_assertions)]
            eprintln!(
                "zellij-attention: Removed stale notification for pane {}",
                id
            );
        }

        true
    }

    /// Returns true if any tab has a stale icon suffix with no active notification.
    pub(crate) fn has_stale_icons(&self) -> bool {
        for tab in &self.tabs {
            if self.get_tab_notification_state(tab.position).is_some() {
                continue;
            }
            if self.pending_strips.contains(&tab.position) {
                continue; // already issued a strip, waiting for Zellij to catch up
            }
            if self.tab_name_has_icon(&tab.name) {
                return true;
            }
        }
        false
    }

    /// Checks if a tab name ends with one of our notification icon suffixes.
    pub(crate) fn tab_name_has_icon(&self, name: &str) -> bool {
        let waiting_suffix = format!(" {}", self.config.waiting_icon);
        let completed_suffix = format!(" {}", self.config.completed_icon);
        name.ends_with(&waiting_suffix) || name.ends_with(&completed_suffix)
    }

    /// Strips notification icon suffixes from a tab name.
    pub(crate) fn strip_icons(&self, name: &str) -> String {
        let mut result = name.to_string();
        for icon in [&self.config.waiting_icon, &self.config.completed_icon] {
            let suffix = format!(" {}", icon);
            while result.ends_with(&suffix) {
                result.truncate(result.len() - suffix.len());
            }
        }
        result
    }

    pub(crate) fn get_tab_notification_state(&self, tab_position: usize) -> Option<NotificationType> {
        let panes = self.panes.panes.get(&tab_position)?;
        let mut has_completed = false;

        for pane in panes {
            // Skip plugin panes — their IDs overlap with terminal pane IDs
            if pane.is_plugin {
                continue;
            }
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

    /// Updates tab names to reflect notification state.
    /// Stateless: derives the desired name from the current tab name on every cycle.
    /// No cached names — immune to tab reordering and position shifts.
    fn update_tab_names(&mut self) {
        if self.updating_tabs || !self.config.enabled {
            return;
        }
        self.updating_tabs = true;

        for tab in &self.tabs {
            let base_name = if tab.name.is_empty() {
                format!("Tab #{}", tab.position + 1)
            } else {
                self.strip_icons(&tab.name)
            };

            if let Some(notification) = self.get_tab_notification_state(tab.position) {
                // Tab HAS a notification — ensure icon is present
                let icon = match notification {
                    NotificationType::Waiting => &self.config.waiting_icon,
                    NotificationType::Completed => &self.config.completed_icon,
                };
                let desired = format!("{} {}", base_name, icon);

                if tab.name != desired {
                    eprintln!(
                        "zellij-attention: RENAME tab pos={} '{}' -> '{}'",
                        tab.position, tab.name, desired
                    );
                    rename_tab((tab.position + 1) as u32, &desired);
                }
                self.pending_strips.remove(&tab.position);
            } else if tab.name != base_name && self.tab_name_has_icon(&tab.name) {
                // Tab has NO notification but has a stale icon — strip it
                if self.pending_strips.contains(&tab.position) {
                    // Already issued a strip, waiting for Zellij to catch up
                    continue;
                }
                eprintln!(
                    "zellij-attention: Stripping stale icon from tab pos={} '{}' -> '{}'",
                    tab.position, tab.name, base_name
                );
                self.pending_strips.insert(tab.position);
                rename_tab((tab.position + 1) as u32, &base_name);
            } else {
                // Name is clean (or user renamed to something without our icons)
                self.pending_strips.remove(&tab.position);
            }
        }

        // Clean up pending_strips for tab positions that no longer exist
        if !self.tabs.is_empty() {
            let valid_positions: HashSet<usize> = self.tabs.iter().map(|t| t.position).collect();
            self.pending_strips.retain(|pos| valid_positions.contains(pos));
        }

        self.updating_tabs = false;
    }
}

impl ZellijPlugin for State {
    fn load(&mut self, configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
            PermissionType::MessageAndLaunchOtherPlugins,
            PermissionType::ReadCliPipes,
        ]);

        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            EventType::PaneUpdate,
        ]);

        self.config = NotificationConfig::from_configuration(&configuration);

        eprintln!("zellij-attention: v{} loaded\n", env!("CARGO_PKG_VERSION"));
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(status) => {
                self.permissions_granted = status == PermissionStatus::Granted;
                set_selectable(false);

                // Strip any stale icons on startup
                self.update_tab_names();
                true
            }
            Event::TabUpdate(tab_info) => {
                // Detect external renames (base name changed by something other than us)
                for new_tab in &tab_info {
                    if let Some(old_tab) = self.tabs.iter().find(|t| t.position == new_tab.position) {
                        if old_tab.name != new_tab.name {
                            let old_base = self.strip_icons(&old_tab.name);
                            let new_base = self.strip_icons(&new_tab.name);
                            if old_base != new_base {
                                eprintln!(
                                    "zellij-attention: EXTERNAL rename at pos={} '{}' -> '{}' (base: '{}' -> '{}')",
                                    new_tab.position, old_tab.name, new_tab.name, old_base, new_base
                                );
                            }
                        }
                    }
                }
                self.tabs = tab_info;
                let focus_cleared = self.check_and_clear_focus();
                let stale_cleaned = self.clean_stale_notifications();
                if focus_cleared || stale_cleaned || self.has_stale_icons()
                {
                    self.update_tab_names();
                }
                false
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes = pane_manifest;
                let focus_cleared = self.check_and_clear_focus();
                let stale_cleaned = self.clean_stale_notifications();
                if focus_cleared || stale_cleaned || self.has_stale_icons()
                {
                    self.update_tab_names();
                }
                false
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, _cols: usize) {}

    fn pipe(&mut self, pipe_message: PipeMessage) -> bool {
        #[cfg(debug_assertions)]
        eprintln!(
            "zellij-attention: pipe name={} payload={:?}\n",
            pipe_message.name, pipe_message.payload
        );

        let message = if pipe_message.name.starts_with("zellij-attention::") {
            pipe_message.name.clone()
        } else if let Some(ref payload) = pipe_message.payload {
            if payload.starts_with("zellij-attention::") {
                payload.clone()
            } else {
                return false;
            }
        } else {
            return false;
        };

        let parts: Vec<&str> = message.split("::").collect();

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

        let notification_type = match event_type.to_lowercase().as_str() {
            "waiting" => NotificationType::Waiting,
            "completed" => NotificationType::Completed,
            unknown => {
                eprintln!("zellij-attention: Unknown event type: {}\n", unknown);
                unblock_cli_pipe_input(&pipe_message.name);
                return false;
            }
        };

        // Unblock the CLI pipe immediately so the caller never hangs,
        // regardless of what happens during state mutation or tab renaming.
        unblock_cli_pipe_input(&pipe_message.name);

        let mut notifications = HashSet::new();
        notifications.insert(notification_type);
        self.notification_state.insert(pane_id, notifications);

        #[cfg(debug_assertions)]
        eprintln!("zellij-attention: Set pane {} to {:?}\n", pane_id, notification_type);

        #[cfg(debug_assertions)]
        {
            for tab in &self.tabs {
                if let Some(panes) = self.panes.panes.get(&tab.position) {
                    let terminal_panes: Vec<String> = panes.iter()
                        .filter(|p| !p.is_plugin)
                        .map(|p| format!("{}", p.id))
                        .collect();
                    let plugin_panes: Vec<String> = panes.iter()
                        .filter(|p| p.is_plugin)
                        .map(|p| format!("{}", p.id))
                        .collect();
                    eprintln!(
                        "zellij-attention: tab pos={} name='{}' terminal_panes=[{}] plugin_panes=[{}]",
                        tab.position, tab.name,
                        terminal_panes.join(","), plugin_panes.join(",")
                    );
                }
            }
        }

        self.update_tab_names();

        false
    }
}
