use crate::atspi_tree::{
    element_states, focused_element_summary, grab_focus, is_stale_object_error,
    list_accessible_apps, perform_action as invoke_accessibility_action, set_element_value,
    snapshot_limits, snapshot_tree, AccessibilityAction, AccessibilityNode, AccessibleAppSummary,
    Bounds, FocusedElementSummary, ValueSetInvocation,
};
use crate::diagnostics::{doctor_report, setup_accessibility_report, DoctorReport, SetupReport};
use crate::gnome_extension::{setup_window_targeting_report, WindowTargetingSetupReport};
use crate::remote_desktop::{
    click as portal_click, drag as portal_drag, keysyms_for_text, press_keycode_chord,
    scroll as portal_scroll, start_portal_keyboard_session, start_portal_pointer_session,
    type_text_with_keysyms, PointerButton, PortalKeyboardSession, PortalPointerSession,
    ScrollDirection,
};
use crate::screenshot::{
    capture_screenshot_raw, prepare_screenshot_payload, RawScreenshotCapture, ScreenshotCapture,
    ScreenshotOutputFormat, ScreenshotPayloadOptions,
};
use crate::windowing::registry;
use crate::windows::{
    focus_window_target, focused_window, list_windows, resolve_window_target,
    window_permission_hint, WindowFocusResult, WindowInfo, WindowOcclusion, WindowTarget,
    GNOME_SHELL_EXTENSION_BACKEND, GNOME_SHELL_INTROSPECT_BACKEND, KWIN_BACKEND,
};
use crate::ydotool;
use anyhow::Result;
use rmcp::{
    handler::server::wrapper::{Json, Parameters},
    model::{CallToolResult, Content},
    schemars::JsonSchema,
    tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    future::Future,
    os::unix::net::UnixDatagram,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    process::Command as TokioCommand,
    time::{sleep, timeout},
};
use zbus::{Connection as ZbusConnection, Proxy as ZbusProxy};

const INPUT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const WAIT_FOR_DEFAULT_TIMEOUT_MS: u64 = 5_000;
const STALE_TREE_MESSAGE: &str =
    "cached accessibility tree is stale (app restarted or window closed); call get_app_state again";
const WAIT_FOR_MAX_TIMEOUT_MS: u64 = 60_000;
const WAIT_FOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
const KEY_SEQUENCE_DELAY: Duration = Duration::from_millis(60);
/// How long an app gets to react before post-action feedback is read.
const POST_ACTION_SETTLE: Duration = Duration::from_millis(120);
const ALLOWED_APPS_ENV: &str = "COMPUTER_USE_LINUX_ALLOWED_APPS";
const YDOTOOL_TYPE_CHARS_PER_SECOND: u64 = 20;
const KDE_CLIPBOARD_DBUS_TIMEOUT: Duration = Duration::from_secs(3);
const KDE_KLIPPER_SERVICE: &str = "org.kde.klipper";
const KDE_KLIPPER_PATH: &str = "/klipper";
const KDE_KLIPPER_INTERFACE: &str = "org.kde.klipper.klipper";
const SHELL_ENABLE_ENV: &str = "COMPUTER_USE_LINUX_ENABLE_SHELL";
const SHELL_DEFAULT_TIMEOUT_SECS: u64 = 30;
const SHELL_MAX_TIMEOUT_SECS: u64 = 120;
const SHELL_MAX_COMMAND_BYTES: usize = 64 * 1024;
const SHELL_MAX_CWD_BYTES: usize = 4096;
const SHELL_MAX_ENV_ENTRIES: usize = 64;
const SHELL_MAX_ENV_BYTES: usize = 64 * 1024;
const SHELL_RESPONSE_STREAM_BYTES: usize = 512 * 1024;

#[derive(Clone, Default)]
pub struct ComputerUseLinux {
    last_nodes: Arc<Mutex<Vec<AccessibilityNode>>>,
    /// Offset that turns the cached nodes' bounds into desktop coordinates
    /// when the tree reported them relative to its window. See
    /// [`window_relative_bounds_offset`].
    node_bounds_offset: Arc<Mutex<Option<BoundsOffset>>>,
    portal_pointer_session: Arc<Mutex<Option<PortalPointerSession>>>,
    portal_keyboard_session: Arc<Mutex<Option<PortalKeyboardSession>>>,
    /// Lazily-created uinput absolute pointer (preferred coordinate backend).
    abs_pointer: Arc<Mutex<Option<crate::abs_pointer::AbsPointer>>>,
    portal_session_init_lock: Arc<tokio::sync::Mutex<()>>,
    input_operation_lock: Arc<tokio::sync::Mutex<()>>,
    kde_clipboard_lock: Arc<tokio::sync::Mutex<()>>,
    /// Cached physical desktop size from the most recent full-frame capture;
    /// used for off-screen warnings and portal logical-coordinate mapping.
    desktop_size: Arc<Mutex<Option<(u32, u32)>>>,
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
    fn mcp_tool_router(&self) -> rmcp::handler::server::router::tool::ToolRouter<Self> {
        let mut router = Self::tool_router();
        if !shell_execution_enabled() {
            router.map.remove("run_shell");
        }
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
        Json(
            tokio::task::spawn_blocking(doctor_report)
                .await
                .expect("diagnostics task panicked"),
        )
    }

    #[tool(
        name = "setup_accessibility",
        description = "Enable GNOME accessibility through gsettings so Linux Computer Use can read AT-SPI trees.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn setup_accessibility(&self) -> Json<SetupReport> {
        Json(
            tokio::task::spawn_blocking(setup_accessibility_report)
                .await
                .expect("accessibility setup task panicked"),
        )
    }

    #[tool(
        name = "setup_window_targeting",
        description = "Install and enable the optional GNOME Shell extension used for exact window list/focus targeting when GNOME blocks native introspection.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn setup_window_targeting(&self) -> Json<WindowTargetingSetupReport> {
        Json(setup_window_targeting_report().await)
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
                    backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
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
                    backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
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
        let diagnostics = tokio::task::spawn_blocking(doctor_report)
            .await
            .expect("diagnostics task panicked");
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
                    let crop = self.window_crop_rect_for_capture(window, &raw).await?;
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
        let (accessibility_tree, accessibility_tree_raw_count, accessibility_error) =
            if diagnostics.readiness.can_build_accessibility_tree {
                let target_pid = window_context.as_ref().and_then(|window| window.pid);
                match snapshot_tree(app_filter.as_deref(), target_pid, max_nodes, max_depth).await {
                    Ok(nodes) => {
                        let raw_count = nodes.len();
                        (compact_accessibility_tree(nodes), raw_count, None)
                    }
                    Err(error) => (Vec::new(), 0, Some(format!("{error:#}"))),
                }
            } else {
                (
                    Vec::new(),
                    0,
                    Some(
                        "GNOME accessibility is disabled; call setup_accessibility first."
                            .to_string(),
                    ),
                )
            };
        if accessibility_error.is_none() {
            self.cache_tree(&accessibility_tree, window_context.as_ref());
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
            message.push_str(&format!(
                " Window target resolved to window_id {}.",
                window.window_id
            ));
        } else if let Some(error) = &window_error {
            message.push_str(&format!(" Window target resolution failed: {error}"));
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
            window_context,
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
        description = "Report the current pointer position in desktop coordinates (the click/scroll/drag coordinate space). Supported on Hyprland (hyprctl cursorpos) and X11 (xdotool getmouselocation); other sessions answer ok=false.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
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
            let elapsed_ms = started.elapsed().as_millis() as u64;
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
        let elapsed_ms = started.elapsed().as_millis() as u64;
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
                let (x, y, width, height) = self
                    .window_crop_rect_for_capture(window, &raw_capture)
                    .await
                    .map_err(|error| {
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
            Content::image(data_url_payload(&capture.data_url), capture.mime_type),
            Content::text(caption.to_string()),
        ]))
    }

    /// Lazily create the uinput absolute pointer, sizing its ABS range to the
    /// logical desktop (the portal screenshot dimensions). Returns `false` if it
    /// can't be created or is disabled via `CU_DISABLE_ABS_POINTER`.
    async fn ensure_abs_pointer(&self) -> bool {
        if env_flag_enabled("CU_DISABLE_ABS_POINTER") {
            return false;
        }
        if self
            .abs_pointer
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false)
        {
            return true;
        }
        let Ok(cap) = capture_screenshot_raw().await else {
            return false;
        };
        self.cache_desktop_size(cap.width, cap.height);
        match tokio::task::spawn_blocking(move || {
            crate::abs_pointer::AbsPointer::create(cap.width as i32, cap.height as i32)
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
        description = "Click an element by index, object_ref, semantic selector, or desktop coordinate pixels from screenshot metadata. A plain left click on an element that exposes an AT-SPI click action invokes that action first and only falls back to the pointer; the message says which path was used. `modifiers` (ctrl/alt/shift/meta) are held around a pointer click.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn click(&self, Parameters(mut params): Parameters<ClickParams>) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        if let Err(message) = self
            .input_gate("click", params.window_target().as_ref())
            .await
        {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "click".to_string(),
                message,
                received,
            });
        }
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        let mut portal_target_point = None;
        // Raise the target window first (if specified) so the click lands on the
        // intended app rather than whatever is stacked on top at that pixel.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "click".to_string(),
                message: "Relative coordinate clicks require a window target.".to_string(),
                received,
            });
        }
        let mut focus = None;
        if let Some(target) = window_target {
            focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message,
                        received,
                    });
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            // Window-relative coordinates: translate by the window's top-left so
            // the agent can click the pixel it saw in a window-cropped screenshot.
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message: "Relative coordinate clicks require verified target-window focus."
                            .to_string(),
                        received,
                    });
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus).await {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(ActionOutput {
                            ok: false,
                            implemented: true,
                            action: "click".to_string(),
                            message,
                            received,
                        });
                    }
                };
                if let Err(message) = apply_window_relative_click_coordinates(
                    &mut params,
                    coordinate_map.capture_rect,
                ) {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message,
                        received,
                    });
                }
                portal_target_point = params
                    .x
                    .zip(params.y)
                    .and_then(|(x, y)| coordinate_map.portal_point(x, y));
            }
        }
        let bounds_offset = self.current_bounds_offset().await;
        let target = match self.resolve_click_target(&params, bounds_offset) {
            Ok(target) => target,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "click".to_string(),
                    message,
                    received,
                });
            }
        };
        let held_modifiers = match modifier_keycodes(&params.modifiers) {
            Ok(codes) => codes,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "click".to_string(),
                    message,
                    received,
                });
            }
        };
        let (element_index, object_ref, action, point, bounds_offset, states) = match target {
            ClickTarget::Coordinates(x, y) => {
                let output = self
                    .click_at_point_with_modifiers(
                        x,
                        y,
                        &params,
                        received,
                        input_guard,
                        portal_target_point,
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
                object_ref,
                action,
                point,
                bounds_offset,
                states,
            } => (
                element_index,
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
                                "Invoked {action_label} on element_index {element_index}; the pointer was not used."
                            ),
                            received,
                        },
                        notes,
                    ));
                }
                Ok(_) => format!("{action_label} on element_index {element_index} returned false"),
                Err(error) if is_stale_object_error(&error) => {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "click".to_string(),
                        message: STALE_TREE_MESSAGE.to_string(),
                        received,
                    });
                }
                Err(error) => format!(
                    "{action_label} on element_index {element_index} failed: {}",
                    first_line(&format!("{error:#}"))
                ),
            };
            if point.is_none() {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "click".to_string(),
                    message: format!("{failure}, and no clickable bounds were cached."),
                    received,
                });
            }
            notes.push(format!("{failure}; fell back to the pointer."));
        }
        let Some((x, y)) = point else {
            unreachable!("an element click target carries an action or a point");
        };
        notes.push(match bounds_offset {
            Some((dx, dy)) => format!(
                "element_index {element_index} resolved to desktop point ({x}, {y}): the tree's window-relative bounds were offset by the window origin ({dx}, {dy})."
            ),
            None => format!("element_index {element_index} resolved to desktop point ({x}, {y})."),
        });
        let output = self
            .click_at_point_with_modifiers(
                x,
                y,
                &params,
                received,
                input_guard,
                portal_target_point,
                &held_modifiers,
            )
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
    #[allow(clippy::too_many_arguments)]
    async fn click_at_point_with_modifiers(
        &self,
        x: i32,
        y: i32,
        params: &ClickParams,
        received: Option<serde_json::Value>,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
        portal_target_point: Option<(i32, i32)>,
        held_modifiers: &[u16],
    ) -> Json<ActionOutput> {
        if held_modifiers.is_empty() {
            return self
                .click_at_point(x, y, params, received, input_guard, portal_target_point)
                .await;
        }
        if let Err(message) = run_ydotool(&modifier_hold_args(held_modifiers, true)).await {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "click".to_string(),
                message: format!("Could not hold the modifiers through ydotool: {message}"),
                received,
            });
        }
        let output = self
            .click_at_point(x, y, params, received, input_guard, portal_target_point)
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

    /// Click a desktop coordinate through the best pointer backend available:
    /// the uinput absolute pointer, then the remote desktop portal, then
    /// xdotool on X11, then ydotool.
    async fn click_at_point(
        &self,
        x: i32,
        y: i32,
        params: &ClickParams,
        received: Option<serde_json::Value>,
        input_guard: tokio::sync::OwnedMutexGuard<()>,
        portal_target_point: Option<(i32, i32)>,
    ) -> Json<ActionOutput> {
        let button = mouse_button_code(params.button.as_deref());
        let click_count = params.click_count.unwrap_or(1).clamp(1, 10).to_string();
        // Preferred backend: the uinput absolute pointer. Unlike ydotool's
        // relative-only device (faked `--absolute` via pin-to-corner + relative
        // move, which acceleration + fractional scaling distort) and unlike the
        // portal (per-monitor coordinate scaling + an approval dialog), the
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
            return Json(with_notes(
                ActionOutput {
                    ok: true,
                    implemented: true,
                    action: "click".to_string(),
                    message: "Action sent through the uinput absolute pointer.".to_string(),
                    received,
                },
                abs_pointer_clamp_note(landing),
            ));
        }
        let off_screen_note = self.off_screen_note_for_point(x, y).await;
        if let Some(session) = self.cached_portal_pointer_session() {
            let Some((portal_x, portal_y)) =
                portal_target_point.or_else(|| self.logical_portal_point(&session, x, y))
            else {
                self.clear_portal_pointer_session(&session);
                return Json(with_notes(
                    portal_coordinate_error("click", received),
                    off_screen_note.clone(),
                ));
            };
            match portal_click(
                &session,
                portal_x,
                portal_y,
                PointerButton::from_name(params.button.as_deref()),
                params.click_count.unwrap_or(1).clamp(1, 10),
            )
            .await
            {
                Ok(()) => {
                    return Json(with_notes(
                        ActionOutput {
                            ok: true,
                            implemented: true,
                            action: "click".to_string(),
                            message: "Action sent through the remote desktop portal.".to_string(),
                            received,
                        },
                        off_screen_note.clone(),
                    ));
                }
                Err(error) => {
                    self.clear_portal_pointer_session(&session);
                    return Json(with_notes(
                        portal_action_error("click", error, received),
                        off_screen_note.clone(),
                    ));
                }
            }
        } else if self.should_prefer_portal_pointer_backend().await {
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => {
                    let Some((portal_x, portal_y)) =
                        portal_target_point.or_else(|| self.logical_portal_point(&session, x, y))
                    else {
                        self.clear_portal_pointer_session(&session);
                        return Json(with_notes(
                            portal_coordinate_error("click", received),
                            off_screen_note.clone(),
                        ));
                    };
                    match portal_click(
                        &session,
                        portal_x,
                        portal_y,
                        PointerButton::from_name(params.button.as_deref()),
                        params.click_count.unwrap_or(1).clamp(1, 10),
                    )
                    .await
                    {
                        Ok(()) => {
                            return Json(with_notes(
                                ActionOutput {
                                    ok: true,
                                    implemented: true,
                                    action: "click".to_string(),
                                    message: "Action sent through the remote desktop portal."
                                        .to_string(),
                                    received,
                                },
                                off_screen_note.clone(),
                            ));
                        }
                        Err(error) => {
                            self.clear_portal_pointer_session(&session);
                            return Json(with_notes(
                                portal_action_error("click", error, received),
                                off_screen_note.clone(),
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        if self.should_prefer_xdotool_pointer() {
            if let Some(xdotool_args) = xdotool_pointer_click_args(
                x,
                y,
                params.click_count.unwrap_or(1).clamp(1, 10),
                params.button.as_deref(),
            ) {
                let ydotool_commands = vec![
                    absolute_mousemove_args(x, y),
                    vec![
                        "click".to_string(),
                        "--repeat".to_string(),
                        click_count.clone(),
                        button.clone(),
                    ],
                ];
                let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
                    run_xdotool_pointer_or_fallback(Path::new("xdotool"), &xdotool_args, || async {
                        run_ydotool_sequence(&ydotool_commands).await
                    })
                    .await
                })
                .await;
                let _input_guard = input_guard;
                let used_xdotool = result
                    .as_ref()
                    .is_ok_and(|result| result.backend == KeyboardCommandBackend::Xdotool);
                let mut output =
                    action_result("click", result.map(|result| result.outputs), received);
                if output.ok && used_xdotool {
                    output.message = "Action sent through xdotool (X11 XTEST).".to_string();
                }
                return Json(with_notes(output, off_screen_note));
            }
        }
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
        if let Err(message) = self.input_gate("perform_action", None).await {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "perform_action".to_string(),
                message,
                received: Some(serde_json::json!(params.clone())),
            });
        }
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
        if let Err(message) = self.input_gate("set_value", None).await {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "set_value".to_string(),
                message,
                received,
            });
        }
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
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "set_value".to_string(),
                    message,
                    received,
                });
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
            Err(error) => Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "set_value".to_string(),
                message: element_error_message(&error),
                received,
            }),
        }
    }

    #[tool(
        name = "scroll",
        description = "Scroll an element in a direction by a number of pages. With element_index, an AT-SPI action named like \"scroll down\" for that direction is invoked first when the element exposes one; otherwise wheel events go to the element's centre. With a window target and no x/y/element_index, scrolls at the centre of the targeted window.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn scroll(&self, Parameters(mut params): Parameters<ScrollParams>) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params.clone()));
        if let Err(message) = self
            .input_gate("scroll", params.window_target().as_ref())
            .await
        {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "scroll".to_string(),
                message,
                received,
            });
        }
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        let mut portal_target_point = None;
        let units = ((params.pages.unwrap_or(1.0).abs().max(0.1) * 5.0).round() as i32).max(1);
        // Raise/focus the target window first (parity with click) so wheel
        // events land on the intended app.
        let window_target = params.window_target();
        if params.relative == Some(true) && window_target.is_none() {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "scroll".to_string(),
                message: "Relative scroll coordinates require a window target.".to_string(),
                received,
            });
        }
        if let Some(target) = window_target {
            let focus = match self.focus_target_for_input(&target).await {
                Ok(focus) => focus,
                Err(message) => {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
            };
            tokio::time::sleep(Duration::from_millis(120)).await;
            if params.relative == Some(true) {
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message:
                            "Relative scroll coordinates require verified target-window focus."
                                .to_string(),
                        received,
                    });
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus).await {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(ActionOutput {
                            ok: false,
                            implemented: true,
                            action: "scroll".to_string(),
                            message,
                            received,
                        });
                    }
                };
                if let Err(message) = apply_window_relative_scroll_coordinates(
                    &mut params,
                    coordinate_map.capture_rect,
                ) {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
                portal_target_point = params
                    .x
                    .zip(params.y)
                    .and_then(|(x, y)| coordinate_map.portal_point(x, y));
            } else if params.x.is_none() && params.y.is_none() && params.element_index.is_none() {
                // A window target without a point would otherwise scroll
                // whatever happens to sit under the pointer: focusing does not
                // move the cursor, and the wheel path never repositions it.
                // Default to the centre of the resolved target window.
                let Some(focus) = focus.as_ref() else {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message: "Window-targeted scroll requires verified target-window focus."
                            .to_string(),
                        received,
                    });
                };
                let coordinate_map = match self.focused_window_coordinate_map(focus).await {
                    Ok(mapping) => mapping,
                    Err(message) => {
                        return Json(ActionOutput {
                            ok: false,
                            implemented: true,
                            action: "scroll".to_string(),
                            message,
                            received,
                        });
                    }
                };
                if let Err(message) =
                    apply_window_center_scroll_point(&mut params, coordinate_map.capture_rect)
                {
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message,
                        received,
                    });
                }
                portal_target_point = params
                    .x
                    .zip(params.y)
                    .and_then(|(x, y)| coordinate_map.portal_point(x, y));
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
                    return Json(ActionOutput {
                        ok: false,
                        implemented: true,
                        action: "scroll".to_string(),
                        message: STALE_TREE_MESSAGE.to_string(),
                        received,
                    });
                }
                Err(error) => notes.push(format!(
                    "{action_label} failed ({}); fell back to the wheel.",
                    first_line(&format!("{error:#}"))
                )),
            }
        }
        let bounds_offset = self.current_bounds_offset().await;
        let target_point = match self.resolve_optional_target_point(
            params.x,
            params.y,
            params.element_index,
            bounds_offset,
        ) {
            Ok(point) => point,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "scroll".to_string(),
                    message,
                    received,
                });
            }
        };
        let direction = match params.direction.to_ascii_lowercase().as_str() {
            "up" => ScrollDirection::Up,
            "down" => ScrollDirection::Down,
            "left" => ScrollDirection::Left,
            "right" => ScrollDirection::Right,
            _ => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "scroll".to_string(),
                    message: "Unsupported scroll direction; expected up, down, left, or right."
                        .to_string(),
                    received,
                });
            }
        };
        let off_screen_note = match target_point {
            Some((x, y)) => self.off_screen_note_for_point(x, y).await,
            None => None,
        };
        if let Some(session) = self.cached_portal_pointer_session() {
            let mapped_target = match (portal_target_point, target_point) {
                (Some(point), _) => Some(Some(point)),
                (None, Some((x, y))) => self.logical_portal_point(&session, x, y).map(Some),
                (None, None) => Some(None),
            };
            let Some(portal_target_point) = mapped_target else {
                self.clear_portal_pointer_session(&session);
                return Json(with_notes(
                    portal_coordinate_error("scroll", received),
                    off_screen_note.clone(),
                ));
            };
            match portal_scroll(&session, portal_target_point, direction, units).await {
                Ok(()) => {
                    return Json(with_notes(
                        ActionOutput {
                            ok: true,
                            implemented: true,
                            action: "scroll".to_string(),
                            message: "Action sent through the remote desktop portal.".to_string(),
                            received,
                        },
                        off_screen_note.clone(),
                    ));
                }
                Err(error) => {
                    self.clear_portal_pointer_session(&session);
                    return Json(with_notes(
                        portal_action_error("scroll", error, received),
                        off_screen_note.clone(),
                    ));
                }
            }
        } else if self.should_prefer_portal_pointer_backend().await {
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => {
                    let mapped_target = match (portal_target_point, target_point) {
                        (Some(point), _) => Some(Some(point)),
                        (None, Some((x, y))) => self.logical_portal_point(&session, x, y).map(Some),
                        (None, None) => Some(None),
                    };
                    let Some(portal_target_point) = mapped_target else {
                        self.clear_portal_pointer_session(&session);
                        return Json(with_notes(
                            portal_coordinate_error("scroll", received),
                            off_screen_note.clone(),
                        ));
                    };
                    match portal_scroll(&session, portal_target_point, direction, units).await {
                        Ok(()) => {
                            return Json(with_notes(
                                ActionOutput {
                                    ok: true,
                                    implemented: true,
                                    action: "scroll".to_string(),
                                    message: "Action sent through the remote desktop portal."
                                        .to_string(),
                                    received,
                                },
                                off_screen_note.clone(),
                            ));
                        }
                        Err(error) => {
                            self.clear_portal_pointer_session(&session);
                            return Json(with_notes(
                                portal_action_error("scroll", error, received),
                                off_screen_note.clone(),
                            ));
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let (dx, dy) = ydotool_wheel_delta(direction, units);
        let mut sequence = Vec::new();
        if let Some((x, y)) = target_point {
            // The absolute pointer lands exactly where the click path does;
            // ydotool's faked absolute move drifts under acceleration and
            // scaling, so it is only the fallback for positioning the wheel.
            if self.try_abs_move(x, y).await.is_none() {
                sequence.push(absolute_mousemove_args(x, y));
            }
        }
        sequence.push(wheel_mousemove_args(dx, dy));
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_sequence(&sequence).await
        })
        .await;
        let _input_guard = input_guard;
        notes.extend(off_screen_note);
        Json(with_notes(action_result("scroll", result, received), notes))
    }

    #[tool(
        name = "drag",
        description = "Drag from one point to another using pixel coordinates.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn drag(&self, Parameters(params): Parameters<DragParams>) -> Json<ActionOutput> {
        if let Err(message) = self.input_gate("drag", None).await {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "drag".to_string(),
                message,
                received: Some(serde_json::json!(params)),
            });
        }
        let held_modifiers = match modifier_keycodes(&params.modifiers) {
            Ok(codes) => codes,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "drag".to_string(),
                    message,
                    received: Some(serde_json::json!(params)),
                });
            }
        };
        let mut notes = Vec::new();
        if !held_modifiers.is_empty() {
            if let Err(message) = run_ydotool(&modifier_hold_args(&held_modifiers, true)).await {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "drag".to_string(),
                    message: format!("Could not hold the modifiers through ydotool: {message}"),
                    received: Some(serde_json::json!(params)),
                });
            }
        }
        let modifiers = params.modifiers.join("+");
        let output = self.drag_inner(params).await;
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

    async fn drag_inner(&self, params: DragParams) -> Json<ActionOutput> {
        let received = Some(serde_json::json!(params));
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        // Preferred backend: the uinput absolute pointer (accurate landing).
        if self.ensure_abs_pointer().await {
            let abs_pointer = Arc::clone(&self.abs_pointer);
            let dragged = tokio::task::spawn_blocking(move || {
                if let Ok(mut guard) = abs_pointer.lock() {
                    guard.as_mut().map(|p| {
                        p.drag(
                            (params.start_x, params.start_y),
                            (params.end_x, params.end_y),
                            crate::abs_pointer::PointerButton::Left,
                        )
                        .is_ok()
                    })
                } else {
                    None
                }
            })
            .await
            .ok()
            .flatten();
            if dragged == Some(true) {
                return Json(ActionOutput {
                    ok: true,
                    implemented: true,
                    action: "drag".to_string(),
                    message: "Action sent through the uinput absolute pointer.".to_string(),
                    received,
                });
            }
        }
        if let Some(session) = self.cached_portal_pointer_session() {
            let _ = self.capture_space_rect().await;
            let Some((start_x, start_y)) =
                self.logical_portal_point(&session, params.start_x, params.start_y)
            else {
                self.clear_portal_pointer_session(&session);
                return Json(portal_coordinate_error("drag", received));
            };
            let Some((end_x, end_y)) =
                self.logical_portal_point(&session, params.end_x, params.end_y)
            else {
                self.clear_portal_pointer_session(&session);
                return Json(portal_coordinate_error("drag", received));
            };
            match portal_drag(&session, start_x, start_y, end_x, end_y).await {
                Ok(()) => {
                    return Json(ActionOutput {
                        ok: true,
                        implemented: true,
                        action: "drag".to_string(),
                        message: "Action sent through the remote desktop portal.".to_string(),
                        received,
                    });
                }
                Err(error) => {
                    self.clear_portal_pointer_session(&session);
                    return Json(portal_action_error("drag", error, received));
                }
            }
        } else if self.should_prefer_portal_pointer_backend().await {
            let _ = self.capture_space_rect().await;
            match self.ensure_portal_pointer_session().await {
                Ok(Some(session)) => {
                    let Some((start_x, start_y)) =
                        self.logical_portal_point(&session, params.start_x, params.start_y)
                    else {
                        self.clear_portal_pointer_session(&session);
                        return Json(portal_coordinate_error("drag", received));
                    };
                    let Some((end_x, end_y)) =
                        self.logical_portal_point(&session, params.end_x, params.end_y)
                    else {
                        self.clear_portal_pointer_session(&session);
                        return Json(portal_coordinate_error("drag", received));
                    };
                    match portal_drag(&session, start_x, start_y, end_x, end_y).await {
                        Ok(()) => {
                            return Json(ActionOutput {
                                ok: true,
                                implemented: true,
                                action: "drag".to_string(),
                                message: "Action sent through the remote desktop portal."
                                    .to_string(),
                                received,
                            });
                        }
                        Err(error) => {
                            self.clear_portal_pointer_session(&session);
                            return Json(portal_action_error("drag", error, received));
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
            run_ydotool_drag(params.start_x, params.start_y, params.end_x, params.end_y).await
        })
        .await;
        let _input_guard = input_guard;
        Json(action_result("drag", result, received))
    }

    #[tool(
        name = "press_key",
        description = "Press a key or key-combination on the keyboard, optionally after focusing a target window or terminal selector. Pass `key` for one key or chord, or `keys` (an array in the same grammar) to send a sequence in one call with a short delay between entries; exactly one of the two must be given. Key grammar (case-insensitive; hyphens/spaces ignored): combos join with '+', e.g. Ctrl+L or Ctrl+Shift+T. Modifiers: ctrl/control, alt/option, shift, meta/super/cmd/command. Named keys: enter/return, escape/esc, tab, backspace, delete/del, space, home, end, pageup, pagedown, arrowleft/left, arrowright/right, arrowup/up, arrowdown/down, f1-f12. Plus single US letters a-z and digits 0-9. Anything else returns an error (never silently dropped). On Wayland, chords are sent through an active remote desktop portal keyboard session when one is available (or when ydotool is absent), falling back to ydotool otherwise. Note: compositor-level shortcuts (e.g. Super+Up) may be consumed by GNOME before reaching the app.",
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
        if let Err(message) = self
            .input_gate("press_key", Some(&params.window_target()))
            .await
        {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "press_key".to_string(),
                message,
                received,
            });
        }
        let keys = match press_key_sequence(params.key.as_deref(), &params.keys) {
            Ok(keys) => keys,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "press_key".to_string(),
                    message,
                    received,
                });
            }
        };
        let mut input_guard = Some(Arc::clone(&self.input_operation_lock).lock_owned().await);
        let focus = match self.focus_target_for_input(&params.window_target()).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "press_key".to_string(),
                    message,
                    received,
                });
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
        let Some((chord_modifiers, chord_key)) = key_chord(key) else {
            return (
                Some(input_guard),
                ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "press_key".to_string(),
                    message: format!(
                        "Unsupported key {key:?}. Use names like Enter, Escape, Tab, ArrowLeft, Super, Ctrl+L, or a single US keyboard letter/digit."
                    ),
                    received,
                },
            );
        };
        if self.should_prefer_portal_keyboard_for_chords().await {
            match self.ensure_portal_keyboard_session().await {
                Ok(Some(session)) => {
                    let modifiers: Vec<i32> =
                        chord_modifiers.iter().map(|m| i32::from(*m)).collect();
                    match press_keycode_chord(&session, &modifiers, i32::from(chord_key)).await {
                        Ok(()) => {
                            return (
                                Some(input_guard),
                                successful_action_with_focus(
                                    "press_key",
                                    "Action sent through the remote desktop portal.",
                                    received,
                                    focus,
                                ),
                            );
                        }
                        Err(error) => {
                            self.clear_portal_keyboard_session(&session);
                            return (
                                Some(input_guard),
                                action_result_with_focus(
                                    "press_key",
                                    Err(format!("{error:#}")),
                                    received,
                                    focus,
                                ),
                            );
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        let Some(key_events) = key_sequence(key) else {
            return (
                Some(input_guard),
                ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "press_key".to_string(),
                    message: format!(
                        "Unsupported key {key:?}. Use names like Enter, Escape, Tab, ArrowLeft, Super, Ctrl+L, or a single US keyboard letter/digit."
                    ),
                    received,
                },
            );
        };
        // X11: prefer xdotool/XTEST. ydotool's raw evdev scancodes get
        // re-mapped by the active XKB layout on X11, so named keys and chords
        // arrive as stray glyphs instead of real key events (issue #58).
        if self.should_prefer_xdotool_keyboard() {
            if let Some(spec) = xdotool_key_spec(key) {
                let xdotool_args = vec!["key".to_string(), "--clearmodifiers".to_string(), spec];
                let ydotool_args =
                    ydotool_key_args(key_events.clone(), !chord_modifiers.is_empty());
                let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
                    run_xdotool_or_fallback(Path::new("xdotool"), &xdotool_args, || {
                        run_ydotool(&ydotool_args)
                    })
                    .await
                })
                .await;
                let used_xdotool = result
                    .as_ref()
                    .is_ok_and(|result| result.backend == KeyboardCommandBackend::Xdotool);
                let mut output = action_result_with_focus(
                    "press_key",
                    result.map(|result| vec![result.output]),
                    received,
                    focus,
                );
                if used_xdotool {
                    output.message = "Action sent through xdotool (X11 XTEST).".to_string();
                }
                return (input_guard, output);
            }
        }
        let args = ydotool_key_args(key_events, !chord_modifiers.is_empty());
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
        name = "run_shell",
        description = "Execute one explicitly approved /bin/sh command with same-user host authority. This tool is absent unless the server operator starts computer-use-linux with COMPUTER_USE_LINUX_ENABLE_SHELL=1. It is not sandboxed: the command can read or modify files and use the network with the server user's permissions. The inherited environment is cleared to a small desktop/runtime allowlist; pass any additional variables explicitly. Execution time and output are bounded: returned streams are truncated to 512 KiB, while a stream exceeding the 8 MiB collection ceiling fails the call without returning partial output. An audit digest is written to server stderr.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false,
            open_world_hint = true
        )
    )]
    async fn run_shell(
        &self,
        Parameters(params): Parameters<RunShellParams>,
    ) -> Json<RunShellOutput> {
        Json(execute_shell(params).await)
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
        if let Err(message) = self
            .input_gate("type_text", Some(&params.window_target()))
            .await
        {
            return Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "type_text".to_string(),
                message,
                received,
            });
        }
        let input_guard = Arc::clone(&self.input_operation_lock).lock_owned().await;
        let focus = match self.focus_target_for_input(&params.window_target()).await {
            Ok(focus) => focus,
            Err(message) => {
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "type_text".to_string(),
                    message,
                    received,
                });
            }
        };
        if self.should_prefer_kde_clipboard_text_backend() {
            match self.ensure_portal_keyboard_session().await {
                Ok(Some(session)) => {
                    let _clipboard_guard = self.kde_clipboard_lock.lock().await;
                    match run_kde_clipboard_paste_text(&session, &params.text).await {
                        Ok(message) => {
                            let notes = self.input_landing_notes(focus.as_ref(), true).await;
                            return Json(with_notes(
                                successful_action_with_focus(
                                    "type_text",
                                    &message,
                                    received,
                                    focus,
                                ),
                                notes,
                            ));
                        }
                        Err(error) => {
                            if error.clear_portal_keyboard_session {
                                self.clear_portal_keyboard_session(&session);
                            }
                            if !error.can_fallback_to_ydotool {
                                return Json(action_result_with_focus(
                                    "type_text",
                                    Err(error.message),
                                    received,
                                    focus,
                                ));
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        if self.should_prefer_portal_keyboard_backend().await {
            if let Ok(keysyms) = keysyms_for_text(&params.text) {
                match self.ensure_portal_keyboard_session().await {
                    Ok(Some(session)) => match type_text_with_keysyms(&session, &keysyms).await {
                        Ok(()) => {
                            let notes = self.input_landing_notes(focus.as_ref(), true).await;
                            return Json(with_notes(
                                successful_action_with_focus(
                                    "type_text",
                                    "Action sent through the remote desktop portal.",
                                    received,
                                    focus,
                                ),
                                notes,
                            ));
                        }
                        Err(error) => {
                            self.clear_portal_keyboard_session(&session);
                            return Json(action_result_with_focus(
                                "type_text",
                                Err(format!("{error:#}")),
                                received,
                                focus,
                            ));
                        }
                    },
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
        }
        // X11: xdotool type resolves keysyms against the live XKB layout.
        // ydotool's raw scancodes get re-mapped by X11 and mangle symbols and
        // digits (`_` → `%`, `1` → `+`) even on a plain US layout (issue #58).
        if self.should_prefer_xdotool_keyboard() {
            let args = xdotool_type_args(&params.text);
            let text = params.text.clone();
            let (input_guard, result) = run_cancellation_safe_input(input_guard, async move {
                run_xdotool_or_fallback(Path::new("xdotool"), &args, || {
                    run_ydotool_type_text(&text)
                })
                .await
            })
            .await;
            let _input_guard = input_guard;
            let used_xdotool = result
                .as_ref()
                .is_ok_and(|result| result.backend == KeyboardCommandBackend::Xdotool);
            let mut output = action_result_with_focus(
                "type_text",
                result.map(|result| vec![result.output]),
                received,
                focus.clone(),
            );
            if used_xdotool {
                output.message = "Action sent through xdotool (X11 XTEST).".to_string();
            }
            if output.ok {
                let notes = self.input_landing_notes(focus.as_ref(), true).await;
                output = with_notes(output, notes);
            }
            return Json(output);
        }
        if self.should_prefer_wtype_keyboard() {
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
        description = "Move a window to a new desktop position (frame top-left in desktop coordinates). Useful to recover windows that are partially off-screen. Works through the computer-use-linux GNOME Shell extension or a generic X11/EWMH window manager (wmctrl).",
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
        description = "Resize a window to a new frame width/height in desktop pixels, unmaximizing it first if needed. Useful to fit a window fully on-screen. Works through the computer-use-linux GNOME Shell extension or a generic X11/EWMH window manager (wmctrl).",
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
}

#[tool_handler(
    router = self.mcp_tool_router(),
    name = "computer-use-linux",
    // NOTE: keep in lockstep with Cargo.toml + package.json on every release.
    // The rmcp tool_handler macro only accepts a string literal here, so this
    // can't be env!("CARGO_PKG_VERSION"); the MCP safety check (CI) fails the
    // build if it drifts from the Cargo version.
    version = "0.5.0",
    instructions = "Begin every turn that uses Computer Use by calling get_app_state. If diagnostics report disabled GNOME accessibility, call setup_accessibility before asking the user to retry. Use list_windows/focused_window before targeted keyboard input. If diagnostics report windowing.can_list_windows=false on GNOME, call setup_window_targeting to install the optional GNOME Shell extension backend, then ask the user to log out and back in if the setup report says a shell reload is required. This Linux backend can capture size-bounded screenshots through GNOME Shell or XDG Desktop Portal, read AT-SPI trees with action/value metadata, invoke native AT-SPI actions, set AT-SPI values or editable text, list/focus compositor windows through registered Linux window backends when the session permits it, attach best-effort terminal tty/process metadata to terminal windows, send coordinate or element-targeted click/scroll/drag input through the Wayland remote desktop portal when available, and send layout-safe literal type_text through KDE clipboard integration on Plasma Wayland or through portal keysyms on other Wayland sessions before falling back to ydotool. Screenshot results include width/height for the returned image plus coordinate_width/coordinate_height and scale for desktop coordinate conversion; request more detail with max_width, max_height, max_bytes, format=jpeg, quality, or a smaller target/crop instead of relying on unbounded screenshots. Tools with readOnlyHint=false may mutate local desktop or application state; hosts should require approval for actions that can submit, delete, send, purchase, or overwrite data. For element-targeted actions, prefer element_index from the latest get_app_state result; click, perform_action, and set_value can also use semantic role/name/text/states selectors when the target is unique. type_text and press_key accept optional window_id, pid, app_id, wm_class, title, tty, terminal_pid, terminal_command, or terminal_cwd selectors and refuse targeted input if focus cannot be verified. After click, drag, perform_action, press_key, and type_text, results append focused-element feedback from AT-SPI (role, name, editable, states) and warn when no editable element holds focus after typing — treat that warning as the input not landing; element clicks and actions also report the element's states before -> after when they changed. When an element operation answers that the cached accessibility tree is stale, call get_app_state again before retrying. wait_for polls until an element selector, a window title substring, or a focused window holds and returns the element with its index in a freshly cached tree. pointer_position reports the pointer's desktop coordinates on Hyprland and X11. The first input action of this process takes a machine-wide session lock; another server process gets ok=false naming the holder's pid. When COMPUTER_USE_LINUX_ALLOWED_APPS is set, input tools refuse windows matching none of its app_id/wm_class/title patterns. Screenshot, click, and input results warn when the target window or coordinate is partially or fully off-screen; use move_window/resize_window (GNOME Shell extension, Hyprland, or X11 backend) to bring a window fully on-screen before retrying. scroll accepts the same window targeting and relative coordinates as click. get_app_state returns a compact readiness block by default; pass verbose=true for the full diagnostics dump. Electron apps expose no AT-SPI tree unless launched with --force-renderer-accessibility."
)]
impl ServerHandler for ComputerUseLinux {}

/// The `COMPUTER_USE_LINUX_ALLOWED_APPS` patterns, or `None` when the
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
/// its app_id, wm_class, or title.
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

fn shell_execution_enabled() -> bool {
    shell_execution_enabled_value(env::var(SHELL_ENABLE_ENV).ok().as_deref())
}

fn shell_execution_enabled_value(value: Option<&str>) -> bool {
    value == Some("1")
}

fn valid_environment_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric())
}

fn inherited_shell_environment_allowed(name: &str) -> bool {
    matches!(
        name,
        "PATH"
            | "HOME"
            | "USER"
            | "LOGNAME"
            | "LANG"
            | "TERM"
            | "XDG_RUNTIME_DIR"
            | "DISPLAY"
            | "WAYLAND_DISPLAY"
            | "DBUS_SESSION_BUS_ADDRESS"
    ) || name.starts_with("LC_")
}

fn inherited_shell_environment(
    variables: impl IntoIterator<Item = (OsString, OsString)>,
) -> Vec<(String, String)> {
    variables
        .into_iter()
        .filter_map(|(name, value)| {
            let (Ok(name), Ok(value)) = (name.into_string(), value.into_string()) else {
                return None;
            };
            inherited_shell_environment_allowed(&name).then_some((name, value))
        })
        .collect()
}

fn shell_command_sha256(command: &str) -> String {
    Sha256::digest(command.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn bounded_shell_stream(bytes: &[u8]) -> (String, bool) {
    let truncated = bytes.len() > SHELL_RESPONSE_STREAM_BYTES;
    let visible = if truncated {
        &bytes[..SHELL_RESPONSE_STREAM_BYTES]
    } else {
        bytes
    };
    (String::from_utf8_lossy(visible).into_owned(), truncated)
}

fn shell_error_output(
    command_sha256: String,
    cwd: String,
    timeout_seconds: u64,
    error: impl Into<String>,
) -> RunShellOutput {
    RunShellOutput {
        ok: false,
        command_sha256,
        cwd,
        timeout_seconds,
        exit_code: None,
        stdout: String::new(),
        stderr: String::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        error: Some(error.into()),
    }
}

async fn execute_shell(params: RunShellParams) -> RunShellOutput {
    let command_sha256 = shell_command_sha256(&params.command);
    let timeout_seconds = params.timeout_seconds.unwrap_or(SHELL_DEFAULT_TIMEOUT_SECS);
    let requested_cwd = params.cwd.as_deref().unwrap_or(".").to_string();

    if !shell_execution_enabled() {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!(
                "shell execution is disabled; restart the MCP server with {SHELL_ENABLE_ENV}=1 to opt in"
            ),
        );
    }
    if params.command.trim().is_empty() {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            "command must not be empty",
        );
    }
    if params.command.len() > SHELL_MAX_COMMAND_BYTES {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!("command exceeds the {SHELL_MAX_COMMAND_BYTES}-byte limit"),
        );
    }
    if requested_cwd.len() > SHELL_MAX_CWD_BYTES {
        return shell_error_output(
            command_sha256,
            "<rejected: cwd too long>".to_string(),
            timeout_seconds,
            format!("cwd exceeds the {SHELL_MAX_CWD_BYTES}-byte limit"),
        );
    }
    if !(1..=SHELL_MAX_TIMEOUT_SECS).contains(&timeout_seconds) {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!("timeout_seconds must be between 1 and {SHELL_MAX_TIMEOUT_SECS}"),
        );
    }
    if params.env.len() > SHELL_MAX_ENV_ENTRIES {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!("env contains more than {SHELL_MAX_ENV_ENTRIES} entries"),
        );
    }
    let env_bytes = params
        .env
        .iter()
        .map(|(name, value)| name.len().saturating_add(value.len()))
        .sum::<usize>();
    if env_bytes > SHELL_MAX_ENV_BYTES {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!("env exceeds the {SHELL_MAX_ENV_BYTES}-byte limit"),
        );
    }
    if let Some(invalid) = params.env.keys().find(|name| !valid_environment_name(name)) {
        return shell_error_output(
            command_sha256,
            requested_cwd,
            timeout_seconds,
            format!("invalid environment variable name: {invalid}"),
        );
    }

    let cwd = match std::fs::canonicalize(&requested_cwd) {
        Ok(path) if path.is_dir() => path,
        Ok(_) => {
            return shell_error_output(
                command_sha256,
                requested_cwd,
                timeout_seconds,
                "cwd is not a directory",
            )
        }
        Err(error) => {
            return shell_error_output(
                command_sha256,
                requested_cwd,
                timeout_seconds,
                format!("failed to resolve cwd: {error}"),
            )
        }
    };
    let cwd_display = cwd.display().to_string();

    let mut child = TokioCommand::new("/bin/sh");
    // Do not use a login shell: profile scripts could reintroduce credentials
    // after env_clear() and contaminate or prevent the requested command.
    child.args(["-c", &params.command]);
    child.current_dir(&cwd);
    child.env_clear();
    let mut inherited_path = false;
    for (name, value) in inherited_shell_environment(env::vars_os()) {
        inherited_path |= name == "PATH";
        child.env(name, value);
    }
    if !inherited_path {
        child.env("PATH", "/usr/local/bin:/usr/bin:/bin");
    }
    child.envs(&params.env);

    eprintln!(
        "[computer-use-linux] run_shell start sha256={command_sha256} cwd={cwd_display:?} timeout_seconds={timeout_seconds}"
    );
    match crate::command_runner::output_with_timeout(
        child,
        "run approved shell command",
        Duration::from_secs(timeout_seconds),
    )
    .await
    {
        Ok(output) => {
            let exit_code = output.status.code();
            let (stdout, stdout_truncated) = bounded_shell_stream(&output.stdout);
            let (stderr, stderr_truncated) = bounded_shell_stream(&output.stderr);
            eprintln!(
                "[computer-use-linux] run_shell finish sha256={command_sha256} exit_code={exit_code:?} stdout_bytes={} stderr_bytes={} stdout_truncated={stdout_truncated} stderr_truncated={stderr_truncated}",
                output.stdout.len(),
                output.stderr.len()
            );
            RunShellOutput {
                ok: output.status.success(),
                command_sha256,
                cwd: cwd_display,
                timeout_seconds,
                exit_code,
                stdout,
                stderr,
                stdout_truncated,
                stderr_truncated,
                error: None,
            }
        }
        Err(error) => {
            let error = format!("{error:#}");
            eprintln!(
                "[computer-use-linux] run_shell error sha256={command_sha256} error={error:?}"
            );
            shell_error_output(command_sha256, cwd_display, timeout_seconds, error)
        }
    }
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
struct MoveWindowParams {
    #[serde(flatten)]
    target: ActivateWindowParams,
    /// New frame-left in desktop coordinates.
    x: i32,
    /// New frame-top in desktop coordinates.
    y: i32,
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct RunShellParams {
    /// Shell program text passed to `/bin/sh -c` without loading login profiles.
    command: String,
    /// Existing directory in which to start the shell. Symlinks are resolved.
    #[serde(default)]
    cwd: Option<String>,
    /// Additional environment entries. The ambient process environment is not inherited wholesale.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// Wall-clock timeout in seconds (default 30, maximum 120).
    #[serde(default)]
    timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
struct RunShellOutput {
    ok: bool,
    command_sha256: String,
    cwd: String,
    timeout_seconds: u64,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    stdout_truncated: bool,
    stderr_truncated: bool,
    error: Option<String>,
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
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
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
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
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
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        })
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
    let capture_rect = (
        left as i32,
        top as i32,
        (right - left) as u32,
        (bottom - top) as u32,
    );
    let desktop_rect = (
        (left + capture_left) as i32,
        (top + capture_top) as i32,
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
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    title: Option<String>,
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
            window_id: self.window_id,
            pid: self.pid,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
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
    /// The `object_ref` string of a node from the latest get_app_state result,
    /// as an alternative to element_index.
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
    /// press_key names). Forces the pointer path even when the element has an
    /// AT-SPI click action.
    #[serde(default)]
    modifiers: Vec<String>,
    // Optional window target: when set, the window is raised/focused before the
    // click so a coordinate click reliably lands on the intended app rather than
    // whatever window happens to be stacked on top at that pixel.
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
    /// Interpret `x`/`y` as relative to the targeted window's top-left corner
    /// (the same coordinate space as a window-cropped `screenshot`). Requires a
    /// window target; ignored otherwise.
    #[serde(default)]
    relative: Option<bool>,
}

impl ClickParams {
    /// A window target if any window-identifying field was supplied.
    fn window_target(&self) -> Option<WindowTarget> {
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.window_title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.window_title.clone(),
        })
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
    /// The `object_ref` string of a node from the latest get_app_state result,
    /// as an alternative to element_index.
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
    /// The `object_ref` string of a node from the latest get_app_state result,
    /// as an alternative to element_index.
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

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
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
    #[serde(default)]
    window_id: Option<u64>,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    app_id: Option<String>,
    #[serde(default)]
    wm_class: Option<String>,
    #[serde(default)]
    window_title: Option<String>,
    /// Interpret `x`/`y` as relative to the targeted window's top-left corner
    /// (the same coordinate space as a window-cropped `screenshot`). Requires a
    /// window target; ignored otherwise.
    #[serde(default)]
    relative: Option<bool>,
}

impl ScrollParams {
    /// A window target if any window-identifying field was supplied.
    fn window_target(&self) -> Option<WindowTarget> {
        if self.window_id.is_none()
            && self.pid.is_none()
            && self.app_id.is_none()
            && self.wm_class.is_none()
            && self.window_title.is_none()
        {
            return None;
        }
        Some(WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: None,
            terminal_pid: None,
            terminal_command: None,
            terminal_cwd: None,
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.window_title.clone(),
        })
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
struct DragParams {
    start_x: i32,
    start_y: i32,
    end_x: i32,
    end_y: i32,
    /// Modifier keys held around the drag (ctrl/alt/shift/meta, the press_key
    /// names).
    #[serde(default)]
    modifiers: Vec<String>,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
struct TypeTextParams {
    text: String,
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

impl PressKeyParams {
    fn window_target(&self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
    }
}

impl TypeTextParams {
    fn window_target(&self) -> WindowTarget {
        WindowTarget {
            window_id: self.window_id,
            pid: self.pid,
            tty: self.tty.clone(),
            terminal_pid: self.terminal_pid,
            terminal_command: self.terminal_command.clone(),
            terminal_cwd: self.terminal_cwd.clone(),
            app_id: self.app_id.clone(),
            wm_class: self.wm_class.clone(),
            title: self.title.clone(),
        }
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
    fn is_wayland_session(&self) -> bool {
        crate::diagnostics::hydrate_session_bus_env();
        let session_type = env::var("XDG_SESSION_TYPE").ok();
        let wayland_display = env::var("WAYLAND_DISPLAY").ok();
        session_is_wayland(session_type.as_deref(), wayland_display.as_deref())
    }

    // The Wayland remote-desktop portal is now a *fallback* for input: when a
    // compatible ydotool CLI and working `ydotoold` socket are present we prefer
    // ydotool, because it injects input without a permission prompt. GNOME
    // refuses to persist remote-desktop
    // grants (`org.freedesktop.portal.Error: Remote desktop sessions cannot
    // persist`), so the portal would otherwise re-prompt on every new session.
    // `COMPUTER_USE_LINUX_FORCE_YDOTOOL_*=1` always uses ydotool;
    // `COMPUTER_USE_LINUX_FORCE_PORTAL_*=1` always uses the portal.
    async fn should_prefer_portal_pointer_backend(&self) -> bool {
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_POINTER") {
            return false;
        }
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_PORTAL_POINTER") {
            return self.is_wayland_session();
        }
        should_prefer_portal_backend_by_default(
            self.is_wayland_session(),
            ydotool_backend_available().await,
        )
    }

    async fn should_prefer_portal_keyboard_backend(&self) -> bool {
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD") {
            return false;
        }
        if self.should_prefer_xdotool_keyboard() {
            return false;
        }
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_PORTAL_KEYBOARD") {
            return self.is_wayland_session() && !self.is_kde_wayland_session();
        }
        !self.is_kde_wayland_session()
            && should_prefer_portal_backend_by_default(
                self.is_wayland_session(),
                ydotool_backend_available().await,
            )
    }

    /// Portal keyboard policy for `press_key` chords. Unlike literal text
    /// (where KDE prefers the clipboard paste backend), key chords have no
    /// clipboard route, so the portal keyboard session is preferred on ANY
    /// Wayland session — including Plasma. An already-active keyboard
    /// session (e.g. established by a KDE clipboard paste) is reused even
    /// when ydotool is available, so the consent the user already granted
    /// keeps covering key chords; otherwise the portal is preferred only
    /// when ydotool is absent or the portal is forced.
    async fn should_prefer_portal_keyboard_for_chords(&self) -> bool {
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD") {
            return false;
        }
        if self.should_prefer_xdotool_keyboard() {
            return false;
        }
        if !self.is_wayland_session() {
            return false;
        }
        if self.cached_portal_keyboard_session().is_some()
            || env_flag_enabled("COMPUTER_USE_LINUX_FORCE_PORTAL_KEYBOARD")
        {
            return true;
        }
        !ydotool_backend_available().await
    }

    fn should_prefer_kde_clipboard_text_backend(&self) -> bool {
        !env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD")
            && !self.should_prefer_xdotool_keyboard()
            && self.is_kde_wayland_session()
    }

    /// Keyboard policy for X11 sessions: prefer `xdotool` (XTEST).
    ///
    /// ydotool writes raw evdev scancodes to a virtual uinput device. On X11
    /// the server then re-interprets them through the active XKB layout, so
    /// `press_key "Return"` and chords like `ctrl+a` land as stray characters,
    /// and literal text can mangle symbols/digits (issue #58). XTEST resolves
    /// keysyms against the live layout instead.
    ///
    /// `COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD=1` opts out;
    /// `COMPUTER_USE_LINUX_FORCE_XDOTOOL_KEYBOARD=1` forces it on.
    fn should_prefer_xdotool_keyboard(&self) -> bool {
        prefer_xdotool_keyboard(
            env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD"),
            env_flag_enabled("COMPUTER_USE_LINUX_FORCE_XDOTOOL_KEYBOARD"),
            self.is_wayland_session(),
            env_var_non_empty("DISPLAY"),
            xdotool_available(),
        )
    }

    fn should_prefer_wtype_keyboard(&self) -> bool {
        prefer_wtype_keyboard(
            env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD"),
            self.is_wayland_session(),
            crate::diagnostics::wtype_compatible_wayland_desktop(
                env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
            ),
            wtype_available(),
        )
    }

    fn should_prefer_xdotool_pointer(&self) -> bool {
        crate::diagnostics::hydrate_session_bus_env();
        prefer_xdotool_pointer(
            env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_POINTER"),
            env::var("XDG_SESSION_TYPE").ok().as_deref(),
            env_var_non_empty("DISPLAY"),
            env::var("WAYLAND_DISPLAY").ok().as_deref(),
            xdotool_available(),
        )
    }

    fn is_kde_wayland_session(&self) -> bool {
        self.is_wayland_session()
            && (env_contains("XDG_CURRENT_DESKTOP", "kde")
                || env_contains("DESKTOP_SESSION", "plasma"))
    }

    fn cached_portal_pointer_session(&self) -> Option<PortalPointerSession> {
        let mut cached = self.portal_pointer_session.lock().ok()?;
        if cached.as_ref().is_some_and(|session| !session.is_valid()) {
            *cached = None;
        }
        cached.clone()
    }

    fn clear_portal_pointer_session(&self, failed: &PortalPointerSession) {
        failed.invalidate_and_close();
        if let Ok(mut cached) = self.portal_pointer_session.lock() {
            if cached
                .as_ref()
                .is_some_and(|session| session.same_session(failed))
            {
                *cached = None;
            }
        }
    }

    fn cached_portal_keyboard_session(&self) -> Option<PortalKeyboardSession> {
        let mut cached = self.portal_keyboard_session.lock().ok()?;
        if cached.as_ref().is_some_and(|session| !session.is_valid()) {
            *cached = None;
        }
        cached.clone()
    }

    fn clear_portal_keyboard_session(&self, failed: &PortalKeyboardSession) {
        failed.invalidate_and_close();
        if let Ok(mut cached) = self.portal_keyboard_session.lock() {
            if cached
                .as_ref()
                .is_some_and(|session| session.same_session(failed))
            {
                *cached = None;
            }
        }
    }

    async fn ensure_portal_pointer_session(&self) -> Result<Option<PortalPointerSession>> {
        if !self.should_prefer_portal_pointer_backend().await {
            return Ok(None);
        }
        if let Some(session) = self.cached_portal_pointer_session() {
            return Ok(Some(session));
        }

        let _guard = self.portal_session_init_lock.lock().await;
        if let Some(session) = self.cached_portal_pointer_session() {
            return Ok(Some(session));
        }

        let session = start_portal_pointer_session().await?;
        if let Ok(mut cached) = self.portal_pointer_session.lock() {
            *cached = Some(session.clone());
        }
        Ok(Some(session))
    }

    async fn ensure_portal_keyboard_session(&self) -> Result<Option<PortalKeyboardSession>> {
        if env_flag_enabled("COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD")
            || !self.is_wayland_session()
        {
            return Ok(None);
        }
        if let Some(session) = self.cached_portal_keyboard_session() {
            return Ok(Some(session));
        }

        let _guard = self.portal_session_init_lock.lock().await;
        if let Some(session) = self.cached_portal_keyboard_session() {
            return Ok(Some(session));
        }

        let session = start_portal_keyboard_session().await?;
        if let Ok(mut cached) = self.portal_keyboard_session.lock() {
            *cached = Some(session.clone());
        }
        Ok(Some(session))
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

    async fn window_crop_rect_for_capture(
        &self,
        window: &WindowInfo,
        raw: &RawScreenshotCapture,
    ) -> Result<(i32, i32, u32, u32)> {
        self.window_crop_rect_for_dimensions(window, raw.width, raw.height)
            .await
    }

    async fn window_crop_rect_for_dimensions(
        &self,
        window: &WindowInfo,
        capture_width: u32,
        capture_height: u32,
    ) -> Result<(i32, i32, u32, u32)> {
        Ok(self
            .window_coordinate_map_for_dimensions(window, capture_width, capture_height)
            .await?
            .capture_rect)
    }

    async fn window_coordinate_map_for_dimensions(
        &self,
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
        let monitors = if window.backend == GNOME_SHELL_EXTENSION_BACKEND {
            Some(
                crate::windowing::backends::gnome::extension_monitor_layout()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "GNOME targeted screenshot requires logical monitor geometry: {error:#}"
                        )
                    })?
                    .into_iter()
                    .map(|monitor| (monitor.x, monitor.y, monitor.width, monitor.height))
                    .collect(),
            )
        } else if window.backend == GNOME_SHELL_INTROSPECT_BACKEND {
            crate::windowing::backends::gnome::extension_monitor_layout()
                .await
                .ok()
                .map(|monitors| {
                    monitors
                        .into_iter()
                        .map(|monitor| (monitor.x, monitor.y, monitor.width, monitor.height))
                        .collect()
                })
        } else if window.backend == KWIN_BACKEND {
            Some(vec![
                crate::windowing::backends::kwin::logical_desktop_rect()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "KWin targeted screenshot requires logical workspace geometry: {error:#}"
                        )
                    })?,
            ])
        } else {
            None
        };
        let (full_capture_rect, portal_rect) = match monitors {
            Some(monitors) => (
                logical_window_crop_rect(bounds, &monitors, capture_width, capture_height)?,
                Some(logical_rect),
            ),
            None => (logical_rect, None),
        };
        Ok(WindowCoordinateMap {
            capture_rect: clip_capture_rect(full_capture_rect, capture_width, capture_height)?,
            full_capture_rect,
            portal_rect,
        })
    }

    async fn focused_window_coordinate_map(
        &self,
        focus: &WindowFocusResult,
    ) -> std::result::Result<WindowCoordinateMap, String> {
        let window = focus
            .focused_window
            .as_ref()
            .unwrap_or(&focus.requested_window);
        if !matches!(
            window.backend.as_str(),
            GNOME_SHELL_EXTENSION_BACKEND | GNOME_SHELL_INTROSPECT_BACKEND | KWIN_BACKEND
        ) {
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
                    clip_capture_rect(full_capture_rect, width, height).map_err(|error| {
                        format!("Could not map target-window coordinates: {error:#}")
                    })
                })
                .transpose()?
                .unwrap_or(full_capture_rect);
            return Ok(WindowCoordinateMap {
                capture_rect,
                full_capture_rect,
                portal_rect: None,
            });
        }
        let (_, _, width, height) = self.capture_space_rect().await.ok_or_else(|| {
            "Could not determine screenshot dimensions for window-relative coordinates.".to_string()
        })?;
        self.window_coordinate_map_for_dimensions(window, width as u32, height as u32)
            .await
            .map_err(|error| format!("Could not map target-window coordinates: {error:#}"))
    }

    async fn resolve_accessibility_app_filter(
        &self,
        params: &GetAppStateParams,
        window_context: Option<&WindowInfo>,
    ) -> Option<String> {
        if let Some(explicit) = trimmed_nonempty(params.app_name_or_bundle_identifier.as_deref()) {
            return Some(explicit.to_string());
        }

        let target_pid = window_context.and_then(|window| window.pid).or(params.pid);
        let candidates = accessibility_filter_candidates(window_context);

        if let Some(target_pid) = target_pid {
            if let Ok(apps) = list_accessible_apps(200).await {
                if let Some(object_ref) =
                    select_accessibility_object_ref(&apps, target_pid, &candidates)
                {
                    return Some(object_ref);
                }
            }
        }

        candidates.into_iter().next()
    }

    /// Every input tool passes here first: the machine-wide session lock, then
    /// the `COMPUTER_USE_LINUX_ALLOWED_APPS` check against the window the
    /// action targets (the focused window when it targets none).
    async fn input_gate(
        &self,
        action: &str,
        target: Option<&WindowTarget>,
    ) -> std::result::Result<(), String> {
        crate::session_lock::acquire_input_lock()?;
        let Some(patterns) = allowed_app_patterns(env::var(ALLOWED_APPS_ENV).ok().as_deref())
        else {
            return Ok(());
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
            Ok(())
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

    /// One evaluation of every wait_for predicate. Stops at the first one
    /// that does not hold and says why; a satisfied probe carries the matched
    /// element and has already cached the tree it came from.
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
                    title
                        .map(|title| format!("{title:?}"))
                        .unwrap_or_else(|| "(none)".to_string())
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
                .or(params.pid);
            let nodes = match snapshot_tree(app_filter.as_deref(), target_pid, max_nodes, max_depth)
                .await
            {
                Ok(nodes) => nodes,
                Err(error) => {
                    probe.error = Some(format!("AT-SPI tree extraction failed: {error:#}"));
                    return probe;
                }
            };
            let raw_count = nodes.len();
            let nodes = compact_accessibility_tree(nodes);
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
            self.cache_tree(&nodes, window_context.as_ref());
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

    fn logical_portal_point(
        &self,
        session: &PortalPointerSession,
        x: i32,
        y: i32,
    ) -> Option<(i32, i32)> {
        let capture_size = self.desktop_size.lock().ok().and_then(|guard| *guard);
        session.logical_point_from_capture(x, y, capture_size)
    }

    /// COORDINATE SPACES: window bounds (list_windows / extension frame rects)
    /// and the extension monitor layout are in LOGICAL pixels, while click/
    /// scroll coordinates and screenshot captures are in PHYSICAL capture
    /// pixels. On fractionally-scaled displays the two differ, so each check
    /// below only ever compares values from the same space.
    ///
    /// Logical monitor rectangles from the GNOME Shell extension, for checks
    /// against logical window bounds. None when the extension is unavailable.
    async fn logical_monitor_rects(&self) -> Option<Vec<(i32, i32, i32, i32)>> {
        let monitors = crate::windowing::backends::gnome::extension_monitor_layout()
            .await
            .ok()?;
        (!monitors.is_empty()).then(|| {
            monitors
                .iter()
                .map(|m| (m.x, m.y, m.width, m.height))
                .collect()
        })
    }

    /// Physical capture-space desktop rectangle (union of monitors as captured
    /// by the screenshot pipeline), for checks against click coordinates.
    /// Best-effort; None disables the check.
    async fn capture_space_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let cached = self.desktop_size.lock().ok().and_then(|guard| *guard);
        if let Some((w, h)) = cached {
            return Some((0, 0, w as i32, h as i32));
        }
        // One-time prime: a full-frame capture reveals the desktop size when
        // no prior capture is available.
        let raw = capture_screenshot_raw().await.ok()?;
        self.cache_desktop_size(raw.width, raw.height);
        (raw.width > 0 && raw.height > 0).then_some((0, 0, raw.width as i32, raw.height as i32))
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
        // Window bounds are logical pixels: prefer the extension's logical
        // monitor layout (same space). The physical capture rect is a safe
        // fallback — on scaled displays it is at least as large as the logical
        // union, so it can only under-warn, never false-positive.
        let rects = match self.logical_monitor_rects().await {
            Some(rects) => rects,
            None => vec![self.capture_space_rect().await?],
        };
        let (w, h) = (bounds.width as i64, bounds.height as i64);
        let window_area = w * h;
        let mut visible_area = 0_i64;
        for (mx, my, mw, mh) in &rects {
            let ix = (x as i64).max(*mx as i64);
            let iy = (y as i64).max(*my as i64);
            let ix2 = (x as i64 + w).min(*mx as i64 + *mw as i64);
            let iy2 = (y as i64 + h).min(*my as i64 + *mh as i64);
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

    /// Shared move/resize plumbing: resolve the window target, run the GNOME
    /// Shell extension operation, then re-query bounds to report the result.
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
        if let Err(message) = self
            .input_gate("move_window/resize_window", Some(target))
            .await
        {
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
                if let Some(bounds) = window.as_ref().and_then(|window| window.bounds.as_ref()) {
                    if let Some(note) = self.off_screen_note_for_bounds(bounds).await {
                        message = format!("{message} {note}");
                    }
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
            if let Some(bounds) = bounds {
                if let Some(note) = self.off_screen_note_for_bounds(bounds).await {
                    notes.push(note);
                }
            }
        }
        if let Some(note) = self.focused_element_feedback(focus, expects_editable).await {
            notes.push(note);
        }
        notes
    }

    /// Feedback appended after an action landed: the focused element (as
    /// press_key reports it) and, for an element action, the states that
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
        if let Some((object_ref, before)) = element {
            if let Some(note) = element_states_note(object_ref, before).await {
                notes.push(note);
            }
        }
        notes
    }

    /// A cached element with the `focusable` and `editable` states can take
    /// typed text even without the Value or EditableText interfaces.
    fn cached_node_is_keyboard_editable(&self, object_ref: &str) -> bool {
        let states = self.cached_node_states(object_ref);
        ["focusable", "editable"]
            .iter()
            .all(|wanted| states.iter().any(|state| normalized_equals(state, wanted)))
    }

    /// set_value through the keyboard: focus the element with AT-SPI
    /// GrabFocus, select everything with Ctrl+A, then type the value.
    async fn keyboard_set_value(
        &self,
        object_ref: &str,
        value: &str,
        received: Option<serde_json::Value>,
    ) -> Json<ActionOutput> {
        let fail = |message: String| {
            Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "set_value".to_string(),
                message,
                received: received.clone(),
            })
        };
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

    #[cfg(test)]
    fn cache_nodes(&self, nodes: &[AccessibilityNode]) {
        self.cache_tree(nodes, None);
    }

    fn cache_tree(&self, nodes: &[AccessibilityNode], window: Option<&WindowInfo>) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
            cached.extend_from_slice(nodes);
        }
        if let Ok(mut offset) = self.node_bounds_offset.lock() {
            *offset = window.and_then(|window| {
                window_relative_bounds_offset(nodes, window).map(|offset| BoundsOffset {
                    window_id: window.window_id,
                    offset,
                })
            });
        }
    }

    fn clear_cached_nodes(&self) {
        if let Ok(mut cached) = self.last_nodes.lock() {
            cached.clear();
        }
        if let Ok(mut offset) = self.node_bounds_offset.lock() {
            *offset = None;
        }
    }

    #[cfg(test)]
    fn cached_bounds_offset(&self) -> Option<(i32, i32)> {
        self.node_bounds_offset
            .lock()
            .ok()
            .and_then(|offset| offset.as_ref().map(|offset| offset.offset))
    }

    /// The offset to apply to cached node bounds right now: the window's
    /// current origin from the compositor when the window can be found (it
    /// may have moved since the tree was cached), the cached offset otherwise.
    async fn current_bounds_offset(&self) -> Option<(i32, i32)> {
        let cached = self
            .node_bounds_offset
            .lock()
            .ok()
            .and_then(|offset| offset.clone())?;
        let windows = list_windows().await.unwrap_or_default();
        Some(fresh_bounds_offset(&cached, &windows))
    }

    /// Centre of a cached node's bounds in desktop coordinates, with the
    /// window-origin offset applied when the tree reported window-relative
    /// bounds.
    fn desktop_center_for_node(
        &self,
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

        let point = self.desktop_center_for_node(&node, offset);
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
            object_ref: node.object_ref.clone(),
            action: action.cloned(),
            point,
            bounds_offset: point.and(offset).filter(|offset| *offset != (0, 0)),
            states: node.states,
        })
    }

    fn center_for_cached_node(
        &self,
        element_index: u32,
        offset: Option<(i32, i32)>,
    ) -> Option<(i32, i32)> {
        let cached = self.last_nodes.lock().ok()?;
        let node = cached.iter().find(|node| node.index == element_index)?;
        self.desktop_center_for_node(node, offset)
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
                        "No cached accessibility node for element_index {element_index}. Call get_app_state first."
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
                return Json(ActionOutput {
                    ok: false,
                    implemented: true,
                    action: "perform_action".to_string(),
                    message,
                    received,
                });
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
            Err(error) => Json(ActionOutput {
                ok: false,
                implemented: true,
                action: "perform_action".to_string(),
                message: element_error_message(&error),
                received,
            }),
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

/// Offset that maps the tree's node bounds onto the desktop, or `None` when
/// the bounds already are desktop coordinates.
///
/// The tree reads extents with `CoordType::Screen`, but an accesskit-backed
/// app (GPUI, winit) answers that relative to the window: accesskit's AT-SPI
/// adapter adds the root window origin the app registered through
/// `set_root_window_bounds`, and on Wayland no app registers one because a
/// Wayland client is never told where it sits, so the origin stays at (0, 0).
/// The signature is a top-level frame that reports no desktop origin, either
/// extents starting at (0, 0) or no bounds at all (GPUI's Frame answers
/// `GetExtents` with nothing usable), while the compositor places the window
/// elsewhere. The compositor backend is the only source of the real origin, so
/// its window bounds supply the offset. A frame that reports a real non-zero
/// origin (GTK, Qt) gets no offset. A window at the desktop origin yields
/// `(0, 0)`, which still records that the tree follows the window.
fn window_relative_bounds_offset(
    nodes: &[AccessibilityNode],
    window: &WindowInfo,
) -> Option<(i32, i32)> {
    let frame = nodes
        .iter()
        .filter(|node| is_top_level_frame_role(&node.role))
        .min_by_key(|node| node.depth)?;
    if frame
        .bounds
        .as_ref()
        .is_some_and(|bounds| bounds.x != 0 || bounds.y != 0)
    {
        return None;
    }
    let window_bounds = window.bounds.as_ref()?;
    Some((window_bounds.x?, window_bounds.y?))
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
        compacted_node.index = compacted.len() as u32;
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

fn accessibility_filter_candidates(window_context: Option<&WindowInfo>) -> Vec<String> {
    let Some(window) = window_context else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    push_candidate(&mut candidates, window.title.as_deref());
    push_candidate(&mut candidates, window.wm_class.as_deref());

    if let Some(app_id) = trimmed_nonempty(window.app_id.as_deref()) {
        if !app_id.starts_with("window:") {
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

fn env_contains(key: &str, needle: &str) -> bool {
    env::var(key)
        .ok()
        .is_some_and(|value| value.to_ascii_lowercase().contains(needle))
}

/// True when an environment variable is set to `"1"` (an explicit on switch).
fn env_flag_enabled(key: &str) -> bool {
    env::var(key).ok().as_deref() == Some("1")
}

fn env_var_non_empty(key: &str) -> bool {
    env::var(key)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
}

/// Return the base64 payload of a `data:` URL (or the original string if bare).
fn data_url_payload(data_url: &str) -> String {
    data_url
        .split_once(',')
        .map(|(_, payload)| payload)
        .unwrap_or(data_url)
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

fn native_x11_xdotool_pointer_session(
    session_type: Option<&str>,
    wayland_display: Option<&str>,
) -> bool {
    session_type.is_some_and(|value| value.trim().eq_ignore_ascii_case("x11"))
        && wayland_display.is_none_or(|value| value.trim().is_empty())
}

fn prefer_xdotool_pointer(
    force_ydotool: bool,
    session_type: Option<&str>,
    display_available: bool,
    wayland_display: Option<&str>,
    xdotool_available: bool,
) -> bool {
    !force_ydotool
        && native_x11_xdotool_pointer_session(session_type, wayland_display)
        && display_available
        && xdotool_available
}

fn prefer_xdotool_keyboard(
    force_ydotool: bool,
    force_xdotool: bool,
    is_wayland: bool,
    display_available: bool,
    xdotool_available: bool,
) -> bool {
    !force_ydotool && display_available && xdotool_available && (force_xdotool || !is_wayland)
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
    full_capture_rect: (i32, i32, u32, u32),
    portal_rect: Option<(i32, i32, u32, u32)>,
}

impl WindowCoordinateMap {
    fn portal_point(&self, capture_x: i32, capture_y: i32) -> Option<(i32, i32)> {
        let (portal_x, portal_y, portal_width, portal_height) = self.portal_rect?;
        let (full_x, full_y, full_width, full_height) = self.full_capture_rect;
        Some((
            map_coordinate_between_rects(capture_x, full_x, full_width, portal_x, portal_width),
            map_coordinate_between_rects(capture_y, full_y, full_height, portal_y, portal_height),
        ))
    }
}

fn map_coordinate_between_rects(
    value: i32,
    source_origin: i32,
    source_size: u32,
    target_origin: i32,
    target_size: u32,
) -> i32 {
    let offset = i64::from(value) - i64::from(source_origin);
    let scaled = offset.saturating_mul(i64::from(target_size)) / i64::from(source_size.max(1));
    (i64::from(target_origin) + scaled).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn logical_window_crop_rect(
    bounds: &crate::windowing::WindowBounds,
    monitors: &[(i32, i32, i32, i32)],
    capture_width: u32,
    capture_height: u32,
) -> Result<(i32, i32, u32, u32)> {
    let mut monitors = monitors
        .iter()
        .filter(|(_, _, width, height)| *width > 0 && *height > 0);
    let first = monitors
        .next()
        .ok_or_else(|| anyhow::anyhow!("desktop returned no usable monitor geometry"))?;
    let (mut min_x, mut min_y) = (i64::from(first.0), i64::from(first.1));
    let (mut max_x, mut max_y) = (
        i64::from(first.0) + i64::from(first.2),
        i64::from(first.1) + i64::from(first.3),
    );
    for (x, y, width, height) in monitors {
        min_x = min_x.min(i64::from(*x));
        min_y = min_y.min(i64::from(*y));
        max_x = max_x.max(i64::from(*x) + i64::from(*width));
        max_y = max_y.max(i64::from(*y) + i64::from(*height));
    }
    let logical_width = max_x - min_x;
    let logical_height = max_y - min_y;
    if logical_width <= 0 || logical_height <= 0 || capture_width == 0 || capture_height == 0 {
        anyhow::bail!("screenshot or monitor geometry is empty");
    }
    let scale_x = f64::from(capture_width) / logical_width as f64;
    let scale_y = f64::from(capture_height) / logical_height as f64;
    if !scale_x.is_finite() || !scale_y.is_finite() || (scale_x - scale_y).abs() > 0.01 {
        anyhow::bail!(
            "captured desktop {}x{} does not have a uniform scale relative to the logical monitor layout {}x{}",
            capture_width,
            capture_height,
            logical_width,
            logical_height
        );
    }

    let x = i64::from(
        bounds
            .x
            .ok_or_else(|| anyhow::anyhow!("window x is unavailable"))?,
    );
    let y = i64::from(
        bounds
            .y
            .ok_or_else(|| anyhow::anyhow!("window y is unavailable"))?,
    );
    if bounds.width == 0 || bounds.height == 0 {
        anyhow::bail!("window bounds are empty");
    }
    let left = (((x - min_x) as f64) * scale_x).floor() as i64;
    let top = (((y - min_y) as f64) * scale_y).floor() as i64;
    let right = (((x + i64::from(bounds.width) - min_x) as f64) * scale_x).ceil() as i64;
    let bottom = (((y + i64::from(bounds.height) - min_y) as f64) * scale_y).ceil() as i64;
    let width = u32::try_from(right - left)
        .map_err(|_| anyhow::anyhow!("scaled window width is invalid"))?;
    let height = u32::try_from(bottom - top)
        .map_err(|_| anyhow::anyhow!("scaled window height is invalid"))?;
    let left = i32::try_from(left).map_err(|_| anyhow::anyhow!("scaled window x is invalid"))?;
    let top = i32::try_from(top).map_err(|_| anyhow::anyhow!("scaled window y is invalid"))?;
    Ok((left, top, width, height))
}

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
    if relative_x as u32 >= width || relative_y as u32 >= height {
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

/// Point a window-targeted scroll at the centre of the resolved window when
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
    params.x = Some(origin_x.saturating_add((width / 2) as i32));
    params.y = Some(origin_y.saturating_add((height / 2) as i32));
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
    if relative_x < 0 || relative_y < 0 || relative_x as u32 >= width || relative_y as u32 >= height
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
    let x = x.max(0) as u32;
    let y = y.max(0) as u32;
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

fn portal_action_error(
    action: &str,
    error: anyhow::Error,
    received: Option<serde_json::Value>,
) -> ActionOutput {
    ActionOutput {
        ok: false,
        implemented: true,
        action: action.to_string(),
        message: format!(
            "Remote desktop portal {action} may have started before it failed; input was not replayed through another backend: {error:#}"
        ),
        received,
    }
}

fn portal_coordinate_error(action: &str, received: Option<serde_json::Value>) -> ActionOutput {
    ActionOutput {
        ok: false,
        implemented: true,
        action: action.to_string(),
        message: format!(
            "Remote desktop portal {action} was not sent because the coordinate could not be mapped safely to the complete shared desktop; input was not replayed through another backend."
        ),
        received,
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

fn successful_action_with_focus(
    action: &str,
    message: &str,
    received: Option<serde_json::Value>,
    focus: Option<WindowFocusResult>,
) -> ActionOutput {
    with_focus_context(
        ActionOutput {
            ok: true,
            implemented: true,
            action: action.to_string(),
            message: message.to_string(),
            received,
        },
        focus,
    )
}

fn with_focus_context(mut output: ActionOutput, focus: Option<WindowFocusResult>) -> ActionOutput {
    if output.ok {
        if let Some(focus) = focus {
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
            let note = registry::list_note(&backend);
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
                backend: GNOME_SHELL_INTROSPECT_BACKEND.to_string(),
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
        .unwrap_or_else(|| GNOME_SHELL_INTROSPECT_BACKEND.to_string())
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

fn xdotool_pointer_click_args(
    x: i32,
    y: i32,
    count: u32,
    button: Option<&str>,
) -> Option<Vec<String>> {
    let button = xdotool_pointer_button_code(button)?;
    Some(vec![
        "mousemove".to_string(),
        "--".to_string(),
        x.to_string(),
        y.to_string(),
        "click".to_string(),
        "--repeat".to_string(),
        count.to_string(),
        button.to_string(),
    ])
}

fn xdotool_pointer_button_code(button: Option<&str>) -> Option<&'static str> {
    match button.unwrap_or("left").to_ascii_lowercase().as_str() {
        "left" => Some("1"),
        "middle" => Some("2"),
        "right" => Some("3"),
        _ => None,
    }
}

#[derive(Debug)]
struct PointerCommandResult {
    outputs: Vec<Output>,
    backend: KeyboardCommandBackend,
}

async fn run_xdotool_pointer_or_fallback<F, Fut>(
    program: &Path,
    args: &[String],
    fallback: F,
) -> std::result::Result<PointerCommandResult, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = std::result::Result<Vec<Output>, String>>,
{
    match run_xdotool(program, args).await {
        XdotoolAttempt::Unavailable => fallback().await.map(|outputs| PointerCommandResult {
            outputs,
            backend: KeyboardCommandBackend::Ydotool,
        }),
        XdotoolAttempt::Finished(result) => result.map(|output| PointerCommandResult {
            outputs: vec![output],
            backend: KeyboardCommandBackend::Xdotool,
        }),
    }
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
        (Some(error), Ok(_)) => Err(error),
        (None, Err(error)) => Err(error),
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
    match tokio::spawn(async move { (input_guard, operation.await) }).await {
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
        Err(ydotool_output_error(output))
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
        Err(ydotool_output_error(output))
    }
}

fn ydotool_type_timeout(text: &str) -> Duration {
    let text_seconds = (text.chars().count() as u64).div_ceil(YDOTOOL_TYPE_CHARS_PER_SECOND);
    Duration::from_secs(INPUT_COMMAND_TIMEOUT.as_secs().saturating_add(text_seconds))
}

const EVDEV_KEY_LEFTCTRL: i32 = 29;
const EVDEV_KEY_V: i32 = 47;
const KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS: u64 = 1_500;
const KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS: u64 = 5_000;
const KDE_CLIPBOARD_RESTORE_CHARS_PER_SECOND: u64 = 250;

fn kde_clipboard_restore_delay(text: &str) -> Duration {
    let text_delay_ms = (text.chars().count() as u64)
        .saturating_mul(1_000)
        .div_ceil(KDE_CLIPBOARD_RESTORE_CHARS_PER_SECOND);
    Duration::from_millis(text_delay_ms.clamp(
        KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS,
        KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS,
    ))
}

#[derive(Debug)]
struct KdeClipboardPasteError {
    message: String,
    can_fallback_to_ydotool: bool,
    clear_portal_keyboard_session: bool,
}

impl KdeClipboardPasteError {
    fn before_text_input(message: String) -> Self {
        Self {
            message,
            can_fallback_to_ydotool: true,
            clear_portal_keyboard_session: false,
        }
    }

    fn after_portal_input(message: String) -> Self {
        Self {
            message,
            can_fallback_to_ydotool: false,
            clear_portal_keyboard_session: true,
        }
    }
}

async fn run_kde_clipboard_paste_text(
    session: &PortalKeyboardSession,
    text: &str,
) -> std::result::Result<String, KdeClipboardPasteError> {
    let previous = kde_clipboard_contents()
        .await
        .map_err(KdeClipboardPasteError::before_text_input)?;
    kde_set_clipboard_contents(text)
        .await
        .map_err(KdeClipboardPasteError::before_text_input)?;

    let paste_result = press_keycode_chord(session, &[EVDEV_KEY_LEFTCTRL], EVDEV_KEY_V)
        .await
        .map_err(|error| format!("{error:#}"));

    sleep(kde_clipboard_restore_delay(text)).await;
    let restore_result = kde_set_clipboard_contents(&previous).await;

    match (paste_result, restore_result) {
        (Ok(_), Ok(_)) => Ok("Action pasted through KDE clipboard integration.".to_string()),
        (Err(error), Ok(_)) => Err(KdeClipboardPasteError::after_portal_input(error)),
        (Ok(_), Err(restore_error)) => Ok(format!(
            "Action pasted through KDE clipboard integration. Warning: previous KDE clipboard contents could not be restored: {restore_error}"
        )),
        (Err(error), Err(restore_error)) => Err(KdeClipboardPasteError::after_portal_input(
            format!("{error}; previous KDE clipboard contents could not be restored: {restore_error}"),
        )),
    }
}

async fn kde_clipboard_contents() -> std::result::Result<String, String> {
    let connection = kde_clipboard_connection().await?;
    let proxy = kde_clipboard_proxy(&connection).await?;
    let output: String = kde_clipboard_dbus_operation(
        "getClipboardContents",
        proxy.call("getClipboardContents", &()),
    )
    .await?;
    Ok(output)
}

async fn kde_set_clipboard_contents(text: &str) -> std::result::Result<(), String> {
    let connection = kde_clipboard_connection().await?;
    let proxy = kde_clipboard_proxy(&connection).await?;
    let _: () = kde_clipboard_dbus_operation(
        "setClipboardContents",
        proxy.call("setClipboardContents", &(text)),
    )
    .await?;
    Ok(())
}

async fn kde_clipboard_connection() -> std::result::Result<ZbusConnection, String> {
    ZbusConnection::session()
        .await
        .map_err(|error| format!("failed to connect to session bus for KDE clipboard: {error}"))
}

async fn kde_clipboard_proxy(
    connection: &ZbusConnection,
) -> std::result::Result<ZbusProxy<'_>, String> {
    kde_clipboard_dbus_operation(
        "proxy creation",
        ZbusProxy::new(
            connection,
            KDE_KLIPPER_SERVICE,
            KDE_KLIPPER_PATH,
            KDE_KLIPPER_INTERFACE,
        ),
    )
    .await
}

async fn kde_clipboard_dbus_operation<T, F>(
    operation: &'static str,
    future: F,
) -> std::result::Result<T, String>
where
    F: Future<Output = zbus::Result<T>>,
{
    kde_clipboard_dbus_operation_with_timeout(operation, future, KDE_CLIPBOARD_DBUS_TIMEOUT).await
}

async fn kde_clipboard_dbus_operation_with_timeout<T, F>(
    operation: &'static str,
    future: F,
    timeout_duration: Duration,
) -> std::result::Result<T, String>
where
    F: Future<Output = zbus::Result<T>>,
{
    timeout(timeout_duration, future)
        .await
        .map_err(|_| format!("KDE clipboard {operation} timed out"))?
        .map_err(|error| format!("KDE clipboard {operation} failed: {error}"))
}

fn ydotool_output_error(output: Output) -> String {
    command_output_error("ydotool", output)
}

/// X11 keyboard input runs through `xdotool` (XTEST) instead of ydotool.
///
/// ydotool injects raw evdev keycodes into a virtual uinput device. Under X11
/// the server re-interprets those scancodes through the active XKB layout, so
/// named keys and chords land as unrelated glyphs and literal text can mangle
/// symbols/digits (`_` → `%`, `1` → `+`). XTEST resolves keysyms against the
/// live layout, which is what X11 clients actually expect. See issue #58.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyboardCommandBackend {
    Wtype,
    Xdotool,
    Ydotool,
}

struct KeyboardCommandResult {
    output: Output,
    backend: KeyboardCommandBackend,
}

enum XdotoolAttempt {
    Unavailable,
    Finished(std::result::Result<Output, String>),
}

async fn run_xdotool(program: &Path, args: &[String]) -> XdotoolAttempt {
    let mut command = TokioCommand::new(program);
    command.args(args);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.kill_on_drop(true);
    command.process_group(0);

    match command.spawn() {
        Ok(child) => XdotoolAttempt::Finished(
            match crate::command_runner::output_child(child, "run xdotool", INPUT_COMMAND_TIMEOUT)
                .await
                .map_err(|error| format!("{error:#}"))
            {
                Ok(output) if output.status.success() => Ok(output),
                Ok(output) => Err(command_output_error("xdotool", output)),
                Err(error) => Err(error),
            },
        ),
        Err(_) => XdotoolAttempt::Unavailable,
    }
}

async fn run_xdotool_or_fallback<F, Fut>(
    program: &Path,
    args: &[String],
    fallback: F,
) -> std::result::Result<KeyboardCommandResult, String>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = std::result::Result<Output, String>>,
{
    match run_xdotool(program, args).await {
        XdotoolAttempt::Unavailable => fallback().await.map(|output| KeyboardCommandResult {
            output,
            backend: KeyboardCommandBackend::Ydotool,
        }),
        XdotoolAttempt::Finished(result) => result.map(|output| KeyboardCommandResult {
            output,
            backend: KeyboardCommandBackend::Xdotool,
        }),
    }
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
        std::fs::metadata(program)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
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
        return Err(command_output_error("wtype", output));
    }
    Ok(KeyboardCommandResult {
        output,
        backend: KeyboardCommandBackend::Wtype,
    })
}

/// True when `xdotool` can drive this session: an X11 session with `DISPLAY`
/// set and the binary present. `COMPUTER_USE_LINUX_FORCE_YDOTOOL_KEYBOARD=1`
/// opts out; `COMPUTER_USE_LINUX_FORCE_XDOTOOL_KEYBOARD=1` forces it on.
fn xdotool_available() -> bool {
    which_in_path("xdotool")
}

fn wtype_available() -> bool {
    which_in_path("wtype")
}

fn prefer_wtype_keyboard(
    force_ydotool: bool,
    is_wayland: bool,
    compatible_desktop: bool,
    available: bool,
) -> bool {
    !force_ydotool && is_wayland && compatible_desktop && available
}

fn xdotool_type_args(text: &str) -> Vec<String> {
    vec![
        "type".to_string(),
        "--clearmodifiers".to_string(),
        "--delay".to_string(),
        "0".to_string(),
        "--".to_string(),
        text.to_string(),
    ]
}

fn which_in_path(binary: &str) -> bool {
    let Ok(path) = env::var("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| {
        let candidate = dir.join(binary);
        std::fs::metadata(&candidate)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
    })
}

/// Map our key grammar onto an `xdotool key` spec such as `ctrl+a`, `Return`,
/// or `shift+F5`. Returns `None` for keys the grammar does not accept, so the
/// caller keeps its existing "never silently dropped" error.
fn xdotool_key_spec(key: &str) -> Option<String> {
    let parts = key
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let (key_part, modifier_parts) = parts.split_last()?;

    // Validate through the same evdev grammar so both backends accept exactly
    // the same input set.
    key_chord(key)?;

    let mut spec = Vec::new();
    for part in modifier_parts {
        spec.push(xdotool_modifier_name(part)?.to_string());
    }

    if modifier_parts.is_empty() {
        if let Some(bare) = xdotool_modifier_keysym(key_part) {
            return Some(bare.to_string());
        }
    }
    spec.push(xdotool_keysym_name(key_part)?);
    Some(spec.join("+"))
}

fn xdotool_modifier_name(key: &str) -> Option<&'static str> {
    match normalize_key(key).as_str() {
        "ctrl" | "control" => Some("ctrl"),
        "alt" | "option" => Some("alt"),
        "shift" => Some("shift"),
        "meta" | "super" | "cmd" | "command" => Some("super"),
        _ => None,
    }
}

/// Standalone keysym for a bare modifier press (`press_key "Super"`).
fn xdotool_modifier_keysym(key: &str) -> Option<&'static str> {
    match normalize_key(key).as_str() {
        "ctrl" | "control" => Some("ctrl"),
        "alt" | "option" => Some("alt"),
        "shift" => Some("shift"),
        "meta" | "super" | "cmd" | "command" => Some("super"),
        _ => None,
    }
}

fn xdotool_keysym_name(key: &str) -> Option<String> {
    let normalized = normalize_key(key);
    let named = match normalized.as_str() {
        "enter" | "return" => "Return",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "backspace" => "BackSpace",
        "delete" | "del" => "Delete",
        "space" => "space",
        "home" => "Home",
        "end" => "End",
        "pageup" | "page_up" => "Page_Up",
        "pagedown" | "page_down" => "Page_Down",
        "arrowleft" | "left" => "Left",
        "arrowright" | "right" => "Right",
        "arrowup" | "up" => "Up",
        "arrowdown" | "down" => "Down",
        "f1" => "F1",
        "f2" => "F2",
        "f3" => "F3",
        "f4" => "F4",
        "f5" => "F5",
        "f6" => "F6",
        "f7" => "F7",
        "f8" => "F8",
        "f9" => "F9",
        "f10" => "F10",
        "f11" => "F11",
        "f12" => "F12",
        value if value.len() == 1 && value.as_bytes()[0].is_ascii_alphanumeric() => {
            return Some(value.to_string());
        }
        _ => return None,
    };
    Some(named.to_string())
}

fn command_output_error(command: &str, output: Output) -> String {
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

async fn ydotool_backend_available() -> bool {
    ydotool_backend_available_from(
        ydotool_socket_connectable(),
        ydotool::ensure_supported_async().await.is_ok(),
    )
}

fn ydotool_socket_connectable() -> bool {
    if let Some(socket) = explicit_ydotool_socket() {
        return ydotool_socket_connects(&PathBuf::from(socket));
    }
    connectable_ydotool_socket_from(fallback_ydotool_socket_candidates()).is_some()
}

fn ydotool_backend_available_from(socket_available: bool, cli_supported: bool) -> bool {
    socket_available && cli_supported
}

fn should_prefer_portal_backend_by_default(is_wayland: bool, ydotool_available: bool) -> bool {
    is_wayland && !ydotool_available
}

fn explicit_ydotool_socket() -> Option<String> {
    if let Ok(socket) = env::var("YDOTOOL_SOCKET") {
        let socket = socket.trim();
        if !socket.is_empty() {
            return Some(socket.to_string());
        }
    }
    None
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
    if modifier_parts.is_empty() {
        if let Some(modifier) = modifier_keycode(key_part) {
            return Some((Vec::new(), modifier));
        }
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

/// The keys press_key sends, from exactly one of `key` and `keys`.
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

fn user_id() -> Option<String> {
    let output = Command::new("id").arg("-u").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
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
    use crate::atspi_tree::{AccessibilityAction, Bounds};
    use crate::windows::{WindowBounds, GNOME_SHELL_EXTENSION_BACKEND};
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
    fn gnome_window_crop_scales_logical_bounds_to_capture_pixels() {
        let bounds = WindowBounds {
            x: Some(6),
            y: Some(36),
            width: 1357,
            height: 1144,
        };
        let monitors = [(0, 0, 1920, 1200)];

        assert_eq!(
            logical_window_crop_rect(&bounds, &monitors, 2560, 1600).unwrap(),
            (8, 48, 1810, 1526)
        );
    }

    #[test]
    fn portal_points_map_capture_pixels_back_to_logical_window_space() {
        let scaled = WindowCoordinateMap {
            capture_rect: (8, 48, 1810, 1526),
            full_capture_rect: (8, 48, 1810, 1526),
            portal_rect: Some((6, 36, 1357, 1144)),
        };
        assert_eq!(scaled.portal_point(913, 811), Some((684, 608)));

        let clipped = WindowCoordinateMap {
            capture_rect: (0, 0, 50, 60),
            full_capture_rect: (-50, -40, 100, 100),
            portal_rect: Some((-50, -40, 100, 100)),
        };
        assert_eq!(clipped.portal_point(0, 0), Some((0, 0)));
    }

    #[test]
    fn scaled_kwin_bounds_keep_capture_and_portal_spaces_distinct() {
        let bounds = WindowBounds {
            x: Some(1000),
            y: Some(100),
            width: 800,
            height: 600,
        };
        let logical_rect = window_crop_rect(&bounds).unwrap();
        let full_capture_rect =
            logical_window_crop_rect(&bounds, &[(0, 0, 1920, 1080)], 3840, 2160).unwrap();
        let mapping = WindowCoordinateMap {
            capture_rect: full_capture_rect,
            full_capture_rect,
            portal_rect: Some(logical_rect),
        };

        assert_eq!(mapping.capture_rect, (2000, 200, 1600, 1200));
        assert_eq!(mapping.portal_point(2400, 500), Some((1200, 250)));
    }

    #[test]
    fn kwin_mapping_uses_the_workspace_geometry_origin() {
        let bounds = WindowBounds {
            x: Some(1100),
            y: Some(50),
            width: 800,
            height: 600,
        };
        let logical_rect = window_crop_rect(&bounds).unwrap();
        let full_capture_rect =
            logical_window_crop_rect(&bounds, &[(100, -50, 1920, 1080)], 3840, 2160).unwrap();
        let mapping = WindowCoordinateMap {
            capture_rect: full_capture_rect,
            full_capture_rect,
            portal_rect: Some(logical_rect),
        };

        assert_eq!(mapping.capture_rect, (2000, 200, 1600, 1200));
        assert_eq!(mapping.portal_point(2400, 500), Some((1300, 200)));
    }

    #[test]
    fn gnome_window_crop_accounts_for_negative_monitor_origins() {
        let bounds = WindowBounds {
            x: Some(-900),
            y: Some(100),
            width: 400,
            height: 300,
        };
        let monitors = [(-1000, 0, 1000, 800), (0, 0, 1200, 800)];

        assert_eq!(
            logical_window_crop_rect(&bounds, &monitors, 2200, 800).unwrap(),
            (100, 100, 400, 300)
        );
    }

    #[test]
    fn readonly_targeted_screenshot_requires_focused_visible_window() {
        let mut window = window_info(1, Some("Target"), None, None, None);
        assert!(ensure_readonly_screenshot_target_is_visible(&window).is_err());
        window.focused = true;
        assert!(ensure_readonly_screenshot_target_is_visible(&window).is_ok());
        window.hidden = true;
        assert!(ensure_readonly_screenshot_target_is_visible(&window).is_err());
    }

    #[test]
    fn wayland_display_is_enough_to_select_portal_fallback() {
        assert!(session_is_wayland(None, Some("wayland-1")));
        assert!(session_is_wayland(Some("  "), Some("wayland-1")));
        assert!(session_is_wayland(Some("wayland"), None));
        assert!(!session_is_wayland(Some("x11"), None));
        assert!(!session_is_wayland(None, Some("  ")));
    }

    #[test]
    fn incompatible_ydotool_socket_does_not_suppress_portal_fallback() {
        let incompatible_ydotool = ydotool_backend_available_from(true, false);
        let compatible_ydotool = ydotool_backend_available_from(true, true);

        assert!(should_prefer_portal_backend_by_default(
            true,
            incompatible_ydotool
        ));
        assert!(!should_prefer_portal_backend_by_default(
            true,
            compatible_ydotool
        ));
        assert!(!should_prefer_portal_backend_by_default(
            false,
            incompatible_ydotool
        ));
    }

    #[test]
    fn xdotool_keyboard_override_policy_matches_documented_precedence() {
        assert!(prefer_xdotool_keyboard(false, true, true, true, true));
        assert!(!prefer_xdotool_keyboard(true, true, true, true, true));
        assert!(!prefer_xdotool_keyboard(false, true, true, false, true));
        assert!(!prefer_xdotool_keyboard(false, false, true, true, true));
        assert!(prefer_xdotool_keyboard(false, false, false, true, true));
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
            focused: false,
            hidden: false,
            client_type: Some("wayland".to_string()),
            backend: GNOME_SHELL_EXTENSION_BACKEND.to_string(),
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
            Some(2914326),
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
                pid: Some(2774076),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
            AccessibleAppSummary {
                object_ref: ":1.64/org/a11y/atspi/accessible/root".to_string(),
                name: Some("cu_atspi_gtk_test.py".to_string()),
                pid: Some(2914326),
                role: "application".to_string(),
                child_count: 1,
                bounds: None,
            },
        ];

        let object_ref = select_accessibility_object_ref(
            &apps,
            2914326,
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
    fn kde_clipboard_restore_delay_uses_minimum_for_short_text() {
        assert_eq!(
            kde_clipboard_restore_delay("short"),
            Duration::from_millis(KDE_CLIPBOARD_RESTORE_MIN_DELAY_MS)
        );
    }

    #[test]
    fn kde_clipboard_restore_delay_scales_and_caps_long_text() {
        let scaled_text = "x".repeat(1_000);
        assert_eq!(
            kde_clipboard_restore_delay(&scaled_text),
            Duration::from_millis(4_000)
        );

        let capped_text = "x".repeat(10_000);
        assert_eq!(
            kde_clipboard_restore_delay(&capped_text),
            Duration::from_millis(KDE_CLIPBOARD_RESTORE_MAX_DELAY_MS)
        );
    }

    #[tokio::test]
    async fn kde_clipboard_dbus_operation_times_out_when_pending() {
        let error = kde_clipboard_dbus_operation_with_timeout(
            "proxy creation",
            std::future::pending::<zbus::Result<()>>(),
            Duration::from_millis(1),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "KDE clipboard proxy creation timed out");
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
        let cached = backend.node_bounds_offset.lock().unwrap().clone().unwrap();
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
        assert!(press_key_sequence(Some("enter"), &["tab".to_string()]).is_err());
        assert!(press_key_sequence(None, &[]).is_err());
        assert!(press_key_sequence(Some("  "), &[]).is_err());
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
        assert!(modifier_keycodes(&["hyper".to_string()]).is_err());
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
        assert!(region_crop_rect(&region, true, None, 1920, 1080)
            .unwrap_err()
            .contains("window target"));
        let outside = ScreenshotRegion {
            x: 5000,
            y: 0,
            width: 10,
            height: 10,
        };
        assert!(region_crop_rect(&outside, false, None, 1920, 1080)
            .unwrap_err()
            .contains("outside"));
        let empty = ScreenshotRegion {
            x: 0,
            y: 0,
            width: 0,
            height: 10,
        };
        assert!(region_crop_rect(&empty, false, None, 1920, 1080).is_err());
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
        assert!(backend
            .cached_scroll_action(4, ScrollDirection::Up)
            .is_none());
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
            pid: Some(42),
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
            pid: Some(4242),
            title: Some("Sophia".to_string()),
            role: Some("button".to_string()),
            ..Default::default()
        };
        let app_state = params.app_state_params();
        assert_eq!(app_state.pid, Some(4242));
        assert_eq!(app_state.title.as_deref(), Some("Sophia"));
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
    fn native_x11_pointer_policy_requires_explicit_x11_without_wayland_display() {
        assert!(native_x11_xdotool_pointer_session(Some("x11"), None));
        assert!(!native_x11_xdotool_pointer_session(
            Some("wayland"),
            Some("wayland-0")
        ));
        assert!(!native_x11_xdotool_pointer_session(
            Some("x11"),
            Some("wayland-0")
        ));
    }

    #[test]
    fn xdotool_pointer_command_is_single_no_sync_move_and_click() {
        assert_eq!(
            xdotool_pointer_click_args(1550, 930, 3, Some("right")),
            Some(vec![
                "mousemove".to_string(),
                "--".to_string(),
                "1550".to_string(),
                "930".to_string(),
                "click".to_string(),
                "--repeat".to_string(),
                "3".to_string(),
                "3".to_string(),
            ])
        );
    }

    #[test]
    fn xdotool_pointer_policy_requires_all_pure_gating_conditions() {
        let eligible = (false, Some("x11"), true, None, true);
        assert!(prefer_xdotool_pointer(
            eligible.0, eligible.1, eligible.2, eligible.3, eligible.4
        ));
        assert!(!prefer_xdotool_pointer(true, Some("x11"), true, None, true));
        assert!(!prefer_xdotool_pointer(
            false,
            Some("wayland"),
            true,
            None,
            true
        ));
        assert!(!prefer_xdotool_pointer(
            false,
            None,
            true,
            Some("wayland-0"),
            true
        ));
        assert!(!prefer_xdotool_pointer(
            false,
            Some("x11"),
            false,
            None,
            true
        ));
        assert!(!prefer_xdotool_pointer(
            false,
            Some("x11"),
            true,
            None,
            false
        ));
    }

    #[test]
    fn xdotool_pointer_supports_only_standard_buttons() {
        assert!(xdotool_pointer_click_args(10, 20, 1, None).is_some());
        assert!(xdotool_pointer_click_args(10, 20, 1, Some("middle")).is_some());
        assert!(xdotool_pointer_click_args(10, 20, 1, Some("right")).is_some());
    }

    #[test]
    fn extended_pointer_buttons_do_not_construct_xdotool_commands() {
        for button in ["side", "extra", "forward", "back"] {
            assert_eq!(xdotool_pointer_click_args(10, 20, 1, Some(button)), None);
        }
    }

    #[tokio::test]
    async fn pointer_xdotool_spawn_failure_uses_ydotool_fallback() {
        let result = run_xdotool_pointer_or_fallback(
            Path::new("/definitely/missing/xdotool"),
            &[],
            || async { Ok::<_, String>(Vec::new()) },
        )
        .await
        .expect("spawn failure should use fallback");

        assert_eq!(result.backend, KeyboardCommandBackend::Ydotool);
    }

    #[tokio::test]
    async fn pointer_xdotool_nonzero_exit_does_not_use_ydotool_fallback() {
        let result = run_xdotool_pointer_or_fallback(
            Path::new("/bin/sh"),
            &["-c".to_string(), "exit 9".to_string()],
            || async { Err::<Vec<Output>, _>("fallback called".to_string()) },
        )
        .await;

        let error = result.expect_err("launched nonzero xdotool must be terminal");
        assert!(!error.contains("fallback called"));
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

    #[test]
    fn xdotool_key_spec_maps_named_keys_to_x11_keysyms() {
        assert_eq!(xdotool_key_spec("Return"), Some("Return".to_string()));
        assert_eq!(xdotool_key_spec("enter"), Some("Return".to_string()));
        assert_eq!(xdotool_key_spec("Escape"), Some("Escape".to_string()));
        assert_eq!(xdotool_key_spec("backspace"), Some("BackSpace".to_string()));
        assert_eq!(xdotool_key_spec("PageUp"), Some("Page_Up".to_string()));
        assert_eq!(xdotool_key_spec("ArrowLeft"), Some("Left".to_string()));
        assert_eq!(xdotool_key_spec("f5"), Some("F5".to_string()));
        assert_eq!(xdotool_key_spec("space"), Some("space".to_string()));
    }

    #[test]
    fn xdotool_key_spec_maps_chords_with_modifier_prefixes() {
        assert_eq!(xdotool_key_spec("ctrl+a"), Some("ctrl+a".to_string()));
        assert_eq!(xdotool_key_spec("Ctrl+S"), Some("ctrl+s".to_string()));
        assert_eq!(
            xdotool_key_spec("Ctrl+Shift+P"),
            Some("ctrl+shift+p".to_string())
        );
        assert_eq!(
            xdotool_key_spec("Meta+Return"),
            Some("super+Return".to_string())
        );
        assert_eq!(xdotool_key_spec("Alt+F4"), Some("alt+F4".to_string()));
    }

    #[test]
    fn xdotool_key_spec_maps_bare_modifier_to_single_keysym() {
        assert_eq!(xdotool_key_spec("Super"), Some("super".to_string()));
        assert_eq!(xdotool_key_spec("ctrl"), Some("ctrl".to_string()));
    }

    #[test]
    fn xdotool_type_disables_per_character_delay_for_long_input() {
        let text = "x".repeat(10_000);
        let args = xdotool_type_args(&text);

        assert_eq!(
            &args[..5],
            ["type", "--clearmodifiers", "--delay", "0", "--"]
        );
        assert_eq!(args[5], text);
    }

    #[tokio::test]
    async fn launched_xdotool_failure_does_not_replay_through_ydotool() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-linux-xdotool-fallback-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).expect("create command test directory");
        let ydotool = dir.join("ydotool");
        let xdotool_marker = dir.join("xdotool-ran");
        let ydotool_marker = dir.join("ydotool-ran");
        std::fs::write(
            &ydotool,
            format!("#!/bin/sh\ntouch '{}'\n", ydotool_marker.display()),
        )
        .expect("write fake ydotool");
        std::fs::set_permissions(&ydotool, std::fs::Permissions::from_mode(0o700))
            .expect("make fake ydotool executable");
        let xdotool_args = vec![
            "-c".to_string(),
            format!("touch '{}'; exit 9", xdotool_marker.display()),
        ];

        let result = run_xdotool_or_fallback(Path::new("/bin/sh"), &xdotool_args, || async {
            TokioCommand::new(&ydotool)
                .output()
                .await
                .map_err(|error| error.to_string())
        })
        .await;

        assert!(result.is_err());
        assert!(xdotool_marker.exists(), "fake xdotool did not execute");
        assert!(
            !ydotool_marker.exists(),
            "ydotool replayed input after xdotool started"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn unavailable_xdotool_uses_ydotool_fallback() {
        let result = run_xdotool_or_fallback(
            Path::new("/definitely/missing/xdotool"),
            &xdotool_type_args("text"),
            || async {
                TokioCommand::new("sh")
                    .args(["-c", "exit 0"])
                    .output()
                    .await
                    .map_err(|error| error.to_string())
            },
        )
        .await
        .expect("spawn failure should use fallback");

        assert_eq!(result.backend, KeyboardCommandBackend::Ydotool);
        assert!(result.output.status.success());
    }

    #[tokio::test]
    async fn wtype_receives_unicode_text_through_stdin() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-linux-wtype-unicode-{}-{:?}",
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
            "computer-use-linux-wtype-fallback-{}-{:?}",
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
    async fn cancelling_xdotool_wait_kills_the_child() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-linux-xdotool-cancel-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        std::fs::create_dir_all(&dir).expect("create command test directory");
        let xdotool = dir.join("xdotool");
        let pid_path = dir.join("pid");
        std::fs::write(
            &xdotool,
            format!(
                "#!/bin/sh\nprintf '%s' $$ > '{}'\nexec sleep 60\n",
                pid_path.display()
            ),
        )
        .expect("write fake xdotool");
        std::fs::set_permissions(&xdotool, std::fs::Permissions::from_mode(0o700))
            .expect("make fake xdotool executable");

        let task = tokio::spawn(async move { run_xdotool(&xdotool, &[]).await });
        let mut pid = None;
        for _ in 0..200 {
            if let Ok(value) = std::fs::read_to_string(&pid_path) {
                pid = value.parse::<u32>().ok();
                if pid.is_some() {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let pid = pid.expect("fake xdotool did not record its pid");
        task.abort();
        let _ = task.await;

        for _ in 0..50 {
            if !Path::new(&format!("/proc/{pid}")).exists() {
                let _ = std::fs::remove_dir_all(dir);
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(dir);
        panic!("cancelled xdotool child {pid} was not killed");
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

    /// The xdotool path must accept exactly the keys the evdev grammar accepts,
    /// so switching backends can never silently widen or narrow the surface.
    #[test]
    fn xdotool_key_spec_rejects_everything_key_chord_rejects() {
        for key in ["NotAKey", "", "ctrl+", "ctrl+NotAKey", "f13", "hyper+a"] {
            assert_eq!(
                xdotool_key_spec(key).is_some(),
                key_chord(key).is_some(),
                "backend grammars diverged for {key:?}"
            );
        }
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
        let dir =
            std::env::temp_dir().join(format!("computer-use-linux-server-{}", std::process::id()));
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
            "computer-use-linux-server-dgram-{}",
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
        let key = "YDOTOOL_SOCKET";
        let original = std::env::var_os(key);
        std::env::set_var(key, " /does/not/exist.sock ");

        let selected = explicit_ydotool_socket();

        match original {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }

        assert_eq!(selected.as_deref(), Some("/does/not/exist.sock"));
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
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
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
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
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
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
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
            window_id: Some(1),
            pid: None,
            app_id: None,
            wm_class: None,
            window_title: None,
            relative: Some(true),
        };
        assert!(
            apply_window_relative_scroll_coordinates(&mut params, (100, 200, 800, 600)).is_err()
        );
    }

    #[test]
    fn shell_execution_requires_exact_operator_opt_in() {
        assert!(shell_execution_enabled_value(Some("1")));
        for value in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert!(!shell_execution_enabled_value(value));
        }
    }

    #[test]
    fn shell_environment_policy_excludes_ambient_credentials() {
        for allowed in [
            "PATH",
            "HOME",
            "LANG",
            "LC_ALL",
            "XDG_RUNTIME_DIR",
            "DISPLAY",
            "WAYLAND_DISPLAY",
            "DBUS_SESSION_BUS_ADDRESS",
        ] {
            assert!(inherited_shell_environment_allowed(allowed));
        }
        for secret in [
            "AWS_SECRET_ACCESS_KEY",
            "GITHUB_TOKEN",
            "OPENAI_API_KEY",
            "SSH_AUTH_SOCK",
            "LD_PRELOAD",
        ] {
            assert!(!inherited_shell_environment_allowed(secret));
        }
    }

    #[test]
    fn shell_environment_skips_non_utf8_entries() {
        use std::os::unix::ffi::OsStringExt;

        let variables = [
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (OsString::from_vec(vec![0xff]), OsString::from("ignored")),
            (OsString::from("LANG"), OsString::from_vec(vec![0xff])),
            (OsString::from("GITHUB_TOKEN"), OsString::from("secret")),
        ];

        assert_eq!(
            inherited_shell_environment(variables),
            vec![("PATH".to_string(), "/usr/bin".to_string())]
        );
    }

    #[test]
    fn shell_environment_names_follow_exec_rules() {
        for valid in ["A", "_PRIVATE", "NAME_2"] {
            assert!(valid_environment_name(valid));
        }
        for invalid in ["", "2FAST", "BAD-NAME", "A=B", "naïve"] {
            assert!(!valid_environment_name(invalid));
        }
    }

    #[test]
    fn shell_output_is_bounded_before_mcp_serialization() {
        let oversized = vec![b'x'; SHELL_RESPONSE_STREAM_BYTES + 17];
        let (visible, truncated) = bounded_shell_stream(&oversized);
        assert!(truncated);
        assert_eq!(visible.len(), SHELL_RESPONSE_STREAM_BYTES);

        let (visible, truncated) = bounded_shell_stream(b"ok");
        assert!(!truncated);
        assert_eq!(visible, "ok");
    }

    #[test]
    fn shell_audit_digest_is_stable_and_does_not_echo_command_text() {
        let digest = shell_command_sha256("printf secret");
        assert_eq!(digest.len(), 64);
        assert!(digest
            .chars()
            .all(|character| character.is_ascii_hexdigit()));
        assert!(!digest.contains("secret"));
        assert_eq!(digest, shell_command_sha256("printf secret"));
    }
}
