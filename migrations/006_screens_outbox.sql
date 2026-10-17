-- One editable screen per chat, reliable redaction, and an outbox.

-- An edit (m.replace) of the event in `replaces`: it isn't a message of its
-- own in the timeline, so it doesn't count when asking "is the screen still
-- the last message?".
ALTER TABLE dm_events ADD COLUMN replaces TEXT;
-- Redactions that failed wait until then (growing backoff, never given up
-- while the error may pass).
ALTER TABLE dm_events ADD COLUMN retry_at TEXT;

-- The bot's buttons under a screen it can still edit: [[key, reaction event],
-- ...] in the order they were added. A screen without a row (older ones, a
-- receipt) is never edited: a fresh one replaces it.
CREATE TABLE screens (
    room_id   TEXT NOT NULL,
    event_id  TEXT NOT NULL,
    buttons   TEXT NOT NULL,
    PRIMARY KEY (room_id, event_id)
);

-- Chats a resident left: the bot stays until everything it tracked there is
-- redacted, then leaves.
CREATE TABLE leaving_rooms (
    room_id   TEXT PRIMARY KEY,
    since     TEXT NOT NULL
);

-- Messages that must go out even across a crash or a homeserver hiccup:
-- check-in notices, reminders and announcements, plenum summaries and
-- updates. A batch is first a row with target '' (who gets it is decided
-- when it is sent, against current membership); that row becomes one row
-- per chat or room. Rows are deleted once the whole batch is sent, so who
-- was told is kept only while a batch is going out.
CREATE TABLE outbox (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,   -- never reused: it names the Matrix transaction
    kind        TEXT NOT NULL CHECK (kind IN
                    ('notice', 'reminder', 'announcement', 'summary', 'moved', 'cancelled')),
    ref_id      INTEGER NOT NULL,
    target      TEXT NOT NULL DEFAULT '',
    sent        INTEGER NOT NULL DEFAULT 0 CHECK (sent IN (0, 1)),
    attempts    INTEGER NOT NULL DEFAULT 0,
    retry_at    TEXT,
    created_at  TEXT NOT NULL,
    UNIQUE (kind, ref_id, target)
);
