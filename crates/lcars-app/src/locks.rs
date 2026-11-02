//! Smart lock status and lock/unlock for the Security page.
//!
//! Catalog and commands use `lcars.locks.list`, `lcars.locks.lock`, and
//! `lcars.locks.unlock`. Names and ids are not logged.

use lcars_api_proto::prost::Message;
use lcars_api_proto::v1::{
    ListLocksRequest, ListLocksResponse, LockDoorRequest, UnlockDoorRequest,
};
use lcars_api_proto::{ERROR_CODE, LOCKS_LIST, LOCKS_LOCK, LOCKS_UNLOCK};
use lcars_ui::slint::Weak;
use lcars_ui::AppWindow;
use tokio::sync::{mpsc, watch};

pub enum Command {
    SetLocked(bool),
}

/// Wire the Security page lock control.
pub fn attach(app: &AppWindow, commands: mpsc::UnboundedSender<Command>) {
    app.on_set_lock(move |locked| {
        let _ = commands.send(Command::SetLocked(locked));
    });
}

pub async fn run(
    clients: watch::Receiver<Option<async_nats::Client>>,
    commands: mpsc::UnboundedReceiver<Command>,
    weak: Weak<AppWindow>,
) {
    let mut session = Session {
        clients,
        commands,
        weak,
        lock: None,
        in_flight: false,
    };
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        let wake = {
            let Session {
                commands, clients, ..
            } = &mut session;
            tokio::select! {
                biased;
                command = commands.recv() => Wake::Command(command),
                changed = clients.changed() => Wake::Client(changed),
                _ = interval.tick() => Wake::Tick,
            }
        };
        match wake {
            Wake::Command(None) => return,
            Wake::Command(Some(command)) => session.handle(command).await,
            Wake::Client(Err(_)) => return,
            Wake::Client(Ok(())) => session.refresh().await,
            Wake::Tick => session.refresh().await,
        }
    }
}

enum Wake {
    Command(Option<Command>),
    Client(Result<(), watch::error::RecvError>),
    Tick,
}

struct ListedLock {
    id: String,
    name: String,
    locked: bool,
    can_lock: bool,
    can_unlock: bool,
}

struct Session {
    clients: watch::Receiver<Option<async_nats::Client>>,
    commands: mpsc::UnboundedReceiver<Command>,
    weak: Weak<AppWindow>,
    lock: Option<ListedLock>,
    in_flight: bool,
}

impl Session {
    fn client(&self) -> Option<async_nats::Client> {
        self.clients.borrow().clone()
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::SetLocked(locked) => self.set_locked(locked).await,
        }
    }

    async fn refresh(&mut self) {
        if self.in_flight {
            return;
        }
        let Some(client) = self.client() else {
            return;
        };
        match fetch_list(&client).await {
            Ok(locks) => {
                let chosen = prefer_lock(locks);
                self.lock = chosen;
                self.paint().await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "lock list failed");
            }
        }
    }

    async fn set_locked(&mut self, locked: bool) {
        if self.in_flight {
            return;
        }
        let Some(client) = self.client() else {
            self.notice("NO LINK").await;
            return;
        };
        let Some(lock) = self.lock.as_ref() else {
            self.notice("NO LOCK").await;
            return;
        };
        if locked && !lock.can_lock {
            self.notice("NO LOCK").await;
            return;
        }
        if !locked && !lock.can_unlock {
            self.notice("NO UNLOCK").await;
            return;
        }
        let id = lock.id.clone();
        let previous = lock.locked;
        self.in_flight = true;
        if let Some(current) = self.lock.as_mut() {
            current.locked = locked;
        }
        self.paint().await;
        self.notice(if locked { "LOCKING" } else { "UNLOCKING" })
            .await;

        let result = if locked {
            request_lock(&client, &id).await
        } else {
            request_unlock(&client, &id).await
        };
        self.in_flight = false;
        match result {
            Ok(()) => {
                self.notice(if locked { "LOCKED" } else { "UNLOCKED" })
                    .await;
                self.refresh().await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "lock command failed");
                if let Some(current) = self.lock.as_mut() {
                    current.locked = previous;
                }
                self.paint().await;
                self.notice("LOCK FAIL").await;
            }
        }
    }

    async fn paint(&self) {
        let (locked, name) = match &self.lock {
            Some(lock) => (lock.locked, display_name(&lock.name)),
            None => (true, "FRONT DOOR".into()),
        };
        let _ = post(
            &self.weak,
            UiUpdate {
                locked: Some(locked),
                name: Some(name),
                notice: None,
            },
        )
        .await;
    }

    async fn notice(&self, notice: &str) {
        let _ = post(
            &self.weak,
            UiUpdate {
                locked: None,
                name: None,
                notice: Some(notice.to_string()),
            },
        )
        .await;
    }
}

struct UiUpdate {
    locked: Option<bool>,
    name: Option<String>,
    notice: Option<String>,
}

async fn post(weak: &Weak<AppWindow>, update: UiUpdate) -> Result<(), ()> {
    let weak = weak.clone();
    tokio::task::spawn_blocking(move || {
        weak.upgrade_in_event_loop(move |app| {
            if let Some(locked) = update.locked {
                app.set_locked(locked);
            }
            if let Some(name) = update.name {
                app.set_lock_name(name.into());
            }
            if let Some(notice) = update.notice {
                app.set_notice(notice.into());
            }
        })
        .map_err(|_| ())
    })
    .await
    .map_err(|_| ())?
}

fn prefer_lock(mut locks: Vec<ListedLock>) -> Option<ListedLock> {
    if locks.is_empty() {
        return None;
    }
    let preferred = locks.iter().position(|lock| {
        let name = lock.name.to_ascii_lowercase();
        name.contains("front") || name.contains("door")
    });
    match preferred {
        Some(index) => Some(locks.swap_remove(index)),
        None => locks.into_iter().next(),
    }
}

fn display_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return "DOOR".into();
    }
    trimmed.to_ascii_uppercase()
}

async fn fetch_list(client: &async_nats::Client) -> Result<Vec<ListedLock>, String> {
    let reply = client
        .request(LOCKS_LIST, ListLocksRequest {}.encode_to_vec().into())
        .await
        .map_err(|error| error.to_string())?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        return Err(format!("server returned {}", code.as_str()));
    }
    let list = ListLocksResponse::decode(reply.payload).map_err(|error| error.to_string())?;
    Ok(list
        .locks
        .into_iter()
        .map(|lock| ListedLock {
            id: lock.id,
            name: lock.name,
            locked: lock.locked,
            can_lock: lock.can_lock,
            can_unlock: lock.can_unlock,
        })
        .collect())
}

async fn request_lock(client: &async_nats::Client, id: &str) -> Result<(), String> {
    let reply = client
        .request(
            LOCKS_LOCK,
            LockDoorRequest { id: id.into() }.encode_to_vec().into(),
        )
        .await
        .map_err(|error| error.to_string())?;
    check_reply(&reply)
}

async fn request_unlock(client: &async_nats::Client, id: &str) -> Result<(), String> {
    let reply = client
        .request(
            LOCKS_UNLOCK,
            UnlockDoorRequest { id: id.into() }.encode_to_vec().into(),
        )
        .await
        .map_err(|error| error.to_string())?;
    check_reply(&reply)
}

fn check_reply(reply: &async_nats::Message) -> Result<(), String> {
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        return Err(format!("server returned {}", code.as_str()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ListedLock, display_name, prefer_lock};

    #[test]
    fn prefers_a_front_door_name() {
        let chosen = prefer_lock(vec![
            ListedLock {
                id: "a".into(),
                name: "Garage".into(),
                locked: true,
                can_lock: true,
                can_unlock: true,
            },
            ListedLock {
                id: "b".into(),
                name: "Front Door".into(),
                locked: false,
                can_lock: true,
                can_unlock: true,
            },
        ])
        .expect("lock");
        assert_eq!(chosen.id, "b");
        assert_eq!(display_name(&chosen.name), "FRONT DOOR");
    }
}
