# 📮 kummerkasten-bot

The house's feedback box, in Matrix: everyone gets a private, encrypted chat
with the bot to share problems, ideas, wishes and thanks (anonymously if
they like) for the next plenum, to answer short check-ins, to look at
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
shows a fresh menu. No "are you sure?". `!help` explains it in five lines
(and shows the step you were on again), `!menu` starts over.

**One screen.** The bot's current message is the screen. Each tap changes
it in place, with its buttons brought in line (new ones added, those that
no longer fit taken away, in the order of the text), so the chat doesn't
fill up with old menus. When something was said after it (your text, a
note, a receipt), the next step comes as a fresh screen at the bottom and
the old one is removed. Receipts (✅ stored, the deletion code) are never
edited.

A pressed button is released (the bot takes your reaction away) once the
screen has changed, so the same button works again on the next step. If
the bot can't handle a tap or a message, it says so in a short note instead
of staying silent. And if the screen is ever gone (removed by a tidy-up or
a restart in the middle of a step), the next message or tap brings it
back, below the answer, so there is always something to tap on.

**The screen is confirmed, not assumed.** A step is saved together with a
"not shown yet" mark that is cleared once the screen was edited or sent
with all its buttons. If the bot crashed or the edit failed in between,
the chat may still show the step before. Then the first tap doesn't act on
the step you can't see: it shows the current one. A typed message is
answered as the flow stands, with the current screen below it. At startup
every unconfirmed screen is shown again, and `!kk round` counts them. The
bot's own buttons (its reactions) are tracked like edits: they go with the
screen when it is removed, and a button that couldn't be taken away is
queued for the redaction worker instead of being forgotten.

A tap also only counts once the bot has seen its own message (or edit)
showing the current step come back through sync. Matrix delivers a room's
events in order, so a tap that arrives before that echo was made before
the step could be seen: a second device, a very fast double tap. It shows
the step again instead of acting on it. When a step is edited, the bot asks
the server which of its reactions are really under the message, rather
than trusting its own ledger, so a reaction whose removal is still waiting
or one a crash left untracked is seen and dealt with, never fought with.
And a button an admin retired while you were looking at the list (a topic
taken away) refreshes the list when tapped instead of doing nothing.

## Survey-only mode (the default for now)

`[kummerkasten] ui = "survey"` keeps the bot to one thing: the check-ins
(surveys) admins start. Everything else stays in the code and the database
and comes back with `ui = "full"` and a restart.

**What a resident sees.** When a check-in starts, one message from the bot:
the check-in itself (title, what it is about, how many questions, open
until when, a privacy line, ▶️ *Start*). Then one question at a time, in
place: tap an answer (or type its number), write when asked, ⬅️ to go back,
⏭️ to skip an optional one. At the end 🥷 / 🏠 / 👤 as everywhere, a preview,
✅. Then a short *Thanks!* and nothing more. No menu, no introduction, no
offer to share an answer as an idea, no ❌ (stopping is simply not
answering).

* **From another homeserver.** Someone from matrix.org who accepts their
  chat gets the check-in 20 seconds later, not at once: sent in the same
  second, it never reached their server (2026-10-20). `!kk checkin remind`
  brings it to those who got an empty chat before that.
* **Pausing is fine.** A half-answered check-in waits until it closes, also
  across restarts; there is no 60-minute timeout here. The one reminder is a
  short note with their current step right below it.
* **One at a time.** While one check-in is half done, another one's message
  waits; it comes up right after the first is submitted (or has closed).
* **Never twice.** Answered is answered: no notice, no reminder, and 📊 on an
  old notice says so.
* **Quiet otherwise.** A message while nothing is open gets one line
  (*Nothing to answer right now*); a tap on something old does nothing.
  `!help` explains the four rules; `!menu` shows the current step again.
* **Switching on.** At the first start in this mode, what the full
  Kummerkasten left in the chats (menus, unfinished feedback drafts, notes)
  is tidied away. A deletion-code receipt stays (it holds their code), its
  buttons go. Check-ins already being answered carry on.
* **Turned off for now:** the 📮 entry message and `!kummerkasten` (they only
  open a chat when a check-in is waiting), the menu with feedback, ideas and
  results, plenum summaries, and `!kk entry` / `!kk invite-all`.

**For admins** `!kk help` shows just this:

```
📎 a survey file (.toml)            upload it here: stored (see below)
!kk survey load house_cleaning      one from store/surveys/ or shipped
!kk survey cleaning                 the questions, numbered
!kk survey cleaning preview         each question as residents see it
!kk checkin start cleaning          what would happen; then
!kk checkin go                      start it (or !kk checkin drop)
!kk checkins                        open and closed ones, how many answered
!kk checkin remind 4                its reminder now (the planned one still comes)
!kk checkin close 4                 end it now (otherwise it closes by itself)
!kk checkin results 4               the answers, once it is closed
```

Starting the same survey for the same people while it is still open is
refused. `!kk checkin results` shows nothing but the count while a check-in
is open (watching answers arrive one by one could tell whose is whose),
nothing below `privacy_threshold` answers, and written answers only from
that many on, sorted, never in the order they came, never with a name.
`!kk help all` lists every command.

**Surveys as files.** A whole survey can be one TOML file, the questions
one below the other, no numbers to keep track of:

```toml
key = "house_cleaning"
emoji = "🧹"
title = "House cleaning"
topic = "cleaning"                 # optional
intro = "We'd like to know …"      # optional

[[question]]
single = "🧼 How happy are you with how clean the house is?"
choices = ["😍 Very happy", "🙂 Fairly happy", "😐 It's okay"]
other = "✏️ Something else"        # optional: they write their own

[[question]]
text = "🌟 If you could wish for one thing …"
optional = true
```

A question is one of `single`, `multiple`, `yes_no`, `scale` or `text`.
Optional per question: `optional`, `comment` (a comment field), `suggestion`
(may become an idea), `metric`, `problem`, `min`/`max` (scales). A choice
is `"emoji label"`, or `{ label = "emoji label", problem = "key" }`.

```
📎 upload the .toml in your chat with the bot    stores it
!kk survey load                                  the files there are
!kk survey load house_cleaning                   store/surveys/house_cleaning.toml,
                                                 else the one that comes with the bot
!kk survey house_cleaning export                 the survey back as a file
```

A file is checked completely before anything is stored, and is stored as
a whole or not at all; a mistake is answered with the question it is in.
A survey that was never started is replaced by a file with its key; one
that was started stays as it is (its results stay comparable), so the file
needs another key then. An uploaded `.txt` is run as a list of `!kk`
commands, as if sent as one message (up to 120).

A ready-made house cleaning survey (12 questions, English) comes with the
bot: `surveys/house_cleaning.toml`, loaded with `!kk survey load
house_cleaning`. A test applies it on a fresh database and exports it
again, so it is known to build, start and show its emoji buttons.

**Trying it out first.** With `mode = "test"` the bot uses its own files
(`kummerkasten-test.sqlite`, `kummerkasten-test.key`, `settings-test.json`)
and a check-in goes only to the admin who starts it. To start the test
over from nothing: stop the bot, delete those three files, start it again.
Live data is never touched; in live mode the survey is loaded once more.

## For residents

* **Getting your chat**: write `!kummerkasten` in the house room, or tap
  📮 on the Kummerkasten message there. The bot invites you to your own
  encrypted chat. You keep that one chat; it is tidied after every plenum.
* **Sharing**: topic → (subtopic) → place → your words → *what would you
  change?* (optional) → **how to submit** → **how long to keep it** →
  preview → ✅.
  * 🥷 **Anonymous**: stored without your Matrix account.
  * 🏠 **With my floor**: stored with your floor only (asked once, then
    remembered with your account, never with what you submit). On a floor
    with few people that can give you away; the bot says so.
  * 👤 **With my name**: stored with your Matrix account.
  * ♾️ **Keep it** (for comparisons over time) or 🧹 **Delete after the
    next plenum** (bound to that very plenum, even if it is moved).
* **Deletion codes**: anonymous and floor submissions come with a code like
  `C7MJ-4P2K-XQ7D-9FRT`, shown with ✅ *I've saved the code* and 🏠 *Main menu*.
  ✅ removes that message from the chat; otherwise it stays until the chat
  is cleared after the plenum. 🔑 *Manage submission* + the code shows that
  one submission and deletes it for good (the message you type the code
  into is removed right away). Named submissions are listed there without a
  code.
* **Check-ins**: when admins start one, everyone it is for gets a
  Kummerkasten chat (the bot invites you if you don't have one yet; you get
  the check-in once you accept), and **one** message in it (📊 *Start
  check-in*), and at most **one** reminder (by default a day
  before it closes, only if you haven't answered). The menu shows
  *Check-ins · 1 open*. Questions come one by one: scales (😩 … 😄),
  yes/no, single or multiple choice (tick ☑️, then ➡️), free text;
  optional ones with ⏭️ Skip, some with an optional comment. At the end:
  🥷 / 🏠 / 👤 as for feedback, a preview, ✅. Nothing is stored before ✅;
  ❌ drops the answers.
* **Ideas**: when what you write sounds like an idea (the 💡 topic, *what
  would you change?*, a check-in question asking for suggestions), the bot
  asks: 🌍 **Add to Ideas** · 🗣️ **Only for the plenum** · 🔒 **Keep
  private**. Only 🌍 puts it on the board, as its own entry, without your
  name and without a link to your feedback (which keeps its own identity
  and retention). If a similar idea exists, you choose: 👍 support it, 💬 add
  yours as a comment, or ➕ create yours separately. Nothing is merged.
* **💡 Ideas board**: the shared ideas, most supported first, with their
  status (💭 Suggested · 🗣️ Discussing · 🧪 Trying · ✅ Implemented). 👍 once
  per idea (take it back any time), one 💬 comment per idea (writing again
  replaces it). Nobody sees who supports or comments.
* **📈 Results**: the current round in numbers, then per topic: the latest
  check-in (e.g. *2.8 / 5 · 18 of 27 eligible residents answered*), how
  things changed over time (with measures the plenum tried shown on the
  timeline), recurring problems (📊 check-in shares and 📮 Kummerkasten
  counts side by side, never added up) and active ideas. ⏪ / ⏩ go through
  earlier check-ins; 🏠 *By floor* where enough people answered; 📚 earlier
  plenums. Never anyone's words.
* **After each plenum** (once it has actually taken place and an admin
  completed it) the chat is cleared, and a fresh menu says a new round has
  started. If a plenum is moved, the round simply stays open until then.
* If you're no longer in any of the house rooms, the chat shows a friendly
  🔒 and nothing else.

## Test mode and rooms from the config

```toml
[kummerkasten]
mode = "test"                 # or "live" (default)

[[kummerkasten.rooms]]
id = "!house:example.org"
label = "House"
central = true                # the day-before summary, `announce`
entry = true                  # the 📮 entry message, posted once
```

* **`mode = "test"`** tries everything out without the house noticing:
  * its own files (`kummerkasten-test.sqlite`, `-test.key`,
    `settings-test.json`); the live database is never opened. Switching
    back to `live` starts where live was; the test files can be deleted;
  * only `admin_users` have access: nobody else gets a chat, whatever room
    they are in;
  * **every check-in goes only to the admin who starts it** (`!kk checkin
    start …` shows the target it would have live);
  * every message starts with 🧪 **Test**.
* **`[[kummerkasten.rooms]]`**: the authorized rooms, synced at every start
  (rooms not listed are deactivated); `!kk rooms add/remove/central` are
  refused. With `entry = true` the 📮 entry message is posted once into the
  room, as soon as I'm in it; `!kk entry` posts another. Without the list,
  admins manage the rooms with the commands below.

## For admins

Admins are `[security] admin_users`. Admin commands only work in an
admin's **encrypted direct chat** with the bot (by default only from a
verified device: `!admin verify`). Their Kummerkasten chat counts as such.
One message may carry several commands, one `!kk` line each (other lines
continue the command above); they run in order and stop at the first that
fails.

```
!kk help
!kk rooms add !abc:example.org House     authorize a room (then invite the bot)
!kk rooms remove !abc:example.org
!kk entry !abc:example.org               post the 📮 entry message there
!kk invite-all                           open chats for everyone in the rooms

!kk plenums                              next plenums with their numbers, and the calendar's last read
!kk plenum complete 12                   it took place: close the round, delete, clear chats
!kk plenum reopen 12                     undo a completion by mistake
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
!kk survey cleaning title 🧹 Cleaning check-in       (title of new check-ins)
!kk survey cleaning add scale metric cleaning.overall_satisfaction How well is cleaning working?
!kk survey cleaning add multiple optional How was it?   (then: choice <n> add …)
!kk survey cleaning add text optional suggestion What would you change?
!kk survey cleaning choice 2 add problem cleaning.skipped_turns 🙋 Cleaning turns are skipped
!kk survey cleaning choice 2 emoji 3 🐢              (a choice's button; warns about look-alikes)
!kk survey cleaning edit 1 How satisfied are you with cleaning?
!kk survey cleaning move 3 1 · disable 2 · optional 1 · comment 1 on · preview
!kk metrics · !kk metric add kitchen.tidiness kitchen How tidy the kitchen is (1–5)
!kk metric rename kitchen.tidiness How tidy the kitchens are (1–5)
!kk rooms central !abc:example.org             where `announce` posts

!kk checkin start cleaning                     plan it; shows a preview
!kk checkin start cleaning room !k2:example.org closes 2026-11-14 18:00 no-reminder announce
!kk checkin start cleaning closes 1d-before          a day before the plenum, moving with it
!kk checkin start cleaning floor floor.2 only anonymous,floor no-plenum
!kk checkin start cleaning opens 2026-11-10 18:00 closes 2026-11-14 20:00   planned: invites then
!kk checkin go · !kk checkin drop              start it / forget the plan
!kk checkins · !kk checkin close 4
```

Editing a question that a check-in already asked creates a **new version**
(the bot says so); earlier check-ins keep showing and storing exactly what
was asked. A question's *kind* can't change. That's a new question. If the
meaning of a question changes, point it at a new metric.

Settings: `privacy_threshold` (results, default 3, never below 2),
`session_minutes` (an unfinished draft is tidied away, default 60),
`reminder_hours` (the one check-in reminder, hours before it closes, default
24), `summary_hours_before` (the plenum summary, default 24),
`auto_complete` (default **off**: a plenum is completed by an admin; on, it
is completed at `complete_at`, default 23:59, on its day).

## Plenums and rounds

```
Nextcloud calendar (plenum-bot's)  →  plenums (one row per date, stable ID)  →  rounds
events, series, moved / deleted       #41 Sun 1 Nov · #42 Sun 15 Nov · …        one open
```

```toml
[kummerkasten.calendar]
account_url = "https://cloud.example.org/remote.php/dav/calendars/kummerkasten-bot/"
calendar = "plenum-house"         # plenum-bot's calendar, shared with this account
username = "kummerkasten-bot"
password_env = "CALDAV_PASSWORD"  # the env var with the app password (default)
refresh_minutes = 5               # default
```

The timezone is `[kummerkasten] timezone`. The app password is only ever
read from the environment variable, never from the config.

* **The dates come only from the Nextcloud calendar** of the house plenum,
  which plenum-bot owns (`plenum-house`) and shares read-only with this
  bot's own Nextcloud account (there it is `plenum-house_shared_by_plenum-bot`;
  either name is found). The bot keeps no schedule of its own: there are no
  rules and no commands to add, move or cancel a plenum. Do that in the
  calendar; `!kk plenum add/move/cancel/remove` and `!kk rule` only point
  there. `!kk plenums` says when the calendar was last read, and why not if
  a read failed.
* Every `refresh_minutes` (and once at startup, before anything is
  announced) the bot reads the calendar from yesterday to ~4 months ahead
  over CalDAV. Every timed occurrence keeps the ID the calendar gives it
  (the event's UID; in a series UID and the date it was first due), and its
  row is linked to it (`calendar_id`):
  * an occurrence with a row: the row follows it. Moved: the same row moves
    (see below). Deleted from the calendar: a future plenum is cancelled.
    Back in the calendar: scheduled again.
  * an occurrence without a row adopts a row from before the calendar (the
    old rules) at the same time, else on the same day; else it is a new
    plenum. One that is already over is not created after the fact.
  * future rows from before the calendar that no occurrence took over were
    the old schedule: they are cancelled (and deleted when nothing points at
    them).
  * completed plenums, and past ones that stay past, are never rewritten.
* **A read that fails changes nothing**, and neither does one that finds no
  plenum ahead: an emptied or re-made calendar must not cancel the plenum
  the open round collects for (the log warns). Without a
  `[kummerkasten.calendar]` (or without its password) the bot has no dates
  and leaves the plenums it knows as they are.
* A **round** collects everything for one plenum: exactly one is open, and
  it belongs to the next scheduled plenum. A plenum cancelled in the
  calendar hands the round (with its "delete after the next plenum"
  submissions and its check-ins) on to the next one.
* **Moving a plenum is a normal operation**, at any notice, even on its day
  or after its time passed without it happening. The row keeps its ID, so
  its round, *delete after* submissions, check-ins, measures and
  announcements stay with it; `original_starts_at` keeps the first date,
  `starts_at` the current one. Everything derived from the date follows the
  current one: check-in deadlines set relative to the plenum (and their
  reminder), the day-before summary, completion. A move past another
  scheduled plenum hands the open round (with its *delete after*
  submissions and check-ins) to the one that now comes first, as a
  cancellation does. Nothing is scheduled as a stored job: the bot works
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
  popular ideas and ideas under discussion, with 📮 📈 💡 to open the
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
  a choice (or a yes/no question) stands for, the same problems free
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
* **Planned starts** (`opens 2026-11-10 18:00`, 09:00 without a time): the
  check-in is stored at `!kk checkin go`, but nobody is told, sees or can
  answer it before then; the invitations go out at that time (the bot
  checks every minute). `!kk checkins` shows it as planned, and
  `!kk checkin close <n>` cancels it. The deadline needs to be at least an
  hour after it opens; the reminder comes after it opens.
* Notices, reminders and announcements go out **per run in one batch**,
  queued in the **outbox** in the transaction that marks them due. The
  outbox decides who gets it when it goes out, checks each person right
  before (access, target, not answered yet), retries what failed (1, 2, 4 …
  minutes, then every 6 hours, never given up: only the sender drops what
  isn't needed any more) and sends each message with its own Matrix
  transaction ID, so a crash neither loses nor doubles it. The plenum
  summary and its updates go the same way. `!kk round` shows how many are
  waiting and how many have been failing for hours.
* **One chat per eligible resident.** Before a check-in's notices go out,
  the bot lists who is in the authorized rooms, checks each again with the
  server, keeps those the check-in is for, and creates the encrypted chat
  for anyone who has none (reusing existing ones and pending invitations,
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
| floor | ❌ | ✅ | ❌ |
| Matrix ID | ❌ | ❌ | ✅ |
| deletion code | HMAC only | HMAC only | ❌ |

* The table's `CHECK` constraints enforce this. Code can't store more.
* Submission IDs are random and dates are kept to the **day**, so neither
  orders nor times submissions.
* The draft (Matrix ID + text) lives in `sessions` while you write; it is
  deleted in **the same transaction** that stores the submission.
* Which submission you're managing after entering a code is kept **in
  memory only**, for 15 minutes. Wrong codes: 5 per 15 minutes.
* Deletion codes: 80 random bits (16 Crockford Base32 characters from the
  OS's random generator, `XXXX-XXXX-XXXX-XXXX`; codes handed out before,
  with 12 characters, still work), stored as HMAC-SHA256 with a server key
  (`store/kummerkasten.key`, 0600, created on first start; once the
  database holds codes or check-in answers, a missing key file stops the
  bot instead of being replaced). Never stored or
  logged in clear. Without the key a database copy can't even test guesses;
  looking one up compares HMACs, so timing can't help guess a code.
* 🗑️ Deleting deletes the database record (and its code), and the bot
  tries not to keep unnecessary copies: nothing goes to a "deleted" table,
  and SQLite's `secure_delete` is on as an extra measure.
  ⚠️ That is not guaranteed physical erasure: the WAL file until its next
  checkpoint, the filesystem, the storage hardware, snapshots and backups
  may still hold older data.
* **Logs** never contain feedback, codes, or anything that ties a log line
  to a submission: "submission created round=27". No ID, no topic, no
  identity, because the time of a log line plus the homeserver's knowledge
  of who wrote to the bot when could otherwise undo anonymity. Likewise
  "check-in response stored run=4".

**Check-in answers**

| | anonymous | floor | named |
|---|---|---|---|
| answers | ✅ | ✅ | ✅ |
| floor | ❌ | ✅ | ❌ |
| Matrix ID | ❌ | ❌ | ✅ |

* Enforced by `CHECK` constraints, as for submissions.
* *That* someone answered is `HMAC(secret, run ‖ Matrix ID)` in
  `participations`; *what* they answered is a response with a random UUID.
  Neither table has a timestamp or a rowid (no insertion order), and no
  column, key or foreign key connects them. With the key file one can test
  "has this account answered run 4?", not "which response is theirs".
* The draft lives in `sessions` while answering and is deleted in the same
  transaction that stores the response.
* **Delivery records are minimal and temporary.** `checkin_messages` maps a
  notice's or reminder's event to its run. No room, so it can't say whose
  chat got a reminder (and so who hadn't answered by then); its rows go
  when the run closes. The outbox has one row per chat (every chat, not
  only those who haven't answered) only while a batch goes out, and
  forgets the whole batch once it is sent. Kept for good: only counts on the run (eligible at
  launch, notices sent, reminders sent). Chat events are tracked only to
  redact them after the plenum, with the day (not the time) they were sent.
  The homeserver itself of course still has the messages it delivered until
  they are redacted. A redaction that fails is tried again with a growing
  wait (up to 6 hours) and is never given up while the message may still
  be there; `!kk round` shows how many keep failing. When a resident leaves
  their chat, the bot redacts what it tracked there first and leaves the
  room afterwards.

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
  together reach the threshold. Otherwise "whole house minus the visible
  floors" would give a hidden floor away (*🔒 Not enough responses to show
  this breakdown*). Small counts read "fewer than 3".
* **No live differencing**: check-in results appear once a check-in has
  closed, and counts of the running round leave out today. Watching a
  number change right after someone wrote can't tell what they wrote.
* Free-text answers and feedback never appear in results.

**Ideas**

* An idea exists only after its author tapped 🌍. It has no author column
  and no link to the submission or response it came from.
* 👍 and 💬 are keyed per account with `HMAC(secret, idea ‖ Matrix ID)`: no
  time, no row order: enough to stop double support and to show you that
  you support it, never a list of supporters.

**Access**

* Who may use it: members of the authorized rooms, asked of the homeserver
  **before every action** (never cached). When the server can't answer,
  nothing is done ("try again in a moment"), and nothing they were
  answering is dropped. Leaving all rooms locks the chat at the next tap.
* Every message or tap in a chat is only handled if the chat is still
  exactly the bot and that resident, both joined, and end-to-end encrypted
  (fresh `/state`), and for messages the event itself must have arrived
  encrypted. Taps (reactions) may arrive in the clear: Element Web and
  FluffyChat don't encrypt reactions, and they carry only an emoji.
* Chats are created by the bot: encrypted, room version 12 when the server
  supports it as stable (else 11, 10, else the server's default, never "the
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
* **Backups** of `store/` contain whatever was in the database at that time,
  including feedback deleted since. Keep backups short-lived, and keep
  `kummerkasten.key` out of the same backup if you can (without it, codes in
  a stolen database can't be tested).
* **The homeserver knows who writes to the bot and when.** That is why
  submissions carry no time and logs no submission details.
* **Small groups**: floor submissions on a floor with two people aren't
  anonymous; results hide groups below `privacy_threshold` (and what would
  reveal them).

## Updates and restarts

An update (or any restart) while a check-in is running loses nothing:

* **Stopping** (`docker stop`, SIGTERM): the taps and messages being
  handled are finished first (up to 8 s, under Docker's 10 s), then the
  bot stops between two sync batches. Nothing is half done.
* **Answers in progress** are in the database after every step. After the
  restart the chat shows the question they were at again; a tap or a typed
  answer meant for the screen before is not used ("🔄 The question had
  just changed"). Answers are stored once, even if ✅ arrives twice.
* **Downtime**: a check-in that closed meanwhile is closed at the first
  tick and nothing goes out for it any more (no notices, no chats created,
  no announcement). A reminder whose time passed is sent late, unless the
  check-in closes within the hour; one that a moved plenum would put in
  the past is dropped.
* **A new version that stores things differently**: answers in progress
  from the old version still read (tests pin what is stored). If a
  conversation can't be read at all, the check-in is kept and starts again
  at its intro with the answers that still read; the log says so (without
  any text).
* **The server having trouble** right after a restart: "try again in a
  moment", never a locked chat or a lost draft.
* **The scheduler** (notices, reminders, closing, redactions) carries on
  after an unexpected error in one tick.
* **Moving to the calendar** (2026-10-22, migration 9): the database of a
  running survey opens as it is. The first read of the calendar links each
  existing plenum to its occurrence at the same time (else the same day), so
  the open round, its feedback, check-ins and announcements stay on the
  same rows; only the future rest of the old schedule is cancelled. An old
  config with `plenum_time` still starts (it is ignored, with a warning);
  one without `[kummerkasten.calendar]` starts too and changes no plenum.

What the bot refuses at startup, to keep data consistent:

* **An older image over a newer database** (rollback): it says which
  version the database is from. Run the newer image again, or restore a
  backup from before the update.
* **A missing `kummerkasten.key`** while the database holds codes or
  check-in answers: a new key would let everyone answer a running check-in
  again and break every deletion code. Restore the file.

Removing a room that an open check-in is for: `!kk rooms remove` refuses
(close the check-in first); a room dropped from `[[kummerkasten.rooms]]`
is logged as a warning.

Not covered: a hard kill (`kill -9`, out of memory) in the middle of
handling can still lose the taps of that moment; the resident taps again.

## Development

```
cargo test
cargo clippy --all-targets
```

The calendar comes from `mxbot-common`'s `calendar` feature (CalDAV and
iCalendar, shared with calendar-bot and plenum-bot). The bot needs `mxbot-common` from `bb2c163` on: the runtime room allow-list
(`rooms::DynamicRooms`), `private` (private chats, fresh membership checks,
room version choice) and `send::classify_error` (rate limits for requests
other than sends). To develop against a local checkout, add a temporary
`[patch."https://github.com/PolynomialDivision/mxbot-common"]` pointing at
`../mxbot-common`, but never commit it: Docker builds can't see `../`.

Modules: `flow` (state machine, pure) · `screens` (texts and buttons) ·
`engine` (flow + database) · `dm` (Matrix side) · `calendar` (reading the
Nextcloud calendar) · `plenums` (rows from the calendar, rounds) · `lifecycle` (completion) · `submissions` ·
`catalog` · `surveys` (templates, versions) · `checkins` (runs, answers,
delivery, deadlines) · `reconcile` (one chat per eligible resident) ·
`stats` (aggregates, privacy policy) · `results` (texts) · `ideas` ·
`measures` · `summary` (day-before summary, updates) · `access` · `secrets` ·
`scheduler` · `admin` · `admin_checkins` · `admin_results` · `store` · `db`
(migrations in `migrations/`, applied once each, in order, each in a
transaction).

## Roadmap

* ✅ **M2: check-ins** (see above), with rescheduling as a normal
  operation, completion only when a plenum took place, one chat per
  eligible resident, minimal delivery records and 80-bit deletion codes.
* ✅ **M3: results, ideas, measures, plenum summaries** (see above).
* Later: public excerpts of free text (only with explicit rules and
  consent); breakdowns by kitchen or room (the same suppression applies);
  an optional dedicated Kummerkasten room for richer dashboards.
