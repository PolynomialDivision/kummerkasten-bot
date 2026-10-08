-- 📈 Results, 💡 ideas, 🧪 measures, 📮 plenum summaries (M3).
--
-- Results are computed when asked, from what is stored *now*: a deleted
-- submission is gone from every statistic, nothing keeps counting it. No
-- shadow aggregates.
--
-- Ideas are public objects a resident chose to share — separate from the
-- feedback or answer they came from (which keeps its own identity and
-- retention). Nothing links an idea to its author, a submission or a
-- response. Support and comments are counted per account through a keyed
-- hash (like check-in participation): one each, never a list of names.

CREATE TABLE ideas (
    id          INTEGER PRIMARY KEY,
    title       TEXT NOT NULL CHECK (length(trim(title)) > 0),
    body        TEXT,
    topic_id    INTEGER REFERENCES topics(id),
    problem_id  INTEGER REFERENCES problems(id),
    -- Normalized words, for "a similar idea already exists" (no ML).
    keywords    TEXT NOT NULL DEFAULT '',
    status      TEXT NOT NULL DEFAULT 'suggested'
                CHECK (status IN ('suggested', 'discussing', 'trying',
                                  'implemented', 'rejected', 'archived')),
    origin      TEXT NOT NULL CHECK (origin IN ('feedback', 'checkin', 'admin')),
    -- The round it was shared in (for "ideas this round"), day only.
    round_id    INTEGER REFERENCES rounds(id),
    created_on  TEXT NOT NULL
);
CREATE INDEX ideas_status ON ideas(status);
CREATE INDEX ideas_topic ON ideas(topic_id);
CREATE INDEX ideas_problem ON ideas(problem_id);

-- That an account supports an idea: HMAC(secret, idea ‖ Matrix ID). No
-- time, no row order, never shown — only counted.
CREATE TABLE idea_supports (
    idea_id        INTEGER NOT NULL REFERENCES ideas(id) ON DELETE CASCADE,
    supporter_key  BLOB NOT NULL,
    PRIMARY KEY (idea_id, supporter_key)
) WITHOUT ROWID;

-- Comments, shown without an author: one per account and idea (writing
-- again replaces it), keyed like support.
CREATE TABLE idea_comments (
    idea_id        INTEGER NOT NULL REFERENCES ideas(id) ON DELETE CASCADE,
    commenter_key  BLOB NOT NULL,
    body           TEXT NOT NULL CHECK (length(trim(body)) > 0),
    created_on     TEXT NOT NULL,
    PRIMARY KEY (idea_id, commenter_key)
) WITHOUT ROWID;

-- What the plenum decided to try. Context on the timeline, never a cause.
CREATE TABLE measures (
    id           INTEGER PRIMARY KEY,
    title        TEXT NOT NULL CHECK (length(trim(title)) > 0),
    description  TEXT NOT NULL DEFAULT '',
    topic_id     INTEGER REFERENCES topics(id),
    problem_id   INTEGER REFERENCES problems(id),
    metric_id    INTEGER REFERENCES metrics(id),
    plenum_id    INTEGER REFERENCES plenums(id),
    starts_on    TEXT,
    ends_on      TEXT,
    status       TEXT NOT NULL DEFAULT 'proposed'
                 CHECK (status IN ('proposed', 'decided', 'trying', 'adopted', 'stopped')),
    created_by   TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    CHECK (ends_on IS NULL OR starts_on IS NULL OR ends_on >= starts_on)
);
CREATE INDEX measures_topic ON measures(topic_id);
CREATE INDEX measures_plenum ON measures(plenum_id);

-- Day-before summaries and updates as posted (aggregate text only), so a
-- move or cancellation can update them in place.
CREATE TABLE plenum_announcements (
    room_id    TEXT NOT NULL,
    event_id   TEXT NOT NULL,
    plenum_id  INTEGER NOT NULL REFERENCES plenums(id),
    kind       TEXT NOT NULL CHECK (kind IN ('summary', 'update')),
    body       TEXT NOT NULL,
    -- The date it told people (an update after it says what changed since).
    for_starts_at  TEXT NOT NULL,
    PRIMARY KEY (room_id, event_id)
);
CREATE INDEX plenum_announcements_plenum ON plenum_announcements(plenum_id);

-- Entry messages may open the chat at a place: 📮 menu, 📈 results, 💡 ideas.
-- (No schema change: the reaction key says which.)
