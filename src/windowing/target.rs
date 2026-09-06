use crate::windowing::registry::{self, WINDOW_PERMISSION_HINT};
use crate::windowing::types::{WindowFocusResult, WindowInfo, WindowTarget};
use anyhow::{Result, bail};
use tokio::time::{Duration, Instant, sleep_until, timeout_at};

const FOCUS_VERIFY_TIMEOUT: Duration = Duration::from_secs(1);
const FOCUS_VERIFY_DELAY: Duration = Duration::from_millis(50);

pub async fn list_windows() -> Result<Vec<WindowInfo>> {
    registry::list_windows().await
}

pub async fn focused_window() -> Result<Option<WindowInfo>> {
    current_focused_window().await
}

pub async fn focus_window_target(target: &WindowTarget) -> Result<WindowFocusResult> {
    if !target.has_target() {
        bail!(
            "Pass window_id, pid, app_id, wm_class, title, tty, terminal_pid, terminal_command, or terminal_cwd to target a window."
        );
    }

    let windows = list_windows().await?;
    let requested_window = resolve_window_target(&windows, target)?.clone();

    registry::activate_window(&requested_window).await?;

    let focused_window = wait_for_focused_window(&requested_window).await;
    let exact_window_focused = focused_window
        .as_ref()
        .is_some_and(|window| window.window_id == requested_window.window_id);
    let app_focused = focused_window.as_ref().is_some_and(|window| {
        same_optional_string(window.app_id.as_deref(), requested_window.app_id.as_deref())
    });

    Ok(WindowFocusResult {
        backend: requested_window.backend.clone(),
        requested_window,
        focused_window,
        exact_window_focused,
        app_focused,
        note: "Computer Use activated the requested window through the available window backend, then verified focus through a fresh window query."
            .to_string(),
    })
}

async fn current_focused_window() -> Result<Option<WindowInfo>> {
    Ok(list_windows()
        .await?
        .into_iter()
        .find(|window| window.focused))
}

async fn wait_for_focused_window(requested_window: &WindowInfo) -> Option<WindowInfo> {
    wait_for_focused_window_with(
        requested_window,
        FOCUS_VERIFY_TIMEOUT,
        registry::focused_window,
    )
    .await
}

async fn wait_for_focused_window_with<F, Fut>(
    requested_window: &WindowInfo,
    verify_timeout: Duration,
    mut query: F,
) -> Option<WindowInfo>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<Option<WindowInfo>>>,
{
    let deadline = Instant::now() + verify_timeout;
    let mut last_focused_window = None;
    loop {
        match timeout_at(deadline, query()).await {
            Ok(Ok(focused_window)) => {
                if focused_window
                    .as_ref()
                    .is_some_and(|window| window.window_id == requested_window.window_id)
                {
                    return focused_window;
                }
                if focused_window.is_some() {
                    last_focused_window = focused_window;
                }
            }
            Ok(Err(_)) => {}
            Err(_) => break,
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        sleep_until((now + FOCUS_VERIFY_DELAY).min(deadline)).await;
    }
    last_focused_window
}

pub fn resolve_window_target<'a>(
    windows: &'a [WindowInfo],
    target: &WindowTarget,
) -> Result<&'a WindowInfo> {
    if let Some(window_id) = target.window_id {
        return resolve_window_id_target(windows, target, window_id);
    }

    if target.has_terminal_target() {
        let matches = windows
            .iter()
            .filter(|window| window_matches_terminal_target(window, target))
            .filter(|window| target.pid.is_none_or(|pid| window.pid == Some(pid)))
            .filter(|window| {
                optional_exact_match(window.app_id.as_deref(), target.app_id.as_deref())
            })
            .filter(|window| {
                optional_exact_match(window.wm_class.as_deref(), target.wm_class.as_deref())
            })
            .filter(|window| optional_title_match(window.title.as_deref(), target.title.as_deref()))
            .collect::<Vec<_>>();
        return unique_window_match(&matches, "terminal target");
    }

    if let Some(pid) = target.pid {
        let matches = windows
            .iter()
            .filter(|window| window.pid == Some(pid))
            .collect::<Vec<_>>();
        return unique_window_match(&matches, &format!("pid {pid}"));
    }

    if let Some(app_id) = normalized_target(target.app_id.as_deref()) {
        let matches = windows
            .iter()
            .filter(|window| {
                window
                    .app_id
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(&app_id))
            })
            .collect::<Vec<_>>();
        return unique_window_match(&matches, &format!("app_id {app_id}"));
    }

    if let Some(wm_class) = normalized_target(target.wm_class.as_deref()) {
        let matches = windows
            .iter()
            .filter(|window| {
                window
                    .wm_class
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(&wm_class))
            })
            .collect::<Vec<_>>();
        return unique_window_match(&matches, &format!("wm_class {wm_class}"));
    }

    if let Some(title) = normalized_target(target.title.as_deref()) {
        return resolve_title_target(windows, &title);
    }

    bail!(
        "Pass window_id, pid, app_id, wm_class, title, tty, terminal_pid, terminal_command, or terminal_cwd to target a window."
    );
}

/// Resolve a `title` selector through three narrowing passes: the windows
/// whose title *is* it, then the ones whose title is it but for letter case,
/// then the ones that merely contain it. The first pass with anything in it
/// decides.
///
/// That ladder is what keeps a window named "Sophia" reachable by that name
/// while an editor two workspaces away carries "sophia" in a project title —
/// two windows a case-insensitive match cannot tell apart. When the deciding
/// pass still holds more than one window the selector refuses and names them,
/// the way an ambiguous element selector does: driving the wrong window is
/// worse than answering a question.
fn resolve_title_target<'a>(windows: &'a [WindowInfo], title: &str) -> Result<&'a WindowInfo> {
    let needle = title.to_ascii_lowercase();
    let titles = |predicate: fn(&str, &str) -> bool| {
        windows
            .iter()
            .filter(|window| {
                window
                    .title
                    .as_deref()
                    .is_some_and(|value| predicate(value.trim(), title))
            })
            .collect::<Vec<_>>()
    };

    let description = format!("title {title}");
    let exact = titles(|value, title| value == title);
    if !exact.is_empty() {
        return unique_window_match(&exact, &description);
    }
    let same_but_for_case = titles(str::eq_ignore_ascii_case);
    if !same_but_for_case.is_empty() {
        return unique_window_match(&same_but_for_case, &description);
    }
    let contains = windows
        .iter()
        .filter(|window| {
            window
                .title
                .as_deref()
                .is_some_and(|value| value.to_ascii_lowercase().contains(&needle))
        })
        .collect::<Vec<_>>();
    unique_window_match(&contains, &description)
}

fn resolve_window_id_target<'a>(
    windows: &'a [WindowInfo],
    target: &WindowTarget,
    window_id: u64,
) -> Result<&'a WindowInfo> {
    if let Some(window) = windows.iter().find(|window| window.window_id == window_id) {
        return Ok(window);
    }

    let matches = windows
        .iter()
        .filter(|window| window_id_matches_json_number(window.window_id, window_id))
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [window] => Ok(*window),
        [] => Err(anyhow::anyhow!("No window matched window_id {window_id}.")),
        windows => resolve_rounded_window_id_matches(windows, target, window_id),
    }
}

fn resolve_rounded_window_id_matches<'a>(
    windows: &[&'a WindowInfo],
    target: &WindowTarget,
    window_id: u64,
) -> Result<&'a WindowInfo> {
    let ids = windows
        .iter()
        .map(|window| window.window_id.to_string())
        .collect::<Vec<_>>()
        .join(", ");

    if has_window_id_disambiguator(target) {
        let matches = windows
            .iter()
            .copied()
            .filter(|window| window_id_disambiguators_match(window, target))
            .collect::<Vec<_>>();
        match matches.as_slice() {
            [window] => return Ok(*window),
            [] => bail!(
                "window_id {window_id} matched multiple windows after JSON number rounding ({ids}), but none matched the provided title, pid, app_id, or wm_class disambiguators."
            ),
            _ => {}
        }
    }

    bail!(
        "window_id {window_id} matched multiple windows after JSON number rounding ({ids}); add title, pid, app_id, or wm_class to disambiguate."
    );
}

fn has_window_id_disambiguator(target: &WindowTarget) -> bool {
    target.pid.is_some()
        || normalized_target(target.app_id.as_deref()).is_some()
        || normalized_target(target.wm_class.as_deref()).is_some()
        || normalized_target(target.title.as_deref()).is_some()
}

fn window_id_disambiguators_match(window: &WindowInfo, target: &WindowTarget) -> bool {
    target.pid.is_none_or(|pid| window.pid == Some(pid))
        && optional_exact_match(window.app_id.as_deref(), target.app_id.as_deref())
        && optional_exact_match(window.wm_class.as_deref(), target.wm_class.as_deref())
        && optional_title_match(window.title.as_deref(), target.title.as_deref())
}

#[expect(
    clippy::float_cmp,
    reason = "the equality is the question: whether two u64 ids collapse to the same f64 after a JSON round trip"
)]
fn window_id_matches_json_number(actual: u64, requested: u64) -> bool {
    const JS_SAFE_INTEGER_MAX: u64 = (1_u64 << 53) - 1;
    (actual > JS_SAFE_INTEGER_MAX || requested > JS_SAFE_INTEGER_MAX)
        && (actual as f64) == (requested as f64)
}

fn unique_window_match<'a>(
    matches: &[&'a WindowInfo],
    description: &str,
) -> Result<&'a WindowInfo> {
    match matches {
        [window] => Ok(*window),
        [] => bail!("No window matched {description}."),
        windows => {
            bail!(
                "{description} matched multiple windows ({}); add window_id or another selector to disambiguate.",
                describe_windows(windows)
            );
        }
    }
}

/// The windows an ambiguous selector matched, in the terms the caller can
/// pick one by.
fn describe_windows(windows: &[&WindowInfo]) -> String {
    windows
        .iter()
        .map(|window| {
            format!(
                "window_id {} {:?} [{}]",
                window.window_id,
                window.title.as_deref().unwrap_or(""),
                window
                    .app_id
                    .as_deref()
                    .or(window.wm_class.as_deref())
                    .unwrap_or("unknown app")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn window_matches_terminal_target(window: &WindowInfo, target: &WindowTarget) -> bool {
    let Some(terminal) = &window.terminal else {
        return false;
    };

    if let Some(tty) = normalized_target(target.tty.as_deref())
        && !tty_matches(&terminal.tty, &tty)
    {
        return false;
    }

    if let Some(pid) = target.terminal_pid {
        let active_pid = terminal.active_process.as_ref().map(|process| process.pid);
        if active_pid != Some(pid) && terminal.root_process.pid != pid {
            return false;
        }
    }

    if let Some(command) = normalized_target(target.terminal_command.as_deref()) {
        let command = command.to_ascii_lowercase();
        let active_matches = terminal
            .active_process
            .as_ref()
            .is_some_and(|process| terminal_process_matches_command(process, &command));
        if !active_matches && !terminal_process_matches_command(&terminal.root_process, &command) {
            return false;
        }
    }

    if let Some(cwd) = normalized_target(target.terminal_cwd.as_deref()) {
        let active_matches = terminal
            .active_process
            .as_ref()
            .is_some_and(|process| terminal_process_matches_cwd(process, &cwd));
        if !active_matches && !terminal_process_matches_cwd(&terminal.root_process, &cwd) {
            return false;
        }
    }

    true
}

fn terminal_process_matches_command(
    process: &crate::terminal::TerminalProcess,
    command_lower: &str,
) -> bool {
    process
        .command_name
        .to_ascii_lowercase()
        .contains(command_lower)
        || process
            .command_line
            .to_ascii_lowercase()
            .contains(command_lower)
}

fn terminal_process_matches_cwd(process: &crate::terminal::TerminalProcess, cwd: &str) -> bool {
    let requested = cwd.trim_end_matches('/');
    process.cwd.as_deref().is_some_and(|value| {
        let actual = value.trim_end_matches('/');
        actual == requested
            || (!requested.starts_with('/')
                && actual
                    .strip_suffix(requested)
                    .is_some_and(|prefix| prefix.ends_with('/')))
    })
}

fn tty_matches(actual: &str, requested: &str) -> bool {
    actual == requested
        || actual
            .strip_prefix("/dev/")
            .is_some_and(|value| value == requested)
        || actual
            .strip_prefix("/dev/pts/")
            .is_some_and(|value| value == requested)
}

fn optional_exact_match(actual: Option<&str>, requested: Option<&str>) -> bool {
    normalized_target(requested)
        .is_none_or(|requested| actual.is_some_and(|value| value.eq_ignore_ascii_case(&requested)))
}

fn optional_title_match(actual: Option<&str>, requested: Option<&str>) -> bool {
    normalized_target(requested).is_none_or(|requested| {
        let requested = requested.to_ascii_lowercase();
        actual.is_some_and(|value| value.to_ascii_lowercase().contains(&requested))
    })
}

pub fn window_permission_hint(error: &str) -> Option<String> {
    let lower = error.to_ascii_lowercase();
    (lower.contains("accessdenied")
        || lower.contains("access denied")
        || lower.contains("not allowed")
        || lower.contains("operation not permitted")
        || lower.contains("failed to connect to session bus"))
    .then(|| WINDOW_PERMISSION_HINT.to_string())
}

fn normalized_target(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn same_optional_string(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(window_id: u64, title: &str, app_id: &str) -> WindowInfo {
        WindowInfo {
            window_id,
            title: Some(title.to_string()),
            app_id: Some(app_id.to_string()),
            wm_class: Some(app_id.to_string()),
            pid: Some(u32::try_from(window_id).unwrap_or(1)),
            bounds: None,
            workspace: None,
            floating: None,
            focused: false,
            hidden: false,
            client_type: None,
            backend: "test".to_string(),
            terminal: None,
        }
    }

    fn title_target(title: &str) -> WindowTarget {
        WindowTarget {
            title: Some(title.to_string()),
            ..WindowTarget::default()
        }
    }

    #[test]
    fn an_exact_title_wins_over_a_window_that_merely_contains_it() {
        let windows = vec![
            window(1, "sophia", "jetbrains-rustrover"),
            window(2, "Sophia", "sophia-desktop"),
        ];

        let resolved = resolve_window_target(&windows, &title_target("Sophia")).unwrap();

        assert_eq!(resolved.window_id, 2);
    }

    #[test]
    fn two_titles_that_differ_only_by_case_refuse_the_ambiguous_one() {
        let windows = vec![
            window(1, "sophia", "jetbrains-rustrover"),
            window(2, "Sophia", "sophia-desktop"),
        ];

        let error = resolve_window_target(&windows, &title_target("SOPHIA"))
            .expect_err("neither title is the one that was asked for")
            .to_string();

        assert!(error.contains("window_id 1"), "{error}");
        assert!(error.contains("window_id 2"), "{error}");
    }

    #[test]
    fn a_title_matching_two_windows_refuses_and_names_them() {
        let windows = vec![
            window(1, "sophia – src/main.rs", "jetbrains-rustrover"),
            window(2, "sophia desktop", "sophia-desktop"),
        ];

        let error = resolve_window_target(&windows, &title_target("sophia"))
            .expect_err("an ambiguous title must not pick one silently")
            .to_string();

        assert!(error.contains("window_id 1"), "{error}");
        assert!(error.contains("window_id 2"), "{error}");
        assert!(error.contains("jetbrains-rustrover"), "{error}");
    }

    #[test]
    fn two_windows_of_one_app_refuse_an_app_id_selector() {
        let windows = vec![
            window(1, "left terminal", "com.mitchellh.ghostty"),
            window(2, "right terminal", "com.mitchellh.ghostty"),
        ];
        let target = WindowTarget {
            app_id: Some("com.mitchellh.ghostty".to_string()),
            ..WindowTarget::default()
        };

        let error = resolve_window_target(&windows, &target)
            .expect_err("two windows of one app must not resolve to the first")
            .to_string();

        assert!(error.contains("app_id com.mitchellh.ghostty"), "{error}");
        assert!(error.contains("window_id"), "{error}");
    }

    #[test]
    fn a_title_that_matches_one_window_still_resolves() {
        let windows = vec![
            window(1, "sophia", "jetbrains-rustrover"),
            window(2, "Aprenda Rust", "google-chrome"),
        ];

        let resolved = resolve_window_target(&windows, &title_target("rust")).unwrap();

        assert_eq!(resolved.window_id, 2);
    }

    #[test]
    fn focus_verification_allows_workspace_transition_latency() {
        assert!(FOCUS_VERIFY_TIMEOUT >= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn slow_focus_query_cannot_exceed_verification_deadline() {
        let requested_window = WindowInfo {
            window_id: 1,
            title: None,
            app_id: None,
            wm_class: None,
            pid: None,
            bounds: None,
            workspace: None,
            floating: None,
            focused: false,
            hidden: false,
            client_type: None,
            backend: "test".to_string(),
            terminal: None,
        };
        let started = Instant::now();

        let focused =
            wait_for_focused_window_with(&requested_window, Duration::from_millis(20), || async {
                tokio::time::sleep(Duration::from_secs(1)).await;
                Ok::<_, anyhow::Error>(None)
            })
            .await;

        assert!(focused.is_none());
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
