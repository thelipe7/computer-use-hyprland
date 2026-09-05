use crate::windowing::registry::{self, HYPRLAND_BACKEND};
use crate::ydotool;
use schemars::JsonSchema;
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    env, fs,
    fs::OpenOptions,
    os::unix::{fs::MetadataExt, net::UnixDatagram},
    path::{Path, PathBuf},
    process::Command,
    sync::Once,
};

const DESKTOP_ENV_KEYS: &[&str] = &[
    "DBUS_SESSION_BUS_ADDRESS",
    "DESKTOP_SESSION",
    "DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "XAUTHORITY",
    "YDOTOOL_SOCKET",
    "XDG_SESSION_DESKTOP",
    "WAYLAND_DISPLAY",
    "XDG_CURRENT_DESKTOP",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
];
const FORCE_YDOTOOL_KEYBOARD_ENV_KEYS: &[&str] = &["COMPUTER_USE_HYPRLAND_FORCE_YDOTOOL_KEYBOARD"];

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DoctorReport {
    pub platform: PlatformReport,
    pub portals: PortalReport,
    pub accessibility: AccessibilityReport,
    pub windowing: WindowingReport,
    pub input: InputReport,
    pub readiness: ReadinessReport,
    /// Which interchangeable backends this environment supports, per layer, plus
    /// the one the tool prefers. Lets an agent (or selector) understand what's
    /// available and choose accordingly instead of assuming one fixed path.
    pub capabilities: CapabilityMap,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CapabilityMap {
    /// Pointer/keyboard injection backends, best-first.
    pub input: Vec<String>,
    /// Screen capture backends, best-first.
    pub screenshot: Vec<String>,
    /// Window listing/focus backends available.
    pub window_control: Vec<String>,
    /// Accessibility (element-targeted, non-pointer) backends.
    pub accessibility: Vec<String>,
    /// Display/session isolation contexts the host can provide.
    pub isolation: Vec<String>,
    /// The backend the tool will use by default for each selectable layer.
    pub preferred: PreferredBackends,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PreferredBackends {
    pub input: Option<String>,
    pub screenshot: Option<String>,
    pub window_control: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlatformReport {
    pub os: String,
    pub arch: String,
    pub desktop_session: Option<String>,
    pub xdg_session_type: Option<String>,
    pub xdg_current_desktop: Option<String>,
    pub wayland_display: Option<String>,
    pub display: Option<String>,
    pub xauthority: Option<String>,
    pub dbus_session_bus_address: Option<String>,
    pub xdg_runtime_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PortalReport {
    pub desktop_portal: Check,
    /// The only portal interface this build uses: screenshots. Input goes
    /// through uinput, not through the `RemoteDesktop` portal, which Hyprland's
    /// portal does not implement.
    pub screenshot: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityReport {
    pub at_spi_bus: Check,
    pub toolkit_accessibility: Check,
    pub at_spi_enabled: Check,
    pub screen_reader_enabled: Check,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WindowingReport {
    pub hyprland: Check,
    pub backends: BTreeMap<String, Check>,
    pub can_list_windows: bool,
    pub can_focus_apps: bool,
    pub can_focus_windows: bool,
    pub note: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InputReport {
    pub ydotool: Check,
    pub ydotoold: Check,
    pub ydotool_socket: Check,
    pub uinput: Check,
    /// Wayland virtual-keyboard backend for layout-safe Unicode literal text.
    pub wtype: Check,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "a readiness report is a list of yes-or-no answers, and each is a field name in the JSON a caller reads"
)]
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReadinessReport {
    pub can_register_mcp_tools: bool,
    pub can_build_accessibility_tree: bool,
    pub can_query_windows: bool,
    pub can_focus_apps: bool,
    pub can_focus_windows: bool,
    pub can_send_development_input: bool,
    pub recommended_next_step: String,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SetupReport {
    pub before: DoctorReport,
    pub accessibility_command: Check,
    pub after: DoctorReport,
    pub changed_accessibility: bool,
    pub requires_target_app_restart: bool,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Check {
    pub ok: bool,
    pub detail: String,
}

impl Check {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

pub fn doctor_report() -> DoctorReport {
    hydrate_session_bus_env();

    let platform = platform_report();
    let portals = portal_report();
    let accessibility = accessibility_report();
    let windowing = windowing_report();
    let input = input_report();
    let readiness = readiness_report(&platform, &accessibility, &windowing, &input);
    let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);

    DoctorReport {
        platform,
        portals,
        accessibility,
        windowing,
        input,
        readiness,
        capabilities,
    }
}

/// Derive the per-layer backend capability map from the individual checks. Lists
/// are ordered best-first and mirror the order the tool actually tries them.
fn capability_map(
    platform: &PlatformReport,
    portals: &PortalReport,
    accessibility: &AccessibilityReport,
    windowing: &WindowingReport,
    input: &InputReport,
) -> CapabilityMap {
    let mut input_backends = Vec::new();
    // Absolute uinput pointer: accurate, non-blocking of coordinates; preferred.
    if input.uinput.ok {
        input_backends.push("abs_pointer".to_string());
    }
    let force_ydotool = env_flag_enabled_any(FORCE_YDOTOOL_KEYBOARD_ENV_KEYS);
    if platform_is_wayland(platform)
        && wtype_compatible_wayland_desktop(platform.xdg_current_desktop.as_deref())
        && input.wtype.ok
        && !force_ydotool
    {
        input_backends.push("wtype".to_string());
    }
    if input.ydotool.ok && input.ydotool_socket.ok {
        input_backends.push("ydotool".to_string());
    }

    let mut screenshot_backends = Vec::new();
    if portals.screenshot.ok {
        screenshot_backends.push("portal".to_string());
    }

    let mut window_backends = Vec::new();
    if windowing.hyprland.ok {
        window_backends.push(HYPRLAND_BACKEND.to_string());
    }

    let mut accessibility_backends = Vec::new();
    if can_build_accessibility_tree(accessibility) {
        accessibility_backends.push("at_spi".to_string());
    }

    // This build drives the live shared session; it has no headless seat.
    let isolation = vec!["shared".to_string()];

    let preferred = PreferredBackends {
        input: input_backends.first().cloned(),
        screenshot: screenshot_backends.first().cloned(),
        window_control: window_backends.first().cloned(),
    };

    CapabilityMap {
        input: input_backends,
        screenshot: screenshot_backends,
        window_control: window_backends,
        accessibility: accessibility_backends,
        isolation,
        preferred,
    }
}

/// Fill in the desktop variables a bus client needs when this process was
/// started without them: from a systemd user service, from a shell that
/// inherited nothing, or from an MCP client that spawns with a bare
/// environment.
///
/// **Runs at most once per process.** Every write it makes is `unsafe` in
/// edition 2024, because setting a variable while another thread reads one is
/// undefined behavior, and this process spawns blocking threads as soon as it
/// starts serving. `run_cli_from_env` calls this as its first statement, when
/// the runtime has not scheduled anything and no other thread exists; the
/// `Once` is what makes that the only call that ever writes, rather than the
/// first of five that happen to find nothing left to do.
///
/// Every `SAFETY` comment below rests on this, and on nothing else.
pub fn hydrate_session_bus_env() {
    static HYDRATED: Once = Once::new();
    HYDRATED.call_once(hydrate_session_bus_env_once);
}

fn hydrate_session_bus_env_once() {
    hydrate_common_command_path();
    hydrate_desktop_env_from_process_tree();
    hydrate_desktop_env_from_systemd_user();

    if env_var("XDG_RUNTIME_DIR").is_none()
        && let Some(runtime) = xdg_runtime_dir()
        && runtime.exists()
    {
        // SAFETY: see `hydrate_session_bus_env`. This runs once, from the
        // startup call, before any other thread exists.
        unsafe { env::set_var("XDG_RUNTIME_DIR", runtime) };
    }

    if env_var("DBUS_SESSION_BUS_ADDRESS").is_none()
        && let Some(runtime) = xdg_runtime_dir()
    {
        let bus = runtime.join("bus");
        if bus.exists() {
            // SAFETY: see `hydrate_session_bus_env`. This runs once, from the
            // startup call, before any other thread exists.
            unsafe {
                env::set_var(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", bus.display()),
                );
            };
        }
    }
}

/// Where a desktop's own commands live on the distributions this runs on.
/// A process started from a systemd user service or an MCP client can inherit
/// a PATH with none of them.
const COMMON_COMMAND_PATHS: [&str; 4] = [
    "/run/current-system/sw/bin",
    "/usr/local/bin",
    "/usr/bin",
    "/bin",
];

fn hydrate_common_command_path() {
    let mut entries = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    let candidates = COMMON_COMMAND_PATHS
        .iter()
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .collect::<Vec<_>>();
    let additions = command_path_additions(&entries, &candidates);
    if additions.is_empty() {
        return;
    }

    entries.extend(additions);
    if let Ok(path) = env::join_paths(entries) {
        // SAFETY: see `hydrate_session_bus_env`. This runs once, from the
        // startup call, before any other thread exists.
        unsafe { env::set_var("PATH", path) };
    }
}

/// Which of `candidates` are missing from `current`, in the order given.
///
/// Empty means PATH already carries them and must not be rewritten. That
/// distinction is the point: this used to rebuild and rewrite PATH on every
/// call, which made an unconditional write out of a function whose whole job
/// is to fill in what is missing.
fn command_path_additions(current: &[PathBuf], candidates: &[PathBuf]) -> Vec<PathBuf> {
    candidates
        .iter()
        .filter(|candidate| !current.contains(candidate))
        .cloned()
        .collect()
}

fn hydrate_desktop_env_from_process_tree() {
    for process_env in desktop_process_environments() {
        hydrate_desktop_env_from_map(&process_env);

        if DESKTOP_ENV_KEYS.iter().all(|key| env_var(key).is_some()) {
            break;
        }
    }
}

fn hydrate_desktop_env_from_systemd_user() {
    let Ok(output) = Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let env_map = parse_line_environment(&output.stdout);
    hydrate_desktop_env_from_map(&env_map);
}

fn hydrate_desktop_env_from_map(process_env: &HashMap<String, String>) {
    let current_env = DESKTOP_ENV_KEYS
        .iter()
        .filter_map(|key| env_var(key).map(|value| ((*key).to_string(), value)))
        .collect();
    for (key, value) in desktop_env_hydration_updates(&current_env, process_env) {
        // SAFETY: see `hydrate_session_bus_env`. This runs once, from the
        // startup call, before any other thread exists.
        unsafe { env::set_var(key, value) };
    }
}

fn desktop_env_hydration_updates(
    current_env: &HashMap<String, String>,
    source_env: &HashMap<String, String>,
) -> Vec<(&'static str, String)> {
    // A nested X11 desktop can share a user manager with a Wayland host.
    // Preserve its complete process-local session instead of grafting the
    // host's WAYLAND_DISPLAY onto it.
    let preserve_native_x11 = current_env
        .get("XDG_SESSION_TYPE")
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("x11"))
        && current_env
            .get("DISPLAY")
            .is_some_and(|value| !value.trim().is_empty())
        && current_env
            .get("WAYLAND_DISPLAY")
            .is_none_or(|value| value.trim().is_empty());

    DESKTOP_ENV_KEYS
        .iter()
        .filter_map(|key| {
            if current_env
                .get(*key)
                .is_some_and(|value| !value.trim().is_empty())
                || preserve_native_x11 && *key == "WAYLAND_DISPLAY"
            {
                return None;
            }
            source_env
                .get(*key)
                .filter(|value| !value.trim().is_empty())
                .map(|value| (*key, value.clone()))
        })
        .collect()
}

fn desktop_process_environments() -> Vec<HashMap<String, String>> {
    let mut environments = Vec::new();
    let mut visited_pids = Vec::new();
    let mut pid = parent_pid("self");

    for _ in 0..8 {
        let Some(current_pid) = pid else {
            break;
        };
        if current_pid <= 1 {
            break;
        }

        visited_pids.push(current_pid);
        if let Some(process_env) = read_process_environ(current_pid) {
            environments.push(process_env);
        }
        pid = parent_pid(&current_pid.to_string());
    }

    if !visited_pids.contains(&1)
        && process_owner_matches_current_user(1)
        && let Some(process_env) = read_process_environ(1).filter(process_env_has_graphical_display)
    {
        environments.push(process_env);
    }

    environments
}

fn parent_pid(pid: &str) -> Option<u32> {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_parent_pid(&status)
}

fn parse_parent_pid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let value = line.strip_prefix("PPid:")?.trim();
        value.parse::<u32>().ok()
    })
}

fn read_process_environ(pid: u32) -> Option<HashMap<String, String>> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(parse_environ(&bytes))
}

fn process_owner_matches_current_user(pid: u32) -> bool {
    let Some(current_uid) = user_id().and_then(|uid| uid.parse::<u32>().ok()) else {
        return false;
    };
    fs::metadata(format!("/proc/{pid}")).is_ok_and(|metadata| metadata.uid() == current_uid)
}

fn process_env_has_graphical_display(process_env: &HashMap<String, String>) -> bool {
    process_env
        .get("DISPLAY")
        .or_else(|| process_env.get("WAYLAND_DISPLAY"))
        .is_some_and(|value| !value.trim().is_empty())
}

fn parse_environ(bytes: &[u8]) -> HashMap<String, String> {
    bytes
        .split(|byte| *byte == 0)
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let split = entry.iter().position(|byte| *byte == b'=')?;
            let (key, value) = entry.split_at(split);
            let value = &value[1..];
            let key = std::str::from_utf8(key).ok()?.to_string();
            let value = std::str::from_utf8(value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

fn parse_line_environment(bytes: &[u8]) -> HashMap<String, String> {
    bytes
        .split(|byte| *byte == b'\n')
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let split = entry.iter().position(|byte| *byte == b'=')?;
            let (key, value) = entry.split_at(split);
            let value = &value[1..];
            let key = std::str::from_utf8(key).ok()?.to_string();
            let value = std::str::from_utf8(value).ok()?.to_string();
            Some((key, value))
        })
        .collect()
}

pub fn setup_accessibility_report() -> SetupReport {
    hydrate_session_bus_env();

    let before = doctor_report();
    let accessibility_command = if can_build_accessibility_tree(&before.accessibility) {
        Check::ok("AT-SPI accessibility is already enabled")
    } else {
        let atspi_status = command_check_with_session_bus(
            "busctl",
            &[
                "--user",
                "set-property",
                "org.a11y.Bus",
                "/org/a11y/bus",
                "org.a11y.Status",
                "IsEnabled",
                "b",
                "true",
            ],
        );
        if atspi_status.ok {
            atspi_status
        } else {
            command_check_with_session_bus(
                "gsettings",
                &[
                    "set",
                    "org.gnome.desktop.interface",
                    "toolkit-accessibility",
                    "true",
                ],
            )
        }
    };
    let after = doctor_report();
    let before_ready = before.readiness.can_build_accessibility_tree;
    let after_ready = after.readiness.can_build_accessibility_tree;
    let changed_accessibility = !before_ready && after_ready;
    let requires_target_app_restart = changed_accessibility;
    let message = if after_ready {
        if changed_accessibility {
            "AT-SPI accessibility is enabled. Restart already-running target apps if their AT-SPI tree is still empty."
        } else {
            "AT-SPI accessibility is ready."
        }
    } else {
        "Could not enable AT-SPI accessibility automatically. Check the accessibility_command detail and enable org.a11y.Status IsEnabled or org.gnome.desktop.interface toolkit-accessibility manually."
    }
    .to_string();

    SetupReport {
        before,
        accessibility_command,
        after,
        changed_accessibility,
        requires_target_app_restart,
        message,
    }
}

fn platform_report() -> PlatformReport {
    PlatformReport {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        desktop_session: env_var("DESKTOP_SESSION"),
        xdg_session_type: env_var("XDG_SESSION_TYPE"),
        xdg_current_desktop: env_var("XDG_CURRENT_DESKTOP"),
        wayland_display: env_var("WAYLAND_DISPLAY"),
        display: env_var("DISPLAY"),
        xauthority: env_var("XAUTHORITY"),
        dbus_session_bus_address: dbus_session_address(),
        xdg_runtime_dir: xdg_runtime_dir().map(|path| path.display().to_string()),
    }
}

fn portal_report() -> PortalReport {
    PortalReport {
        desktop_portal: bus_name_check("org.freedesktop.portal.Desktop"),
        screenshot: portal_interface_check("org.freedesktop.portal.Screenshot"),
    }
}

fn accessibility_report() -> AccessibilityReport {
    AccessibilityReport {
        at_spi_bus: atspi_bus_address_check(),
        toolkit_accessibility: command_check_with_session_bus(
            "gsettings",
            &[
                "get",
                "org.gnome.desktop.interface",
                "toolkit-accessibility",
            ],
        ),
        at_spi_enabled: atspi_status_property_check("IsEnabled"),
        screen_reader_enabled: atspi_status_property_check("ScreenReaderEnabled"),
    }
}

fn windowing_report() -> WindowingReport {
    let probes = registry::probe_backends();
    let hyprland = probes
        .iter()
        .find(|probe| probe.id == HYPRLAND_BACKEND)
        .map_or_else(
            || Check::fail("backend probe did not run"),
            check_from_backend_probe,
        );
    let backends = probes
        .iter()
        .map(|probe| (probe.id.to_string(), check_from_backend_probe(probe)))
        .collect::<BTreeMap<_, _>>();
    let can_list_windows = probes.iter().any(|probe| probe.can_list_windows);
    let can_focus_apps = probes.iter().any(|probe| probe.can_focus_apps);
    let can_focus_windows = probes.iter().any(|probe| probe.can_focus_windows);
    let note = if can_list_windows {
        "A Hyprland window backend is available for list_windows, focused_window, and targeted input verification."
    } else {
        registry::WINDOW_PERMISSION_HINT
    }
    .to_string();

    WindowingReport {
        hyprland,
        backends,
        can_list_windows,
        can_focus_apps,
        can_focus_windows,
        note,
    }
}

fn check_from_backend_probe(probe: &registry::BackendProbe) -> Check {
    if probe.ok {
        Check::ok(probe.detail.clone())
    } else {
        Check::fail(probe.detail.clone())
    }
}

fn input_report() -> InputReport {
    InputReport {
        ydotool: match ydotool::ensure_supported() {
            Ok(support) => Check::ok(support.detail),
            Err(detail) => Check::fail(detail),
        },
        ydotoold: process_check("ydotoold"),
        ydotool_socket: ydotool_socket_check(),
        uinput: read_write_path_check(Path::new("/dev/uinput")),
        wtype: command_path_check("wtype"),
    }
}

fn readiness_report(
    platform: &PlatformReport,
    accessibility: &AccessibilityReport,
    windowing: &WindowingReport,
    input: &InputReport,
) -> ReadinessReport {
    let mut blockers = Vec::new();
    let can_build_accessibility_tree = can_build_accessibility_tree(accessibility);
    let can_query_windows = windowing.can_list_windows;
    let can_focus_apps = windowing.can_focus_apps;
    let can_focus_windows = windowing.can_focus_windows;
    let can_send_development_input = can_send_development_input(platform, input);

    if !can_build_accessibility_tree {
        blockers.push(
            "AT-SPI accessibility is disabled; enable org.a11y.Status IsEnabled or org.gnome.desktop.interface toolkit-accessibility for tree extraction."
                .to_string(),
        );
    }

    if !can_query_windows {
        blockers.push(
            "Window introspection is unavailable; targeted window focus and verification will be disabled."
                .to_string(),
        );
    }

    if can_query_windows && !can_focus_windows {
        blockers.push(
            "Exact window activation is unavailable; app-level focus may work, but window_id/title/terminal-targeted input cannot be verified."
                .to_string(),
        );
    }

    if !can_send_development_input {
        blockers.push(
            "Development keyboard input is unavailable; install wtype, or start ydotoold with a socket this user can connect to. Read/write /dev/uinput alone provides only absolute pointer input."
                .to_string(),
        );
    }

    let recommended_next_step = if !can_build_accessibility_tree {
        "Run setup_accessibility to enable AT-SPI accessibility before element-aware actions."
            .to_string()
    } else if !can_query_windows {
        registry::WINDOW_PERMISSION_HINT.to_string()
    } else if !can_focus_windows {
        "Enable an exact-focus window backend before using window_id, title, or terminal-targeted input.".to_string()
    } else if !can_send_development_input {
        "Enable a keyboard-capable input backend: install wtype, or start ydotoold with a socket accessible to this desktop user."
            .to_string()
    } else {
        "Computer Use is ready: AT-SPI tree support, window targeting, and a uinput input backend are available."
            .to_string()
    };

    ReadinessReport {
        can_register_mcp_tools: true,
        can_build_accessibility_tree,
        can_query_windows,
        can_focus_apps,
        can_focus_windows,
        can_send_development_input,
        recommended_next_step,
        blockers,
    }
}

fn can_send_development_input(platform: &PlatformReport, input: &InputReport) -> bool {
    let force_ydotool = env_flag_enabled_any(FORCE_YDOTOOL_KEYBOARD_ENV_KEYS);
    platform_is_wayland(platform)
        && wtype_compatible_wayland_desktop(platform.xdg_current_desktop.as_deref())
        && input.wtype.ok
        && !force_ydotool
        || input.ydotool.ok && input.ydotool_socket.ok
}

fn can_build_accessibility_tree(accessibility: &AccessibilityReport) -> bool {
    accessibility.at_spi_bus.ok
        && (check_detail_contains_true(&accessibility.at_spi_enabled)
            || check_detail_contains_true(&accessibility.toolkit_accessibility))
}

fn check_detail_contains_true(check: &Check) -> bool {
    check.ok && check.detail.to_ascii_lowercase().contains("true")
}

fn env_var(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.trim().is_empty())
}

fn xdg_runtime_dir() -> Option<PathBuf> {
    if let Some(value) = env_var("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(value));
    }
    user_id().map(|uid| PathBuf::from(format!("/run/user/{uid}")))
}

fn dbus_session_address() -> Option<String> {
    if let Some(value) = env_var("DBUS_SESSION_BUS_ADDRESS") {
        return Some(value);
    }
    xdg_runtime_dir()
        .map(|runtime| format!("unix:path={}", runtime.join("bus").display()))
        .filter(|address| {
            address
                .strip_prefix("unix:path=")
                .is_some_and(|p| Path::new(p).exists())
        })
}

fn ydotool_socket_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(value) = env_var("YDOTOOL_SOCKET") {
        candidates.push(PathBuf::from(value));
    }

    if let Some(runtime_socket) = xdg_runtime_dir().map(|runtime| runtime.join(".ydotool_socket")) {
        candidates.push(runtime_socket);
    }
    candidates.push(PathBuf::from("/tmp/.ydotool_socket"));
    candidates
}

fn ydotool_socket_check() -> Check {
    let mut checked = Vec::new();
    for candidate in ydotool_socket_candidates() {
        match socket_connect_result(&candidate) {
            Ok(()) => return Check::ok(format!("connectable: {}", candidate.display())),
            Err(detail) => checked.push(detail),
        }
    }

    Check::fail(format!(
        "no connectable ydotool socket ({})",
        checked.join("; ")
    ))
}

pub(crate) fn user_id() -> Option<String> {
    let output = Command::new("id").arg("-u").output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

fn command_path_check(command: &str) -> Check {
    command_check("sh", &["-c", &format!("command -v {command}")])
}

fn platform_is_wayland(platform: &PlatformReport) -> bool {
    match platform.xdg_session_type.as_deref() {
        Some(value) => value.eq_ignore_ascii_case("wayland"),
        None => platform
            .wayland_display
            .as_deref()
            .is_some_and(|display| !display.trim().is_empty()),
    }
}

pub(crate) fn wtype_compatible_wayland_desktop(desktop: Option<&str>) -> bool {
    desktop.is_none_or(|desktop| {
        let desktop = desktop.to_ascii_lowercase();
        !["gnome", "kde", "plasma", "cosmic"]
            .iter()
            .any(|known_incompatible| desktop.contains(known_incompatible))
    })
}

fn env_flag_enabled_any(keys: &[&str]) -> bool {
    keys.iter()
        .any(|key| env::var(key).ok().as_deref() == Some("1"))
}

fn process_check(process_name: &str) -> Check {
    command_check("pgrep", &["-a", process_name])
}

#[cfg(test)]
fn socket_connect_check(path: &Path) -> Check {
    match socket_connect_result(path) {
        Ok(()) => Check::ok(format!("connectable: {}", path.display())),
        Err(detail) => Check::fail(detail),
    }
}

fn socket_connect_result(path: &Path) -> std::result::Result<(), String> {
    if !path.exists() {
        return Err(format!("missing: {}", path.display()));
    }

    UnixDatagram::unbound()
        .and_then(|socket| socket.connect(path))
        .map_err(|error| format!("{}: datagram: {error}", path.display()))
}

fn read_write_path_check(path: &Path) -> Check {
    if !path.exists() {
        return Check::fail(format!("missing: {}", path.display()));
    }

    match OpenOptions::new().read(true).write(true).open(path) {
        Ok(_) => Check::ok(format!("read/write: {}", path.display())),
        Err(error) => Check::fail(format!("{}: {error}", path.display())),
    }
}

fn bus_name_check(name: &str) -> Check {
    command_check_with_session_bus("busctl", &["--user", "status", name])
}

fn portal_interface_check(interface: &str) -> Check {
    command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "introspect",
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            interface,
        ],
    )
}

fn atspi_bus_address_check() -> Check {
    let busctl = command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "call",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Bus",
            "GetAddress",
        ],
    );
    if busctl.ok {
        return busctl;
    }

    gdbus_call_check(
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.a11y.Bus.GetAddress",
        &[],
    )
}

fn atspi_status_property_check(property: &str) -> Check {
    let busctl = command_check_with_session_bus(
        "busctl",
        &[
            "--user",
            "get-property",
            "org.a11y.Bus",
            "/org/a11y/bus",
            "org.a11y.Status",
            property,
        ],
    );
    if busctl.ok {
        return busctl;
    }

    gdbus_call_check(
        "org.a11y.Bus",
        "/org/a11y/bus",
        "org.freedesktop.DBus.Properties.Get",
        &["org.a11y.Status", property],
    )
}

fn gdbus_call_check(destination: &str, object_path: &str, method: &str, args: &[&str]) -> Check {
    let mut command_args = vec![
        "call",
        "--session",
        "--dest",
        destination,
        "--object-path",
        object_path,
        "--method",
        method,
    ];
    command_args.extend_from_slice(args);
    command_check_with_session_bus("gdbus", &command_args)
}

fn command_check(command: &str, args: &[&str]) -> Check {
    run_command(command, args, false)
}

fn command_check_with_session_bus(command: &str, args: &[&str]) -> Check {
    run_command(command, args, true)
}

fn run_command(command: &str, args: &[&str], with_session_bus: bool) -> Check {
    let mut cmd = Command::new(command);
    cmd.args(args);

    if with_session_bus {
        if let Some(address) = dbus_session_address() {
            cmd.env("DBUS_SESSION_BUS_ADDRESS", address);
        }
        if let Some(runtime) = xdg_runtime_dir() {
            cmd.env("XDG_RUNTIME_DIR", runtime);
        }
    }

    match cmd.output() {
        Ok(output) if output.status.success() => {
            let detail = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Check::ok(if detail.is_empty() {
                "ok".into()
            } else {
                detail
            })
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let detail = if stderr.is_empty() { stdout } else { stderr };
            Check::fail(if detail.is_empty() {
                format!("exit status {}", output.status)
            } else {
                detail
            })
        }
        Err(error) => Check::fail(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn platform_report() -> PlatformReport {
        PlatformReport {
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            desktop_session: None,
            xdg_session_type: Some("wayland".to_string()),
            xdg_current_desktop: Some("Hyprland".to_string()),
            wayland_display: Some("wayland-0".to_string()),
            display: Some(":0".to_string()),
            xauthority: Some("/run/user/1000/Xauthority".to_string()),
            dbus_session_bus_address: Some("unix:path=/run/user/1000/bus".to_string()),
            xdg_runtime_dir: Some("/run/user/1000".to_string()),
        }
    }

    fn portal_report(screenshot: Check) -> PortalReport {
        PortalReport {
            desktop_portal: Check::ok("ok"),
            screenshot,
        }
    }

    fn accessibility_report(
        at_spi_bus: Check,
        toolkit_accessibility: Check,
    ) -> AccessibilityReport {
        AccessibilityReport {
            at_spi_bus,
            toolkit_accessibility,
            at_spi_enabled: Check::fail("(<false>,)"),
            screen_reader_enabled: Check::fail("(<false>,)"),
        }
    }

    fn windowing_report(can_list_windows: bool, can_focus_windows: bool) -> WindowingReport {
        WindowingReport {
            hyprland: if can_list_windows {
                Check::ok("ok")
            } else {
                Check::fail("hyprctl unavailable")
            },
            backends: BTreeMap::new(),
            can_list_windows,
            can_focus_apps: true,
            can_focus_windows,
            note: String::new(),
        }
    }

    fn input_report(can_send_input: bool) -> InputReport {
        let check = if can_send_input {
            Check::ok("ok")
        } else {
            Check::fail("missing")
        };
        input_report_parts(check.clone(), check.clone(), check.clone(), check)
    }

    fn input_report_parts(
        ydotool: Check,
        ydotoold: Check,
        ydotool_socket: Check,
        uinput: Check,
    ) -> InputReport {
        InputReport {
            ydotool,
            ydotoold,
            ydotool_socket,
            uinput,
            wtype: Check::fail("missing wtype"),
        }
    }

    #[test]
    fn accessibility_tree_requires_reachable_at_spi_bus() {
        let report = accessibility_report(Check::fail("permission denied"), Check::ok("true"));

        assert!(!can_build_accessibility_tree(&report));
    }

    #[test]
    fn accessibility_tree_is_ready_when_bus_and_toolkit_are_ready() {
        let report = accessibility_report(
            Check::ok("('unix:path=/run/user/1000/at-spi/bus',)"),
            Check::ok("true"),
        );

        assert!(can_build_accessibility_tree(&report));
    }

    #[test]
    fn capability_map_advertises_only_a_buildable_accessibility_tree() {
        let platform = platform_report();
        let portals = portal_report(Check::fail("missing"));
        let windowing = windowing_report(false, false);
        let input = input_report(false);

        for accessibility in [
            accessibility_report(Check::fail("permission denied"), Check::ok("true")),
            accessibility_report(
                Check::ok("('unix:path=/run/user/1000/at-spi/bus',)"),
                Check::ok("false"),
            ),
        ] {
            let capabilities =
                capability_map(&platform, &portals, &accessibility, &windowing, &input);
            assert!(capabilities.accessibility.is_empty());
        }
    }

    #[test]
    fn parses_parent_pid_from_proc_status() {
        let status = "Name:\ttest\nPid:\t42\nPPid:\t7\n";

        assert_eq!(parse_parent_pid(status), Some(7));
    }

    #[test]
    fn parses_nul_separated_process_environment() {
        let environment = parse_environ(
            b"DISPLAY=:0\0WAYLAND_DISPLAY=wayland-0\0EMPTY=\0NO_EQUALS\0XDG_SESSION_TYPE=wayland\0",
        );

        assert_eq!(environment.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            environment.get("WAYLAND_DISPLAY").map(String::as_str),
            Some("wayland-0")
        );
        assert_eq!(environment.get("EMPTY").map(String::as_str), Some(""));
        assert!(!environment.contains_key("NO_EQUALS"));
    }

    #[test]
    fn a_path_that_already_carries_every_candidate_is_not_rewritten() {
        let current = ["/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from);
        let candidates = ["/usr/bin", "/bin"].map(PathBuf::from);

        assert!(command_path_additions(&current, &candidates).is_empty());
    }

    #[test]
    fn only_the_missing_candidates_are_added_in_the_order_given() {
        let current = ["/usr/bin"].map(PathBuf::from);
        let candidates = ["/run/current-system/sw/bin", "/usr/bin", "/bin"].map(PathBuf::from);

        assert_eq!(
            command_path_additions(&current, &candidates),
            ["/run/current-system/sw/bin", "/bin"].map(PathBuf::from)
        );
    }

    #[test]
    fn an_empty_path_takes_every_candidate() {
        let candidates = ["/usr/bin", "/bin"].map(PathBuf::from);

        assert_eq!(command_path_additions(&[], &candidates), candidates);
    }

    #[test]
    fn desktop_env_hydration_includes_xauthority() {
        assert!(DESKTOP_ENV_KEYS.contains(&"XAUTHORITY"));
    }

    #[test]
    fn desktop_env_hydration_preserves_explicit_native_x11() {
        let current_env = HashMap::from([
            ("DISPLAY".to_string(), ":90".to_string()),
            ("XDG_SESSION_TYPE".to_string(), "x11".to_string()),
        ]);
        let host_env = HashMap::from([
            ("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string()),
            (
                "XDG_CURRENT_DESKTOP".to_string(),
                "ubuntu:GNOME".to_string(),
            ),
        ]);

        let updates = desktop_env_hydration_updates(&current_env, &host_env);

        assert!(!updates.iter().any(|(key, _)| *key == "WAYLAND_DISPLAY"));
        assert!(
            updates
                .iter()
                .any(|(key, value)| { *key == "XDG_CURRENT_DESKTOP" && value == "ubuntu:GNOME" })
        );
    }

    #[test]
    fn desktop_env_hydration_still_imports_wayland_for_incomplete_sessions() {
        let current_env = HashMap::new();
        let host_env = HashMap::from([("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string())]);

        let updates = desktop_env_hydration_updates(&current_env, &host_env);

        assert!(
            updates
                .iter()
                .any(|(key, value)| *key == "WAYLAND_DISPLAY" && value == "wayland-0")
        );
    }

    #[test]
    fn graphical_process_env_requires_display() {
        let with_display = HashMap::from([("DISPLAY".to_string(), ":0".to_string())]);
        let with_wayland =
            HashMap::from([("WAYLAND_DISPLAY".to_string(), "wayland-0".to_string())]);
        let without_display = HashMap::from([("XAUTHORITY".to_string(), "/tmp/xauth".to_string())]);

        assert!(process_env_has_graphical_display(&with_display));
        assert!(process_env_has_graphical_display(&with_wayland));
        assert!(!process_env_has_graphical_display(&without_display));
    }

    #[test]
    fn wayland_diagnostics_advertise_wtype_without_portal_or_ydotool() {
        let mut platform = platform_report();
        platform.xdg_session_type = Some("wayland".to_string());
        platform.xdg_current_desktop = Some("Hyprland".to_string());
        platform.wayland_display = Some("wayland-0".to_string());
        let portals = portal_report(Check::fail("missing"));
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let mut input = input_report(false);
        input.wtype = Check::ok("wtype");

        let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);
        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert_eq!(capabilities.input, ["wtype"]);
        assert_eq!(capabilities.preferred.input.as_deref(), Some("wtype"));
        assert!(readiness.can_send_development_input);
    }

    #[test]
    fn wtype_excludes_known_incompatible_wayland_desktops() {
        assert!(wtype_compatible_wayland_desktop(Some("Hyprland")));
        assert!(wtype_compatible_wayland_desktop(Some("sway")));
        assert!(wtype_compatible_wayland_desktop(None));
        assert!(!wtype_compatible_wayland_desktop(Some("GNOME")));
        assert!(!wtype_compatible_wayland_desktop(Some("KDE;Plasma")));
        assert!(!wtype_compatible_wayland_desktop(Some("COSMIC")));
    }

    #[test]
    fn ydotool_carries_input_when_uinput_is_unreadable() {
        let platform = platform_report();
        let portals = portal_report(Check::ok("org.freedesktop.portal.Screenshot"));
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::ok("ydotoold"),
            Check::ok("connectable"),
            Check::fail("missing uinput"),
        );

        let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);

        assert_eq!(capabilities.input, ["ydotool"]);
        assert_eq!(capabilities.preferred.input.as_deref(), Some("ydotool"));
    }

    #[test]
    fn parses_systemd_show_environment_output() {
        let environment = parse_line_environment(
            b"DISPLAY=:0\nHYPRLAND_INSTANCE_SIGNATURE=abc\nNO_EQUALS\nYDOTOOL_SOCKET=/run/ydotoold/socket\n",
        );

        assert_eq!(environment.get("DISPLAY").map(String::as_str), Some(":0"));
        assert_eq!(
            environment
                .get("HYPRLAND_INSTANCE_SIGNATURE")
                .map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            environment.get("YDOTOOL_SOCKET").map(String::as_str),
            Some("/run/ydotoold/socket")
        );
        assert!(!environment.contains_key("NO_EQUALS"));
    }

    #[test]
    fn readiness_requires_exact_window_focus_for_targeted_input() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, false);
        let input = input_report(true);

        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(readiness.can_query_windows);
        assert!(!readiness.can_focus_windows);
        assert!(
            readiness
                .recommended_next_step
                .contains("exact-focus window backend")
        );
        assert!(
            readiness
                .blockers
                .iter()
                .any(|blocker| blocker.contains("Exact window activation"))
        );
    }

    #[test]
    fn readiness_message_mentions_generic_window_targeting() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report(true);

        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(readiness.blockers.is_empty());
        assert!(
            readiness
                .recommended_next_step
                .contains("AT-SPI tree support")
        );
        assert!(readiness.recommended_next_step.contains("window targeting"));
        assert!(
            !readiness
                .recommended_next_step
                .contains("GNOME window targeting")
        );
    }

    #[test]
    fn readiness_accepts_connectable_ydotool_socket_without_direct_uinput_access() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::ok("ydotoold"),
            Check::ok("connectable: /tmp/.ydotool_socket"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(readiness.can_send_development_input);
        assert!(readiness.blockers.is_empty());
    }

    #[test]
    fn readiness_uses_connectable_ydotool_socket_when_process_probe_fails() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::fail("ydotoold process name not found"),
            Check::ok("connectable: /run/user/1000/.ydotool_socket"),
            Check::fail("/dev/uinput: Permission denied"),
        );
        let portals = portal_report(Check::fail("missing"));

        let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);
        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(
            capabilities
                .input
                .iter()
                .any(|backend| backend == "ydotool")
        );
        assert!(readiness.can_send_development_input);
    }

    #[test]
    fn wayland_readiness_rejects_pointer_only_uinput_without_keyboard_backend() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::fail("ydotoold not running"),
            Check::fail("no connectable ydotool socket"),
            Check::ok("read/write: /dev/uinput"),
        );

        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(!readiness.can_send_development_input);
        assert!(
            readiness
                .blockers
                .iter()
                .any(|blocker| blocker.contains("absolute pointer input"))
        );
        assert!(
            readiness
                .recommended_next_step
                .contains("keyboard-capable input backend")
        );
    }

    #[test]
    fn readiness_rejects_inaccessible_ydotool_paths() {
        let platform = platform_report();
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::ok("ydotool"),
            Check::ok("ydotoold"),
            Check::fail("/tmp/.ydotool_socket: Permission denied"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(!readiness.can_send_development_input);
        assert!(
            readiness
                .recommended_next_step
                .contains("Enable a keyboard-capable input backend")
        );
        assert!(
            readiness
                .blockers
                .iter()
                .any(|blocker| blocker.contains("Development keyboard input is unavailable"))
        );
    }

    #[test]
    fn capability_map_rejects_incompatible_ydotool_with_live_daemon() {
        let platform = platform_report();
        let portals = portal_report(Check::fail("missing"));
        let accessibility = accessibility_report(Check::ok("bus"), Check::ok("true"));
        let windowing = windowing_report(true, true);
        let input = input_report_parts(
            Check::fail("unsupported legacy ydotool CLI"),
            Check::ok("ydotoold"),
            Check::ok("connectable socket"),
            Check::fail("/dev/uinput: Permission denied"),
        );

        let capabilities = capability_map(&platform, &portals, &accessibility, &windowing, &input);
        let readiness = readiness_report(&platform, &accessibility, &windowing, &input);

        assert!(
            !capabilities
                .input
                .iter()
                .any(|backend| backend == "ydotool")
        );
        assert!(!readiness.can_send_development_input);
    }

    #[test]
    fn ydotool_socket_check_rejects_legacy_stream_socket() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-diagnostics-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp diagnostics dir");
        let socket = dir.join("ydotool.sock");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket).expect("bind temp diagnostics socket");

        let check = socket_connect_check(&socket);

        assert!(!check.ok, "{check:?}");
        drop(listener);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ydotool_socket_check_accepts_datagram_socket() {
        let dir = std::env::temp_dir().join(format!(
            "computer-use-hyprland-diagnostics-dgram-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp diagnostics dir");
        let socket = dir.join("ydotool.sock");
        let datagram =
            std::os::unix::net::UnixDatagram::bind(&socket).expect("bind temp datagram socket");

        let check = socket_connect_check(&socket);

        assert!(check.ok, "{check:?}");
        drop(datagram);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
