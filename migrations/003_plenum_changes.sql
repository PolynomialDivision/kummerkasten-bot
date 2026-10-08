-- 🗓️ Plenums move, are cancelled, or simply don't happen (M2 fixes).
--
-- * A moved plenum keeps its ID: everything pointing at it (its round,
--   "delete after the plenum" submissions, check-ins, announcements) stays
--   pointed. `original_starts_at` keeps the first date, `starts_at` is the
--   one in force.
-- * A plenum whose time has passed is *not* completed by that: it waits
--   until an admin completes it (or the explicit auto-complete policy does).
-- * Check-in deadlines are either fixed, or follow their plenum.
-- * Who was sent a check-in notice or reminder is not kept per person.

ALTER TABLE plenums ADD COLUMN original_starts_at TEXT;
ALTER TABLE plenums ADD COLUMN reschedule_reason TEXT;
ALTER TABLE plenums ADD COLUMN rescheduled_at TEXT;
-- The day-before summary went out, and since then the plenum was moved or
-- cancelled: a short update is due in the same room(s).
ALTER TABLE plenums ADD COLUMN update_pending TEXT
    CHECK (update_pending IS NULL OR update_pending IN ('moved', 'cancelled'));

-- The first date of plenums moved before this migration: what the first
-- move in the audit trail moved it from.
UPDATE plenums SET original_starts_at = coalesce(
    (SELECT trim(substr(c.detail, 1, instr(c.detail, '→') - 1))
     FROM plenum_changes c
     WHERE c.plenum_id = plenums.id AND c.action = 'move'
     ORDER BY c.id LIMIT 1),
    starts_at);

-- ── Check-in deadlines ──────────────────────────────────────────────────────

-- 'fixed': `closes_at` is what the admin set and never moves.
-- 'relative_to_plenum': `closes_at` = the plenum's start + offset, and the
-- reminder `reminder_offset_minutes` before that — both recomputed whenever
-- the plenum moves (or the round moves to another plenum).
ALTER TABLE survey_runs ADD COLUMN deadline_mode TEXT NOT NULL DEFAULT 'fixed'
    CHECK (deadline_mode IN ('fixed', 'relative_to_plenum'));
ALTER TABLE survey_runs ADD COLUMN deadline_offset_minutes INTEGER;
ALTER TABLE survey_runs ADD COLUMN reminder_offset_minutes INTEGER;

-- Runs that close when their plenum starts were "until the plenum" all along.
UPDATE survey_runs SET
    deadline_mode = 'relative_to_plenum',
    deadline_offset_minutes = 0,
    reminder_offset_minutes = CASE WHEN reminder_at IS NULL THEN NULL ELSE
        CAST(round((julianday(closes_at) - julianday(reminder_at)) * 1440) AS INTEGER) END
WHERE plenum_id IS NOT NULL
  AND closes_at = (SELECT starts_at FROM plenums WHERE id = survey_runs.plenum_id);

-- Aggregates only: how many were asked when it started, how many notices
-- and reminders went out. Never who.
ALTER TABLE survey_runs ADD COLUMN eligible_at_launch INTEGER;
ALTER TABLE survey_runs ADD COLUMN notices_sent INTEGER NOT NULL DEFAULT 0;
ALTER TABLE survey_runs ADD COLUMN reminders_sent INTEGER NOT NULL DEFAULT 0;

-- ── Delivery bookkeeping, minimized ─────────────────────────────────────────

-- A check-in notice or reminder only needs its 📊 to work: event → run.
-- Without the room, the table can't say whose chat got a reminder (and so
-- who hadn't answered by then). Rows still go when the run closes.
ALTER TABLE checkin_messages RENAME TO checkin_messages_old;
CREATE TABLE checkin_messages (
    event_id  TEXT PRIMARY KEY,
    run_id    INTEGER NOT NULL REFERENCES survey_runs(id)
) WITHOUT ROWID;
INSERT OR IGNORE INTO checkin_messages (event_id, run_id)
    SELECT event_id, run_id FROM checkin_messages_old;
DROP TABLE checkin_messages_old;
CREATE INDEX checkin_messages_run ON checkin_messages(run_id);

-- Chat events are only kept to be redacted; the day is enough to order
-- that, and a minute-exact time could tell which chats got a reminder.
UPDATE dm_events SET sent_at = substr(sent_at, 1, 10);
