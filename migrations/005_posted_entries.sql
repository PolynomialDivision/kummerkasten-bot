-- The 📮 entry message posted into a room ([[kummerkasten.rooms]] entry = true
-- posts one only where none was posted yet; `!kk entry` posts another).
CREATE TABLE posted_entries (
    room_id    TEXT PRIMARY KEY,
    event_id   TEXT NOT NULL,
    posted_at  TEXT NOT NULL
);
