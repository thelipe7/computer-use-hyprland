//! Everything but `run_cli_from_env` is internal: the binary is the only
//! consumer, and the old public surface existed for a downstream embedder that
//! this fork does not have.

mod abs_pointer;
#[path = "atspi_tree.rs"]
mod atspi_tree_impl;
mod cli;
mod command_runner;
#[path = "diagnostics.rs"]
mod diagnostics_impl;
#[path = "screenshot.rs"]
mod screenshot_impl;
mod server;
mod session_lock;
mod terminal;
mod windowing;
mod ydotool;

pub(crate) mod atspi_tree {
    pub(crate) use crate::atspi_tree_impl::{
        AccessibilityAction, AccessibilityNode, AccessibleAppSummary, Bounds,
        FocusedElementSummary, ValueSetInvocation, element_states, focused_element_summary,
        grab_focus, is_stale_object_error, list_accessible_apps, perform_action, set_element_value,
        snapshot_limits, snapshot_tree,
    };
}

pub(crate) mod diagnostics {
    pub(crate) use crate::diagnostics_impl::{
        DoctorReport, ReadinessReport, SetupReport, doctor_report, hydrate_session_bus_env,
        setup_accessibility_report, user_id, wtype_compatible_wayland_desktop,
    };
}

pub(crate) mod screenshot {
    pub(crate) use crate::screenshot_impl::{
        RawScreenshotCapture, ScreenshotCapture, ScreenshotOutputFormat, ScreenshotPayloadOptions,
        capture_screenshot, capture_screenshot_raw, prepare_screenshot_payload,
    };
}

/// Run the subcommand named on the command line, or print the usage when
/// none is. This is the whole public surface of the crate.
///
/// # Errors
///
/// Returns whatever the subcommand failed with: a desktop it could not
/// reach, a window it could not find, an input device it could not open.
pub async fn run_cli_from_env() -> anyhow::Result<()> {
    cli::run_from_env().await
}
