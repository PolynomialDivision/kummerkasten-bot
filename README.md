# 📮 kummerkasten-bot

The house's feedback box, in Matrix: everyone gets a private, encrypted chat
with the bot to share problems, ideas, wishes and thanks — anonymously if
they like — for the next plenum — to answer short check-ins, to look at
results and to share and support ideas. The day before each plenum it posts
a short summary into the house room.

```
📮 Kummerkasten
🗓️ Next plenum: Sunday, 18 October · 19:00 · in 13 days

What would you like to do? 🌱

📝 Share something
📊 Check-ins · 1 open
💡 Ideas
📈 Results
🔑 Manage submission
🔐 Privacy
```

Everything is a tap on a button (a reaction the bot puts under its
message). Free text is just a message. Every step has ⬅️ Back and ❌ Cancel;
Cancel throws the draft away, clears the conversation from the chat and
shows a fresh menu — no "are you sure?".

## For residents

* **Getting your chat** — write `!kummerkasten` in the house room, or tap
  📮 on the Kummerkasten message there. The bot invites you to your own
  encrypted chat. You keep that one chat; it is tidied after every plenum.
* **Sharing** — topic → (subtopic) → place → your words → *what would you
  change?* (optional) → **how to submit** → **how long to keep it** →
  preview → ✅.
  * 🥷 **Anonymous** — stored without your Matrix account.
  * 🏠 **With my floor** — stored with your floor only (asked once, then
    remembered with your account — never with what you submit). On a floor
    with few people that can give you away; the bot says so.
  * 👤 **With my name** — stored with your Matrix account.
  * ♾️ **Keep it** (for comparisons over time) or 🧹 **Delete after the
    next plenum** (bound to that very plenum, even if it is moved).
* **Deletion codes** — anonymous and floor submissions come with a code like
  `C7MJ-4P2K-XQ7D-9FRT`, shown with ✅ *I've saved the code* and 🏠 *Main menu*.
  ✅ removes that message from the chat; otherwise it stays until the chat
  is cleared after the plenum. 🔑 *Manage submission* + the code shows that
  one submission and deletes it for good (the message you type the code
  into is removed right away). Named submissions are listed there without a
  code.
* **Check-ins** — when admins start one, everyone it is for gets a
  Kummerkasten chat (the bot invites you if you don't have one yet; you get
  the check-in once you accept), and **one** message in it (📊 *Start
  check-in*), and at most **one** reminder (by default a day
  before it closes, only if you haven't answered). The menu shows
  *Check-ins · 1 open*. Questions come one by one — scales (😩 … 😄),
  yes/no, single or multiple choice (tick ☑️, then ➡️), free text —
  optional ones with ⏭️ Skip, some with an optional comment. At the end:
  🥷 / 🏠 / 👤 as for feedback, a preview, ✅. Nothing is stored before ✅;
  ❌ drops the answers.
* **Ideas** — when what you write sounds like an idea (the 💡 topic, *what
  would you change?*, a check-in question asking for suggestions), the bot
  asks: 🌍 **Add to Ideas** · 🗣️ **Only for the plenum** · 🔒 **Keep
  private**. Only 🌍 puts it on the board — as its own entry, without your
  name and without a link to your feedback (which keeps its own identity
  and retention). If a similar idea exists, you choose: 👍 support it, 💬 add
  yours as a comment, or ➕ create yours separately. Nothing is merged.
* **💡 Ideas board** — the shared ideas, most supported first, with their
  status (💭 Suggested · 🗣️ Discussing · 🧪 Trying · ✅ Implemented). 👍 once
  per idea (take it back any time), one 💬 comment per idea (writing again
  replaces it). Nobody sees who supports or comments.
* **📈 Results** — the current round in numbers, then per topic: the latest
  check-in (e.g. *2.8 / 5 · 18 of 27 eligible residents answered*), how
  things changed over time (with measures the plenum tried shown on the
  timeline), recurring problems — 📊 check-in shares and 📮 Kummerkasten
  counts side by side, never added up — and active ideas. ⏪ / ⏩ go through
  earlier check-ins; 🏠 *By floor* where enough people answered; 📚 earlier
  plenums. Never anyone's words.
* **After each plenum** — once it has actually taken place and an admin
  completed it — the chat is cleared, and a fresh menu says a new round has
  started. If a plenum is moved, the round simply stays open until then.
* If you're no longer in any of the house rooms, the chat shows a friendly
  🔒 and nothing else.

## For admins

Admins are `[security] admin_users`. Admin commands only work in an
admin's **encrypted direct chat** with the bot (by default only from a
verified device: `!admin verify`). Their Kummerkasten chat counts as such.

```
!kk help
!kk rooms add !abc:example.org House     authorize a room (then invite the bot)
!kk rooms remove !abc:example.org
!kk entry !abc:example.org               post the 📮 entry message there
!kk invite-all                           open chats for everyone in the rooms

!kk plenums                              next plenums with their numbers
!kk plenum add 2026-11-14 18:00          an extra one
!kk plenum move 12 2026-11-14 18:00 because room booked
                                         only this one, at any notice; everything stays attached
!kk plenum cancel 12 · !kk plenum remove 12
!kk plenum complete 12                   it took place: close the round, delete, clear chats
!kk plenum reopen 12                     undo a completion by mistake
!kk rule                                 the rhythm (with its history)
!kk rule set sun 1,3,5 19:00 [starting 2026-12-01]
!kk rule set sun 19:00 · !kk rule set every 2 sun 19:00 from 2026-11-01
!kk round                                the current round in numbers

!kk topics · !kk locations · !kk problems
!kk topic add kitchen.dishes 🍽️ Dishwasher under kitchen problem kitchen.dishes
!kk location add floor.5 5️⃣ 5th floor floor
!kk problem add kitchen.dishes kitchen Dishwasher not emptied
!kk topic rename <key> [emoji] <label> · retire <key> · restore <key>

!kk ideas · !kk idea 3 · !kk idea 3 status discussing · rename · topic
!kk idea add cleaning Earlier cleaning reminders      one raised at the plenum
!kk measures
!kk measure add cleaning Send reminders 24h earlier problem cleaning.skipped_turns from 2026-10-19 status trying
!kk measure 2 status adopted · describe <text> · from <date> · until <date> · plenum 42
!kk summary preview                      the day-before summary, as it would read now

!admin settings · !admin set privacy_threshold 5
```

Check-ins:

```
!kk surveys                                    templates
!kk survey new kitchen 🍳 Kitchen organisation topic kitchen
!kk survey cleaning                            questions, numbered, with versions
!kk survey cleaning add scale metric cleaning.overall_satisfaction How well is cleaning working?
!kk survey cleaning add multiple optional How was it?   (then: choice <n> add …)
!kk survey cleaning add text optional suggestion What would you change?
!kk survey cleaning choice 2 add problem cleaning.skipped_turns 🙋 Cleaning turns are skipped
!kk survey cleaning edit 1 How satisfied are you with cleaning?
!kk survey cleaning move 3 1 · disable 2 · optional 1 · comment 1 on · preview
!kk metrics · !kk metric add kitchen.tidiness kitchen How tidy the kitchen is (1–5)
!kk rooms central !abc:example.org             where `announce` posts

!kk checkin start cleaning                     plan it; shows a preview
!kk checkin start cleaning room !k2:example.org closes 2026-11-14 18:00 no-reminder announce
!kk checkin start cleaning closes 1d-before          a day before the plenum, moving with it
!kk checkin start cleaning floor floor.2 only anonymous,floor no-plenum
!kk checkin go · !kk checkin drop              start it / forget the plan
!kk checkins · !kk checkin close 4
```

Editing a question that a check-in already asked creates a **new version**
(the bot says so); earlier check-ins keep showing and storing exactly what
was asked. A question's *kind* can't change — that's a new question. If the
meaning of a question changes, point it at a new metric.

Settings: `privacy_threshold` (results, default 3, never below 2),
`session_minutes` (an unfinished draft is tidied away, default 60),
`reminder_hours` (the one check-in reminder, hours before it closes, default
24), `summary_hours_before` (the plenum summary, default 24),
`auto_complete` (default **off**: a plenum is completed by an admin; on, it
is completed at `complete_at`, default 23:59, on its day).

## Plenums and rounds

```
rule (versioned)  →  plenums (one row per date, stable ID)  →  changes per row
1st/3rd/5th Sunday   #41 Sun 1 Nov · #42 Sun 15 Nov · …      moved / cancelled / completed
```

* The starting rule is the 1st, 3rd and (where there is one) 5th Sunday of
  each month at `plenum_time` — stored as data on first start, changeable
  with `!kk rule set` (each change is a new version from a date on).
* Plenums exist as rows ~4 months ahead. A row is never generated twice, so
  moving or cancelling one occurrence sticks; a rule change replaces only
  future occurrences nobody moved. Past plenums are never recomputed.
* A **round** collects everything for one plenum: exactly one is open, and
  it belongs to the next scheduled plenum. Cancel a plenum, and the round
  (with its "delete after the next plenum" submissions and its check-ins)
  carries on to the next one.
* **Moving a plenum is a normal operation**, at any notice — even on its day
  or after its time passed without it happening. The row keeps its ID, so
  its round, *delete after* submissions, check-ins, measures and
  announcements stay with it; `original_starts_at` keeps the first date,
  `starts_at` the current one, with an optional reason. Everything derived
  from the date follows the current one: check-in deadlines set relative to
  the plenum (and their reminder), the day-before summary, completion. A
  move may not pass another scheduled plenum (that would hand the round to
  it) — cancel instead. Nothing is scheduled as a stored job: the bot works
  from the plenum's current date every minute, so nothing can still run
  for an old date.
* **A date passing is not the plenum happening.** After its time a plenum
  is *awaiting completion*: the round stays open, nothing is deleted or
  cleared. Only `!kk plenum complete` (or the `auto_complete` policy, if an
  admin turns it on) completes it, and then in one transaction: the plenum
  is completed, its round closed, *delete after* submissions deleted, the
  chats' events marked for redaction, drafts dropped, the next round
  opened. Then the bot redacts and posts "🧹 Previous round cleared · 🌱 A
  new round has started" + menu into each chat.
* **📮 The day-before summary** goes to the central room(s)
  `summary_hours_before` before the plenum's *current* start, once: counts
  (feedback, check-in responses, ideas), the most mentioned topics and
  recurring issues (only with at least `privacy_threshold` behind them),
  popular ideas and ideas under discussion — with 📮 📈 💡 to open the
  chat. A cancelled plenum gets none. If an announced plenum moves, a short
  update follows (`~~Sunday~~ ➡️ Monday`, "the round stays open") and the
  posted summary is edited in place; a move by more than three days also
  gets a fresh summary the day before the new date. If it is cancelled, the
  update says so and where the round goes.

## Check-ins

```
survey_templates ─┬─ questions ── question_versions ── choice_versions
   (editable)     │      └─ choices ─────────────────────────┘
                  └─ survey_runs ── survey_run_questions ──→ question_versions
                      (a snapshot)
participations (run, HMAC key)        responses (UUID) ── answers ── answer_choices
        nothing links these two ↔
```

* **Canonical metrics** (`cleaning.overall_satisfaction`) say what a
  question measures; **canonical problems** (`cleaning.skipped_turns`) what
  a choice (or a yes/no question) stands for — the same problems free
  feedback points at through its subtopic. Wording can change; the keys
  stay. Feedback and check-ins remain different signals: 📈 Results show
  them side by side, never as one number.
* A **run** points at the exact question versions asked, in order; their
  choice versions come with them. Triggers make used versions, and a run's
  question list, impossible to change or delete.
* The **target** is stored as a rule (whole house · members of one
  authorized room · residents who said they live on a floor) and resolved
  against current membership whenever someone is notified, starts,
  continues or submits.
* **Deadlines** are fixed (`closes 2026-11-14 18:00`: never moves) or
  relative to the plenum (the default "until the plenum starts", or
  `closes 1d-before`): those, and their reminder, move when the plenum moves
  or the round moves on to another plenum. A reminder already sent stays
  sent.
* Notices, reminders and announcements go out **per run in one batch**,
  marked sent before sending (a crash means a missed message, never a
  second one). Each person is checked right before: access, target, not
  answered yet.
* **One chat per eligible resident.** Before a check-in's notices go out,
  the bot lists who is in the authorized rooms, checks each again with the
  server, keeps those the check-in is for, and creates the encrypted chat
  for anyone who has none (reusing existing ones and pending invitations —
  never a second one), waiting out the homeserver's rate limits. Someone
  joining an authorized room while a check-in for them is open gets a chat
  too. An invitation isn't being told: residents get the notice of every
  open check-in for them when they accept. `!kk invite-all` does the same
  for everyone.
* **Response rates** use the number of residents eligible when the
  check-in started (counted then, stored as a number only): *18 of 27
  eligible residents answered*. People who become eligible later can still
  answer but don't change the denominator; nothing maps people to runs.
* **Indexes for results**: answers by question version and value,
  selected choices by choice version, question versions by metric and
  problem, choice versions by problem, responses by run and floor, runs by
  plenum/round.

## Privacy and security

**What is stored** (SQLite, `store/kummerkasten.sqlite`):

| | anonymous | floor | named |
|---|---|---|---|
| text, topic, place, retention, day | ✅ | ✅ | ✅ |
| floor | — | ✅ | — |
| Matrix ID | — | — | ✅ |
| deletion code | HMAC only | HMAC only | — |

* The table's `CHECK` constraints enforce this — code can't store more.
* Submission IDs are random and dates are kept to the **day**, so neither
  orders nor times submissions.
* The draft (Matrix ID + text) lives in `sessions` while you write; it is
  deleted in **the same transaction** that stores the submission.
* Which submission you're managing after entering a code is kept **in
  memory only**, for 15 minutes. Wrong codes: 5 per 15 minutes.
* Deletion codes: 80 random bits (16 Crockford Base32 characters from the
  OS's random generator, `XXXX-XXXX-XXXX-XXXX`; codes handed out before,
  with 12 characters, still work), stored as HMAC-SHA256 with a server key
  (`store/kummerkasten.key`, 0600, created on first start). Never stored or
  logged in clear. Without the key a database copy can't even test guesses;
  looking one up compares HMACs, so timing can't help guess a code.
* 🗑️ Deleting deletes the database record (and its code), and the bot
  tries not to keep unnecessary copies: nothing goes to a "deleted" table,
  and SQLite's `secure_delete` is on as an extra measure.
  ⚠️ That is not guaranteed physical erasure — the WAL file until its next
  checkpoint, the filesystem, the storage hardware, snapshots and backups
  may still hold older data.
* **Logs** never contain feedback, codes, or anything that ties a log line
  to a submission: "submission created round=27" — no ID, no topic, no
  identity, because the time of a log line plus the homeserver's knowledge
  of who wrote to the bot when could otherwise undo anonymity. Likewise
  "check-in response stored run=4".

**Check-in answers**

| | anonymous | floor | named |
|---|---|---|---|
| answers | ✅ | ✅ | ✅ |
| floor | — | ✅ | — |
| Matrix ID | — | — | ✅ |

* Enforced by `CHECK` constraints, as for submissions.
* *That* someone answered is `HMAC(secret, run ‖ Matrix ID)` in
  `participations`; *what* they answered is a response with a random UUID.
  Neither table has a timestamp or a rowid (no insertion order), and no
  column, key or foreign key connects them. With the key file one can test
  "has this account answered run 4?" — not "which response is theirs".
* The draft lives in `sessions` while answering and is deleted in the same
  transaction that stores the response.
* **Delivery records are minimal and temporary.** `checkin_messages` maps a
  notice's or reminder's event to its run — no room, so it can't say whose
  chat got a reminder (and so who hadn't answered by then); its rows go
  when the run closes. Kept for good: only counts on the run (eligible at
  launch, notices sent, reminders sent). Chat events are tracked only to
  redact them after the plenum, with the day (not the time) they were sent.
  The homeserver itself of course still has the messages it delivered until
  they are redacted.

**Results**

* Computed when asked, in SQL, from what is stored *now*: a deleted
  submission (by its author, or *delete after the plenum*) is in no
  statistic afterwards. No caches, no shadow totals; kept feedback keeps
  counting over time.
* Comparisons only through canonical keys: one metric, or one problem
  within one metric, across question versions. Different metrics are never
  merged; each point remembers the exact question version.
* Spontaneous feedback (counts of submissions) and check-ins (shares of
  respondents) are shown side by side and never combined.
* **Privacy threshold**: every value describing a group needs at least
  `privacy_threshold` responses. Breakdowns (by floor; "not stated" is a
  group too) hide small groups *and then more*, until the hidden ones
  together reach the threshold — otherwise "whole house minus the visible
  floors" would give a hidden floor away (*🔒 Not enough responses to show
  this breakdown*). Small counts read "fewer than 3".
* **No live differencing**: check-in results appear once a check-in has
  closed, and counts of the running round leave out today — watching a
  number change right after someone wrote can't tell what they wrote.
* Free-text answers and feedback never appear in results.

**Ideas**

* An idea exists only after its author tapped 🌍. It has no author column
  and no link to the submission or response it came from.
* 👍 and 💬 are keyed per account with `HMAC(secret, idea ‖ Matrix ID)` — no
  time, no row order: enough to stop double support and to show you that
  you support it, never a list of supporters.

**Access**

* Who may use it: members of the authorized rooms, asked of the homeserver
  **before every action** (never cached). Errors count as "no". Leaving all
  rooms locks the chat at the next tap.
* Every message or tap in a chat is only handled if the chat is still
  exactly the bot and that resident, both joined, and end-to-end encrypted
  (fresh `/state`), and the event itself arrived encrypted.
* Chats are created by the bot: encrypted, room version 12 when the server
  supports it as stable (else 11, 10, else the server's default — never "the
  highest number"), and only the bot may invite, change state or kick.
* Nobody but admins may invite the bot; `"all"` in the invite lists is
  refused at startup. Rooms are authorized by admins at runtime.
* Matrix events are handled once (`processed_events`), so a re-delivery
  after a restart doesn't run anything twice.

**Honest limits**

* **Redaction is not erasure.** It removes content for clients that follow
  the spec, but homeservers keep redacted events' metadata (and may keep
  content for a while), backups keep what they had, and devices may have
  cached messages. The bot says so in 🔐 Privacy.
* **The bot sees what you send** while processing it, as does anyone who
  controls the bot's host. Anonymity is towards the people reading the
  feedback, not towards the server operator in every possible case.
* **Backups** of `store/` contain whatever was in the database at that time
  — including feedback deleted since. Keep backups short-lived, and keep
  `kummerkasten.key` out of the same backup if you can (without it, codes in
  a stolen database can't be tested).
* **The homeserver knows who writes to the bot and when.** That is why
  submissions carry no time and logs no submission details.
* **Small groups**: floor submissions on a floor with two people aren't
  anonymous; results hide groups below `privacy_threshold` (and what would
  reveal them).

## Development

```
cargo test
cargo clippy --all-targets
```

`mxbot-common` gained a runtime room allow-list (`rooms::DynamicRooms`),
`private` (private chats, fresh membership checks, room version choice) and
`send::classify_error` (rate limits for requests other than sends).
Until that is pushed, `Cargo.toml` patches it to `../mxbot-common` — remove
the `[patch]` section after pushing and `cargo update -p mxbot-common`
(Docker builds can't see `../`).

Modules: `flow` (state machine, pure) · `screens` (texts and buttons) ·
`engine` (flow + database) · `dm` (Matrix side) · `plenum` (rules) ·
`plenums` (occurrences, rounds) · `lifecycle` (completion) · `submissions` ·
`catalog` · `surveys` (templates, versions) · `checkins` (runs, answers,
delivery, deadlines) · `reconcile` (one chat per eligible resident) ·
`stats` (aggregates, privacy policy) · `results` (texts) · `ideas` ·
`measures` · `summary` (day-before summary, updates) · `access` · `secrets` ·
`scheduler` · `admin` · `admin_checkins` · `admin_results` · `store` · `db`
(migrations in `migrations/`, applied once each, in order, each in a
transaction).

## Roadmap

* ✅ **M2 — check-ins** (see above), with rescheduling as a normal
  operation, completion only when a plenum took place, one chat per
  eligible resident, minimal delivery records and 80-bit deletion codes.
* ✅ **M3 — results, ideas, measures, plenum summaries** (see above).
* Later: public excerpts of free text (only with explicit rules and
  consent); breakdowns by kitchen or room (the same suppression applies);
  an optional dedicated Kummerkasten room for richer dashboards.
