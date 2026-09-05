use crate::windowing::backends::hyprland;
use crate::windowing::types::{WindowInfo, WindowOcclusion};
use anyhow::{Result, anyhow};

pub use hyprland::HYPRLAND_BACKEND;

pub const WINDOW_PERMISSION_HINT: &str = "Computer Use could not reach Hyprland. Targeted window input needs hyprctl on PATH and a running Hyprland instance whose socket this process can read.";

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

pub fn backend_can_exact_focus(id: &str) -> bool {
    id == HYPRLAND_BACKEND
}

pub async fn list_windows() -> Result<Vec<WindowInfo>> {
    match hyprland::list_windows().await {
        Ok(windows) if windows.is_empty() => Err(anyhow!("Hyprland returned no windows")),
        Ok(windows) => Ok(windows),
        Err(error) => Err(anyhow!("Hyprland failed: {error:#}")),
    }
}

pub async fn activate_window(window: &WindowInfo) -> Result<()> {
    require_hyprland(window, "activation")?;
    hyprland::activate_window(window.window_id).await
}

pub async fn focused_window_for_backend(backend: &str) -> Result<Option<WindowInfo>> {
    if backend != HYPRLAND_BACKEND {
        return Err(anyhow!(
            "Unsupported window backend for focus query: {backend}"
        ));
    }
    Ok(hyprland::list_windows()
        .await?
        .into_iter()
        .find(|window| window.focused))
}

pub async fn move_window(window: &WindowInfo, x: i32, y: i32) -> Result<String> {
    require_hyprland(window, "move windows")?;
    hyprland::move_window(window.window_id, x, y).await
}

pub async fn resize_window(window: &WindowInfo, width: i32, height: i32) -> Result<String> {
    require_hyprland(window, "resize windows")?;
    hyprland::resize_window(window.window_id, width, height).await
}

/// Float or tile a window, so a caller can act on the tiled-window refusal
/// `move_window` and `resize_window` answer with.
pub async fn set_window_floating(window: &WindowInfo, floating: bool) -> Result<String> {
    require_hyprland(window, "change a window's floating state")?;
    hyprland::set_floating(window.window_id, floating).await
}

/// Windows overlapping `window` from above, for a capture that does not
/// raise it. Empty when the window is not a Hyprland one.
pub async fn occluding_windows(window: &WindowInfo) -> Vec<WindowOcclusion> {
    if window.backend != HYPRLAND_BACKEND {
        return Vec::new();
    }
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

pub fn probe_backends() -> Vec<BackendProbe> {
    vec![hyprland::probe()]
}

fn require_hyprland(window: &WindowInfo, operation: &str) -> Result<()> {
    if window.backend == HYPRLAND_BACKEND {
        return Ok(());
    }
    Err(anyhow!(
        "Window backend {} cannot {operation}; this build only drives Hyprland.",
        window.backend
    ))
}
