use super::*;
use std::time::{Duration, Instant};

const EDITOR_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const EDITOR_REQUEST_GAP: Duration = Duration::from_millis(300);
pub(super) const NEOVIM_ICON: &str = "";

pub(super) fn workspace_icon_color(
    status: crate::api::schema::AgentStatus,
    palette: &Palette,
) -> ratatui::style::Color {
    match status {
        crate::api::schema::AgentStatus::Blocked | crate::api::schema::AgentStatus::Done => {
            palette.text
        }
        crate::api::schema::AgentStatus::Working
        | crate::api::schema::AgentStatus::Idle
        | crate::api::schema::AgentStatus::Unknown => palette.overlay0,
    }
}

pub(super) fn workspace_icon<'a>(
    workspace: &'a ClientShellWorkspace,
    snapshot: &ClientShellSnapshot,
    endpoint_id: &ClientEndpointId,
    editor_source: Option<&(ClientEndpointId, String)>,
    editor_checks: &HashMap<String, WorkspaceEditorCheck>,
) -> Option<&'a str> {
    if editor_source
        .is_some_and(|(source, boot_id)| source == endpoint_id && boot_id == &snapshot.boot_id)
        && editor_checks
            .get(&workspace.workspace_id)
            .is_some_and(|check| {
                check.is_neovim
                    && snapshot
                        .panes
                        .iter()
                        .filter(|pane| pane.workspace_id == workspace.workspace_id)
                        .map(|pane| pane.pane_id.as_str())
                        .eq(std::iter::once(check.pane_id.as_str()))
            })
    {
        return Some(NEOVIM_ICON);
    }
    super::sidebar::first_workspace_emoji(&workspace.label)
}

fn is_neovim_process(process: &crate::api::schema::PaneProcessInfoProcess) -> bool {
    [process.argv0.as_deref(), Some(process.name.as_str())]
        .into_iter()
        .flatten()
        .any(|name| {
            let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
            name.eq_ignore_ascii_case("nvim") || name.eq_ignore_ascii_case("nvim.exe")
        })
}

impl ClientShellState {
    pub(crate) fn tick_workspace_editors(&mut self, now: Instant) -> Vec<ClientShellAction> {
        if !self.sidebar_collapsed
            || self.config.sidebar_collapsed_mode == SidebarCollapsedModeConfig::Hidden
            || self
                .last_composed_size
                .is_some_and(|(cols, _)| cols <= self.config.mobile_width_threshold)
        {
            return Vec::new();
        }
        let Some(snapshot) = self.snapshot.as_deref() else {
            return Vec::new();
        };
        let source = (self.active_endpoint_id.clone(), snapshot.boot_id.clone());
        if self.editor_source.as_ref() != Some(&source) {
            self.editor_source = Some(source);
            self.editor_checks.clear();
            self.last_editor_request = None;
        }
        if self
            .last_editor_request
            .is_some_and(|last| now.saturating_duration_since(last) < EDITOR_REQUEST_GAP)
            || self.pending_requests.values().any(|pending| {
                matches!(
                    pending.kind,
                    PendingEndpointKind::WorkspaceEditorCheck { .. }
                )
            })
        {
            return Vec::new();
        }
        let method_name = "pane.process_info";
        for workspace in &snapshot.workspaces {
            let mut panes = snapshot
                .panes
                .iter()
                .filter(|pane| pane.workspace_id == workspace.workspace_id);
            let Some(pane) = panes.next() else {
                continue;
            };
            if panes.next().is_some() {
                continue;
            }
            if self
                .editor_checks
                .get(&workspace.workspace_id)
                .is_some_and(|check| {
                    check.pane_id == pane.pane_id
                        && now.saturating_duration_since(check.checked_at) < EDITOR_CHECK_INTERVAL
                })
            {
                continue;
            }
            let method = crate::api::schema::Method::PaneProcessInfo(
                crate::api::schema::PaneProcessInfoParams {
                    pane_id: Some(pane.pane_id.clone()),
                },
            );
            if !self.supports_endpoint_method(&method) {
                return Vec::new();
            }
            let check = self
                .editor_checks
                .entry(workspace.workspace_id.clone())
                .or_insert_with(|| WorkspaceEditorCheck {
                    pane_id: pane.pane_id.clone(),
                    checked_at: now,
                    is_neovim: false,
                });
            if check.pane_id != pane.pane_id {
                check.pane_id.clone_from(&pane.pane_id);
                check.is_neovim = false;
            }
            check.checked_at = now;
            self.last_editor_request = Some(now);
            let request_id = format!("client-shell:{}", self.next_request_id);
            self.next_request_id = self.next_request_id.saturating_add(1);
            self.pending_requests.insert(
                request_id.clone(),
                PendingEndpointRequest {
                    boot_id: snapshot.boot_id.clone(),
                    method_name: method_name.into(),
                    confirmation_workspace_id: None,
                    kind: PendingEndpointKind::WorkspaceEditorCheck {
                        workspace_id: workspace.workspace_id.clone(),
                        pane_id: pane.pane_id.clone(),
                    },
                },
            );
            return vec![ClientShellAction::Endpoint {
                endpoint_id: self.active_endpoint_id.clone(),
                boot_id: snapshot.boot_id.clone(),
                request: Box::new(crate::api::schema::Request {
                    id: request_id,
                    method,
                }),
            }];
        }
        Vec::new()
    }

    pub(super) fn complete_workspace_editor_check(
        &mut self,
        workspace_id: &str,
        pane_id: &str,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
    ) -> (bool, Vec<ClientShellAction>) {
        let is_neovim = matches!(result, Ok(crate::api::schema::ResponseResult::PaneProcessInfo { process_info })
            if process_info.pane_id == pane_id
                && process_info.foreground_processes.iter().any(is_neovim_process));
        let Some(check) = self.editor_checks.get_mut(workspace_id) else {
            return (false, Vec::new());
        };
        if check.pane_id != pane_id {
            return (false, Vec::new());
        }
        let changed = check.is_neovim != is_neovim;
        check.is_neovim = is_neovim;
        (changed && self.sidebar_collapsed, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::is_neovim_process;
    use crate::api::schema::PaneProcessInfoProcess;

    #[test]
    fn neovim_process_matches_executable_name_not_a_filename() {
        let mut process = PaneProcessInfoProcess {
            pid: 1,
            name: "nvim".into(),
            argv0: Some("C:\\Program Files\\Neovim\\bin\\nvim.exe".into()),
            argv: None,
            cmdline: None,
            cwd: None,
        };
        assert!(is_neovim_process(&process));
        process.name = "shell".into();
        process.argv0 = Some("/tmp/nvim-notes.txt".into());
        assert!(!is_neovim_process(&process));
    }
}
