use crate::terminal::TerminalWindowContext;
use anyhow::{Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WindowInfo {
    pub window_id: u64,
    pub title: Option<String>,
    pub app_id: Option<String>,
    pub wm_class: Option<String>,
    pub pid: Option<u32>,
    pub bounds: Option<WindowBounds>,
    pub workspace: Option<i32>,
    pub focused: bool,
    pub hidden: bool,
    pub client_type: Option<String>,
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalWindowContext>,
}

/// A window that overlaps a screenshot target and sits above it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub struct WindowOcclusion {
    pub window_id: u64,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WindowBounds {
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
pub struct WindowTarget {
    #[serde(default)]
    pub window_id: Option<u64>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub tty: Option<String>,
    #[serde(default)]
    pub terminal_pid: Option<u32>,
    #[serde(default)]
    pub terminal_command: Option<String>,
    #[serde(default)]
    pub terminal_cwd: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub wm_class: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowFocusResult {
    pub requested_window: WindowInfo,
    pub focused_window: Option<WindowInfo>,
    pub exact_window_focused: bool,
    pub app_focused: bool,
    pub backend: String,
    pub note: String,
}

impl WindowTarget {
    pub fn has_target(&self) -> bool {
        self.window_id.is_some()
            || self.pid.is_some()
            || self.has_terminal_target()
            || self
                .app_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .wm_class
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .title
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    pub fn requires_exact_focus(&self) -> bool {
        self.window_id.is_some()
            || self.pid.is_some()
            || self.has_terminal_target()
            || self
                .title
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }

    pub(crate) fn has_terminal_target(&self) -> bool {
        self.terminal_pid.is_some()
            || self
                .tty
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .terminal_command
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            || self
                .terminal_cwd
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
    }
}

/// A workspace a caller can name: an id, or the first one with nothing on it.
///
/// Hyprland's own selector grammar is much wider -- relative offsets, names,
/// special workspaces -- and none of the rest is accepted here, because a
/// tool's description has to say what it takes and this build only exercises
/// these two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkspaceTarget {
    /// A workspace by id. Hyprland creates it when it holds no windows yet.
    Id(i32),
    /// The first workspace with no windows on it, which Hyprland picks: ids
    /// run to 2,147,483,647 and a workspace exists only while something is on
    /// it, so there is always a next free one.
    FirstEmpty,
}

impl WorkspaceTarget {
    /// Read the `workspace` a caller passed: a positive id, or `empty`.
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("empty") {
            return Ok(Self::FirstEmpty);
        }
        match value.parse::<i32>() {
            Ok(id) if id > 0 => Ok(Self::Id(id)),
            _ => bail!(
                "Unsupported workspace {value:?}. Pass a workspace id (a positive number, as list_windows reports in each window's workspace) or \"empty\" for the first workspace with nothing on it."
            ),
        }
    }

    /// The value this target takes in a dispatcher's `workspace` field: a Lua
    /// number for an id, a quoted selector for the empty one.
    pub(crate) fn lua_value(self) -> String {
        match self {
            Self::Id(id) => id.to_string(),
            Self::FirstEmpty => "\"empty\"".to_string(),
        }
    }

    /// Whether a workspace the compositor reported is the one that was asked
    /// for. An id is checked against itself; the first empty one is only
    /// known by having left the workspace the caller was on.
    pub(crate) fn reached(self, workspace: i32, from: Option<i32>) -> bool {
        match self {
            Self::Id(id) => workspace == id,
            Self::FirstEmpty => from != Some(workspace),
        }
    }

    pub fn describe(self) -> String {
        match self {
            Self::Id(id) => format!("workspace {id}"),
            Self::FirstEmpty => "the first empty workspace".to_string(),
        }
    }
}

/// A workspace as `hyprctl activeworkspace -j` reports it.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct WorkspaceSummary {
    pub id: i32,
    pub name: String,
    /// How many windows are on it, which is what makes an empty one empty.
    pub windows: u32,
}

/// The workspace the view was on, and the one it is on now.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkspaceChange {
    pub previous: WorkspaceSummary,
    pub current: WorkspaceSummary,
}

#[cfg(test)]
mod tests {
    use super::WorkspaceTarget;

    #[test]
    fn a_workspace_is_named_by_id_or_by_being_empty() {
        assert_eq!(WorkspaceTarget::parse("5").unwrap(), WorkspaceTarget::Id(5));
        assert_eq!(
            WorkspaceTarget::parse(" 12 ").unwrap(),
            WorkspaceTarget::Id(12)
        );
        assert_eq!(
            WorkspaceTarget::parse("empty").unwrap(),
            WorkspaceTarget::FirstEmpty
        );
        assert_eq!(
            WorkspaceTarget::parse("EMPTY").unwrap(),
            WorkspaceTarget::FirstEmpty
        );
    }

    #[test]
    fn the_rest_of_hyprlands_selector_grammar_is_refused_by_name() {
        // Accepting a selector this build never exercises would put it in a
        // tool description that cannot answer for it.
        for value in [
            "",
            "0",
            "-1",
            "previous",
            "e+1",
            "name:coding",
            "special:magic",
        ] {
            let error = WorkspaceTarget::parse(value)
                .expect_err("only an id or empty is supported")
                .to_string();
            assert!(error.contains("workspace id"), "{value}: {error}");
            assert!(error.contains("empty"), "{value}: {error}");
        }
    }

    #[test]
    fn a_dispatched_workspace_is_a_number_or_the_empty_selector() {
        assert_eq!(WorkspaceTarget::Id(7).lua_value(), "7");
        assert_eq!(WorkspaceTarget::FirstEmpty.lua_value(), "\"empty\"");
    }

    #[test]
    fn an_id_target_is_reached_by_its_id_and_an_empty_one_by_moving() {
        assert!(WorkspaceTarget::Id(5).reached(5, Some(4)));
        assert!(!WorkspaceTarget::Id(5).reached(6, Some(4)));
        // The compositor picks the empty one, so the only proof is that the
        // view is no longer where it started.
        assert!(WorkspaceTarget::FirstEmpty.reached(6, Some(4)));
        assert!(!WorkspaceTarget::FirstEmpty.reached(4, Some(4)));
    }
}
