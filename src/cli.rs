use crate::{abs_pointer, atspi_tree, diagnostics, screenshot, server, windowing};
use anyhow::{Context, Result};
use serde::Serialize;

pub(crate) async fn run_from_env() -> Result<()> {
    diagnostics::hydrate_session_bus_env();

    match std::env::args().nth(1).as_deref() {
        Some("mcp") => server::serve_mcp().await,
        Some("doctor") => print_json(&diagnostics::doctor_report(), "doctor report"),
        Some("setup") => print_json(&diagnostics::setup_accessibility_report(), "setup report"),
        Some("apps") => print_json(
            &atspi_tree::list_accessible_apps(50).await?,
            "accessible apps",
        ),
        Some("state") => run_state().await,
        Some("screenshot") => run_screenshot().await,
        Some("windows") => run_windows().await,
        Some("abs-test") => run_abs_test().await,
        Some("--help" | "-h") | None => {
            print_help();
            Ok(())
        }
        Some(command) => {
            anyhow::bail!(
                "unknown command '{command}'. Expected one of: mcp, doctor, setup, apps, state, screenshot, windows, abs-test"
            );
        }
    }
}

/// Print a report as indented JSON, which is what every subcommand but `mcp`
/// does with whatever it produced.
fn print_json<T: Serialize>(value: &T, what: &str) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value)
            .with_context(|| format!("failed to serialize {what}"))?
    );
    Ok(())
}

/// The accessibility tree of one application, or of whatever is focused when
/// no name is given.
async fn run_state() -> Result<()> {
    let app_name_or_bundle_identifier = std::env::args().nth(2);
    let (max_nodes, max_depth) = atspi_tree::snapshot_limits(None, None);
    let snapshot = atspi_tree::snapshot_tree(
        app_name_or_bundle_identifier.as_deref(),
        None,
        max_nodes,
        max_depth,
    )
    .await?;
    print_json(&snapshot.nodes, "accessibility tree")
}

/// One capture, reported by its metadata rather than by its bytes: the image
/// itself is only useful to an MCP client.
async fn run_screenshot() -> Result<()> {
    let capture = screenshot::capture_screenshot().await?;
    print_json(
        &serde_json::json!({
            "mime_type": capture.mime_type,
            "source": capture.source,
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
            "data_url_length": capture.data_url.len()
        }),
        "screenshot report",
    )
}

/// Every window the compositor reports, or the reason there are none.
async fn run_windows() -> Result<()> {
    let report = match windowing::list_windows().await {
        Ok(windows) => {
            let backend = windows
                .first()
                .map_or(windowing::HYPRLAND_BACKEND, |window| {
                    window.backend.as_str()
                });
            serde_json::json!({
                "backend": backend,
                "windows": windows,
                "error": null,
                "permissions_hint": null,
            })
        }
        Err(error) => {
            let error = format!("{error:#}");
            serde_json::json!({
                "backend": "unavailable",
                "windows": [],
                "error": error,
                "permissions_hint": windowing::window_permission_hint(&error),
            })
        }
    };
    print_json(&report, "window report")
}

/// Click one desktop coordinate through the uinput pointer and report where
/// it actually landed, which is how a coordinate problem is told apart from an
/// input-backend one.
async fn run_abs_test() -> Result<()> {
    let x: i32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let y: i32 = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let cap = screenshot::capture_screenshot_raw().await?;
    eprintln!("desktop logical size: {}x{}", cap.width, cap.height);
    let mut pointer = abs_pointer::AbsPointer::create(
        i32::try_from(cap.width).unwrap_or(i32::MAX),
        i32::try_from(cap.height).unwrap_or(i32::MAX),
    )?;
    let landing = pointer.click(x, y, abs_pointer::PointerButton::Left, 1)?;
    println!("{}", abs_test_report(landing, (cap.width, cap.height)));
    Ok(())
}

fn abs_test_report(
    landing: abs_pointer::PointerLanding,
    dimensions: (u32, u32),
) -> serde_json::Value {
    serde_json::json!({
        "ok": true,
        "requested_x": landing.requested.0,
        "requested_y": landing.requested.1,
        "x": landing.emitted.0,
        "y": landing.emitted.1,
        "w": dimensions.0,
        "h": dimensions.1
    })
}

fn print_help() {
    println!(
        "computer-use-hyprland\n\nUsage:\n  computer-use-hyprland mcp\n  computer-use-hyprland doctor\n  computer-use-hyprland setup\n  computer-use-hyprland apps\n  computer-use-hyprland state [APP_NAME]\n  computer-use-hyprland screenshot\n  computer-use-hyprland windows\n  computer-use-hyprland abs-test X Y"
    );
}

#[cfg(test)]
mod tests {
    use super::{abs_pointer, abs_test_report};

    #[test]
    fn abs_test_report_distinguishes_requested_and_emitted_coordinates() {
        assert_eq!(
            abs_test_report(
                abs_pointer::PointerLanding {
                    requested: (1920, 1080),
                    emitted: (1919, 1079),
                },
                (1920, 1080)
            ),
            serde_json::json!({
                "ok": true,
                "requested_x": 1920,
                "requested_y": 1080,
                "x": 1919,
                "y": 1079,
                "w": 1920,
                "h": 1080
            })
        );
    }
}
