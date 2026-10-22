//! To-do list from lcars-api.
//!
//! The screen requests `lcars.reminders.list` and refreshes when
//! `lcars.reminders.updated` is published. A tap sends `lcars.reminders.set`.
//! Until the server has a list, the column stays the local sample rows.

use std::time::Duration;

use futures::StreamExt;
use lcars_api_proto::prost::Message;
use lcars_api_proto::v1::{ListRemindersRequest, ListRemindersResponse, SetReminderRequest};
use lcars_api_proto::{ERROR, ERROR_CODE, REMINDERS_LIST, REMINDERS_SET, REMINDERS_UPDATED};
use lcars_ui::slint::{ModelRc, VecModel, Weak};
use lcars_ui::{AppWindow, ReminderItem};
use tokio::sync::{mpsc, watch};

const VISIBLE: usize = 6;

pub fn attach(app: &AppWindow, toggles: mpsc::UnboundedSender<String>) {
    app.on_reminder_toggled(move |id| {
        let _ = toggles.send(id.to_string());
    });
}

pub async fn run(
    mut clients: watch::Receiver<Option<async_nats::Client>>,
    mut toggles: mpsc::UnboundedReceiver<String>,
    weak: Weak<AppWindow>,
) {
    let mut warned = false;
    loop {
        let Some(client) = wait_client(&mut clients).await else {
            return;
        };
        let mut updates = match client.subscribe(REMINDERS_UPDATED).await {
            Ok(updates) => updates,
            Err(error) => {
                tracing::warn!(error = %error, "reminder subscription failed");
                sleep_before_retry().await;
                continue;
            }
        };
        if let Err(error) = refresh(&client, &weak, &mut warned).await {
            tracing::warn!(error = %error, "reminder list failed");
        }
        loop {
            tokio::select! {
                biased;
                toggle = toggles.recv() => {
                    let Some(id) = toggle else { return };
                    if let Err(error) = toggle_done(&client, &weak, &id).await {
                        tracing::warn!(error = %error, "reminder update failed");
                        let _ = refresh(&client, &weak, &mut warned).await;
                    }
                }
                message = updates.next() => {
                    if message.is_none() {
                        break;
                    }
                    if let Err(error) = refresh(&client, &weak, &mut warned).await {
                        tracing::warn!(error = %error, "reminder list failed");
                    }
                }
                changed = clients.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    break;
                }
                () = tokio::time::sleep(Duration::from_secs(5 * 60)) => {
                    if let Err(error) = refresh(&client, &weak, &mut warned).await {
                        tracing::warn!(error = %error, "reminder list failed");
                    }
                }
            }
        }
    }
}

async fn wait_client(
    clients: &mut watch::Receiver<Option<async_nats::Client>>,
) -> Option<async_nats::Client> {
    loop {
        if let Some(client) = clients.borrow().clone() {
            return Some(client);
        }
        if clients.changed().await.is_err() {
            return None;
        }
    }
}

async fn sleep_before_retry() {
    tokio::time::sleep(Duration::from_secs(5)).await;
}

async fn refresh(
    client: &async_nats::Client,
    weak: &Weak<AppWindow>,
    warned: &mut bool,
) -> Result<(), String> {
    match fetch(client).await {
        Ok(list) => {
            paint(weak, &list).await;
            Ok(())
        }
        Err(error) if error.contains("503") => {
            if !*warned {
                *warned = true;
                tracing::info!("reminders are not configured on the server");
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

async fn toggle_done(
    client: &async_nats::Client,
    weak: &Weak<AppWindow>,
    id: &str,
) -> Result<(), String> {
    // The tap reports the id. The server stores the opposite of the current
    // done flag, and the reply replaces the column.
    let list = set(client, id).await?;
    paint(weak, &list).await;
    Ok(())
}

async fn fetch(client: &async_nats::Client) -> Result<ListRemindersResponse, String> {
    request(client, REMINDERS_LIST, &ListRemindersRequest {}).await
}

async fn set(client: &async_nats::Client, id: &str) -> Result<ListRemindersResponse, String> {
    // The display flips whatever state it last showed. Ask the server for
    // the list, then send the opposite of the matching row. A missing row
    // falls through to a refresh.
    let current = fetch(client).await?;
    let Some(row) = current.reminders.iter().find(|row| row.id == id) else {
        return Ok(current);
    };
    request(
        client,
        REMINDERS_SET,
        &SetReminderRequest {
            id: id.to_string(),
            done: !row.done,
        },
    )
    .await
}

async fn request<R: Message + Default>(
    client: &async_nats::Client,
    subject: &str,
    message: &impl Message,
) -> Result<R, String> {
    let reply = client
        .request(subject, message.encode_to_vec().into())
        .await
        .map_err(|err| err.to_string())?;
    if let Some(code) = reply
        .headers
        .as_ref()
        .and_then(|headers| headers.get(ERROR_CODE))
    {
        let message = reply
            .headers
            .as_ref()
            .and_then(|headers| headers.get(ERROR))
            .map(|value| value.as_str())
            .unwrap_or_default();
        return Err(format!("server returned {}: {message}", code.as_str()));
    }
    R::decode(reply.payload).map_err(|err| err.to_string())
}

async fn paint(weak: &Weak<AppWindow>, list: &ListRemindersResponse) {
    let view = project(list);
    for _ in 0..100 {
        let weak = weak.clone();
        let view = view.clone();
        if lcars_ui::slint::invoke_from_event_loop(move || {
            if let Some(app) = weak.upgrade() {
                apply(&app, &view);
            }
        })
        .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tracing::warn!("ui event loop did not accept a reminder update");
}

fn apply(app: &AppWindow, view: &View) {
    app.set_reminders(ModelRc::new(VecModel::from(
        view.rows
            .iter()
            .map(|row| ReminderItem {
                id: row.id.clone().into(),
                title: row.title.clone().into(),
                detail: row.detail.clone().into(),
                accent: row.accent.clone().into(),
                done: row.done,
            })
            .collect::<Vec<_>>(),
    )));
    app.set_reminders_live(true);
    app.set_reminder_open(view.open);
    app.set_reminder_done(view.done);
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct View {
    rows: Vec<Row>,
    open: i32,
    done: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Row {
    id: String,
    title: String,
    detail: String,
    accent: String,
    done: bool,
}

fn project(list: &ListRemindersResponse) -> View {
    let rows: Vec<Row> = list
        .reminders
        .iter()
        .take(VISIBLE)
        .map(|item| Row {
            id: item.id.clone(),
            title: item.title.to_uppercase(),
            detail: item.detail.clone(),
            accent: item.accent.clone(),
            done: item.done,
        })
        .collect();
    let done = i32::try_from(rows.iter().filter(|row| row.done).count()).unwrap_or(i32::MAX);
    let open = i32::try_from(rows.len()).unwrap_or(i32::MAX) - done;
    View { rows, open, done }
}

#[cfg(test)]
mod tests {
    use super::{VISIBLE, project};
    use lcars_api_proto::v1::{ListRemindersResponse, Reminder};

    #[test]
    fn projects_the_rows_the_panel_can_show() {
        let mut reminders = vec![
            Reminder {
                id: "rem_abcdefghij12".into(),
                title: "Bring in recycling bin".into(),
                detail: "Due today · House".into(),
                accent: "purple".into(),
                done: false,
            },
            Reminder {
                id: "rem_klmnopqrst34".into(),
                title: "Send invoices".into(),
                detail: "Due today · Work".into(),
                accent: "blue".into(),
                done: true,
            },
        ];
        for n in 0..8 {
            reminders.push(Reminder {
                id: format!("rem_{n:012}"),
                title: format!("extra {n}"),
                detail: String::new(),
                accent: "orange".into(),
                done: false,
            });
        }
        let view = project(&ListRemindersResponse {
            updated_at_unix: 1,
            reminders,
        });
        assert_eq!(view.rows.len(), VISIBLE);
        assert_eq!(view.rows[0].title, "BRING IN RECYCLING BIN");
        assert_eq!(view.rows[0].detail, "Due today · House");
        assert!(!view.rows[0].done);
        assert!(view.rows[1].done);
        assert_eq!(view.done, 1);
        assert_eq!(view.open, 5);
    }
}
