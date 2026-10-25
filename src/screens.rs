//! What each screen says, and its buttons (reactions the bot seeds under
//! the message). A tap is mapped back to an action by rendering the same
//! screen again: nothing about buttons needs storing.
//!
//! Lists use their own emojis when those are distinct, numbers otherwise
//! (🍳 Kitchen 1–4), and typing the number works too.

use chrono::{DateTime, NaiveDate, Utc};

use crate::{
    catalog::{Catalog, Node},
    checkins::{Answer, Listing, Run},
    flow::{Action, Flow, IdeaPlan, MenuItem, Screen, Share},
    ideas::Idea,
    results::TopicPage,
    submissions::{self, Identity, Retention},
    surveys::{QKind, Version},
    time::Zone,
};

pub const BACK: &str = "⬅️";
pub const CANCEL: &str = "❌";
const SKIP: &str = "⏭️";
const SUBMIT: &str = "✅";
const TOGGLE: &str = "🗣️";
const DELETE: &str = "🗑️";
const NEXT: &str = "➡️";
const BEGIN: &str = "▶️";
const HOME: &str = "🏠";
/// The button on check-in notices and reminders.
pub const START_CHECKIN: &str = "📊";
const NUMBERS: [&str; 10] = ["1️⃣", "2️⃣", "3️⃣", "4️⃣", "5️⃣", "6️⃣", "7️⃣", "8️⃣", "9️⃣", "🔟"];
const RESERVED: [&str; 8] = [BACK, CANCEL, SKIP, SUBMIT, TOGGLE, DELETE, NEXT, BEGIN];

/// What a screen shows besides the flow itself.
#[derive(Default)]
pub struct Ctx<'a> {
    pub cat: Option<&'a Catalog>,
    /// "Sunday, 18 October" and "in 13 days".
    pub next_plenum: Option<(String, String)>,
    pub own_floor: Option<i64>,
    pub named: &'a [submissions::View],
    pub managed: Option<&'a submissions::View>,
    /// Check-ins this person is asked (open or recently answered).
    pub checkins: &'a [Listing],
    /// The check-in run being answered.
    pub run: Option<&'a Run>,
    pub zone: Option<Zone>,
    pub today: Option<NaiveDate>,
    /// What the showing screen needs from the database (results, ideas).
    pub prepared: Option<Prepared>,
}

/// Content prepared for a results or ideas screen (already through the
/// privacy policy: screens only lay it out).
#[derive(Clone, Debug, PartialEq)]
pub enum Prepared {
    Results {
        text: String,
        topics: Vec<(i64, String, String)>,
    },
    Topic(TopicPage),
    Text(String),
    History(Vec<(i64, String)>),
    Board {
        text: String,
        items: Vec<(i64, String)>,
    },
    Idea {
        idea: Idea,
        /// "🧹 Cleaning".
        topic: String,
        comments: Vec<String>,
        /// This account supports it (shown to them only).
        supported: bool,
    },
    /// What would be shared ("share as an idea?").
    Share(String),
    Similar(Idea),
}

impl Ctx<'_> {
    /// "Saturday, 14 November".
    pub fn day(&self, t: DateTime<Utc>) -> String {
        match (self.zone, self.today) {
            (Some(zone), Some(today)) => zone.day_label(t, today),
            _ => t.format("%A, %-d %B").to_string(),
        }
    }

    fn open_checkins(&self) -> usize {
        self.checkins
            .iter()
            .filter(|c| c.open && !c.completed)
            .count()
    }
}

pub struct View {
    pub text: String,
    pub buttons: Vec<(String, Action)>,
    /// Actions addressed by typed list number, including choices that cannot
    /// be represented by a reaction emoji.
    pub numbered: Vec<Action>,
}

/// The most reactions Element shows under a message: with more, only the
/// first 8 and "Show all" (`MAX_ITEMS_WHEN_LIMITED`, element-web 2026-10).
pub const REACTIONS_SHOWN: usize = 9;

/// A screen with more buttons than Element shows: its step buttons come
/// first and its options are numbered, so the hidden ones can be typed.
pub fn crowded(buttons: usize) -> bool {
    buttons > REACTIONS_SHOWN
}

impl View {
    /// The buttons in the order they are added as reactions: the order of
    /// the text, but on a screen with more than Element shows, the step
    /// buttons (➡️ Next, ⬅️ Back …) first, so they are never the hidden ones
    /// (the choices are listed in the text, and "Show all" brings the rest).
    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self.buttons.iter().map(|(k, _)| k.clone()).collect();
        if crowded(keys.len()) {
            keys.sort_by_key(|k| !RESERVED.iter().any(|r| same_key(r, k)));
        }
        keys
    }
}

/// Same key, with or without the emoji variation selector.
pub fn same_key(a: &str, b: &str) -> bool {
    let bare = |s: &str| s.replace('\u{fe0f}', "");
    bare(a) == bare(b)
}

/// Emojis that look alike as reactions (small in a phone's row of them).
const LOOK_ALIKE: &[&[&str]] = &[
    &["📅", "📆", "🗓️"],
    &["⏰", "⏱️", "⏲️", "🕰️", "🕐", "🕑", "🕒", "🕓", "🕔", "🕕"],
    &["✅", "✔️", "☑️", "🆗"],
    &["❌", "✖️", "❎"],
    &["🙂", "😊", "☺️", "😀", "😃", "😄"],
    &["👍", "👌"],
    &["⚠️", "🚸"],
    &["⬆️", "🔼", "⏫"],
    &["⬇️", "🔽", "⏬"],
];

/// What would make a question's choice buttons work badly, for admins:
/// a missing, repeated or reserved emoji (then every choice is shown as
/// 1️⃣ 2️⃣ …), or two that look alike.
pub fn choice_key_warnings(emojis: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let reserved: Vec<&String> = emojis
        .iter()
        .filter(|e| RESERVED.iter().any(|r| same_key(r, e)))
        .collect();
    let mut seen: Vec<&str> = Vec::new();
    let mut twice: Vec<&str> = Vec::new();
    for e in emojis.iter().filter(|e| !e.is_empty()) {
        if seen.iter().any(|s| same_key(s, e)) {
            if !twice.iter().any(|t| same_key(t, e)) {
                twice.push(e);
            }
        } else {
            seen.push(e);
        }
    }
    let numbered = if emojis.iter().any(|e| e.is_empty()) {
        Some("a choice has no emoji".to_owned())
    } else if !twice.is_empty() {
        Some(format!("{} is used twice", twice.join(" ")))
    } else if !reserved.is_empty() {
        Some(format!(
            "{} is a navigation button",
            reserved
                .iter()
                .map(|e| e.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        ))
    } else {
        None
    };
    if let Some(why) = numbered {
        out.push(format!(
            "🔢 The choices are shown as 1️⃣ 2️⃣ … instead of their emojis: {why}."
        ));
    }
    for group in LOOK_ALIKE {
        let alike: Vec<&str> = seen
            .iter()
            .copied()
            .filter(|e| group.iter().any(|g| same_key(g, e)))
            .collect();
        if alike.len() > 1 {
            out.push(format!(
                "👀 {} look alike as reactions: pick clearly different ones \
                 (`choice <n> emoji <c> <emoji>`).",
                alike.join(" ")
            ));
        }
    }
    out
}

/// How many of the bot's buttons under an edited screen stay: the common
/// start of the old buttons (`(key, reaction)`, in the order they were
/// added) and the new view's keys. The rest are taken away and added anew,
/// so clients (which show reactions in the order they came) keep the order
/// of the text, and every shown button has its reaction.
pub fn buttons_kept(old: &[(String, String)], new: &[String]) -> usize {
    old.iter()
        .zip(new)
        .take_while(|((had, _), want)| same_key(had, want))
        .count()
}

/// A long reaction row can only be edited in place if its reaction keys
/// already have the exact order wanted by the new screen. Matrix clients
/// may retain the first-seen order even after a reaction is redacted, so a
/// changed long row needs a fresh event to make its visible controls usable.
pub fn can_edit_reaction_order(old: &[(String, String)], new: &[String]) -> bool {
    !crowded(new.len()) || buttons_kept(old, new) == new.len()
}

pub fn resolve(view: &View, key: &str) -> Option<Action> {
    view.buttons
        .iter()
        .find(|(k, _)| same_key(k, key))
        .map(|(_, a)| a.clone())
}

/// A typed answer on a button screen: a list number ("2"), or back/cancel.
pub fn typed(view: &View, text: &str) -> Option<Action> {
    let t = text.trim().to_lowercase();
    match t.as_str() {
        "back" | "zurück" => return Some(Action::Back),
        // Matrix clients differ in how many reactions they show and how
        // they order them. Multi-select screens keep ➡️ near the front,
        // but plain text must remain a reliable way to advance even when a
        // client hides that reaction.
        "next" | "weiter" | "continue" => return Some(Action::Next),
        "cancel" | "abbrechen" | "menu" | "stop" => return Some(Action::Cancel),
        _ => {}
    }
    let n: usize = t.trim_end_matches('.').parse().ok()?;
    if let Some(action) = view.numbered.get(n.checked_sub(1)?) {
        return Some(action.clone());
    }
    let key = NUMBERS.get(n.checked_sub(1)?)?;
    resolve(view, key)
}

struct Builder {
    lines: Vec<String>,
    buttons: Vec<(String, Action)>,
    numbered: Vec<Action>,
    /// Option lines with a button, numbered ("3. ") once the screen is done
    /// if it is crowded: (line, where the number goes, the number).
    numbers: Vec<(usize, usize, usize)>,
    /// Lines that depend on the numbers: (line, numbered, plain); without a
    /// plain text the line (and the gap before it) is left out.
    hints: Vec<(usize, String, Option<String>)>,
}

impl Builder {
    fn new(title: impl Into<String>) -> Self {
        Self {
            lines: vec![title.into()],
            buttons: Vec::new(),
            numbered: Vec::new(),
            numbers: Vec::new(),
            hints: Vec::new(),
        }
    }

    fn line(&mut self, text: impl Into<String>) -> &mut Self {
        self.lines.push(text.into());
        self
    }

    fn gap(&mut self) -> &mut Self {
        self.lines.push(String::new());
        self
    }

    /// A line that reads `numbered` on a crowded screen, `plain` (or
    /// nothing) otherwise.
    fn hint(&mut self, numbered: &str, plain: Option<&str>) -> &mut Self {
        self.hints.push((
            self.lines.len(),
            numbered.to_owned(),
            plain.map(str::to_owned),
        ));
        self.lines.push(String::new());
        self
    }

    /// Option `i` (from [`option_keys`]): its button, or without one (past
    /// ten numbers) a line with its number.
    fn option(&mut self, i: usize, key: Option<String>, text: &str, action: Action) {
        self.numbered.push(action.clone());
        match key {
            Some(key) => {
                self.numbers.push((self.lines.len(), key.len() + 1, i + 1));
                self.lines.push(format!("{key} {text}"));
                self.buttons.push((key, action));
            }
            None => self.lines.push(format!("{}. {text}", i + 1)),
        }
    }

    fn button(&mut self, key: &str, label: &str, action: Action) -> &mut Self {
        self.lines.push(format!("{key} {label}"));
        self.buttons.push((key.to_owned(), action));
        self
    }

    /// One button per node: its emoji when they're all distinct (and not a
    /// navigation emoji), else numbered.
    fn choices(&mut self, nodes: &[&Node], action: impl Fn(&Node) -> Action) -> &mut Self {
        let items: Vec<(String, String)> = nodes
            .iter()
            .map(|n| (n.emoji.clone(), n.label.clone()))
            .collect();
        self.options(&items, |i| action(nodes[i]))
    }

    /// `(emoji, label)` options as buttons: see [`Builder::choices`].
    fn options(
        &mut self,
        items: &[(String, String)],
        action: impl Fn(usize) -> Action,
    ) -> &mut Self {
        self.gap()
            .hint("🔢 Or send the number of your choice.", None);
        for (i, (key, line, a)) in option_keys(items, action).into_iter().enumerate() {
            self.option(i, key, &line, a);
        }
        self
    }

    /// ⬅️ Back · ❌ Cancel, under every screen but the menu.
    fn nav(&mut self) -> View {
        self.gap();
        self.lines.push(nav_line());
        self.buttons.push((BACK.to_owned(), Action::Back));
        self.buttons.push((CANCEL.to_owned(), Action::Cancel));
        self.done()
    }

    fn done(&mut self) -> View {
        let numbered = crowded(self.buttons.len());
        for (line, at, n) in self.numbers.drain(..) {
            if numbered {
                self.lines[line].insert_str(at, &format!("{n}. "));
            }
        }
        // From the last, so the lines before keep their place.
        for (line, with, plain) in std::mem::take(&mut self.hints).into_iter().rev() {
            match (numbered, plain) {
                (true, _) => self.lines[line] = with,
                (false, Some(plain)) => self.lines[line] = plain,
                (false, None) => {
                    self.lines.remove(line);
                    if line > 0 && self.lines[line - 1].is_empty() {
                        self.lines.remove(line - 1);
                    }
                }
            }
        }
        View {
            text: self.lines.join("\n"),
            buttons: std::mem::take(&mut self.buttons),
            numbered: std::mem::take(&mut self.numbered),
        }
    }
}

/// The navigation line [`Builder::nav`] puts under a screen.
fn nav_line() -> String {
    format!("{BACK} Back · {CANCEL} Cancel")
}

/// A check-in screen as survey-only mode shows it: no ❌ Cancel (stopping
/// is simply not answering; the answers so far wait until it closes), and
/// on the intro no navigation at all (there is nothing before it). A
/// privacy line on the intro, which is the first thing they see.
pub fn survey_view(mut view: View, screen: &Screen) -> View {
    let intro = matches!(screen, Screen::CheckinIntro);
    view.buttons
        .retain(|(key, _)| !same_key(key, CANCEL) && !(intro && same_key(key, BACK)));
    let nav = nav_line();
    view.text = if intro {
        // The privacy line just above ▶️ Start (after the gap before it).
        let mut lines: Vec<String> = view
            .text
            .replace(&format!("\n\n{nav}"), "")
            .lines()
            .map(str::to_owned)
            .collect();
        let start = lines
            .iter()
            .rposition(|l| l.starts_with(BEGIN))
            .unwrap_or(lines.len());
        let at = if start > 0 && lines[start - 1].is_empty() {
            start - 1
        } else {
            start
        };
        lines.insert(at, SURVEY_PRIVACY.to_owned());
        lines.join("\n")
    } else {
        view.text.replace(&nav, &format!("{BACK} Back"))
    };
    view
}

/// On a check-in's intro in survey-only mode, the first thing they read.
const SURVEY_PRIVACY: &str = "🔐 Just you and me here. Nothing is stored before you tap ✅.";

/// `!help` in survey-only mode.
pub fn survey_help() -> String {
    "❓ **How this works**\n\n\
     • Tap an emoji under my message to answer. Where the choices are numbered, \
     you can also type the number.\n\
     • ✍️ When I ask you to write, just send a message.\n\
     • ⬅️ goes back to the question before. Nothing is stored before you tap ✅.\n\
     • ⏸️ No time? Just stop. Your answers wait here until the check-in closes."
        .to_owned()
}

/// The reminder in survey-only mode: the screen they are on comes right
/// below it.
pub fn survey_reminder(run: &Run, ctx: &Ctx) -> String {
    format!(
        "🔔 **Quick reminder**\n\nThe {} {} check-in is open until {}. It takes {}. 🌱",
        run.emoji,
        crate::markup::literal(&run.title),
        ctx.day(run.closes_at),
        run.duration_hint()
    )
}

/// Survey-only mode, a message while no check-in is waiting for them.
pub fn nothing_open() -> &'static str {
    "🌱 Nothing to answer right now. I'll message you when the next check-in starts."
}

/// Survey-only mode: 📊 on another check-in while one is half done.
pub fn finish_first(run: &Run) -> String {
    format!(
        "👉 First the {} {} check-in you started, then this one comes up.",
        run.emoji,
        crate::markup::literal(&run.title)
    )
}

/// `(button key, line, action)` per option; no key beyond ten numbers
/// (see [`Builder::option`]).
fn option_keys(
    items: &[(String, String)],
    action: impl Fn(usize) -> Action,
) -> Vec<(Option<String>, String, Action)> {
    let mut emojis: Vec<&str> = items.iter().map(|(e, _)| e.as_str()).collect();
    emojis.sort_unstable();
    emojis.dedup();
    let distinct = emojis.len() == items.len()
        && !emojis.contains(&"")
        && !items
            .iter()
            .any(|(e, _)| RESERVED.iter().any(|r| same_key(r, e)));
    items
        .iter()
        .enumerate()
        .map(|(i, (emoji, label))| {
            let titled = if emoji.is_empty() {
                label.clone()
            } else {
                format!("{emoji} {label}")
            };
            if distinct {
                (Some(emoji.clone()), label.clone(), action(i))
            } else {
                match NUMBERS.get(i) {
                    Some(n) => (Some((*n).to_owned()), titled, action(i)),
                    None => (None, titled, action(i)),
                }
            }
        })
        .collect()
}

/// A resident's own words, shown as they wrote them (never as markup).
fn quote(text: &str) -> String {
    format!("“{}”", crate::markup::literal(text.trim()))
}

/// "With my floor", "With my floor (3rd floor)".
fn floor_label(ctx: &Ctx, floor: Option<i64>) -> String {
    let name = floor
        .and_then(|f| ctx.cat.and_then(|c| c.location(f)))
        .map(|f| format!(" ({})", f.label))
        .unwrap_or_default();
    format!("With my floor{name}")
}

pub fn view(flow: &Flow, ctx: &Ctx) -> View {
    let empty = Catalog::default();
    let cat = ctx.cat.unwrap_or(&empty);
    let draft = &flow.draft;
    match flow.screen() {
        Screen::Menu => menu(ctx),
        Screen::Topic { parent: None } => {
            let mut b = Builder::new("📝 **Share something**");
            b.gap().line("What is this about?").gap();
            b.choices(&cat.topic_children(None), |n| Action::Topic(n.id));
            b.nav()
        }
        Screen::Topic { parent: Some(p) } => {
            let title = cat.topic(*p).map(|t| t.title()).unwrap_or_default();
            let mut b = Builder::new(format!("**{title}**"));
            b.gap().line("What exactly? 🔎").gap();
            b.choices(&cat.topic_children(Some(*p)), |n| Action::Topic(n.id));
            b.nav()
        }
        Screen::Location { parent: None } => {
            let mut b = Builder::new("📍 **Where?**");
            b.gap();
            b.choices(&cat.location_children(None), |n| Action::Location(n.id));
            b.button(SKIP, "Not about a place", Action::NoLocation);
            b.nav()
        }
        Screen::Location { parent: Some(p) } => {
            let title = cat.location(*p).map(|t| t.title()).unwrap_or_default();
            let mut b = Builder::new(format!("📍 **{title}**"));
            b.gap().line("Which one?").gap();
            b.choices(&cat.location_children(Some(*p)), |n| Action::Location(n.id));
            b.nav()
        }
        Screen::Text => {
            let mut b = if flow.is_idea(cat) {
                let mut b = Builder::new("💡 **What's your idea or wish?**");
                b.gap().line("Just write it here as a message. ✍️");
                b
            } else {
                let mut b = Builder::new("💬 **Tell me what's going on**");
                b.gap()
                    .line("Write as much or as little as you want. 🌱")
                    .line("Just send it as a message here.");
                b
            };
            if let Some(body) = &draft.body {
                b.gap().line(format!("So far: {}", quote(body))).gap();
                b.button(SKIP, "Keep that", Action::Skip);
            }
            b.nav()
        }
        Screen::Suggestion => {
            let mut b = Builder::new("💡 **What would you change?**");
            b.gap()
                .line("Optional. Send it as a message, or skip.")
                .gap();
            b.button(SKIP, "Skip", Action::Skip);
            b.nav()
        }
        Screen::Identity => {
            let mut b = Builder::new("👀 **How would you like to submit this?**");
            identities(&mut b, ctx, draft.floor, &ALL_IDENTITIES);
            b.nav()
        }
        Screen::Floor => {
            let mut b = Builder::new("🏠 **Which floor do you live on?**");
            b.gap()
                .line("I'll remember it for next time. Your submissions only")
                .line("ever store the floor, never who you are.")
                .gap();
            b.choices(&cat.floors(), |n| Action::Floor(n.id));
            b.nav()
        }
        Screen::Retention => {
            let when = ctx
                .next_plenum
                .as_ref()
                .map(|(day, _)| format!("the plenum on {day}"))
                .unwrap_or_else(|| "the next plenum".to_owned());
            let mut b = Builder::new("🗄️ **How long should this feedback be kept?**");
            b.gap();
            b.button("♾️", "**Keep it**", Action::Retention(Retention::Keep))
                .line("     so recurring issues can be compared over time");
            b.button(
                "🧹",
                "**Delete after the next plenum**",
                Action::Retention(Retention::DeleteAfterPlenum),
            )
            .line(format!("     kept for {when}, then permanently removed"));
            b.nav()
        }
        Screen::Preview => {
            let mut b = Builder::new("📮 **Ready to submit**");
            b.gap();
            if let Some(t) = draft.topic {
                b.line(cat.topic_path(t));
            }
            b.line(match draft.location {
                Some(l) => format!("📍 {}", cat.location_path(l)),
                None => "📍 (none)".to_owned(),
            });
            match draft.identity {
                Some(Identity::Floor) => {
                    b.line(format!(
                        "🏠 {}",
                        floor_label(ctx, draft.floor.or(ctx.own_floor))
                    ));
                }
                Some(identity) => {
                    b.line(identity.title());
                }
                None => {}
            }
            b.line(if draft.for_plenum {
                "🗣️ Include in next plenum"
            } else {
                "🤫 Not for the plenum, just for the record"
            });
            if let Some(r) = draft.retention {
                b.line(r.title());
            }
            b.gap();
            if let Some(body) = &draft.body {
                b.line(quote(body));
            }
            if let Some(s) = &draft.suggestion {
                b.line(format!("💡 {}", quote(s)));
            }
            if let Some(plan) = &draft.idea {
                b.gap().line(idea_line(plan));
            }
            b.gap();
            b.button(SUBMIT, "**Submit**", Action::Submit);
            b.button(
                TOGGLE,
                if draft.for_plenum {
                    "Don't bring it up at the plenum"
                } else {
                    "Bring it up at the plenum"
                },
                Action::TogglePlenum,
            );
            b.nav()
        }
        Screen::ManageEnter => {
            let mut b = Builder::new("🔑 **Manage a submission**");
            b.gap()
                .line("Send me your deletion code, like `C7MJ-4P2K-XQ7D-9FRT`.");
            if !ctx.named.is_empty() {
                b.gap().line("Or pick one you sent with your name:");
                let nodes: Vec<Node> = ctx
                    .named
                    .iter()
                    .map(|s| Node {
                        id: s.id,
                        key: String::new(),
                        parent_id: None,
                        emoji: String::new(),
                        label: format!("{} · {}", s.topic, s.submitted_on.format("%-d %b")),
                        problem_id: None,
                        is_idea: false,
                        is_floor: false,
                    })
                    .collect();
                let refs: Vec<&Node> = nodes.iter().collect();
                b.choices(&refs, |n| Action::Named(n.id));
            }
            b.nav()
        }
        Screen::Manage => {
            let mut b = Builder::new("🔑 **Your submission**");
            b.gap();
            match ctx.managed {
                Some(s) => {
                    b.line(&s.topic);
                    if let Some(l) = &s.location {
                        b.line(format!("📍 {l}"));
                    }
                    b.line(s.identity.title())
                        .line(s.retention.title())
                        .line(format!("🗓️ {}", s.submitted_on.format("%-d %B %Y")))
                        .gap()
                        .line(quote(&s.body));
                    if let Some(sug) = &s.suggestion {
                        b.line(format!("💡 {}", quote(sug)));
                    }
                    b.gap();
                    b.button(DELETE, "**Delete it** for good", Action::Delete);
                }
                None => {
                    b.line("⏳ I've forgotten which one that was. Send the code again.");
                }
            }
            b.nav()
        }
        Screen::Privacy => {
            let mut b = Builder::new("🔐 **Privacy**");
            b.gap()
                .line("🧹 Messages in this chat are temporary. They're removed after the next plenum.")
                .line("🗄️ Submitted feedback is stored as you choose: kept, or deleted after the next plenum.")
                .line("🥷 Anonymous feedback is not stored with your Matrix account; 🏠 floor feedback only with your floor.")
                .line("🔑 For deletion codes I keep only a fingerprint, never the code.")
                .line("📊 Results never show groups so small that someone could be recognised.")
                .gap()
                .line("🔎 *The honest small print:* removing Matrix messages (redaction) hides them for everyone, \
                       but servers, backups and your own devices may keep copies for a while. \
                       And I see what you send me while handling it.");
            b.nav()
        }
        Screen::CodeSaved => {
            // The receipt itself (with the code) is written once, by the
            // engine; this is what its buttons mean.
            let mut b = Builder::new("🔑 **Your deletion code**");
            b.gap();
            b.button(SUBMIT, "**I've saved the code**", Action::CodeSaved);
            b.button(HOME, "Main menu", Action::Home);
            b.done()
        }
        Screen::Checkins => checkin_list(ctx),
        Screen::CheckinIntro => match ctx.run {
            Some(run) => {
                let mut b = Builder::new(format!("📊 **{} {} check-in**", run.emoji, run.title));
                if !run.intro.is_empty() {
                    b.gap().line(crate::markup::literal(&run.intro));
                }
                b.gap()
                    .line(format!(
                        "📝 {} question{} · ⏱️ {}",
                        run.questions.len(),
                        if run.questions.len() == 1 { "" } else { "s" },
                        run.duration_hint()
                    ))
                    .line(format!("🗓️ Open until {}", ctx.day(run.closes_at)));
                if run.plenum_id.is_some() {
                    b.line("🗣️ For the next plenum");
                }
                b.line(match run.identities.as_slice() {
                    [Identity::Anonymous] => {
                        "🥷 Anonymous: your answers aren't stored with your account."
                    }
                    [_] => "👀 How your answers are stored is shown before you submit.",
                    _ => "👀 At the end you choose how to submit, anonymously if you like.",
                })
                .gap();
                b.button(BEGIN, "**Start**", Action::Begin);
                b.nav()
            }
            None => lost(),
        },
        Screen::CheckinQuestion { index } => {
            match ctx.run.zip(ctx.run.and_then(|r| r.questions.get(*index))) {
                Some((run, q)) => question(run, *index, q, answer_of(flow, *index)),
                None => lost(),
            }
        }
        Screen::CheckinComment { .. } => {
            let mut b = Builder::new("💬 **Anything to add?**");
            b.gap()
                .line("Optional. Send it as a message, or skip.")
                .gap();
            b.button(SKIP, "Skip", Action::Skip);
            b.nav()
        }
        Screen::CheckinIdentity => {
            let mut b = Builder::new("👀 **How would you like to submit your answers?**");
            let allowed = ctx
                .run
                .map_or(ALL_IDENTITIES.to_vec(), |r| r.identities.clone());
            identities(&mut b, ctx, draft.floor, &allowed);
            b.nav()
        }
        Screen::CheckinPreview => match ctx.run {
            Some(run) => checkin_preview(ctx, flow, run),
            None => lost(),
        },
        Screen::ShareIdea | Screen::CheckinShareIdea { .. } => {
            let mut b = Builder::new("💡 **Would you like to share this as an idea?**");
            if let Some(Prepared::Share(text)) = &ctx.prepared {
                b.gap().line(quote(text));
            }
            b.gap();
            b.button("🌍", "**Add to Ideas**", Action::Share(Share::Public))
                .line("     everyone can see and 👍 it, without your name");
            if matches!(flow.screen(), Screen::ShareIdea) {
                b.button(
                    "🗣️",
                    "**Only for the plenum**",
                    Action::Share(Share::PlenumOnly),
                );
                b.button("🔒", "**Keep private**", Action::Share(Share::Private));
            } else {
                b.button(
                    "🔒",
                    "**Keep it in my answers**",
                    Action::Share(Share::Private),
                );
            }
            b.nav()
        }
        Screen::SimilarIdea { .. } | Screen::CheckinSimilarIdea { .. } => {
            let mut b = Builder::new("💡 **A similar idea already exists**");
            if let Some(Prepared::Similar(idea)) = &ctx.prepared {
                b.gap()
                    .line(format!("**{}**", crate::markup::literal(&idea.title)))
                    .line(format!("👍 {} · {}", idea.support, idea.status.label()));
            }
            b.gap();
            b.button("👍", "**Support this idea**", Action::SupportExisting);
            b.button("💬", "Add mine as a comment", Action::CommentExisting);
            b.button("➕", "Create mine separately", Action::CreateSeparately);
            b.nav()
        }
        Screen::Results => match &ctx.prepared {
            Some(Prepared::Results { text, topics }) => {
                let mut b = Builder::new(text.clone());
                b.gap();
                b.options(
                    &topics
                        .iter()
                        .map(|(_, e, l)| (e.clone(), l.clone()))
                        .collect::<Vec<_>>(),
                    |i| Action::OpenTopic(topics[i].0),
                );
                b.button("📚", "Previous plenums", Action::History);
                b.nav()
            }
            _ => lost_results(),
        },
        Screen::ResultsTopic { page, .. } => match &ctx.prepared {
            Some(Prepared::Topic(t)) => {
                let mut b = Builder::new(t.text.clone());
                b.gap();
                if page + 1 < t.runs {
                    b.button("⏪", "Older check-in", Action::Older);
                }
                if *page > 0 {
                    b.button("⏩", "Newer check-in", Action::Newer);
                }
                if t.floors.is_some() {
                    b.button("🏠", "By floor", Action::Floors);
                }
                b.nav()
            }
            _ => lost_results(),
        },
        Screen::ResultsFloors { .. } | Screen::ResultsPlenum { .. } => match &ctx.prepared {
            Some(Prepared::Text(text)) => {
                let mut b = Builder::new(text.clone());
                b.nav()
            }
            _ => lost_results(),
        },
        Screen::ResultsHistory => {
            let mut b = Builder::new("📚 **Previous plenums**");
            b.gap();
            match &ctx.prepared {
                Some(Prepared::History(list)) if !list.is_empty() => {
                    b.options(
                        &list
                            .iter()
                            .map(|(_, l)| ("🗓️".to_owned(), l.clone()))
                            .collect::<Vec<_>>(),
                        |i| Action::OpenPlenum(list[i].0),
                    );
                }
                _ => {
                    b.line("🌱 No plenum has been completed yet.");
                }
            }
            b.nav()
        }
        Screen::Ideas => match &ctx.prepared {
            Some(Prepared::Board { text, items }) => {
                let mut b = Builder::new(text.clone());
                b.gap();
                b.options(
                    &items
                        .iter()
                        .map(|(_, l)| (String::new(), l.clone()))
                        .collect::<Vec<_>>(),
                    |i| Action::OpenIdea(items[i].0),
                );
                b.nav()
            }
            _ => lost_results(),
        },
        Screen::Idea { .. } => match &ctx.prepared {
            Some(Prepared::Idea {
                idea,
                topic,
                comments,
                supported,
            }) => {
                let mut b = Builder::new(format!("💡 **{}**", crate::markup::literal(&idea.title)));
                if !topic.is_empty() {
                    b.line(topic.clone());
                }
                b.line(format!(
                    "{} · 👍 {} · 💬 {}",
                    idea.status.label(),
                    idea.support,
                    idea.comments
                ));
                if let Some(body) = &idea.body {
                    b.gap().line(quote(body));
                }
                if !comments.is_empty() {
                    b.gap().line("💬 **Comments**");
                    for c in comments {
                        b.line(format!("• {}", quote(c)));
                    }
                }
                b.gap();
                if idea.status.active() {
                    if *supported {
                        b.line("✅ You support this idea.");
                        b.button("↩️", "Take my 👍 back", Action::WithdrawSupport);
                    } else {
                        b.button("👍", "**Support this idea**", Action::SupportIdea);
                    }
                    b.button("💬", "Add or change my comment", Action::CommentIdea);
                } else {
                    b.line("🗄️ This idea is closed.");
                }
                b.line("🥷 Nobody sees who supports or comments.");
                b.nav()
            }
            _ => lost_results(),
        },
        Screen::IdeaComment { .. } => {
            let mut b = Builder::new("💬 **Your comment**");
            b.gap()
                .line("Write it as a message. It's shown without your name.")
                .line("One comment per idea: writing again replaces yours.");
            b.nav()
        }
    }
}

fn lost_results() -> View {
    let mut b = Builder::new("⏳ **That's not there any more**");
    b.gap().line("Go back and pick it again.");
    b.nav()
}

/// How a draft's idea will be shared (preview line).
fn idea_line(plan: &IdeaPlan) -> &'static str {
    match plan {
        IdeaPlan::New => "🌍 Also shared on the Ideas board, without your name",
        IdeaPlan::Support { .. } => "👍 Also supports the similar idea",
        IdeaPlan::Comment { .. } => "💬 Also added as a comment to the similar idea",
    }
}

const ALL_IDENTITIES: [Identity; 3] = [Identity::Anonymous, Identity::Floor, Identity::Named];

/// The identity choices (as many as `allowed`), each with what it means.
fn identities(b: &mut Builder, ctx: &Ctx, draft_floor: Option<i64>, allowed: &[Identity]) {
    b.gap();
    for identity in allowed {
        match identity {
            Identity::Anonymous => {
                b.button("🥷", "Anonymous", Action::Identity(Identity::Anonymous))
                    .line("     not stored with your account");
            }
            Identity::Floor => {
                b.button(
                    "🏠",
                    &floor_label(ctx, ctx.own_floor.or(draft_floor)),
                    Action::Identity(Identity::Floor),
                )
                .line("     only your floor is stored. On a floor with few people,")
                .line("     that can give you away");
            }
            Identity::Named => {
                b.button("👤", "With my name", Action::Identity(Identity::Named))
                    .line("     stored with your Matrix account");
            }
        }
    }
}

fn lost() -> View {
    let mut b = Builder::new("⏳ **I've lost track of that check-in**");
    b.gap().line("Open it again from 📊 Check-ins in the menu.");
    b.nav()
}

fn answer_of(flow: &Flow, index: usize) -> Option<&Answer> {
    flow.draft
        .checkin
        .as_ref()
        .and_then(|c| c.answers.get(&index))
}

fn checkin_list(ctx: &Ctx) -> View {
    let mut b = Builder::new("📊 **Check-ins**");
    b.gap();
    let open: Vec<&Listing> = ctx
        .checkins
        .iter()
        .filter(|c| c.open && !c.completed)
        .collect();
    if open.is_empty() {
        b.line("🌱 **No open check-ins right now**");
    } else {
        let items: Vec<(String, String)> = open
            .iter()
            .map(|c| {
                (
                    String::new(),
                    format!(
                        "🟢 {} **{}** · open until {}",
                        c.emoji,
                        crate::markup::literal(&c.title),
                        ctx.day(c.closes_at)
                    ),
                )
            })
            .collect();
        b.line("Tap one to answer it:");
        b.options(&items, |i| Action::OpenRun(open[i].id));
    }
    let done: Vec<&Listing> = ctx.checkins.iter().filter(|c| c.completed).collect();
    if !done.is_empty() {
        b.gap();
        for c in done {
            b.line(format!(
                "✅ {} {} · Completed",
                c.emoji,
                crate::markup::literal(&c.title)
            ));
        }
    }
    b.nav()
}

/// "😐" or "3" for a scale value, as shown on its button.
fn scale_face(q: &Version, value: i64) -> String {
    let steps = q.config.steps();
    let pos = steps.iter().position(|v| *v == value);
    match pos.and_then(|p| q.config.faces.get(p)) {
        Some(face) if q.config.faces.len() == steps.len() => format!("{face} {value}"),
        _ => value.to_string(),
    }
}

/// The text of a given answer, for the preview and when coming back.
fn answer_text(q: &Version, a: &Answer) -> String {
    if !a.is_given() {
        return "⏭️ skipped".to_owned();
    }
    let main = match q.kind {
        QKind::Scale => a.value.map(|v| scale_face(q, v)).unwrap_or_default(),
        QKind::YesNo => match a.value {
            Some(1) => "👍 Yes".to_owned(),
            _ => "👎 No".to_owned(),
        },
        QKind::Single | QKind::Multiple => q
            .choices
            .iter()
            .filter(|c| a.choices.contains(&c.version_id))
            .map(|c| crate::markup::literal(&c.label))
            .collect::<Vec<_>>()
            .join(", "),
        QKind::Text => quote(a.text.as_deref().unwrap_or("")),
    };
    match &a.comment {
        Some(c) => format!("{main} · 💬 {}", quote(c)),
        None => main,
    }
}

fn question(run: &Run, index: usize, q: &Version, answer: Option<&Answer>) -> View {
    let mut b = Builder::new(format!(
        "{} **{} check-in · {}/{}**",
        run.emoji,
        crate::markup::literal(&run.title),
        index + 1,
        run.questions.len()
    ));
    b.gap()
        .line(format!("**{}**", crate::markup::literal(&q.text)))
        .gap();
    match q.kind {
        QKind::Scale => {
            let steps = q.config.steps();
            let faces = q.config.faces.len() == steps.len();
            for (i, value) in steps.iter().enumerate() {
                let key = if faces {
                    q.config.faces[i].clone()
                } else {
                    NUMBERS[i].to_owned()
                };
                b.button(&key, &value.to_string(), Action::Value(*value));
            }
        }
        QKind::YesNo => {
            b.button("👍", "Yes", Action::Value(1));
            b.button("👎", "No", Action::Value(0));
        }
        QKind::Single => {
            let items: Vec<(String, String)> = q
                .choices
                .iter()
                .map(|c| (c.emoji.clone(), crate::markup::literal(&c.label)))
                .collect();
            b.options(&items, |i| Action::Choose(q.choices[i].version_id));
        }
        QKind::Multiple => {
            let ticked = |id: i64| answer.is_some_and(|a| a.choices.contains(&id));
            let items: Vec<(String, String)> = q
                .choices
                .iter()
                .map(|c| (c.emoji.clone(), crate::markup::literal(&c.label)))
                .collect();
            for (i, (key, line, action)) in
                option_keys(&items, |i| Action::Toggle(q.choices[i].version_id))
                    .into_iter()
                    .enumerate()
            {
                let mark = if ticked(q.choices[i].version_id) {
                    "☑️"
                } else {
                    "⬜"
                };
                b.option(i, key, &format!("{mark} {line}"), action);
            }
            b.gap().hint(
                "👆 Tap choices or send their numbers, then ➡️ Next (or send `next`):",
                Some("👆 Tap choices, then ➡️ Next:"),
            );
            b.button(NEXT, "**Next**", Action::Next);
        }
        QKind::Text => {
            b.line("Write anything you like. ✍️ Just send it as a message.");
        }
    }
    if let Some(a) = answer.filter(|a| a.is_given() && q.kind != QKind::Multiple) {
        b.gap()
            .line(format!("Your answer so far: {}", answer_text(q, a)));
    }
    if !q.required {
        b.gap();
        b.button(SKIP, "Skip", Action::Skip);
    }
    b.nav()
}

fn checkin_preview(ctx: &Ctx, flow: &Flow, run: &Run) -> View {
    let mut b = Builder::new("📊 **Ready to submit**");
    b.gap().line(format!(
        "{} {} check-in",
        run.emoji,
        crate::markup::literal(&run.title)
    ));
    let draft = flow.draft.checkin.as_ref();
    match draft.and_then(|d| d.identity) {
        Some(Identity::Floor) => {
            b.line(format!(
                "🏠 {}",
                floor_label(ctx, flow.draft.floor.or(ctx.own_floor))
            ));
        }
        Some(identity) => {
            b.line(identity.title());
        }
        None => {}
    }
    b.gap();
    let empty = Answer::default();
    for (i, q) in run.questions.iter().enumerate() {
        let a = draft.and_then(|d| d.answers.get(&i)).unwrap_or(&empty);
        b.line(format!(
            "{}. {} → {}",
            i + 1,
            crate::markup::literal(&q.text),
            answer_text(q, a)
        ));
        if let Some(plan) = draft.and_then(|d| d.ideas.get(&i)) {
            b.line(format!("   {}", idea_line(plan)));
        }
    }
    b.gap();
    b.button(SUBMIT, "**Submit**", Action::Submit);
    b.nav()
}

fn menu(ctx: &Ctx) -> View {
    let mut b = Builder::new("📮 **Kummerkasten**");
    match &ctx.next_plenum {
        Some((day, when)) => b.line(format!("🗓️ Next plenum: **{day}** · {when}")),
        None => b.line("🗓️ No plenum scheduled right now"),
    };
    b.gap().line("What would you like to do? 🌱").gap();
    b.button("📝", "Share something", Action::Open(MenuItem::Share));
    let open = ctx.open_checkins();
    if open > 0 {
        b.button(
            "📊",
            &format!("**Check-ins · {open} open**"),
            Action::Open(MenuItem::Checkins),
        );
    } else {
        b.button("📊", "Check-ins", Action::Open(MenuItem::Checkins));
    }
    b.button("💡", "Ideas", Action::Open(MenuItem::Ideas));
    b.button("📈", "Results", Action::Open(MenuItem::Results));
    b.button("🔑", "Manage submission", Action::Open(MenuItem::Manage));
    b.button("🔐", "Privacy", Action::Open(MenuItem::Privacy));
    b.gap().line("❓ `!help` · 🏠 `!menu` anytime");
    b.done()
}

/// `!help`: how to use the chat, short (the privacy details are behind 🔐).
pub fn help() -> String {
    "❓ **How this chat works**\n\n\
     • Tap an emoji under my last message to choose. Where the choices are numbered, \
     you can also type the number.\n\
     • ✍️ When I ask you to write, just send a message.\n\
     • ⬅️ goes back, ❌ cancels: nothing is saved until you confirm with ✅.\n\
     • 🏠 `!menu` starts over at the menu, any time.\n\
     • 🔐 *Privacy* in the menu says what is stored and for how long."
        .to_owned()
}

/// Whether a message asks for help (never taken as text for a submission).
pub fn asks_for_help(body: &str) -> bool {
    matches!(
        body.trim().to_lowercase().as_str(),
        "!help" | "!hilfe" | "help" | "hilfe"
    )
}

// ── Messages that aren't screens ────────────────────────────────────────────

pub fn intro() -> String {
    "👋 **Hi, I'm the Kummerkasten!** 📮\n\n\
     Tell me what's bothering you, what's going well, or what you'd change, \
     anonymously if you like. 🌱\n\n\
     🔐 **Privacy**\n\
     Messages in this chat are temporary and will be removed after the next plenum. 🧹\n\
     Submitted feedback is stored according to the retention choice you select.\n\
     🥷 Anonymous feedback is not stored together with your Matrix account."
        .to_owned()
}

pub fn round_cleared() -> String {
    "🧹 **Previous Kummerkasten round cleared**\n\n\
     Messages from the previous round were removed from this chat.\n\
     Feedback was handled according to the storage choice you selected. 🔐\n\n\
     🌱 **A new round has started.**"
        .to_owned()
}

pub fn locked() -> String {
    "🔒 **The Kummerkasten is locked**\n\n\
     It's only open to members of the house rooms. If that's a mistake, \
     have a word with an admin. 💚"
        .to_owned()
}

/// A message typed while the chat still showed the step before (the bot
/// had just restarted or an update hadn't arrived): not used.
pub fn shown_again() -> &'static str {
    "🔄 The question had just changed, so I didn't use that.\n👇 Here it is: please answer it again."
}

pub fn expired() -> &'static str {
    "⏳ That took a while, so I tidied away the unfinished draft. Here's the menu again!"
}

pub fn deleted() -> String {
    "🗑️ **Deleted.** It's gone from the Kummerkasten and won't count in any results.".to_owned()
}

pub fn submitted(
    identity: Identity,
    token: Option<&str>,
    retention: Retention,
    plenum_day: Option<&str>,
) -> String {
    let mut text = match identity {
        Identity::Anonymous => "✅ **Submitted anonymously** 🥷".to_owned(),
        Identity::Floor => "✅ **Submitted with your floor** 🏠".to_owned(),
        Identity::Named => "✅ **Submitted with your name** 👤".to_owned(),
    };
    text.push_str("\n\n");
    match token {
        Some(token) => text.push_str(&format!(
            "🔑 Your deletion code:\n\n`{token}`\n\n\
             Keep this code if you want to manage or delete this submission later. \
             📌 Save it somewhere outside this chat. Once you have, tap ✅ and I'll \
             remove this message; otherwise it goes when the chat is cleared after \
             the plenum.\n\n"
        )),
        None => text.push_str("🔑 You can find it under **Manage submission** any time.\n\n"),
    }
    text.push_str(&match (retention, plenum_day) {
        (Retention::DeleteAfterPlenum, Some(day)) => {
            format!("🧹 It's deleted after the plenum on {day}.")
        }
        (Retention::DeleteAfterPlenum, None) => "🧹 It's deleted after the next plenum.".to_owned(),
        (Retention::Keep, _) => "♾️ It's kept, so we can compare over time.".to_owned(),
    });
    text.push_str("\n\n💚 Thank you!");
    text
}

/// How question `index` of `run` looks to residents (the admin preview).
pub fn question_preview(run: &Run, index: usize) -> String {
    question(run, index, &run.questions[index], None).text
}

pub fn checkin_thanks(run: &Run, identity: Identity) -> String {
    format!(
        "✅ **Thanks! Your answers were submitted.** 🌱\n\n📊 {} {} check-in\n{}",
        run.emoji,
        crate::markup::literal(&run.title),
        identity.title()
    )
}

/// The one message a check-in sends into everyone's chat when it starts.
pub fn checkin_notice(run: &Run, ctx: &Ctx) -> String {
    let mut text = format!(
        "📊 **New check-in!** ✨\n\n{} **{}**\n",
        run.emoji,
        crate::markup::literal(&run.title)
    );
    if !run.intro.is_empty() {
        text.push_str(&format!("{}\n", crate::markup::literal(&run.intro)));
    }
    text.push_str(&format!(
        "\n📝 {} question{}\n🗓️ Open until {}\n",
        run.questions.len(),
        if run.questions.len() == 1 { "" } else { "s" },
        ctx.day(run.closes_at)
    ));
    if run.plenum_id.is_some() {
        text.push_str("🗣️ For the next plenum\n");
    }
    text.push_str(&format!("\n{START_CHECKIN} **Start check-in**"));
    text
}

/// The one optional reminder.
pub fn checkin_reminder(run: &Run, ctx: &Ctx) -> String {
    format!(
        "🔔 **Quick reminder**\n\nThe {} {} check-in is still open until {}.\n\
         It takes {}. 🌱\n\n{START_CHECKIN} **Continue**",
        run.emoji,
        crate::markup::literal(&run.title),
        ctx.day(run.closes_at),
        run.duration_hint()
    )
}

/// Into the central room: only when the admin chose it.
pub fn checkin_announcement(run: &Run, ctx: &Ctx) -> String {
    format!(
        "📊 **New check-in: {} {}**\n\n{} · 🗓️ open until {}\n\n\
         Answer it in your private Kummerkasten chat. Tap 📮 below if you don't have one yet. \
         🥷 Anonymous if you like.",
        run.emoji,
        crate::markup::literal(&run.title),
        run.target.describe(),
        ctx.day(run.closes_at)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_emojis_that_work_badly_are_pointed_out() {
        let e = |keys: &[&str]| -> Vec<String> { keys.iter().map(|k| k.to_string()).collect() };
        assert!(choice_key_warnings(&e(&["1️⃣", "2️⃣", "4️⃣"])).is_empty());
        let alike = choice_key_warnings(&e(&["📅", "🗓", "📆"]));
        assert_eq!(alike.len(), 1);
        assert!(alike[0].contains("look alike"));
        assert!(choice_key_warnings(&e(&["🧴", "🧴"]))[0].contains("used twice"));
        assert!(choice_key_warnings(&e(&["🧴", ""]))[0].contains("no emoji"));
        assert!(choice_key_warnings(&e(&["⬅️", "🧴"]))[0].contains("navigation"));
    }

    #[test]
    fn help_is_asked_for_only_by_these_words() {
        for ask in ["!help", "Help", " hilfe ", "!HILFE"] {
            assert!(asks_for_help(ask), "{ask}");
        }
        for text in ["help me with the dishes", "?", "helpful", ""] {
            assert!(!asks_for_help(text), "{text}");
        }
    }

    #[test]
    fn an_edit_keeps_the_buttons_that_still_fit_in_order() {
        let had = |keys: &[&str]| -> Vec<(String, String)> {
            keys.iter()
                .map(|k| (k.to_string(), format!("${k}")))
                .collect()
        };
        let want = |keys: &[&str]| -> Vec<String> { keys.iter().map(|k| k.to_string()).collect() };
        // A box ticked: the same buttons, nothing changes.
        let boxes = ["🙋", "📋", "➡️", "⬅️", "❌"];
        assert_eq!(buttons_kept(&had(&boxes), &want(&boxes)), 5);
        // 👍 becomes ↩️ (support taken): from there on, anew.
        assert_eq!(
            buttons_kept(
                &had(&["👍", "💬", "⬅️", "❌"]),
                &want(&["↩️", "💬", "⬅️", "❌"])
            ),
            0
        );
        // Results: ⏩ appears after ⏪.
        assert_eq!(
            buttons_kept(&had(&["⏪", "🏠", "⬅️"]), &want(&["⏪", "⏩", "🏠", "⬅️"])),
            1
        );
        // Fewer buttons: the common start stays.
        assert_eq!(buttons_kept(&had(&["😩", "😕", "⬅️"]), &want(&["😩"])), 1);
        // The emoji variation selector doesn't matter.
        assert_eq!(buttons_kept(&had(&["➡\u{fe0f}"]), &want(&["➡"])), 1);
        assert_eq!(buttons_kept(&[], &want(&["📝"])), 0);
    }

    #[test]
    fn a_long_reaction_row_is_replaced_if_its_order_would_change() {
        let had = |keys: &[&str]| -> Vec<(String, String)> {
            keys.iter()
                .map(|k| (k.to_string(), format!("${k}")))
                .collect()
        };
        let want = |keys: &[&str]| -> Vec<String> { keys.iter().map(|k| k.to_string()).collect() };
        let old = had(&["⬅️", "😩", "😕", "🙂", "😐", "🙁", "😭", "➡️", "🫠", "🤔"]);
        let new = want(&["➡️", "⬅️", "😩", "😕", "🙂", "😐", "🙁", "😭", "🫠", "🤔"]);
        assert!(!can_edit_reaction_order(&old, &new));
        assert!(can_edit_reaction_order(
            &had(&["➡️", "⬅️", "😩", "😕", "🙂", "😐", "🙁", "😭", "🫠", "🤔"]),
            &new
        ));
        assert!(can_edit_reaction_order(&old, &want(&["⬅️", "✅"])));
    }
    use crate::db::memory_connection;

    #[test]
    fn duplicate_emojis_become_numbers_and_numbers_can_be_typed() {
        let cat = Catalog::load(&memory_connection()).unwrap();
        let kitchens = cat.locations.iter().find(|l| l.key == "kitchens").unwrap();
        let mut f = Flow::default();
        f.push(Screen::Location {
            parent: Some(kitchens.id),
        });
        let ctx = Ctx {
            cat: Some(&cat),
            ..Default::default()
        };
        let v = view(&f, &ctx);
        assert_eq!(v.keys(), ["1️⃣", "2️⃣", "3️⃣", "4️⃣", BACK, CANCEL]);
        assert!(v.text.contains("2️⃣ 🍳 Kitchen 2"));
        let kitchen2 = cat.locations.iter().find(|l| l.key == "kitchen.2").unwrap();
        assert_eq!(resolve(&v, "2️⃣"), Some(Action::Location(kitchen2.id)));
        assert_eq!(typed(&v, " 2 "), Some(Action::Location(kitchen2.id)));
        assert_eq!(typed(&v, "cancel"), Some(Action::Cancel));
        assert_eq!(typed(&v, "weiter"), Some(Action::Next));
        assert_eq!(typed(&v, "next"), Some(Action::Next));
        // Top level: distinct emojis are their own buttons.
        let mut f = Flow::default();
        f.push(Screen::Location { parent: None });
        let v = view(&f, &ctx);
        assert!(v.keys().contains(&"🍳".to_owned()));
        assert!(v.keys().contains(&"⏭️".to_owned()));
    }

    #[test]
    fn options_after_the_tenth_can_be_selected_by_their_displayed_number() {
        let items: Vec<(String, String)> = (1..=12)
            .map(|i| ("🙂".to_owned(), format!("choice {i}")))
            .collect();
        let mut builder = Builder::new("Pick a choice");
        builder.options(&items, |i| Action::Value((i + 1) as i64));
        let view = builder.done();

        assert!(view.text.contains("1️⃣ 1. 🙂 choice 1"));
        assert!(view.text.contains("11. 🙂 choice 11"));
        assert!(view.text.contains("12. 🙂 choice 12"));
        assert_eq!(typed(&view, "11"), Some(Action::Value(11)));
        assert_eq!(typed(&view, "12."), Some(Action::Value(12)));
    }

    /// A one-question check-in whose question picks several of `choices`.
    fn multiple(choices: &[&str], required: bool) -> (Run, Version) {
        let q = Version {
            id: 1,
            version: 1,
            kind: QKind::Multiple,
            text: "Where?".to_owned(),
            config: Default::default(),
            required,
            allow_comment: false,
            asks_suggestion: false,
            metric: None,
            problem: None,
            choices: choices
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let (emoji, label) = c.split_once(' ').unwrap();
                    crate::surveys::Choice {
                        choice_id: i as i64 + 1,
                        version_id: i as i64 + 1,
                        emoji: emoji.to_owned(),
                        label: label.to_owned(),
                        problem: None,
                        is_other: false,
                    }
                })
                .collect(),
        };
        let run = Run {
            id: 1,
            emoji: "🧹".to_owned(),
            title: "Cleaning".to_owned(),
            intro: String::new(),
            target: crate::checkins::Target::House,
            identities: Vec::new(),
            plenum_id: None,
            opens_at: Utc::now(),
            closes_at: Utc::now(),
            reminder_at: None,
            announce: false,
            open: true,
            questions: vec![q.clone()],
        };
        (run, q)
    }

    #[test]
    fn a_multiple_choice_past_the_tenth_is_listed_with_its_number() {
        let choices: Vec<String> = (1..=12).map(|i| format!("🙂 choice{i}")).collect();
        let choices: Vec<&str> = choices.iter().map(String::as_str).collect();
        let (run, q) = multiple(&choices, true);
        let view = question(&run, 0, &q, None);
        assert!(view.text.contains("1️⃣ 1. ⬜ 🙂 choice1"), "{}", view.text);
        assert!(view.text.contains("\n11. ⬜ 🙂 choice11"), "{}", view.text);
        assert!(view.text.contains("\n12. ⬜ 🙂 choice12"), "{}", view.text);
        assert_eq!(typed(&view, "12"), Some(Action::Toggle(12)));
    }

    #[test]
    fn choices_are_numbered_exactly_when_their_buttons_are_reordered() {
        let seven = [
            "🍳 Kitchen",
            "🚿 Baths",
            "🪜 Stairs",
            "🛋️ Rooms",
            "♻️ Rubbish",
            "🧺 Laundry",
            "🌿 Garden",
        ];
        // 7 choices, ➡️ Next, ⬅️ Back, ❌ Cancel: more than Element shows.
        let (run, q) = multiple(&seven, true);
        let view = question(&run, 0, &q, None);
        assert!(crowded(view.buttons.len()));
        assert!(same_key(&view.keys()[0], NEXT));
        assert!(view.text.contains("🍳 1. ⬜ Kitchen"), "{}", view.text);
        assert!(view.text.contains("🌿 7. ⬜ Garden"), "{}", view.text);
        assert!(view.text.contains("send their numbers"), "{}", view.text);
        // Three fit: no numbers, and the text does not promise any.
        let (run, q) = multiple(&seven[..3], true);
        let view = question(&run, 0, &q, None);
        assert!(!crowded(view.buttons.len()));
        assert!(view.text.contains("🍳 ⬜ Kitchen"), "{}", view.text);
        assert!(!view.text.contains("1."), "{}", view.text);
        assert!(
            view.text.contains("👆 Tap choices, then ➡️ Next:"),
            "{}",
            view.text
        );
        assert!(!view.text.contains("numbers"), "{}", view.text);
        // Single choice: the hint (and its gap) only on a crowded screen.
        let items: Vec<(String, String)> = seven[..3]
            .iter()
            .map(|c| {
                let (e, l) = c.split_once(' ').unwrap();
                (e.to_owned(), l.to_owned())
            })
            .collect();
        let mut b = Builder::new("Pick one");
        b.gap().options(&items, |i| Action::Value(i as i64));
        let view = b.nav();
        assert_eq!(
            view.text,
            format!(
                "Pick one\n\n🍳 Kitchen\n🚿 Baths\n🪜 Stairs\n\n{}",
                nav_line()
            )
        );
    }

    #[test]
    fn every_screen_but_the_menu_has_back_and_cancel_and_keys_are_unique() {
        let cat = Catalog::load(&memory_connection()).unwrap();
        let ctx = Ctx {
            cat: Some(&cat),
            next_plenum: Some(("Sunday, 18 October".into(), "in 13 days".into())),
            ..Default::default()
        };
        let screens = [
            Screen::Topic { parent: None },
            Screen::Topic {
                parent: Some(cat.topics[0].id),
            },
            Screen::Location { parent: None },
            Screen::Text,
            Screen::Suggestion,
            Screen::Identity,
            Screen::Floor,
            Screen::Retention,
            Screen::Preview,
            Screen::ManageEnter,
            Screen::Manage,
            Screen::Privacy,
            Screen::ShareIdea,
            Screen::SimilarIdea { id: 1 },
            Screen::Results,
            Screen::ResultsTopic { topic: 1, page: 0 },
            Screen::ResultsFloors { topic: 1, page: 0 },
            Screen::ResultsHistory,
            Screen::ResultsPlenum { plenum: 1 },
            Screen::Ideas,
            Screen::Idea { id: 1 },
            Screen::IdeaComment { id: 1 },
            Screen::CheckinShareIdea { index: 0 },
            Screen::CheckinSimilarIdea { index: 0, id: 1 },
        ];
        for screen in screens {
            let mut f = Flow::default();
            f.push(screen.clone());
            let v = view(&f, &ctx);
            let keys = v.keys();
            assert!(keys.contains(&BACK.to_owned()), "{screen:?}");
            assert!(keys.contains(&CANCEL.to_owned()), "{screen:?}");
            let mut unique = keys.clone();
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), keys.len(), "{screen:?}");
        }
        let menu = view(&Flow::default(), &ctx);
        assert!(menu.text.contains("Next plenum: **Sunday, 18 October**"));
        assert_eq!(menu.keys(), ["📝", "📊", "💡", "📈", "🔑", "🔐"]);
    }

    #[test]
    fn the_receipt_shows_the_code_only_when_there_is_one() {
        let anon = submitted(
            Identity::Anonymous,
            Some("C7MJ-4P2K-XQ7D-9FRT"),
            Retention::Keep,
            None,
        );
        assert!(anon.contains("`C7MJ-4P2K-XQ7D-9FRT`"));
        let named = submitted(
            Identity::Named,
            None,
            Retention::DeleteAfterPlenum,
            Some("Sunday, 18 October"),
        );
        assert!(!named.contains('`'));
        assert!(named.contains("deleted after the plenum on Sunday, 18 October"));
    }
}
