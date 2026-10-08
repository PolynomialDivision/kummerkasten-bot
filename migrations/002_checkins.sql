-- 📊 Check-ins (M2).
--
--   survey_templates ─┬─ questions ── question_versions ── choice_versions
--                     │      └─ choices ────────────────────────┘
--                     └─ survey_runs ── survey_run_questions ──→ question_versions
--
-- A template is editable; a run is a snapshot: it points at the exact
-- question versions (and through them, choice versions) people saw. A
-- version that a run uses can't be changed or deleted any more (triggers
-- below); editing it creates the next version.
--
-- Answers: `participations` says *that* someone answered a run (an HMAC of
-- run + Matrix ID, no time, no row order); `responses` say *what* was
-- answered (random UUID, no time). Nothing links the two.

ALTER TABLE authorized_rooms ADD COLUMN central INTEGER NOT NULL DEFAULT 0
    CHECK (central IN (0, 1));

-- Canonical metrics: what a question measures, with a stable key whatever
-- the wording. A question that means something else gets another metric.
CREATE TABLE metrics (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    topic_id    INTEGER REFERENCES topics(id),
    label       TEXT NOT NULL,
    description TEXT,
    active      INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at  TEXT NOT NULL
);

CREATE TABLE survey_templates (
    id          INTEGER PRIMARY KEY,
    key         TEXT NOT NULL UNIQUE,
    topic_id    INTEGER REFERENCES topics(id),
    emoji       TEXT NOT NULL,
    title       TEXT NOT NULL,
    intro       TEXT NOT NULL DEFAULT '',
    active      INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at  TEXT NOT NULL
);

-- A question's place in its template. What it asks lives in its versions.
CREATE TABLE questions (
    id           INTEGER PRIMARY KEY,
    template_id  INTEGER NOT NULL REFERENCES survey_templates(id),
    position     INTEGER NOT NULL,
    active       INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at   TEXT NOT NULL
);
CREATE INDEX questions_template ON questions(template_id, position);

CREATE TABLE question_versions (
    id               INTEGER PRIMARY KEY,
    question_id      INTEGER NOT NULL REFERENCES questions(id),
    version          INTEGER NOT NULL,
    kind             TEXT NOT NULL
                     CHECK (kind IN ('single', 'multiple', 'yes_no', 'scale', 'text')),
    text             TEXT NOT NULL CHECK (length(trim(text)) > 0),
    -- Kind-specific details only (scale range and faces), see `surveys::Config`.
    config           TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config)),
    required         INTEGER NOT NULL DEFAULT 1 CHECK (required IN (0, 1)),
    allow_comment    INTEGER NOT NULL DEFAULT 0 CHECK (allow_comment IN (0, 1)),
    -- Free text asking what to change: M3 may offer answers as ideas,
    -- only with the author's explicit confirmation.
    asks_suggestion  INTEGER NOT NULL DEFAULT 0 CHECK (asks_suggestion IN (0, 1)),
    metric_id        INTEGER REFERENCES metrics(id),
    problem_id       INTEGER REFERENCES problems(id),
    created_at       TEXT NOT NULL,
    UNIQUE (question_id, version)
);
CREATE INDEX question_versions_metric ON question_versions(metric_id);
CREATE INDEX question_versions_problem ON question_versions(problem_id);

-- A choice's identity across wordings ("People don't clean when it's their
-- turn" in 2026, "Cleaning turns are missed" in 2027).
CREATE TABLE choices (
    id           INTEGER PRIMARY KEY,
    question_id  INTEGER NOT NULL REFERENCES questions(id),
    created_at   TEXT NOT NULL
);

-- A choice exactly as one question version shows it.
CREATE TABLE choice_versions (
    id                   INTEGER PRIMARY KEY,
    choice_id            INTEGER NOT NULL REFERENCES choices(id),
    question_version_id  INTEGER NOT NULL REFERENCES question_versions(id),
    position             INTEGER NOT NULL,
    emoji                TEXT NOT NULL DEFAULT '',
    label                TEXT NOT NULL CHECK (length(trim(label)) > 0),
    problem_id           INTEGER REFERENCES problems(id),
    is_other             INTEGER NOT NULL DEFAULT 0 CHECK (is_other IN (0, 1)),
    UNIQUE (question_version_id, choice_id)
);
CREATE INDEX choice_versions_version ON choice_versions(question_version_id, position);
CREATE INDEX choice_versions_choice ON choice_versions(choice_id);
CREATE INDEX choice_versions_problem ON choice_versions(problem_id);

-- ── Runs ────────────────────────────────────────────────────────────────────

CREATE TABLE survey_runs (
    id            INTEGER PRIMARY KEY,
    template_id   INTEGER NOT NULL REFERENCES survey_templates(id),
    -- As shown at the start, whatever the template says later.
    emoji         TEXT NOT NULL,
    title         TEXT NOT NULL,
    intro         TEXT NOT NULL,
    -- Who is asked, as a rule (`checkins::Target`), resolved when needed.
    target        TEXT NOT NULL CHECK (json_valid(target)),
    -- Identity choices offered, comma-separated.
    identities    TEXT NOT NULL DEFAULT 'anonymous,floor,named',
    plenum_id     INTEGER REFERENCES plenums(id),
    round_id      INTEGER REFERENCES rounds(id),
    opens_at      TEXT NOT NULL,
    closes_at     TEXT NOT NULL,
    reminder_at   TEXT,
    announce      INTEGER NOT NULL DEFAULT 0 CHECK (announce IN (0, 1)),
    -- When each batch went out (per run, never per person).
    notified_at   TEXT,
    reminded_at   TEXT,
    announced_at  TEXT,
    status        TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'closed')),
    created_by    TEXT NOT NULL,
    created_at    TEXT NOT NULL,
    CHECK (closes_at > opens_at),
    CHECK (reminder_at IS NULL OR (reminder_at > opens_at AND reminder_at < closes_at))
);
CREATE INDEX survey_runs_status ON survey_runs(status, closes_at);
CREATE INDEX survey_runs_template ON survey_runs(template_id);
CREATE INDEX survey_runs_plenum ON survey_runs(plenum_id);
CREATE INDEX survey_runs_round ON survey_runs(round_id);

CREATE TABLE survey_run_questions (
    run_id               INTEGER NOT NULL REFERENCES survey_runs(id),
    position             INTEGER NOT NULL,
    question_version_id  INTEGER NOT NULL REFERENCES question_versions(id),
    PRIMARY KEY (run_id, position),
    UNIQUE (run_id, question_version_id)
);
CREATE INDEX survey_run_questions_version ON survey_run_questions(question_version_id);

-- ── Answers ─────────────────────────────────────────────────────────────────

-- That someone answered: HMAC(secret, run || Matrix ID). No time, no rowid
-- (so no insertion order), no link to `responses`.
CREATE TABLE participations (
    run_id             INTEGER NOT NULL REFERENCES survey_runs(id),
    participation_key  BLOB NOT NULL,
    PRIMARY KEY (run_id, participation_key)
) WITHOUT ROWID;

-- What was answered: a random UUID (no rowid, no time). Like submissions,
-- only named responses know who, only floor ones the floor.
CREATE TABLE responses (
    id                   TEXT PRIMARY KEY,
    run_id               INTEGER NOT NULL REFERENCES survey_runs(id),
    identity             TEXT NOT NULL CHECK (identity IN ('anonymous', 'floor', 'named')),
    respondent_mxid      TEXT,
    respondent_floor_id  INTEGER REFERENCES locations(id),
    CHECK ((identity = 'named') = (respondent_mxid IS NOT NULL)),
    CHECK (identity <> 'anonymous' OR respondent_floor_id IS NULL),
    CHECK (identity <> 'floor' OR respondent_floor_id IS NOT NULL)
) WITHOUT ROWID;
CREATE INDEX responses_run ON responses(run_id, identity);
CREATE INDEX responses_floor ON responses(run_id, respondent_floor_id);

-- One row per question asked: a value (scale, yes/no = 1/0), a text, an
-- optional comment, or skipped.
CREATE TABLE answers (
    response_id          TEXT NOT NULL REFERENCES responses(id) ON DELETE CASCADE,
    question_version_id  INTEGER NOT NULL REFERENCES question_versions(id),
    value                REAL,
    text                 TEXT,
    comment              TEXT,
    skipped              INTEGER NOT NULL DEFAULT 0 CHECK (skipped IN (0, 1)),
    PRIMARY KEY (response_id, question_version_id)
) WITHOUT ROWID;
CREATE INDEX answers_question ON answers(question_version_id, value);

CREATE TABLE answer_choices (
    response_id          TEXT NOT NULL,
    question_version_id  INTEGER NOT NULL,
    choice_version_id    INTEGER NOT NULL REFERENCES choice_versions(id),
    PRIMARY KEY (response_id, question_version_id, choice_version_id),
    FOREIGN KEY (response_id, question_version_id)
        REFERENCES answers(response_id, question_version_id) ON DELETE CASCADE
) WITHOUT ROWID;
CREATE INDEX answer_choices_choice ON answer_choices(choice_version_id);

-- Check-in notices and reminders in residents' chats, so their 📊 works.
-- Says who was *told*, never who answered.
CREATE TABLE checkin_messages (
    room_id   TEXT NOT NULL,
    event_id  TEXT NOT NULL,
    run_id    INTEGER NOT NULL REFERENCES survey_runs(id),
    PRIMARY KEY (room_id, event_id)
);

-- ── What has been asked stays as it was ─────────────────────────────────────

CREATE TRIGGER question_versions_used_no_update
BEFORE UPDATE ON question_versions
WHEN EXISTS (SELECT 1 FROM survey_run_questions WHERE question_version_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'question version already used in a check-in'); END;

CREATE TRIGGER question_versions_used_no_delete
BEFORE DELETE ON question_versions
WHEN EXISTS (SELECT 1 FROM survey_run_questions WHERE question_version_id = OLD.id)
BEGIN SELECT RAISE(ABORT, 'question version already used in a check-in'); END;

CREATE TRIGGER choice_versions_used_no_update
BEFORE UPDATE ON choice_versions
WHEN EXISTS (SELECT 1 FROM survey_run_questions
             WHERE question_version_id = OLD.question_version_id)
BEGIN SELECT RAISE(ABORT, 'choice version already used in a check-in'); END;

CREATE TRIGGER choice_versions_used_no_delete
BEFORE DELETE ON choice_versions
WHEN EXISTS (SELECT 1 FROM survey_run_questions
             WHERE question_version_id = OLD.question_version_id)
BEGIN SELECT RAISE(ABORT, 'choice version already used in a check-in'); END;

CREATE TRIGGER choice_versions_used_no_insert
BEFORE INSERT ON choice_versions
WHEN EXISTS (SELECT 1 FROM survey_run_questions
             WHERE question_version_id = NEW.question_version_id)
BEGIN SELECT RAISE(ABORT, 'question version already used in a check-in'); END;

CREATE TRIGGER survey_run_questions_no_update
BEFORE UPDATE ON survey_run_questions
BEGIN SELECT RAISE(ABORT, 'a started check-in can''t change'); END;

CREATE TRIGGER survey_run_questions_no_delete
BEFORE DELETE ON survey_run_questions
BEGIN SELECT RAISE(ABORT, 'a started check-in can''t change'); END;

-- ── Starting catalog ────────────────────────────────────────────────────────

INSERT INTO problems (key, topic_id, label, created_at) VALUES
    ('cleaning.frequency_problem', (SELECT id FROM topics WHERE key = 'cleaning'),
        'Cleaning happens too rarely', '1970-01-01T00:00:00Z'),
    ('cleaning.reminders', (SELECT id FROM topics WHERE key = 'cleaning'),
        'Reminders don''t work well', '1970-01-01T00:00:00Z');

INSERT INTO metrics (key, topic_id, label, created_at) VALUES
    ('cleaning.overall_satisfaction', (SELECT id FROM topics WHERE key = 'cleaning'),
        'How well cleaning works overall (1–5)', '1970-01-01T00:00:00Z'),
    ('cleaning.problems_experienced', (SELECT id FROM topics WHERE key = 'cleaning'),
        'Which cleaning problems people noticed', '1970-01-01T00:00:00Z');

INSERT INTO survey_templates (key, topic_id, emoji, title, intro, created_at) VALUES
    ('cleaning', (SELECT id FROM topics WHERE key = 'cleaning'), '🧹', 'Cleaning',
     'We would like to know how the current cleaning system is working for you.',
     '1970-01-01T00:00:00Z');

INSERT INTO questions (template_id, position, created_at)
SELECT id, n, '1970-01-01T00:00:00Z'
FROM survey_templates, (SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3)
WHERE key = 'cleaning';

INSERT INTO question_versions
    (question_id, version, kind, text, config, required, allow_comment,
     asks_suggestion, metric_id, created_at)
SELECT q.id, 1, v.kind, v.text, v.config, v.required, 0, v.asks, m.id, '1970-01-01T00:00:00Z'
FROM questions q
JOIN survey_templates t ON t.id = q.template_id AND t.key = 'cleaning'
JOIN (SELECT 1 AS n, 'scale' AS kind, 'How well is cleaning currently working?' AS text,
             '{"min":1,"max":5,"faces":["😩","😕","😐","🙂","😄"]}' AS config,
             1 AS required, 0 AS asks, 'cleaning.overall_satisfaction' AS metric
      UNION ALL SELECT 2, 'multiple', 'Which problems have you noticed?', '{}', 0, 0,
             'cleaning.problems_experienced'
      UNION ALL SELECT 3, 'text', 'What would you change?', '{}', 0, 1, NULL) v
  ON v.n = q.position
LEFT JOIN metrics m ON m.key = v.metric;

INSERT INTO choices (question_id, created_at)
SELECT q.id, '1970-01-01T00:00:00Z'
FROM questions q JOIN survey_templates t ON t.id = q.template_id AND t.key = 'cleaning',
     (SELECT 1 AS n UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5)
WHERE q.position = 2;

INSERT INTO choice_versions
    (choice_id, question_version_id, position, emoji, label, problem_id, is_other)
SELECT c.id, qv.id, c.rn, v.emoji, v.label, p.id, v.other
FROM (SELECT id, question_id, row_number() OVER (ORDER BY id) AS rn FROM choices) c
JOIN question_versions qv ON qv.question_id = c.question_id AND qv.version = 1
JOIN (SELECT 1 AS n, '🙋' AS emoji, 'Cleaning turns are skipped' AS label,
             'cleaning.skipped_turns' AS problem, 0 AS other
      UNION ALL SELECT 2, '📋', 'Responsibilities are unclear', 'cleaning.tasks_unclear', 0
      UNION ALL SELECT 3, '⏳', 'Cleaning happens too rarely', 'cleaning.frequency_problem', 0
      UNION ALL SELECT 4, '🔔', 'Reminders don''t work well', 'cleaning.reminders', 0
      UNION ALL SELECT 5, '💬', 'Something else', NULL, 1) v
  ON v.n = c.rn
LEFT JOIN problems p ON p.key = v.problem;
