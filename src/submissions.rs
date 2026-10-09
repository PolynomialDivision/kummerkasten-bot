//! Submissions: what is stored, with which identity, and how it goes away.
//!
//! * 🥷 anonymous: no Matrix ID, no floor;
//! * 🏠 floor: the floor, no Matrix ID;
//! * 👤 named: the Matrix ID.
//!
//! The table's CHECK constraints enforce exactly that, so no code path can
//! quietly keep more. Anonymous and floor submissions get a deletion code;
//! only its HMAC is stored. IDs are random and dates kept to the day, so
//! neither says when (in which order) something was written.

use anyhow::{bail, Context, Result};
use chrono::NaiveDate;
use rand::Rng;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::{plenums, secrets::Secrets, time};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Identity {
    Anonymous,
    Floor,
    Named,
}

impl Identity {
    pub fn as_str(self) -> &'static str {
        match self {
            Identity::Anonymous => "anonymous",
            Identity::Floor => "floor",
            Identity::Named => "named",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "floor" => Identity::Floor,
            "named" => Identity::Named,
            _ => Identity::Anonymous,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Identity::Anonymous => "🥷 Anonymous",
            Identity::Floor => "🏠 With my floor",
            Identity::Named => "👤 With my name",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Retention {
    Keep,
    DeleteAfterPlenum,
}

impl Retention {
    pub fn as_str(self) -> &'static str {
        match self {
            Retention::Keep => "keep",
            Retention::DeleteAfterPlenum => "delete_after_plenum",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "delete_after_plenum" => Retention::DeleteAfterPlenum,
            _ => Retention::Keep,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Retention::Keep => "♾️ Keep it",
            Retention::DeleteAfterPlenum => "🧹 Delete after the next plenum",
        }
    }
}

/// Everything the flow collected.
#[derive(Clone, Debug, PartialEq)]
pub struct NewSubmission {
    pub topic_id: i64,
    pub problem_id: Option<i64>,
    pub location_id: Option<i64>,
    pub body: String,
    pub suggestion: Option<String>,
    pub identity: Identity,
    pub retention: Retention,
    pub for_plenum: bool,
}

#[derive(Debug, PartialEq)]
pub struct Submitted {
    pub id: i64,
    /// The deletion code, for anonymous and floor submissions: shown once.
    pub token: Option<String>,
    pub round_id: i64,
    pub plenum_id: i64,
}

/// Store a submission in the open round. `submitter` is only written for
/// named ones; `floor` only for floor ones (and required there).
pub fn submit(
    conn: &Connection,
    secrets: &Secrets,
    new: &NewSubmission,
    submitter: &str,
    floor: Option<i64>,
    today: NaiveDate,
) -> Result<Submitted> {
    let round = plenums::open_round(conn)?
        .context("There's no plenum scheduled, so the Kummerkasten is resting.")?;
    let body = new.body.trim();
    if body.is_empty() {
        bail!("Empty feedback");
    }
    let (mxid, floor) = match new.identity {
        Identity::Anonymous => (None, None),
        Identity::Floor => (None, Some(floor.context("Which floor?")?)),
        Identity::Named => (Some(submitter), None),
    };
    let delete_after = (new.retention == Retention::DeleteAfterPlenum).then_some(round.plenum_id);
    let mut rng = rand::rngs::OsRng;
    let id = loop {
        let id: i64 = rng.gen_range(1..i64::MAX);
        let inserted = conn.execute(
            "INSERT OR IGNORE INTO submissions
               (id, round_id, topic_id, problem_id, location_id, body, suggestion,
                identity, submitter_mxid, submitter_floor_id, retention,
                delete_after_plenum_id, for_plenum, submitted_on)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                id,
                round.id,
                new.topic_id,
                new.problem_id,
                new.location_id,
                body,
                new.suggestion
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty()),
                new.identity.as_str(),
                mxid,
                floor,
                new.retention.as_str(),
                delete_after,
                new.for_plenum,
                time::date_text(today),
            ],
        )?;
        if inserted == 1 {
            break id;
        }
    };
    let token = if new.identity == Identity::Named {
        None
    } else {
        let token = crate::secrets::new_token();
        conn.execute(
            "INSERT INTO deletion_credentials (submission_id, token_hmac) VALUES (?1, ?2)",
            params![id, secrets.token_hmac(&token).context("Fresh code")?],
        )?;
        Some(token)
    };
    Ok(Submitted {
        id,
        token,
        round_id: round.id,
        plenum_id: round.plenum_id,
    })
}

/// A submission as its author sees it when managing it.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    pub id: i64,
    pub topic: String,
    pub location: Option<String>,
    pub body: String,
    pub suggestion: Option<String>,
    pub identity: Identity,
    pub retention: Retention,
    pub for_plenum: bool,
    pub submitted_on: NaiveDate,
}

const VIEW: &str = "SELECT s.id, t.emoji || ' ' || t.label, l.emoji || ' ' || l.label,
                           s.body, s.suggestion, s.identity, s.retention, s.for_plenum,
                           s.submitted_on
                    FROM submissions s
                    JOIN topics t ON t.id = s.topic_id
                    LEFT JOIN locations l ON l.id = s.location_id";

fn view(r: &rusqlite::Row) -> rusqlite::Result<View> {
    Ok(View {
        id: r.get(0)?,
        topic: r.get(1)?,
        location: r.get(2)?,
        body: r.get(3)?,
        suggestion: r.get(4)?,
        identity: Identity::parse(&r.get::<_, String>(5)?),
        retention: Retention::parse(&r.get::<_, String>(6)?),
        for_plenum: r.get(7)?,
        submitted_on: time::parse_date(&r.get::<_, String>(8)?).unwrap_or_default(),
    })
}

/// The submission a deletion code belongs to: that one only.
pub fn by_token(conn: &Connection, secrets: &Secrets, token: &str) -> Result<Option<View>> {
    let Some(hmac) = secrets.token_hmac(token) else {
        return Ok(None);
    };
    Ok(conn
        .query_row(
            &format!(
                "{VIEW} JOIN deletion_credentials d ON d.submission_id = s.id
                 WHERE d.token_hmac = ?1"
            ),
            [hmac],
            view,
        )
        .optional()?)
}

/// `mxid`'s named submissions, newest first.
pub fn named_by(conn: &Connection, mxid: &str) -> Result<Vec<View>> {
    let mut stmt = conn.prepare(&format!(
        "{VIEW} WHERE s.identity = 'named' AND s.submitter_mxid = ?1
         ORDER BY s.submitted_on DESC, s.id LIMIT 10"
    ))?;
    let rows = stmt.query_map([mxid], view)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A named submission of `mxid` (nobody else's).
pub fn named_one(conn: &Connection, mxid: &str, id: i64) -> Result<Option<View>> {
    Ok(conn
        .query_row(
            &format!("{VIEW} WHERE s.id = ?1 AND s.identity = 'named' AND s.submitter_mxid = ?2"),
            params![id, mxid],
            view,
        )
        .optional()?)
}

/// A submission that has a deletion code, by ID: only for one whose code
/// was just entered.
pub fn coded_one(conn: &Connection, id: i64) -> Result<Option<View>> {
    Ok(conn
        .query_row(
            &format!(
                "{VIEW} JOIN deletion_credentials d ON d.submission_id = s.id WHERE s.id = ?1"
            ),
            [id],
            view,
        )
        .optional()?)
}

/// Delete the record and its deletion code. Nothing is copied aside; what
/// storage layers below the database keep is outside the bot's control
/// (see `db::configure`).
pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM submissions WHERE id = ?1", [id])? == 1)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::plenums::tests::{at, with_default_rule};

    pub fn new(conn: &Connection, identity: Identity, retention: Retention) -> NewSubmission {
        let topic_id: i64 = conn
            .query_row(
                "SELECT id FROM topics WHERE key = 'cleaning.skipped_turns'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        NewSubmission {
            topic_id,
            problem_id: None,
            location_id: None,
            body: "The bathroom often isn't cleaned when it is someone's turn.".into(),
            suggestion: None,
            identity,
            retention,
            for_plenum: true,
        }
    }

    fn floor(conn: &Connection) -> i64 {
        conn.query_row("SELECT id FROM locations WHERE key = 'floor.3'", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn raw(conn: &Connection, id: i64) -> (Option<String>, Option<i64>, Option<i64>) {
        conn.query_row(
            "SELECT submitter_mxid, submitter_floor_id, delete_after_plenum_id
             FROM submissions WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    /// Every column of every table, as text, to look for the user anywhere.
    fn everything(conn: &Connection) -> String {
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut out = String::new();
        for table in tables {
            let mut stmt = conn.prepare(&format!("SELECT * FROM {table}")).unwrap();
            let n = stmt.column_count();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                for i in 0..n {
                    if let Ok(Some(s)) = row.get::<_, Option<String>>(i) {
                        out.push_str(&s);
                        out.push('\n');
                    }
                }
            }
        }
        out
    }

    #[test]
    fn anonymous_submissions_know_nobody() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let secrets = Secrets::from_master(&[3; 32]);
        let s = new(&conn, Identity::Anonymous, Retention::Keep);
        let done = submit(
            &conn,
            &secrets,
            &s,
            "@alice:x",
            Some(floor(&conn)),
            at(2026, 11, 2, 12).date_naive(),
        )
        .unwrap();
        assert_eq!(raw(&conn, done.id), (None, None, None));
        let token = done.token.unwrap();
        // Neither the user nor the raw code is stored anywhere.
        let all = everything(&conn);
        assert!(!all.contains("@alice:x"));
        assert!(!all.contains(&token));
        assert!(!all.contains(&crate::secrets::normalize_token(&token).unwrap()));
        // The schema itself refuses a user ID on an anonymous submission.
        let sneaky = conn.execute(
            "UPDATE submissions SET submitter_mxid = '@alice:x' WHERE id = ?1",
            [done.id],
        );
        assert!(sneaky.is_err());
    }

    #[test]
    fn floor_submissions_know_the_floor_only_and_named_ones_the_person() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let secrets = Secrets::from_master(&[3; 32]);
        let today = at(2026, 11, 2, 12).date_naive();
        let f = floor(&conn);
        let s = new(&conn, Identity::Floor, Retention::Keep);
        assert!(submit(&conn, &secrets, &s, "@bob:x", None, today).is_err());
        let done = submit(&conn, &secrets, &s, "@bob:x", Some(f), today).unwrap();
        assert_eq!(raw(&conn, done.id), (None, Some(f), None));
        assert!(done.token.is_some());
        assert!(!everything(&conn).contains("@bob:x"));

        let s = new(&conn, Identity::Named, Retention::Keep);
        let done = submit(&conn, &secrets, &s, "@carol:x", Some(f), today).unwrap();
        assert_eq!(raw(&conn, done.id), (Some("@carol:x".into()), None, None));
        assert!(done.token.is_none());
        assert_eq!(named_by(&conn, "@carol:x").unwrap().len(), 1);
        assert!(named_one(&conn, "@bob:x", done.id).unwrap().is_none());
    }

    #[test]
    fn a_code_opens_its_own_submission_only() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let secrets = Secrets::from_master(&[3; 32]);
        let today = at(2026, 11, 2, 12).date_naive();
        let s = new(&conn, Identity::Anonymous, Retention::Keep);
        let one = submit(&conn, &secrets, &s, "@a:x", None, today).unwrap();
        let two = submit(&conn, &secrets, &s, "@a:x", None, today).unwrap();
        let found = by_token(&conn, &secrets, one.token.as_ref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(found.id, one.id);
        assert_eq!(
            by_token(&conn, &secrets, two.token.as_ref().unwrap())
                .unwrap()
                .unwrap()
                .id,
            two.id
        );
        // Wrong, malformed, or right code with another secret: nothing.
        assert!(by_token(&conn, &secrets, "AAAA-BBBB-CCCC")
            .unwrap()
            .is_none());
        assert!(by_token(&conn, &secrets, "hello").unwrap().is_none());
        let other = Secrets::from_master(&[4; 32]);
        assert!(by_token(&conn, &other, one.token.as_ref().unwrap())
            .unwrap()
            .is_none());
        // Deleting removes the row and its code.
        assert!(delete(&conn, one.id).unwrap());
        assert!(by_token(&conn, &secrets, one.token.as_ref().unwrap())
            .unwrap()
            .is_none());
        let codes: i64 = conn
            .query_row("SELECT count(*) FROM deletion_credentials", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(codes, 1);
    }

    #[test]
    fn delete_after_plenum_points_at_the_plenum_entity() {
        let conn = with_default_rule(at(2026, 11, 2, 12));
        let secrets = Secrets::from_master(&[3; 32]);
        let today = at(2026, 11, 2, 12).date_naive();
        let next = plenums::next(&conn).unwrap().unwrap();
        let s = new(&conn, Identity::Anonymous, Retention::DeleteAfterPlenum);
        let done = submit(&conn, &secrets, &s, "@a:x", None, today).unwrap();
        assert_eq!(raw(&conn, done.id).2, Some(next.id));
        assert_eq!(done.plenum_id, next.id);
        let s = new(&conn, Identity::Anonymous, Retention::Keep);
        let kept = submit(&conn, &secrets, &s, "@a:x", None, today).unwrap();
        assert_eq!(raw(&conn, kept.id).2, None);
    }
}
