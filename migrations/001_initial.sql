-- 📮 Kummerkasten — initial schema.
--
-- Conventions:
--   * timestamps are UTC, RFC 3339 text; `*_on` columns are local dates
--     (YYYY-MM-DD) in the configured timezone;
--   * structural rows (topics, locations, problems) are deactivated, never
--     deleted, so history keeps pointing at them;
--   * nothing that belongs to a submission references a Matrix user unless
--     the submitter chose to be named (see the CHECKs on `submissions`).

-- ── Access ──────────────────────────────────────────────────────────────────

-- Rooms whose current members may use the Kummerkasten. Removing a room
-- deactivates it (kept for the audit trail).
CREATE TABLE authorized_rooms (
    room_id   TEXT PRIMARY KEY,
    label     TEXT NOT NULL,
    added_by  TEXT NOT NULL,
    added_at  TEXT NOT NULL,
    active    INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1))
);

-- ── Catalog ─────────────────────────────────────────────────────────────────

-- A tree: a location with children (🍳 Kitchens) is a group to pick from.
CREATE TABLE locations (
    id        INTEGER PRIMARY KEY,
    key       TEXT NOT NULL UNIQUE,
    parent_id INTEGER REFERENCES locations(id),
    emoji     TEXT NOT NULL,
    label     TEXT NOT NULL,
    -- Residents pick their floor from these (🏠 With my floor).
    is_floor  INTEGER NOT NULL DEFAULT 0 CHECK (is_floor IN (0, 1)),
    sort      INTEGER NOT NULL DEFAULT 0,
    active    INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    CHECK (parent_id IS NULL OR parent_id <> id)
);

-- Topics and their subtopics (one level deep in the UI, a tree here).
CREATE TABLE topics (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    parent_id   INTEGER REFERENCES topics(id),
    emoji       TEXT NOT NULL,
    label       TEXT NOT NULL,
    -- A subtopic that names a recurring problem.
    problem_id  INTEGER REFERENCES problems(id),
    -- "💡 Idea / suggestion" entries.
    is_idea     INTEGER NOT NULL DEFAULT 0 CHECK (is_idea IN (0, 1)),
    sort        INTEGER NOT NULL DEFAULT 0,
    active      INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    CHECK (parent_id IS NULL OR parent_id <> id)
);
CREATE INDEX topics_parent ON topics(parent_id);

-- Canonical recurring problems: a stable key, whatever the wording of the
-- day. Free feedback and check-ins point at the same rows.
CREATE TABLE problems (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    topic_id    INTEGER REFERENCES topics(id),
    label       TEXT NOT NULL,
    description TEXT,
    active      INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at  TEXT NOT NULL
);

-- ── People ──────────────────────────────────────────────────────────────────

-- Who has a Kummerkasten chat. Never referenced by submissions.
CREATE TABLE residents (
    mxid        TEXT PRIMARY KEY,
    dm_room_id  TEXT UNIQUE,
    dm_joined   INTEGER NOT NULL DEFAULT 0 CHECK (dm_joined IN (0, 1)),
    -- Self-chosen, for 🏠 With my floor.
    floor_id    INTEGER REFERENCES locations(id),
    -- A notice to post (with a fresh menu) once the chat is cleared:
    -- 'round_cleared'.
    pending_notice TEXT,
    created_at  TEXT NOT NULL
);

-- ── Plenums ─────────────────────────────────────────────────────────────────

-- Versions of the recurrence: each applies from `effective_from` (a local
-- date) until the next version's. JSON for the rule's parameters only, see
-- `plenum::Recurrence`.
CREATE TABLE plenum_rules (
    id              INTEGER PRIMARY KEY,
    recurrence      TEXT NOT NULL,
    effective_from  TEXT NOT NULL,
    created_by      TEXT,
    created_at      TEXT NOT NULL
);

-- Concrete plenums: generated from a rule (rule_id + rule_date) or added by
-- hand (both NULL). A generated row is never generated twice, so moving or
-- cancelling it sticks.
CREATE TABLE plenums (
    id            INTEGER PRIMARY KEY,
    rule_id       INTEGER REFERENCES plenum_rules(id),
    rule_date     TEXT,
    starts_at     TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'scheduled'
                  CHECK (status IN ('scheduled', 'cancelled', 'completed')),
    rescheduled   INTEGER NOT NULL DEFAULT 0 CHECK (rescheduled IN (0, 1)),
    completed_at  TEXT,
    -- The day-before summary went out (M3), so it isn't sent twice.
    announced_at  TEXT,
    created_at    TEXT NOT NULL,
    UNIQUE (rule_id, rule_date),
    CHECK ((rule_id IS NULL) = (rule_date IS NULL)),
    CHECK ((status = 'completed') = (completed_at IS NOT NULL))
);
CREATE INDEX plenums_starts_at ON plenums(starts_at);

-- Who changed which plenum how (admin audit trail).
CREATE TABLE plenum_changes (
    id         INTEGER PRIMARY KEY,
    plenum_id  INTEGER REFERENCES plenums(id) ON DELETE SET NULL,
    action     TEXT NOT NULL,
    detail     TEXT NOT NULL,
    actor      TEXT NOT NULL,
    at         TEXT NOT NULL
);

-- A round collects everything for one plenum. Exactly one is open.
CREATE TABLE rounds (
    id         INTEGER PRIMARY KEY,
    plenum_id  INTEGER NOT NULL UNIQUE REFERENCES plenums(id),
    opened_at  TEXT NOT NULL,
    closed_at  TEXT,
    status     TEXT NOT NULL CHECK (status IN ('open', 'closed')),
    CHECK ((status = 'closed') = (closed_at IS NOT NULL))
);
CREATE UNIQUE INDEX rounds_one_open ON rounds(status) WHERE status = 'open';

-- ── Submissions ─────────────────────────────────────────────────────────────

-- IDs are random, not sequential, so their order says nothing about who
-- wrote when. Dates are kept to the day.
CREATE TABLE submissions (
    id                      INTEGER PRIMARY KEY,
    round_id                INTEGER NOT NULL REFERENCES rounds(id),
    topic_id                INTEGER NOT NULL REFERENCES topics(id),
    problem_id              INTEGER REFERENCES problems(id),
    location_id             INTEGER REFERENCES locations(id),
    body                    TEXT NOT NULL CHECK (length(trim(body)) > 0),
    suggestion              TEXT,
    identity                TEXT NOT NULL
                            CHECK (identity IN ('anonymous', 'floor', 'named')),
    submitter_mxid          TEXT,
    submitter_floor_id      INTEGER REFERENCES locations(id),
    retention               TEXT NOT NULL
                            CHECK (retention IN ('keep', 'delete_after_plenum')),
    delete_after_plenum_id  INTEGER REFERENCES plenums(id),
    for_plenum              INTEGER NOT NULL DEFAULT 1 CHECK (for_plenum IN (0, 1)),
    submitted_on            TEXT NOT NULL,
    -- Only named submissions know who wrote them.
    CHECK ((identity = 'named') = (submitter_mxid IS NOT NULL)),
    -- Anonymous ones don't know the floor either; floor ones must.
    CHECK (identity <> 'anonymous' OR submitter_floor_id IS NULL),
    CHECK (identity <> 'floor' OR submitter_floor_id IS NOT NULL),
    CHECK ((retention = 'delete_after_plenum') = (delete_after_plenum_id IS NOT NULL))
);
CREATE INDEX submissions_round ON submissions(round_id);
CREATE INDEX submissions_topic ON submissions(topic_id);
CREATE INDEX submissions_problem ON submissions(problem_id);
CREATE INDEX submissions_delete_after ON submissions(delete_after_plenum_id);
CREATE INDEX submissions_submitter ON submissions(submitter_mxid);

-- An HMAC of the deletion code, never the code itself.
CREATE TABLE deletion_credentials (
    submission_id  INTEGER PRIMARY KEY REFERENCES submissions(id) ON DELETE CASCADE,
    token_hmac     BLOB NOT NULL UNIQUE
);

-- ── Matrix bookkeeping (temporary) ──────────────────────────────────────────

-- One conversation per resident: where they are in a flow and their draft.
-- Deleted on submit (in the same transaction), cancel, expiry and cleanup.
CREATE TABLE sessions (
    mxid             TEXT PRIMARY KEY,
    room_id          TEXT NOT NULL,
    flow             TEXT NOT NULL,
    screen_event_id  TEXT,
    updated_at       TEXT NOT NULL
);

-- Every event in a Kummerkasten chat, so it can be redacted precisely: the
-- current flow's on Cancel/Submit, everything of a round after its plenum.
-- Deliberately knows nothing about submissions.
CREATE TABLE dm_events (
    room_id     TEXT NOT NULL,
    event_id    TEXT NOT NULL,
    round_id    INTEGER REFERENCES rounds(id),
    in_session  INTEGER NOT NULL DEFAULT 0 CHECK (in_session IN (0, 1)),
    -- Marked for redaction; the job runner redacts and deletes the row.
    doomed      INTEGER NOT NULL DEFAULT 0 CHECK (doomed IN (0, 1)),
    attempts    INTEGER NOT NULL DEFAULT 0,
    sent_at     TEXT NOT NULL,
    PRIMARY KEY (room_id, event_id)
);
CREATE INDEX dm_events_doomed ON dm_events(doomed) WHERE doomed = 1;
CREATE INDEX dm_events_round ON dm_events(round_id);

-- Matrix events already handled (a sync after a restart delivers again).
CREATE TABLE processed_events (
    event_id  TEXT PRIMARY KEY,
    seen_at   TEXT NOT NULL
);

-- Entry messages in authorized rooms whose 📮 opens a private chat.
CREATE TABLE entry_messages (
    room_id   TEXT NOT NULL,
    event_id  TEXT NOT NULL,
    PRIMARY KEY (room_id, event_id)
);

-- ── Starting catalog (admins extend it; nothing here is special) ────────────

INSERT INTO locations (key, emoji, label, is_floor, sort) VALUES
    ('floor.1',   '1️⃣', '1st floor',   1, 10),
    ('floor.2',   '2️⃣', '2nd floor',   1, 20),
    ('floor.3',   '3️⃣', '3rd floor',   1, 30),
    ('floor.4',   '4️⃣', '4th floor',   1, 40),
    ('kitchens',  '🍳', 'Kitchens',    0, 50),
    ('garden',    '🌳', 'Garden',      0, 90),
    ('basement',  '⬇️', 'Basement',    0, 100),
    ('house',     '🏠', 'Whole house', 0, 110),
    ('other',     '✏️', 'Other',       0, 120);

INSERT INTO locations (key, parent_id, emoji, label, sort) VALUES
    ('kitchen.1', (SELECT id FROM locations WHERE key = 'kitchens'), '🍳', 'Kitchen 1', 10),
    ('kitchen.2', (SELECT id FROM locations WHERE key = 'kitchens'), '🍳', 'Kitchen 2', 20),
    ('kitchen.3', (SELECT id FROM locations WHERE key = 'kitchens'), '🍳', 'Kitchen 3', 30),
    ('kitchen.4', (SELECT id FROM locations WHERE key = 'kitchens'), '🍳', 'Kitchen 4', 40);

INSERT INTO topics (key, emoji, label, is_idea, sort) VALUES
    ('cleaning',  '🧹', 'Cleaning',      0, 10),
    ('kitchen',   '🍳', 'Kitchen',       0, 20),
    ('noise',     '🔊', 'Noise',         0, 30),
    ('community', '🤝', 'Community',     0, 40),
    ('house',     '🏠', 'House',         0, 50),
    ('idea',      '💡', 'Idea / Wish',   1, 60),
    ('positive',  '❤️', 'Something good', 0, 70),
    ('other',     '💬', 'Something else', 0, 80);

INSERT INTO problems (key, topic_id, label, created_at) VALUES
    ('cleaning.cleanliness',   (SELECT id FROM topics WHERE key = 'cleaning'), 'Cleanliness', '1970-01-01T00:00:00Z'),
    ('cleaning.schedule',      (SELECT id FROM topics WHERE key = 'cleaning'), 'Cleaning schedule', '1970-01-01T00:00:00Z'),
    ('cleaning.skipped_turns', (SELECT id FROM topics WHERE key = 'cleaning'), 'People skipping their turn', '1970-01-01T00:00:00Z'),
    ('cleaning.tasks_unclear', (SELECT id FROM topics WHERE key = 'cleaning'), 'Responsibilities unclear', '1970-01-01T00:00:00Z'),
    ('cleaning.equipment',     (SELECT id FROM topics WHERE key = 'cleaning'), 'Equipment', '1970-01-01T00:00:00Z');

INSERT INTO topics (key, parent_id, emoji, label, problem_id, is_idea, sort) VALUES
    ('cleaning.cleanliness',   (SELECT id FROM topics WHERE key = 'cleaning'), '🧼', 'Cleanliness',
        (SELECT id FROM problems WHERE key = 'cleaning.cleanliness'), 0, 10),
    ('cleaning.schedule',      (SELECT id FROM topics WHERE key = 'cleaning'), '📅', 'Cleaning schedule',
        (SELECT id FROM problems WHERE key = 'cleaning.schedule'), 0, 20),
    ('cleaning.skipped_turns', (SELECT id FROM topics WHERE key = 'cleaning'), '🙋', 'People skipping their turn',
        (SELECT id FROM problems WHERE key = 'cleaning.skipped_turns'), 0, 30),
    ('cleaning.tasks_unclear', (SELECT id FROM topics WHERE key = 'cleaning'), '📋', 'Responsibilities unclear',
        (SELECT id FROM problems WHERE key = 'cleaning.tasks_unclear'), 0, 40),
    ('cleaning.equipment',     (SELECT id FROM topics WHERE key = 'cleaning'), '🧽', 'Equipment',
        (SELECT id FROM problems WHERE key = 'cleaning.equipment'), 0, 50),
    ('cleaning.idea',          (SELECT id FROM topics WHERE key = 'cleaning'), '💡', 'Idea / suggestion', NULL, 1, 60),
    ('cleaning.other',         (SELECT id FROM topics WHERE key = 'cleaning'), '💬', 'Something else', NULL, 0, 70);
