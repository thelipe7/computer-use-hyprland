use crate::command_runner;
use crate::terminal::enrich_terminal_windows;
use crate::windowing::registry::BackendProbe;
use crate::windowing::types::{
    WindowBounds, WindowInfo, WindowOcclusion, WorkspaceChange, WorkspaceSummary, WorkspaceTarget,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::time::{Duration, SystemTime};
use tokio::process::Command;
use tokio::time::sleep;

pub const HYPRLAND_BACKEND: &str = "hyprland";

const GEOMETRY_VERIFY_ATTEMPTS: usize = 11;
const GEOMETRY_VERIFY_DELAY: Duration = Duration::from_millis(50);

pub fn probe() -> BackendProbe {
    match hyprctl_output(&["clients", "-j"]) {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let ok = matches!(
                serde_json::from_str::<serde_json::Value>(&stdout),
                Ok(serde_json::Value::Array(_))
            );
            BackendProbe {
                id: HYPRLAND_BACKEND,
                ok,
                can_list_windows: ok,
                can_focus_apps: ok,
                can_focus_windows: ok,
                detail: if ok {
                    "hyprctl clients -j returned a JSON array".to_string()
                } else {
                    "hyprctl clients -j did not return a JSON array".to_string()
                },
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            BackendProbe {
                id: HYPRLAND_BACKEND,
                ok: false,
                can_list_windows: false,
                can_focus_apps: false,
                can_focus_windows: false,
                detail: if stderr.is_empty() { stdout } else { stderr },
            }
        }
        Err(error) => BackendProbe {
            id: HYPRLAND_BACKEND,
            ok: false,
            can_list_windows: false,
            can_focus_apps: false,
            can_focus_windows: false,
            detail: error.to_string(),
        },
    }
}

pub async fn list_windows() -> Result<Vec<WindowInfo>> {
    let output = hyprctl_output_async(&["clients", "-j"])
        .await
        .context("failed to run hyprctl clients -j")?;
    if !output.status.success() {
        bail!(
            "hyprctl clients -j failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let clients_json = String::from_utf8_lossy(&output.stdout);
    let monitors_output = hyprctl_output_async(&["monitors", "-j"]).await.ok();
    match monitors_output.filter(|output| output.status.success()) {
        Some(monitors) => parse_hyprland_clients_with_monitors(&clients_json, &monitors.stdout),
        None => parse_hyprland_clients_without_bounds(&clients_json),
    }
}

fn parse_hyprland_clients_with_monitors(
    clients_json: &str,
    monitors_json: &[u8],
) -> Result<Vec<WindowInfo>> {
    let mut clients: Vec<HyprlandClient> =
        serde_json::from_str(clients_json).context("failed to parse hyprctl clients -j output")?;
    let Ok(monitors) = serde_json::from_slice::<Vec<HyprlandMonitor>>(monitors_json) else {
        clear_hyprland_client_bounds(&mut clients);
        return windows_from_hyprland_clients(clients);
    };
    let Some(layout) = HyprlandCaptureLayout::from_monitors(&monitors) else {
        clear_hyprland_client_bounds(&mut clients);
        return windows_from_hyprland_clients(clients);
    };
    let monitor_ids = monitors
        .iter()
        .map(|monitor| monitor.id)
        .collect::<std::collections::HashSet<_>>();
    for client in &mut clients {
        if !client
            .monitor
            .is_some_and(|monitor_id| monitor_ids.contains(&monitor_id))
        {
            client.at = None;
            client.size = None;
            continue;
        }
        let Some((at, size)) = client
            .at
            .zip(client.size)
            .and_then(|(at, size)| layout.map_bounds(at, size))
        else {
            client.at = None;
            client.size = None;
            continue;
        };
        client.at = Some(at);
        client.size = Some(size);
    }
    windows_from_hyprland_clients(clients)
}

#[cfg(test)]
pub(crate) fn parse_hyprland_clients(json: &str) -> Result<Vec<WindowInfo>> {
    let clients: Vec<HyprlandClient> =
        serde_json::from_str(json).context("failed to parse hyprctl clients -j output")?;
    windows_from_hyprland_clients(clients)
}

fn parse_hyprland_clients_without_bounds(json: &str) -> Result<Vec<WindowInfo>> {
    let mut clients: Vec<HyprlandClient> =
        serde_json::from_str(json).context("failed to parse hyprctl clients -j output")?;
    clear_hyprland_client_bounds(&mut clients);
    windows_from_hyprland_clients(clients)
}

fn clear_hyprland_client_bounds(clients: &mut [HyprlandClient]) {
    for client in clients {
        client.at = None;
        client.size = None;
    }
}

#[derive(Debug, Clone, Copy)]
struct HyprlandCaptureLayout {
    origin_x: i32,
    origin_y: i32,
    scale: f64,
}

impl HyprlandCaptureLayout {
    fn from_monitors(monitors: &[HyprlandMonitor]) -> Option<Self> {
        let first = monitors.first()?;
        if monitors
            .iter()
            .any(|monitor| !monitor.scale.is_finite() || monitor.scale <= 0.0)
        {
            return None;
        }
        Some(Self {
            origin_x: monitors
                .iter()
                .map(|monitor| monitor.x)
                .min()
                .unwrap_or(first.x),
            origin_y: monitors
                .iter()
                .map(|monitor| monitor.y)
                .min()
                .unwrap_or(first.y),
            scale: monitors
                .iter()
                .map(|monitor| monitor.scale)
                .fold(first.scale, f64::max),
        })
    }

    fn map_bounds(&self, at: [i32; 2], size: [u32; 2]) -> Option<([i32; 2], [u32; 2])> {
        if size[0] == 0 || size[1] == 0 {
            return None;
        }
        let left = ((i64::from(at[0]) - i64::from(self.origin_x)) as f64 * self.scale).floor();
        let top = ((i64::from(at[1]) - i64::from(self.origin_y)) as f64 * self.scale).floor();
        let right = ((i64::from(at[0]) + i64::from(size[0]) - i64::from(self.origin_x)) as f64
            * self.scale)
            .ceil();
        let bottom = ((i64::from(at[1]) + i64::from(size[1]) - i64::from(self.origin_y)) as f64
            * self.scale)
            .ceil();
        if !left.is_finite()
            || !top.is_finite()
            || !right.is_finite()
            || !bottom.is_finite()
            || left < f64::from(i32::MIN)
            || left > f64::from(i32::MAX)
            || top < f64::from(i32::MIN)
            || top > f64::from(i32::MAX)
            || right <= left
            || bottom <= top
            || right - left > f64::from(u32::MAX)
            || bottom - top > f64::from(u32::MAX)
        {
            return None;
        }
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the block above returns None for every value outside the i32 and u32 ranges these produce"
        )]
        Some((
            [left as i32, top as i32],
            [(right - left) as u32, (bottom - top) as u32],
        ))
    }

    /// [`Self::map_bounds`] for a single point: Hyprland's global layout
    /// coordinates to desktop (screenshot-space) coordinates.
    fn map_point(&self, point: [i32; 2]) -> (i32, i32) {
        (
            self.map_axis(point[0], self.origin_x),
            self.map_axis(point[1], self.origin_y),
        )
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to the i32 range on the line it is cast from"
    )]
    fn map_axis(&self, value: i32, origin: i32) -> i32 {
        let scaled = ((i64::from(value) - i64::from(origin)) as f64 * self.scale).round();
        scaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }

    /// Inverse of [`Self::map_bounds`] for a point: desktop (screenshot-space)
    /// coordinates back to Hyprland's global layout coordinates.
    fn unmap_point(&self, point: [i32; 2]) -> [i32; 2] {
        [
            self.unmap_axis(point[0], self.origin_x),
            self.unmap_axis(point[1], self.origin_y),
        ]
    }

    /// Inverse of [`Self::map_bounds`] for a size: desktop pixels back to
    /// Hyprland's global layout units.
    fn unmap_size(&self, size: [i32; 2]) -> [i32; 2] {
        [self.unmap_axis(size[0], 0), self.unmap_axis(size[1], 0)]
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "clamped to the i32 range on the line it is cast from"
    )]
    fn unmap_axis(&self, value: i32, origin: i32) -> i32 {
        let unscaled = (f64::from(value) / self.scale).round();
        let unscaled = unscaled.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
        unscaled.saturating_add(origin)
    }
}

async fn capture_layout() -> Option<HyprlandCaptureLayout> {
    let output = hyprctl_output_async(&["monitors", "-j"]).await.ok()?;
    if !output.status.success() {
        return None;
    }
    let monitors = serde_json::from_slice::<Vec<HyprlandMonitor>>(&output.stdout).ok()?;
    HyprlandCaptureLayout::from_monitors(&monitors)
}

fn windows_from_hyprland_clients(clients: Vec<HyprlandClient>) -> Result<Vec<WindowInfo>> {
    let mut windows = clients
        .into_iter()
        .filter(|client| client.mapped.unwrap_or(true))
        .map(WindowInfo::try_from)
        .collect::<Result<Vec<_>>>()?;
    windows.sort_by_key(|window| window.window_id);
    enrich_terminal_windows(&mut windows);
    Ok(windows)
}

pub async fn activate_window(window_id: u64) -> Result<()> {
    let address = format!("address:0x{window_id:x}");
    run_lua_dispatch(&lua_focus_dispatch(&address))
        .await
        .with_context(|| format!("Hyprland window focus failed for {address}"))
}

/// The workspace the view is on right now.
pub async fn active_workspace() -> Result<WorkspaceSummary> {
    let output = hyprctl_output_async(&["activeworkspace", "-j"])
        .await
        .context("failed to run hyprctl activeworkspace -j")?;
    if !output.status.success() {
        bail!(
            "hyprctl activeworkspace -j failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl activeworkspace -j output")
}

/// Move the view to another workspace, reporting where it was so a caller can
/// put it back.
pub async fn focus_workspace(target: WorkspaceTarget) -> Result<WorkspaceChange> {
    let previous = active_workspace().await?;
    run_lua_dispatch(&lua_workspace_focus_dispatch(target)).await?;
    let current = wait_for_active_workspace(|workspace| {
        target.reached(workspace.id, Some(previous.id))
            || (target == WorkspaceTarget::FirstEmpty && workspace.windows == 0)
    })
    .await?;
    Ok(WorkspaceChange { previous, current })
}

fn lua_workspace_focus_dispatch(target: WorkspaceTarget) -> String {
    format!("hl.dsp.focus({{ workspace = {} }})", target.lua_value())
}

async fn wait_for_active_workspace(
    reached: impl Fn(&WorkspaceSummary) -> bool,
) -> Result<WorkspaceSummary> {
    let mut last = None;
    for attempt in 0..GEOMETRY_VERIFY_ATTEMPTS {
        let workspace = active_workspace().await?;
        if reached(&workspace) {
            return Ok(workspace);
        }
        last = Some(workspace);
        if attempt + 1 < GEOMETRY_VERIFY_ATTEMPTS {
            sleep(GEOMETRY_VERIFY_DELAY).await;
        }
    }
    last.context("Hyprland did not report an active workspace after the request")
}

/// The oldest Hyprland this build can drive: the release that introduced the
/// Lua dispatchers, and stopped parsing the string ones. Everything this
/// server dispatches -- focus, move, resize, float -- speaks the Lua form, so
/// an older compositor rejects all of it.
pub const MINIMUM_HYPRLAND_RELEASE: (u32, u32) = (0, 55);

/// The running Hyprland release, from the first line of `hyprctl version`.
/// `None` when hyprctl cannot be run or its version line cannot be read.
pub fn release() -> Option<HyprlandRelease> {
    let output = hyprctl_output(&["version"]).ok()?;
    if !output.status.success() {
        return None;
    }
    parse_release(&String::from_utf8_lossy(&output.stdout))
}

/// A Hyprland release as `hyprctl version` states it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HyprlandRelease {
    pub major: u32,
    pub minor: u32,
    /// The version as printed, e.g. `0.56.2`.
    pub printed: String,
}

impl HyprlandRelease {
    /// Whether this release speaks the Lua dispatchers, which is every
    /// dispatch this server makes.
    pub fn drives_lua_dispatchers(&self) -> bool {
        (self.major, self.minor) >= MINIMUM_HYPRLAND_RELEASE
    }
}

/// `hyprctl version` opens with `Hyprland 0.56.2 built from branch ...`; a
/// tagged build may print `v0.56.2`, and a development build may carry a
/// suffix after the patch number. Only the first two numbers decide.
fn parse_release(output: &str) -> Option<HyprlandRelease> {
    let line = output.lines().next()?;
    let token = line.split_whitespace().find(|token| {
        token
            .trim_start_matches('v')
            .starts_with(|c: char| c.is_ascii_digit())
    })?;
    let printed = token.trim_start_matches('v').to_string();
    let mut numbers = printed.split(['.', '-', '+']).map(str::parse::<u32>);
    let major = numbers.next()?.ok()?;
    let minor = numbers.next()?.ok()?;
    Some(HyprlandRelease {
        major,
        minor,
        printed,
    })
}

/// True when this process runs inside a Hyprland session it can reach.
pub fn is_active() -> bool {
    std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok_and(|value| !value.trim().is_empty())
        || infer_hyprland_instance_signature().is_some()
}

/// The pointer position in desktop (screenshot-space) coordinates, from
/// `hyprctl cursorpos`.
pub async fn cursor_position() -> Result<(i32, i32)> {
    let output = hyprctl_output_async(&["cursorpos"])
        .await
        .context("failed to run hyprctl cursorpos")?;
    if !output.status.success() {
        bail!("hyprctl cursorpos failed: {}", command_detail(&output));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (x, y) = parse_cursorpos(&text)
        .with_context(|| format!("unexpected hyprctl cursorpos output {:?}", text.trim()))?;
    Ok(match capture_layout().await {
        Some(layout) => layout.map_point([x, y]),
        None => (x, y),
    })
}

/// `hyprctl cursorpos` prints `<x>, <y>` in global layout coordinates.
fn parse_cursorpos(text: &str) -> Option<(i32, i32)> {
    let mut parts = text.trim().split(',').map(str::trim);
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((x, y))
}

/// Windows that overlap `window_id` and sit above it.
///
/// `hyprctl clients -j` carries no stacking order, so `focusHistoryID` stands
/// in for it: Hyprland raises a floating window when it is focused, so a more
/// recently focused overlapping window on the same workspace is above the
/// target. Tiled windows never overlap each other, so the approximation only
/// misses a floating window raised without focus.
pub async fn occluding_windows(window_id: u64) -> Result<Vec<WindowOcclusion>> {
    let output = hyprctl_output_async(&["clients", "-j"])
        .await
        .context("failed to run hyprctl clients -j")?;
    if !output.status.success() {
        bail!(
            "hyprctl clients -j failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let clients: Vec<HyprlandClient> = serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl clients -j output")?;
    Ok(occluders_from_clients(&clients, window_id))
}

fn occluders_from_clients(clients: &[HyprlandClient], window_id: u64) -> Vec<WindowOcclusion> {
    let Some(target) = clients
        .iter()
        .find(|client| parse_hyprland_address(&client.address).ok() == Some(window_id))
    else {
        return Vec::new();
    };
    let Some((target_at, target_size)) = target.at.zip(target.size) else {
        return Vec::new();
    };
    let target_workspace = target.workspace.as_ref().and_then(|workspace| workspace.id);
    clients
        .iter()
        .filter(|client| client.mapped.unwrap_or(true) && !client.hidden.unwrap_or(false))
        .filter(|client| client.address != target.address)
        .filter(|client| {
            client.workspace.as_ref().and_then(|workspace| workspace.id) == target_workspace
        })
        .filter(
            |client| match (client.focus_history_id, target.focus_history_id) {
                (Some(client_focus), Some(target_focus)) => client_focus < target_focus,
                (Some(_), None) => true,
                (None, _) => false,
            },
        )
        .filter(|client| {
            client
                .at
                .zip(client.size)
                .is_some_and(|(at, size)| rects_intersect(at, size, target_at, target_size))
        })
        .filter_map(|client| {
            Some(WindowOcclusion {
                window_id: parse_hyprland_address(&client.address).ok()?,
                title: client.title.clone(),
            })
        })
        .collect()
}

fn rects_intersect(at_a: [i32; 2], size_a: [u32; 2], at_b: [i32; 2], size_b: [u32; 2]) -> bool {
    let right_a = i64::from(at_a[0]) + i64::from(size_a[0]);
    let bottom_a = i64::from(at_a[1]) + i64::from(size_a[1]);
    let right_b = i64::from(at_b[0]) + i64::from(size_b[0]);
    let bottom_b = i64::from(at_b[1]) + i64::from(size_b[1]);
    i64::from(at_a[0]) < right_b
        && i64::from(at_b[0]) < right_a
        && i64::from(at_a[1]) < bottom_b
        && i64::from(at_b[1]) < bottom_a
}

pub async fn move_window(window_id: u64, x: i32, y: i32) -> Result<String> {
    let target = match capture_layout().await {
        Some(layout) => layout.unmap_point([x, y]),
        None => [x, y],
    };
    refuse_if_tiled(window_id, &query_client(window_id).await?, "move")?;
    run_lua_dispatch(&lua_move_dispatch(window_id, target)).await?;
    let after = wait_for_client(window_id, |client| client.at == Some(target)).await?;
    if after.at == Some(target) {
        return Ok(format!("Moved window to ({x}, {y})."));
    }
    Ok(format!(
        "Requested move to ({x}, {y}); Hyprland reported global position {}.",
        describe_pair(after.at.map(|[x, y]| (i64::from(x), i64::from(y))))
    ))
}

pub async fn resize_window(window_id: u64, width: i32, height: i32) -> Result<String> {
    if width <= 0 || height <= 0 {
        bail!("resize requires positive width and height (got {width}x{height})");
    }
    let target = match capture_layout().await {
        Some(layout) => layout.unmap_size([width, height]),
        None => [width, height],
    };
    let target_size = [
        target[0].max(1).cast_unsigned(),
        target[1].max(1).cast_unsigned(),
    ];
    refuse_if_tiled(window_id, &query_client(window_id).await?, "resize")?;
    run_lua_dispatch(&lua_resize_dispatch(window_id, target)).await?;
    // An exact resize re-centers the window, so `at` changes too; the caller
    // sees the fresh geometry through the re-query, and only the size decides
    // whether the request landed.
    let after = wait_for_client(window_id, |client| client.size == Some(target_size)).await?;
    if after.size == Some(target_size) {
        return Ok(format!("Resized window to {width}x{height}."));
    }
    Ok(format!(
        "Requested resize to {width}x{height}; Hyprland reported global size {}.",
        describe_pair(after.size.map(|[w, h]| (i64::from(w), i64::from(h))))
    ))
}

fn window_address(window_id: u64) -> String {
    format!("address:0x{window_id:x}")
}

/// Hyprland 0.55+ wraps every `hyprctl dispatch` argument as
/// `return hl.dispatch(<arg>)`, so the string dispatchers that came before it
/// are rejected as Lua ("')' expected near 'exact'") and the table form is the
/// only one that works -- over the raw IPC socket too, not just the CLI.
/// Coordinates are Hyprland's global layout coordinates, the ones
/// `hyprctl clients -j` reports in `at`.
fn lua_move_dispatch(window_id: u64, [x, y]: [i32; 2]) -> String {
    format!(
        "hl.dsp.window.move({{ window = \"{}\", exact = true, x = {x}, y = {y} }})",
        window_address(window_id)
    )
}

/// The Lua resize form: `x`/`y` carry the width and height.
fn lua_resize_dispatch(window_id: u64, [width, height]: [i32; 2]) -> String {
    format!(
        "hl.dsp.window.resize({{ window = \"{}\", exact = true, x = {width}, y = {height} }})",
        window_address(window_id)
    )
}

/// Float or tile a window. `move_window` and `resize_window` refuse a tiled
/// window, so this is how a caller acts on that refusal, and how it restores
/// the layout afterwards.
pub async fn set_floating(window_id: u64, floating: bool) -> Result<String> {
    let state = describe_floating(floating);
    if query_client(window_id).await?.floating == Some(floating) {
        return Ok(format!("Window 0x{window_id:x} is already {state}."));
    }
    run_lua_dispatch(&lua_float_dispatch(window_id, floating)).await?;
    let after = wait_for_client(window_id, |client| client.floating == Some(floating)).await?;
    if after.floating != Some(floating) {
        bail!(
            "Requested {state} for window 0x{window_id:x}, but Hyprland still reports it {}.",
            describe_floating(!floating)
        );
    }
    Ok(format!("Window 0x{window_id:x} is now {state}."))
}

fn describe_floating(floating: bool) -> &'static str {
    if floating { "floating" } else { "tiled" }
}

/// The Lua float form. The window is named explicitly: `hl.dsp.window.float()`
/// without arguments acts on the focused window instead.
fn lua_float_dispatch(window_id: u64, floating: bool) -> String {
    format!(
        "hl.dsp.window.float({{ window = \"{}\", action = \"{}\" }})",
        window_address(window_id),
        if floating { "set" } else { "unset" }
    )
}

/// Hyprland cannot give a tiled window an exact geometry: it drops a pixel
/// move outright, and turns a pixel resize into a layout-split adjustment,
/// which resizes the *neighboring* windows instead of honoring the request.
/// So both are refused here, before any dispatch runs, and a refusal leaves
/// the layout untouched. The floating state is deliberately left alone: the
/// caller decides whether to float the window, so the refusal names the tool
/// that does it.
fn refuse_if_tiled(window_id: u64, client: &HyprlandClient, operation: &str) -> Result<()> {
    if client.floating == Some(false) {
        bail!(
            "Window 0x{window_id:x} is tiled, so Hyprland cannot {operation} it to an exact geometry: a pixel move is ignored, and a pixel resize moves the layout split, resizing neighboring windows instead. Nothing was dispatched and the layout is unchanged. Call set_window_floating with floating=true, retry, then set_window_floating with floating=false to tile it again."
        );
    }
    Ok(())
}

fn describe_pair(pair: Option<(i64, i64)>) -> String {
    match pair {
        Some((a, b)) => format!("({a}, {b})"),
        None => "unknown".to_string(),
    }
}

/// Run one Lua dispatcher and answer with Hyprland's own words when it is
/// rejected.
///
/// A rejected dispatch still exits zero and prints its complaint on stdout,
/// which is why success is "the output is exactly ok" rather than the exit
/// status.
async fn run_lua_dispatch(lua_dispatch: &str) -> Result<()> {
    let output = hyprctl_output_async(&["dispatch", lua_dispatch])
        .await
        .with_context(|| format!("failed to run hyprctl dispatch {lua_dispatch}"))?;
    if dispatch_succeeded(&output) {
        return Ok(());
    }
    bail!(
        "hyprctl dispatch {lua_dispatch} failed: {}",
        command_detail(&output)
    );
}

async fn query_client(window_id: u64) -> Result<HyprlandClient> {
    let output = hyprctl_output_async(&["clients", "-j"])
        .await
        .context("failed to run hyprctl clients -j")?;
    if !output.status.success() {
        bail!(
            "hyprctl clients -j failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let clients: Vec<HyprlandClient> = serde_json::from_slice(&output.stdout)
        .context("failed to parse hyprctl clients -j output")?;
    clients
        .into_iter()
        .find(|client| parse_hyprland_address(&client.address).ok() == Some(window_id))
        .with_context(|| format!("Hyprland window 0x{window_id:x} is not in hyprctl clients -j"))
}

/// Poll `hyprctl clients -j` until the window satisfies `reached` or the
/// attempts run out, returning the last client state seen.
async fn wait_for_client(
    window_id: u64,
    reached: impl Fn(&HyprlandClient) -> bool,
) -> Result<HyprlandClient> {
    let mut last = None;
    for attempt in 0..GEOMETRY_VERIFY_ATTEMPTS {
        let client = query_client(window_id).await?;
        if reached(&client) {
            return Ok(client);
        }
        last = Some(client);
        if attempt + 1 < GEOMETRY_VERIFY_ATTEMPTS {
            sleep(GEOMETRY_VERIFY_DELAY).await;
        }
    }
    last.with_context(|| {
        format!("Hyprland window 0x{window_id:x} could not be queried after the geometry request")
    })
}

fn dispatch_succeeded(output: &std::process::Output) -> bool {
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "ok"
}

fn command_detail(output: &std::process::Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = if stderr.trim().is_empty() {
        stdout.trim()
    } else {
        stderr.trim()
    };
    if detail.is_empty() {
        format!("exit status {}", output.status)
    } else {
        detail.to_string()
    }
}

fn lua_focus_dispatch(address: &str) -> String {
    format!("hl.dsp.focus({{ window = \"{address}\" }})")
}

fn hyprctl_output(args: &[&str]) -> std::io::Result<std::process::Output> {
    let mut command = StdCommand::new("hyprctl");
    let has_signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok_and(|value| !value.trim().is_empty());
    if !has_signature && let Some(signature) = infer_hyprland_instance_signature() {
        command.args(["-i", &signature]);
    }
    command.args(args).output()
}

async fn hyprctl_output_async(args: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("hyprctl");
    let has_signature =
        std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok_and(|value| !value.trim().is_empty());
    if !has_signature && let Some(signature) = infer_hyprland_instance_signature() {
        command.args(["-i", &signature]);
    }
    command.args(args);
    command_runner::output(command, "run hyprctl").await
}

fn infer_hyprland_instance_signature() -> Option<String> {
    let runtime = xdg_runtime_dir()?;
    let hypr_dir = runtime.join("hypr");
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let candidates = fs::read_dir(hypr_dir)
        .ok()?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let path = entry.path();
            let signature = path.file_name()?.to_string_lossy().into_owned();
            hyprland_instance_candidate(&path, signature, wayland_display.as_deref())
        })
        .collect::<Vec<_>>();

    select_hyprland_instance(candidates).map(|candidate| candidate.signature)
}

fn hyprland_instance_candidate(
    path: &Path,
    signature: String,
    wayland_display: Option<&str>,
) -> Option<HyprlandInstanceCandidate> {
    if !path
        .join(".socket.sock")
        .metadata()
        .is_ok_and(|metadata| metadata.file_type().is_socket())
    {
        return None;
    }

    let lock = fs::read_to_string(path.join("hyprland.lock")).ok()?;
    let mut lines = lock.lines();
    let pid = lines.next()?.trim();
    if pid.is_empty() || !Path::new("/proc").join(pid).exists() {
        return None;
    }
    let lock_wayland_display = lines
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let wayland_display_matches =
        wayland_display.is_some() && lock_wayland_display == wayland_display;
    let modified = path
        .join(".socket.sock")
        .metadata()
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);

    Some(HyprlandInstanceCandidate {
        signature,
        wayland_display_matches,
        modified,
    })
}

fn select_hyprland_instance(
    candidates: Vec<HyprlandInstanceCandidate>,
) -> Option<HyprlandInstanceCandidate> {
    candidates
        .into_iter()
        .max_by_key(|candidate| (candidate.wayland_display_matches, candidate.modified))
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(value));
    }
    let uid = fs::metadata("/proc/self").ok()?.uid();
    Some(PathBuf::from(format!("/run/user/{uid}")))
}

#[derive(Debug)]
struct HyprlandInstanceCandidate {
    signature: String,
    wayland_display_matches: bool,
    modified: SystemTime,
}

#[derive(Debug, Deserialize)]
struct HyprlandMonitor {
    id: i32,
    x: i32,
    y: i32,
    scale: f64,
}

#[derive(Debug, Deserialize)]
struct HyprlandClient {
    address: String,
    mapped: Option<bool>,
    hidden: Option<bool>,
    at: Option<[i32; 2]>,
    size: Option<[u32; 2]>,
    floating: Option<bool>,
    monitor: Option<i32>,
    workspace: Option<HyprlandWorkspace>,
    #[serde(rename = "class")]
    class_name: Option<String>,
    title: Option<String>,
    pid: Option<i64>,
    xwayland: Option<bool>,
    #[serde(rename = "focusHistoryID")]
    focus_history_id: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct HyprlandWorkspace {
    id: Option<i32>,
}

impl TryFrom<HyprlandClient> for WindowInfo {
    type Error = anyhow::Error;

    fn try_from(client: HyprlandClient) -> Result<Self> {
        let window_id = parse_hyprland_address(&client.address)?;
        let bounds = client.size.map(|[width, height]| WindowBounds {
            x: client.at.map(|[x, _]| x),
            y: client.at.map(|[_, y]| y),
            width,
            height,
        });
        let client_type = client.xwayland.map(|xwayland| {
            if xwayland {
                "x11".to_string()
            } else {
                "wayland".to_string()
            }
        });

        Ok(WindowInfo {
            window_id,
            title: client.title,
            app_id: client.class_name.clone(),
            wm_class: client.class_name,
            pid: client.pid.and_then(|pid| u32::try_from(pid).ok()),
            bounds,
            workspace: client.workspace.and_then(|workspace| workspace.id),
            focused: client.focus_history_id == Some(0),
            hidden: client.hidden.unwrap_or(false),
            client_type,
            backend: HYPRLAND_BACKEND.to_string(),
            terminal: None,
        })
    }
}

fn parse_hyprland_address(address: &str) -> Result<u64> {
    let hex = address
        .trim()
        .strip_prefix("0x")
        .context("Hyprland window address did not start with 0x")?;
    u64::from_str_radix(hex, 16)
        .with_context(|| format!("failed to parse Hyprland window address {address}"))
}

#[cfg(test)]
#[expect(
    clippy::unreadable_literal,
    reason = "the long literals here are window addresses copied verbatim from hyprctl output; grouping their digits stops them matching the source they were taken from"
)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn rebases_global_window_coordinates_to_screenshot_space() {
        let clients = r#"[{
            "address":"0x1234",
            "mapped":true,
            "at":[4714,1494],
            "size":[931,1124],
            "monitor":0,
            "class":"com.mitchellh.ghostty",
            "title":"Ghostty"
        }]"#;
        let monitors = br#"[
            {"id":0,"x":3747,"y":1440,"scale":1.8},
            {"id":1,"x":5667,"y":0,"scale":2.0}
        ]"#;
        let windows = parse_hyprland_clients_with_monitors(clients, monitors).unwrap();

        let bounds = windows[0].bounds.as_ref().unwrap();
        assert_eq!((bounds.x, bounds.y), (Some(1934), Some(2988)));
        assert_eq!((bounds.width, bounds.height), (1862, 2248));
    }

    #[test]
    fn global_union_accounts_for_negative_monitor_origins() {
        let clients = r#"[{
            "address":"0x1234",
            "at":[-1800,100],
            "size":[600,400],
            "monitor":0,
            "class":"foot",
            "title":"Shell"
        }]"#;
        let monitors = br#"[
            {"id":0,"x":-1920,"y":0,"scale":1.0},
            {"id":1,"x":0,"y":0,"scale":2.0}
        ]"#;
        let windows = parse_hyprland_clients_with_monitors(clients, monitors).unwrap();

        let bounds = windows[0].bounds.as_ref().unwrap();
        assert_eq!((bounds.x, bounds.y), (Some(240), Some(200)));
        assert_eq!((bounds.width, bounds.height), (1200, 800));
    }

    #[test]
    fn fractional_scale_rounds_crop_edges_outward() {
        let clients = r#"[{
            "address":"0x1234",
            "at":[1,1],
            "size":[1,1],
            "monitor":0,
            "class":"foot",
            "title":"Shell"
        }]"#;
        let monitors = br#"[{"id":0,"x":0,"y":0,"scale":1.25}]"#;
        let windows = parse_hyprland_clients_with_monitors(clients, monitors).unwrap();

        let bounds = windows[0].bounds.as_ref().unwrap();
        assert_eq!((bounds.x, bounds.y), (Some(1), Some(1)));
        assert_eq!((bounds.width, bounds.height), (2, 2));
    }

    #[test]
    fn bounds_are_omitted_without_valid_monitor_metadata() {
        let clients = r#"[{
            "address":"0x1234",
            "at":[100,100],
            "size":[600,400],
            "monitor":7,
            "class":"foot",
            "title":"Shell"
        }]"#;

        for monitors in [
            br"[]".as_slice(),
            br#"[{"id":7,"x":0,"y":0,"scale":0.0}]"#.as_slice(),
            b"not json".as_slice(),
        ] {
            let windows = parse_hyprland_clients_with_monitors(clients, monitors).unwrap();
            assert!(windows[0].bounds.is_none());
        }
        let windows = parse_hyprland_clients_without_bounds(clients).unwrap();
        assert!(windows[0].bounds.is_none());
    }

    #[test]
    fn builds_hyprland_055_lua_focus_dispatch() {
        assert_eq!(
            lua_focus_dispatch("address:0x1234abcd"),
            "hl.dsp.focus({ window = \"address:0x1234abcd\" })"
        );
    }

    #[test]
    fn dispatch_rejects_exit_zero_error_output() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"Invalid dispatcher\n".to_vec(),
            stderr: Vec::new(),
        };

        assert!(!dispatch_succeeded(&output));
        assert_eq!(command_detail(&output), "Invalid dispatcher");
    }

    #[test]
    fn dispatch_accepts_ok_output() {
        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(0),
            stdout: b"ok\n".to_vec(),
            stderr: Vec::new(),
        };

        assert!(dispatch_succeeded(&output));
    }

    #[test]
    fn selects_wayland_matching_hyprland_instance_before_newer_nonmatch() {
        let older_match = HyprlandInstanceCandidate {
            signature: "match".to_string(),
            wayland_display_matches: true,
            modified: SystemTime::UNIX_EPOCH,
        };
        let newer_nonmatch = HyprlandInstanceCandidate {
            signature: "nonmatch".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        };

        let selected = select_hyprland_instance(vec![older_match, newer_nonmatch]).unwrap();

        assert_eq!(selected.signature, "match");
    }

    #[test]
    fn selects_newest_hyprland_instance_when_wayland_match_is_tied() {
        let older = HyprlandInstanceCandidate {
            signature: "older".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH,
        };
        let newer = HyprlandInstanceCandidate {
            signature: "newer".to_string(),
            wayland_display_matches: false,
            modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
        };

        let selected = select_hyprland_instance(vec![older, newer]).unwrap();

        assert_eq!(selected.signature, "newer");
    }

    fn client(floating: Option<bool>) -> HyprlandClient {
        serde_json::from_value(serde_json::json!({
            "address": "0x1234abcd",
            "at": [965, 48],
            "size": [900, 700],
            "floating": floating,
            "class": "sophia",
            "title": "Sophia"
        }))
        .unwrap()
    }

    #[test]
    fn reads_the_release_out_of_the_hyprctl_version_banner() {
        let release = parse_release(
            "Hyprland 0.56.2 built from branch v0.56.2 at commit efb5099 clean\nDate: Wed Aug 5\n",
        )
        .expect("the banner names a release");
        assert_eq!((release.major, release.minor), (0, 56));
        assert_eq!(release.printed, "0.56.2");
        assert!(release.drives_lua_dispatchers());

        let tagged = parse_release("Hyprland v0.55.0 built from branch main\n").unwrap();
        assert_eq!(tagged.printed, "0.55.0");
        assert!(tagged.drives_lua_dispatchers());

        let old = parse_release("Hyprland 0.54.1 built from branch main\n").unwrap();
        assert!(!old.drives_lua_dispatchers());

        let development =
            parse_release("Hyprland 0.57.0-3-gabcdef built from branch main").unwrap();
        assert_eq!((development.major, development.minor), (0, 57));

        assert!(parse_release("").is_none());
        assert!(parse_release("Hyprland built from branch main\n").is_none());
    }

    #[test]
    fn formats_lua_window_dispatches() {
        assert_eq!(
            lua_move_dispatch(0x555c37cb3770, [1000, 100]),
            "hl.dsp.window.move({ window = \"address:0x555c37cb3770\", exact = true, x = 1000, y = 100 })"
        );
        assert_eq!(
            lua_resize_dispatch(0x555c37cb3770, [800, 600]),
            "hl.dsp.window.resize({ window = \"address:0x555c37cb3770\", exact = true, x = 800, y = 600 })"
        );
        assert_eq!(
            lua_float_dispatch(0x555c37cb3770, true),
            "hl.dsp.window.float({ window = \"address:0x555c37cb3770\", action = \"set\" })"
        );
        assert_eq!(
            lua_float_dispatch(0x555c37cb3770, false),
            "hl.dsp.window.float({ window = \"address:0x555c37cb3770\", action = \"unset\" })"
        );
    }

    #[test]
    fn a_rejected_lua_dispatch_is_not_a_success() {
        for stdout in [
            "error: ')' expected near 'exact' (dispatch in lua is a shorthand for hl.dispatch(...))\n",
            "hl.dsp.window.move: expected a table, e.g. { direction = \"left\" }\n",
            "unrecognized arguments. Expected one of: direction, x+y(+relative), workspace, into_group, out_of_group\n",
        ] {
            let output = std::process::Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: stdout.as_bytes().to_vec(),
                stderr: Vec::new(),
            };
            assert!(!dispatch_succeeded(&output), "{stdout}");
        }
    }

    #[test]
    fn tiled_window_is_refused_naming_the_tool_that_clears_the_refusal() {
        let error = refuse_if_tiled(0x1234abcd, &client(Some(false)), "resize")
            .unwrap_err()
            .to_string();

        assert!(error.contains("Window 0x1234abcd is tiled"), "{error}");
        assert!(error.contains("cannot resize it"), "{error}");
        assert!(error.contains("Nothing was dispatched"), "{error}");
        assert!(error.contains("set_window_floating"), "{error}");
    }

    #[test]
    fn floating_or_unknown_windows_are_not_refused() {
        refuse_if_tiled(0x1234abcd, &client(Some(true)), "move").unwrap();
        refuse_if_tiled(0x1234abcd, &client(None), "resize").unwrap();
    }

    #[test]
    fn unmap_inverts_the_screenshot_space_mapping() {
        let monitors: Vec<HyprlandMonitor> = serde_json::from_str(
            r#"[{"id":0,"x":3747,"y":1440,"scale":1.8},{"id":1,"x":5667,"y":0,"scale":2.0}]"#,
        )
        .unwrap();
        let layout = HyprlandCaptureLayout::from_monitors(&monitors).unwrap();

        assert_eq!(layout.unmap_point([1934, 2988]), [4714, 1494]);
        assert_eq!(layout.unmap_size([1862, 2248]), [931, 1124]);
    }

    #[test]
    fn parses_hyprctl_cursorpos_and_maps_it_to_screenshot_space() {
        assert_eq!(parse_cursorpos("1234, 567\n"), Some((1234, 567)));
        assert_eq!(parse_cursorpos("-10,0"), Some((-10, 0)));
        assert_eq!(parse_cursorpos("1234"), None);
        assert_eq!(parse_cursorpos("a, b"), None);

        let monitors: Vec<HyprlandMonitor> =
            serde_json::from_str(r#"[{"id":0,"x":3747,"y":1440,"scale":2.0}]"#).unwrap();
        let layout = HyprlandCaptureLayout::from_monitors(&monitors).unwrap();
        assert_eq!(layout.map_point([4714, 1494]), (1934, 108));
    }

    #[test]
    fn occluders_are_overlapping_more_recently_focused_windows_on_the_workspace() {
        let clients: Vec<HyprlandClient> = serde_json::from_str(
            r#"[
                {"address":"0x1","at":[100,100],"size":[500,400],"workspace":{"id":1},"title":"Target","focusHistoryID":2},
                {"address":"0x2","at":[300,300],"size":[400,400],"workspace":{"id":1},"title":"Above","focusHistoryID":0},
                {"address":"0x3","at":[300,300],"size":[400,400],"workspace":{"id":1},"title":"Below","focusHistoryID":3},
                {"address":"0x4","at":[300,300],"size":[400,400],"workspace":{"id":2},"title":"Elsewhere","focusHistoryID":1},
                {"address":"0x5","at":[700,700],"size":[100,100],"workspace":{"id":1},"title":"Apart","focusHistoryID":1},
                {"address":"0x6","at":[100,100],"size":[50,50],"workspace":{"id":1},"title":"Hidden","hidden":true,"focusHistoryID":1}
            ]"#,
        )
        .unwrap();

        assert_eq!(
            occluders_from_clients(&clients, 0x1),
            vec![WindowOcclusion {
                window_id: 0x2,
                title: Some("Above".to_string()),
            }]
        );
        assert!(occluders_from_clients(&clients, 0x99).is_empty());
    }
}
