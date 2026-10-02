//! Platform notifications sing-box asks its GUI to show:
//! `SubscribeNotifications`.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};

impl SingBoxApi {
    /// Stream `SubscribeNotifications`. Nothing is replayed on subscribe;
    /// events are pushed as they happen (and missed while not subscribed).
    ///
    /// Under the `api` service nothing sends notifications today: the
    /// senders (Taildrop file arrivals, Tailscale login prompts) only notify
    /// through a platform interface, which only the official GUIs' daemon
    /// provides. So this stream stays silent — no headers even — and
    /// returns `TimedOut` every `IDLE_STREAM_READ_TIMEOUT`; re-subscribe.
    pub fn stream_notifications(
        &self,
        mut on_event: impl FnMut(NotificationEvent) -> bool,
    ) -> Result<(), ApiError> {
        self.stream(
            "SubscribeNotifications",
            &(),
            IDLE_STREAM_READ_TIMEOUT,
            |event: pb::NotificationEvent| match NotificationEvent::from_proto(event) {
                Some(event) => on_event(event),
                None => true,
            },
        )
    }
}

/// A notification to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Notification {
    /// Stable per notification (e.g. `taildrop/<endpoint>/<file>`), so a
    /// later `Cancel` can withdraw it.
    pub identifier: String,
    /// Category name and id (e.g. Taildrop's own).
    pub type_name: String,
    pub type_id: i32,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    /// Where clicking it should go; may be empty.
    pub open_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotificationEvent {
    Send(Notification),
    /// Withdraw the notification with this identifier and type.
    Cancel {
        identifier: String,
        type_id: i32,
    },
}

impl NotificationEvent {
    /// `None` for an empty oneof (an event kind newer than this client).
    fn from_proto(event: pb::NotificationEvent) -> Option<Self> {
        Some(match event.event? {
            pb::notification_event::Event::Send(notification) => {
                NotificationEvent::Send(Notification {
                    identifier: notification.identifier,
                    type_name: notification.type_name,
                    type_id: notification.type_id,
                    title: notification.title,
                    subtitle: notification.subtitle,
                    body: notification.body,
                    open_url: notification.open_url,
                })
            }
            pb::notification_event::Event::Cancel(cancel) => NotificationEvent::Cancel {
                identifier: cancel.identifier,
                type_id: cancel.type_id,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_events_map_both_arms() {
        let send = NotificationEvent::from_proto(pb::NotificationEvent {
            event: Some(pb::notification_event::Event::Send(pb::Notification {
                identifier: "taildrop/ts/a.txt".into(),
                type_name: "Taildrop".into(),
                type_id: 3,
                title: "File received".into(),
                subtitle: "from laptop".into(),
                body: "a.txt".into(),
                open_url: "sing-box://taildrop".into(),
            })),
        });
        assert_eq!(
            send,
            Some(NotificationEvent::Send(Notification {
                identifier: "taildrop/ts/a.txt".into(),
                type_name: "Taildrop".into(),
                type_id: 3,
                title: "File received".into(),
                subtitle: "from laptop".into(),
                body: "a.txt".into(),
                open_url: "sing-box://taildrop".into(),
            }))
        );
        let cancel = NotificationEvent::from_proto(pb::NotificationEvent {
            event: Some(pb::notification_event::Event::Cancel(
                pb::NotificationCancel {
                    identifier: "taildrop/ts/a.txt".into(),
                    type_id: 3,
                },
            )),
        });
        assert_eq!(
            cancel,
            Some(NotificationEvent::Cancel {
                identifier: "taildrop/ts/a.txt".into(),
                type_id: 3
            })
        );
        assert_eq!(
            NotificationEvent::from_proto(pb::NotificationEvent { event: None }),
            None
        );
    }
}
