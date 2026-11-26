-- A chat's screen as the engine expects it, not yet confirmed on Matrix: set
-- in the transaction that saves a flow step with a new view, cleared once the
-- screen was shown with all its buttons. In between (a crash or a failed edit
-- between saving and showing, a button that could not be added) the first
-- tap re-shows the screen instead of acting on a view the person may not
-- have seen; at startup every pending screen is shown again.
ALTER TABLE sessions ADD COLUMN pending INTEGER NOT NULL DEFAULT 0 CHECK (pending IN (0, 1));
