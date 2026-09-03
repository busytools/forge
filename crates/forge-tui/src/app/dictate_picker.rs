//! `/dictate` overlay: transient state + key handling.
//!
//! A centered overlay (rendered by [`crate::ui::dictate_picker`])
//! following the `/account` picker idiom: arrows move the highlight,
//! enter sets the highlighted row, esc closes. Enter never closes -
//! the dialog is a set of choices made in one visit, and reset
//! deliberately stays open so fine-tuning can continue. The rows are
//! derived from the session's live state on every read, so the markers
//! and the reset row's dimness can never drift from what the workspace
//! echoed.
//!
//! Two modes. `Options` is the normalizer axes plus the INPUT DEVICE
//! block's readout row; enter there opens `Devices`, the enumerated
//! input list, where esc steps back instead of closing. The device
//! pick is the `/spinner` shape: the `forge.toml` `[dictate] device`
//! pin is the default, a pick overrides it until the session ends, and
//! a restart reverts to the pin.

use crossterm::event::{KeyCode, KeyEvent};
use forge_workspace::{
    Context, DictateDeviceChoice, DictateOverrideUpdate, DictateOverrides, Structure, Styling,
};

use super::App;

/// State for the open `/dictate` overlay. `None` on `App` when closed.
#[derive(Debug, Clone)]
pub struct DictatePickerState {
    /// Which body the dialog draws.
    pub mode: PickerMode,
    /// Highlighted row index into the options-mode rows.
    pub highlight: usize,
    /// Highlighted row index into the device list.
    pub devices_highlight: usize,
}

/// Which body the overlay draws: the axes + device readout, or the
/// enumerated input list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerMode {
    Options,
    Devices,
}

/// One action a committed options row can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RowAction {
    Override(DictateOverrideUpdate),
    OpenDevices,
}

/// How a state tag renders: the source of a value. Dim for defaults,
/// the accent colour for a session pick, the error colour for a pin
/// whose device is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagTone {
    Dim,
    Accent,
    Error,
}

/// One selectable options row. `marker` is the `●` on the value in
/// force (the session override, else the default); `session_set` adds
/// the "· this session" suffix naming the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PickerRow {
    pub group: &'static str,
    pub label: String,
    pub action: RowAction,
    pub marker: bool,
    pub session_set: bool,
    /// Right-justified state tag; only the INPUT DEVICE row carries
    /// one today.
    pub tag: Option<(String, TagTone)>,
    /// False only for the inert reset row - nothing overridden and no
    /// device pick: DIM and unreachable, there is nothing to clear.
    pub selectable: bool,
}

/// One device row in pick mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeviceRow {
    pub label: String,
    pub tag: Option<(String, TagTone)>,
    /// The `●`: a take would record from this device right now.
    pub in_force: bool,
    /// What enter dispatches for this row; `None` when the row is not
    /// selectable (a pin whose device is absent).
    pub pick: Option<DictateDeviceChoice>,
    pub selectable: bool,
}

/// The pick-mode body: rows, the not-yet-arrived catalog, a failed
/// enumeration, or a machine with no inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeviceList {
    Rows(Vec<DeviceRow>),
    Listing,
    Failed(String),
    NoDevices,
}

/// The axis rows, in dialog order: VOICE, STRUCTURE, DESTINATION.
/// Each group's in-force row is marked - the override when there is
/// one, the crate default otherwise.
pub(crate) fn axis_rows(overrides: DictateOverrides) -> Vec<PickerRow> {
    let mut rows = Vec::new();
    let voice = [
        (Styling::Casual, "casual"),
        (Styling::SemiCasual, "semi-casual"),
        (Styling::SemiFormal, "semi-formal"),
        (Styling::Formal, "formal"),
    ];
    for (value, label) in voice {
        rows.push(PickerRow {
            group: "VOICE",
            label: label.to_owned(),
            action: RowAction::Override(DictateOverrideUpdate::Styling(value)),
            marker: overrides.styling.unwrap_or_default() == value,
            session_set: overrides.styling == Some(value),
            tag: None,
            selectable: true,
        });
    }
    let structure = [(Structure::Prose, "prose"), (Structure::Lists, "may bullet a list")];
    for (value, label) in structure {
        rows.push(PickerRow {
            group: "STRUCTURE",
            label: label.to_owned(),
            action: RowAction::Override(DictateOverrideUpdate::Structure(value)),
            marker: overrides.structure.unwrap_or_default() == value,
            session_set: overrides.structure == Some(value),
            tag: None,
            selectable: true,
        });
    }
    let context = [(Context::General, "plain text"), (Context::Email, "email layout")];
    for (value, label) in context {
        rows.push(PickerRow {
            group: "DESTINATION",
            label: label.to_owned(),
            action: RowAction::Override(DictateOverrideUpdate::Context(value)),
            marker: overrides.context.unwrap_or_default() == value,
            session_set: overrides.context == Some(value),
            tag: None,
            selectable: true,
        });
    }
    rows
}

/// The session's live overrides, or the default set when no session is
/// active (the overlay is opened per session, so this is defensive).
fn live_overrides(app: &App) -> DictateOverrides {
    app.active_session().map(|s| s.dictate_overrides).unwrap_or_default()
}

fn live_device_pin(app: &App) -> Option<DictateDeviceChoice> {
    app.active_session().and_then(|s| s.dictate_device_pin.clone())
}

/// The INPUT DEVICE readout row: what a take records from, and where
/// that value came from. Resolved from the session pick over the
/// configured pin; names come from the last catalog. A machine with
/// no inputs says so rather than naming a default that is not there.
fn device_readout(app: &App) -> (String, Option<(String, TagTone)>) {
    let pin = live_device_pin(app);
    let Some(catalog) = app.dictate_devices.as_ref() else {
        return ("Device: ...".to_owned(), Some(("listing".to_owned(), TagTone::Dim)));
    };
    let catalog = match catalog {
        Ok(catalog) => catalog,
        Err(error) => {
            return ("Device: ...".to_owned(), Some((error.clone(), TagTone::Error)));
        }
    };
    if catalog.devices.is_empty() || catalog.devices.iter().all(|d| !d.is_default) {
        return (
            "Device: no input device".to_owned(),
            Some(("no input devices found".to_owned(), TagTone::Error)),
        );
    }
    let name_of = |id: &str| catalog.devices.iter().find(|d| d.id == id).map(|d| d.name.clone());
    let default_name = || catalog.devices.iter().find(|d| d.is_default).map(|d| d.name.clone());
    let in_force =
        forge_workspace::resolve_capture_device(pin.as_ref(), catalog.configured.as_deref());
    // Where the value came from: a session pick until the session
    // ends, else the pin, else the system default.
    let source: (&str, TagTone) = match pin {
        Some(_) => ("active until restart", TagTone::Accent),
        None if catalog.configured.is_some() => ("configured default (forge.toml)", TagTone::Dim),
        None => ("system default", TagTone::Dim),
    };
    match in_force {
        None => (
            format!("Device: {}", default_name().unwrap_or_else(|| "system default".into())),
            Some((source.0.to_owned(), source.1)),
        ),
        Some(id) => match name_of(&id) {
            Some(name) => (format!("Device: {name}"), Some((source.0.to_owned(), source.1))),
            None => (format!("Device: {id}"), Some(("not present".to_owned(), TagTone::Error))),
        },
    }
}

/// The options-mode rows: the axis rows, then the INPUT DEVICE
/// readout row (enter opens pick mode), then the reset row.
pub(crate) fn rows(app: &App) -> Vec<PickerRow> {
    let overrides = live_overrides(app);
    let mut rows = axis_rows(overrides);
    let (label, tag) = device_readout(app);
    rows.push(PickerRow {
        group: "INPUT DEVICE",
        label,
        action: RowAction::OpenDevices,
        marker: false,
        session_set: false,
        tag,
        selectable: true,
    });
    rows.push(PickerRow {
        group: "",
        label: "Reset all to defaults".to_owned(),
        action: RowAction::Override(DictateOverrideUpdate::Reset),
        marker: false,
        session_set: false,
        tag: None,
        // A device-only pick leaves the override axes empty, but back
        // to defaults still has something to clear.
        selectable: !overrides.is_empty() || live_device_pin(app).is_some(),
    });
    rows
}

/// The pick-mode body. "System default" leads as the unpin row; the
/// enumerated devices follow by name; a pin whose device is absent
/// trails DIM and unreachable, so a stale pin can be seen and cleared
/// rather than only failing at record time.
pub(crate) fn device_list(app: &App) -> DeviceList {
    let Some(catalog) = app.dictate_devices.as_ref() else {
        return DeviceList::Listing;
    };
    let catalog = match catalog {
        Ok(catalog) => catalog,
        Err(error) => return DeviceList::Failed(error.clone()),
    };
    if catalog.devices.is_empty() {
        return DeviceList::NoDevices;
    }
    let pin = live_device_pin(app);
    let in_force =
        forge_workspace::resolve_capture_device(pin.as_ref(), catalog.configured.as_deref());
    let default = catalog.devices.iter().find(|d| d.is_default);
    let mut rows = vec![DeviceRow {
        label: "System default".to_owned(),
        tag: Some((
            match default {
                Some(d) => format!("no pin \u{b7} now: {}", d.name),
                None => "no pin".to_owned(),
            },
            TagTone::Dim,
        )),
        in_force: in_force.is_none(),
        pick: Some(DictateDeviceChoice::System),
        selectable: true,
    }];
    for device in &catalog.devices {
        rows.push(DeviceRow {
            label: device.name.clone(),
            tag: device.is_default.then(|| ("system default input".to_owned(), TagTone::Dim)),
            in_force: in_force.as_deref() == Some(device.id.as_str()),
            pick: Some(DictateDeviceChoice::Device(device.id.clone())),
            selectable: true,
        });
    }
    if let Some(gone) =
        in_force.as_deref().filter(|id| !catalog.devices.iter().any(|d| d.id == *id))
    {
        // Either source can go absent between walks: a configured
        // pin, or a session pick whose device was unplugged since the
        // last enumeration. The tag names forge.toml only when the
        // pin field is the one that is stale.
        let tag =
            if pin.is_none() { "not present \u{b7} pinned in forge.toml" } else { "not present" };
        rows.push(DeviceRow {
            label: gone.to_owned(),
            tag: Some((tag.to_owned(), TagTone::Error)),
            in_force: false,
            pick: None,
            selectable: false,
        });
    }
    DeviceList::Rows(rows)
}

pub(crate) fn open(app: &mut App) {
    app.dictate_picker =
        Some(DictatePickerState { mode: PickerMode::Options, highlight: 0, devices_highlight: 0 });
    super::dictate_devices::mark_stale(app);
    app.needs_redraw = true;
}

pub(crate) fn close(app: &mut App) {
    app.dictate_picker = None;
    app.needs_redraw = true;
}

/// Handle a key while the overlay is open. Always consumes the key
/// (returns `true`; the overlay is modal). Up/Down move the highlight
/// over the selectable rows (inert rows are skipped); enter sets the
/// highlighted row (enter on the INPUT DEVICE row opens pick mode);
/// esc steps back out of pick mode and closes from the options body.
pub(crate) fn handle_key(app: &mut App, key: KeyEvent) -> bool {
    if app.dictate_picker.is_none() {
        return false;
    }
    match key.code {
        KeyCode::Up => move_highlight(app, false),
        KeyCode::Down => move_highlight(app, true),
        KeyCode::Enter => commit(app),
        KeyCode::Esc => step_back(app),
        _ => {}
    }
    true
}

fn move_highlight(app: &mut App, forward: bool) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let (mode, options_at, devices_at) = (state.mode, state.highlight, state.devices_highlight);
    match mode {
        PickerMode::Options => {
            let rows = rows(app);
            let next = neighbour(&rows, options_at, forward, |row| row.selectable);
            if let Some(state) = app.dictate_picker.as_mut() {
                state.highlight = next;
            }
        }
        PickerMode::Devices => {
            // Nothing to move over until the catalog lands.
            let next = match device_list(app) {
                DeviceList::Rows(list) => {
                    neighbour(&list, devices_at, forward, |row| row.selectable)
                }
                _ => devices_at,
            };
            if let Some(state) = app.dictate_picker.as_mut() {
                state.devices_highlight = next;
            }
        }
    }
    app.needs_redraw = true;
}

fn neighbour<T>(rows: &[T], from: usize, forward: bool, selectable: impl Fn(&T) -> bool) -> usize {
    if forward {
        for (idx, row) in rows.iter().enumerate().skip(from + 1) {
            if selectable(row) {
                return idx;
            }
        }
    } else {
        for idx in (0..from).rev() {
            if selectable(&rows[idx]) {
                return idx;
            }
        }
    }
    from
}

/// Esc: back from pick mode to the options body; close from there.
fn step_back(app: &mut App) {
    if let Some(state) = app.dictate_picker.as_mut()
        && matches!(state.mode, PickerMode::Devices)
    {
        state.mode = PickerMode::Options;
        app.needs_redraw = true;
        return;
    }
    close(app);
}

fn commit(app: &mut App) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    match state.mode {
        PickerMode::Options => commit_options(app),
        PickerMode::Devices => commit_devices(app),
    }
}

/// Apply the highlighted options row and keep the dialog open: the
/// dialog is a set of choices made in one visit. The markers update
/// when the workspace echo lands. A reset also restarts the highlight
/// at the first row, because fine-tuning begins again from the top.
fn commit_options(app: &mut App) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let rows = rows(app);
    let Some(row) = rows.get(state.highlight) else {
        return;
    };
    if !row.selectable {
        return;
    }
    match row.action {
        RowAction::OpenDevices => {
            if let Some(state) = app.dictate_picker.as_mut() {
                state.mode = PickerMode::Devices;
                state.devices_highlight = 0;
            }
            app.needs_redraw = true;
        }
        RowAction::Override(update) => {
            dispatch_override(app, update);
            if update == DictateOverrideUpdate::Reset
                && let Some(state) = app.dictate_picker.as_mut()
            {
                state.highlight = 0;
            }
            app.needs_redraw = true;
        }
    }
}

/// Pin the highlighted device and stay in pick mode; the `●` moves
/// when the workspace echo lands.
fn commit_devices(app: &mut App) {
    let Some(state) = app.dictate_picker.as_ref() else {
        return;
    };
    let DeviceList::Rows(list) = device_list(app) else {
        return;
    };
    let Some(row) = list.get(state.devices_highlight) else {
        return;
    };
    if !row.selectable {
        return;
    }
    let Some(pick) = row.pick.clone() else {
        return;
    };
    let result = app.dispatch_command(|key| forge_workspace::Command::SetDictateDevice {
        key,
        pick: Some(pick),
    });
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "dictate_device_dispatch_failed",
            message = "could not apply the highlighted /dictate device pick",
            outcome = "failure",
            error_message = %error,
        );
    }
    app.needs_redraw = true;
}

fn dispatch_override(app: &mut App, update: DictateOverrideUpdate) {
    let result =
        app.dispatch_command(|key| forge_workspace::Command::SetDictateOverride { key, update });
    if let Err(error) = result {
        tracing::warn!(
            target: crate::logging::targets::APP_SESSION,
            event_name = "dictate_override_dispatch_failed",
            message = "could not apply the highlighted /dictate row",
            outcome = "failure",
            error_message = %error,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use forge_workspace::DictateDeviceCatalog;

    fn overridden() -> DictateOverrides {
        DictateOverrides {
            structure: Some(Structure::Lists),
            context: Some(Context::Email),
            ..Default::default()
        }
    }

    /// A catalog whose two devices are distinct: one is the system
    /// default, the other a pinned interface.
    fn catalog() -> DictateDeviceCatalog {
        DictateDeviceCatalog {
            devices: vec![
                forge_workspace::Device {
                    id: "mbp-mic".into(),
                    name: "MacBook Pro Microphone".into(),
                    is_default: true,
                },
                forge_workspace::Device {
                    id: "shure-id".into(),
                    name: "Shure SM7B".into(),
                    is_default: false,
                },
            ],
            configured: Some("shure-id".into()),
        }
    }

    fn catalog_app(configured: Option<String>) -> App {
        let mut app = App::test_default();
        app.dictate_devices = Some(Ok(DictateDeviceCatalog { configured, ..catalog() }));
        app
    }

    #[test]
    fn rows_carry_the_crates_vocabulary_under_the_dialogs_groups() {
        let rows = axis_rows(DictateOverrides::default());

        let voice: Vec<&str> =
            rows.iter().filter(|r| r.group == "VOICE").map(|r| r.label.as_str()).collect();
        assert_eq!(voice, vec!["casual", "semi-casual", "semi-formal", "formal"]);

        let structure: Vec<&str> =
            rows.iter().filter(|r| r.group == "STRUCTURE").map(|r| r.label.as_str()).collect();
        assert_eq!(structure, vec!["prose", "may bullet a list"]);

        let destination: Vec<&str> =
            rows.iter().filter(|r| r.group == "DESTINATION").map(|r| r.label.as_str()).collect();
        assert_eq!(destination, vec!["plain text", "email layout"]);

        assert_eq!(rows.len(), 8, "two axes rows per group over three groups");
    }

    #[test]
    fn the_marker_marks_the_value_in_force_and_the_defaults_stand_fresh() {
        let rows = axis_rows(DictateOverrides::default());
        let marked: Vec<&str> =
            rows.iter().filter(|r| r.marker).map(|r| r.label.as_str()).collect();
        assert_eq!(
            marked,
            vec!["semi-formal", "prose", "plain text"],
            "a fresh session shows each group's default as in force"
        );
        assert!(rows.iter().all(|r| !r.session_set), "nothing is session-set before a pick");
    }

    #[test]
    fn a_session_pick_moves_the_marker_and_names_its_source() {
        let rows = axis_rows(overridden());
        let marked: Vec<&str> =
            rows.iter().filter(|r| r.marker).map(|r| r.label.as_str()).collect();
        assert_eq!(
            marked,
            vec!["semi-formal", "may bullet a list", "email layout"],
            "the picked rows take the marker; the untouched group keeps its default"
        );
        let session_set: Vec<&str> =
            rows.iter().filter(|r| r.session_set).map(|r| r.label.as_str()).collect();
        assert_eq!(session_set, vec!["may bullet a list", "email layout"]);

        let casual = rows.iter().find(|r| r.label == "semi-formal").expect("voice row");
        assert!(
            casual.marker && !casual.session_set,
            "an untouched group keeps its default marker with no suffix"
        );
        let prose = rows.iter().find(|r| r.label == "prose").expect("prose row");
        assert!(
            !prose.marker && !prose.session_set,
            "a row the pick displaced loses the marker entirely"
        );
    }

    #[test]
    fn reset_row_is_selectable_only_when_something_is_set() {
        let mut app = App::test_default();
        let bare = rows(&app);
        let reset = bare.last().expect("reset row is always drawn");
        assert_eq!(reset.label, "Reset all to defaults");
        assert!(!reset.selectable, "nothing to clear");

        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_overrides = overridden();
        let set = rows(&app);
        assert!(set.last().expect("reset row").selectable);
    }

    /// A device-only pick leaves the override axes empty; the reset
    /// row is still the way back, so it must stay reachable.
    #[test]
    fn a_device_only_pick_keeps_the_reset_row_reachable() {
        let mut app = App::test_default();
        app.dictate_devices = Some(Ok(catalog()));
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_device_pin =
            Some(DictateDeviceChoice::Device("shure-id".into()));

        let reset = rows(&app).last().expect("reset row").clone();
        assert!(reset.selectable, "a pick the reset must clear exists, whatever the axes say");

        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        let key_codes = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..9 {
            handle_key(&mut app, key_codes(KeyCode::Down));
        }
        handle_key(&mut app, key_codes(KeyCode::Enter));

        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::SetDictateOverride { update, .. }) => {
                assert_eq!(*update, DictateOverrideUpdate::Reset);
            }
            other => panic!("a reset dispatch, got {other:?}"),
        }
    }

    #[test]
    fn the_device_row_reads_the_pin_over_the_configured_default() {
        let app = catalog_app(Some("shure-id".into()));
        let row = rows(&app).into_iter().find(|r| r.group == "INPUT DEVICE").expect("device row");
        assert_eq!(row.label, "Device: Shure SM7B");
        assert_eq!(
            row.tag,
            Some(("configured default (forge.toml)".into(), TagTone::Dim)),
            "the configured pin is the default state, so the tag names it"
        );

        let mut app = catalog_app(Some("shure-id".into()));
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_device_pin =
            Some(DictateDeviceChoice::Device("mbp-mic".into()));
        let row = rows(&app).into_iter().find(|r| r.group == "INPUT DEVICE").expect("device row");
        assert_eq!(row.label, "Device: MacBook Pro Microphone");
        assert_eq!(row.tag, Some(("active until restart".into(), TagTone::Accent)));
    }

    #[test]
    fn an_empty_or_defaultless_catalog_says_no_input_device() {
        let mut app = App::test_default();
        app.dictate_devices = Some(Ok(DictateDeviceCatalog { devices: vec![], configured: None }));
        let (label, tag) = device_readout(&app);
        assert_eq!(
            label, "Device: no input device",
            "an empty catalog must not dress up a default that does not exist"
        );
        assert_eq!(tag, Some(("no input devices found".into(), TagTone::Error)));

        // Same for a catalog whose entries carry no default flag: the
        // system default is not a functioning device here.
        let mut app = App::test_default();
        app.dictate_devices = Some(Ok(DictateDeviceCatalog {
            devices: vec![forge_workspace::Device {
                id: "d".into(),
                name: "Some Mic".into(),
                is_default: false,
            }],
            configured: None,
        }));
        let (label, tag) = device_readout(&app);
        assert_eq!(label, "Device: no input device");
        assert_eq!(tag.map(|(_, tone)| tone), Some(TagTone::Error));
    }

    #[test]
    fn a_pin_whose_device_is_absent_reads_the_raw_id() {
        let mut app = catalog_app(Some("shure-id".into()));
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_device_pin =
            Some(DictateDeviceChoice::Device("unplugged-id".into()));
        let row = rows(&app).into_iter().find(|r| r.group == "INPUT DEVICE").expect("device row");
        assert_eq!(row.label, "Device: unplugged-id");
        assert_eq!(row.tag, Some(("not present".into(), TagTone::Error)));
    }

    #[test]
    fn the_device_list_leads_with_system_default_and_marks_the_device_in_force() {
        let app = catalog_app(Some("shure-id".into()));
        let list = device_list(&app);
        let DeviceList::Rows(rows) = list else {
            panic!("a loaded catalog yields rows");
        };
        assert_eq!(rows[0].label, "System default", "the unpin row leads");
        assert!(!rows[0].in_force, "the configured pin is in force, not the system default");
        let shure = rows.iter().find(|r| r.label == "Shure SM7B").expect("shure row");
        assert!(shure.in_force, "the configured pin is the device in force");
        assert_eq!(shure.tag, None, "a non-default enumerated device carries no tag");
        let mic = rows.iter().find(|r| r.label == "MacBook Pro Microphone").expect("mic row");
        assert_eq!(mic.tag, Some(("system default input".into(), TagTone::Dim)));
    }

    #[test]
    fn a_session_pick_moves_the_in_force_marker_off_the_pin() {
        let mut app = catalog_app(Some("shure-id".into()));
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_device_pin =
            Some(DictateDeviceChoice::System);
        let DeviceList::Rows(rows) = device_list(&app) else {
            panic!("a loaded catalog yields rows");
        };
        assert!(rows[0].in_force, "the system-default pick is in force");
        assert!(rows.iter().filter(|r| r.in_force).count() == 1, "exactly one device is in force");
    }

    #[test]
    fn a_pin_whose_device_is_gone_trails_dim_and_unreachable() {
        let mut app = App::test_default();
        app.dictate_devices =
            Some(Ok(DictateDeviceCatalog { configured: Some("unplugged-id".into()), ..catalog() }));
        let DeviceList::Rows(rows) = device_list(&app) else {
            panic!("a loaded catalog yields rows");
        };
        let gone = rows.last().expect("the stale pin trails");
        assert_eq!(gone.label, "unplugged-id");
        assert_eq!(
            gone.tag,
            Some(("not present \u{b7} pinned in forge.toml".into(), TagTone::Error))
        );
        assert!(!gone.selectable, "pinning an absent device is not an offer");
        assert!(!gone.in_force, "nothing is in force while the pin names a gone device");
        assert!(
            rows.iter().filter(|r| r.in_force).count() == 0,
            "no row claims the microphone while the pin is stale"
        );
    }

    #[test]
    fn submit_opens_the_overlay_and_args_are_refused() {
        let mut app = App::test_default();
        assert!(crate::app::slash::try_handle_submit(&mut app, "/dictate"));
        assert!(app.dictate_picker.is_some(), "no-arg opens the overlay");

        let mut app = App::test_default();
        assert!(crate::app::slash::try_handle_submit(&mut app, "/dictate extra"));
        assert!(app.dictate_picker.is_none(), "arguments are not part of the command");
        let last = app.messages().last().expect("a usage notice");
        assert!(matches!(last.role, crate::app::MessageRole::System(_)));
    }

    #[test]
    fn enter_sets_the_highlighted_axis_and_stays_open() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        assert!(app.dictate_picker.is_some());

        // Highlight `semi-formal` (third row) and commit.
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Enter));

        assert!(app.dictate_picker.is_some(), "enter does not close the dialog");
        let dispatched = workspace.drain_test_dispatch_buffer();
        assert_eq!(dispatched.len(), 1, "one set per enter: {dispatched:?}");
        match &dispatched[0] {
            forge_workspace::Command::SetDictateOverride { key, update } => {
                assert_eq!(key, &app.active_session_key.clone().expect("active session"));
                assert_eq!(*update, DictateOverrideUpdate::Styling(Styling::SemiFormal),);
            }
            other => panic!("a set dispatch, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_the_device_row_opens_pick_mode_without_dispatching() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // Down onto the INPUT DEVICE row (index 8: eight axis rows).
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));

        let state = app.dictate_picker.as_ref().expect("open");
        assert_eq!(state.mode, PickerMode::Devices, "enter opens pick mode");
        assert!(
            workspace.drain_test_dispatch_buffer().is_empty(),
            "opening the list dispatches nothing"
        );
    }

    #[test]
    fn enter_on_a_device_row_pins_it_and_pick_mode_stays_open() {
        let mut app = catalog_app(Some("shure-id".into()));
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        // Down onto the Shure row (index 2: System default, mic, Shure).
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Down));
        handle_key(&mut app, key(KeyCode::Enter));

        let state = app.dictate_picker.as_ref().expect("open");
        assert_eq!(state.mode, PickerMode::Devices, "a pick does not close the dialog");
        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::SetDictateDevice { key, pick }) => {
                assert_eq!(key, &app.active_session_key.clone().expect("active session"));
                assert_eq!(*pick, Some(DictateDeviceChoice::Device("shure-id".into())),);
            }
            other => panic!("a device-pin dispatch, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_the_system_default_row_pins_the_system_choice() {
        let mut app = catalog_app(Some("shure-id".into()));
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        handle_key(&mut app, key(KeyCode::Enter));

        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::SetDictateDevice { pick, .. }) => {
                assert_eq!(*pick, Some(DictateDeviceChoice::System));
            }
            other => panic!("a device-pin dispatch, got {other:?}"),
        }
    }

    #[test]
    fn arrows_move_over_selectable_rows_only() {
        let mut app = App::test_default();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // Down to the end: the inert reset row is skipped, so the
        // highlight stops on the INPUT DEVICE row (index 8).
        for _ in 0..20 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        let state = app.dictate_picker.as_ref().expect("open");
        assert_eq!(state.highlight, 8, "the dim reset row is unreachable");

        // Up never leaves the top.
        for _ in 0..20 {
            handle_key(&mut app, key(KeyCode::Up));
        }
        assert_eq!(app.dictate_picker.as_ref().expect("open").highlight, 0);
    }

    #[test]
    fn arrows_in_pick_mode_skip_the_stale_pin_row() {
        let mut app = App::test_default();
        app.dictate_devices =
            Some(Ok(DictateDeviceCatalog { configured: Some("unplugged-id".into()), ..catalog() }));
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        for _ in 0..20 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        let state = app.dictate_picker.as_ref().expect("open");
        assert_eq!(
            state.devices_highlight, 2,
            "the highlight stops on the last selectable row: the stale pin is unreachable"
        );
    }

    #[test]
    fn reset_commits_a_full_clear_then_restarts_the_highlight() {
        let mut app = App::test_default();
        let workspace = app.workspace.clone().expect("test workspace");
        workspace.enable_test_dispatch_intercept();
        let key = app.active_session_key.clone().expect("active session");
        app.sessions.get_mut(&key).expect("bucket").dictate_overrides = overridden();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");

        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        // Down onto the now-selectable reset row (index 9).
        for _ in 0..9 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));

        assert!(app.dictate_picker.is_some(), "reset does not close the dialog");
        let dispatched = workspace.drain_test_dispatch_buffer();
        match dispatched.last() {
            Some(forge_workspace::Command::SetDictateOverride { key, update }) => {
                assert_eq!(key, &app.active_session_key.clone().expect("active session"));
                assert_eq!(*update, DictateOverrideUpdate::Reset);
            }
            other => panic!("a reset dispatch, got {other:?}"),
        }
        assert_eq!(
            app.dictate_picker.as_ref().expect("open").highlight,
            0,
            "fine-tuning starts over from the first row"
        );
    }

    #[test]
    fn esc_steps_back_from_pick_mode_then_closes() {
        let mut app = App::test_default();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        let key = |code| KeyEvent::new(code, crossterm::event::KeyModifiers::NONE);
        for _ in 0..8 {
            handle_key(&mut app, key(KeyCode::Down));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        assert_eq!(app.dictate_picker.as_ref().expect("open").mode, PickerMode::Devices,);

        handle_key(&mut app, key(KeyCode::Esc));
        assert_eq!(
            app.dictate_picker.as_ref().expect("open").mode,
            PickerMode::Options,
            "esc steps back to the options body"
        );

        handle_key(&mut app, key(KeyCode::Esc));
        assert!(app.dictate_picker.is_none(), "esc again closes the overlay");
    }

    #[test]
    fn esc_closes_from_the_options_body() {
        let mut app = App::test_default();
        crate::app::slash::try_handle_submit(&mut app, "/dictate");
        assert!(handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE)
        ));
        assert!(app.dictate_picker.is_none());
    }
}
