//! `/dictate` device enumeration, off the render thread.
//!
//! The overlay's device block and pick mode render from a
//! [`DictateDeviceCatalog`]: the inputs a pick can offer plus the
//! configured pin. Enumerating walks cpal and blocks, so the request
//! runs in a blocking task and the result lands on this module's
//! channel, drained once per main-loop iteration like the other
//! per-feature channels. The last catalog is cached on `App` and kept
//! visible while a fresh walk runs, so re-opening the overlay neither
//! flashes the list empty nor stacks walks. The spawn happens in the
//! main-loop [`tick`], never in `open` itself: unit tests drive the
//! overlay without a runtime and inject catalogs directly.

use std::sync::mpsc as std_mpsc;

use forge_workspace::DictateDeviceCatalog;

use super::App;

/// One enumeration result per main-loop drain budget.
const EVENT_DRAIN_BUDGET: usize = 8;

/// The workspace's answer to one catalog request.
#[derive(Debug)]
pub enum DictateDevicesEvent {
    CatalogReady { result: Result<DictateDeviceCatalog, String> },
}

/// Mark the cache stale: the next main-loop tick re-enumerates while
/// the overlay is open.
pub fn mark_stale(app: &mut App) {
    app.dictate_devices_dirty = true;
}

/// Start a fresh walk if the overlay is open and the cache is stale.
/// No-op outside the runtime (unit tests), which is fine: tests
/// inject catalogs on `App` directly.
pub fn tick(app: &mut App) {
    if app.dictate_picker.is_none() || !app.dictate_devices_dirty {
        return;
    }
    if app.dictate_devices_in_flight {
        return;
    }
    let Some(workspace) = app.workspace.clone() else {
        return;
    };
    app.dictate_devices_dirty = false;
    app.dictate_devices_in_flight = true;
    let tx = app.dictate_devices_tx.clone();
    tokio::task::spawn_local(async move {
        let result = tokio::task::spawn_blocking(move || workspace.dictate_device_catalog())
            .await
            .unwrap_or_else(|error| Err(format!("device enumeration failed to join: {error}")));
        let _ = tx.send(DictateDevicesEvent::CatalogReady { result });
    });
}

/// Apply every catalog that arrived since the last frame.
pub fn drain_events(app: &mut App) {
    for _ in 0..EVENT_DRAIN_BUDGET {
        let event = match app.dictate_devices_rx.try_recv() {
            Ok(event) => event,
            Err(std_mpsc::TryRecvError::Empty | std_mpsc::TryRecvError::Disconnected) => return,
        };
        match event {
            DictateDevicesEvent::CatalogReady { result } => {
                app.dictate_devices_in_flight = false;
                app.dictate_devices = Some(result);
                app.needs_redraw = true;
            }
        }
    }
}
