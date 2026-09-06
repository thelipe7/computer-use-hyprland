use crate::windowing::backends::hyprland;
use crate::windowing::types::{
    WindowInfo, WindowOcclusion, WindowWorkspaceMove, WorkspaceChange, WorkspaceTarget,
};
use anyhow::{Result, anyhow};

pub use hyprland::{HYPRLAND_BACKEND, HyprlandRelease, LaunchRules, MINIMUM_HYPRLAND_RELEASE};

/// The running Hyprland release, or `None` when it cannot be read.
pub fn hyprland_release() -> Option<HyprlandRelease> {
    hyprland::release()
}

pub const WINDOW_PERMISSION_HINT: &str = "Computer Use could not reach Hyprland. Targeted window input needs hyprctl on PATH and a running Hyprland instance whose socket this process can read.";

#[expect(
    clippy::struct_excessive_bools,
    reason = "a probe answers three independent capability questions, and the readiness report copies each one out by name"
)]
#[derive(Debug, Clone)]
pub struct BackendProbe {
    pub id: &'static str,
    pub ok: bool,
    pub can_list_windows: bool,
    pub can_focus_apps: bool,
    pub can_focus_windows: bool,
    pub detail: String,
}

pub const LIST_NOTE: &str = "Window list came from Hyprland hyprctl. Terminal windows may include best-effort PTY and active-process context when the process tree is readable.";

pub async fn list_windows() -> Result<Vec<WindowInfo>> {
    match hyprland::list_windows().await {
        Ok(windows) if windows.is_empty() => Err(anyhow!("Hyprland returned no windows")),
        Ok(windows) => Ok(windows),
        Err(error) => Err(anyhow!("Hyprland failed: {error:#}")),
    }
}

pub async fn activate_window(window: &WindowInfo) -> Result<()> {
    hyprland::activate_window(window.window_id).await
}

pub async fn focused_window() -> Result<Option<WindowInfo>> {
    Ok(hyprland::list_windows()
        .await?
        .into_iter()
        .find(|window| window.focused))
}

/// Move the view to another workspace, reporting the one it left.
pub async fn focus_workspace(target: WorkspaceTarget) -> Result<WorkspaceChange> {
    hyprland::focus_workspace(target).await
}

/// Spawn a program so its first window opens under `rules`, answering with
/// the command line the compositor was given.
pub async fn launch(program: &str, args: &[String], rules: LaunchRules) -> Result<String> {
    hyprland::launch(program, args, rules).await
}

/// Move one window to another workspace, taking the view with it when
/// `follow` is set.
pub async fn move_window_to_workspace(
    window: &WindowInfo,
    target: WorkspaceTarget,
    follow: bool,
) -> Result<WindowWorkspaceMove> {
    hyprland::move_window_to_workspace(window.window_id, target, follow).await
}

pub async fn move_window(window: &WindowInfo, x: i32, y: i32) -> Result<String> {
    hyprland::move_window(window.window_id, x, y).await
}

pub async fn resize_window(window: &WindowInfo, width: i32, height: i32) -> Result<String> {
    hyprland::resize_window(window.window_id, width, height).await
}

/// Float or tile a window, so a caller can act on the tiled-window refusal
/// `move_window` and `resize_window` answer with.
pub async fn set_window_floating(window: &WindowInfo, floating: bool) -> Result<String> {
    hyprland::set_floating(window.window_id, floating).await
}

/// Windows overlapping `window` from above, for a capture that does not
/// raise it.
pub async fn occluding_windows(window: &WindowInfo) -> Vec<WindowOcclusion> {
    hyprland::occluding_windows(window.window_id)
        .await
        .unwrap_or_default()
}

/// The pointer's desktop coordinates and the backend that answered, or
/// `None` when Hyprland is not the active session.
pub async fn pointer_position() -> Result<Option<((i32, i32), &'static str)>> {
    if !hyprland::is_active() {
        return Ok(None);
    }
    hyprland::cursor_position()
        .await
        .map(|point| Some((point, HYPRLAND_BACKEND)))
}

pub fn probe_backend() -> BackendProbe {
    hyprland::probe()
}
