//! Desktop notifications (`org.freedesktop.Notifications`). [`Watcher`]
//! decides what changed between two daemon snapshots (pure, tested); texts
//! are translated on the UI thread (Slint `Fmt`), then [`Notifier`] sends
//! them and reports the buttons clicked.

use std::collections::HashMap;

use odrs_ipc::{AUTH_ERROR_PREFIX, OFFLINE_ERROR_PREFIX, Snapshot};
use zbus::zvariant::Value;

/// Conflicts of one drive in one batch shown one by one, beyond that as a count.
const MAX_SINGLE_CONFLICTS: usize = 3;

/// Something worth a notification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    LoginExpired,
    DriveError { drive: String, message: String },
    Conflict { drive: String, path: String, backup: String },
    Conflicts { drive: String, count: usize },
    InitialSync { drive: String, sync_dir: String },
    VerifyDone { drive: String, problems: u64 },
}

/// Remembers what was already notified. The first snapshot is a baseline:
/// nothing that happened before the UI started is notified.
#[derive(Default)]
pub struct Watcher {
    started: bool,
    last_event: u64,
    needs_login: bool,
    /// Error notified per drive (digits blanked: counts change every pass).
    errors: HashMap<String, String>,
    verify_states: HashMap<String, String>,
}

impl Watcher {
    pub fn changes(&mut self, snapshot: Option<&Snapshot>) -> Vec<Change> {
        // Daemon stopped: keep the state, it may come back.
        let Some(s) = snapshot else { return Vec::new() };
        let mut out = Vec::new();
        let baseline = !self.started;
        self.started = true;

        let needs_login =
            s.last_error.starts_with(AUTH_ERROR_PREFIX) || s.drives.iter().any(|d| d.last_error.starts_with(AUTH_ERROR_PREFIX));
        if needs_login && !self.needs_login && !baseline {
            out.push(Change::LoginExpired);
        }
        self.needs_login = needs_login;

        for d in &s.drives {
            let notable = d.state == "error"
                && !d.last_error.is_empty()
                && !d.last_error.starts_with(AUTH_ERROR_PREFIX)
                && !d.last_error.starts_with(OFFLINE_ERROR_PREFIX);
            if !notable {
                if d.state == "idle" {
                    self.errors.remove(&d.name);
                }
                continue;
            }
            let key: String = d.last_error.chars().map(|c| if c.is_ascii_digit() { '#' } else { c }).collect();
            if self.errors.get(&d.name) != Some(&key) {
                if !baseline {
                    out.push(Change::DriveError { drive: d.name.clone(), message: d.last_error.clone() });
                }
                self.errors.insert(d.name.clone(), key);
            }
        }

        let new_events: Vec<_> = s.events.iter().filter(|e| e.id > self.last_event).collect();
        self.last_event = s.events.iter().map(|e| e.id).max().unwrap_or(0).max(self.last_event);
        if !baseline {
            let mut conflicts: Vec<(&str, Vec<&odrs_ipc::Event>)> = Vec::new();
            for e in new_events {
                match e.kind.as_str() {
                    "conflict" => match conflicts.iter_mut().find(|(d, _)| *d == e.drive) {
                        Some((_, list)) => list.push(e),
                        None => conflicts.push((&e.drive, vec![e])),
                    },
                    "initial-sync" => {
                        let sync_dir = s.drives.iter().find(|d| d.name == e.drive).map(|d| d.sync_dir.clone()).unwrap_or_default();
                        out.push(Change::InitialSync { drive: e.drive.clone(), sync_dir });
                    }
                    _ => {}
                }
            }
            for (drive, list) in conflicts {
                if list.len() > MAX_SINGLE_CONFLICTS {
                    out.push(Change::Conflicts { drive: drive.to_owned(), count: list.len() });
                } else {
                    out.extend(list.into_iter().map(|e| Change::Conflict {
                        drive: drive.to_owned(),
                        path: e.path.clone(),
                        backup: e.detail.clone(),
                    }));
                }
            }
        }

        for v in &s.verifications {
            let before = self.verify_states.insert(v.name.clone(), v.state.clone());
            let was_running = matches!(before.as_deref(), Some("running" | "queued"));
            if was_running && v.state == "done" {
                out.push(Change::VerifyDone { drive: v.name.clone(), problems: v.problems() });
            }
        }
        out
    }
}

/// What a notification's button (or a click on it) does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    ShowWindow,
    Reconnect,
    ShowReport(String),
    ShowItem(String),
    OpenFolder(String),
}

/// A translated notification, ready to send.
#[derive(Debug, Clone)]
pub struct Note {
    pub title: String,
    pub body: String,
    /// Extra button: label and what it does.
    pub button: Option<(String, Action)>,
    pub urgent: bool,
}

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
pub(crate) trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;
}

/// Sends notifications and remembers what their buttons do.
pub struct Notifier {
    proxy: NotificationsProxy<'static>,
    actions: HashMap<u32, (Option<Action>, Action)>,
}

impl Notifier {
    pub async fn new() -> zbus::Result<Self> {
        let conn = zbus::Connection::session().await?;
        Ok(Self { proxy: NotificationsProxy::new(&conn).await?, actions: HashMap::new() })
    }

    /// Stream of `(notification id, action key)` for clicked notifications.
    pub async fn clicks(&self) -> zbus::Result<ActionInvokedStream> {
        self.proxy.receive_action_invoked().await
    }

    pub async fn send(&mut self, note: Note) {
        let mut actions = vec!["default", ""];
        if let Some((label, _)) = &note.button {
            actions.extend(["button", label.as_str()]);
        }
        let mut hints = HashMap::new();
        hints.insert("desktop-entry", Value::from(crate::APP_ID));
        hints.insert("urgency", Value::from(if note.urgent { 2u8 } else { 1u8 }));
        let body = escape(&note.body);
        match self.proxy.notify("OneDrive", 0, crate::APP_ID, &note.title, &body, &actions, hints, -1).await {
            Ok(id) => {
                self.actions.insert(id, (note.button.map(|(_, a)| a), Action::ShowWindow));
            }
            Err(e) => tracing::warn!(error = %e, "cannot show a notification"),
        }
    }

    /// The action behind a click, if it is one of ours.
    pub fn action(&mut self, id: u32, key: &str) -> Option<Action> {
        let (button, default) = self.actions.remove(&id)?;
        if key == "button" { button } else { Some(default) }
    }
}

/// Bodies may contain markup (KDE, GNOME): file names must not.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use odrs_ipc::{DriveStatus, Event, VerifyStatus};

    fn drive(name: &str, state: &str, error: &str) -> DriveStatus {
        DriveStatus { name: name.into(), state: state.into(), last_error: error.into(), ..Default::default() }
    }

    fn event(id: u64, kind: &str, drive: &str, path: &str) -> Event {
        Event { id, kind: kind.into(), drive: drive.into(), path: path.into(), detail: format!("{path}-pc-safeBackup-0001"), ..Default::default() }
    }

    #[test]
    fn first_snapshot_is_a_baseline() {
        let mut w = Watcher::default();
        let s = Snapshot {
            last_error: format!("{AUTH_ERROR_PREFIX} expired"),
            drives: vec![drive("main", "error", "boom")],
            events: vec![event(5, "conflict", "main", "/a")],
            ..Default::default()
        };
        assert!(w.changes(Some(&s)).is_empty());
        assert!(w.changes(Some(&s)).is_empty());
    }

    #[test]
    fn login_errors_and_events() {
        let mut w = Watcher::default();
        let mut s = Snapshot { drives: vec![drive("main", "idle", "")], ..Default::default() };
        w.changes(Some(&s));

        s.last_error = format!("{AUTH_ERROR_PREFIX} expired");
        assert_eq!(w.changes(Some(&s)), [Change::LoginExpired]);
        assert!(w.changes(Some(&s)).is_empty());

        // An item count changing every pass is the same error.
        s.drives = vec![drive("main", "error", "3 item(s) failed")];
        assert_eq!(w.changes(Some(&s)), [Change::DriveError { drive: "main".into(), message: "3 item(s) failed".into() }]);
        s.drives = vec![drive("main", "error", "4 item(s) failed")];
        assert!(w.changes(Some(&s)).is_empty());
        // Offline: not notified.
        s.drives = vec![drive("main", "error", &format!("{OFFLINE_ERROR_PREFIX} no route"))];
        assert!(w.changes(Some(&s)).is_empty());

        s.events = vec![event(10, "conflict", "main", "/a"), event(11, "initial-sync", "Sales", "")];
        let c = w.changes(Some(&s));
        assert_eq!(c.len(), 2);
        assert!(matches!(&c[0], Change::InitialSync { drive, .. } if drive == "Sales"));
        assert!(matches!(&c[1], Change::Conflict { path, .. } if path == "/a"));
        assert!(w.changes(Some(&s)).is_empty());

        s.events = (20..25).map(|i| event(i, "conflict", "main", &format!("/f{i}"))).collect();
        assert_eq!(w.changes(Some(&s)), [Change::Conflicts { drive: "main".into(), count: 5 }]);
    }

    #[test]
    fn verification_end() {
        let mut w = Watcher::default();
        let v = |state: &str, missing| VerifyStatus { name: "main".into(), state: state.into(), missing, ..Default::default() };
        let mut s = Snapshot { verifications: vec![v("done", 0)], ..Default::default() };
        w.changes(Some(&s));
        s.verifications = vec![v("running", 0)];
        assert!(w.changes(Some(&s)).is_empty());
        s.verifications = vec![v("done", 2)];
        assert_eq!(w.changes(Some(&s)), [Change::VerifyDone { drive: "main".into(), problems: 2 }]);
        assert!(w.changes(Some(&s)).is_empty());
    }
}
