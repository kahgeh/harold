CREATE TABLE app_messages (
    event_id TEXT PRIMARY KEY,
    at_ms INTEGER NOT NULL,
    sender TEXT NOT NULL CHECK (sender IN ('chief', 'user')),
    text TEXT NOT NULL,
    state TEXT CHECK (state IS NULL OR state IN
        ('typed', 'waiting: chief busy', 'not sent: no chief', 'not sent: messaging paused'))
);
CREATE INDEX idx_app_messages_at ON app_messages(at_ms);

CREATE TABLE app_push_subscriptions (
    endpoint TEXT PRIMARY KEY,
    p256dh TEXT NOT NULL,
    auth TEXT NOT NULL,
    subscribed_at_ms INTEGER NOT NULL,
    last_event_version INTEGER NOT NULL
);
