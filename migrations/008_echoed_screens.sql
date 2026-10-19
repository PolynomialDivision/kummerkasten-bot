-- Whether the bot has seen its own event showing the chat's current view
-- come back through sync. Sync delivers a room's events in order, so a tap
-- that arrives before that echo was made before the view could be seen (a
-- second device, a fast double tap): it shows the view again instead of
-- acting. Reset together with `pending` when a step with a new view is saved.
ALTER TABLE sessions ADD COLUMN echoed INTEGER NOT NULL DEFAULT 1 CHECK (echoed IN (0, 1));
