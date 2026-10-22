-- 🗓️ The plenum's dates come from the Nextcloud calendar (plenum-bot's, shared
-- read-only with this bot), no longer from recurrence rules of our own.
--
-- * `calendar_id` is the calendar occurrence a row follows (the event's UID,
--   for one of a series UID#original-date): it keeps the row (its round,
--   "delete after" submissions, check-ins, announcements) when the event moves.
-- * Rows the old rules generated have none: the first read of the calendar
--   adopts the one at the same time (else the same day), and cancels the rest
--   of the old schedule, so the open round stays on the row it belongs to.
-- * `plenum_rules`, `rule_id` and `rule_date` stay as history; nothing reads
--   or writes them any more.

ALTER TABLE plenums ADD COLUMN calendar_id TEXT;
CREATE UNIQUE INDEX plenums_calendar_id ON plenums(calendar_id) WHERE calendar_id IS NOT NULL;
