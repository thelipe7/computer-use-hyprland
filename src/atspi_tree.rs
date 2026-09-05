use crate::diagnostics::hydrate_session_bus_env;
use anyhow::{Context, Result, anyhow};
use atspi::{
    CoordType, ObjectRef, ObjectRefOwned, StateSet,
    proxy::{
        accessible::{AccessibleProxy, ObjectRefExt},
        proxy_ext::ProxyExt,
        text::TextProxy,
    },
};
// Direct dependency (p2p feature off) — see Cargo.toml for why we bypass
// atspi's "connection" re-export.
use atspi_connection::AccessibilityConnection;
use futures_util::{StreamExt, stream};
use schemars::JsonSchema;
use serde::Serialize;
use std::{collections::VecDeque, time::Duration};
use tokio::time::timeout;
use zbus::{
    fdo::DBusProxy,
    names::{BusName, UniqueName},
    zvariant::ObjectPath,
};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibleAppSummary {
    pub object_ref: String,
    pub name: Option<String>,
    pub pid: Option<u32>,
    pub role: String,
    pub child_count: i32,
    pub bounds: Option<Bounds>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityNode {
    pub index: u32,
    pub parent_index: Option<u32>,
    pub depth: u32,
    pub object_ref: String,
    pub role: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub child_count: i32,
    pub bounds: Option<Bounds>,
    pub states: Vec<String>,
    pub actions: Vec<AccessibilityAction>,
    pub value: Option<AccessibilityValue>,
    pub text: Option<AccessibilityText>,
    pub supports_editable_text: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Bounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityAction {
    pub index: i32,
    pub name: String,
    pub description: String,
    pub keybinding: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityValue {
    pub current: f64,
    pub minimum: f64,
    pub maximum: f64,
    pub minimum_increment: f64,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityText {
    pub character_count: i32,
    pub caret_offset: Option<i32>,
    pub content: Option<String>,
    pub truncated: bool,
    pub selections: Vec<AccessibilityTextSelection>,
    /// Why `selections` is empty when the selection count could not be read
    /// at all, which is a different answer from "nothing is selected".
    /// Serialized only when present, so a large tree does not carry one null
    /// per text node.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct AccessibilityTextSelection {
    pub start_offset: i32,
    pub end_offset: i32,
}

#[derive(Debug, Clone)]
pub struct ActionInvocation {
    pub action_index: i32,
    pub action_name: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub enum ValueSetInvocation {
    Numeric { value: f64 },
    EditableText,
}

/// One app's accessibility tree and the processes whose AT-SPI roots it was
/// walked from.
///
/// The pids are the only link between a tree and the compositor window it was
/// drawn in: an accesskit app reports window-relative bounds, so a caller that
/// named no window target still needs a window to offset them by, and the
/// snapshot's own process is what finds one.
#[derive(Debug, Clone, Default)]
pub struct TreeSnapshot {
    pub nodes: Vec<AccessibilityNode>,
    pub root_pids: Vec<u32>,
}

const MAX_TEXT_READBACK_CHARS: i32 = 4096;
const MAX_TEXT_SELECTIONS: i32 = 8;
const DEFAULT_SNAPSHOT_MAX_NODES: usize = 1_000;
const HARD_SNAPSHOT_MAX_NODES: usize = 2_000;
const DEFAULT_SNAPSHOT_MAX_DEPTH: u32 = 32;
const HARD_SNAPSHOT_MAX_DEPTH: u32 = 64;
const CHILD_READ_CONCURRENCY: usize = 16;
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_DISCOVERY_ROOTS: usize = 256;
const ROOT_MATCH_CHILD_LIMIT: usize = 8;
const MAX_DISCOVERY_CHILD_READS: usize = MAX_DISCOVERY_ROOTS * ROOT_MATCH_CHILD_LIMIT;
/// Above this many selected roots the snapshot spans too many apps for its
/// pids to name one window, so they are not looked up at all.
const MAX_SNAPSHOT_ROOT_PIDS: usize = 8;

fn snapshot_child_read_budgets(max_nodes: usize) -> (usize, usize, usize) {
    (MAX_DISCOVERY_ROOTS, MAX_DISCOVERY_CHILD_READS, max_nodes)
}

pub(crate) fn snapshot_limits(
    requested_max_nodes: Option<usize>,
    requested_max_depth: Option<u32>,
) -> (usize, u32) {
    (
        requested_max_nodes
            .unwrap_or(DEFAULT_SNAPSHOT_MAX_NODES)
            .clamp(1, HARD_SNAPSHOT_MAX_NODES),
        requested_max_depth
            .unwrap_or(DEFAULT_SNAPSHOT_MAX_DEPTH)
            .min(HARD_SNAPSHOT_MAX_DEPTH),
    )
}

struct BoundedTraversal<T> {
    queue: VecDeque<T>,
    attempted: usize,
    max_items: usize,
}

impl<T> BoundedTraversal<T> {
    fn new(max_items: usize) -> Self {
        Self {
            queue: VecDeque::new(),
            attempted: 0,
            max_items,
        }
    }

    fn enqueue(&mut self, items: impl IntoIterator<Item = T>) {
        self.queue
            .extend(items.into_iter().take(self.remaining_capacity()));
    }

    fn pop(&mut self) -> Option<T> {
        if self.attempted >= self.max_items {
            return None;
        }
        let item = self.queue.pop_front()?;
        self.attempted += 1;
        Some(item)
    }

    fn remaining_capacity(&self) -> usize {
        self.max_items
            .saturating_sub(self.attempted.saturating_add(self.queue.len()))
    }
}

fn bounded_child_count(reported: i32, limit: usize) -> usize {
    usize::try_from(reported).unwrap_or_default().min(limit)
}

struct IndexedReadBatch<T> {
    items: Vec<T>,
    attempted: usize,
}

impl<T> IndexedReadBatch<T> {
    fn all_failed(&self) -> bool {
        self.attempted > 0 && self.items.is_empty()
    }
}

async fn fetch_indexed_up_to<T, E, F, Fut>(
    reported: i32,
    limit: usize,
    remaining_attempts: &mut usize,
    fetch: F,
) -> IndexedReadBatch<T>
where
    F: Fn(i32) -> Fut,
    Fut: Future<Output = std::result::Result<T, E>>,
{
    let attempt_count = bounded_child_count(reported, limit).min(*remaining_attempts);
    *remaining_attempts = (*remaining_attempts).saturating_sub(attempt_count);
    let end_index = i32::try_from(attempt_count).unwrap_or(i32::MAX);

    let items = stream::iter(0..end_index)
        .map(fetch)
        .buffered(CHILD_READ_CONCURRENCY)
        .filter_map(|result| async move { result.ok() })
        .collect()
        .await;

    IndexedReadBatch {
        items,
        attempted: attempt_count,
    }
}

async fn children_up_to(
    proxy: &AccessibleProxy<'_>,
    limit: usize,
    remaining_attempts: &mut usize,
) -> zbus::Result<IndexedReadBatch<ObjectRefOwned>> {
    if limit == 0 || *remaining_attempts == 0 {
        return Ok(IndexedReadBatch {
            items: Vec::new(),
            attempted: 0,
        });
    }

    let child_count = proxy.child_count().await?;
    Ok(
        fetch_indexed_up_to(child_count, limit, remaining_attempts, |index| {
            proxy.get_child_at_index(index)
        })
        .await,
    )
}

pub async fn list_accessible_apps(limit: usize) -> Result<Vec<AccessibleAppSummary>> {
    let conn = connect().await?;
    let mut remaining_child_reads = limit;
    let roots = registry_children(&conn, limit, &mut remaining_child_reads).await?;
    let dbus = DBusProxy::new(conn.connection()).await.ok();
    let mut apps = Vec::new();

    for object_ref in roots.into_iter().take(limit) {
        if let Ok(proxy) = open_accessible(&conn, &object_ref).await {
            apps.push(read_app_summary(&proxy, &object_ref, dbus.as_ref()).await);
        }
    }

    Ok(apps)
}

pub async fn snapshot_tree(
    app_name_or_bundle_identifier: Option<&str>,
    target_pid: Option<u32>,
    max_nodes: usize,
    max_depth: u32,
) -> Result<TreeSnapshot> {
    let (max_nodes, max_depth) = snapshot_limits(Some(max_nodes), Some(max_depth));
    timeout(
        SNAPSHOT_TIMEOUT,
        snapshot_tree_inner(
            app_name_or_bundle_identifier,
            target_pid,
            max_nodes,
            max_depth,
        ),
    )
    .await
    .context("AT-SPI snapshot exceeded its 10-second deadline")?
}

async fn snapshot_tree_inner(
    app_name_or_bundle_identifier: Option<&str>,
    target_pid: Option<u32>,
    max_nodes: usize,
    max_depth: u32,
) -> Result<TreeSnapshot> {
    let conn = connect().await?;
    // App discovery is bounded independently so a tiny requested tree still
    // finds a target registered after the first accessibility root.
    let (mut remaining_registry_reads, mut remaining_filter_reads, mut remaining_traversal_reads) =
        snapshot_child_read_budgets(max_nodes);
    let roots =
        registry_children(&conn, MAX_DISCOVERY_ROOTS, &mut remaining_registry_reads).await?;
    let selected_roots = select_roots(
        &conn,
        roots,
        app_name_or_bundle_identifier,
        target_pid,
        &mut remaining_filter_reads,
    )
    .await;
    let root_pids = root_pids(conn.connection(), &selected_roots).await;
    let mut nodes = Vec::new();
    let mut traversal = BoundedTraversal::new(max_nodes);

    traversal.enqueue(
        selected_roots
            .into_iter()
            .map(|object_ref| (object_ref, 0_u32, None)),
    );

    while let Some((object_ref, depth, parent_index)) = traversal.pop() {
        let Ok(proxy) = open_accessible(&conn, &object_ref).await else {
            continue;
        };
        let index = nodes.len() as u32;
        let remaining = traversal.remaining_capacity();
        let child_refs = if depth < max_depth && remaining > 0 {
            children_up_to(&proxy, remaining, &mut remaining_traversal_reads)
                .await
                .map(|batch| batch.items)
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        nodes.push(read_node(&proxy, &object_ref, index, parent_index, depth).await);

        traversal.enqueue(
            child_refs
                .into_iter()
                .map(|child| (child, depth + 1, Some(index))),
        );
    }

    Ok(TreeSnapshot { nodes, root_pids })
}

/// The distinct processes owning the roots a snapshot walked.
///
/// Bounded by [`MAX_SNAPSHOT_ROOT_PIDS`]: a snapshot that selected more roots
/// than that spans the whole desktop rather than one app, and no single window
/// could be its own, so the lookup is skipped instead of costing one bus call
/// per registered app.
async fn root_pids(conn: &zbus::Connection, roots: &[ObjectRefOwned]) -> Vec<u32> {
    if roots.is_empty() || roots.len() > MAX_SNAPSHOT_ROOT_PIDS {
        return Vec::new();
    }
    let Ok(dbus) = DBusProxy::new(conn).await else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = Vec::new();
    for object_ref in roots {
        if let Some(pid) = object_ref_pid(Some(&dbus), object_ref).await
            && !pids.contains(&pid)
        {
            pids.push(pid);
        }
    }
    pids
}

/// Compact description of the AT-SPI element that currently holds keyboard
/// focus, used as post-input feedback for type_text/press_key.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct FocusedElementSummary {
    pub role: String,
    pub name: Option<String>,
    pub editable: bool,
    pub states: Vec<String>,
}

const FOCUS_PROBE_MAX_NODES: usize = 400;
const FOCUS_PROBE_MAX_DEPTH: u32 = 16;

/// Find the element with the `focused` state inside the target app (by pid) or
/// across all apps. Best-effort and bounded: returns Ok(None) when no focused
/// element is reachable through AT-SPI (common for apps without accessibility
/// support, e.g. Electron without --force-renderer-accessibility).
pub async fn focused_element_summary(
    target_pid: Option<u32>,
) -> Result<Option<FocusedElementSummary>> {
    let conn = connect().await?;
    let mut remaining_registry_reads = MAX_DISCOVERY_ROOTS;
    let roots =
        registry_children(&conn, MAX_DISCOVERY_ROOTS, &mut remaining_registry_reads).await?;
    let mut remaining_filter_reads = MAX_DISCOVERY_CHILD_READS;
    let selected_roots =
        select_roots(&conn, roots, None, target_pid, &mut remaining_filter_reads).await;
    let mut traversal = BoundedTraversal::new(FOCUS_PROBE_MAX_NODES);
    let mut remaining_traversal_reads = FOCUS_PROBE_MAX_NODES;

    traversal.enqueue(
        selected_roots
            .into_iter()
            .map(|object_ref| (object_ref, 0_u32)),
    );

    while let Some((object_ref, depth)) = traversal.pop() {
        let Ok(proxy) = open_accessible(&conn, &object_ref).await else {
            continue;
        };
        let Ok(state) = proxy.get_state().await else {
            continue;
        };
        if state.contains(atspi::State::Focused) {
            let proxies = proxy.proxies().await.ok();
            return Ok(Some(FocusedElementSummary {
                role: role_name(&proxy).await,
                name: optional_string(proxy.name().await.ok()),
                editable: supports_editable_text(proxies.as_ref()).await,
                states: state_labels(state),
            }));
        }
        if depth < FOCUS_PROBE_MAX_DEPTH {
            let remaining = traversal.remaining_capacity();
            let children = children_up_to(&proxy, remaining, &mut remaining_traversal_reads)
                .await
                .map(|batch| batch.items)
                .unwrap_or_default();
            traversal.enqueue(children.into_iter().map(|child| (child, depth + 1)));
        }
    }

    Ok(None)
}

pub async fn perform_action(
    object_ref_id: &str,
    requested_action: Option<&str>,
) -> Result<ActionInvocation> {
    let conn = connect().await?;
    let object_ref = object_ref_from_id(object_ref_id)?;
    let proxy = open_accessible(&conn, &object_ref)
        .await
        .with_context(|| format!("failed to open AT-SPI object {object_ref_id}"))?;
    let action = proxy
        .proxies()
        .await?
        .action()
        .await
        .context("element does not expose the AT-SPI Action interface")?;
    let actions = action.get_actions().await.unwrap_or_default();
    let action_index = select_action_index(&actions, requested_action)?;
    let action_name = actions
        .get(action_index as usize)
        .map(|action| action.name.clone());
    let ok = action
        .do_action(action_index)
        .await
        .with_context(|| format!("failed to invoke AT-SPI action {action_index}"))?;

    Ok(ActionInvocation {
        action_index,
        action_name,
        ok,
    })
}

/// True when the error says the AT-SPI object's owner is gone from the bus:
/// the app restarted or the window closed, so every cached object ref of that
/// tree is dead and only a fresh get_app_state can recover.
pub fn is_stale_object_error(error: &anyhow::Error) -> bool {
    const STALE_MARKERS: [&str; 5] = [
        "ServiceUnknown",
        "UnknownObject",
        "NameHasNoOwner",
        "not activatable",
        "was not provided by any .service files",
    ];
    error.chain().any(|cause| {
        if let Some(zbus::Error::FDO(fdo)) = cause.downcast_ref::<zbus::Error>()
            && matches!(
                **fdo,
                zbus::fdo::Error::ServiceUnknown(_)
                    | zbus::fdo::Error::UnknownObject(_)
                    | zbus::fdo::Error::NameHasNoOwner(_)
            )
        {
            return true;
        }
        let text = cause.to_string();
        STALE_MARKERS.iter().any(|marker| text.contains(marker))
    })
}

/// Current AT-SPI state labels of one element.
pub async fn element_states(object_ref_id: &str) -> Result<Vec<String>> {
    let conn = connect().await?;
    let object_ref = object_ref_from_id(object_ref_id)?;
    let proxy = open_accessible(&conn, &object_ref)
        .await
        .with_context(|| format!("failed to open AT-SPI object {object_ref_id}"))?;
    let state = proxy
        .get_state()
        .await
        .with_context(|| format!("failed to read AT-SPI states of {object_ref_id}"))?;
    Ok(state_labels(state))
}

/// Ask the element to take keyboard focus through AT-SPI `Component.GrabFocus`.
pub async fn grab_focus(object_ref_id: &str) -> Result<bool> {
    let conn = connect().await?;
    let object_ref = object_ref_from_id(object_ref_id)?;
    let proxy = open_accessible(&conn, &object_ref)
        .await
        .with_context(|| format!("failed to open AT-SPI object {object_ref_id}"))?;
    let component = proxy
        .proxies()
        .await?
        .component()
        .await
        .context("element does not expose the AT-SPI Component interface")?;
    component
        .grab_focus()
        .await
        .with_context(|| format!("AT-SPI GrabFocus failed for {object_ref_id}"))
}

pub async fn set_element_value(object_ref_id: &str, value: &str) -> Result<ValueSetInvocation> {
    let conn = connect().await?;
    let object_ref = object_ref_from_id(object_ref_id)?;
    let proxy = open_accessible(&conn, &object_ref)
        .await
        .with_context(|| format!("failed to open AT-SPI object {object_ref_id}"))?;
    let proxies = proxy.proxies().await?;

    if let Ok(numeric_value) = value.parse::<f64>()
        && let Ok(value_proxy) = proxies.value().await
    {
        value_proxy
            .set_current_value(numeric_value)
            .await
            .with_context(|| format!("failed to set AT-SPI numeric value to {numeric_value}"))?;
        return Ok(ValueSetInvocation::Numeric {
            value: numeric_value,
        });
    }

    if let Ok(editable_text) = proxies.editable_text().await {
        let ok = editable_text
            .set_text_contents(value)
            .await
            .context("failed to set AT-SPI editable text contents")?;
        if ok {
            return Ok(ValueSetInvocation::EditableText);
        }
        return Err(anyhow!("AT-SPI EditableText rejected the new contents"));
    }

    if value.parse::<f64>().is_err() && proxies.value().await.is_ok() {
        return Err(anyhow!(
            "element exposes the AT-SPI Value interface, but the requested value is not numeric"
        ));
    }

    Err(anyhow!(
        "element does not expose AT-SPI Value or EditableText interfaces"
    ))
}

async fn connect() -> Result<AccessibilityConnection> {
    hydrate_session_bus_env();
    AccessibilityConnection::new()
        .await
        .context("failed to connect to AT-SPI bus")
}

/// Open an `AccessibleProxy` for an object on the a11y bus.
///
/// We deliberately avoid `AccessibilityConnection::object_as_accessible` (the
/// `P2P` trait). For apps that advertise a peer-to-peer bus address it routes
/// reads over that socket, but for apps that don't (notably GTK4 apps such as
/// Nautilus / Text Editor / baobab, which don't implement the legacy
/// `GetApplicationBusAddress`) it falls back to a proxy built with only a path
/// and *no destination*. On the shared a11y bus that proxy can't address the
/// app and every call fails with `ServiceUnknown`, which surfaces as an empty
/// tree (`role: "unknown"`, `child_count: 0`). `as_accessible_proxy` always
/// pins the destination to the object's bus name, so it works for every app
/// regardless of P2P support. See issue #31.
async fn open_accessible<'r>(
    conn: &AccessibilityConnection,
    object_ref: &'r ObjectRefOwned,
) -> Result<AccessibleProxy<'r>, atspi::AtspiError> {
    object_ref.as_accessible_proxy(conn.connection()).await
}

async fn registry_children(
    conn: &AccessibilityConnection,
    limit: usize,
    remaining_child_reads: &mut usize,
) -> Result<Vec<ObjectRefOwned>> {
    let root = conn
        .root_accessible_on_registry()
        .await
        .context("failed to open AT-SPI registry root")?;
    let batch = children_up_to(&root, limit, remaining_child_reads)
        .await
        .context("failed to read AT-SPI registry children")?;
    if batch.all_failed() {
        return Err(anyhow!(
            "AT-SPI registry reported children, but every indexed child read failed"
        ));
    }
    Ok(batch.items)
}

async fn select_roots(
    conn: &AccessibilityConnection,
    roots: Vec<ObjectRefOwned>,
    app_name_or_bundle_identifier: Option<&str>,
    target_pid: Option<u32>,
    remaining_child_reads: &mut usize,
) -> Vec<ObjectRefOwned> {
    let needle = app_name_or_bundle_identifier
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase());
    let dbus = DBusProxy::new(conn.connection()).await.ok();
    let mut remaining = roots;

    if let Some(target_pid) = target_pid {
        let mut pid_and_filter_matches = Vec::new();
        let mut pid_matches = Vec::new();
        let mut non_pid_matches = Vec::new();

        for object_ref in remaining {
            if object_ref_pid(dbus.as_ref(), &object_ref).await == Some(target_pid) {
                if let Some(needle) = needle.as_deref() {
                    if root_matches(conn, &object_ref, needle, remaining_child_reads).await {
                        pid_and_filter_matches.push(object_ref);
                    } else {
                        pid_matches.push(object_ref);
                    }
                } else {
                    pid_matches.push(object_ref);
                }
            } else {
                non_pid_matches.push(object_ref);
            }
        }

        if !pid_and_filter_matches.is_empty() {
            return pid_and_filter_matches;
        }
        if !pid_matches.is_empty() {
            return pid_matches;
        }

        remaining = non_pid_matches;
    }

    let Some(needle) = needle.as_deref() else {
        return remaining;
    };

    let mut selected = Vec::new();
    for object_ref in remaining {
        if root_matches(conn, &object_ref, needle, remaining_child_reads).await {
            selected.push(object_ref);
        }
    }

    selected
}

async fn root_matches(
    conn: &AccessibilityConnection,
    object_ref: &ObjectRefOwned,
    needle: &str,
    remaining_child_reads: &mut usize,
) -> bool {
    let Ok(proxy) = open_accessible(conn, object_ref).await else {
        return object_ref_id(object_ref)
            .to_ascii_lowercase()
            .contains(needle);
    };

    if proxy_matches(&proxy, object_ref, needle).await {
        return true;
    }

    for child_ref in children_up_to(&proxy, ROOT_MATCH_CHILD_LIMIT, remaining_child_reads)
        .await
        .map(|batch| batch.items)
        .unwrap_or_default()
    {
        let Ok(child_proxy) = open_accessible(conn, &child_ref).await else {
            continue;
        };
        if proxy_matches(&child_proxy, &child_ref, needle).await {
            return true;
        }
    }

    false
}

async fn proxy_matches(
    proxy: &AccessibleProxy<'_>,
    object_ref: &ObjectRefOwned,
    needle: &str,
) -> bool {
    let name = proxy.name().await.unwrap_or_default();
    let role = proxy.get_role_name().await.unwrap_or_default();
    format!("{} {} {}", object_ref_id(object_ref), name, role)
        .to_ascii_lowercase()
        .contains(needle)
}

async fn read_app_summary(
    proxy: &AccessibleProxy<'_>,
    object_ref: &ObjectRefOwned,
    dbus: Option<&DBusProxy<'_>>,
) -> AccessibleAppSummary {
    AccessibleAppSummary {
        object_ref: object_ref_id(object_ref),
        name: optional_string(proxy.name().await.ok()),
        pid: object_ref_pid(dbus, object_ref).await,
        role: role_name(proxy).await,
        child_count: proxy.child_count().await.unwrap_or_default(),
        bounds: bounds(proxy).await,
    }
}

async fn read_node(
    proxy: &AccessibleProxy<'_>,
    object_ref: &ObjectRefOwned,
    index: u32,
    parent_index: Option<u32>,
    depth: u32,
) -> AccessibilityNode {
    let proxies = proxy.proxies().await.ok();

    AccessibilityNode {
        index,
        parent_index,
        depth,
        object_ref: object_ref_id(object_ref),
        role: role_name(proxy).await,
        name: optional_string(proxy.name().await.ok()),
        description: optional_string(proxy.description().await.ok()),
        child_count: proxy.child_count().await.unwrap_or_default(),
        bounds: bounds_from_proxies(proxies.as_ref(), proxy).await,
        states: states_from_proxy(proxy).await,
        actions: actions_from_proxies(proxies.as_ref()).await,
        value: value_from_proxies(proxies.as_ref()).await,
        text: text_from_proxies(proxies.as_ref()).await,
        supports_editable_text: supports_editable_text(proxies.as_ref()).await,
    }
}

async fn role_name(proxy: &AccessibleProxy<'_>) -> String {
    if let Ok(role) = proxy.get_role_name().await
        && !role.trim().is_empty()
    {
        return role;
    }
    proxy
        .get_role()
        .await
        .map(|role| format!("{role:?}"))
        .unwrap_or_else(|_| "unknown".to_string())
}

async fn bounds(proxy: &AccessibleProxy<'_>) -> Option<Bounds> {
    bounds_from_proxies(proxy.proxies().await.ok().as_ref(), proxy).await
}

async fn object_ref_pid(dbus: Option<&DBusProxy<'_>>, object_ref: &ObjectRefOwned) -> Option<u32> {
    let dbus = dbus?;
    let bus_name = BusName::try_from(object_ref.name_as_str()?.to_string()).ok()?;
    dbus.get_connection_unix_process_id(bus_name).await.ok()
}

async fn bounds_from_proxies(
    proxies: Option<&atspi::proxy::proxy_ext::Proxies<'_>>,
    proxy: &AccessibleProxy<'_>,
) -> Option<Bounds> {
    let owned_proxies;
    let proxies = if let Some(proxies) = proxies {
        proxies
    } else {
        owned_proxies = proxy.proxies().await.ok()?;
        &owned_proxies
    };
    let component = proxies.component().await.ok()?;
    let (x, y, width, height) = component.get_extents(CoordType::Screen).await.ok()?;
    normalize_bounds(Bounds {
        x,
        y,
        width,
        height,
    })
}

fn normalize_bounds(bounds: Bounds) -> Option<Bounds> {
    if bounds.width <= 0 || bounds.height <= 0 {
        return None;
    }
    if bounds.x <= i32::MIN / 2 || bounds.y <= i32::MIN / 2 {
        return None;
    }
    Some(bounds)
}

async fn actions_from_proxies(
    proxies: Option<&atspi::proxy::proxy_ext::Proxies<'_>>,
) -> Vec<AccessibilityAction> {
    let Some(proxies) = proxies else {
        return Vec::new();
    };
    let Ok(action_proxy) = proxies.action().await else {
        return Vec::new();
    };

    action_proxy
        .get_actions()
        .await
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(index, action)| AccessibilityAction {
            index: index as i32,
            name: action.name,
            description: action.description,
            keybinding: action.keybinding,
        })
        .collect()
}

async fn states_from_proxy(proxy: &AccessibleProxy<'_>) -> Vec<String> {
    proxy
        .get_state()
        .await
        .map(state_labels)
        .unwrap_or_default()
}

async fn value_from_proxies(
    proxies: Option<&atspi::proxy::proxy_ext::Proxies<'_>>,
) -> Option<AccessibilityValue> {
    let value = proxies?.value().await.ok()?;
    Some(AccessibilityValue {
        current: value.current_value().await.ok()?,
        minimum: value.minimum_value().await.ok()?,
        maximum: value.maximum_value().await.ok()?,
        minimum_increment: value.minimum_increment().await.ok()?,
        text: optional_string(value.text().await.ok()),
    })
}

async fn text_from_proxies(
    proxies: Option<&atspi::proxy::proxy_ext::Proxies<'_>>,
) -> Option<AccessibilityText> {
    let text = proxies?.text().await.ok()?;
    let character_count = text.character_count().await.ok()?.max(0);
    let caret_offset = text.caret_offset().await.ok();
    let capped_count = character_count.min(MAX_TEXT_READBACK_CHARS);
    let content = if capped_count > 0 {
        optional_string(text.get_text(0, capped_count).await.ok())
    } else {
        None
    };
    let (selection_count, selection_error) = selection_read_plan(text_selection_count(&text).await);
    let mut selections = Vec::new();
    for index in 0..selection_count {
        if let Ok((start_offset, end_offset)) = text.get_selection(index).await {
            selections.push(AccessibilityTextSelection {
                start_offset,
                end_offset,
            });
        }
    }

    Some(AccessibilityText {
        character_count,
        caret_offset,
        content,
        truncated: character_count > MAX_TEXT_READBACK_CHARS,
        selections,
        selection_error,
    })
}

/// `org.a11y.atspi.Text.GetNSelections`.
///
/// Until atspi-proxies 0.14.0 this needed a hand-rolled call: the proxy method
/// was named `get_nselections`, and zbus derives the wire name by pascal-casing
/// the Rust one, so it was sent as `GetNselections` while the interface spells
/// it `GetNSelections`. Every call failed with `UnknownMethod` and the count
/// read as zero. Upstream renamed the method and pinned the name with
/// `#[zbus(name)]` in 0.14.0, so the generated call is correct now.
async fn text_selection_count(text: &TextProxy<'_>) -> zbus::Result<i32> {
    text.get_n_selections().await
}

/// How many selections to read back, and the error to report when the count
/// could not be read at all.
///
/// A count that failed to read is not the same answer as "nothing is
/// selected", so the error travels with the text instead of being swallowed
/// into an empty list.
fn selection_read_plan<E: std::fmt::Display>(
    count: std::result::Result<i32, E>,
) -> (i32, Option<String>) {
    match count {
        Ok(count) => (count.clamp(0, MAX_TEXT_SELECTIONS), None),
        Err(error) => (
            0,
            Some(format!(
                "failed to read the AT-SPI selection count: {error}"
            )),
        ),
    }
}

async fn supports_editable_text(proxies: Option<&atspi::proxy::proxy_ext::Proxies<'_>>) -> bool {
    let Some(proxies) = proxies else {
        return false;
    };
    proxies.editable_text().await.is_ok()
}

fn state_labels(state_set: StateSet) -> Vec<String> {
    state_set.iter().map(|state| state.to_string()).collect()
}

fn select_action_index(actions: &[atspi::Action], requested_action: Option<&str>) -> Result<i32> {
    if actions.is_empty() {
        return Err(anyhow!("element exposes no AT-SPI actions"));
    }

    if let Some(requested_action) = requested_action
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let requested_action = requested_action.to_ascii_lowercase();
        if let Some((index, _)) = actions.iter().enumerate().find(|(_, action)| {
            action.name.to_ascii_lowercase() == requested_action
                || action.description.to_ascii_lowercase() == requested_action
        }) {
            return Ok(index as i32);
        }

        if let Ok(index) = requested_action.parse::<usize>()
            && index < actions.len()
        {
            return Ok(index as i32);
        }

        return Err(anyhow!(
            "requested AT-SPI action was not found; available actions: {}",
            actions
                .iter()
                .map(|action| action.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(if actions.len() > 1 { 1 } else { 0 })
}

fn optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn object_ref_from_id(object_ref_id: &str) -> Result<ObjectRefOwned> {
    let (name, path) = split_object_ref_id(object_ref_id)?;
    let name = UniqueName::try_from(name.to_string())
        .with_context(|| format!("invalid AT-SPI bus name in object ref {object_ref_id}"))?;
    let path = ObjectPath::try_from(path.to_string())
        .with_context(|| format!("invalid AT-SPI object path in object ref {object_ref_id}"))?;
    Ok(ObjectRef::new_owned(name, path))
}

fn split_object_ref_id(object_ref_id: &str) -> Result<(&str, &str)> {
    let Some(path_start) = object_ref_id.find('/') else {
        return Err(anyhow!(
            "invalid AT-SPI object ref '{object_ref_id}'; expected ':bus/path'"
        ));
    };
    let (name, path) = object_ref_id.split_at(path_start);
    if name.is_empty() || path.is_empty() {
        return Err(anyhow!(
            "invalid AT-SPI object ref '{object_ref_id}'; expected ':bus/path'"
        ));
    }
    Ok((name, path))
}

fn object_ref_id(object_ref: &ObjectRefOwned) -> String {
    format!(
        "{}{}",
        object_ref.name_as_str().unwrap_or(""),
        object_ref.path_as_str()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_object_ref_id_separates_bus_name_and_path() {
        let (name, path) = split_object_ref_id(":1.42/org/a11y/atspi/accessible/7").unwrap();

        assert_eq!(name, ":1.42");
        assert_eq!(path, "/org/a11y/atspi/accessible/7");
    }

    #[test]
    fn a_selection_count_is_clamped_to_the_readback_limit() {
        assert_eq!(selection_read_plan(Ok::<_, String>(0)), (0, None));
        assert_eq!(selection_read_plan(Ok::<_, String>(1)), (1, None));
        assert_eq!(selection_read_plan(Ok::<_, String>(-3)), (0, None));
        assert_eq!(
            selection_read_plan(Ok::<_, String>(i32::MAX)),
            (MAX_TEXT_SELECTIONS, None)
        );
    }

    #[test]
    fn an_unreadable_selection_count_is_reported_rather_than_read_as_none() {
        let (count, error) = selection_read_plan(Err::<i32, _>("Unknown method 'GetNselections'"));

        assert_eq!(count, 0);
        assert_eq!(
            error.as_deref(),
            Some("failed to read the AT-SPI selection count: Unknown method 'GetNselections'")
        );
    }

    #[test]
    fn select_action_index_uses_named_action() {
        let actions = vec![
            atspi::Action {
                name: "click".to_string(),
                description: "Clicks".to_string(),
                keybinding: String::new(),
            },
            atspi::Action {
                name: "show-menu".to_string(),
                description: "Shows menu".to_string(),
                keybinding: String::new(),
            },
        ];

        assert_eq!(select_action_index(&actions, Some("show-menu")).unwrap(), 1);
    }

    #[test]
    fn select_action_index_defaults_to_secondary_when_available() {
        let actions = vec![
            atspi::Action {
                name: "click".to_string(),
                description: String::new(),
                keybinding: String::new(),
            },
            atspi::Action {
                name: "show-menu".to_string(),
                description: String::new(),
                keybinding: String::new(),
            },
        ];

        assert_eq!(select_action_index(&actions, None).unwrap(), 1);
    }

    #[test]
    fn state_labels_serialize_in_bit_order() {
        let labels = state_labels(StateSet::new(atspi::State::Focused | atspi::State::Checked));

        assert_eq!(labels, vec!["checked".to_string(), "focused".to_string()]);
    }

    #[test]
    fn default_snapshot_limits_cover_deep_gtk4_trees() {
        // Nautilus 50 places file-list cells below depth 20 and can expose
        // more than 850 raw nodes. Keep the defaults above that known shape.
        assert_eq!(snapshot_limits(None, None), (1_000, 32));
    }

    #[test]
    fn requested_snapshot_limits_remain_bounded() {
        assert_eq!(snapshot_limits(Some(0), Some(0)), (1, 0));
        assert_eq!(snapshot_limits(Some(10_000), Some(128)), (2_000, 64));
    }

    #[test]
    fn app_discovery_budget_is_independent_of_requested_tree_size() {
        assert_eq!(snapshot_child_read_budgets(1), (256, 2_048, 1));
    }

    #[test]
    fn traversal_attempts_and_queue_share_one_work_budget() {
        let mut traversal = BoundedTraversal::new(4);
        traversal.enqueue([1]);

        assert_eq!(traversal.pop(), Some(1));
        traversal.enqueue(2..=10_000);
        assert_eq!(traversal.queue, VecDeque::from([2, 3, 4]));

        assert_eq!(traversal.pop(), Some(2));
        traversal.enqueue(5..=10_000);
        assert_eq!(traversal.queue, VecDeque::from([3, 4]));
        assert_eq!(traversal.pop(), Some(3));
        assert_eq!(traversal.pop(), Some(4));
        assert_eq!(traversal.pop(), None);
        assert_eq!(traversal.attempted, 4);
    }

    #[test]
    fn child_count_is_clamped_before_indexed_reads() {
        assert_eq!(bounded_child_count(-1, 4), 0);
        assert_eq!(bounded_child_count(3, 4), 3);
        assert_eq!(bounded_child_count(i32::MAX, 4), 4);
    }

    #[tokio::test]
    async fn indexed_child_reads_consume_attempts_even_when_one_fails() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut remaining_attempts = 3;
        let batch = fetch_indexed_up_to(100, 10, &mut remaining_attempts, {
            let calls = std::sync::Arc::clone(&calls);
            move |index| {
                let calls = std::sync::Arc::clone(&calls);
                async move {
                    calls.lock().unwrap().push(index);
                    if index == 1 { Err(()) } else { Ok(index) }
                }
            }
        })
        .await;

        assert_eq!(batch.items, vec![0, 2]);
        assert_eq!(batch.attempted, 3);
        assert!(!batch.all_failed());
        assert_eq!(*calls.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(remaining_attempts, 0);

        let no_children = fetch_indexed_up_to(100, 10, &mut remaining_attempts, |_| async {
            Ok::<_, ()>(99)
        })
        .await;
        assert!(no_children.items.is_empty());
        assert_eq!(no_children.attempted, 0);
        assert!(!no_children.all_failed());

        let mut failed_attempts = 2;
        let all_failed =
            fetch_indexed_up_to(2, 2, &mut failed_attempts, |_| async { Err::<i32, _>(()) }).await;
        assert!(all_failed.all_failed());
    }

    #[test]
    fn stale_object_errors_are_recognized() {
        let fdo = anyhow::Error::from(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::ServiceUnknown("The name :1.99 was not provided".to_string()),
        )))
        .context("failed to open AT-SPI object :1.99/org/a11y/atspi/accessible/7");
        assert!(is_stale_object_error(&fdo));

        let unknown_object = anyhow::Error::from(zbus::Error::FDO(Box::new(
            zbus::fdo::Error::UnknownObject("gone".to_string()),
        )));
        assert!(is_stale_object_error(&unknown_object));

        let textual = anyhow::anyhow!(
            "org.freedesktop.DBus.Error.ServiceUnknown: The name is not activatable"
        );
        assert!(is_stale_object_error(&textual));

        let rejected = anyhow::anyhow!("AT-SPI EditableText rejected the new contents");
        assert!(!is_stale_object_error(&rejected));
        let no_interface = anyhow::Error::from(zbus::Error::InterfaceNotFound);
        assert!(!is_stale_object_error(&no_interface));
    }
}
