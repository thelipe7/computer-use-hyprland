use crate::atspi_tree::{
    AccessibilityAction, AccessibilityNode, AccessibleAppSummary, Bounds, FocusedElementSummary,
    ValueSetInvocation, element_states, focused_element_summary, grab_focus, is_stale_object_error,
    list_accessible_apps, perform_action as invoke_accessibility_action, set_element_value,
    snapshot_limits, snapshot_tree,
};
use crate::diagnostics::{
    DoctorReport, SetupReport, doctor_report, setup_accessibility_report, user_id,
};
use crate::screenshot::{
    RawScreenshotCapture, ScreenshotCapture, ScreenshotOutputFormat, ScreenshotPayloadOptions,
    capture_screenshot_raw, prepare_screenshot_payload,
};
use crate::windowing::registry;
use crate::windowing::{
    WindowFocusResult, WindowInfo, WindowOcclusion, WindowTarget, WorkspaceSummary,
    WorkspaceTarget, focus_window_target, focused_window, list_windows, resolve_window_target,
    window_permission_hint,
};
use crate::ydotool;

/// What `backend` reports when no window backend could answer at all.
const UNKNOWN_BACKEND: &str = "unavailable";
use anyhow::Result;
use rmcp::{
    ErrorData, ServerHandler, ServiceExt,
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, ContentBlock},
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use std::{
    env,
    fmt::Write as _,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    process::Command as TokioCommand,
    time::{sleep, timeout},
};

const INPUT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_FOR_DEFAULT_TIMEOUT_MS: u64 = 5_000;
const STALE_TREE_MESSAGE: &str =
    "cached accessibility tree is stale (app restarted or window closed); call get_app_state again";
const WAIT_FOR_MAX_TIMEOUT_MS: u64 = 60_000;
const WAIT_FOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
const KEY_SEQUENCE_DELAY: Duration = Duration::from_millis(60);
/// How long an app gets to react before post-action feedback is read.
const POST_ACTION_SETTLE: Duration = Duration::from_millis(120);
const ALLOWED_APPS_ENV: &str = "COMPUTER_USE_HYPRLAND_ALLOWED_APPS";
const YDOTOOL_TYPE_CHARS_PER_SECOND: u64 = 20;
/// How far the compositor's pointer position may sit from the point a
/// pointer action emitted before the result says so. One pixel of slack
/// absorbs fractional-scaling rounding; anything more is a lost event.
const POINTER_LANDING_TOLERANCE: i32 = 1;
/// Tail of the note explaining a point that could not be moved onto the
/// desktop, and how to make it resolvable.
const UNANCHORED_BOUNDS_NOTE: &str = "but the tree's bounds are window-relative and no window origin was available, so they were not offset to desktop coordinates and the pointer will miss. Call get_app_state again with pid or window_id so the tree is tied to its window.";

#[derive(Clone, Default)]
pub struct ComputerUseLinux {
    last_nodes: Arc<Mutex<Vec<AccessibilityNode>>>,
    /// How the cached nodes' bounds map onto desktop coordinates. See
    /// [`bounds_are_window_relative`].
    node_bounds: Arc<Mutex<CachedBounds>>,
    /// Lazily-created uinput absolute pointer, the only pointer backend.
    abs_pointer: Arc<Mutex<Option<crate::abs_pointer::AbsPointer>>>,
    input_operation_lock: Arc<tokio::sync::Mutex<()>>,
    /// Cached physical desktop size from the most recent full-frame capture,
    /// used for off-screen warnings.
    desktop_size: Arc<Mutex<Option<(u32, u32)>>>,
    /// The element index every `object_ref` read so far was given.
    element_indices: Arc<Mutex<StableElementIndices>>,
}

/// The element index each `object_ref` was minted with, so an index outlives
/// the re-read of the tree that produced it.
///
/// A positional index is only true of the one snapshot it came from: the next
/// read renumbers every node, and an index taken from the read before it then
/// names a different element without erroring — the click lands on whatever
/// now sits in that position. Keyed by `object_ref`, which survives a re-read
/// and dies with the process that owns it, an index names the same element
/// until that element is gone, and names nothing once it is.
#[derive(Debug, Default)]
struct StableElementIndices {
    assigned: std::collections::HashMap<String, u32>,
    next: u32,
}

impl StableElementIndices {
    /// How many elements are remembered before the map is dropped whole. A
    /// desktop session reads far fewer than this; the cap is what keeps a
    /// server that runs for days from growing without bound.
    const MAX_TRACKED: usize = 20_000;

    /// The index for `object_ref`: the one it already has, or the next unused
    /// number. Numbers are not recycled while they are remembered, so a stale
    /// index fails to resolve rather than resolving to another element.
    fn index_for(&mut self, object_ref: &str) -> u32 {
        if let Some(index) = self.assigned.get(object_ref) {
            return *index;
        }
        if self.assigned.len() >= Self::MAX_TRACKED || self.next == u32::MAX {
            self.assigned.clear();
        }
        let index = self.next;
        self.next = self.next.wrapping_add(1);
        self.assigned.insert(object_ref.to_string(), index);
        index
    }
}

fn sanitize_unsigned_integer_formats(value: &mut serde_json::Value) {
    let serde_json::Value::Object(object) = value else {
        return;
    };

    let has_unsigned_format = matches!(
        object.get("format").and_then(serde_json::Value::as_str),
        Some("uint" | "uint8" | "uint16" | "uint32" | "uint64" | "usize")
    );
    if has_unsigned_format {
        object.remove("format");
    }

    for nested in object.values_mut() {
        match nested {
            serde_json::Value::Object(_) => sanitize_unsigned_integer_formats(nested),
            serde_json::Value::Array(items) => {
                for item in items {
                    sanitize_unsigned_integer_formats(item);
                }
            }
            _ => {}
        }
    }
}

impl ComputerUseLinux {
    #[expect(
        clippy::unused_self,
        reason = "the tool_handler macro expands to `self.mcp_tool_router()`, so it has to be a method"
    )]
    fn mcp_tool_router(&self) -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut router = Self::tool_router();
        for route in router.map.values_mut() {
            let input_schema = Arc::make_mut(&mut route.attr.input_schema);
            for value in input_schema.values_mut() {
                sanitize_unsigned_integer_formats(value);
            }
            if let Some(output_schema) = route.attr.output_schema.as_mut() {
                for value in Arc::make_mut(output_schema).values_mut() {
                    sanitize_unsigned_integer_formats(value);
                }
            }
        }
        router
    }
}

#[tool_router]
impl ComputerUseLinux {
    #[tool(
        name = "doctor",
        description = "Report Linux Computer Use desktop integration readiness.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn doctor(&self) -> Json<DoctorReport> {
        Json(doctor_report().await)
    }

    #[tool(
        name = "setup_accessibility",
        description = "Turn AT-SPI accessibility on through gsettings so the accessibility tree can be read. Only needed when doctor reports at_spi_enabled or toolkit_accessibility as failing; it writes session settings, so leave it alone when they already pass.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn setup_accessibility(&self) -> Json<SetupReport> {
        Json(setup_accessibility_report().await)
    }

    #[tool(
        name = "list_apps",
        description = "List running Linux desktop app candidates visible to the Computer Use backend.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_apps(&self) -> Json<ListAppsOutput> {
        let (accessible_apps, accessibility_error) = match list_accessible_apps(50).await {
            Ok(apps) => (apps, None),
            Err(error) => (Vec::new(), Some(format!("{error:#}"))),
        };

        Json(ListAppsOutput {
            apps: list_process_apps(),
            accessible_apps,
            accessibility_error,
            note: "Linux Computer Use lists process candidates plus AT-SPI application roots when accessibility is enabled.".to_string(),
        })
    }

    #[tool(
        name = "list_windows",
        description = "List compositor windows with title, app id, class, focus state, client type, and known bounds.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn list_windows(&self) -> Json<ListWindowsOutput> {
        Json(window_list_output().await)
    }

    #[tool(
        name = "focused_window",
        description = "Return the compositor window that currently has keyboard focus.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn focused_window(&self) -> Json<FocusedWindowOutput> {
        match focused_window().await {
            Ok(window) => {
                let backend = window_backend(window.as_ref().into_iter());
                Json(FocusedWindowOutput {
                    backend,
                    focused_window: window,
                    error: None,
                    permissions_hint: None,
                    message:
                        "Focused window query completed through the available compositor window backend."
                            .to_string(),
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(FocusedWindowOutput {
                    backend: UNKNOWN_BACKEND.to_string(),
                    focused_window: None,
                    permissions_hint: window_permission_hint(&error),
                    error: Some(error),
                    message: "Focused window query failed; targeted keyboard input is unavailable until window introspection works.".to_string(),
                })
            }
        }
    }

    #[tool(
        name = "activate_window",
        description = "Focus a Linux desktop window by window_id, pid, app_id, wm_class, title, or terminal selectors when the compositor permits it.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn activate_window(
        &self,
        Parameters(params): Parameters<ActivateWindowParams>,
    ) -> Json<ActivateWindowOutput> {
        let target = params.into_target();
        let received = Some(serde_json::json!(target.clone()));
        match focus_window_target(&target).await {
            Ok(focus) => {
                let ok = focus_satisfies_target(&focus, &target);
                Json(ActivateWindowOutput {
                    ok,
                    implemented: true,
                    backend: focus.backend.clone(),
                    focus: Some(focus),
                    error: None,
                    permissions_hint: None,
                    received,
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(ActivateWindowOutput {
                    ok: false,
                    implemented: true,
                    backend: UNKNOWN_BACKEND.to_string(),
                    focus: None,
                    permissions_hint: window_permission_hint(&error),
                    error: Some(error),
                    received,
                })
            }
        }
    }

    #[tool(
        name = "get_app_state",
        description = "Start an app use session if needed, then get a size-bounded screenshot and accessibility state for a Linux app. Screenshot results include coordinate_width, coordinate_height, scale, format, and quality when the returned image is downscaled or compressed; callers can request jpeg/quality for compression before resizing.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn get_app_state(
        &self,
        Parameters(params): Parameters<GetAppStateParams>,
    ) -> Json<GetAppStateOutput> {
        let verbose = params.verbose.unwrap_or(false);
        let diagnostics = doctor_report().await;
        let (window_context, window_error, window_permissions_hint) =
            self.resolve_window_context(&params).await;
        let (max_nodes, max_depth) =
            crate::atspi_tree::snapshot_limits(params.max_nodes, params.max_depth);
        let include_screenshot = params.include_screenshot.unwrap_or(true);
        let screenshot_options = params.screenshot_options();
        let screenshot_target_requested = params.window_target().has_target();
        let app_filter = self
            .resolve_accessibility_app_filter(&params, window_context.as_ref())
            .await;
        let (screenshot, screenshot_error) = if include_screenshot {
            let result: Result<ScreenshotCapture> = async {
                let raw = capture_screenshot_raw().await?;
                self.cache_desktop_size(raw.width, raw.height);
                if let Some(window) = window_context.as_ref() {
                    ensure_readonly_screenshot_target_is_visible(window)?;
                    let crop = Self::window_crop_rect_for_capture(window, &raw)?;
                    prepare_app_state_screenshot(
                        raw,
                        Some(crop),
                        screenshot_target_requested,
                        screenshot_options,
                    )
                } else {
                    prepare_app_state_screenshot(
                        raw,
                        None,
                        screenshot_target_requested,
                        screenshot_options,
                    )
                }
            }
            .await;
            match result {
                Ok(capture) => (Some(capture), None),
                Err(error) => (None, Some(format!("{error:#}"))),
            }
        } else {
            (None, None)
        };
        let (accessibility_tree, accessibility_tree_raw_count, root_pids, accessibility_error) =
            if diagnostics.readiness.can_build_accessibility_tree {
                let target_pid = window_context.as_ref().and_then(|window| window.pid);
                match snapshot_tree(app_filter.as_deref(), target_pid, max_nodes, max_depth).await {
                    Ok(snapshot) => {
                        let raw_count = snapshot.nodes.len();
                        let mut nodes = compact_accessibility_tree(snapshot.nodes);
                        self.apply_stable_indices(&mut nodes);
                        (nodes, raw_count, snapshot.root_pids, None)
                    }
                    Err(error) => (Vec::new(), 0, Vec::new(), Some(format!("{error:#}"))),
                }
            } else {
                (
                    Vec::new(),
                    0,
                    Vec::new(),
                    Some(
                        "AT-SPI accessibility is disabled; call setup_accessibility first."
                            .to_string(),
                    ),
                )
            };
        // A tree whose bounds follow its window needs a window even when the
        // caller named none, or every element point stays window-relative and
        // the pointer misses; the snapshot's own process supplies one.
        let bounds_window = match window_context.clone() {
            Some(window) => Some(window),
            None => {
                self.window_for_untargeted_tree(&accessibility_tree, &root_pids)
                    .await
            }
        };
        if accessibility_error.is_none() {
            self.cache_tree(&accessibility_tree, bounds_window.as_ref());
        } else {
            self.clear_cached_nodes();
        }
        let mut message = if let Some(error) = &accessibility_error {
            format!("MCP registration is working, but AT-SPI tree extraction failed: {error}")
        } else if let Some(capture) = &screenshot {
            format!(
                "MCP registration, screenshot capture, and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}) and a screenshot through {}.",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
                capture.source
            )
        } else if let Some(error) = &screenshot_error {
            format!(
                "MCP registration and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}). Screenshot capture failed: {error}",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
            )
        } else {
            format!(
                "MCP registration and AT-SPI tree extraction are working. Captured {} accessibility nodes (compacted from {}). Screenshot capture was not requested.",
                accessibility_tree.len(),
                accessibility_tree_raw_count,
            )
        };
        if let Some(window) = &window_context {
            let _ = write!(
                message,
                " Window target resolved to window_id {}.",
                window.window_id
            );
            if let Some(note) = cross_process_tree_note(window, &root_pids) {
                let _ = write!(message, " {note}");
            }
        } else if let Some(error) = &window_error {
            let _ = write!(message, " Window target resolution failed: {error}");
        } else if let Some(window) = &bounds_window {
            let _ = write!(
                message,
                " No window target was given; the tree's window-relative bounds were tied to window_id {} through the app's own pid.",
                window.window_id
            );
        }

        // Full diagnostics are huge (portal/process dumps); emit them only on
        // request. The compact readiness block always travels, and failures get
        // a pointer to verbose=true instead of an automatic dump.
        let readiness = diagnostics.readiness.clone();
        let include_full = verbose;
        if !include_full
            && (accessibility_error.is_some()
                || screenshot_error.is_some()
                || window_error.is_some())
        {
            message.push_str(" Pass verbose=true for full diagnostics.");
        }
        Json(GetAppStateOutput {
            app_name_or_bundle_identifier: params.app_name_or_bundle_identifier,
            window_context_source: window_context_source(
                window_context.as_ref(),
                bounds_window.as_ref(),
            ),
            window_context: bounds_window,
            window_error,
            window_permissions_hint,
            backend: "linux-atspi".to_string(),
            screenshot,
            screenshot_error,
            accessibility_tree,
            accessibility_tree_raw_count,
            accessibility_error,
            readiness,
            diagnostics: include_full.then_some(diagnostics),
            message,
        })
    }

    #[tool(
        name = "pointer_position",
        description = "Report the current pointer position in desktop coordinates, the space click, scroll and drag take. Read from hyprctl cursorpos; a session that is not Hyprland answers ok=false.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn pointer_position(&self) -> Json<PointerPositionOutput> {
        Json(match registry::pointer_position().await {
            Ok(Some(((x, y), backend))) => PointerPositionOutput {
                ok: true,
                implemented: true,
                backend: Some(backend.to_string()),
                x: Some(x),
                y: Some(y),
                message: format!("Pointer at ({x}, {y}) via {backend}."),
            },
            Ok(None) => PointerPositionOutput {
                ok: false,
                implemented: true,
                backend: None,
                x: None,
                y: None,
                message: "unsupported backend: pointer_position needs a Hyprland or X11 session."
                    .to_string(),
            },
            Err(error) => PointerPositionOutput {
                ok: false,
                implemented: true,
                backend: None,
                x: None,
                y: None,
                message: format!("{error:#}"),
            },
        })
    }

    #[tool(
        name = "wait_for",
        description = "Poll until every given predicate holds or a timeout elapses (timeout_ms default 5000, max 60000; polls every 100 ms). Predicates, all of which must hold: an element selector (role/name/text/states, the same matcher click and perform_action use) present in the target app's AT-SPI tree, optionally with focused=true; window_title, a substring of the target window's title (or of the focused window's title without a target); focused_window, a window selector that must hold focus. Target the app with the same selectors as get_app_state (pid/window_id/app_id/wm_class/title). On success the matching element is returned with its index in a freshly cached tree, so a following click/perform_action/set_value can pass element_index. On timeout ok=false with the last tree summary.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn wait_for(&self, Parameters(params): Parameters<WaitForParams>) -> Json<WaitForOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let started = std::time::Instant::now();
        let timeout = wait_for_timeout(params.timeout_ms);
        let deadline = started + timeout;
        if !wait_for_has_predicate(&params) {
            return Json(WaitForOutput {
                ok: false,
                implemented: true,
                satisfied: false,
                elapsed_ms: 0,
                element: None,
                window_context: None,
                window_context_source: None,
                focused_window: None,
                last_tree_summary: None,
                message: "wait_for needs at least one predicate: an element selector (role/name/text/states), window_title, or focused_window.".to_string(),
                received,
            });
        }
        let app_state_params = params.app_state_params();
        let selector = params.selector();
        let mut last = loop {
            let probe = self
                .probe_wait_predicates(&params, &app_state_params, &selector)
                .await;
            let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            if probe.satisfied {
                let element_note = probe
                    .element
                    .as_ref()
                    .map(|element| {
                        format!(
                            " Matched element_index {} ({}{}) in a freshly cached tree.",
                            element.index,
                            element.role,
                            element
                                .name
                                .as_deref()
                                .map(|name| format!(" \"{name}\""))
                                .unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                return Json(WaitForOutput {
                    ok: true,
                    implemented: true,
                    satisfied: true,
                    elapsed_ms,
                    element: probe.element,
                    window_context: probe.window_context,
                    window_context_source: probe.window_context_source,
                    focused_window: probe.focused_window,
                    last_tree_summary: probe.summary,
                    message: format!("Predicates satisfied after {elapsed_ms} ms.{element_note}"),
                    received,
                });
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break probe;
            }
            sleep(WAIT_FOR_POLL_INTERVAL.min(deadline - now)).await;
        };
        let elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let reason = last
            .error
            .take()
            .unwrap_or_else(|| "predicates never held".to_string());
        let summary = last
            .summary
            .as_deref()
            .map(|summary| format!(" {summary}."))
            .unwrap_or_default();
        Json(WaitForOutput {
            ok: false,
            implemented: true,
            satisfied: false,
            elapsed_ms,
            element: None,
            window_context: last.window_context,
            window_context_source: last.window_context_source,
            focused_window: last.focused_window,
            last_tree_summary: last.summary,
            message: format!(
                "Timed out after {} ms: {reason}.{summary}",
                timeout.as_millis()
            ),
            received,
        })
    }

    #[tool(
        name = "screenshot",
        description = "Capture the screen and return it as a viewable, size-bounded image. Optionally target a window (window_id/pid/wm_class/title/app_id): the window is raised to the front and the image is cropped before any resize. With raise_window=false the window is captured where it is and the caption lists `occluded_by` (windows above it on the same workspace, when the backend can tell) with a warning. `region` ({x, y, width, height} in desktop coordinates, or window-relative with a window target and relative=true) crops further before any resize, to zoom into small text; the caption's `crop` reports the desktop rectangle returned. Returns the image plus a short caption with returned dimensions, coordinate dimensions, scale, format, quality, source, and crop bounds; callers can request jpeg/quality for compression before resizing.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn screenshot(
        &self,
        Parameters(params): Parameters<ScreenshotParams>,
    ) -> Result<CallToolResult, ErrorData> {
        let target = params.window_target();
        let target_window = match target.as_ref() {
            Some(target) => Some(
                self.resolve_screenshot_window(target, params.raise_window.unwrap_or(true))
                    .await
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("targeted screenshot failed: {error:#}"),
                            None,
                        )
                    })?,
            ),
            None => None,
        };
        let crop_window = (!params.full_screen.unwrap_or(false))
            .then_some(target_window.as_ref())
            .flatten();
        let occluded_by = match target_window.as_ref() {
            Some(window) if !params.raise_window.unwrap_or(true) => {
                registry::occluding_windows(window).await
            }
            _ => Vec::new(),
        };
        let window_label = target_window
            .as_ref()
            .and_then(|window| window.title.clone());

        let raw_capture = capture_screenshot_raw()
            .await
            .map_err(|e| ErrorData::internal_error(format!("screenshot failed: {e}"), None))?;
        self.cache_desktop_size(raw_capture.width, raw_capture.height);

        // Warn when the target window extends past the visible desktop: the
        // portal only captures on-screen pixels, so the crop silently loses the
        // off-screen region while coordinate metadata still claims full size.
        let off_screen_note = match crop_window.and_then(|window| window.bounds.as_ref()) {
            Some(bounds) => self.off_screen_note_for_bounds(bounds).await,
            None => None,
        };

        let (capture, window_crop) = match crop_window {
            Some(window) => {
                let (x, y, width, height) =
                    Self::window_crop_rect_for_capture(window, &raw_capture).map_err(|error| {
                        ErrorData::internal_error(
                            format!("targeted screenshot crop failed: {error:#}"),
                            None,
                        )
                    })?;
                let (bytes, width, height) = crop_png(&raw_capture.bytes, x, y, width, height)
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("targeted screenshot crop failed: {error}"),
                            None,
                        )
                    })?;
                (
                    RawScreenshotCapture {
                        mime_type: raw_capture.mime_type,
                        bytes,
                        source: raw_capture.source,
                        width,
                        height,
                    },
                    Some((x, y, width, height)),
                )
            }
            None => (raw_capture, None),
        };
        let cropped = window_crop.is_some();
        let mut crop_bounds = window_crop;
        let capture = match params.region.as_ref() {
            Some(region) => {
                let ((x, y, width, height), desktop_rect) = region_crop_rect(
                    region,
                    params.relative.unwrap_or(false),
                    window_crop,
                    capture.width,
                    capture.height,
                )
                .map_err(|message| {
                    ErrorData::invalid_params(
                        format!("screenshot region rejected: {message}"),
                        None,
                    )
                })?;
                let (bytes, width, height) = crop_png(&capture.bytes, x, y, width, height)
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("screenshot region crop failed: {error}"),
                            None,
                        )
                    })?;
                crop_bounds = Some(desktop_rect);
                RawScreenshotCapture {
                    mime_type: capture.mime_type,
                    bytes,
                    source: capture.source,
                    width,
                    height,
                }
            }
            None => capture,
        };
        let capture =
            prepare_screenshot_payload(capture, params.screenshot_options()).map_err(|e| {
                ErrorData::internal_error(format!("screenshot resize failed: {e}"), None)
            })?;

        let mut caption = serde_json::json!({
            "width": capture.width,
            "height": capture.height,
            "coordinate_width": capture.coordinate_width,
            "coordinate_height": capture.coordinate_height,
            "scale": capture.scale,
            "resized": capture.resized,
            "bytes": capture.bytes,
            "original_bytes": capture.original_bytes,
            "max_bytes": capture.max_bytes,
            "format": capture.format,
            "quality": capture.quality,
            "source": capture.source,
            "cropped_to_window": cropped,
            "cropped_to_region": params.region.is_some(),
            "crop": crop_bounds.map(|(x, y, width, height)| {
                serde_json::json!({ "x": x, "y": y, "width": width, "height": height })
            }),
            "window_title": window_label,
        });
        if let Some(note) = off_screen_note {
            caption["window_off_screen"] = serde_json::json!(true);
            caption["off_screen_note"] = serde_json::json!(note);
        }
        if target_window.is_some() {
            caption["occluded_by"] = serde_json::json!(occluded_by);
            if let Some(note) = occlusion_note(&occluded_by) {
                caption["occlusion_note"] = serde_json::json!(note);
            }
        }
        Ok(CallToolResult::success(vec![
            ContentBlock::image(data_url_payload(&capture.data_url), capture.mime_type),
            ContentBlock::text(caption.to_string()),
        ]))
    }

    /// Lazily create the uinput absolute pointer, sizing its ABS range to the
    /// logical desktop (the portal screenshot dimensions). Returns `false` if it
    /// can't be created or is disabled via `COMPUTER_USE_HYPRLAND_DISABLE_ABS_POINTER`.
    async fn ensure_abs_pointer(&self) -> bool {
        if env_flag_enabled("COMPUTER_USE_HYPRLAND_DISABLE_ABS_POINTER") {
            return false;
        }
        if self.abs_pointer.lock().is_ok_and(|g| g.is_some()) {
            return true;
        }
        let Ok(cap) = capture_screenshot_raw().await else {
            return false;
        };
        self.cache_desktop_size(cap.width, cap.height);
        match tokio::task::spawn_blocking(move || {
            crate::abs_pointer::AbsPointer::create(
                i32::try_from(cap.width).unwrap_or(i32::MAX),
                i32::try_from(cap.height).unwrap_or(i32::MAX),
            )
        })
        .await
        {
            Ok(Ok(pointer)) => {
                if let Ok(mut guard) = self.abs_pointer.lock() {
                    *guard = Some(pointer);
                    return true;
                }
                false
            }
            _ => false,
        }
    }

    /// Try a coordinate click through the absolute uinput pointer. Returns the
    /// requested and emitted coordinates from that backend, or `None` to fall
    /// through.
    async fn try_abs_click(
        &self,
        x: i32,
        y: i32,
        button: Option<&str>,
        count: u32,
    ) -> Option<crate::abs_pointer::PointerLanding> {
        let btn = crate::abs_pointer::PointerButton::from_name(button)?;
        if !self.ensure_abs_pointer().await {
            return None;
        }
        let abs_pointer = Arc::clone(&self.abs_pointer);
        tokio::task::spawn_blocking(move || {
            let mut guard = abs_pointer.lock().ok()?;
            let pointer = guard.as_mut()?;
            pointer.click(x, y, btn, count).ok()
        })
        .await
        .ok()
        .flatten()
    }

    /// Move the pointer through the absolute uinput device, or `None` to fall
    /// through to ydotool.
    async fn try_abs_move(&self, x: i32, y: i32) -> Option<crate::abs_pointer::PointerLanding> {
        if !self.ensure_abs_pointer().await {
            return None;
        }
        let abs_pointer = Arc::clone(&self.abs_pointer);
        tokio::task::spawn_blocking(move || {
            let mut guard = abs_pointer.lock().ok()?;
            let pointer = guard.as_mut()?;
            pointer.move_to(x, y).ok()
        })
        .await
        .ok()
        .flatten()
    }

    #[tool(
        name = "click",
        description = "Click an element by index, object_ref, semantic selector, or desktop coordinate pixels from screenshot metadata. A plain left click on an element that exposes an AT-SPI click action invokes that action first and only falls back to the pointer; the message says which path was used. `modifiers` (ctrl/alt/shift/meta) are held around a pointer click. A pointer click reports the desktop point it landed on, and warns when the compositor reports the pointer somewhere else.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click(&self, Parameters(mut params): Parameters<ClickParams>) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let _input_lease = match self
            .input_gate("click", params.window_target().as_ref())
            .await
        {
            Ok(lease) => lease,
            Err(message) => return Json(action_failure("click", message, received)),
        };
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        // Raise the target window first (if specified) so the click lands on the
        // intended app rather than whatever is stacked on top at that pixel.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(action_failure(
                "click",
                "Relative coordinate clicks require a window target.".to_string(),
                received,
            ));
        }
        let mut focus = None;
        if let Some(target) = window_target {
            focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(action_failure("click", message, received));
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Window-relative coordinates: translate by the window's top-left so
            // the agent can click the pixel it saw in a window-cropped screenshot.
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(action_failure(
                        "click",
                        "Relative coordinate clicks require verified target-window focus."
                            .to_string(),
                        received,
                    ));
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus) {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(action_failure("click", message, received));
                    }
                };
                if let Err(message) = apply_window_relative_click_coordinates(
                    &mut params,
                    coordinate_map.capture_rect,
                ) {
                    return Json(action_failure("click", message, received));
                }
            }
        }
        let bounds = self.current_bounds().await;
        let target = match self.resolve_click_target(&params, bounds.offset()) {
            Ok(target) => target,
            Err(message) => {
                return Json(action_failure("click", message, received));
            }
        };
        let held_modifiers = match modifier_keycodes(&params.modifiers) {
            Ok(codes) => codes,
            Err(message) => {
                return Json(action_failure("click", message, received));
            }
        };
        let (element_index, label, object_ref, action, point, bounds_offset, states) = match target
        {
            ClickTarget::Coordinates(x, y) => {
                let output = self
                    .click_at_point_with_modifiers(
                        x,
                        y,
                        &params,
                        received,
                        input_guard,
                        &held_modifiers,
                    )
                    .await;
                if !output.0.ok {
                    return output;
                }
                let notes = self.post_action_notes(focus.as_ref(), None).await;
                return Json(with_notes(output.0, notes));
            }
            ClickTarget::Element {
                element_index,
                label,
                object_ref,
                action,
                point,
                bounds_offset,
                states,
            } => (
                element_index,
                label,
                object_ref,
                action,
                point,
                bounds_offset,
                states,
            ),
        };
        let mut notes = Vec::new();
        let action = if held_modifiers.is_empty() {
            action
        } else {
            if action.is_some() {
                notes.push(
                    "Modifiers only apply to the pointer, so the AT-SPI click action was skipped."
                        .to_string(),
                );
            }
            None
        };
        if let Some(action) = action {
            let action_label = format!(
                "AT-SPI action {} ({})",
                action.index,
                if action.name.is_empty() {
                    "unnamed"
                } else {
                    action.name.as_str()
                }
            );
            let action_index = action.index.to_string();
            let failure = match invoke_accessibility_action(&object_ref, Some(&action_index)).await
            {
                Ok(invocation) if invocation.ok => {
                    let notes = self
                        .post_action_notes(focus.as_ref(), Some((&object_ref, &states)))
                        .await;
                    return Json(with_notes(
                        ActionOutput {
                            ok: true,
                            implemented: true,
                            action: "click".to_string(),
                            message: format!(
                                "Invoked {action_label} on element_index {element_index} ({label}); the pointer was not used."
                            ),
                            received,
                        },
                        notes,
                    ));
                }
                Ok(_) => format!("{action_label} on element_index {element_index} returned false"),
                Err(error) if is_stale_object_error(&error) => {
                    return Json(action_failure(
                        "click",
                        STALE_TREE_MESSAGE.to_string(),
                        received,
                    ));
                }
                Err(error) => format!(
                    "{action_label} on element_index {element_index} failed: {}",
                    first_line(&format!("{error:#}"))
                ),
            };
            if point.is_none() {
                return Json(action_failure(
                    "click",
                    format!("{failure}, and no clickable bounds were cached."),
                    received,
                ));
            }
            notes.push(format!("{failure}; fell back to the pointer."));
        }
        let Some((x, y)) = point else {
            unreachable!("an element click target carries an action or a point");
        };
        notes.push(match (bounds_offset, &bounds) {
            (Some((dx, dy)), _) => format!(
                "element_index {element_index} ({label}) resolved to desktop point ({x}, {y}): the tree's window-relative bounds were offset by the window origin ({dx}, {dy})."
            ),
            (None, CachedBounds::Unanchored) => {
                format!("element_index {element_index} ({label}) resolved to point ({x}, {y}), {UNANCHORED_BOUNDS_NOTE}")
            }
            (None, _) => {
                format!("element_index {element_index} ({label}) resolved to desktop point ({x}, {y}).")
            }
        });
        let output = self
            .click_at_point_with_modifiers(x, y, &params, received, input_guard, &held_modifiers)
            .await;
        if output.0.ok {
            notes.extend(
                self.post_action_notes(focus.as_ref(), Some((&object_ref, &states)))
                    .await,
            );
        }
        Json(with_notes(output.0, notes))
    }

    /// `click_at_point` with modifier keys held through ydotool around it.
    async fn click_at_point_with_modifiers(
        &self,
        x: i32,
        y: i32,
        params: &ClickParams,
        received: Option<serde_json::Value>,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
        held_modifiers: &[u16],
    ) -> Json<ActionOutput> {
        if held_modifiers.is_empty() {
            return self
                .click_at_point(x, y, params, received, input_guard)
                .await;
        }
        if let Err(message) = run_ydotool(&modifier_hold_args(held_modifiers, true)).await {
            return Json(action_failure(
                "click",
                format!("Could not hold the modifiers through ydotool: {message}"),
                received,
            ));
        }
        let output = self
            .click_at_point(x, y, params, received, input_guard)
            .await;
        let release = run_ydotool(&modifier_hold_args(held_modifiers, false)).await;
        let note = match release {
            Ok(_) => format!(
                "Held modifiers {} around the click.",
                params.modifiers.join("+")
            ),
            Err(message) => format!(
                "WARNING: modifiers {} may still be held; releasing them failed: {message}",
                params.modifiers.join("+")
            ),
        };
        Json(with_notes(output.0, [note]))
    }

    /// Click a desktop coordinate: the uinput absolute pointer, then ydotool.
    async fn click_at_point(
        &self,
        x: i32,
        y: i32,
        params: &ClickParams,
        received: Option<serde_json::Value>,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Json<ActionOutput> {
        let button = mouse_button_code(params.button.as_deref());
        let click_count = params.click_count.unwrap_or(1).clamp(1, 10).to_string();
        // Preferred backend: the uinput absolute pointer. Unlike ydotool's
        // relative-only device (faked `--absolute` via pin-to-corner + relative
        // move, which acceleration + fractional scaling distort), the
        // absolute pointer uses screenshot-pixel coordinates directly and
        // reports the point it emitted after desktop-edge clamping.
        if let Some(landing) = self
            .try_abs_click(
                x,
                y,
                params.button.as_deref(),
                params.click_count.unwrap_or(1).clamp(1, 10),
            )
            .await
        {
            let (emitted_x, emitted_y) = landing.emitted;
            let mut notes = abs_pointer_clamp_note(landing)
                .into_iter()
                .collect::<Vec<_>>();
            notes.extend(self.pointer_landing_note(landing.emitted).await);
            return Json(with_notes(
                ActionOutput {
                    ok: true,
                    implemented: true,
                    action: "click".to_string(),
                    message: format!(
                        "Action sent through the uinput absolute pointer at desktop point ({emitted_x}, {emitted_y})."
                    ),
                    received,
                },
                notes,
            ));
        }
        let off_screen_note = self.off_screen_note_for_point(x, y).await;
        let commands = vec![
            absolute_mousemove_args(x, y),
            vec![
                "click".to_string(),
                "--repeat".to_string(),
                click_count,
                button,
            ],
        ];
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_sequence(&commands).await
        })
        .await;
        let _input_guard = input_guard;
        Json(with_notes(
            action_result("click", result, received),
            off_screen_note,
        ))
    }

    #[tool(
        name = "perform_action",
        description = "Invoke an accessibility action exposed by an element selected by index, identifier, or semantic selector. Defaults to the primary action unless action is provided.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn perform_action(
        &self,
        Parameters(params): Parameters<ActionParams>,
    ) -> Json<ActionOutput> {
        let _input_lease = match self.input_gate("perform_action", None).await {
            Ok(lease) => lease,
            Err(message) => {
                return Json(action_failure(
                    "perform_action",
                    message,
                    Some(serde_json::json!(params.clone())),
                ));
            }
        };
        let requested_action = requested_or_primary_action(params.action.as_deref());
        self.perform_element_action(&params, Some(requested_action))
            .await
    }

    #[tool(
        name = "set_value",
        description = "Set the value of a settable accessibility element selected by index, object_ref, identifier, or semantic selector. Uses the AT-SPI Value or EditableText interface; an element with neither that is focusable and editable by state gets a keyboard fallback (GrabFocus, Ctrl+A, type the value), which the message reports.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn set_value(
        &self,
        Parameters(params): Parameters<SetValueParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let _input_lease = match self.input_gate("set_value", None).await {
            Ok(lease) => lease,
            Err(message) => return Json(action_failure("set_value", message, received)),
        };
        let object_ref = match self.resolve_object_ref(
            params.element_index,
            params
                .element_identifier
                .as_deref()
                .or(params.object_ref.as_deref()),
            &params.selector(),
            ElementResolvePurpose::SetValue,
        ) {
            Ok(object_ref) => object_ref,
            Err(message) => {
                return Json(action_failure("set_value", message, received));
            }
        };

        match set_element_value(&object_ref, &params.value).await {
            Ok(ValueSetInvocation::Numeric { value }) => Json(ActionOutput {
                ok: true,
                implemented: true,
                action: "set_value".to_string(),
                message: format!("AT-SPI numeric value set to {value}."),
                received,
            }),
            Ok(ValueSetInvocation::EditableText) => Json(ActionOutput {
                ok: true,
                implemented: true,
                action: "set_value".to_string(),
                message: "AT-SPI editable text contents set.".to_string(),
                received,
            }),
            Err(error)
                if !is_stale_object_error(&error)
                    && error.to_string().contains("does not expose AT-SPI Value")
                    && self.cached_node_is_keyboard_editable(&object_ref) =>
            {
                self.keyboard_set_value(&object_ref, &params.value, received)
                    .await
            }
            Err(error) => Json(action_failure(
                "set_value",
                element_error_message(&error),
                received,
            )),
        }
    }

    #[tool(
        name = "scroll",
        description = "Scroll an element in a direction by a number of pages. With element_index, an AT-SPI action named like \"scroll down\" for that direction is invoked first when the element exposes one; otherwise wheel events go to the element's center. With a window target and no x/y/element_index, scrolls at the center of the targeted window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn scroll(&self, Parameters(mut params): Parameters<ScrollParams>) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let _input_lease = match self
            .input_gate("scroll", params.window_target().as_ref())
            .await
        {
            Ok(lease) => lease,
            Err(message) => return Json(action_failure("scroll", message, received)),
        };
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "a float-to-int cast saturates, and max(1) below makes the saturated value harmless"
        )]
        let units = ((params.pages.unwrap_or(1.0).abs().max(0.1) * 5.0).round() as i32).max(1);
        // Raise/focus the target window first (parity with click) so wheel
        // events land on the intended app.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(action_failure(
                "scroll",
                "Relative scroll coordinates require a window target.".to_string(),
                received,
            ));
        }
        if let Some(target) = window_target {
            let focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(action_failure("scroll", message, received));
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(action_failure(
                        "scroll",
                        "Relative scroll coordinates require verified target-window focus."
                            .to_string(),
                        received,
                    ));
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus) {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(action_failure("scroll", message, received));
                    }
                };
                if let Err(message) = apply_window_relative_scroll_coordinates(
                    &mut params,
                    coordinate_map.capture_rect,
                ) {
                    return Json(action_failure("scroll", message, received));
                }
            } else if params.x.is_none() && params.y.is_none() && params.element_index.is_none() {
                // A window target without a point would otherwise scroll
                // whatever happens to sit under the pointer: focusing does not
                // move the cursor, and the wheel path never repositions it.
                // Default to the center of the resolved target window.
                let Some(focus) = focus.as_ref() else {
                    return Json(action_failure(
                        "scroll",
                        "Window-targeted scroll requires verified target-window focus.".to_string(),
                        received,
                    ));
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus) {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(action_failure("scroll", message, received));
                    }
                };
                if let Err(message) =
                    apply_window_center_scroll_point(&mut params, coordinate_map.capture_rect)
                {
                    return Json(action_failure("scroll", message, received));
                }
            }
        }
        let mut notes = Vec::new();
        if let Some((object_ref, action)) = params
            .element_index
            .zip(parse_scroll_direction(&params.direction))
            .and_then(|(element_index, direction)| {
                self.cached_scroll_action(element_index, direction)
            })
        {
            let action_label = format!("AT-SPI action {} ({})", action.index, action.name);
            match invoke_accessibility_action(&object_ref, Some(&action.index.to_string())).await {
                Ok(invocation) if invocation.ok => {
                    return Json(ActionOutput {
                        ok: true,
                        implemented: true,
                        action: "scroll".to_string(),
                        message: format!(
                            "Invoked {action_label} on element_index {}; the wheel was not used.",
                            params.element_index.unwrap_or_default()
                        ),
                        received,
                    });
                }
                Ok(_) => notes.push(format!(
                    "{action_label} returned false; fell back to the wheel."
                )),
                Err(error) if is_stale_object_error(&error) => {
                    return Json(action_failure(
                        "scroll",
                        STALE_TREE_MESSAGE.to_string(),
                        received,
                    ));
                }
                Err(error) => notes.push(format!(
                    "{action_label} failed ({}); fell back to the wheel.",
                    first_line(&format!("{error:#}"))
                )),
            }
        }
        let bounds = self.current_bounds().await;
        let target_point = match self.resolve_optional_target_point(
            params.x,
            params.y,
            params.element_index,
            bounds.offset(),
        ) {
            Ok(point) => point,
            Err(message) => {
                return Json(action_failure("scroll", message, received));
            }
        };
        let direction = match params.direction.to_ascii_lowercase().as_str() {
            "up" => ScrollDirection::Up,
            "down" => ScrollDirection::Down,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            _ => {
                return Json(action_failure(
                    "scroll",
                    "Unsupported scroll direction; expected up, down, left, or right.".to_string(),
                    received,
                ));
            }
        };
        let mut point_notes = Vec::new();
        if let Some((x, y)) = target_point {
            if params.element_index.is_some() && bounds == CachedBounds::Unanchored {
                point_notes.push(format!(
                    "The scroll point ({x}, {y}) came from cached bounds, {UNANCHORED_BOUNDS_NOTE}"
                ));
            }
            point_notes.extend(self.off_screen_note_for_point(x, y).await);
        }
        let (dx, dy) = ydotool_wheel_delta(direction, units);
        let mut sequence = Vec::new();
        if let Some((x, y)) = target_point {
            // The absolute pointer lands exactly where the click path does;
            // ydotool's faked absolute move drifts under acceleration and
            // scaling, so it is only the fallback for positioning the wheel.
            match self.try_abs_move(x, y).await {
                Some(landing) => {
                    point_notes.extend(abs_pointer_clamp_note(landing));
                    point_notes.extend(self.pointer_landing_note(landing.emitted).await);
                }
                None => sequence.push(absolute_mousemove_args(x, y)),
            }
        }
        sequence.push(wheel_mousemove_args(dx, dy));
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_sequence(&sequence).await
        })
        .await;
        let _input_guard = input_guard;
        notes.extend(point_notes);
        Json(with_notes(action_result("scroll", result, received), notes))
    }

    #[tool(
        name = "drag",
        description = "Drag from one point to another. Each end is either a desktop coordinate pair, a window-relative pair (with a window target and `relative: true`), or the center of an element (`start_element_index`/`end_element_index` from the latest get_app_state tree). A window target is raised and focused first, so the drag lands on the intended app rather than whatever is stacked on top at that pixel. The pointer travels in small steps between the two ends rather than jumping, so a component that reacts to the first movement under it -- a title bar handing its window to the compositor, a list reordering itself -- sees the drag. The result reports the desktop point each end resolved to and how many steps carried the pointer between them.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn drag(&self, Parameters(params): Parameters<DragParams>) -> Json<ActionOutput> {
        let _input_lease = match self.input_gate("drag", None).await {
            Ok(lease) => lease,
            Err(message) => {
                return Json(action_failure(
                    "drag",
                    message,
                    Some(serde_json::json!(params)),
                ));
            }
        };
        let held_modifiers = match modifier_keycodes(&params.modifiers) {
            Ok(codes) => codes,
            Err(message) => {
                return Json(action_failure(
                    "drag",
                    message,
                    Some(serde_json::json!(params)),
                ));
            }
        };
        let (start, end, mut notes) = match self.resolve_drag_endpoints(&params).await {
            Ok(resolved) => resolved,
            Err(message) => {
                return Json(action_failure(
                    "drag",
                    message,
                    Some(serde_json::json!(params)),
                ));
            }
        };
        if !held_modifiers.is_empty()
            && let Err(message) = run_ydotool(&modifier_hold_args(&held_modifiers, true)).await
        {
            return Json(action_failure(
                "drag",
                format!("Could not hold the modifiers through ydotool: {message}"),
                Some(serde_json::json!(params)),
            ));
        }
        let modifiers = params.modifiers.join("+");
        let output = self.drag_inner(params, start, end).await;
        if !held_modifiers.is_empty() {
            notes.push(
                match run_ydotool(&modifier_hold_args(&held_modifiers, false)).await {
                    Ok(_) => format!("Held modifiers {modifiers} around the drag."),
                    Err(message) => format!(
                        "WARNING: modifiers {modifiers} may still be held; releasing them failed: {message}"
                    ),
                },
            );
        }
        if !output.0.ok {
            return Json(with_notes(output.0, notes));
        }
        notes.extend(self.post_action_notes(None, None).await);
        Json(with_notes(output.0, notes))
    }

    /// Resolve both drag ends to desktop coordinates, focusing the target
    /// window first when one was given. Returns a note per end saying how it
    /// resolved, so a drag that lands somewhere unexpected can be read back
    /// from the result instead of guessed at.
    async fn resolve_drag_endpoints(
        &self,
        params: &DragParams,
    ) -> std::result::Result<((i32, i32), (i32, i32), Vec<String>), String> {
        let window_target = params.window_target();
        let relative = params.relative == Some(true);
        if relative && window_target.is_none() {
            return Err("Relative drag coordinates require a window target.".to_string());
        }
        let mut origin = (0, 0);
        if let Some(target) = window_target {
            let focus = self.focus_target_for_input(&target).await?;
            tokio::time::sleep(Duration::from_millis(120)).await;
            if relative {
                let focus = focus.as_ref().ok_or_else(|| {
                    "Relative drag coordinates require verified target-window focus.".to_string()
                })?;
                let (x, y, _, _) = self.focused_window_coordinate_map(focus)?.capture_rect;
                origin = (x, y);
            }
        }
        let offset = self.current_bounds().await.offset();
        let (start, start_note) = self.drag_endpoint(
            "start",
            params.start_element_index,
            params.start_x,
            params.start_y,
            origin,
            offset,
        )?;
        let (end, end_note) = self.drag_endpoint(
            "end",
            params.end_element_index,
            params.end_x,
            params.end_y,
            origin,
            offset,
        )?;
        Ok((start, end, vec![start_note, end_note]))
    }

    fn drag_endpoint(
        &self,
        label: &str,
        element_index: Option<u32>,
        x: Option<i32>,
        y: Option<i32>,
        origin: (i32, i32),
        offset: Option<(i32, i32)>,
    ) -> std::result::Result<((i32, i32), String), String> {
        if let Some(element_index) = element_index {
            if x.is_some() || y.is_some() {
                return Err(format!(
                    "Give either {label}_element_index or {label}_x/{label}_y, not both."
                ));
            }
            let (px, py) = self
                .center_for_cached_node(element_index, offset)
                .ok_or_else(|| {
                    format!(
                        "No bounds cached for {label}_element_index {element_index}. Call get_app_state first and choose a node with positive width and height."
                    )
                })?;
            let element = self
                .cached_node_label(element_index)
                .unwrap_or_else(|| "unknown element".to_string());
            let note = match offset {
                Some((dx, dy)) => format!(
                    "{label}_element_index {element_index} ({element}) resolved to desktop point ({px}, {py}): the tree's window-relative bounds were offset by the window origin ({dx}, {dy})."
                ),
                None => format!(
                    "{label}_element_index {element_index} ({element}) resolved to desktop point ({px}, {py})."
                ),
            };
            return Ok(((px, py), note));
        }
        let (Some(x), Some(y)) = (x, y) else {
            return Err(format!(
                "A drag needs {label}_x and {label}_y, or {label}_element_index."
            ));
        };
        let point = (x + origin.0, y + origin.1);
        let note = if origin == (0, 0) {
            format!("{label} resolved to desktop point ({x}, {y}).")
        } else {
            format!(
                "{label} resolved to desktop point ({}, {}): window-relative ({x}, {y}) offset by the window origin ({}, {}).",
                point.0, point.1, origin.0, origin.1
            )
        };
        Ok((point, note))
    }

    async fn drag_inner(
        &self,
        params: DragParams,
        start: (i32, i32),
        end: (i32, i32),
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params));
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        // Preferred backend: the uinput absolute pointer (accurate landing).
        if self.ensure_abs_pointer().await {
            let abs_pointer = Arc::clone(&self.abs_pointer);
            let dragged = tokio::task::spawn_blocking(move || {
                let mut guard = abs_pointer.lock().ok()?;
                let pointer = guard.as_mut()?;
                pointer
                    .drag(start, end, crate::abs_pointer::PointerButton::Left)
                    .ok()
            })
            .await
            .ok()
            .flatten();
            if let Some(landing) = dragged {
                let (start_x, start_y) = landing.start.emitted;
                let (end_x, end_y) = landing.end.emitted;
                let steps = landing.steps;
                let mut notes = abs_pointer_clamp_note(landing.start)
                    .into_iter()
                    .chain(abs_pointer_clamp_note(landing.end))
                    .collect::<Vec<_>>();
                notes.extend(self.pointer_landing_note(landing.end.emitted).await);
                return Json(with_notes(
                    ActionOutput {
                        ok: true,
                        implemented: true,
                        action: "drag".to_string(),
                        message: format!(
                            "Action sent through the uinput absolute pointer: pressed at ({start_x}, {start_y}), moved to ({end_x}, {end_y}) in {steps} steps, released there."
                        ),
                        received,
                    },
                    notes,
                ));
            }
        }
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_drag(start.0, start.1, end.0, end.1).await
        })
        .await;
        let _input_guard = input_guard;
        Json(action_result("drag", result, received))
    }

    #[tool(
        name = "press_key",
        description = "Press a key or key-combination on the keyboard, optionally after focusing a target window or terminal selector. Pass `key` for one key or chord, or `keys` (an array in the same grammar) to send a sequence in one call with a short delay between entries; exactly one of the two must be given. Key grammar (case-insensitive; hyphens/spaces ignored): combos join with '+', e.g. Ctrl+L or Ctrl+Shift+T. Modifiers: ctrl/control, alt/option, shift, meta/super/cmd/command. Named keys: enter/return, escape/esc, tab, backspace, delete/del, space, home, end, pageup, pagedown, arrowleft/left, arrowright/right, arrowup/up, arrowdown/down, f1-f12. Plus single US letters a-z and digits 0-9. Anything else returns an error (never silently dropped). Keys and chords are sent through ydotool, which needs a connectable ydotoold socket; `doctor` reports whether there is one. Note: compositor-level shortcuts (e.g. Super+Up) may be consumed by Hyprland before reaching the app.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let _input_lease = match self
            .input_gate("press_key", Some(&params.window_target()))
            .await
        {
            Ok(lease) => lease,
            Err(message) => return Json(action_failure("press_key", message, received)),
        };
        let keys = match press_key_sequence(params.key.as_deref(), &params.keys) {
            Ok(keys) => keys,
            Err(message) => {
                return Json(action_failure("press_key", message, received));
            }
        };
        let mut input_guard = Some(Arc::clone(&self.input_operation_lock).lock_owned().await);
        let focus = match self.focus_target_for_input(&params.window_target()).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(action_failure("press_key", message, received));
            }
        };
        let mut last_output = None;
        for (index, key) in keys.iter().enumerate() {
            let guard = match input_guard.take() {
                Some(guard) => guard,
                None => Arc::clone(&self.input_operation_lock).lock_owned().await,
            };
            let (guard, mut output) = self
                .press_key_once(key, focus.clone(), received.clone(), guard)
                .await;
            input_guard = guard;
            if !output.ok {
                if keys.len() > 1 {
                    output.message = format!(
                        "Key {}/{} ({key}) failed: {}",
                        index + 1,
                        keys.len(),
                        output.message
                    );
                }
                return Json(output);
            }
            last_output = Some(output);
            if index + 1 < keys.len() {
                sleep(KEY_SEQUENCE_DELAY).await;
            }
        }
        let mut output = last_output.expect("press_key_sequence yields at least one key");
        if keys.len() > 1 {
            output.message = format!(
                "Sent {} keys ({}). {}",
                keys.len(),
                keys.join(", "),
                output.message
            );
        }
        let notes = self.input_landing_notes(focus.as_ref(), false).await;
        Json(with_notes(output, notes))
    }

    /// One key or chord through the best keyboard backend. Returns the input
    /// guard when the backend handed it back so a sequence can keep it.
    async fn press_key_once(
        &self,
        key: &str,
        focus: Option<WindowFocusResult>,
        received: Option<serde_json::Value>,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> (Option<tokio::sync::OwnedMutexGuard<()>>, ActionOutput) {
        let Some(key_events) = key_sequence(key) else {
            return (
                Some(input_guard),
                action_failure(
                    "press_key",
                    format!(
                        "Unsupported key {key:?}. Use names like Enter, Escape, Tab, ArrowLeft, Super, Ctrl+L, or a single US keyboard letter/digit."
                    ),
                    received,
                ),
            );
        };
        // A chord holds its modifiers down across the key press; a bare key
        // does not, and ydotool needs to be told which shape this is.
        let is_chord = key_chord(key).is_some_and(|(modifiers, _)| !modifiers.is_empty());
        let args = ydotool_key_args(key_events, is_chord);
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool(&args).await.map(|output| vec![output])
        })
        .await;
        (
            input_guard,
            action_result_with_focus("press_key", result, received, focus),
        )
    }

    #[tool(
        name = "type_text",
        description = "Type literal text using keyboard input, optionally after focusing a target window or terminal selector.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn type_text(
        &self,
        Parameters(params): Parameters<TypeTextParams>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let _input_lease = match self
            .input_gate("type_text", Some(&params.window_target()))
            .await
        {
            Ok(lease) => lease,
            Err(message) => return Json(action_failure("type_text", message, received)),
        };
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        let focus = match self.focus_target_for_input(&params.window_target()).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(action_failure("type_text", message, received));
            }
        };
        // X11: xdotool type resolves keysyms against the live XKB layout.
        // ydotool's raw scancodes get re-mapped by X11 and mangle symbols and
        // digits (`_` → `%`, `1` → `+`) even on a plain US layout (issue #58).
        if Self::should_prefer_wtype_keyboard() {
            let text = params.text.clone();
            let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
                run_wtype_type_text_or_fallback(Path::new("wtype"), &text, || {
                    run_ydotool_type_text(&text)
                })
                .await
            })
            .await;
            let _input_guard = input_guard;
            let used_wtype = result
                .as_ref()
                .is_ok_and(|result| result.backend == KeyboardCommandBackend::Wtype);
            let mut output = action_result_with_focus(
                "type_text",
                result.map(|result| vec![result.output]),
                received,
                focus.clone(),
            );
            if used_wtype {
                output.message =
                    "Action sent through wtype (Wayland virtual-keyboard protocol).".to_string();
            }
            if output.ok {
                let notes = self.input_landing_notes(focus.as_ref(), true).await;
                output = with_notes(output, notes);
            }
            return Json(output);
        }
        let text = params.text.clone();
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_type_text(&text)
                .await
                .map(|output| vec![output])
        })
        .await;
        let _input_guard = input_guard;
        let mut output = action_result_with_focus("type_text", result, received, focus.clone());
        if output.ok {
            let notes = self.input_landing_notes(focus.as_ref(), true).await;
            output = with_notes(output, notes);
        }
        Json(output)
    }

    #[tool(
        name = "move_window",
        description = "Move a window to a new desktop position (frame top-left in desktop coordinates). Useful to recover a window that is partially off-screen. Dispatched through hyprctl, so it needs a floating window: Hyprland ignores a pixel move on a tiled one, and the call is refused before anything is dispatched. Call set_window_floating with floating=true first, then put the layout back with floating=false.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn move_window(
        &self,
        Parameters(params): Parameters<MoveWindowParams>,
    ) -> Json<WindowGeometryOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let target = params.target.clone().into_target();
        self.window_geometry_op(received, &target, |window| async move {
            registry::move_window(&window, params.x, params.y).await
        })
        .await
    }

    #[tool(
        name = "resize_window",
        description = "Resize a window to a new frame width/height in desktop pixels. Useful to fit a window fully on-screen. Dispatched through hyprctl, so it needs a floating window: on a tiled one a pixel resize moves the layout split and resizes the neighbors instead, so the call is refused before anything is dispatched. Call set_window_floating with floating=true first, then put the layout back with floating=false.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn resize_window(
        &self,
        Parameters(params): Parameters<ResizeWindowParams>,
    ) -> Json<WindowGeometryOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let target = params.target.clone().into_target();
        self.window_geometry_op(received, &target, |window| async move {
            registry::resize_window(&window, params.width, params.height).await
        })
        .await
    }

    #[tool(
        name = "focus_workspace",
        description = "Show a workspace. `workspace` is a workspace id -- the number list_windows reports for every window -- or \"empty\" for the first workspace with nothing on it, which Hyprland picks. The result names the workspace the view was on before, which is how to put it back when the work is done. Use it to give an application under test a workspace of its own: a window that opens next to another is tiled to share the space, so its geometry depends on whatever else was open, and screenshots and pointer input only reach the visible workspace.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn focus_workspace(
        &self,
        Parameters(params): Parameters<FocusWorkspaceParams>,
    ) -> Json<WorkspaceOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let failure = |message: String| {
            Json(WorkspaceOutput {
                ok: false,
                implemented: true,
                workspace: None,
                previous_workspace: None,
                window: None,
                message,
                received: received.clone(),
            })
        };
        let target = match WorkspaceTarget::parse(&params.workspace) {
            Ok(target) => target,
            Err(error) => return failure(format!("{error:#}")),
        };
        let _input_lease = match self.input_gate("focus_workspace", None).await {
            Ok(lease) => lease,
            Err(message) => return failure(message),
        };
        match registry::focus_workspace(target).await {
            Ok(change) => {
                let previous = change.previous.id;
                let current = &change.current;
                let message = format!(
                    "Workspace {} ({}) is visible, with {} window(s) on it. The view was on workspace {previous}; pass workspace=\"{previous}\" to put it back.",
                    current.id, current.name, current.windows
                );
                Json(WorkspaceOutput {
                    ok: true,
                    implemented: true,
                    workspace: Some(change.current),
                    previous_workspace: Some(change.previous),
                    window: None,
                    message,
                    received,
                })
            }
            Err(error) => failure(format!("Could not show {}: {error:#}", target.describe())),
        }
    }

    #[tool(
        name = "move_window_to_workspace",
        description = "Move one window to another workspace. `workspace` is a workspace id -- the number list_windows reports for every window -- or \"empty\" for the first workspace with nothing on it. `follow` (default true) takes the view along, which is what driving that window afterwards needs: screenshots and pointer input only reach the visible workspace. Use it to give an application already running a workspace of its own; the result names the workspace the view came from, so it can be put back.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn move_window_to_workspace(
        &self,
        Parameters(params): Parameters<MoveWindowToWorkspaceParams>,
    ) -> Json<WorkspaceOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let failure = |message: String| {
            Json(WorkspaceOutput {
                ok: false,
                implemented: true,
                workspace: None,
                previous_workspace: None,
                window: None,
                message,
                received: received.clone(),
            })
        };
        let target = match WorkspaceTarget::parse(&params.workspace) {
            Ok(target) => target,
            Err(error) => return failure(format!("{error:#}")),
        };
        let follow = params.follow.unwrap_or(true);
        let window_target = params.target.clone().into_target();
        let _input_lease = match self
            .input_gate("move_window_to_workspace", Some(&window_target))
            .await
        {
            Ok(lease) => lease,
            Err(message) => return failure(message),
        };
        let windows = match list_windows().await {
            Ok(windows) => windows,
            Err(error) => return failure(format!("Window listing failed: {error:#}")),
        };
        let window = match resolve_window_target(&windows, &window_target) {
            Ok(window) => window.clone(),
            Err(error) => return failure(format!("{error:#}")),
        };
        let window_id = window.window_id;
        match registry::move_window_to_workspace(&window, target, follow).await {
            Ok(moved) => {
                let view = &moved.view;
                let mut message = format!(
                    "Moved window 0x{window_id:x} from workspace {} to workspace {}.",
                    describe_workspace_id(moved.from),
                    describe_workspace_id(moved.to)
                );
                if follow {
                    let _ = write!(
                        message,
                        " The view followed and is on workspace {} ({} window(s)); it was on workspace {}, which is where to put it back.",
                        view.current.id, view.current.windows, view.previous.id
                    );
                } else {
                    let _ = write!(
                        message,
                        " The view stayed on workspace {}, so the window is not visible: screenshots and pointer input cannot reach it until a focus_workspace brings it back.",
                        view.current.id
                    );
                }
                let window = list_windows()
                    .await
                    .ok()
                    .and_then(|windows| windows.into_iter().find(|w| w.window_id == window_id));
                Json(WorkspaceOutput {
                    ok: true,
                    implemented: true,
                    workspace: Some(moved.view.current),
                    previous_workspace: Some(moved.view.previous),
                    window,
                    message,
                    received,
                })
            }
            Err(error) => failure(format!(
                "Could not move window 0x{window_id:x} to {}: {error:#}",
                target.describe()
            )),
        }
    }

    #[tool(
        name = "set_window_floating",
        description = "Float a window, or tile it again. On Hyprland a tiled window cannot be given an exact geometry, so move_window and resize_window refuse one; this is how to clear that refusal. Float the window, move or resize it, then tile it again to restore the layout. Reports the state Hyprland ended up in.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = true
        )
    )]
    async fn set_window_floating(
        &self,
        Parameters(params): Parameters<SetWindowFloatingParams>,
    ) -> Json<WindowGeometryOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let target = params.target.clone().into_target();
        self.window_geometry_op(received, &target, |window| async move {
            registry::set_window_floating(&window, params.floating).await
        })
        .await
    }
}

#[expect(
    clippy::unused_async_trait_impl,
    reason = "the ServerHandler impl is generated by the tool_handler macro, which writes the trait's async signatures"
)]
#[tool_handler(
    router = self.mcp_tool_router(),
    name = "computer-use-hyprland",
    // NOTE: keep in lockstep with Cargo.toml + package.json on every release.
    // The rmcp tool_handler macro only accepts a string literal here, so this
    // can't be env!("CARGO_PKG_VERSION"); the MCP safety check (CI) fails the
    // build if it drifts from the Cargo version.
    version = "0.1.0",
    instructions = "Begin every turn that uses Computer Use by calling get_app_state. This server drives one desktop: Hyprland on Wayland. Windows come from hyprctl, the accessibility tree from AT-SPI, screenshots from the XDG Screenshot portal, and every input event from uinput -- an absolute pointer device for click, scroll and drag, wtype for literal text, and ydotool for keys and chords. There is no RemoteDesktop portal on Hyprland and this build does not look for one. Use list_windows/focused_window before targeted keyboard input. Screenshot results include width/height for the returned image plus coordinate_width/coordinate_height and scale for desktop coordinate conversion; request more detail with max_width, max_height, max_bytes, format=jpeg, quality, or a smaller target/crop instead of relying on unbounded screenshots. A window-targeted screenshot raises the window first; with raise_window=false the caption lists occluded_by so overlapping pixels are not mistaken for the target's own. Tools with readOnlyHint=false may mutate local desktop or application state; hosts should require approval for actions that can submit, delete, send, purchase, or overwrite data. For element-targeted actions, prefer element_index from the latest get_app_state result; click, perform_action and set_value can also use semantic role/name/text/states selectors when the target is unique, and a semantic selector needs a get_app_state or wait_for in this same server process first because it matches against the cached tree. An element_index keeps naming the same element across re-reads of the tree and stops resolving once that element is gone, so a stale one errors instead of acting on whatever took its place. A plain left click on an element that exposes an AT-SPI click action invokes that action and never moves the pointer; the message says which path ran. Every window-targeted tool takes the same nine selectors -- window_id, pid, app_id, wm_class, title, tty, terminal_pid, terminal_command, terminal_cwd -- and they refuse targeted input if focus cannot be verified. click, scroll and drag also accept relative coordinates, and drag accepts start_element_index/end_element_index; each reports the desktop point every end resolved to, names the element an index resolved to, and warns when the compositor reports the pointer somewhere other than where the action was aimed. A drag travels in small steps rather than one jump, so a component that only reacts to movement under the pointer sees it. A window selector that matches more than one window refuses and names them rather than picking one: pass window_id, or the exact title. On wait_for, window_title is a predicate, not a selector: the substring the target window's title must contain. After click, drag, perform_action, press_key and type_text, results append focused-element feedback from AT-SPI (role, name, editable, states) and warn when no editable element holds focus after typing -- treat that warning as the input not landing; element clicks and actions also report the element's states before -> after when they changed. When an element operation answers that the cached accessibility tree is stale, call get_app_state again before retrying. wait_for polls until an element selector, a window title substring, or a focused window holds and returns the element with its index in a freshly cached tree. pointer_position reports the pointer's desktop coordinates. Hyprland cannot give a tiled window an exact geometry, so move_window and resize_window refuse one without dispatching anything; call set_window_floating with floating=true, retry, then set_window_floating with floating=false to restore the layout. The first input action of this process takes a machine-wide session lock, every call renews it, and it frees after COMPUTER_USE_HYPRLAND_LOCK_IDLE_SECS seconds without a call (30 unless set; 0 holds it until exit); another server process gets ok=false naming the holder's pid, and the right response is to wait that long and retry once. When COMPUTER_USE_HYPRLAND_ALLOWED_APPS is set, input tools refuse windows matching none of its app_id/wm_class/title patterns. Screenshot, click and input results warn when the target window or coordinate is partially or fully off-screen. get_app_state returns a compact readiness block by default; pass verbose=true for the full diagnostics dump. Electron apps expose no AT-SPI tree unless launched with --force-renderer-accessibility, and GPUI apps put only labeled nodes on the bus, so text in a plain container is readable by screenshot alone. When get_app_state answers with a tree from a different process than the targeted window, the message says so: the two describe different applications."
)]
impl ServerHandler for ComputerUseLinux {
    /// Every call renews the session lease before it is dispatched, so a
    /// session reading the screen between two inputs keeps its input lock.
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> std::result::Result<rmcp::model::CallToolResponse, ErrorData> {
        crate::session_lock::touch_input_lock();
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.mcp_tool_router().call(call).await
    }
}

/// The `COMPUTER_USE_HYPRLAND_ALLOWED_APPS` patterns, or `None` when the
/// variable is unset or blank (no restriction).
fn allowed_app_patterns(value: Option<&str>) -> Option<Vec<String>> {
    let patterns = value?
        .split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!patterns.is_empty()).then_some(patterns)
}

/// A window is allowed when any pattern is a case-insensitive substring of
/// its `app_id`, `wm_class`, or title.
fn window_matches_allowlist(window: &WindowInfo, patterns: &[String]) -> bool {
    let haystacks = [
        window.app_id.as_deref(),
        window.wm_class.as_deref(),
        window.title.as_deref(),
    ];
    patterns.iter().any(|pattern| {
        haystacks
            .iter()
            .flatten()
            .any(|value| normalized_contains(Some(value), pattern))
    })
}

pub async fn serve_mcp() -> Result<()> {
    ComputerUseLinux::default()
        .serve(rmcp::transport::stdio())
        .await?
        .waiting()
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListAppsOutput {
    apps: Vec<AppCandidate>,
    accessible_apps: Vec<AccessibleAppSummary>,
    accessibility_error: Option<String>,
    note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ListWindowsOutput {
    backend: String,
    windows: Vec<WindowInfo>,
    error: Option<String>,
    permissions_hint: Option<String>,
    note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct FocusedWindowOutput {
    backend: String,
    focused_window: Option<WindowInfo>,
    error: Option<String>,
    permissions_hint: Option<String>,
    message: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ActivateWindowParams {
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    terminal_pid: Option<u32>,
    #[serde(default)]
    terminal_command: Option<String>,
    #[serde(default)]
    terminal_cwd: Option<String>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

impl ActivateWindowParams {
    /// The same target without consuming the params.
    fn to_target(&self) -> WindowTarget {
        self.clone().into_target()
    }

    /// The target, or `None` when no selector field was supplied at all.
    fn optional_target(&self) -> Option<WindowTarget> {
        let target = self.to_target();
        target.has_target().then_some(target)
    }

    fn into_target(self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty,
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command,
            terminal_cwd: self.terminal_cwd,
            app_id: self.app_id,
            wm_class: self.wm_class,
            title: self.title,
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ActivateWindowOutput {
    ok: bool,
    implemented: bool,
    backend: String,
    focus: Option<WindowFocusResult>,
    error: Option<String>,
    permissions_hint: Option<String>,
    // Echo of the request for debugging. `serde_json::Value` has no fixed JSON
    // schema, which strict MCP clients (Claude Code) reject in `outputSchema` —
    // and one invalid tool fails the whole tool list. Keep it in the runtime
    // response (serde) but omit it from the generated schema.
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct FocusWorkspaceParams {
    /// The workspace to show: an id (the number `list_windows` reports for
    /// each window) or `"empty"` for the first workspace with nothing on it.
    workspace: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct MoveWindowToWorkspaceParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// The workspace to move the window to: an id (the number `list_windows`
    /// reports for each window) or `"empty"` for the first workspace with
    /// nothing on it.
    workspace: String,
    /// Take the view to that workspace with the window (default true).
    /// Screenshots and pointer input only reach the visible workspace, so a
    /// window sent away without the view cannot be driven until something
    /// brings it back.
    follow: Option<bool>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WorkspaceOutput {
    ok: bool,
    implemented: bool,
    /// The workspace the view is on now.
    workspace: Option<WorkspaceSummary>,
    /// The workspace the view was on before the call, so a caller can put it
    /// back where it found it.
    previous_workspace: Option<WorkspaceSummary>,
    /// The window that was moved, as the compositor reports it afterwards.
    /// Only `move_window_to_workspace` fills this in.
    window: Option<WindowInfo>,
    message: String,
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct MoveWindowParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// New frame-left in desktop coordinates.
    x: i32,
    /// New frame-top in desktop coordinates.
    y: i32,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct SetWindowFloatingParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// True to float the window, false to tile it again.
    floating: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ResizeWindowParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// New frame width in desktop pixels.
    width: i32,
    /// New frame height in desktop pixels.
    height: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WindowGeometryOutput {
    ok: bool,
    implemented: bool,
    backend: String,
    /// Post-operation window info (compositor-final geometry).
    window: Option<WindowInfo>,
    message: String,
    permissions_hint: Option<String>,
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct AppCandidate {
    name: String,
    pid: u32,
    command: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct GetAppStateParams {
    #[serde(default)]
    app_name_or_bundle_identifier: Option<String>,
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// Maximum raw AT-SPI nodes to inspect before compaction (default 1000, hard max 2000).
    #[serde(default)]
    max_nodes: Option<usize>,
    /// Maximum AT-SPI traversal depth (default 32, hard max 64).
    #[serde(default)]
    max_depth: Option<u32>,
    #[serde(default)]
    include_screenshot: Option<bool>,
    /// Maximum returned screenshot width in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_width: Option<u32>,
    /// Maximum returned screenshot height in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_height: Option<u32>,
    /// Maximum returned screenshot image bytes before base64 (default 2 MiB, hard-capped).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Additional downscale factor from 0.0 to 1.0, applied before max dimensions.
    #[serde(default)]
    scale: Option<f32>,
    /// Output image format (default png). Use jpeg with quality to trade exact pixels for smaller payloads.
    #[serde(default)]
    format: Option<ScreenshotOutputFormat>,
    /// JPEG quality from 1 to 95 (default 80). Ignored for png.
    #[serde(default)]
    #[schemars(range(min = 1, max = 95))]
    quality: Option<u8>,
    /// Include the full diagnostics report (large). Default false: only the
    /// compact readiness block is returned.
    #[serde(default)]
    verbose: Option<bool>,
}

impl GetAppStateParams {
    fn window_target(&self) -> WindowTarget {
        self.target.to_target()
    }

    fn screenshot_options(&self) -> ScreenshotPayloadOptions {
        ScreenshotPayloadOptions {
            max_width: self.max_width,
            max_height: self.max_height,
            max_bytes: self.max_bytes,
            scale: self.scale,
            format: self.format,
            quality: self.quality,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ScreenshotParams {
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// Raise the targeted window before capture (default true). Ignored without
    /// a window target.
    #[serde(default)]
    raise_window: Option<bool>,
    /// Capture the whole desktop even when a window is targeted (default false).
    #[serde(default)]
    full_screen: Option<bool>,
    /// Crop to this rectangle before any resize, to zoom into small text. In
    /// desktop coordinates, or window-relative with a window target and
    /// `relative: true`.
    #[serde(default)]
    region: Option<ScreenshotRegion>,
    /// Interpret `region` relative to the targeted window's top-left corner.
    #[serde(default)]
    relative: Option<bool>,
    /// Maximum returned screenshot width in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_width: Option<u32>,
    /// Maximum returned screenshot height in pixels (default 1920, hard-capped).
    #[serde(default)]
    max_height: Option<u32>,
    /// Maximum returned screenshot image bytes before base64 (default 2 MiB, hard-capped).
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Additional downscale factor from 0.0 to 1.0, applied before max dimensions.
    #[serde(default)]
    scale: Option<f32>,
    /// Output image format (default png). Use jpeg with quality to trade exact pixels for smaller payloads.
    #[serde(default)]
    format: Option<ScreenshotOutputFormat>,
    /// JPEG quality from 1 to 95 (default 80). Ignored for png.
    #[serde(default)]
    #[schemars(range(min = 1, max = 95))]
    quality: Option<u8>,
}

impl ScreenshotParams {
    fn window_target(&self) -> Option<WindowTarget> {
        self.target.optional_target()
    }

    fn screenshot_options(&self) -> ScreenshotPayloadOptions {
        ScreenshotPayloadOptions {
            max_width: self.max_width,
            max_height: self.max_height,
            max_bytes: self.max_bytes,
            scale: self.scale,
            format: self.format,
            quality: self.quality,
        }
    }
}

/// `(x, y, width, height)` in pixels.
type PixelRect = (i32, i32, u32, u32);

fn occlusion_note(occluded_by: &[WindowOcclusion]) -> Option<String> {
    if occluded_by.is_empty() {
        return None;
    }
    let names = occluded_by
        .iter()
        .map(|window| {
            format!(
                "{} (window_id {})",
                window.title.as_deref().unwrap_or("untitled"),
                window.window_id
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "WARNING: {} window(s) overlap the target and sit above it, so their pixels appear in the crop: {names}. Raise the target (raise_window=true) or activate_window first for a clean capture.",
        occluded_by.len()
    ))
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct ScreenshotRegion {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

/// The rectangle a `region` request cuts out of the current capture (in that
/// capture's pixels) and the same rectangle in desktop coordinates for the
/// caption. `window_crop` is the desktop rectangle the capture was already
/// cropped to, when it was.
#[expect(
    clippy::map_err_ignore,
    reason = "the discarded error is a TryFromIntError, whose message names no dimension; each arm names the one that failed"
)]
fn region_crop_rect(
    region: &ScreenshotRegion,
    relative: bool,
    window_crop: Option<PixelRect>,
    capture_width: u32,
    capture_height: u32,
) -> std::result::Result<(PixelRect, PixelRect), String> {
    if region.width == 0 || region.height == 0 {
        return Err("region needs a positive width and height.".to_string());
    }
    let (origin_x, origin_y) = match (relative, window_crop) {
        (true, Some((x, y, _, _))) => (x, y),
        (true, None) => {
            return Err("relative regions need a window target.".to_string());
        }
        (false, _) => (0, 0),
    };
    let desktop_left = i64::from(origin_x) + i64::from(region.x);
    let desktop_top = i64::from(origin_y) + i64::from(region.y);
    let desktop_right = desktop_left + i64::from(region.width);
    let desktop_bottom = desktop_top + i64::from(region.height);
    let (capture_left, capture_top) = match window_crop {
        Some((x, y, _, _)) => (i64::from(x), i64::from(y)),
        None => (0, 0),
    };
    let left = (desktop_left - capture_left).max(0);
    let top = (desktop_top - capture_top).max(0);
    let right = (desktop_right - capture_left).min(i64::from(capture_width));
    let bottom = (desktop_bottom - capture_top).min(i64::from(capture_height));
    if right <= left || bottom <= top {
        return Err(format!(
            "region ({}, {}, {}x{}) lies outside the captured {}x{} image.",
            region.x, region.y, region.width, region.height, capture_width, capture_height
        ));
    }
    let out_of_range = |what: &str| format!("region {what} does not fit the captured image.");
    let capture_rect = (
        i32::try_from(left).map_err(|_| out_of_range("x"))?,
        i32::try_from(top).map_err(|_| out_of_range("y"))?,
        u32::try_from(right - left).map_err(|_| out_of_range("width"))?,
        u32::try_from(bottom - top).map_err(|_| out_of_range("height"))?,
    );
    let desktop_rect = (
        i32::try_from(left + capture_left).map_err(|_| out_of_range("x"))?,
        i32::try_from(top + capture_top).map_err(|_| out_of_range("y"))?,
        capture_rect.2,
        capture_rect.3,
    );
    Ok((capture_rect, desktop_rect))
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct PointerPositionOutput {
    ok: bool,
    implemented: bool,
    backend: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    message: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct WaitForParams {
    #[serde(default)]
    app_name_or_bundle_identifier: Option<String>,
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// Element predicate: role of the awaited element (substring, case-insensitive).
    #[serde(default)]
    role: Option<String>,
    /// Element predicate: accessible name (substring, case-insensitive).
    #[serde(default)]
    name: Option<String>,
    /// Element predicate: text, name, or description content (substring).
    #[serde(default)]
    text: Option<String>,
    /// Element predicate: AT-SPI states the element must carry.
    #[serde(default)]
    states: Vec<String>,
    /// Require the matched element to hold keyboard focus.
    #[serde(default)]
    focused: Option<bool>,
    /// Substring the target window's title must contain (the focused window's
    /// title when no window target is given).
    #[serde(default)]
    window_title: Option<String>,
    /// A window selector that must hold focus.
    #[serde(default)]
    focused_window: Option<ActivateWindowParams>,
    /// Give up after this many milliseconds (default 5000, max 60000).
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Maximum raw AT-SPI nodes to inspect per poll (default 1000, hard max 2000).
    #[serde(default)]
    max_nodes: Option<usize>,
    /// Maximum AT-SPI traversal depth per poll (default 32, hard max 64).
    #[serde(default)]
    max_depth: Option<u32>,
}

impl WaitForParams {
    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }

    fn app_state_params(&self) -> GetAppStateParams {
        GetAppStateParams {
            app_name_or_bundle_identifier: self.app_name_or_bundle_identifier.clone(),
            target: self.target.clone(),
            ..GetAppStateParams::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct WaitForOutput {
    ok: bool,
    implemented: bool,
    satisfied: bool,
    elapsed_ms: u64,
    /// The element that satisfied the selector, with its index in the tree
    /// cached by this call.
    element: Option<AccessibilityNode>,
    window_context: Option<WindowInfo>,
    /// How `window_context` was found; absent when there is no window.
    #[serde(skip_serializing_if = "Option::is_none")]
    window_context_source: Option<WindowContextSource>,
    focused_window: Option<WindowInfo>,
    last_tree_summary: Option<String>,
    message: String,
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

#[derive(Debug, Default)]
struct WaitProbe {
    satisfied: bool,
    element: Option<AccessibilityNode>,
    window_context: Option<WindowInfo>,
    window_context_source: Option<WindowContextSource>,
    focused_window: Option<WindowInfo>,
    summary: Option<String>,
    error: Option<String>,
}

fn wait_for_timeout(timeout_ms: Option<u64>) -> Duration {
    Duration::from_millis(
        timeout_ms
            .unwrap_or(WAIT_FOR_DEFAULT_TIMEOUT_MS)
            .min(WAIT_FOR_MAX_TIMEOUT_MS),
    )
}

fn wait_for_has_predicate(params: &WaitForParams) -> bool {
    !params.selector().is_empty()
        || trimmed_nonempty(params.window_title.as_deref()).is_some()
        || params.focused_window.is_some()
}

fn title_contains(title: Option<&str>, needle: &str) -> bool {
    title.is_some_and(|title| normalized_contains(Some(title), needle))
}

fn node_has_state(node: &AccessibilityNode, state: &str) -> bool {
    node.states
        .iter()
        .any(|node_state| normalized_equals(node_state, state))
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct GetAppStateOutput {
    app_name_or_bundle_identifier: Option<String>,
    window_context: Option<WindowInfo>,
    /// How `window_context` was found; absent when there is no window.
    #[serde(skip_serializing_if = "Option::is_none")]
    window_context_source: Option<WindowContextSource>,
    window_error: Option<String>,
    window_permissions_hint: Option<String>,
    backend: String,
    screenshot: Option<ScreenshotCapture>,
    screenshot_error: Option<String>,
    accessibility_tree: Vec<AccessibilityNode>,
    accessibility_tree_raw_count: usize,
    accessibility_error: Option<String>,
    /// Compact readiness summary (always present).
    readiness: crate::diagnostics::ReadinessReport,
    /// Full diagnostics; populated only when verbose=true.
    #[serde(skip_serializing_if = "Option::is_none")]
    diagnostics: Option<DoctorReport>,
    message: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ClickParams {
    #[serde(default)]
    element_index: Option<u32>,
    /// The `object_ref` string of a node from the latest `get_app_state` result,
    /// as an alternative to `element_index`.
    #[serde(default)]
    object_ref: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    #[serde(default)]
    button: Option<String>,
    #[serde(default)]
    click_count: Option<u32>,
    /// Modifier keys held around the pointer click (ctrl/alt/shift/meta, the
    /// `press_key` names). Forces the pointer path even when the element has an
    /// AT-SPI click action.
    #[serde(default)]
    modifiers: Vec<String>,
    // Optional window target: when set, the window is raised/focused before the
    // click so a coordinate click reliably lands on the intended app rather than
    // whatever window happens to be stacked on top at that pixel.
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// Interpret `x`/`y` as relative to the targeted window's top-left corner
    /// (the same coordinate space as a window-cropped `screenshot`). Requires a
    /// window target; ignored otherwise.
    #[serde(default)]
    relative: Option<bool>,
}

impl ClickParams {
    fn window_target(&self) -> Option<WindowTarget> {
        self.target.optional_target()
    }

    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ActionParams {
    #[serde(default)]
    element_index: Option<u32>,
    /// The `object_ref` string of a node from the latest `get_app_state` result,
    /// as an alternative to `element_index`.
    #[serde(default)]
    object_ref: Option<String>,
    #[serde(default)]
    element_identifier: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    #[serde(default)]
    action: Option<String>,
}

impl ActionParams {
    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct SetValueParams {
    #[serde(default)]
    element_index: Option<u32>,
    /// The `object_ref` string of a node from the latest `get_app_state` result,
    /// as an alternative to `element_index`.
    #[serde(default)]
    object_ref: Option<String>,
    #[serde(default)]
    element_identifier: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    states: Vec<String>,
    value: String,
}

impl SetValueParams {
    fn selector(&self) -> ElementSelector<'_> {
        ElementSelector {
            role: self.role.as_deref(),
            name: self.name.as_deref(),
            text: self.text.as_deref(),
            states: &self.states,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct ScrollParams {
    #[serde(default)]
    element_index: Option<u32>,
    #[serde(default)]
    x: Option<i32>,
    #[serde(default)]
    y: Option<i32>,
    direction: String,
    #[serde(default)]
    pages: Option<f64>,
    // Optional window target (parity with click): the window is raised/focused
    // before scrolling so the wheel events land on the intended app.
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// Interpret `x`/`y` as relative to the targeted window's top-left corner
    /// (the same coordinate space as a window-cropped `screenshot`). Requires a
    /// window target; ignored otherwise.
    #[serde(default)]
    relative: Option<bool>,
}

impl ScrollParams {
    fn window_target(&self) -> Option<WindowTarget> {
        self.target.optional_target()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct DragParams {
    /// Drag origin in desktop pixels, or window-relative with a window target
    /// and `relative: true`. Omit when `start_element_index` names the origin.
    #[serde(default)]
    start_x: Option<i32>,
    #[serde(default)]
    start_y: Option<i32>,
    /// Drag destination, in the same space as the origin. Omit when
    /// `end_element_index` names it.
    #[serde(default)]
    end_x: Option<i32>,
    #[serde(default)]
    end_y: Option<i32>,
    /// Drag from the center of this element, from the latest `get_app_state`
    /// tree, instead of `start_x`/`start_y`.
    #[serde(default)]
    start_element_index: Option<u32>,
    /// Drag to the center of this element instead of `end_x`/`end_y`.
    #[serde(default)]
    end_element_index: Option<u32>,
    /// Modifier keys held around the drag (ctrl/alt/shift/meta, the `press_key`
    /// names).
    #[serde(default)]
    modifiers: Vec<String>,
    /// Interpret the coordinates as relative to the targeted window's top-left
    /// corner, the same space as a window-cropped `screenshot`. Requires a
    /// window target.
    #[serde(default)]
    relative: Option<bool>,
    // Optional window target: the window is raised/focused before the drag, so
    // it lands on the intended app rather than whatever is stacked on top.
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
}

impl DragParams {
    fn window_target(&self) -> Option<WindowTarget> {
        self.target.optional_target()
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct PressKeyParams {
    /// One key or chord (see the tool description for the grammar).
    #[serde(default)]
    key: Option<String>,
    /// A sequence of keys or chords sent in order with a short delay between
    /// them. Exactly one of `key` and `keys` must be given.
    #[serde(default)]
    keys: Vec<String>,
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct TypeTextParams {
    text: String,
    /// Which window to act on: `window_id`, pid, `app_id`, `wm_class`, title, or a
    /// terminal selector (tty, `terminal_pid`, `terminal_command`, `terminal_cwd`).
    #[serde(flatten)]
    target: ActivateWindowParams,
}

impl PressKeyParams {
    fn window_target(&self) -> WindowTarget {
        self.target.to_target()
    }
}

impl TypeTextParams {
    fn window_target(&self) -> WindowTarget {
        self.target.to_target()
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct ActionOutput {
    ok: bool,
    implemented: bool,
    action: String,
    message: String,
    // See ActivateWindowOutput: kept in the response, omitted from the schema
    // because `serde_json::Value` produces a non-object schema strict MCP
    // clients reject.
    #[schemars(skip)]
    received: Option<serde_json::Value>,
}

impl ComputerUseLinux {
    fn is_wayland_session() -> bool {
        crate::diagnostics::hydrate_session_bus_env();
        let session_type = env::var("XDG_SESSION_TYPE").ok();
        let wayland_display = env::var("WAYLAND_DISPLAY").ok();
        session_is_wayland(session_type.as_deref(), wayland_display.as_deref())
    }

    fn should_prefer_wtype_keyboard() -> bool {
        prefer_wtype_keyboard(
            env_flag_enabled("COMPUTER_USE_HYPRLAND_FORCE_YDOTOOL_KEYBOARD"),
            Self::is_wayland_session(),
            crate::diagnostics::wtype_compatible_wayland_desktop(
                env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
            ),
            wtype_available(),
        )
    }

    async fn resolve_window_context(
        &self,
        params: &GetAppStateParams,
    ) -> (Option<WindowInfo>, Option<String>, Option<String>) {
        let target = params.window_target();
        if !target.has_target() {
            return (None, None, None);
        }

        match list_windows().await {
            Ok(windows) => match resolve_window_target(&windows, &target) {
                Ok(window) => (Some(window.clone()), None, None),
                Err(error) => (None, Some(format!("{error:#}")), None),
            },
            Err(error) => {
                let error = format!("{error:#}");
                let hint = window_permission_hint(&error);
                (None, Some(error), hint)
            }
        }
    }

    async fn resolve_screenshot_window(
        &self,
        target: &WindowTarget,
        raise_window: bool,
    ) -> Result<WindowInfo> {
        let window = if raise_window {
            let focus = focus_window_target(target).await?;
            if !focus.exact_window_focused {
                anyhow::bail!(
                    "the requested window could not be focused exactly; refusing to capture unrelated desktop pixels"
                );
            }
            sleep(Duration::from_millis(250)).await;
            focus
                .focused_window
                .filter(|window| window.window_id == focus.requested_window.window_id)
                .ok_or_else(|| anyhow::anyhow!("focused-window verification returned no window"))?
        } else {
            // The capture is a full-output frame cropped afterwards, so an
            // unfocused window can be captured; whatever sits above it is
            // reported as `occluded_by` instead of refusing.
            let windows = list_windows().await?;
            resolve_window_target(&windows, target)?.clone()
        };
        if window.hidden {
            anyhow::bail!("the requested window is hidden or minimized");
        }
        Ok(window)
    }

    fn window_crop_rect_for_capture(
        window: &WindowInfo,
        raw: &RawScreenshotCapture,
    ) -> Result<(i32, i32, u32, u32)> {
        Self::window_crop_rect_for_dimensions(window, raw.width, raw.height)
    }

    fn window_crop_rect_for_dimensions(
        window: &WindowInfo,
        capture_width: u32,
        capture_height: u32,
    ) -> Result<(i32, i32, u32, u32)> {
        Ok(
            Self::window_coordinate_map_for_dimensions(window, capture_width, capture_height)?
                .capture_rect,
        )
    }

    fn window_coordinate_map_for_dimensions(
        window: &WindowInfo,
        capture_width: u32,
        capture_height: u32,
    ) -> Result<WindowCoordinateMap> {
        let bounds = window.bounds.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "targeted screenshot requires window bounds; refusing to return the full desktop"
            )
        })?;
        let logical_rect = window_crop_rect(bounds).ok_or_else(|| {
            anyhow::anyhow!(
                "targeted screenshot has unusable window bounds; refusing to return the full desktop"
            )
        })?;
        Ok(WindowCoordinateMap {
            capture_rect: clip_capture_rect(logical_rect, capture_width, capture_height)?,
        })
    }

    fn focused_window_coordinate_map(
        &self,
        focus: &WindowFocusResult,
    ) -> std::result::Result<WindowCoordinateMap, String> {
        let window = focus
            .focused_window
            .as_ref()
            .unwrap_or(&focus.requested_window);
        let full_capture_rect = window
            .bounds
            .as_ref()
            .and_then(window_crop_rect)
            .ok_or_else(|| {
                "Window-relative coordinates require usable target-window bounds.".to_string()
            })?;
        let capture_rect = self
            .desktop_size
            .lock()
            .ok()
            .and_then(|guard| *guard)
            .map(|(width, height)| {
                clip_capture_rect(full_capture_rect, width, height)
                    .map_err(|error| format!("Could not map target-window coordinates: {error:#}"))
            })
            .transpose()?
            .unwrap_or(full_capture_rect);
        Ok(WindowCoordinateMap { capture_rect })
    }

    async fn resolve_accessibility_app_filter(
        &self,
        params: &GetAppStateParams,
        window_context: Option<&WindowInfo>,
    ) -> Option<String> {
        if let Some(explicit) = trimmed_nonempty(params.app_name_or_bundle_identifier.as_deref()) {
            return Some(explicit.to_string());
        }

        let target_pid = window_context
            .and_then(|window| window.pid)
            .or(params.target.pid);
        let candidates = accessibility_filter_candidates(window_context);

        if let Some(target_pid) = target_pid
            && let Ok(apps) = list_accessible_apps(200).await
            && let Some(object_ref) =
                select_accessibility_object_ref(&apps, target_pid, &candidates)
        {
            return Some(object_ref);
        }

        candidates.into_iter().next()
    }

    /// Every input tool passes here first: the machine-wide session lock, then
    /// the `COMPUTER_USE_HYPRLAND_ALLOWED_APPS` check against the window the
    /// action targets (the focused window when it targets none). The guard it
    /// returns is this operation's hold on the lock: keep it alive until the
    /// input has been sent, or the idle timer may free the lock half-way.
    async fn input_gate(
        &self,
        action: &str,
        target: Option<&WindowTarget>,
    ) -> std::result::Result<crate::session_lock::InputLeaseGuard, String> {
        let lease = crate::session_lock::acquire_input_lock()?;
        let Some(patterns) = allowed_app_patterns(env::var(ALLOWED_APPS_ENV).ok().as_deref())
        else {
            return Ok(lease);
        };
        let window = match target.filter(|target| target.has_target()) {
            Some(target) => {
                let windows = list_windows().await.map_err(|error| {
                    format!("Refused {action}: {ALLOWED_APPS_ENV} is set but the window list is unavailable: {error:#}")
                })?;
                resolve_window_target(&windows, target)
                    .map_err(|error| format!("Refused {action}: {error:#}"))?
                    .clone()
            }
            None => focused_window()
                .await
                .map_err(|error| {
                    format!("Refused {action}: {ALLOWED_APPS_ENV} is set but the focused window is unknown: {error:#}")
                })?
                .ok_or_else(|| {
                    format!("Refused {action}: {ALLOWED_APPS_ENV} is set and no window holds focus.")
                })?,
        };
        if window_matches_allowlist(&window, &patterns) {
            Ok(lease)
        } else {
            Err(format!(
                "Refused {action}: window_id {} (app_id {:?}, wm_class {:?}, title {:?}) matches none of the {ALLOWED_APPS_ENV} patterns [{}].",
                window.window_id,
                window.app_id.as_deref().unwrap_or(""),
                window.wm_class.as_deref().unwrap_or(""),
                window.title.as_deref().unwrap_or(""),
                patterns.join(", ")
            ))
        }
    }

    async fn focus_target_for_input(
        &self,
        target: &WindowTarget,
    ) -> std::result::Result<Option<WindowFocusResult>, String> {
        if !target.has_target() {
            return Ok(None);
        }

        let focus = focus_window_target(target).await.map_err(|error| {
            let error = format!("{error:#}");
            if let Some(hint) = window_permission_hint(&error) {
                format!("Did not send input because the target window could not be focused: {error}. {hint}")
            } else {
                format!("Did not send input because the target window could not be focused: {error}")
            }
        })?;

        if focus_satisfies_target(&focus, target) {
            Ok(Some(focus))
        } else {
            let required = if target.requires_exact_focus() {
                "exact target-window focus"
            } else {
                "app-level focus"
            };
            Err(format!(
                "Did not send input because {required} verification failed after activating the target window. Focus result: requested window_id {}, focused window_id {:?}.",
                focus.requested_window.window_id,
                focus.focused_window.as_ref().map(|window| window.window_id)
            ))
        }
    }

    /// One evaluation of every `wait_for` predicate. Stops at the first one
    /// that does not hold and says why; a satisfied probe carries the matched
    /// element and has already cached the tree it came from.
    #[expect(
        clippy::too_many_lines,
        reason = "one predicate after another, each stopping the probe; the shared WaitProbe is what makes them one function"
    )]
    async fn probe_wait_predicates(
        &self,
        params: &WaitForParams,
        app_state_params: &GetAppStateParams,
        selector: &ElementSelector<'_>,
    ) -> WaitProbe {
        let mut probe = WaitProbe::default();
        let target = app_state_params.window_target();
        let (window_context, window_error, _) = self.resolve_window_context(app_state_params).await;
        if target.has_target() && window_context.is_none() {
            probe.error = window_error.or_else(|| "target window not found".to_string().into());
            return probe;
        }
        probe.window_context = window_context.clone();
        probe.window_context_source = window_context_source(window_context.as_ref(), None);

        if let Some(needle) = trimmed_nonempty(params.window_title.as_deref()) {
            let title_window = match window_context.as_ref() {
                Some(window) => Some(window.clone()),
                None => focused_window().await.ok().flatten(),
            };
            let title = title_window
                .as_ref()
                .and_then(|window| window.title.as_deref());
            if !title_contains(title, needle) {
                probe.error = Some(format!(
                    "window title {} does not contain {needle:?}",
                    title.map_or_else(|| "(none)".to_string(), |title| format!("{title:?}"))
                ));
                return probe;
            }
        }

        if let Some(selector) = params.focused_window.as_ref() {
            let target = selector.clone().into_target();
            let windows = match list_windows().await {
                Ok(windows) => windows,
                Err(error) => {
                    probe.error = Some(format!("window listing failed: {error:#}"));
                    return probe;
                }
            };
            match resolve_window_target(&windows, &target) {
                Ok(window) if window.focused => probe.focused_window = Some(window.clone()),
                Ok(window) => {
                    probe.error = Some(format!(
                        "window_id {} ({}) does not hold focus",
                        window.window_id,
                        window.title.as_deref().unwrap_or("untitled")
                    ));
                    return probe;
                }
                Err(error) => {
                    probe.error = Some(format!("{error:#}"));
                    return probe;
                }
            }
        }

        if !selector.is_empty() {
            let app_filter = self
                .resolve_accessibility_app_filter(app_state_params, window_context.as_ref())
                .await;
            let (max_nodes, max_depth) = snapshot_limits(params.max_nodes, params.max_depth);
            let target_pid = window_context
                .as_ref()
                .and_then(|window| window.pid)
                .or(params.target.pid);
            let snapshot = match snapshot_tree(
                app_filter.as_deref(),
                target_pid,
                max_nodes,
                max_depth,
            )
            .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    probe.error = Some(format!("AT-SPI tree extraction failed: {error:#}"));
                    return probe;
                }
            };
            let raw_count = snapshot.nodes.len();
            let mut nodes = compact_accessibility_tree(snapshot.nodes);
            self.apply_stable_indices(&mut nodes);
            let matches = nodes
                .iter()
                .filter(|node| node_matches_selector(node, selector))
                .filter(|node| !params.focused.unwrap_or(false) || node_has_state(node, "focused"))
                .cloned()
                .collect::<Vec<_>>();
            probe.summary = Some(format!(
                "last tree: {} nodes (compacted from {raw_count}), {} matching {}",
                nodes.len(),
                matches.len(),
                describe_selector(selector)
            ));
            let Some(element) =
                resolve_semantic_node(&matches, selector, ElementResolvePurpose::Action)
                    .ok()
                    .or_else(|| matches.first().cloned())
            else {
                probe.error = Some(format!(
                    "no element matched {}{}",
                    describe_selector(selector),
                    if params.focused.unwrap_or(false) {
                        " with focus"
                    } else {
                        ""
                    }
                ));
                return probe;
            };
            let bounds_window = match window_context.clone() {
                Some(window) => Some(window),
                None => {
                    self.window_for_untargeted_tree(&nodes, &snapshot.root_pids)
                        .await
                }
            };
            self.cache_tree(&nodes, bounds_window.as_ref());
            probe.window_context_source =
                window_context_source(window_context.as_ref(), bounds_window.as_ref());
            probe.window_context = bounds_window;
            probe.element = Some(element);
        }

        probe.satisfied = true;
        probe
    }

    fn cache_desktop_size(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if let Ok(mut guard) = self.desktop_size.lock() {
            *guard = Some((width, height));
        }
    }

    /// Physical capture-space desktop rectangle (union of monitors as captured
    /// by the screenshot pipeline), for checks against click coordinates.
    /// Best-effort; None disables the check.
    async fn capture_space_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let cached = self.desktop_size.lock().ok().and_then(|guard| *guard);
        if let Some((w, h)) = cached {
            return Some((0, 0, i32::try_from(w).ok()?, i32::try_from(h).ok()?));
        }
        // One-time prime: a full-frame capture reveals the desktop size when
        // no prior capture is available.
        let raw = capture_screenshot_raw().await.ok()?;
        self.cache_desktop_size(raw.width, raw.height);
        (raw.width > 0 && raw.height > 0).then_some((
            0,
            0,
            i32::try_from(raw.width).ok()?,
            i32::try_from(raw.height).ok()?,
        ))
    }

    /// Warn when a targeted window pokes outside every monitor: clicks and
    /// screenshots silently truncate to visible pixels there, which reads as
    /// "success" while landing nowhere.
    async fn off_screen_note_for_bounds(
        &self,
        bounds: &crate::windowing::WindowBounds,
    ) -> Option<String> {
        let (x, y) = bounds.x.zip(bounds.y)?;
        if bounds.width == 0 || bounds.height == 0 {
            return None;
        }
        // Hyprland window bounds are already mapped into capture space by the
        // backend, so the captured desktop rectangle is the right yardstick.
        let rects = vec![self.capture_space_rect().await?];
        let (w, h) = (i64::from(bounds.width), i64::from(bounds.height));
        let window_area = w * h;
        let mut visible_area = 0_i64;
        for (mx, my, mw, mh) in &rects {
            let ix = i64::from(x).max(i64::from(*mx));
            let iy = i64::from(y).max(i64::from(*my));
            let ix2 = (i64::from(x) + w).min(i64::from(*mx) + i64::from(*mw));
            let iy2 = (i64::from(y) + h).min(i64::from(*my) + i64::from(*mh));
            if ix2 > ix && iy2 > iy {
                // Overlapping monitors are rare; treating them as additive keeps
                // this a cheap best-effort heuristic.
                visible_area += (ix2 - ix) * (iy2 - iy);
            }
        }
        let visible_pct = (visible_area.min(window_area) * 100) / window_area.max(1);
        if visible_pct >= 100 {
            return None;
        }
        Some(format!(
            "WARNING: the target window (bounds {x},{y} {w}x{h}) is only ~{visible_pct}% on-screen; off-screen regions are missing from screenshots and unreachable by coordinate input. Use move_window/resize_window to bring it fully on-screen."
        ))
    }

    /// Warn when a click/scroll coordinate is outside the captured desktop.
    /// Click coordinates are physical capture-space pixels, so compare ONLY
    /// against the capture rect — the extension's logical layout is a
    /// different space on scaled displays and would false-positive.
    async fn off_screen_note_for_point(&self, x: i32, y: i32) -> Option<String> {
        let (mx, my, mw, mh) = self.capture_space_rect().await?;
        let visible = x >= mx && y >= my && x < mx.saturating_add(mw) && y < my.saturating_add(mh);
        if visible {
            return None;
        }
        Some(format!(
            "WARNING: coordinate {x},{y} is outside the captured desktop ({mw}x{mh}); the input landed on no visible pixel."
        ))
    }

    /// Post-input feedback: which AT-SPI element holds keyboard focus in the
    /// target app, and whether it is editable. Guards against the blind-typing
    /// trap where verified *window* focus still sends keystrokes nowhere.
    async fn focused_element_feedback(
        &self,
        focus: Option<&WindowFocusResult>,
        expects_editable: bool,
    ) -> Option<String> {
        let pid = focus.and_then(|focus| {
            focus
                .focused_window
                .as_ref()
                .and_then(|window| window.pid)
                .or(focus.requested_window.pid)
        });
        match timeout(Duration::from_millis(1500), focused_element_summary(pid)).await {
            Ok(Ok(Some(element))) => Some(describe_focused_element(&element, expects_editable)),
            Ok(Ok(None)) => Some(
                "WARNING: AT-SPI reports no focused element in the target app — the input may have landed nowhere. If this is an Electron app, launch it with --force-renderer-accessibility to expose its UI tree."
                    .to_string(),
            ),
            Ok(Err(error)) => Some(format!(
                "Focused-element feedback unavailable ({}).",
                first_line(&format!("{error:#}"))
            )),
            Err(_) => Some("Focused-element feedback unavailable (AT-SPI probe timed out).".to_string()),
        }
    }

    /// Shared move/resize plumbing: resolve the window target, dispatch the
    /// geometry change through hyprctl, then re-query the bounds to report
    /// what actually happened rather than what was asked for.
    async fn window_geometry_op<F, Fut>(
        &self,
        received: Option<serde_json::Value>,
        target: &WindowTarget,
        op: F,
    ) -> Json<WindowGeometryOutput>
    where
        F: FnOnce(crate::windowing::WindowInfo) -> Fut,
        Fut: Future<Output = Result<String>>,
    {
        let _input_lease = match self
            .input_gate("move_window/resize_window", Some(target))
            .await
        {
            Ok(lease) => lease,
            Err(message) => {
                return Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: "unknown".to_string(),
                    window: None,
                    message,
                    permissions_hint: None,
                    received,
                });
            }
        };
        let windows = match list_windows().await {
            Ok(windows) => windows,
            Err(error) => {
                let error = format!("{error:#}");
                return Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: "unknown".to_string(),
                    window: None,
                    message: format!("Window listing failed: {error}"),
                    permissions_hint: window_permission_hint(&error),
                    received,
                });
            }
        };
        let window = match resolve_window_target(&windows, target) {
            Ok(window) => window.clone(),
            Err(error) => {
                return Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend: "unknown".to_string(),
                    window: None,
                    message: format!("{error:#}"),
                    permissions_hint: None,
                    received,
                });
            }
        };
        let backend = window.backend.clone();
        let window_id = window.window_id;
        match op(window).await {
            Ok(message) => {
                // Re-query so the caller sees the compositor-final geometry
                // (tiling constraints, minimum sizes, etc. may adjust it).
                let window = list_windows().await.ok().and_then(|windows| {
                    windows
                        .into_iter()
                        .find(|window| window.window_id == window_id)
                });
                let mut message = message;
                if let Some(bounds) = window.as_ref().and_then(|window| window.bounds.as_ref())
                    && let Some(note) = self.off_screen_note_for_bounds(bounds).await
                {
                    message = format!("{message} {note}");
                }
                Json(WindowGeometryOutput {
                    ok: true,
                    implemented: true,
                    backend,
                    window,
                    message,
                    permissions_hint: None,
                    received,
                })
            }
            Err(error) => {
                let error = format!("{error:#}");
                Json(WindowGeometryOutput {
                    ok: false,
                    implemented: true,
                    backend,
                    window: None,
                    permissions_hint: window_permission_hint(&error),
                    message: error,
                    received,
                })
            }
        }
    }

    /// Where the compositor says the pointer actually is, when that is not
    /// where the pointer backend was told to put it.
    ///
    /// The check exists because a lost motion event is invisible otherwise:
    /// the backend reports success, and the button lands wherever the cursor
    /// happened to be. Costs one `hyprctl` round trip per pointer action, and
    /// stays quiet when the two agree or when the compositor cannot be asked.
    async fn pointer_landing_note(&self, expected: (i32, i32)) -> Option<String> {
        let ((actual_x, actual_y), _) = registry::pointer_position().await.ok()??;
        let (expected_x, expected_y) = expected;
        let off_by = (actual_x - expected_x)
            .abs()
            .max((actual_y - expected_y).abs());
        (off_by > POINTER_LANDING_TOLERANCE).then(|| {
            format!(
                "WARNING: the compositor reports the pointer at ({actual_x}, {actual_y}), not the ({expected_x}, {expected_y}) this action asked for, so it landed there instead. Something moved the cursor after the motion was emitted."
            )
        })
    }

    /// Notes appended after targeted keyboard input: off-screen window warning
    /// plus focused-element feedback.
    async fn input_landing_notes(
        &self,
        focus: Option<&WindowFocusResult>,
        expects_editable: bool,
    ) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(focus) = focus {
            let bounds = focus
                .focused_window
                .as_ref()
                .and_then(|window| window.bounds.as_ref())
                .or(focus.requested_window.bounds.as_ref());
            if let Some(bounds) = bounds
                && let Some(note) = self.off_screen_note_for_bounds(bounds).await
            {
                notes.push(note);
            }
        }
        if let Some(note) = self.focused_element_feedback(focus, expects_editable).await {
            notes.push(note);
        }
        notes
    }

    /// Feedback appended after an action landed: the focused element (as
    /// `press_key` reports it) and, for an element action, the states that
    /// changed on that element.
    async fn post_action_notes(
        &self,
        focus: Option<&WindowFocusResult>,
        element: Option<(&str, &[String])>,
    ) -> Vec<String> {
        // The app handles the event asynchronously; read the focus and the
        // states after it has had a moment, or the previous state is reported.
        sleep(POST_ACTION_SETTLE).await;
        let mut notes = self.input_landing_notes(focus, false).await;
        if let Some((object_ref, before)) = element
            && let Some(note) = element_states_note(object_ref, before).await
        {
            notes.push(note);
        }
        notes
    }

    /// A cached element with the `focusable` and `editable` states can take
    /// typed text even without the Value or `EditableText` interfaces.
    fn cached_node_is_keyboard_editable(&self, object_ref: &str) -> bool {
        let states = self.cached_node_states(object_ref);
        ["focusable", "editable"]
            .iter()
            .all(|wanted| states.iter().any(|state| normalized_equals(state, wanted)))
    }

    /// `set_value` through the keyboard: focus the element with AT-SPI
    /// `GrabFocus`, select everything with Ctrl+A, then type the value.
    async fn keyboard_set_value(
        &self,
        object_ref: &str,
        value: &str,
        received: Option<serde_json::Value>,
    ) -> Json<ActionOutput> {
        let fail = |message: String| Json(action_failure("set_value", message, received.clone()));
        match grab_focus(object_ref).await {
            Ok(true) => {}
            Ok(false) => {
                return fail(
                    "The element exposes neither Value nor EditableText, and AT-SPI GrabFocus returned false, so the keyboard fallback could not focus it."
                        .to_string(),
                );
            }
            Err(error) => return fail(element_error_message(&error)),
        }
        sleep(Duration::from_millis(80)).await;
        let select_all = self
            .press_key(Parameters(PressKeyParams {
                key: Some("ctrl+a".to_string()),
                ..PressKeyParams::default()
            }))
            .await;
        if !select_all.0.ok {
            return fail(format!(
                "Keyboard fallback failed at select-all: {}",
                select_all.0.message
            ));
        }
        let typed = self
            .type_text(Parameters(TypeTextParams {
                text: value.to_string(),
                ..TypeTextParams::default()
            }))
            .await;
        if !typed.0.ok {
            return fail(format!(
                "Keyboard fallback failed while typing: {}",
                typed.0.message
            ));
        }
        Json(ActionOutput {
            ok: true,
            implemented: true,
            action: "set_value".to_string(),
            message: format!(
                "The element exposes neither Value nor EditableText, so a keyboard fallback was used: AT-SPI GrabFocus, Ctrl+A, then typed the value. {}",
                typed.0.message
            ),
            received,
        })
    }

    /// The cached element's AT-SPI action that scrolls in `direction`, if it
    /// exposes one (GTK scrolled windows and web views name them "scroll down"
    /// and so on).
    fn cached_scroll_action(
        &self,
        element_index: u32,
        direction: ScrollDirection,
    ) -> Option<(String, AccessibilityAction)> {
        let cached = self.last_nodes.lock().ok()?;
        let node = cached.iter().find(|node| node.index == element_index)?;
        let action = scroll_action_for_direction(&node.actions, direction)?;
        Some((node.object_ref.clone(), action.clone()))
    }

    fn cached_node_states(&self, object_ref: &str) -> Vec<String> {
        self.last_nodes
            .lock()
            .ok()
            .and_then(|cached| {
                cached
                    .iter()
                    .find(|node| node.object_ref == object_ref)
                    .map(|node| node.states.clone())
            })
            .unwrap_or_default()
    }

    /// Renumber a freshly compacted tree so every node keeps the index it was
    /// given the last time it was read, and `parent_index` follows.
    fn apply_stable_indices(&self, nodes: &mut [AccessibilityNode]) {
        let Ok(mut indices) = self.element_indices.lock() else {
            return;
        };
        let assigned = nodes
            .iter()
            .map(|node| indices.index_for(&node.object_ref))
            .collect::<Vec<_>>();
        for (node, index) in nodes.iter_mut().zip(&assigned) {
            // Read before the write: `parent_index` still holds the position
            // the compaction gave it, which is this node's index into
            // `assigned`.
            node.parent_index = node
                .parent_index
                .and_then(|parent| assigned.get(parent as usize).copied());
            node.index = *index;
        }
    }

    #[cfg(test)]
    fn cache_nodes(&self, nodes: &[AccessibilityNode]) {
        self.cache_tree(nodes, None);
    }

    fn cache_tree(&self, nodes: &[AccessibilityNode], window: Option<&WindowInfo>) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
            cached.extend_from_slice(nodes);
        }
        if let Ok(mut bounds) = self.node_bounds.lock() {
            *bounds = cached_bounds_for(nodes, window);
        }
    }

    /// The compositor window a tree fetched without a window target belongs
    /// to, so its window-relative bounds can still be offset.
    ///
    /// Only asked when the bounds need it, because listing windows costs a
    /// compositor round trip on every `get_app_state`.
    async fn window_for_untargeted_tree(
        &self,
        nodes: &[AccessibilityNode],
        root_pids: &[u32],
    ) -> Option<WindowInfo> {
        if root_pids.is_empty() || !bounds_are_window_relative(nodes) {
            return None;
        }
        let windows = list_windows().await.ok()?;
        sole_window_for_pids(&windows, root_pids).cloned()
    }

    fn clear_cached_nodes(&self) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
        }
        if let Ok(mut bounds) = self.node_bounds.lock() {
            *bounds = CachedBounds::Desktop;
        }
    }

    #[cfg(test)]
    fn cached_bounds_offset(&self) -> Option<(i32, i32)> {
        self.node_bounds
            .lock()
            .ok()
            .and_then(|bounds| bounds.offset())
    }

    /// How the cached node bounds map onto the desktop right now: the window's
    /// current origin from the compositor when the window can be found (it
    /// may have moved since the tree was cached), the cached offset otherwise.
    async fn current_bounds(&self) -> CachedBounds {
        let cached = self
            .node_bounds
            .lock()
            .ok()
            .map(|bounds| bounds.clone())
            .unwrap_or_default();
        let CachedBounds::Window(cached) = cached else {
            return cached;
        };
        let windows = list_windows().await.unwrap_or_default();
        CachedBounds::Window(BoundsOffset {
            offset: fresh_bounds_offset(&cached, &windows),
            ..cached
        })
    }

    /// Center of a cached node's bounds in desktop coordinates, with the
    /// window-origin offset applied when the tree reported window-relative
    /// bounds.
    fn desktop_center_for_node(
        node: &AccessibilityNode,
        offset: Option<(i32, i32)>,
    ) -> Option<(i32, i32)> {
        let (x, y) = bounds_center(node.bounds.as_ref()?)?;
        let (dx, dy) = offset.unwrap_or((0, 0));
        Some((x.checked_add(dx)?, y.checked_add(dy)?))
    }

    fn resolve_optional_target_point(
        &self,
        x: Option<i32>,
        y: Option<i32>,
        element_index: Option<u32>,
        offset: Option<(i32, i32)>,
    ) -> std::result::Result<Option<(i32, i32)>, String> {
        match (x.zip(y), element_index) {
            (Some(point), _) => Ok(Some(point)),
            (None, Some(index)) => self
                .center_for_cached_node(index, offset)
                .map(Some)
                .ok_or_else(|| {
                    format!(
                        "No clickable bounds cached for element_index {index}. Call get_app_state first and choose a node with positive width and height."
                    )
                }),
            (None, None) => Ok(None),
        }
    }

    fn resolve_click_target(
        &self,
        params: &ClickParams,
        offset: Option<(i32, i32)>,
    ) -> std::result::Result<ClickTarget, String> {
        if let Some((x, y)) = params.x.zip(params.y) {
            return Ok(ClickTarget::Coordinates(x, y));
        }

        let selector = params.selector();
        let node = self.resolve_cached_node(
            params.element_index,
            params.object_ref.as_deref(),
            &selector,
            ElementResolvePurpose::Click,
        )?;

        let point = Self::desktop_center_for_node(&node, offset);
        let plain_click = is_plain_left_click(params.button.as_deref(), params.click_count);

        // A plain left click is what the element's own `click` action does,
        // and the action does not depend on where the window sits on the
        // desktop, so it is tried first and the pointer is the fallback.
        // Without bounds, any primary action is better than nothing.
        let action = if plain_click {
            click_action(node.actions.as_slice()).or_else(|| {
                point
                    .is_none()
                    .then(|| primary_action(node.actions.as_slice()))
                    .flatten()
            })
        } else {
            None
        };

        if point.is_none() && action.is_none() {
            return Err(if plain_click {
                format!(
                    "No clickable bounds cached for element_index {}, and the element exposes no primary AT-SPI action.",
                    node.index
                )
            } else {
                format!(
                    "No clickable bounds cached for element_index {}. Call get_app_state first and choose a node with positive width and height.",
                    node.index
                )
            });
        }

        Ok(ClickTarget::Element {
            element_index: node.index,
            label: describe_cached_node(&node),
            object_ref: node.object_ref.clone(),
            action: action.cloned(),
            point,
            bounds_offset: point.and(offset).filter(|offset| *offset != (0, 0)),
            states: node.states,
        })
    }

    /// How the cached tree describes an element, for a note that says what
    /// was acted on and not only where it was.
    fn cached_node_label(&self, element_index: u32) -> Option<String> {
        let cached = self.last_nodes.lock().ok()?;
        cached
            .iter()
            .find(|node| node.index == element_index)
            .map(describe_cached_node)
    }

    fn center_for_cached_node(
        &self,
        element_index: u32,
        offset: Option<(i32, i32)>,
    ) -> Option<(i32, i32)> {
        let cached = self.last_nodes.lock().ok()?;
        let node = cached.iter().find(|node| node.index == element_index)?;
        Self::desktop_center_for_node(node, offset)
    }

    fn resolve_object_ref(
        &self,
        element_index: Option<u32>,
        element_identifier: Option<&str>,
        selector: &ElementSelector<'_>,
        purpose: ElementResolvePurpose,
    ) -> std::result::Result<String, String> {
        if let Some(element_identifier) = element_identifier
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return Ok(element_identifier.to_string());
        }

        self.resolve_cached_node(element_index, None, selector, purpose)
            .map(|node| node.object_ref)
    }

    fn resolve_cached_node(
        &self,
        element_index: Option<u32>,
        object_ref: Option<&str>,
        selector: &ElementSelector<'_>,
        purpose: ElementResolvePurpose,
    ) -> std::result::Result<AccessibilityNode, String> {
        #[expect(
            clippy::map_err_ignore,
            reason = "the discarded error is a PoisonError holding the guard; the caller needs the retry instruction, not it"
        )]
        let cached = self.last_nodes.lock().map_err(|_| {
            "Could not read cached accessibility nodes. Call get_app_state and retry.".to_string()
        })?;

        if let Some(object_ref) = object_ref.map(str::trim).filter(|value| !value.is_empty()) {
            return cached
                .iter()
                .find(|node| node.object_ref == object_ref)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "No cached accessibility node with object_ref {object_ref}. Call get_app_state first."
                    )
                });
        }

        if let Some(element_index) = element_index {
            return cached
                .iter()
                .find(|node| node.index == element_index)
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "No element_index {element_index} in the tree as it was last read. An index names the same element until that element goes away, so this one is gone: the view changed, or the application restarted. Call get_app_state or wait_for and read the index again."
                    )
                });
        }

        if selector.is_empty() {
            return Err(
                "Pass element_index, element_identifier, or a semantic selector such as role/name/text/states from the latest get_app_state result."
                    .to_string(),
            );
        }

        resolve_semantic_node(cached.as_slice(), selector, purpose)
    }

    async fn perform_element_action(
        &self,
        params: &ActionParams,
        requested_action: Option<&str>,
    ) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        let object_ref = match self.resolve_object_ref(
            params.element_index,
            params
                .element_identifier
                .as_deref()
                .or(params.object_ref.as_deref()),
            &params.selector(),
            ElementResolvePurpose::Action,
        ) {
            Ok(object_ref) => object_ref,
            Err(message) => {
                return Json(action_failure("perform_action", message, received));
            }
        };

        let states_before = self.cached_node_states(&object_ref);
        match invoke_accessibility_action(&object_ref, requested_action).await {
            Ok(invocation) if invocation.ok => {
                let notes = self
                    .post_action_notes(None, Some((&object_ref, &states_before)))
                    .await;
                Json(with_notes(
                    ActionOutput {
                        ok: true,
                        implemented: true,
                        action: "perform_action".to_string(),
                        message: format!(
                            "AT-SPI action {} ({}) invoked.",
                            invocation.action_index,
                            invocation
                                .action_name
                                .as_deref()
                                .filter(|name| !name.is_empty())
                                .unwrap_or("unnamed")
                        ),
                        received,
                    },
                    notes,
                ))
            }
            Ok(invocation) => Json(ActionOutput {
                ok: invocation.ok,
                implemented: true,
                action: "perform_action".to_string(),
                message: if invocation.ok {
                    format!(
                        "AT-SPI action {} ({}) invoked.",
                        invocation.action_index,
                        invocation
                            .action_name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("unnamed")
                    )
                } else {
                    format!(
                        "AT-SPI action {} ({}) returned false.",
                        invocation.action_index,
                        invocation
                            .action_name
                            .as_deref()
                            .filter(|name| !name.is_empty())
                            .unwrap_or("unnamed")
                    )
                },
                received,
            }),
            Err(error) => Json(action_failure(
                "perform_action",
                element_error_message(&error),
                received,
            )),
        }
    }
}

#[derive(Debug)]
enum ClickTarget {
    Coordinates(i32, i32),
    /// An element from the cached tree: the AT-SPI action to invoke first, if
    /// any, and the desktop point for the pointer fallback, if it has bounds.
    Element {
        element_index: u32,
        /// The element in the terms the tree shows it, so a note names what
        /// was clicked and not only where.
        label: String,
        object_ref: String,
        action: Option<AccessibilityAction>,
        point: Option<(i32, i32)>,
        /// The window-origin offset folded into `point`, when one applied.
        bounds_offset: Option<(i32, i32)>,
        /// The element's states when the tree was cached, for the
        /// before -> after note.
        states: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy)]
enum ElementResolvePurpose {
    Click,
    Action,
    SetValue,
}

#[derive(Debug, Clone, Copy, Default)]
struct ElementSelector<'a> {
    role: Option<&'a str>,
    name: Option<&'a str>,
    text: Option<&'a str>,
    states: &'a [String],
}

impl ElementSelector<'_> {
    fn is_empty(&self) -> bool {
        [self.role, self.name, self.text]
            .into_iter()
            .all(|value| value.map(str::trim).is_none_or(str::is_empty))
            && self.states.iter().all(|value| value.trim().is_empty())
    }
}

fn resolve_semantic_node(
    nodes: &[AccessibilityNode],
    selector: &ElementSelector<'_>,
    purpose: ElementResolvePurpose,
) -> std::result::Result<AccessibilityNode, String> {
    let mut matches = nodes
        .iter()
        .filter(|node| node_matches_selector(node, selector))
        .collect::<Vec<_>>();

    if matches.is_empty() {
        return Err(format!(
            "No cached accessibility node matched semantic selector {}. Call get_app_state first or pass element_index.",
            describe_selector(selector)
        ));
    }

    if let Some(node) =
        unique_preferred_node(&matches, |node| node_matches_resolve_purpose(node, purpose))
    {
        return Ok(node.clone());
    }

    let useful_matches = matches
        .iter()
        .copied()
        .filter(|node| node_matches_resolve_purpose(node, purpose))
        .collect::<Vec<_>>();
    if !useful_matches.is_empty() {
        matches = useful_matches;
    }

    if let Some(node) = unique_preferred_node(&matches, node_is_showing) {
        return Ok(node.clone());
    }

    let visible_matches = matches
        .iter()
        .copied()
        .filter(|node| node_is_showing(node))
        .collect::<Vec<_>>();
    if !visible_matches.is_empty() {
        matches = visible_matches;
    }

    if matches.len() == 1 {
        return Ok(matches[0].clone());
    }

    Err(format!(
        "Semantic selector {} matched multiple cached nodes: {}. Pass element_index or add more selector fields.",
        describe_selector(selector),
        describe_matching_nodes(&matches),
    ))
}

fn unique_preferred_node<'a>(
    nodes: &[&'a AccessibilityNode],
    predicate: impl Fn(&AccessibilityNode) -> bool,
) -> Option<&'a AccessibilityNode> {
    let mut matches = nodes.iter().copied().filter(|node| predicate(node));
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn node_matches_selector(node: &AccessibilityNode, selector: &ElementSelector<'_>) -> bool {
    selector
        .role
        .is_none_or(|role| normalized_contains(Some(node.role.as_str()), role))
        && selector
            .name
            .is_none_or(|name| normalized_contains(node.name.as_deref(), name))
        && selector.text.is_none_or(|text| {
            normalized_contains(
                node.text
                    .as_ref()
                    .and_then(|value| value.content.as_deref()),
                text,
            ) || normalized_contains(node.name.as_deref(), text)
                || normalized_contains(node.description.as_deref(), text)
        })
        && selector
            .states
            .iter()
            .filter(|state| !state.trim().is_empty())
            .all(|state| {
                node.states
                    .iter()
                    .any(|node_state| normalized_equals(node_state, state))
            })
}

fn node_matches_resolve_purpose(node: &AccessibilityNode, purpose: ElementResolvePurpose) -> bool {
    match purpose {
        ElementResolvePurpose::Click => {
            node.bounds.as_ref().and_then(bounds_center).is_some()
                || primary_action_name(&node.actions).is_some()
        }
        ElementResolvePurpose::Action => !node.actions.is_empty(),
        ElementResolvePurpose::SetValue => node.supports_editable_text || node.value.is_some(),
    }
}

fn node_is_showing(node: &AccessibilityNode) -> bool {
    node.states
        .iter()
        .any(|state| normalized_equals(state, "showing"))
        && node
            .states
            .iter()
            .any(|state| normalized_equals(state, "visible"))
}

fn normalized_equals(actual: &str, expected: &str) -> bool {
    normalize_text(actual) == normalize_text(expected)
}

fn normalized_contains(actual: Option<&str>, expected: &str) -> bool {
    let expected = normalize_text(expected);
    !expected.is_empty()
        && actual
            .map(normalize_text)
            .is_some_and(|actual| actual.contains(&expected))
}

fn normalize_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// An element in the terms the tree shows it: its role, and its name when it
/// has one.
/// A workspace id the compositor reported, or a word for one it did not.
fn describe_workspace_id(workspace: Option<i32>) -> String {
    workspace.map_or_else(|| "unknown".to_string(), |id| id.to_string())
}

fn describe_cached_node(node: &AccessibilityNode) -> String {
    match trimmed_nonempty(node.name.as_deref()) {
        Some(name) => format!("{} {name:?}", node.role),
        None => node.role.clone(),
    }
}

fn describe_selector(selector: &ElementSelector<'_>) -> String {
    let mut parts = Vec::new();
    if let Some(role) = selector.role.map(str::trim).filter(|role| !role.is_empty()) {
        parts.push(format!("role={role:?}"));
    }
    if let Some(name) = selector.name.map(str::trim).filter(|name| !name.is_empty()) {
        parts.push(format!("name={name:?}"));
    }
    if let Some(text) = selector.text.map(str::trim).filter(|text| !text.is_empty()) {
        parts.push(format!("text={text:?}"));
    }
    let states = selector
        .states
        .iter()
        .map(|state| state.trim())
        .filter(|state| !state.is_empty())
        .collect::<Vec<_>>();
    if !states.is_empty() {
        parts.push(format!("states={states:?}"));
    }
    if parts.is_empty() {
        "<empty>".to_string()
    } else {
        parts.join(", ")
    }
}

fn describe_matching_nodes(nodes: &[&AccessibilityNode]) -> String {
    nodes
        .iter()
        .take(8)
        .map(|node| {
            format!(
                "element_index {} role={:?} name={:?}",
                node.index, node.role, node.name
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn is_plain_left_click(button: Option<&str>, click_count: Option<u32>) -> bool {
    let button = button.unwrap_or("left");
    let click_count = click_count.unwrap_or(1);
    matches!(button.to_ascii_lowercase().as_str(), "left" | "primary") && click_count == 1
}

fn requested_or_primary_action(action: Option<&str>) -> &str {
    match action.map(str::trim).filter(|value| !value.is_empty()) {
        Some(action) => action,
        None => "0",
    }
}

fn primary_action(actions: &[AccessibilityAction]) -> Option<&AccessibilityAction> {
    actions.first()
}

/// Which way a wheel event goes. Four directions, no backend meaning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

fn parse_scroll_direction(direction: &str) -> Option<ScrollDirection> {
    match direction.trim().to_ascii_lowercase().as_str() {
        "up" => Some(ScrollDirection::Up),
        "down" => Some(ScrollDirection::Down),
        "left" => Some(ScrollDirection::Left),
        "right" => Some(ScrollDirection::Right),
        _ => None,
    }
}

/// An action named like "scroll down" / "scrollDown" / "scroll-down" for the
/// direction, matched on the normalized name.
fn scroll_action_for_direction(
    actions: &[AccessibilityAction],
    direction: ScrollDirection,
) -> Option<&AccessibilityAction> {
    let word = match direction {
        ScrollDirection::Up => "up",
        ScrollDirection::Down => "down",
        ScrollDirection::Left => "left",
        ScrollDirection::Right => "right",
    };
    actions.iter().find(|action| {
        let name = normalize_text(&action.name);
        name.contains("scroll") && name.contains(word)
    })
}

fn click_action(actions: &[AccessibilityAction]) -> Option<&AccessibilityAction> {
    actions
        .iter()
        .find(|action| action.name.trim().eq_ignore_ascii_case("click"))
}

/// True when the tree reports its node bounds relative to its own window
/// rather than to the desktop.
///
/// The tree reads extents with `CoordType::Screen`, but an accesskit-backed
/// app (GPUI, winit) answers that relative to the window: accesskit's AT-SPI
/// adapter adds the root window origin the app registered through
/// `set_root_window_bounds`, and on Wayland no app registers one because a
/// Wayland client is never told where it sits, so the origin stays at (0, 0).
/// The signature is a top-level frame that reports no desktop origin, either
/// extents starting at (0, 0) or no bounds at all (GPUI's Frame answers
/// `GetExtents` with nothing usable), while the compositor places the window
/// elsewhere. A frame that reports a real non-zero origin (GTK, Qt) is already
/// desktop-relative, and so is a tree with no top-level frame at all, which
/// gives nothing to judge by.
fn bounds_are_window_relative(nodes: &[AccessibilityNode]) -> bool {
    nodes
        .iter()
        .filter(|node| is_top_level_frame_role(&node.role))
        .min_by_key(|node| node.depth)
        .is_some_and(|frame| {
            !frame
                .bounds
                .as_ref()
                .is_some_and(|bounds| bounds.x != 0 || bounds.y != 0)
        })
}

/// Offset that maps a window-relative tree's node bounds onto the desktop.
///
/// The compositor backend is the only source of the real origin, so the
/// window's bounds supply it. A window at the desktop origin yields `(0, 0)`,
/// which still records that the tree follows the window.
fn window_relative_bounds_offset(
    nodes: &[AccessibilityNode],
    window: &WindowInfo,
) -> Option<(i32, i32)> {
    if !bounds_are_window_relative(nodes) {
        return None;
    }
    let window_bounds = window.bounds.as_ref()?;
    Some((window_bounds.x?, window_bounds.y?))
}

/// How a cached tree's node bounds map onto desktop coordinates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum CachedBounds {
    /// The bounds already are desktop coordinates, or no tree is cached.
    #[default]
    Desktop,
    /// The bounds follow a window, whose origin offsets them.
    Window(BoundsOffset),
    /// The bounds follow a window that could not be identified, so no offset
    /// can be applied and element points stay window-relative.
    Unanchored,
}

impl CachedBounds {
    /// The offset to add to a cached node's bounds, or `None` when they need
    /// none or none can be known.
    fn offset(&self) -> Option<(i32, i32)> {
        match self {
            Self::Window(bounds) => Some(bounds.offset),
            Self::Desktop | Self::Unanchored => None,
        }
    }
}

/// How a freshly snapshotted tree's bounds map onto the desktop, given the
/// window it was tied to (if any).
fn cached_bounds_for(nodes: &[AccessibilityNode], window: Option<&WindowInfo>) -> CachedBounds {
    if !bounds_are_window_relative(nodes) {
        return CachedBounds::Desktop;
    }
    match window.and_then(|window| {
        window_relative_bounds_offset(nodes, window).map(|offset| BoundsOffset {
            window_id: window.window_id,
            offset,
        })
    }) {
        Some(bounds) => CachedBounds::Window(bounds),
        None => CachedBounds::Unanchored,
    }
}

/// The single window belonging to one of `pids`, or `None` when none or
/// several do.
///
/// This is how a tree fetched without a window target still finds its window:
/// the AT-SPI roots it walked name a process, and a process with exactly one
/// window leaves no room to pick the wrong one. Several windows would, so they
/// are refused rather than guessed at — an offset by the wrong window moves
/// every click.
fn sole_window_for_pids<'a>(windows: &'a [WindowInfo], pids: &[u32]) -> Option<&'a WindowInfo> {
    let mut matches = windows
        .iter()
        .filter(|window| window.pid.is_some_and(|pid| pids.contains(&pid)));
    let window = matches.next()?;
    matches.next().is_none().then_some(window)
}

/// How the window reported alongside a tree was arrived at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum WindowContextSource {
    /// The caller named a window target and it resolved to this window.
    Target,
    /// The caller named none. This is the sole window of the process whose
    /// AT-SPI tree was walked, and it is the origin the tree's
    /// window-relative bounds are offset by.
    AppPid,
}

/// Which of the two answers the reported window is.
///
/// A tree anchored through the app's own pid belongs in `window_context` just
/// as much as a resolved target does: it is the window every element point was
/// offset by, so a caller reading the field to learn where the app is gets
/// what the click path used instead of a null that reads as "no window was
/// involved". The source keeps the two distinguishable without parsing prose
/// out of the message.
fn window_context_source(
    target: Option<&WindowInfo>,
    reported: Option<&WindowInfo>,
) -> Option<WindowContextSource> {
    match (target, reported) {
        (Some(_), _) => Some(WindowContextSource::Target),
        (None, Some(_)) => Some(WindowContextSource::AppPid),
        (None, None) => None,
    }
}

/// The window a window-relative tree belongs to and the origin it had when
/// the tree was cached.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoundsOffset {
    window_id: u64,
    offset: (i32, i32),
}

/// The window's origin as the compositor reports it now, or the cached one
/// when the window is no longer listed or has no position.
fn fresh_bounds_offset(cached: &BoundsOffset, windows: &[WindowInfo]) -> (i32, i32) {
    windows
        .iter()
        .find(|window| window.window_id == cached.window_id)
        .and_then(|window| window.bounds.as_ref())
        .and_then(|bounds| bounds.x.zip(bounds.y))
        .unwrap_or(cached.offset)
}

fn is_top_level_frame_role(role: &str) -> bool {
    matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "frame" | "window" | "dialog"
    )
}

fn primary_action_name(actions: &[AccessibilityAction]) -> Option<String> {
    primary_action(actions).map(|action| action.name.clone())
}

fn bounds_center(bounds: &Bounds) -> Option<(i32, i32)> {
    if bounds.width <= 0 || bounds.height <= 0 {
        return None;
    }
    if bounds.x <= i32::MIN / 2 || bounds.y <= i32::MIN / 2 {
        return None;
    }
    Some((
        bounds.x.checked_add(bounds.width / 2)?,
        bounds.y.checked_add(bounds.height / 2)?,
    ))
}

fn compact_accessibility_tree(nodes: Vec<AccessibilityNode>) -> Vec<AccessibilityNode> {
    if nodes.is_empty() {
        return nodes;
    }

    let keep = nodes
        .iter()
        .map(should_keep_accessibility_node)
        .collect::<Vec<_>>();
    let mut old_to_new = vec![None; nodes.len()];
    let mut compacted = Vec::new();

    for (old_index, node) in nodes.iter().enumerate() {
        if !keep[old_index] {
            continue;
        }

        let mut compacted_node = node.clone();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the tree it compacts is capped at HARD_SNAPSHOT_MAX_NODES, which is 2,000"
        )]
        let index = compacted.len() as u32;
        compacted_node.index = index;
        compacted_node.parent_index = nearest_kept_parent(&keep, &nodes, old_index);
        old_to_new[old_index] = Some(compacted_node.index);
        compacted.push(compacted_node);
    }

    for node in &mut compacted {
        node.parent_index = node
            .parent_index
            .and_then(|old_parent| old_to_new.get(old_parent as usize).copied().flatten());
    }

    let child_counts = compacted.iter().filter_map(|node| node.parent_index).fold(
        vec![0_i32; compacted.len()],
        |mut counts, parent_index| {
            counts[parent_index as usize] += 1;
            counts
        },
    );

    for (index, node) in compacted.iter_mut().enumerate() {
        node.child_count = child_counts[index];
    }

    compacted
}

fn nearest_kept_parent(
    keep: &[bool],
    nodes: &[AccessibilityNode],
    old_index: usize,
) -> Option<u32> {
    let mut parent = nodes[old_index].parent_index;
    while let Some(parent_index) = parent {
        let parent_usize = parent_index as usize;
        if keep.get(parent_usize).copied().unwrap_or(false) {
            return Some(parent_index);
        }
        parent = nodes.get(parent_usize).and_then(|node| node.parent_index);
    }
    None
}

fn should_keep_accessibility_node(node: &AccessibilityNode) -> bool {
    if node.depth <= 1 {
        return true;
    }

    if is_actionable_accessibility_node(node) || has_meaningful_node_copy(node) {
        return true;
    }

    matches!(
        node.role.as_str(),
        "page tab" | "menu item" | "menu" | "list item" | "tree item"
    ) && !is_sentinel_or_missing_bounds(node.bounds.as_ref())
}

fn is_actionable_accessibility_node(node: &AccessibilityNode) -> bool {
    !node.actions.is_empty() || node.supports_editable_text || node.value.is_some()
}

fn has_meaningful_node_copy(node: &AccessibilityNode) -> bool {
    has_non_empty_text(node.name.as_deref())
        || has_non_empty_text(node.description.as_deref())
        || has_non_empty_text(node.text.as_ref().and_then(|text| text.content.as_deref()))
}

fn has_non_empty_text(value: Option<&str>) -> bool {
    value.map(str::trim).is_some_and(|value| !value.is_empty())
}

fn is_sentinel_or_missing_bounds(bounds: Option<&Bounds>) -> bool {
    bounds.is_none()
}

fn select_accessibility_object_ref(
    apps: &[AccessibleAppSummary],
    target_pid: u32,
    candidates: &[String],
) -> Option<String> {
    let mut pid_matches = apps.iter().filter(|app| app.pid == Some(target_pid));
    let first = pid_matches.next()?;
    let second = pid_matches.next();

    if second.is_none() {
        return Some(first.object_ref.clone());
    }

    let lowered_candidates = candidates
        .iter()
        .map(|candidate| candidate.to_ascii_lowercase())
        .collect::<Vec<_>>();

    apps.iter()
        .filter(|app| app.pid == Some(target_pid))
        .find(|app| {
            let name = app.name.as_deref().unwrap_or_default().to_ascii_lowercase();
            lowered_candidates
                .iter()
                .any(|candidate| !candidate.is_empty() && name.contains(candidate))
        })
        .map(|app| app.object_ref.clone())
        .or_else(|| Some(first.object_ref.clone()))
}

/// The warning for a tree that came from a different process than the window
/// the caller targeted.
///
/// It happens because the AT-SPI application is looked up by pid first and by
/// name second, and the name fallback is what makes a wrapper, a Flatpak or
/// any app whose window pid is not its bus pid readable at all. So the
/// fallback stays and says so, rather than answering with a tree from an
/// application the caller never named while `window_context` describes
/// another one.
fn cross_process_tree_note(window: &WindowInfo, root_pids: &[u32]) -> Option<String> {
    let window_pid = window.pid?;
    if root_pids.is_empty() || root_pids.contains(&window_pid) {
        return None;
    }
    let pids = root_pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!(
        "WARNING: the accessibility tree came from pid {pids}, not the target window's process (pid {window_pid}), so the tree and window_context describe different applications; the AT-SPI application was matched by name because that pid exposes none. Element coordinates are offset by this window's origin, which is only right if they are the same application."
    ))
}

fn accessibility_filter_candidates(window_context: Option<&WindowInfo>) -> Vec<String> {
    let Some(window) = window_context else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    push_candidate(&mut candidates, window.title.as_deref());
    push_candidate(&mut candidates, window.wm_class.as_deref());

    if let Some(app_id) = trimmed_nonempty(window.app_id.as_deref())
        && !app_id.starts_with("window:")
    {
        push_candidate(&mut candidates, Some(app_id));
        if let Some(stripped) = app_id.strip_suffix(".desktop") {
            push_candidate(&mut candidates, Some(stripped));
            let normalized = stripped.replace(['-', '_', '.'], " ");
            push_candidate(&mut candidates, Some(normalized.as_str()));
        } else {
            let normalized = app_id.replace(['-', '_', '.'], " ");
            push_candidate(&mut candidates, Some(normalized.as_str()));
        }
    }

    candidates
}

fn push_candidate(candidates: &mut Vec<String>, value: Option<&str>) {
    let Some(value) = trimmed_nonempty(value) else {
        return;
    };

    if !candidates.iter().any(|candidate| candidate == value) {
        candidates.push(value.to_string());
    }
}

fn trimmed_nonempty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

/// True when an environment variable is set to `"1"` (an explicit on switch).
fn env_flag_enabled(key: &str) -> bool {
    env::var(key).ok().as_deref() == Some("1")
}

/// Return the base64 payload of a `data:` URL (or the original string if bare).
fn data_url_payload(data_url: &str) -> String {
    data_url
        .split_once(',')
        .map_or(data_url, |(_, payload)| payload)
        .to_string()
}

fn session_is_wayland(session_type: Option<&str>, wayland_display: Option<&str>) -> bool {
    match session_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => value.eq_ignore_ascii_case("wayland"),
        None => wayland_display.is_some_and(|value| !value.trim().is_empty()),
    }
}

fn prepare_app_state_screenshot(
    mut raw: RawScreenshotCapture,
    crop: Option<(i32, i32, u32, u32)>,
    target_requested: bool,
    options: ScreenshotPayloadOptions,
) -> Result<ScreenshotCapture> {
    if target_requested && crop.is_none() {
        anyhow::bail!(
            "targeted screenshot requires a resolved window; refusing to return the full desktop"
        );
    }
    if let Some(rect) = crop {
        let (x, y, width, height) = clip_capture_rect(rect, raw.width, raw.height)?;
        let (bytes, width, height) = crop_png(&raw.bytes, x, y, width, height)
            .map_err(|error| anyhow::anyhow!("targeted screenshot crop failed: {error}"))?;
        raw = RawScreenshotCapture {
            mime_type: raw.mime_type,
            bytes,
            source: raw.source,
            width,
            height,
        };
    }
    prepare_screenshot_payload(raw, options)
}

fn ensure_readonly_screenshot_target_is_visible(window: &WindowInfo) -> Result<()> {
    if window.hidden {
        anyhow::bail!("targeted get_app_state screenshot requires a visible, unminimized window");
    }
    if !window.focused {
        anyhow::bail!(
            "targeted get_app_state screenshot requires the window to already be focused; use the screenshot tool to raise it before capture"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct WindowCoordinateMap {
    capture_rect: (i32, i32, u32, u32),
}

#[expect(
    clippy::map_err_ignore,
    reason = "the discarded error is a TryFromIntError, whose message names no dimension; each arm below names the one that failed"
)]
fn clip_capture_rect(
    (x, y, width, height): (i32, i32, u32, u32),
    capture_width: u32,
    capture_height: u32,
) -> Result<(i32, i32, u32, u32)> {
    let left = i64::from(x).max(0);
    let top = i64::from(y).max(0);
    let right = (i64::from(x) + i64::from(width)).min(i64::from(capture_width));
    let bottom = (i64::from(y) + i64::from(height)).min(i64::from(capture_height));
    if right <= left || bottom <= top {
        anyhow::bail!(
            "targeted screenshot window is outside the captured desktop; refusing to return the full desktop"
        );
    }
    Ok((
        i32::try_from(left).map_err(|_| anyhow::anyhow!("capture crop x is invalid"))?,
        i32::try_from(top).map_err(|_| anyhow::anyhow!("capture crop y is invalid"))?,
        u32::try_from(right - left)
            .map_err(|_| anyhow::anyhow!("capture crop width is invalid"))?,
        u32::try_from(bottom - top)
            .map_err(|_| anyhow::anyhow!("capture crop height is invalid"))?,
    ))
}

/// Convert a window's bounds into a crop rectangle, if it has a usable origin
/// and non-zero size.
fn window_crop_rect(bounds: &crate::windowing::WindowBounds) -> Option<(i32, i32, u32, u32)> {
    let x = bounds.x?;
    let y = bounds.y?;
    if bounds.width == 0 || bounds.height == 0 {
        return None;
    }
    Some((x, y, bounds.width, bounds.height))
}

fn apply_window_relative_click_coordinates(
    params: &mut ClickParams,
    capture_rect: (i32, i32, u32, u32),
) -> std::result::Result<(), String> {
    let (relative_x, relative_y) = params
        .x
        .zip(params.y)
        .ok_or_else(|| "Relative coordinate clicks require both x and y.".to_string())?;
    let (origin_x, origin_y, width, height) = capture_rect;
    if width == 0 || height == 0 {
        return Err(
            "Relative coordinate clicks require non-empty target-window bounds.".to_string(),
        );
    }
    if relative_x < 0 || relative_y < 0 {
        return Err("Relative click coordinates must be inside target-window bounds.".to_string());
    }
    if i64::from(relative_x) >= i64::from(width) || i64::from(relative_y) >= i64::from(height) {
        return Err("Relative click coordinates must be inside target-window bounds.".to_string());
    }
    let x = origin_x
        .checked_add(relative_x)
        .ok_or_else(|| "Relative click x coordinate overflowed.".to_string())?;
    let y = origin_y
        .checked_add(relative_y)
        .ok_or_else(|| "Relative click y coordinate overflowed.".to_string())?;
    params.x = Some(x);
    params.y = Some(y);
    Ok(())
}

/// Point a window-targeted scroll at the center of the resolved window when
/// the caller supplied no coordinates. Without this the wheel events land on
/// whatever is under the current pointer position.
fn apply_window_center_scroll_point(
    params: &mut ScrollParams,
    capture_rect: (i32, i32, u32, u32),
) -> std::result::Result<(), String> {
    let (origin_x, origin_y, width, height) = capture_rect;
    if width == 0 || height == 0 {
        return Err(
            "Window-targeted scroll requires non-empty target-window bounds; pass x/y explicitly."
                .to_string(),
        );
    }
    params.x = Some(origin_x.saturating_add(i32::try_from(width / 2).unwrap_or(i32::MAX)));
    params.y = Some(origin_y.saturating_add(i32::try_from(height / 2).unwrap_or(i32::MAX)));
    Ok(())
}

fn apply_window_relative_scroll_coordinates(
    params: &mut ScrollParams,
    capture_rect: (i32, i32, u32, u32),
) -> std::result::Result<(), String> {
    let (relative_x, relative_y) = params
        .x
        .zip(params.y)
        .ok_or_else(|| "Relative scroll coordinates require both x and y.".to_string())?;
    let (origin_x, origin_y, width, height) = capture_rect;
    if width == 0 || height == 0 {
        return Err(
            "Relative scroll coordinates require non-empty target-window bounds.".to_string(),
        );
    }
    if relative_x < 0
        || relative_y < 0
        || i64::from(relative_x) >= i64::from(width)
        || i64::from(relative_y) >= i64::from(height)
    {
        return Err("Relative scroll coordinates must be inside target-window bounds.".to_string());
    }
    params.x = Some(origin_x.saturating_add(relative_x));
    params.y = Some(origin_y.saturating_add(relative_y));
    Ok(())
}

/// Crop a PNG image to `(x, y, w, h)` (clamped to the image), returning the
/// re-encoded PNG and the actual cropped dimensions.
fn crop_png(
    raw: &[u8],
    x: i32,
    y: i32,
    w: u32,
    h: u32,
) -> std::result::Result<(Vec<u8>, u32, u32), String> {
    use std::io::Cursor;
    let img = image::load_from_memory_with_format(raw, image::ImageFormat::Png)
        .map_err(|e| format!("decode png: {e}"))?;
    let (iw, ih) = (img.width(), img.height());
    let x = x.max(0).cast_unsigned();
    let y = y.max(0).cast_unsigned();
    if x >= iw || y >= ih {
        return Err("crop origin outside image".into());
    }
    let w = w.min(iw - x);
    let h = h.min(ih - y);
    let sub = img.crop_imm(x, y, w, h);
    let mut out = Vec::new();
    sub.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .map_err(|e| format!("encode png: {e}"))?;
    Ok((out, w, h))
}

/// A failed tool result. Every input tool builds the same five fields on every
/// early return, so they are built here instead of at each of them.
fn action_failure(
    action: &str,
    message: String,
    received: Option<serde_json::Value>,
) -> ActionOutput {
    ActionOutput {
        ok: false,
        implemented: true,
        action: action.to_string(),
        message,
        received,
    }
}

fn action_result(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
) -> ActionOutput {
    match result {
        Ok(_) => ActionOutput {
            ok: true,
            implemented: true,
            action: action.to_string(),
            message: "Action sent through ydotool.".to_string(),
            received,
        },
        Err(message) => ActionOutput {
            ok: false,
            implemented: true,
            action: action.to_string(),
            message,
            received,
        },
    }
}

fn action_result_with_focus(
    action: &str,
    result: std::result::Result<Vec<Output>, String>,
    received: Option<serde_json::Value>,
    focus: Option<WindowFocusResult>,
) -> ActionOutput {
    with_focus_context(action_result(action, result, received), focus)
}

fn with_focus_context(mut output: ActionOutput, focus: Option<WindowFocusResult>) -> ActionOutput {
    if output.ok
        && let Some(focus) = focus
    {
        let verification = if focus.exact_window_focused {
            "exact window-focus"
        } else {
            "app-level focus"
        };
        output.message = format!(
            "{} Target window_id {} was focused with {verification} verification before input.",
            output.message, focus.requested_window.window_id,
        );
    }
    output
}

fn describe_focused_element(element: &FocusedElementSummary, expects_editable: bool) -> String {
    let name = element
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .map(|name| format!(" \"{name}\""))
        .unwrap_or_default();
    // An element is editable when it implements the EditableText interface
    // or carries the `editable` state. GPUI (accesskit_unix) text inputs set
    // the state without the interface, and typed text does land in them.
    let editable = element.editable
        || element
            .states
            .iter()
            .any(|state| state.eq_ignore_ascii_case("editable"));
    let states = if element.states.is_empty() {
        String::new()
    } else {
        format!("; states: {}", element.states.join(", "))
    };
    if editable {
        format!(
            "Focused element: {}{name} (editable{states}).",
            element.role
        )
    } else if expects_editable {
        format!(
            "WARNING: focused element is {}{name}, which is not editable{states} — the typed text likely went nowhere. Click the intended input first or use set_value.",
            element.role
        )
    } else {
        format!(
            "Focused element: {}{name} (not editable{states}).",
            element.role
        )
    }
}

/// "element states before -> after" when an element action changed them.
fn states_change_note(before: &[String], after: &[String]) -> Option<String> {
    let mut sorted_before = before.to_vec();
    let mut sorted_after = after.to_vec();
    sorted_before.sort_unstable();
    sorted_after.sort_unstable();
    if sorted_before == sorted_after {
        return None;
    }
    Some(format!(
        "Element states before -> after: [{}] -> [{}].",
        sorted_before.join(", "),
        sorted_after.join(", ")
    ))
}

async fn element_states_note(object_ref: &str, before: &[String]) -> Option<String> {
    let after = timeout(Duration::from_millis(1500), element_states(object_ref))
        .await
        .ok()?
        .ok()?;
    states_change_note(before, &after)
}

/// The message for a failed element operation: the stale-tree hint when the
/// object's owner left the bus, the error itself otherwise.
fn element_error_message(error: &anyhow::Error) -> String {
    if is_stale_object_error(error) {
        STALE_TREE_MESSAGE.to_string()
    } else {
        error.to_string()
    }
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// Append supplemental notes (off-screen or focused-element feedback) to an
/// action result message without changing ok/implemented semantics.
fn with_notes(mut output: ActionOutput, notes: impl IntoIterator<Item = String>) -> ActionOutput {
    for note in notes {
        output.message = format!("{} {note}", output.message);
    }
    output
}

fn abs_pointer_clamp_note(landing: crate::abs_pointer::PointerLanding) -> Option<String> {
    (landing.requested != landing.emitted).then(|| {
        format!(
            "Requested coordinate {},{} was clamped to {},{} by the uinput absolute pointer.",
            landing.requested.0, landing.requested.1, landing.emitted.0, landing.emitted.1
        )
    })
}

fn focus_satisfies_target(focus: &WindowFocusResult, target: &WindowTarget) -> bool {
    if target.requires_exact_focus() {
        focus.exact_window_focused
    } else {
        focus.exact_window_focused || focus.app_focused
    }
}

async fn window_list_output() -> ListWindowsOutput {
    match list_windows().await {
        Ok(windows) => {
            let backend = window_backend(windows.iter());
            let note = registry::LIST_NOTE;
            ListWindowsOutput {
                backend,
                windows,
                error: None,
                permissions_hint: None,
                note: note.to_string(),
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            ListWindowsOutput {
                backend: UNKNOWN_BACKEND.to_string(),
                windows: Vec::new(),
                permissions_hint: window_permission_hint(&error),
                error: Some(error),
                note: "Window listing failed, so targeted keyboard input cannot safely focus or verify a target window."
                    .to_string(),
            }
        }
    }
}

fn window_backend<'a>(windows: impl Iterator<Item = &'a WindowInfo>) -> String {
    windows
        .map(|window| window.backend.clone())
        .next()
        .unwrap_or_else(|| UNKNOWN_BACKEND.to_string())
}

fn absolute_mousemove_args(x: i32, y: i32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--absolute".to_string(),
        "--".to_string(),
        x.to_string(),
        y.to_string(),
    ]
}

/// Map a semantic scroll direction to the `(dx, dy)` pair for
/// `ydotool mousemove --wheel`, which emits `dx` as `REL_HWHEEL` and `dy` as
/// `REL_WHEEL`. The two evdev axes are not symmetric: positive `REL_WHEEL`
/// is "wheel up" (content scrolls up), while positive `REL_HWHEEL` is "scroll
/// right" (content scrolls right). Verified on Hyprland 2026-09-04: `right`
/// sent as a negative `REL_HWHEEL` never moved a horizontal scroll area and
/// `left` moved it the wrong way.
fn ydotool_wheel_delta(direction: ScrollDirection, units: i32) -> (i32, i32) {
    match direction {
        ScrollDirection::Up => (0, units),
        ScrollDirection::Down => (0, -units),
        ScrollDirection::Left => (-units, 0),
        ScrollDirection::Right => (units, 0),
    }
}

fn wheel_mousemove_args(dx: i32, dy: i32) -> Vec<String> {
    vec![
        "mousemove".to_string(),
        "--wheel".to_string(),
        "--".to_string(),
        dx.to_string(),
        dy.to_string(),
    ]
}

async fn run_ydotool_sequence(
    commands: &[Vec<String>],
) -> std::result::Result<Vec<Output>, String> {
    let mut outputs = Vec::new();
    for (index, args) in commands.iter().enumerate() {
        outputs.push(run_ydotool(args).await?);
        if index + 1 < commands.len() {
            sleep(Duration::from_millis(35)).await;
        }
    }
    Ok(outputs)
}

async fn run_ydotool_drag(
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
) -> std::result::Result<Vec<Output>, String> {
    let mut outputs = vec![run_ydotool(&absolute_mousemove_args(start_x, start_y)).await?];
    sleep(Duration::from_millis(35)).await;

    let mut first_error = match run_ydotool(&["click".to_string(), "0x40".to_string()]).await {
        Ok(output) => {
            outputs.push(output);
            None
        }
        Err(error) => Some(error),
    };
    sleep(Duration::from_millis(35)).await;

    if first_error.is_none() {
        match run_ydotool(&absolute_mousemove_args(end_x, end_y)).await {
            Ok(output) => outputs.push(output),
            Err(error) => first_error = Some(error),
        }
        sleep(Duration::from_millis(35)).await;
    }

    let release = run_ydotool(&["click".to_string(), "0x80".to_string()]).await;
    match (first_error, release) {
        (None, Ok(output)) => {
            outputs.push(output);
            Ok(outputs)
        }
        (Some(error), Ok(_)) | (None, Err(error)) => Err(error),
        (Some(error), Err(release_error)) => Err(format!(
            "{error}; ydotool button release also failed: {release_error}"
        )),
    }
}

async fn run_cancellation_safe_input<T, F>(
    input_guard: tokio::sync::OwnedMutexGuard<()>,
    operation: F,
) -> (
    Option<tokio::sync::OwnedMutexGuard<()>>,
    std::result::Result<T, String>,
)
where
    T: Send + 'static,
    F: Future<Output = std::result::Result<T, String>> + Send + 'static,
{
    // Dropping a JoinHandle detaches its task, retaining the guard until the
    // stateful input operation has completed even if the caller is cancelled.
    // The session lease rides along for the same reason: the caller's own
    // hold dies with the caller, and the idle timer must not free the
    // machine-wide lock while the detached half is still sending input.
    let session_lease = crate::session_lock::hold_input_lock();
    match tokio::spawn(async move {
        let _session_lease = session_lease;
        (input_guard, operation.await)
    })
    .await
    {
        Ok((input_guard, result)) => (Some(input_guard), result),
        Err(error) => (None, Err(format!("stateful input task failed: {error}"))),
    }
}

async fn run_ydotool(args: &[String]) -> std::result::Result<Output, String> {
    let support = ydotool::ensure_supported_async().await?;
    let mut command = TokioCommand::new(&support.executable);
    command.args(args);
    if let Some(socket) = ydotool_socket() {
        command.env("YDOTOOL_SOCKET", socket);
    }
    let output =
        crate::command_runner::output_with_timeout(command, "run ydotool", INPUT_COMMAND_TIMEOUT)
            .await
            .map_err(|error| format!("{error:#}"))?;
    if output.status.success() {
        if let Some(error) = ydotool::cli_error(&output.stderr) {
            Err(error)
        } else {
            Ok(output)
        }
    } else {
        Err(ydotool_output_error(&output))
    }
}

async fn run_ydotool_type_text(text: &str) -> std::result::Result<Output, String> {
    let support = ydotool::ensure_supported_async().await?;
    let mut command = TokioCommand::new(&support.executable);
    command.args(["type", "--file", "-"]);
    if let Some(socket) = ydotool_socket() {
        command.env("YDOTOOL_SOCKET", socket);
    }
    let output = crate::command_runner::output_with_stdin(
        command,
        "run ydotool type",
        ydotool_type_timeout(text),
        text.as_bytes().to_vec(),
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    if output.status.success() {
        if let Some(error) = ydotool::cli_error(&output.stderr) {
            Err(error)
        } else {
            Ok(output)
        }
    } else {
        Err(ydotool_output_error(&output))
    }
}

fn ydotool_type_timeout(text: &str) -> Duration {
    let text_seconds = (text.chars().count() as u64).div_ceil(YDOTOOL_TYPE_CHARS_PER_SECOND);
    Duration::from_secs(INPUT_COMMAND_TIMEOUT.as_secs().saturating_add(text_seconds))
}

fn ydotool_output_error(output: &Output) -> String {
    command_output_error("ydotool", output)
}

/// Which keyboard lane produced a result: `wtype` speaks the Wayland
/// virtual-keyboard protocol and is layout-safe for literal text, while
/// ydotool injects raw evdev keycodes and is the fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardCommandBackend {
    Wtype,
    Ydotool,
}

struct KeyboardCommandResult {
    output: Output,
    backend: KeyboardCommandBackend,
}

async fn run_wtype_type_text_or_fallback<F, Fut>(
    program: &Path,
    text: &str,
    fallback: F,
) -> std::result::Result<KeyboardCommandResult, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = std::result::Result<Output, String>>,
{
    let available = if program.components().count() > 1 {
        std::fs::metadata(program).is_ok_and(|meta| meta.is_file())
    } else {
        program.to_str().is_some_and(which_in_path)
    };
    if !available {
        return fallback().await.map(|output| KeyboardCommandResult {
            output,
            backend: KeyboardCommandBackend::Ydotool,
        });
    }

    let mut command = TokioCommand::new(program);
    command.arg("-");
    let output = crate::command_runner::output_with_stdin(
        command,
        "run wtype",
        ydotool_type_timeout(text),
        text.as_bytes().to_vec(),
    )
    .await
    .map_err(|error| format!("{error:#}"))?;
    if !output.status.success() {
        return Err(command_output_error("wtype", &output));
    }
    Ok(KeyboardCommandResult {
        output,
        backend: KeyboardCommandBackend::Wtype,
    })
}

fn wtype_available() -> bool {
    which_in_path("wtype")
}

#[expect(
    clippy::fn_params_excessive_bools,
    reason = "four measured facts, each genuinely a yes or a no; a struct would name them twice and be built at one call site"
)]
fn prefer_wtype_keyboard(
    force_ydotool: bool,
    is_wayland: bool,
    compatible_desktop: bool,
    available: bool,
) -> bool {
    !force_ydotool && is_wayland && compatible_desktop && available
}

fn which_in_path(binary: &str) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(binary);
        std::fs::metadata(&candidate).is_ok_and(|meta| meta.is_file())
    })
}

fn command_output_error(command: &str, output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let detail = if stderr.is_empty() { stdout } else { stderr };
    if detail.is_empty() {
        format!("{command} exited with {}", output.status)
    } else {
        detail
    }
}

fn ydotool_socket() -> Option<String> {
    if let Some(socket) = explicit_ydotool_socket() {
        return Some(socket);
    }

    connectable_ydotool_socket_from(fallback_ydotool_socket_candidates())
        .map(|path| path.display().to_string())
}

fn explicit_ydotool_socket() -> Option<String> {
    explicit_ydotool_socket_from(env::var("YDOTOOL_SOCKET").ok().as_deref())
}

/// The socket an explicit `YDOTOOL_SOCKET` names, or `None` when the variable
/// is absent or blank. Split from the read so a test can state the value: the
/// alternative is writing one into the process environment, which edition 2024
/// makes `unsafe` because `cargo test` runs these on several threads.
fn explicit_ydotool_socket_from(socket: Option<&str>) -> Option<String> {
    let socket = socket?.trim();
    (!socket.is_empty()).then(|| socket.to_string())
}

fn fallback_ydotool_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(runtime) = env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .or_else(|| user_id().map(|uid| PathBuf::from(format!("/run/user/{uid}"))))
    {
        candidates.push(runtime.join(".ydotool_socket"));
    }
    candidates.push(PathBuf::from("/tmp/.ydotool_socket"));
    candidates
}

fn connectable_ydotool_socket_from(candidates: Vec<PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(ydotool_socket_connects)
}

fn ydotool_socket_connects(path: &PathBuf) -> bool {
    UnixDatagram::unbound()
        .and_then(|socket| socket.connect(path))
        .is_ok()
}

fn mouse_button_code(button: Option<&str>) -> String {
    match button.unwrap_or("left").to_ascii_lowercase().as_str() {
        "right" => "0xC1",
        "middle" => "0xC2",
        "side" => "0xC3",
        "extra" => "0xC4",
        "forward" => "0xC5",
        "back" => "0xC6",
        _ => "0xC0",
    }
    .to_string()
}

/// Parse a chord like `Ctrl+Shift+P` into raw evdev codes: the held
/// modifiers plus the final key. A bare modifier (`Super`) parses as a
/// chord with no held modifiers whose key is the modifier itself.
fn key_chord(key: &str) -> Option<(Vec<u16>, u16)> {
    let parts = key
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let (key_part, modifier_parts) = parts.split_last()?;
    if modifier_parts.is_empty()
        && let Some(modifier) = modifier_keycode(key_part)
    {
        return Some((Vec::new(), modifier));
    }
    let mut modifiers = Vec::new();
    for part in modifier_parts {
        modifiers.push(modifier_keycode(part)?);
    }
    let keycode = keycode(key_part)?;
    Some((modifiers, keycode))
}

fn key_sequence(key: &str) -> Option<Vec<String>> {
    let (modifiers, keycode) = key_chord(key)?;
    let mut events = Vec::new();
    for modifier in &modifiers {
        events.push(format!("{modifier}:1"));
    }
    events.push(format!("{keycode}:1"));
    events.push(format!("{keycode}:0"));
    for modifier in modifiers.iter().rev() {
        events.push(format!("{modifier}:0"));
    }
    Some(events)
}

/// The keys `press_key` sends, from exactly one of `key` and `keys`.
fn press_key_sequence(
    key: Option<&str>,
    keys: &[String],
) -> std::result::Result<Vec<String>, String> {
    let key = key.map(str::trim).filter(|value| !value.is_empty());
    let keys = keys
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    match (key, keys.is_empty()) {
        (Some(key), true) => Ok(vec![key.to_string()]),
        (None, false) => Ok(keys),
        (Some(_), false) => Err("Pass either key or keys, not both.".to_string()),
        (None, true) => Err("Pass key (one key or chord) or keys (a sequence).".to_string()),
    }
}

/// Evdev codes for the modifier names click/drag hold around a pointer action.
fn modifier_keycodes(modifiers: &[String]) -> std::result::Result<Vec<u16>, String> {
    let mut codes = Vec::new();
    for modifier in modifiers {
        let code = modifier_keycode(modifier).ok_or_else(|| {
            format!("Unsupported modifier {modifier:?}; use ctrl, alt, shift, or meta.")
        })?;
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    Ok(codes)
}

/// `ydotool key` arguments that press (`pressed`) or release every modifier.
fn modifier_hold_args(codes: &[u16], pressed: bool) -> Vec<String> {
    let state = u8::from(pressed);
    let mut args = vec!["key".to_string()];
    args.extend(codes.iter().map(|code| format!("{code}:{state}")));
    args
}

fn ydotool_key_args(key_events: Vec<String>, has_modifiers: bool) -> Vec<String> {
    let mut args = vec!["key".to_string()];
    if has_modifiers {
        args.extend(["-d".to_string(), "100".to_string()]);
    }
    args.extend(key_events);
    args
}

fn modifier_keycode(key: &str) -> Option<u16> {
    match normalize_key(key).as_str() {
        "ctrl" | "control" => Some(29),
        "alt" | "option" => Some(56),
        "shift" => Some(42),
        "meta" | "super" | "cmd" | "command" => Some(125),
        _ => None,
    }
}

fn keycode(key: &str) -> Option<u16> {
    match normalize_key(key).as_str() {
        "enter" | "return" => Some(28),
        "escape" | "esc" => Some(1),
        "tab" => Some(15),
        "backspace" => Some(14),
        "delete" | "del" => Some(111),
        "space" => Some(57),
        "home" => Some(102),
        "end" => Some(107),
        "pageup" | "page_up" => Some(104),
        "pagedown" | "page_down" => Some(109),
        "arrowleft" | "left" => Some(105),
        "arrowright" | "right" => Some(106),
        "arrowup" | "up" => Some(103),
        "arrowdown" | "down" => Some(108),
        "f1" => Some(59),
        "f2" => Some(60),
        "f3" => Some(61),
        "f4" => Some(62),
        "f5" => Some(63),
        "f6" => Some(64),
        "f7" => Some(65),
        "f8" => Some(66),
        "f9" => Some(67),
        "f10" => Some(68),
        "f11" => Some(87),
        "f12" => Some(88),
        value if value.len() == 1 => keycode_for_ascii(value.as_bytes()[0] as char),
        _ => None,
    }
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace(['-', ' '], "")
}

fn keycode_for_ascii(value: char) -> Option<u16> {
    match value {
        'a' => Some(30),
        'b' => Some(48),
        'c' => Some(46),
        'd' => Some(32),
        'e' => Some(18),
        'f' => Some(33),
        'g' => Some(34),
        'h' => Some(35),
        'i' => Some(23),
        'j' => Some(36),
        'k' => Some(37),
        'l' => Some(38),
        'm' => Some(50),
        'n' => Some(49),
        'o' => Some(24),
        'p' => Some(25),
        'q' => Some(16),
        'r' => Some(19),
        's' => Some(31),
        't' => Some(20),
        'u' => Some(22),
        'v' => Some(47),
        'w' => Some(17),
        'x' => Some(45),
        'y' => Some(21),
        'z' => Some(44),
        '1' => Some(2),
        '2' => Some(3),
        '3' => Some(4),
        '4' => Some(5),
        '5' => Some(6),
        '6' => Some(7),
        '7' => Some(8),
        '8' => Some(9),
        '9' => Some(10),
        '0' => Some(11),
        _ => None,
    }
}

fn list_process_apps() -> Vec<AppCandidate> {
    let output = Command::new("ps")
        .args(["-eo", "pid=,comm=,args="])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_line)
        .filter(|app| looks_like_desktop_app(&app.name, &app.command))
        .take(50)
        .collect()
}

fn parse_process_line(line: &str) -> Option<AppCandidate> {
    let trimmed = line.trim();
    let mut parts = trimmed.splitn(3, char::is_whitespace);
    let pid = parts.next()?.parse().ok()?;
    let name = parts.next()?.to_string();
    let command = parts.next().unwrap_or("").trim().to_string();
    Some(AppCandidate { name, pid, command })
}

fn looks_like_desktop_app(name: &str, command: &str) -> bool {
    let haystack = format!("{name} {command}").to_ascii_lowercase();
    [
        "codex",
        "electron",
        "chrome",
        "chromium",
        "firefox",
        "brave",
        "code",
        "gnome-terminal",
        "ptyxis",
        "kgx",
        "nautilus",
        "slack",
        "discord",
        "spotify",
        "obsidian",
    ]
    .iter()
    .any(|needle| haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windowing::{HYPRLAND_BACKEND, WindowBounds};
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn exported_tool_schemas_omit_unsigned_integer_formats() {
        let tools = ComputerUseLinux::default().mcp_tool_router().list_all();
        let value = serde_json::to_value(tools).unwrap();
        let mut unsupported = Vec::new();
        collect_unsigned_integer_formats(&value, "$", &mut unsupported);

        assert!(
            unsupported.is_empty(),
            "unsupported unsigned integer formats: {unsupported:?}"
        );
    }

    fn collect_unsigned_integer_formats(
        value: &serde_json::Value,
        path: &str,
        unsupported: &mut Vec<String>,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                if matches!(
                    object.get("format").and_then(serde_json::Value::as_str),
                    Some("uint" | "uint8" | "uint16" | "uint32" | "uint64" | "usize")
                ) {
                    unsupported.push(path.to_string());
                }
                for (key, nested) in object {
                    collect_unsigned_integer_formats(nested, &format!("{path}/{key}"), unsupported);
                }
            }
            serde_json::Value::Array(items) => {
                for (index, nested) in items.iter().enumerate() {
                    collect_unsigned_integer_formats(
                        nested,
                        &format!("{path}/{index}"),
                        unsupported,
                    );
                }
            }
            _ => {}
        }
    }

    /// A node identified by its `object_ref` rather than its position, which
    /// is what the stable-index tests are about.
    fn node_with_ref(object_ref: &str, parent_index: Option<u32>) -> AccessibilityNode {
        AccessibilityNode {
            object_ref: object_ref.to_string(),
            parent_index,
            ..node(0, None)
        }
    }

    #[test]
    fn an_element_index_survives_a_re_read_of_the_tree() {
        let backend = ComputerUseLinux::default();
        let mut first = vec![
            node_with_ref("app/frame", None),
            node_with_ref("app/toolbar", Some(0)),
            node_with_ref("app/details", Some(1)),
        ];

        backend.apply_stable_indices(&mut first);
        let details = first[2].index;
        assert_eq!(first[2].parent_index, Some(first[1].index));

        // The next read finds a node that was not there before, which would
        // renumber everything after it if indices were positional.
        let mut second = vec![
            node_with_ref("app/frame", None),
            node_with_ref("app/banner", Some(0)),
            node_with_ref("app/toolbar", Some(0)),
            node_with_ref("app/details", Some(2)),
        ];
        backend.apply_stable_indices(&mut second);

        assert_eq!(second[3].index, details);
        assert_eq!(second[3].parent_index, Some(second[2].index));
        assert_ne!(second[1].index, details);
    }

    #[test]
    fn an_index_whose_element_is_gone_is_not_handed_to_another_one() {
        let backend = ComputerUseLinux::default();
        let mut first = vec![node_with_ref("app/details", None)];
        backend.apply_stable_indices(&mut first);
        let details = first[0].index;

        let mut second = vec![node_with_ref("app/status-bar", None)];
        backend.apply_stable_indices(&mut second);

        assert_ne!(second[0].index, details);
        backend.cache_tree(&second, None);
        let error = backend
            .resolve_cached_node(
                Some(details),
                None,
                &ElementSelector::default(),
                ElementResolvePurpose::Click,
            )
            .expect_err("an index whose element is gone must not resolve");
        assert!(
            error.contains(&format!("element_index {details}")),
            "{error}"
        );
    }

    #[test]
    fn a_tree_from_another_process_than_the_window_says_so() {
        let window = WindowInfo {
            pid: Some(4242),
            ..placed_window(Some(8), Some(48))
        };

        assert!(cross_process_tree_note(&window, &[4242]).is_none());
        assert!(cross_process_tree_note(&window, &[]).is_none());
        let note = cross_process_tree_note(&window, &[99]).expect("a foreign pid is worth saying");
        assert!(note.contains("pid 99"), "{note}");
        assert!(note.contains("pid 4242"), "{note}");
    }

    fn node(index: u32, bounds: Option<Bounds>) -> AccessibilityNode {
        node_with_actions(index, bounds, Vec::new())
    }

    fn node_with_actions(
        index: u32,
        bounds: Option<Bounds>,
        actions: Vec<AccessibilityAction>,
    ) -> AccessibilityNode {
        AccessibilityNode {
            index,
            parent_index: None,
            depth: 0,
            object_ref: format!(":1.{index}/org/a11y/atspi/accessible/{index}"),
            role: "push button".to_string(),
            name: Some(format!("Button {index}")),
            description: None,
            child_count: 0,
            bounds,
            states: Vec::new(),
            actions,
            value: None,
            text: None,
            supports_editable_text: false,
        }
    }

    fn click_action() -> AccessibilityAction {
        AccessibilityAction {
            index: 0,
            name: "Click".to_string(),
            description: "Clicks the element".to_string(),
            keybinding: String::new(),
        }
    }

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(width, height, image::Rgba([32, 128, 192, 255]));
        let mut out = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn targeted_app_state_crops_before_screenshot_payload_resize() {
        let raw = RawScreenshotCapture {
            mime_type: "image/png".to_string(),
            bytes: solid_png(400, 200),
            source: "test".to_string(),
            width: 400,
            height: 200,
        };
        let capture = prepare_app_state_screenshot(
            raw,
            Some((50, 20, 200, 100)),
            true,
            ScreenshotPayloadOptions {
                max_width: Some(100),
                max_height: Some(100),
                max_bytes: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            (capture.coordinate_width, capture.coordinate_height),
            (200, 100)
        );
        assert_eq!((capture.width, capture.height), (100, 50));
    }

    #[test]
    fn unresolved_app_state_target_refuses_full_desktop_screenshot() {
        let raw = RawScreenshotCapture {
            mime_type: "image/png".to_string(),
            bytes: solid_png(400, 200),
            source: "test".to_string(),
            width: 400,
            height: 200,
        };

        let error =
            prepare_app_state_screenshot(raw, None, true, ScreenshotPayloadOptions::default())
                .unwrap_err();

        assert!(error.to_string().contains("requires a resolved window"));
    }

    #[test]
    fn targeted_app_state_crops_only_visible_part_of_offscreen_window() {
        let raw = RawScreenshotCapture {
            mime_type: "image/png".to_string(),
            bytes: solid_png(400, 200),
            source: "test".to_string(),
            width: 400,
            height: 200,
        };
        let capture = prepare_app_state_screenshot(
            raw,
            Some((-50, -40, 100, 100)),
            true,
            ScreenshotPayloadOptions::default(),
        )
        .unwrap();

        assert_eq!(
            (capture.coordinate_width, capture.coordinate_height),
            (50, 60)
        );
    }

    #[test]
    fn readonly_targeted_screenshot_requires_focused_visible_window() {
        let mut window = window_info(1, Some("Target"), None, None, None);
        ensure_readonly_screenshot_target_is_visible(&window).unwrap_err();
        window.focused = true;
        ensure_readonly_screenshot_target_is_visible(&window).unwrap();
        window.hidden = true;
        ensure_readonly_screenshot_target_is_visible(&window).unwrap_err();
    }

    #[test]
    fn wayland_prefers_wtype_unless_ydotool_is_forced() {
        assert!(prefer_wtype_keyboard(false, true, true, true));
        assert!(!prefer_wtype_keyboard(true, true, true, true));
        assert!(!prefer_wtype_keyboard(false, false, true, true));
        assert!(!prefer_wtype_keyboard(false, true, false, true));
        assert!(!prefer_wtype_keyboard(false, true, true, false));
    }

    #[test]
    fn window_crop_happens_before_screenshot_payload_resize() {
        let (cropped, width, height) = crop_png(&solid_png(400, 200), 50, 20, 200, 100).unwrap();
        let capture = prepare_screenshot_payload(
            RawScreenshotCapture {
                mime_type: "image/png".to_string(),
                bytes: cropped,
                source: "test".to_string(),
                width,
                height,
            },
            ScreenshotPayloadOptions {
                max_width: Some(100),
                max_height: Some(100),
                max_bytes: Some(1024 * 1024),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            (capture.coordinate_width, capture.coordinate_height),
            (200, 100)
        );
        assert_eq!((capture.width, capture.height), (100, 50));
        assert!(capture.resized);
    }

    fn window_info(
        window_id: u64,
        title: Option<&str>,
        app_id: Option<&str>,
        wm_class: Option<&str>,
        pid: Option<u32>,
    ) -> WindowInfo {
        WindowInfo {
            window_id,
            title: title.map(str::to_string),
            app_id: app_id.map(str::to_string),
            wm_class: wm_class.map(str::to_string),
            pid,
            bounds: Some(WindowBounds {
                x: Some(10),
                y: Some(20),
                width: 800,
                height: 600,
            }),
            workspace: Some(0),
            floating: None,
            focused: false,
            hidden: false,
            client_type: Some("wayland".to_string()),
            backend: HYPRLAND_BACKEND.to_string(),
            terminal: None,
        }
    }

    #[test]
    fn relative_click_coordinates_use_capture_space_rect() {
        let mut params = ClickParams {
            x: Some(7),
            y: Some(9),
            relative: Some(true),
            ..Default::default()
        };

        apply_window_relative_click_coordinates(&mut params, (133, 267, 1067, 800)).unwrap();

        assert_eq!((params.x, params.y), (Some(140), Some(276)));
    }

    #[test]
    fn relative_click_coordinates_require_xy() {
        let mut params = ClickParams {
            x: Some(7),
            relative: Some(true),
            ..Default::default()
        };

        let error =
            apply_window_relative_click_coordinates(&mut params, (100, 200, 800, 600)).unwrap_err();

        assert!(error.contains("both x and y"));
        assert_eq!((params.x, params.y), (Some(7), None));
    }

    #[test]
    fn relative_click_coordinates_must_stay_inside_bounds() {
        for (x, y) in [(-1, 9), (7, -1), (800, 9), (7, 600)] {
            let mut params = ClickParams {
                x: Some(x),
                y: Some(y),
                relative: Some(true),
                ..Default::default()
            };

            let error = apply_window_relative_click_coordinates(&mut params, (100, 200, 800, 600))
                .unwrap_err();

            assert!(error.contains("inside target-window bounds"));
            assert_eq!((params.x, params.y), (Some(x), Some(y)));
        }
    }

    #[test]
    fn absolute_pointer_note_reports_the_emitted_coordinate() {
        assert_eq!(
            abs_pointer_clamp_note(crate::abs_pointer::PointerLanding {
                requested: (1920, 1080),
                emitted: (1919, 1079),
            }),
            Some(
                "Requested coordinate 1920,1080 was clamped to 1919,1079 by the uinput absolute pointer."
                    .to_string()
            )
        );
        assert_eq!(
            abs_pointer_clamp_note(crate::abs_pointer::PointerLanding {
                requested: (640, 480),
                emitted: (640, 480),
            }),
            None
        );
    }

    #[test]
    fn accessibility_filter_candidates_prefer_title_and_skip_synthetic_app_id() {
        let window = window_info(
            42,
            Some("CU ATSPI GTK Test"),
            Some("window:46"),
            Some("cu_atspi_gtk_test.py"),
            Some(2_914_326),
        );

        let candidates = accessibility_filter_candidates(Some(&window));

        assert_eq!(
            candidates,
            vec![
                "CU ATSPI GTK Test".to_string(),
                "cu_atspi_gtk_test.py".to_string(),
            ]
        );
    }

    #[test]
    fn select_accessibility_object_ref_prefers_exact_pid_match() {
        let apps = vec![
            AccessibleAppSummary {
                object_ref: ":1.31/org/a11y/atspi/accessible/root".to_string(),
                name: Some("electron".to_string()),
                pid: Some(2_774_076),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
            AccessibleAppSummary {
                object_ref: ":1.64/org/a11y/atspi/accessible/root".to_string(),
                name: Some("cu_atspi_gtk_test.py".to_string()),
                pid: Some(2_914_326),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
        ];

        let object_ref = select_accessibility_object_ref(
            &apps,
            2_914_326,
            &[
                "CU ATSPI GTK Test".to_string(),
                "cu_atspi_gtk_test.py".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(object_ref, ":1.64/org/a11y/atspi/accessible/root");
    }

    #[test]
    fn compact_accessibility_tree_reparents_actionable_descendants() {
        let nodes = vec![
            AccessibilityNode {
                index: 0,
                parent_index: None,
                depth: 0,
                object_ref: ":1.0/root".to_string(),
                role: "application".to_string(),
                name: Some("demo-app".to_string()),
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                object_ref: ":1.1/frame".to_string(),
                role: "frame".to_string(),
                name: Some("Demo Frame".to_string()),
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 2,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.2/filler".to_string(),
                role: "filler".to_string(),
                name: None,
                description: None,
                child_count: 1,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 3,
                parent_index: Some(2),
                depth: 3,
                object_ref: ":1.3/button".to_string(),
                role: "button".to_string(),
                name: Some("Run".to_string()),
                description: None,
                child_count: 0,
                bounds: Some(Bounds {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 40,
                }),
                states: Vec::new(),
                actions: vec![AccessibilityAction {
                    index: 0,
                    name: "Click".to_string(),
                    description: "Clicks the button".to_string(),
                    keybinding: String::new(),
                }],
                value: None,
                text: None,
                supports_editable_text: false,
            },
        ];

        let compacted = compact_accessibility_tree(nodes);

        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[0].role, "application");
        assert_eq!(compacted[1].role, "frame");
        assert_eq!(compacted[2].role, "button");
        assert_eq!(compacted[2].parent_index, Some(1));
        assert_eq!(compacted[1].child_count, 1);
    }

    #[test]
    fn compact_accessibility_tree_drops_structural_noise() {
        let nodes = vec![
            AccessibilityNode {
                index: 0,
                parent_index: None,
                depth: 0,
                object_ref: ":1.0/root".to_string(),
                role: "application".to_string(),
                name: Some("demo-app".to_string()),
                description: None,
                child_count: 2,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 1,
                parent_index: Some(0),
                depth: 1,
                object_ref: ":1.1/frame".to_string(),
                role: "frame".to_string(),
                name: Some("Demo Frame".to_string()),
                description: None,
                child_count: 2,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 2,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.2/tab".to_string(),
                role: "page tab".to_string(),
                name: Some("Hidden".to_string()),
                description: None,
                child_count: 0,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
            AccessibilityNode {
                index: 3,
                parent_index: Some(1),
                depth: 2,
                object_ref: ":1.3/separator".to_string(),
                role: "separator".to_string(),
                name: None,
                description: None,
                child_count: 0,
                bounds: None,
                states: Vec::new(),
                actions: Vec::new(),
                value: None,
                text: None,
                supports_editable_text: false,
            },
        ];

        let compacted = compact_accessibility_tree(nodes);

        assert_eq!(compacted.len(), 3);
        assert_eq!(compacted[2].role, "page tab");
        assert_eq!(compacted[2].name.as_deref(), Some("Hidden"));
    }

    #[test]
    fn pointer_tools_carry_terminal_selectors_into_the_window_target() {
        // click, scroll, drag and screenshot used to hardcode every terminal
        // selector to None while the schema still advertised them, so a tty
        // target was accepted and silently ignored.
        let target = ActivateWindowParams {
            tty: Some("/dev/pts/3".to_string()),
            terminal_cwd: Some("/home/felipe/workspace".to_string()),
            ..Default::default()
        };

        let click = ClickParams {
            target: target.clone(),
            ..Default::default()
        }
        .window_target()
        .expect("a terminal selector is a window target");
        assert_eq!(click.tty.as_deref(), Some("/dev/pts/3"));
        assert_eq!(
            click.terminal_cwd.as_deref(),
            Some("/home/felipe/workspace")
        );

        let drag = DragParams {
            target,
            ..Default::default()
        }
        .window_target()
        .expect("a terminal selector is a window target");
        assert_eq!(drag.tty.as_deref(), Some("/dev/pts/3"));
    }

    #[test]
    fn no_selector_at_all_is_no_window_target() {
        assert!(ClickParams::default().window_target().is_none());
        assert!(ScrollParams::default().window_target().is_none());
        assert!(DragParams::default().window_target().is_none());
        assert!(ScreenshotParams::default().window_target().is_none());
    }

    #[test]
    fn drag_endpoint_resolves_an_element_index_to_its_center() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);

        let (point, note) = backend
            .drag_endpoint("start", Some(7), None, None, (0, 0), None)
            .expect("a cached node with positive bounds resolves");

        assert_eq!(point, (60, 40));
        assert!(note.contains("start_element_index 7"), "{note}");
        assert!(note.contains("desktop point (60, 40)"), "{note}");
    }

    #[test]
    fn drag_endpoint_offsets_an_element_by_the_window_origin() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            3,
            Some(Bounds {
                x: 232,
                y: 347,
                width: 220,
                height: 28,
            }),
        )]);

        let (point, note) = backend
            .drag_endpoint("end", Some(3), None, None, (0, 0), Some((965, 48)))
            .expect("a cached node with positive bounds resolves");

        assert_eq!(point, (1307, 409));
        assert!(
            note.contains("offset by the window origin (965, 48)"),
            "{note}"
        );
    }

    #[test]
    fn drag_endpoint_offsets_relative_coordinates_by_the_window_origin() {
        let backend = ComputerUseLinux::default();

        let (point, note) = backend
            .drag_endpoint("start", None, Some(352), Some(971), (965, 48), None)
            .expect("a coordinate pair resolves");

        assert_eq!(point, (1317, 1019));
        assert!(note.contains("window-relative (352, 971)"), "{note}");
    }

    #[test]
    fn drag_endpoint_refuses_a_half_given_or_doubly_given_end() {
        let backend = ComputerUseLinux::default();

        let missing = backend
            .drag_endpoint("end", None, Some(10), None, (0, 0), None)
            .unwrap_err();
        assert!(
            missing.contains("end_x and end_y, or end_element_index"),
            "{missing}"
        );

        let both = backend
            .drag_endpoint("start", Some(1), Some(10), Some(20), (0, 0), None)
            .unwrap_err();
        assert!(both.contains("not both"), "{both}");
    }

    #[test]
    fn cached_element_index_resolves_to_bounds_center() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);

        let point = backend
            .resolve_optional_target_point(None, None, Some(7), backend.cached_bounds_offset())
            .unwrap()
            .unwrap();

        assert_eq!(point, (60, 40));
    }

    #[test]
    fn coordinate_target_overrides_cached_element_index() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);

        let point = backend
            .resolve_optional_target_point(
                Some(200),
                Some(300),
                Some(7),
                backend.cached_bounds_offset(),
            )
            .unwrap()
            .unwrap();

        assert_eq!(point, (200, 300));
    }

    #[test]
    fn cached_element_index_requires_positive_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 0,
                height: 40,
            }),
        )]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7), backend.cached_bounds_offset())
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn cached_element_index_ignores_sentinel_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: i32::MIN,
                y: i32::MIN,
                width: 1,
                height: 1,
            }),
        )]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7), backend.cached_bounds_offset())
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn empty_node_cache_clears_stale_element_index() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
        )]);
        backend.cache_nodes(&[]);

        let error = backend
            .resolve_optional_target_point(None, None, Some(7), backend.cached_bounds_offset())
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn click_target_falls_back_to_primary_action_without_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            None,
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        match target {
            ClickTarget::Element {
                element_index,
                object_ref,
                action,
                point,
                bounds_offset,
                ..
            } => {
                assert_eq!(element_index, 7);
                assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
                let action = action.expect("primary action");
                assert_eq!(action.name, "Click");
                assert_eq!(action.index, 0);
                assert_eq!(point, None);
                assert_eq!(bounds_offset, None);
            }
            ClickTarget::Coordinates(_, _) => {
                panic!("expected AT-SPI primary-action fallback")
            }
        }
    }

    #[test]
    fn click_target_falls_back_to_primary_action_with_sentinel_bounds() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            Some(Bounds {
                x: i32::MIN,
                y: i32::MIN,
                width: 1,
                height: 1,
            }),
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        match target {
            ClickTarget::Element {
                element_index,
                object_ref,
                action,
                point,
                bounds_offset,
                ..
            } => {
                assert_eq!(element_index, 7);
                assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
                let action = action.expect("primary action");
                assert_eq!(action.name, "Click");
                assert_eq!(action.index, 0);
                assert_eq!(point, None);
                assert_eq!(bounds_offset, None);
            }
            ClickTarget::Coordinates(_, _) => {
                panic!("expected AT-SPI primary-action fallback")
            }
        }
    }

    #[test]
    fn click_target_requires_bounds_for_non_plain_clicks() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            None,
            vec![AccessibilityAction {
                index: 0,
                name: "Click".to_string(),
                description: "Clicks the button".to_string(),
                keybinding: String::new(),
            }],
        )]);

        let error = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    button: Some("right".to_string()),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap_err();

        assert!(error.contains("No clickable bounds cached for element_index 7"));
    }

    #[test]
    fn click_target_prefers_click_action_over_primary_action() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
            vec![
                AccessibilityAction {
                    index: 0,
                    name: "focus".to_string(),
                    description: String::new(),
                    keybinding: String::new(),
                },
                AccessibilityAction {
                    index: 1,
                    name: "click".to_string(),
                    description: String::new(),
                    keybinding: String::new(),
                },
            ],
        )]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        let ClickTarget::Element { action, point, .. } = target else {
            panic!("expected an element click target");
        };
        assert_eq!(action.map(|action| action.index), Some(1));
        assert_eq!(point, Some((60, 40)));
    }

    #[test]
    fn click_target_with_bounds_skips_non_click_primary_action() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
            vec![AccessibilityAction {
                index: 0,
                name: "focus".to_string(),
                description: String::new(),
                keybinding: String::new(),
            }],
        )]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        let ClickTarget::Element { action, point, .. } = target else {
            panic!("expected an element click target");
        };
        assert!(action.is_none());
        assert_eq!(point, Some((60, 40)));
    }

    #[test]
    fn non_plain_click_never_uses_the_atspi_action() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
            vec![click_action()],
        )]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(7),
                    click_count: Some(2),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        let ClickTarget::Element { action, point, .. } = target else {
            panic!("expected an element click target");
        };
        assert!(action.is_none());
        assert_eq!(point, Some((60, 40)));
    }

    fn frame_node(index: u32, bounds: Bounds) -> AccessibilityNode {
        let mut frame = node(index, Some(bounds));
        frame.role = "frame".to_string();
        frame
    }

    fn placed_window(x: Option<i32>, y: Option<i32>) -> WindowInfo {
        let mut window = window_info(1, Some("Sophia"), Some("sophia"), None, Some(4242));
        window.bounds = Some(WindowBounds {
            x,
            y,
            width: 900,
            height: 700,
        });
        window
    }

    #[test]
    fn window_relative_tree_is_offset_by_the_window_origin() {
        let nodes = [
            frame_node(
                0,
                Bounds {
                    x: 0,
                    y: 0,
                    width: 900,
                    height: 700,
                },
            ),
            node_with_actions(
                1,
                Some(Bounds {
                    x: 8,
                    y: 151,
                    width: 191,
                    height: 24,
                }),
                vec![click_action()],
            ),
        ];
        let window = placed_window(Some(965), Some(48));

        assert_eq!(
            window_relative_bounds_offset(&nodes, &window),
            Some((965, 48))
        );

        let backend = ComputerUseLinux::default();
        backend.cache_tree(&nodes, Some(&window));

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(1),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();
        let ClickTarget::Element {
            point,
            bounds_offset,
            ..
        } = target
        else {
            panic!("expected an element click target");
        };
        assert_eq!(point, Some((965 + 103, 48 + 163)));
        assert_eq!(bounds_offset, Some((965, 48)));
        assert_eq!(
            backend
                .resolve_optional_target_point(None, None, Some(1), backend.cached_bounds_offset())
                .unwrap(),
            Some((965 + 103, 48 + 163))
        );
    }

    #[test]
    fn frame_without_bounds_is_treated_as_window_relative() {
        let mut frame = node(1, None);
        frame.role = "Frame".to_string();
        frame.depth = 1;
        frame.name = Some("sophia-ui".to_string());
        let nodes = [
            frame,
            node_with_actions(
                2,
                Some(Bounds {
                    x: 8,
                    y: 151,
                    width: 191,
                    height: 24,
                }),
                vec![click_action()],
            ),
        ];

        assert_eq!(
            window_relative_bounds_offset(&nodes, &placed_window(Some(965), Some(48))),
            Some((965, 48))
        );
    }

    #[test]
    fn element_points_follow_the_window_after_it_moves() {
        let nodes = [
            frame_node(
                0,
                Bounds {
                    x: 0,
                    y: 0,
                    width: 900,
                    height: 700,
                },
            ),
            node_with_actions(
                1,
                Some(Bounds {
                    x: 8,
                    y: 151,
                    width: 191,
                    height: 24,
                }),
                vec![click_action()],
            ),
        ];
        let backend = ComputerUseLinux::default();
        backend.cache_tree(&nodes, Some(&placed_window(Some(965), Some(48))));
        let CachedBounds::Window(cached) = backend.node_bounds.lock().unwrap().clone() else {
            panic!("expected window-relative cached bounds");
        };
        assert_eq!(cached.window_id, 1);
        assert_eq!(cached.offset, (965, 48));

        let moved = placed_window(Some(1182), Some(419));
        let offset = fresh_bounds_offset(&cached, std::slice::from_ref(&moved));
        assert_eq!(offset, (1182, 419));
        assert_eq!(fresh_bounds_offset(&cached, &[]), (965, 48));
        assert_eq!(
            fresh_bounds_offset(&cached, &[placed_window(None, None)]),
            (965, 48)
        );

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    element_index: Some(1),
                    ..Default::default()
                },
                Some(offset),
            )
            .unwrap();
        let ClickTarget::Element {
            point,
            bounds_offset,
            ..
        } = target
        else {
            panic!("expected an element click target");
        };
        assert_eq!(point, Some((1182 + 103, 419 + 163)));
        assert_eq!(bounds_offset, Some((1182, 419)));
        assert_eq!(
            backend
                .resolve_optional_target_point(None, None, Some(1), Some(offset))
                .unwrap(),
            Some((1285, 582))
        );
    }

    #[test]
    fn an_untargeted_tree_is_anchored_by_the_app_pid_when_one_window_matches() {
        let nodes = [
            frame_node(
                0,
                Bounds {
                    x: 0,
                    y: 0,
                    width: 900,
                    height: 700,
                },
            ),
            node_with_actions(
                1,
                Some(Bounds {
                    x: 8,
                    y: 151,
                    width: 191,
                    height: 24,
                }),
                vec![click_action()],
            ),
        ];
        let window = placed_window(Some(965), Some(48));
        let other_app = window_info(2, Some("Files"), Some("nautilus"), None, Some(77));

        assert_eq!(
            sole_window_for_pids(&[other_app.clone(), window.clone()], &[4242])
                .map(|window| window.window_id),
            Some(1)
        );

        let backend = ComputerUseLinux::default();
        backend.cache_tree(&nodes, sole_window_for_pids(&[other_app, window], &[4242]));

        assert_eq!(backend.cached_bounds_offset(), Some((965, 48)));
        assert_eq!(
            backend
                .resolve_optional_target_point(None, None, Some(1), backend.cached_bounds_offset())
                .unwrap(),
            Some((965 + 103, 48 + 163))
        );
    }

    #[test]
    fn a_pid_owning_no_window_or_several_anchors_nothing() {
        let first = placed_window(Some(965), Some(48));
        let mut second = placed_window(Some(100), Some(100));
        second.window_id = 2;
        let unknown_pid = window_info(3, Some("Files"), Some("nautilus"), None, None);

        let window_id = |window: Option<&WindowInfo>| window.map(|window| window.window_id);

        assert_eq!(window_id(sole_window_for_pids(&[], &[4242])), None);
        assert_eq!(
            window_id(sole_window_for_pids(std::slice::from_ref(&first), &[])),
            None
        );
        assert_eq!(
            window_id(sole_window_for_pids(&[first.clone(), unknown_pid], &[4242])),
            Some(1)
        );
        assert_eq!(
            window_id(sole_window_for_pids(&[first, second], &[4242])),
            None
        );
    }

    #[test]
    fn a_resolved_target_is_reported_as_the_window_context_source() {
        let target = placed_window(Some(965), Some(48));

        assert_eq!(
            window_context_source(Some(&target), Some(&target)),
            Some(WindowContextSource::Target)
        );
    }

    #[test]
    fn a_tree_anchored_through_its_own_pid_reports_the_window_it_was_tied_to() {
        let anchored = placed_window(Some(965), Some(48));

        assert_eq!(
            window_context_source(None, Some(&anchored)),
            Some(WindowContextSource::AppPid)
        );
        assert_eq!(window_context_source(None, None), None);
    }

    #[test]
    fn a_window_context_source_serializes_in_the_case_the_payload_uses() {
        assert_eq!(
            serde_json::to_string(&WindowContextSource::AppPid).unwrap(),
            "\"app_pid\""
        );
        assert_eq!(
            serde_json::to_string(&WindowContextSource::Target).unwrap(),
            "\"target\""
        );
    }

    #[test]
    fn an_unanchored_window_relative_tree_is_recorded_as_such() {
        let window_relative = [
            frame_node(
                0,
                Bounds {
                    x: 0,
                    y: 0,
                    width: 900,
                    height: 700,
                },
            ),
            node(
                1,
                Some(Bounds {
                    x: 8,
                    y: 151,
                    width: 191,
                    height: 24,
                }),
            ),
        ];

        assert_eq!(
            cached_bounds_for(&window_relative, None),
            CachedBounds::Unanchored
        );
        assert_eq!(
            cached_bounds_for(&window_relative, Some(&placed_window(None, None))),
            CachedBounds::Unanchored
        );
        assert_eq!(
            cached_bounds_for(&window_relative, Some(&placed_window(Some(965), Some(48)))),
            CachedBounds::Window(BoundsOffset {
                window_id: 1,
                offset: (965, 48),
            })
        );

        // A tree that already reports desktop coordinates is never unanchored,
        // window or no window: it needs no offset.
        let desktop_relative = [frame_node(
            0,
            Bounds {
                x: 965,
                y: 48,
                width: 900,
                height: 700,
            },
        )];
        assert_eq!(
            cached_bounds_for(&desktop_relative, None),
            CachedBounds::Desktop
        );
        assert_eq!(CachedBounds::Unanchored.offset(), None);
        assert_eq!(CachedBounds::default(), CachedBounds::Desktop);
    }

    #[test]
    fn desktop_relative_tree_gets_no_offset() {
        let nodes = [
            frame_node(
                0,
                Bounds {
                    x: 965,
                    y: 48,
                    width: 900,
                    height: 700,
                },
            ),
            node(
                1,
                Some(Bounds {
                    x: 973,
                    y: 199,
                    width: 191,
                    height: 24,
                }),
            ),
        ];
        let window = placed_window(Some(965), Some(48));

        assert_eq!(window_relative_bounds_offset(&nodes, &window), None);

        let backend = ComputerUseLinux::default();
        backend.cache_tree(&nodes, Some(&window));
        assert_eq!(
            backend
                .resolve_optional_target_point(None, None, Some(1), backend.cached_bounds_offset())
                .unwrap(),
            Some((1068, 211))
        );
    }

    #[test]
    fn window_relative_tree_needs_a_window_origin_and_a_frame() {
        let frame = frame_node(
            0,
            Bounds {
                x: 0,
                y: 0,
                width: 900,
                height: 700,
            },
        );
        let button = node(
            1,
            Some(Bounds {
                x: 8,
                y: 151,
                width: 191,
                height: 24,
            }),
        );

        assert_eq!(
            window_relative_bounds_offset(
                std::slice::from_ref(&button),
                &placed_window(Some(965), Some(48))
            ),
            None
        );
        assert_eq!(
            window_relative_bounds_offset(
                &[frame.clone(), button.clone()],
                &placed_window(None, None)
            ),
            None
        );
        assert_eq!(
            window_relative_bounds_offset(&[frame, button], &placed_window(Some(0), Some(0))),
            Some((0, 0))
        );
    }

    #[test]
    fn click_target_resolves_by_object_ref() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[
            node(3, None),
            node_with_actions(
                7,
                Some(Bounds {
                    x: 10,
                    y: 20,
                    width: 100,
                    height: 40,
                }),
                vec![click_action()],
            ),
        ]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    object_ref: Some(":1.7/org/a11y/atspi/accessible/7".to_string()),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();
        let ClickTarget::Element {
            element_index,
            point,
            ..
        } = target
        else {
            panic!("expected an element click target");
        };
        assert_eq!(element_index, 7);
        assert_eq!(point, Some((60, 40)));

        let error = backend
            .resolve_click_target(
                &ClickParams {
                    object_ref: Some(":1.9/org/a11y/atspi/accessible/9".to_string()),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap_err();
        assert!(error.contains("No cached accessibility node with object_ref"));
    }

    #[test]
    fn stale_element_errors_get_the_refresh_hint() {
        let stale = anyhow::anyhow!(
            "org.freedesktop.DBus.Error.ServiceUnknown: The name :1.42 was not provided by any .service files"
        );
        assert_eq!(element_error_message(&stale), STALE_TREE_MESSAGE);
        let other = anyhow::anyhow!("element exposes no AT-SPI actions");
        assert_eq!(element_error_message(&other), other.to_string());
    }

    #[test]
    fn states_change_note_ignores_order_and_reports_changes() {
        assert_eq!(
            states_change_note(
                &["focused".to_string(), "enabled".to_string()],
                &["enabled".to_string(), "focused".to_string()]
            ),
            None
        );
        let note = states_change_note(
            &["enabled".to_string()],
            &["enabled".to_string(), "checked".to_string()],
        )
        .unwrap();
        assert_eq!(
            note,
            "Element states before -> after: [enabled] -> [checked, enabled]."
        );
    }

    #[test]
    fn describe_focused_element_lists_states() {
        let element = FocusedElementSummary {
            role: "entry".to_string(),
            name: None,
            editable: true,
            states: vec!["focused".to_string(), "editable".to_string()],
        };
        assert_eq!(
            describe_focused_element(&element, true),
            "Focused element: entry (editable; states: focused, editable)."
        );
    }

    #[test]
    fn press_key_sequence_takes_exactly_one_of_key_and_keys() {
        assert_eq!(
            press_key_sequence(Some("ctrl+l"), &[]).unwrap(),
            vec!["ctrl+l".to_string()]
        );
        assert_eq!(
            press_key_sequence(
                None,
                &["ctrl+a".to_string(), " ".to_string(), "Delete".to_string()]
            )
            .unwrap(),
            vec!["ctrl+a".to_string(), "Delete".to_string()]
        );
        press_key_sequence(Some("enter"), &["tab".to_string()]).unwrap_err();
        press_key_sequence(None, &[]).unwrap_err();
        press_key_sequence(Some("  "), &[]).unwrap_err();
    }

    #[test]
    fn modifier_hold_args_press_and_release_evdev_codes() {
        let codes =
            modifier_keycodes(&["ctrl".to_string(), "Shift".to_string(), "ctrl".to_string()])
                .unwrap();
        assert_eq!(codes, vec![29, 42]);
        assert_eq!(
            modifier_hold_args(&codes, true),
            vec!["key".to_string(), "29:1".to_string(), "42:1".to_string()]
        );
        assert_eq!(
            modifier_hold_args(&codes, false),
            vec!["key".to_string(), "29:0".to_string(), "42:0".to_string()]
        );
        modifier_keycodes(&["hyper".to_string()]).unwrap_err();
        assert!(modifier_keycodes(&[]).unwrap().is_empty());
    }

    #[test]
    fn region_crop_maps_desktop_and_window_relative_rectangles() {
        let region = ScreenshotRegion {
            x: 10,
            y: 20,
            width: 100,
            height: 50,
        };
        assert_eq!(
            region_crop_rect(&region, false, None, 1920, 1080).unwrap(),
            ((10, 20, 100, 50), (10, 20, 100, 50))
        );
        let window = Some((965, 48, 900, 700));
        assert_eq!(
            region_crop_rect(&region, true, window, 900, 700).unwrap(),
            ((10, 20, 100, 50), (975, 68, 100, 50))
        );
        assert_eq!(
            region_crop_rect(
                &ScreenshotRegion {
                    x: 975,
                    y: 68,
                    width: 100,
                    height: 50
                },
                false,
                window,
                900,
                700
            )
            .unwrap(),
            ((10, 20, 100, 50), (975, 68, 100, 50))
        );
    }

    #[test]
    fn region_crop_clips_and_rejects_empty_rectangles() {
        let region = ScreenshotRegion {
            x: 1900,
            y: 1070,
            width: 100,
            height: 50,
        };
        assert_eq!(
            region_crop_rect(&region, false, None, 1920, 1080).unwrap(),
            ((1900, 1070, 20, 10), (1900, 1070, 20, 10))
        );
        assert!(
            region_crop_rect(&region, true, None, 1920, 1080)
                .unwrap_err()
                .contains("window target")
        );
        let outside = ScreenshotRegion {
            x: 5000,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(
            region_crop_rect(&outside, false, None, 1920, 1080)
                .unwrap_err()
                .contains("outside")
        );
        let empty = ScreenshotRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 10,
        };
        region_crop_rect(&empty, false, None, 1920, 1080).unwrap_err();
    }

    #[test]
    fn occlusion_note_names_the_windows_above() {
        assert_eq!(occlusion_note(&[]), None);
        let note = occlusion_note(&[
            WindowOcclusion {
                window_id: 0x10,
                title: Some("Terminal".to_string()),
            },
            WindowOcclusion {
                window_id: 0x20,
                title: None,
            },
        ])
        .unwrap();
        assert!(note.starts_with("WARNING: 2 window(s)"));
        assert!(note.contains("Terminal (window_id 16)"));
        assert!(note.contains("untitled (window_id 32)"));
    }

    #[test]
    fn allowed_app_patterns_split_and_ignore_blanks() {
        assert_eq!(allowed_app_patterns(None), None);
        assert_eq!(allowed_app_patterns(Some("  , ")), None);
        assert_eq!(
            allowed_app_patterns(Some("sophia, Ghostty ,,firefox")),
            Some(vec![
                "sophia".to_string(),
                "Ghostty".to_string(),
                "firefox".to_string()
            ])
        );
    }

    #[test]
    fn allowlist_matches_app_id_wm_class_or_title_substrings() {
        let window = window_info(
            1,
            Some("Sophia — main.rs"),
            Some("sophia-ui"),
            Some("Sophia"),
            Some(1),
        );
        assert!(window_matches_allowlist(&window, &["SOPHIA".to_string()]));
        assert!(window_matches_allowlist(&window, &["main.rs".to_string()]));
        assert!(!window_matches_allowlist(&window, &["ghostty".to_string()]));
        let untitled = window_info(2, None, None, None, None);
        assert!(!window_matches_allowlist(
            &untitled,
            &["sophia".to_string()]
        ));
    }

    #[test]
    fn scroll_actions_match_direction_names_loosely() {
        let actions = vec![
            AccessibilityAction {
                index: 0,
                name: "click".to_string(),
                description: String::new(),
                keybinding: String::new(),
            },
            AccessibilityAction {
                index: 1,
                name: "scrollDown".to_string(),
                description: String::new(),
                keybinding: String::new(),
            },
            AccessibilityAction {
                index: 2,
                name: "Scroll left".to_string(),
                description: String::new(),
                keybinding: String::new(),
            },
        ];
        assert_eq!(
            scroll_action_for_direction(&actions, ScrollDirection::Down).map(|action| action.index),
            Some(1)
        );
        assert_eq!(
            scroll_action_for_direction(&actions, ScrollDirection::Left).map(|action| action.index),
            Some(2)
        );
        assert!(scroll_action_for_direction(&actions, ScrollDirection::Up).is_none());
        assert!(matches!(
            parse_scroll_direction(" Right "),
            Some(ScrollDirection::Right)
        ));
        assert!(parse_scroll_direction("sideways").is_none());

        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node_with_actions(4, None, actions)]);
        let (object_ref, action) = backend
            .cached_scroll_action(4, ScrollDirection::Down)
            .unwrap();
        assert_eq!(object_ref, ":1.4/org/a11y/atspi/accessible/4");
        assert_eq!(action.index, 1);
        assert!(
            backend
                .cached_scroll_action(4, ScrollDirection::Up)
                .is_none()
        );
    }

    #[test]
    fn keyboard_editable_needs_focusable_and_editable_states() {
        let backend = ComputerUseLinux::default();
        let mut entry = node(1, None);
        entry.states = vec!["focusable".to_string(), "Editable".to_string()];
        let mut label = node(2, None);
        label.states = vec!["editable".to_string()];
        backend.cache_nodes(&[entry, label]);

        assert!(backend.cached_node_is_keyboard_editable(":1.1/org/a11y/atspi/accessible/1"));
        assert!(!backend.cached_node_is_keyboard_editable(":1.2/org/a11y/atspi/accessible/2"));
        assert!(!backend.cached_node_is_keyboard_editable(":1.9/org/a11y/atspi/accessible/9"));
    }

    #[test]
    fn wait_for_timeout_defaults_and_caps() {
        assert_eq!(wait_for_timeout(None), Duration::from_millis(5_000));
        assert_eq!(wait_for_timeout(Some(250)), Duration::from_millis(250));
        assert_eq!(
            wait_for_timeout(Some(600_000)),
            Duration::from_millis(60_000)
        );
    }

    #[test]
    fn wait_for_requires_a_predicate() {
        assert!(!wait_for_has_predicate(&WaitForParams::default()));
        assert!(!wait_for_has_predicate(&WaitForParams {
            target: ActivateWindowParams {
                pid: Some(42),
                ..Default::default()
            },
            window_title: Some("   ".to_string()),
            ..Default::default()
        }));
        assert!(wait_for_has_predicate(&WaitForParams {
            role: Some("button".to_string()),
            ..Default::default()
        }));
        assert!(wait_for_has_predicate(&WaitForParams {
            window_title: Some("Sophia".to_string()),
            ..Default::default()
        }));
        assert!(wait_for_has_predicate(&WaitForParams {
            focused_window: Some(ActivateWindowParams::default()),
            ..Default::default()
        }));
    }

    #[test]
    fn wait_for_title_and_state_predicates_normalize() {
        assert!(title_contains(Some("Sophia — main.rs"), "sophia"));
        assert!(!title_contains(Some("Sophia"), "zed"));
        assert!(!title_contains(None, "sophia"));

        let mut focused = node(1, None);
        focused.states = vec!["Focused".to_string(), "editable".to_string()];
        assert!(node_has_state(&focused, "focused"));
        assert!(!node_has_state(&node(2, None), "focused"));
    }

    #[test]
    fn wait_for_params_map_onto_app_state_targeting() {
        let params = WaitForParams {
            target: ActivateWindowParams {
                pid: Some(4242),
                title: Some("Sophia".to_string()),
                ..Default::default()
            },
            role: Some("button".to_string()),
            ..Default::default()
        };
        let app_state = params.app_state_params();
        assert_eq!(app_state.target.pid, Some(4242));
        assert_eq!(app_state.target.title.as_deref(), Some("Sophia"));
        assert!(app_state.window_target().has_target());
        assert_eq!(params.selector().role, Some("button"));
    }

    #[test]
    fn ydotool_wheel_delta_follows_evdev_axis_signs() {
        assert_eq!(ydotool_wheel_delta(ScrollDirection::Up, 3), (0, 3));
        assert_eq!(ydotool_wheel_delta(ScrollDirection::Down, 3), (0, -3));
        assert_eq!(ydotool_wheel_delta(ScrollDirection::Left, 3), (-3, 0));
        assert_eq!(ydotool_wheel_delta(ScrollDirection::Right, 3), (3, 0));
    }

    #[test]
    fn absolute_mousemove_uses_coordinate_separator() {
        assert_eq!(
            absolute_mousemove_args(200, 300),
            vec![
                "mousemove".to_string(),
                "--absolute".to_string(),
                "--".to_string(),
                "200".to_string(),
                "300".to_string(),
            ]
        );
    }

    #[test]
    fn wheel_mousemove_uses_coordinate_separator_for_negative_values() {
        assert_eq!(
            wheel_mousemove_args(0, -3),
            vec![
                "mousemove".to_string(),
                "--wheel".to_string(),
                "--".to_string(),
                "0".to_string(),
                "-3".to_string(),
            ]
        );
    }

    #[test]
    fn pointer_actions_keep_pixel_coordinates_for_ydotool_absolute_moves() {
        assert_eq!(
            absolute_mousemove_args(1550, 930),
            vec![
                "mousemove".to_string(),
                "--absolute".to_string(),
                "--".to_string(),
                "1550".to_string(),
                "930".to_string(),
            ]
        );
    }

    #[test]
    fn key_chord_splits_modifiers_and_key() {
        assert_eq!(key_chord("Ctrl+Shift+P"), Some((vec![29, 42], 25)));
        assert_eq!(key_chord("Ctrl+S"), Some((vec![29], 31)));
        assert_eq!(key_chord("Enter"), Some((vec![], 28)));
        // A bare modifier is a chord with no held modifiers.
        assert_eq!(key_chord("Super"), Some((vec![], 125)));
        assert_eq!(key_chord("NotAKey"), None);
    }

    #[test]
    fn key_sequence_presses_modifiers_around_key() {
        assert_eq!(
            key_sequence("Ctrl+Shift+P"),
            Some(vec![
                "29:1".to_string(),
                "42:1".to_string(),
                "25:1".to_string(),
                "25:0".to_string(),
                "42:0".to_string(),
                "29:0".to_string(),
            ])
        );
    }

    #[test]
    fn ydotool_modifier_chords_include_an_inter_event_delay() {
        let args = ydotool_key_args(key_sequence("Ctrl+T").unwrap(), true);

        assert_eq!(args, ["key", "-d", "100", "29:1", "20:1", "20:0", "29:0"]);
    }

    #[test]
    fn key_sequence_presses_bare_modifier() {
        assert_eq!(
            key_sequence("Super"),
            Some(vec!["125:1".to_string(), "125:0".to_string()])
        );
    }

    #[tokio::test]
    async fn wtype_receives_unicode_text_through_stdin() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-wtype-unicode-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).expect("create command test directory");
        let wtype = dir.join("wtype");
        let captured = dir.join("captured");
        std::fs::write(
            &wtype,
            format!("#!/bin/sh\ncat > '{}'\n", captured.display()),
        )
        .expect("write fake wtype");
        std::fs::set_permissions(&wtype, std::fs::Permissions::from_mode(0o700))
            .expect("make fake wtype executable");
        let text = "Zwölf Yaks aßen Öl über München";

        let result = run_wtype_type_text_or_fallback(&wtype, text, || async {
            panic!("available wtype must not fall back")
        })
        .await
        .expect("wtype should succeed");

        assert_eq!(result.backend, KeyboardCommandBackend::Wtype);
        assert_eq!(std::fs::read_to_string(&captured).unwrap(), text);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn unavailable_wtype_uses_ydotool_fallback() {
        let result = run_wtype_type_text_or_fallback(
            Path::new("/definitely/missing/wtype"),
            "text",
            || async {
                TokioCommand::new("sh")
                    .args(["-c", "exit 0"])
                    .output()
                    .await
                    .map_err(|error| error.to_string())
            },
        )
        .await
        .expect("missing wtype should use fallback");

        assert_eq!(result.backend, KeyboardCommandBackend::Ydotool);
    }

    #[tokio::test]
    async fn launched_wtype_failure_does_not_replay_through_ydotool() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-wtype-fallback-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).expect("create command test directory");
        let wtype = dir.join("wtype");
        let fallback_marker = dir.join("ydotool-ran");
        std::fs::write(&wtype, "#!/bin/sh\nexit 9\n").expect("write fake wtype");
        std::fs::set_permissions(&wtype, std::fs::Permissions::from_mode(0o700))
            .expect("make fake wtype executable");

        let result = run_wtype_type_text_or_fallback(&wtype, "text", || async {
            std::fs::write(&fallback_marker, "ran").unwrap();
            TokioCommand::new("true")
                .output()
                .await
                .map_err(|error| error.to_string())
        })
        .await;

        assert!(result.is_err());
        assert!(
            !fallback_marker.exists(),
            "ydotool replayed input after wtype started"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn cancelling_between_press_and_release_keeps_input_locked() {
        let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
        let guard = std::sync::Arc::clone(&lock).lock_owned().await;
        let (pressed_tx, pressed_rx) = tokio::sync::oneshot::channel();
        let (allow_release_tx, allow_release_rx) = tokio::sync::oneshot::channel();
        let (released_tx, released_rx) = tokio::sync::oneshot::channel();

        let waiter = tokio::spawn(async move {
            run_cancellation_safe_input(guard, async move {
                let _ = pressed_tx.send(());
                let _ = allow_release_rx.await;
                let _ = released_tx.send(());
                Ok::<(), String>(())
            })
            .await
        });

        pressed_rx.await.expect("press command did not finish");
        waiter.abort();
        let _ = waiter.await;
        assert!(
            lock.try_lock().is_err(),
            "input lock was released while the paired operation was incomplete"
        );

        allow_release_tx
            .send(())
            .expect("release command stopped on caller cancellation");
        timeout(Duration::from_secs(1), released_rx)
            .await
            .expect("release command did not finish")
            .expect("release command dropped its completion marker");
        timeout(
            Duration::from_secs(1),
            std::sync::Arc::clone(&lock).lock_owned(),
        )
        .await
        .expect("input lock remained held after the operation finished");
    }

    #[test]
    fn key_sequence_keeps_shortcuts_and_navigation_on_raw_events() {
        assert_eq!(
            key_sequence("Ctrl+L"),
            Some(vec![
                "29:1".to_string(),
                "38:1".to_string(),
                "38:0".to_string(),
                "29:0".to_string(),
            ])
        );
        assert_eq!(
            key_sequence("ArrowLeft"),
            Some(vec!["105:1".to_string(), "105:0".to_string()])
        );
        assert_eq!(
            key_sequence("Escape"),
            Some(vec!["1:1".to_string(), "1:0".to_string()])
        );
        assert_eq!(
            key_sequence("Enter"),
            Some(vec!["28:1".to_string(), "28:0".to_string()])
        );
    }

    #[test]
    fn ydotool_type_timeout_scales_with_text_length() {
        assert_eq!(ydotool_type_timeout("").as_secs(), 10);
        assert_eq!(ydotool_type_timeout("x").as_secs(), 11);
        assert_eq!(ydotool_type_timeout(&"x".repeat(200)).as_secs(), 20);
        assert_eq!(ydotool_type_timeout(&"x".repeat(500)).as_secs(), 35);
    }

    #[tokio::test]
    async fn command_wait_drains_output_before_exit() {
        use std::process::Stdio;
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "yes noisy | head -c 200000 >&2; exit 7"]);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let output = crate::command_runner::output_with_timeout(
            command,
            "run test command",
            Duration::from_secs(5),
        )
        .await
        .expect("child should exit before timeout");

        assert_eq!(output.status.code(), Some(7));
        assert!(output.stderr.len() >= 100_000);
    }

    #[test]
    fn ydotool_socket_selection_rejects_legacy_stream_socket() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-server-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp server dir");
        let stale_socket = dir.join("stale.sock");
        std::fs::write(&stale_socket, b"not a socket").expect("write stale socket placeholder");
        let usable_socket = dir.join("usable.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&usable_socket).expect("bind usable socket");

        let selected = connectable_ydotool_socket_from(vec![stale_socket, usable_socket.clone()]);

        assert!(selected.is_none());
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ydotool_socket_selection_accepts_datagram_socket() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-server-dgram-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp server dir");
        let stale_socket = dir.join("stale.sock");
        std::fs::write(&stale_socket, b"not a socket").expect("write stale socket placeholder");
        let usable_socket = dir.join("usable.sock");
        let datagram =
            std::os::unix::net::UnixDatagram::bind(&usable_socket).expect("bind usable socket");

        let selected = connectable_ydotool_socket_from(vec![stale_socket, usable_socket.clone()])
            .expect("usable socket should be selected");

        assert_eq!(selected, usable_socket);
        drop(datagram);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn perform_action_defaults_to_primary_action_index() {
        assert_eq!(requested_or_primary_action(None), "0");
        assert_eq!(requested_or_primary_action(Some("   ")), "0");
        assert_eq!(
            requested_or_primary_action(Some(" show-menu ")),
            "show-menu"
        );
    }

    #[test]
    fn explicit_ydotool_socket_is_used_without_connectability_probe() {
        let selected = explicit_ydotool_socket_from(Some(" /does/not/exist.sock "));

        assert_eq!(selected.as_deref(), Some("/does/not/exist.sock"));
    }

    #[test]
    fn a_blank_or_absent_ydotool_socket_is_no_socket() {
        assert_eq!(explicit_ydotool_socket_from(Some("   ")), None);
        assert_eq!(explicit_ydotool_socket_from(None), None);
    }

    #[test]
    fn element_identifier_overrides_cached_object_ref() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(7, None)]);

        let object_ref = backend
            .resolve_object_ref(
                Some(7),
                Some(":1.99/org/a11y/atspi/accessible/3"),
                &ElementSelector::default(),
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.99/org/a11y/atspi/accessible/3");
    }

    #[test]
    fn element_index_resolves_to_cached_object_ref() {
        let backend = ComputerUseLinux::default();
        backend.cache_nodes(&[node(7, None)]);

        let object_ref = backend
            .resolve_object_ref(
                Some(7),
                None,
                &ElementSelector::default(),
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_resolves_unique_cached_node_by_role_and_name() {
        let backend = ComputerUseLinux::default();
        let mut search_entry = node(7, None);
        search_entry.role = "entry".to_string();
        search_entry.name = Some("Search files".to_string());
        search_entry.supports_editable_text = true;
        backend.cache_nodes(&[search_entry]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    role: Some("entry"),
                    name: Some("search"),
                    ..Default::default()
                },
                ElementResolvePurpose::SetValue,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_prefers_actionable_match() {
        let backend = ComputerUseLinux::default();
        let mut label = node(4, None);
        label.role = "label".to_string();
        label.name = Some("Close".to_string());
        let mut button = node_with_actions(7, None, vec![click_action()]);
        button.role = "push button".to_string();
        button.name = Some("Close".to_string());
        backend.cache_nodes(&[label, button]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("close"),
                    ..Default::default()
                },
                ElementResolvePurpose::Action,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_prefers_editable_match() {
        let backend = ComputerUseLinux::default();
        let mut label = node(4, None);
        label.role = "label".to_string();
        label.name = Some("Search".to_string());
        let mut entry = node(7, None);
        entry.role = "entry".to_string();
        entry.name = Some("Search".to_string());
        entry.supports_editable_text = true;
        backend.cache_nodes(&[label, entry]);

        let object_ref = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("search"),
                    ..Default::default()
                },
                ElementResolvePurpose::SetValue,
            )
            .unwrap();

        assert_eq!(object_ref, ":1.7/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn semantic_selector_reports_ambiguous_matches() {
        let backend = ComputerUseLinux::default();
        let mut first = node_with_actions(7, None, vec![click_action()]);
        first.name = Some("Close".to_string());
        let mut second = node_with_actions(9, None, vec![click_action()]);
        second.name = Some("Close".to_string());
        backend.cache_nodes(&[first, second]);

        let error = backend
            .resolve_object_ref(
                None,
                None,
                &ElementSelector {
                    name: Some("close"),
                    ..Default::default()
                },
                ElementResolvePurpose::Action,
            )
            .unwrap_err();

        assert!(error.contains("matched multiple cached nodes"));
        assert!(error.contains("element_index 7"));
        assert!(error.contains("element_index 9"));
    }

    #[test]
    fn semantic_click_selector_resolves_coordinates() {
        let backend = ComputerUseLinux::default();
        let mut button = node_with_actions(
            7,
            Some(Bounds {
                x: 10,
                y: 20,
                width: 100,
                height: 40,
            }),
            vec![click_action()],
        );
        button.name = Some("Run".to_string());
        backend.cache_nodes(&[button]);

        let target = backend
            .resolve_click_target(
                &ClickParams {
                    role: Some("button".to_string()),
                    name: Some("run".to_string()),
                    ..Default::default()
                },
                backend.cached_bounds_offset(),
            )
            .unwrap();

        match target {
            ClickTarget::Element {
                element_index,
                action,
                point,
                ..
            } => {
                assert_eq!(element_index, 7);
                assert_eq!(action.map(|action| action.name).as_deref(), Some("Click"));
                assert_eq!(point, Some((60, 40)));
            }
            ClickTarget::Coordinates(_, _) => panic!("expected an element click target"),
        }
    }

    #[test]
    fn describe_focused_element_editable() {
        let element = FocusedElementSummary {
            role: "text".to_string(),
            name: Some("Message".to_string()),
            editable: true,
            states: vec!["focused".to_string()],
        };
        let described = describe_focused_element(&element, true);
        assert!(described.contains("editable"));
        assert!(!described.contains("WARNING"));
    }

    #[test]
    fn describe_focused_element_warns_on_non_editable_when_typing() {
        let element = FocusedElementSummary {
            role: "push button".to_string(),
            name: Some("OK".to_string()),
            editable: false,
            states: vec!["focused".to_string()],
        };
        let described = describe_focused_element(&element, true);
        assert!(described.contains("WARNING"));
        assert!(described.contains("not editable"));
    }

    #[test]
    fn describe_focused_element_trusts_the_editable_state() {
        let element = FocusedElementSummary {
            role: "entry".to_string(),
            name: Some("Prompt".to_string()),
            editable: false,
            states: vec!["focused".to_string(), "editable".to_string()],
        };
        let described = describe_focused_element(&element, true);
        assert!(!described.contains("WARNING"));
        assert!(described.contains("(editable;"));
    }

    #[test]
    fn describe_focused_element_no_warning_for_press_key() {
        let element = FocusedElementSummary {
            role: "push button".to_string(),
            name: None,
            editable: false,
            states: vec![],
        };
        let described = describe_focused_element(&element, false);
        assert!(!described.contains("WARNING"));
    }

    #[test]
    fn relative_scroll_translates_coordinates() {
        let mut params = ScrollParams {
            element_index: None,
            x: Some(10),
            y: Some(20),
            direction: "down".to_string(),
            pages: None,
            target: ActivateWindowParams {
                window_id: Some(1),
                ..Default::default()
            },
            relative: Some(true),
        };
        apply_window_relative_scroll_coordinates(&mut params, (100, 200, 800, 600)).unwrap();
        assert_eq!(params.x, Some(110));
        assert_eq!(params.y, Some(220));
    }

    #[test]
    fn window_targeted_scroll_defaults_to_window_center() {
        let mut params = ScrollParams {
            element_index: None,
            x: None,
            y: None,
            direction: "down".to_string(),
            pages: None,
            target: ActivateWindowParams {
                window_id: Some(1),
                ..Default::default()
            },
            relative: None,
        };
        apply_window_center_scroll_point(&mut params, (100, 200, 800, 600)).unwrap();
        assert_eq!(params.x, Some(500));
        assert_eq!(params.y, Some(500));
    }

    #[test]
    fn window_targeted_scroll_with_empty_capture_rect_errors() {
        let mut params = ScrollParams {
            element_index: None,
            x: None,
            y: None,
            direction: "down".to_string(),
            pages: None,
            target: ActivateWindowParams {
                window_id: Some(1),
                ..Default::default()
            },
            relative: None,
        };
        let error = apply_window_center_scroll_point(&mut params, (0, 0, 0, 0)).unwrap_err();
        assert!(error.contains("pass x/y explicitly"));
        assert_eq!(params.x, None);
        assert_eq!(params.y, None);
    }

    #[test]
    fn relative_scroll_rejects_out_of_bounds() {
        let mut params = ScrollParams {
            element_index: None,
            x: Some(801),
            y: Some(20),
            direction: "down".to_string(),
            pages: None,
            target: ActivateWindowParams {
                window_id: Some(1),
                ..Default::default()
            },
            relative: Some(true),
        };
        assert!(
            apply_window_relative_scroll_coordinates(&mut params, (100, 200, 800, 600)).is_err()
        );
    }
}
