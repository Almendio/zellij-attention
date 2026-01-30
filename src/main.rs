mod state;

use std::collections::{BTreeMap, HashMap, HashSet};
use zellij_tile::prelude::*;

use crate::state::{load_state, NotificationType};

#[derive(Default)]
struct State {
    permissions_granted: bool,
    tabs: Vec<TabInfo>,
    panes: PaneManifest,
    notification_state: HashMap<u32, HashSet<NotificationType>>,
}

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        // Request permissions needed for future functionality
        request_permission(&[
            PermissionType::ReadApplicationState,
            PermissionType::ChangeApplicationState,
        ]);

        // Subscribe to events
        subscribe(&[
            EventType::PermissionRequestResult,
            EventType::TabUpdate,
            EventType::PaneUpdate,
        ]);

        // Load persisted state
        self.notification_state = load_state().notifications;
    }

    fn update(&mut self, event: Event) -> bool {
        #[cfg(debug_assertions)]
        eprintln!("zellij-attention: Received event: {:?}", event);

        match event {
            Event::PermissionRequestResult(status) => {
                self.permissions_granted = status == PermissionStatus::Granted;
                true // Re-render to show updated status
            }
            Event::TabUpdate(tab_info) => {
                self.tabs = tab_info;
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: TabUpdate - {} tabs, active: {:?}",
                    self.tabs.len(),
                    self.tabs.iter().find(|t| t.active).map(|t| &t.name)
                );
                true // Will trigger render
            }
            Event::PaneUpdate(pane_manifest) => {
                self.panes = pane_manifest;
                #[cfg(debug_assertions)]
                eprintln!(
                    "zellij-attention: PaneUpdate - {} tabs with panes",
                    self.panes.panes.len()
                );
                true // Will trigger render
            }
            _ => false,
        }
    }

    fn render(&mut self, _rows: usize, _cols: usize) {
        if self.permissions_granted {
            println!(
                "zellij-attention: {} tabs, {} pane groups, {} notifications",
                self.tabs.len(),
                self.panes.panes.len(),
                self.notification_state.len()
            );
        } else {
            println!("zellij-attention: Waiting for permissions...");
        }
    }
}

register_plugin!(State);
