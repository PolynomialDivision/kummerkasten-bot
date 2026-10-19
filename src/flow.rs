//! The conversation as an explicit state machine: a stack of screens (the
//! top one is showing; ⬅️ Back pops it) and the draft collected so far:
//! feedback or check-in answers. Pure: no Matrix, no database; `engine`
//! runs the effects.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    catalog::Catalog,
    checkins::{Answer, Run},
    submissions::{Identity, Retention},
    surveys::QKind,
};

/// Longest text accepted in one message.
pub const MAX_TEXT: usize = 4000;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "screen", rename_all = "snake_case")]
pub enum Screen {
    Menu,
    /// Pick a topic (`parent: None`) or a subtopic of `parent`.
    Topic {
        parent: Option<i64>,
    },
    Location {
        parent: Option<i64>,
    },
    Text,
    Suggestion,
    Identity,
    Floor,
    Retention,
    Preview,
    /// The receipt with a deletion code, until "I've saved the code".
    CodeSaved,
    ManageEnter,
    /// Looking at one submission (which one is kept in memory only).
    Manage,
    Privacy,
    /// 💡 Would you like to share this as an idea? (feedback)
    ShareIdea,
    /// A similar idea exists: support it, comment, or create yours.
    SimilarIdea {
        id: i64,
    },
    /// 📈 Results: the overview, a topic (centred on its `page`-th latest
    /// check-in), its floor breakdown, previous plenums, one of them.
    Results,
    ResultsTopic {
        topic: i64,
        page: usize,
    },
    ResultsFloors {
        topic: i64,
        page: usize,
    },
    ResultsHistory,
    ResultsPlenum {
        plenum: i64,
    },
    /// 💡 The board, one idea, writing a comment on it.
    Ideas,
    Idea {
        id: i64,
    },
    IdeaComment {
        id: i64,
    },
    /// Open (and recently answered) check-ins.
    Checkins,
    CheckinIntro,
    CheckinQuestion {
        index: usize,
    },
    CheckinComment {
        index: usize,
    },
    CheckinIdentity,
    CheckinPreview,
    /// A check-in's "what would you change?" answer: share it as an idea?
    CheckinShareIdea {
        index: usize,
    },
    CheckinSimilarIdea {
        index: usize,
        id: i64,
    },
}

/// What happens with someone's idea, if they chose 🌍 Add to Ideas.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum IdeaPlan {
    /// A new public idea.
    New,
    /// 👍 the similar one instead.
    Support { id: i64 },
    /// Add it as a 💬 comment on the similar one.
    Comment { id: i64 },
}

/// 🌍 / 🗣️ / 🔒: sharing a suggestion.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Share {
    Public,
    PlenumOnly,
    Private,
}

/// Answers so far to one check-in run (never stored as a response before
/// ✅ Submit).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct CheckinDraft {
    pub run: i64,
    /// By question position (0-based).
    #[serde(default)]
    pub answers: BTreeMap<usize, Answer>,
    pub identity: Option<Identity>,
    /// Answers the author chose to share as ideas, by question.
    #[serde(default)]
    pub ideas: BTreeMap<usize, IdeaPlan>,
}

/// Stored with the session: a field added later needs a default (missing
/// in older rows), and the tests in `a_saved_flow_*` pin what is stored.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Draft {
    pub topic: Option<i64>,
    pub location: Option<i64>,
    pub body: Option<String>,
    pub suggestion: Option<String>,
    pub identity: Option<Identity>,
    /// Their floor, picked during this flow (saved with the submission).
    pub floor: Option<i64>,
    pub retention: Option<Retention>,
    pub for_plenum: bool,
    #[serde(default)]
    pub checkin: Option<CheckinDraft>,
    /// 🌍 Shared as an idea (only with that choice).
    #[serde(default)]
    pub idea: Option<IdeaPlan>,
}

impl Default for Draft {
    fn default() -> Self {
        Self {
            topic: None,
            location: None,
            body: None,
            suggestion: None,
            identity: None,
            floor: None,
            retention: None,
            for_plenum: true,
            checkin: None,
            idea: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Flow {
    pub stack: Vec<Screen>,
    pub draft: Draft,
}

impl Default for Flow {
    fn default() -> Self {
        Self {
            stack: vec![Screen::Menu],
            draft: Draft::default(),
        }
    }
}

impl Flow {
    /// A saved flow read back. One this version can't read (a release
    /// changed what is stored) is logged, and a check-in in it is kept:
    /// from its intro, with the answers that still read. Only the position
    /// of the problem is logged, never the text around it.
    pub fn restore(json: &str) -> Flow {
        match serde_json::from_str(json) {
            Ok(flow) => flow,
            Err(error) => {
                let kept = salvage_checkin(json);
                tracing::error!(
                    kind = ?error.classify(),
                    line = error.line(),
                    column = error.column(),
                    kept_checkin = kept.is_some(),
                    "A saved conversation could not be read"
                );
                kept.unwrap_or_default()
            }
        }
    }

    pub fn screen(&self) -> &Screen {
        self.stack.last().unwrap_or(&Screen::Menu)
    }

    /// Somewhere past the main menu (something to clean up on expiry).
    pub fn in_progress(&self) -> bool {
        self.stack.len() > 1 && self.screen() != &Screen::CodeSaved
    }

    pub fn push(&mut self, screen: Screen) {
        self.stack.push(screen);
    }

    /// Whether the showing screen takes a typed message as its answer.
    pub fn wants_text(&self, run: Option<&Run>) -> bool {
        match self.screen() {
            Screen::Text
            | Screen::Suggestion
            | Screen::ManageEnter
            | Screen::IdeaComment { .. }
            | Screen::CheckinComment { .. } => true,
            Screen::CheckinQuestion { index } => run
                .and_then(|r| r.questions.get(*index))
                .is_some_and(|q| q.kind == QKind::Text),
            _ => false,
        }
    }

    /// The topic picked is an idea / wish (its text step asks for the idea,
    /// and there's no separate "what would you change" step).
    pub fn is_idea(&self, cat: &Catalog) -> bool {
        self.draft
            .topic
            .and_then(|t| cat.topic(t))
            .is_some_and(|t| t.is_idea)
    }

    /// The check-in run this flow is answering, if any.
    pub fn run(&self) -> Option<i64> {
        self.draft.checkin.as_ref().map(|c| c.run)
    }

    /// The text that would become an idea: the suggestion, or for the 💡
    /// topic what they wrote.
    pub fn idea_text(&self, cat: &Catalog) -> Option<&str> {
        if self.is_idea(cat) {
            self.draft.body.as_deref()
        } else {
            self.draft.suggestion.as_deref()
        }
    }
}

/// The check-in of a flow that no longer reads as a whole: its run and the
/// answers that still read, back at the intro.
fn salvage_checkin(json: &str) -> Option<Flow> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let checkin = value.get("draft")?.get("checkin")?;
    let run = checkin.get("run")?.as_i64()?;
    let answers = checkin
        .get("answers")
        .and_then(|a| serde_json::from_value(a.clone()).ok())
        .unwrap_or_default();
    let mut flow = Flow::default();
    flow.draft.checkin = Some(CheckinDraft {
        run,
        answers,
        identity: None,
        ideas: BTreeMap::new(),
    });
    flow.push(Screen::CheckinIntro);
    Some(flow)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MenuItem {
    Share,
    Checkins,
    Ideas,
    Results,
    Manage,
    Privacy,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Open(MenuItem),
    Topic(i64),
    Location(i64),
    NoLocation,
    Floor(i64),
    Identity(Identity),
    Retention(Retention),
    Text(String),
    /// Skip an optional step / keep what was written.
    Skip,
    TogglePlenum,
    Submit,
    /// "I've saved the code".
    CodeSaved,
    /// Back to the main menu (after a result).
    Home,
    /// Open one of their named submissions.
    Named(i64),
    Delete,
    /// Results: a topic, older/newer check-in, by floor, history, a plenum.
    OpenTopic(i64),
    Older,
    Newer,
    Floors,
    History,
    OpenPlenum(i64),
    /// Ideas: open one, 👍 / take it back, 💬.
    OpenIdea(i64),
    SupportIdea,
    WithdrawSupport,
    CommentIdea,
    /// Sharing a suggestion: 🌍 / 🗣️ / 🔒; with a similar idea: 👍 / 💬 / ➕.
    Share(Share),
    SupportExisting,
    CommentExisting,
    CreateSeparately,
    /// Check-ins: pick a run, start it, answer.
    OpenRun(i64),
    Begin,
    Value(i64),
    Choose(i64),
    Toggle(i64),
    Next,
    Back,
    Cancel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Show the (new) top screen.
    Show,
    /// The same screen changed (a box ticked): update it in place.
    Refresh,
    /// Throw the conversation away and show a fresh menu.
    Cancel,
    Submit,
    SubmitCheckin,
    /// Remove the message with the deletion code.
    ForgetCode,
    LookupToken(String),
    OpenNamed(i64),
    Delete,
    /// 👍 / take back / 💬 on an idea, then show the idea again.
    Support(i64),
    Withdraw(i64),
    Comment(i64, String),
    /// Stay, with a short note.
    Hint(&'static str),
}

/// What a step may look at besides the flow.
pub struct Env<'a> {
    pub cat: &'a Catalog,
    /// The floor they told us before.
    pub own_floor: Option<i64>,
    /// The check-in run being answered.
    pub run: Option<&'a Run>,
    /// On a "share as an idea?" screen: a similar existing idea.
    pub similar: Option<i64>,
    /// On a results topic: how many of its check-ins there are to page.
    pub pages: usize,
    /// Survey-only mode: no offer to share an answer as an idea, and ⬅️
    /// never leaves the check-in (its intro is the first screen).
    pub survey: bool,
}

/// Survey-only mode: a check-in's flow, `[Menu, CheckinIntro, …]`. The
/// `Menu` underneath is never shown; ⬅️ stops at the intro.
pub const SURVEY_DEPTH: usize = 2;

/// `apply_in` without a check-in run.
#[cfg(test)]
pub fn apply(flow: &mut Flow, action: Action, cat: &Catalog, own_floor: Option<i64>) -> Effect {
    apply_in(
        flow,
        action,
        &Env {
            cat,
            own_floor,
            run: None,
            similar: None,
            pages: 0,
            survey: false,
        },
    )
}

/// Apply `action` to `flow`.
pub fn apply_in(flow: &mut Flow, action: Action, env: &Env) -> Effect {
    use Screen as S;
    let cat = env.cat;
    match action {
        Action::Cancel => {
            *flow = Flow::default();
            return Effect::Cancel;
        }
        Action::Back => {
            let floor = if env.survey { SURVEY_DEPTH } else { 1 };
            if flow.stack.len() > floor {
                flow.stack.pop();
            }
            return Effect::Show;
        }
        _ => {}
    }
    match (flow.screen().clone(), action) {
        (S::Menu, Action::Open(item)) => {
            *flow = Flow::default();
            flow.push(match item {
                MenuItem::Share => S::Topic { parent: None },
                MenuItem::Manage => S::ManageEnter,
                MenuItem::Privacy => S::Privacy,
                MenuItem::Checkins => S::Checkins,
                MenuItem::Ideas => S::Ideas,
                MenuItem::Results => S::Results,
            });
            Effect::Show
        }
        (S::Topic { parent }, Action::Topic(id)) => {
            let Some(topic) = cat.topic(id).filter(|t| t.parent_id == parent) else {
                return Effect::Hint("👆 Pick one of the topics.");
            };
            if cat.topic_children(Some(topic.id)).is_empty() {
                flow.draft.topic = Some(topic.id);
                flow.push(S::Location { parent: None });
            } else {
                flow.push(S::Topic {
                    parent: Some(topic.id),
                });
            }
            Effect::Show
        }
        (S::Location { parent }, Action::Location(id)) => {
            let Some(place) = cat.location(id).filter(|l| l.parent_id == parent) else {
                return Effect::Hint("👆 Pick one of the places.");
            };
            if cat.location_children(Some(place.id)).is_empty() {
                flow.draft.location = Some(place.id);
                flow.push(S::Text);
            } else {
                flow.push(S::Location {
                    parent: Some(place.id),
                });
            }
            Effect::Show
        }
        (S::Location { .. }, Action::NoLocation) => {
            flow.draft.location = None;
            flow.push(S::Text);
            Effect::Show
        }
        (S::Text, Action::Text(text)) => match clean(&text) {
            Err(hint) => Effect::Hint(hint),
            Ok(text) => {
                flow.draft.body = Some(text);
                after_text(flow, cat);
                Effect::Show
            }
        },
        (S::Text, Action::Skip) if flow.draft.body.is_some() => {
            after_text(flow, cat);
            Effect::Show
        }
        (S::Suggestion, Action::Text(text)) => match clean(&text) {
            Err(hint) => Effect::Hint(hint),
            Ok(text) => {
                flow.draft.suggestion = Some(text);
                // Something to change: ask whether to share it as an idea.
                flow.push(S::ShareIdea);
                Effect::Show
            }
        },
        (S::Suggestion, Action::Skip) => {
            flow.draft.suggestion = None;
            flow.draft.idea = None;
            flow.push(S::Identity);
            Effect::Show
        }
        (S::ShareIdea, Action::Share(share)) => {
            match share {
                Share::Public => match env.similar {
                    Some(id) => {
                        flow.push(S::SimilarIdea { id });
                        return Effect::Show;
                    }
                    None => flow.draft.idea = Some(IdeaPlan::New),
                },
                // Not on the board; the feedback itself goes to the plenum
                // (🗣️) or not (🔒): both can still be changed in the preview.
                Share::PlenumOnly => {
                    flow.draft.idea = None;
                    flow.draft.for_plenum = true;
                }
                Share::Private => {
                    flow.draft.idea = None;
                    flow.draft.for_plenum = false;
                }
            }
            flow.push(S::Identity);
            Effect::Show
        }
        (
            S::SimilarIdea { id },
            action @ (Action::SupportExisting | Action::CommentExisting | Action::CreateSeparately),
        ) => {
            flow.draft.idea = Some(match action {
                Action::SupportExisting => IdeaPlan::Support { id },
                Action::CommentExisting => IdeaPlan::Comment { id },
                _ => IdeaPlan::New,
            });
            flow.push(S::Identity);
            Effect::Show
        }
        (S::Identity, Action::Identity(identity)) => {
            flow.draft.identity = Some(identity);
            flow.push(if needs_floor(flow, env, identity) {
                S::Floor
            } else {
                S::Retention
            });
            Effect::Show
        }
        (S::Floor, Action::Floor(id)) => {
            if !cat.floors().iter().any(|f| f.id == id) {
                return Effect::Hint("👆 Pick your floor.");
            }
            flow.draft.floor = Some(id);
            flow.push(if flow.draft.checkin.is_some() {
                S::CheckinPreview
            } else {
                S::Retention
            });
            Effect::Show
        }
        (S::Retention, Action::Retention(retention)) => {
            flow.draft.retention = Some(retention);
            flow.push(S::Preview);
            Effect::Show
        }
        (S::Preview, Action::TogglePlenum) => {
            flow.draft.for_plenum = !flow.draft.for_plenum;
            Effect::Show
        }
        (S::Preview, Action::Submit) => Effect::Submit,
        (S::CodeSaved, Action::CodeSaved) => {
            *flow = Flow::default();
            Effect::ForgetCode
        }
        (_, Action::Home) => {
            *flow = Flow::default();
            Effect::Show
        }
        // ── Results ────────────────────────────────────────────────────────
        (S::Results, Action::OpenTopic(topic)) => {
            flow.push(S::ResultsTopic { topic, page: 0 });
            Effect::Show
        }
        (S::Results, Action::History) => {
            flow.push(S::ResultsHistory);
            Effect::Show
        }
        (S::ResultsTopic { topic, page }, Action::Older) if page + 1 < env.pages => {
            flow.stack.pop();
            flow.push(S::ResultsTopic {
                topic,
                page: page + 1,
            });
            Effect::Refresh
        }
        (S::ResultsTopic { topic, page }, Action::Newer) if page > 0 => {
            flow.stack.pop();
            flow.push(S::ResultsTopic {
                topic,
                page: page - 1,
            });
            Effect::Refresh
        }
        (S::ResultsTopic { .. }, Action::Older | Action::Newer) => {
            Effect::Hint("📊 That's as far as the check-ins go.")
        }
        (S::ResultsTopic { topic, page }, Action::Floors) => {
            flow.push(S::ResultsFloors { topic, page });
            Effect::Show
        }
        (S::ResultsHistory, Action::OpenPlenum(plenum)) => {
            flow.push(S::ResultsPlenum { plenum });
            Effect::Show
        }

        // ── Ideas ──────────────────────────────────────────────────────────
        (S::Ideas, Action::OpenIdea(id)) => {
            flow.push(S::Idea { id });
            Effect::Show
        }
        (S::Idea { id }, Action::SupportIdea) => Effect::Support(id),
        (S::Idea { id }, Action::WithdrawSupport) => Effect::Withdraw(id),
        (S::Idea { id }, Action::CommentIdea) => {
            flow.push(S::IdeaComment { id });
            Effect::Show
        }
        (S::IdeaComment { id }, Action::Text(text)) => match clean(&text) {
            Err(hint) => Effect::Hint(hint),
            Ok(text) => {
                flow.stack.pop();
                Effect::Comment(id, text)
            }
        },

        (S::ManageEnter, Action::Text(text)) => Effect::LookupToken(text),
        (S::ManageEnter, Action::Named(id)) => Effect::OpenNamed(id),
        (S::Manage, Action::Delete) => Effect::Delete,

        // ── Check-ins ──────────────────────────────────────────────────────
        (S::Checkins, Action::OpenRun(run)) => {
            flow.draft = Draft {
                checkin: Some(CheckinDraft {
                    run,
                    answers: BTreeMap::new(),
                    identity: None,
                    ideas: BTreeMap::new(),
                }),
                ..Draft::default()
            };
            flow.push(S::CheckinIntro);
            Effect::Show
        }
        (screen, action) if is_checkin(&screen) => match env.run {
            Some(run) if flow.run() == Some(run.id) => checkin(flow, screen, action, run, env),
            _ => Effect::Hint("⏳ I've lost track of that check-in. Open it again from the menu."),
        },
        (_, Action::Text(_)) => Effect::Hint("👆 Tap one of the buttons under my last message."),
        _ => Effect::Hint("👆 That button isn't for this step."),
    }
}

fn is_checkin(screen: &Screen) -> bool {
    matches!(
        screen,
        Screen::CheckinIntro
            | Screen::CheckinQuestion { .. }
            | Screen::CheckinComment { .. }
            | Screen::CheckinIdentity
            | Screen::CheckinPreview
            | Screen::CheckinShareIdea { .. }
            | Screen::CheckinSimilarIdea { .. }
    )
}

/// 🏠 chosen, and we don't know their floor yet.
fn needs_floor(flow: &Flow, env: &Env, identity: Identity) -> bool {
    identity == Identity::Floor && env.own_floor.or(flow.draft.floor).is_none()
}

fn checkin(flow: &mut Flow, screen: Screen, action: Action, run: &Run, env: &Env) -> Effect {
    use Screen as S;
    let n = run.questions.len();
    let answer = answer_mut;
    match (screen, action) {
        (S::CheckinIntro, Action::Begin) if n > 0 => {
            flow.push(S::CheckinQuestion { index: 0 });
            Effect::Show
        }
        (S::CheckinQuestion { index }, action) => {
            let Some(q) = run.questions.get(index) else {
                return Effect::Hint("⏳ That question is gone. Open the check-in again.");
            };
            match (q.kind, action) {
                (_, Action::Skip) if !q.required => {
                    *answer(flow, index) = Answer {
                        skipped: true,
                        ..Answer::default()
                    };
                    next_question(flow, run, index, env)
                }
                (QKind::Scale, Action::Value(v)) if q.config.steps().contains(&v) => {
                    let comment = answer(flow, index).comment.take();
                    *answer(flow, index) = Answer {
                        value: Some(v),
                        comment,
                        ..Answer::default()
                    };
                    answered(flow, run, index, env)
                }
                (QKind::YesNo, Action::Value(v @ (0 | 1))) => {
                    let comment = answer(flow, index).comment.take();
                    *answer(flow, index) = Answer {
                        value: Some(v),
                        comment,
                        ..Answer::default()
                    };
                    answered(flow, run, index, env)
                }
                (QKind::Single, Action::Choose(c))
                    if q.choices.iter().any(|x| x.version_id == c) =>
                {
                    let comment = answer(flow, index).comment.take();
                    *answer(flow, index) = Answer {
                        choices: vec![c],
                        comment,
                        ..Answer::default()
                    };
                    answered(flow, run, index, env)
                }
                (QKind::Multiple, Action::Toggle(c))
                    if q.choices.iter().any(|x| x.version_id == c) =>
                {
                    let a = answer(flow, index);
                    a.skipped = false;
                    match a.choices.iter().position(|x| *x == c) {
                        Some(pos) => {
                            a.choices.remove(pos);
                        }
                        None => a.choices.push(c),
                    }
                    Effect::Refresh
                }
                (QKind::Multiple, Action::Next) => {
                    if answer(flow, index).choices.is_empty() {
                        if q.required {
                            return Effect::Hint("👆 Tick at least one first.");
                        }
                        answer(flow, index).skipped = true;
                        return next_question(flow, run, index, env);
                    }
                    answered(flow, run, index, env)
                }
                (QKind::Text, Action::Text(text)) => match clean(&text) {
                    Err(hint) => Effect::Hint(hint),
                    Ok(text) => {
                        *answer(flow, index) = Answer {
                            text: Some(text),
                            ..Answer::default()
                        };
                        if q.asks_suggestion && !env.survey {
                            // "What would you change?": offer it as an idea.
                            flow.push(S::CheckinShareIdea { index });
                            return Effect::Show;
                        }
                        next_question(flow, run, index, env)
                    }
                },
                (QKind::Text, _) => Effect::Hint("✍️ Just write your answer as a message."),
                (_, Action::Text(_)) => {
                    Effect::Hint("👆 Tap one of the buttons under the question.")
                }
                _ => Effect::Hint("👆 That button isn't for this question."),
            }
        }
        (S::CheckinComment { index }, Action::Text(text)) => match clean(&text) {
            Err(hint) => Effect::Hint(hint),
            Ok(text) => {
                answer(flow, index).comment = Some(text);
                next_question(flow, run, index, env)
            }
        },
        (S::CheckinComment { index }, Action::Skip) => {
            answer(flow, index).comment = None;
            next_question(flow, run, index, env)
        }
        (S::CheckinIdentity, Action::Identity(identity)) if run.identities.contains(&identity) => {
            set_identity(flow, run, identity, env)
        }
        (S::CheckinShareIdea { index }, Action::Share(share)) => {
            let ideas = &mut flow.draft.checkin.as_mut().expect("answering").ideas;
            match share {
                Share::Public => match env.similar {
                    Some(id) => {
                        flow.push(S::CheckinSimilarIdea { index, id });
                        return Effect::Show;
                    }
                    None => {
                        ideas.insert(index, IdeaPlan::New);
                    }
                },
                Share::PlenumOnly | Share::Private => {
                    ideas.remove(&index);
                }
            }
            next_question(flow, run, index, env)
        }
        (
            S::CheckinSimilarIdea { index, id },
            action @ (Action::SupportExisting | Action::CommentExisting | Action::CreateSeparately),
        ) => {
            let plan = match action {
                Action::SupportExisting => IdeaPlan::Support { id },
                Action::CommentExisting => IdeaPlan::Comment { id },
                _ => IdeaPlan::New,
            };
            flow.draft
                .checkin
                .as_mut()
                .expect("answering")
                .ideas
                .insert(index, plan);
            next_question(flow, run, index, env)
        }
        (S::CheckinPreview, Action::Submit) => Effect::SubmitCheckin,
        (_, Action::Text(_)) => Effect::Hint("👆 Tap one of the buttons under my last message."),
        _ => Effect::Hint("👆 That button isn't for this step."),
    }
}

/// The draft answer to question `index` (created empty if needed).
fn answer_mut(flow: &mut Flow, index: usize) -> &mut Answer {
    flow.draft
        .checkin
        .as_mut()
        .expect("only called while answering a check-in")
        .answers
        .entry(index)
        .or_default()
}

/// After an answer: its comment step if the question has one, else on.
fn answered(flow: &mut Flow, run: &Run, index: usize, env: &Env) -> Effect {
    if run.questions[index].allow_comment {
        flow.push(Screen::CheckinComment { index });
        Effect::Show
    } else {
        next_question(flow, run, index, env)
    }
}

fn next_question(flow: &mut Flow, run: &Run, index: usize, env: &Env) -> Effect {
    if index + 1 < run.questions.len() {
        flow.push(Screen::CheckinQuestion { index: index + 1 });
        return Effect::Show;
    }
    match run.identities.as_slice() {
        [only] => set_identity(flow, run, *only, env),
        _ => {
            flow.push(Screen::CheckinIdentity);
            Effect::Show
        }
    }
}

fn set_identity(flow: &mut Flow, _run: &Run, identity: Identity, env: &Env) -> Effect {
    if let Some(c) = flow.draft.checkin.as_mut() {
        c.identity = Some(identity);
    }
    flow.push(if needs_floor(flow, env, identity) {
        Screen::Floor
    } else {
        Screen::CheckinPreview
    });
    Effect::Show
}

fn after_text(flow: &mut Flow, cat: &Catalog) {
    flow.push(if flow.is_idea(cat) {
        // The idea itself: ask whether to share it on the board.
        Screen::ShareIdea
    } else {
        Screen::Suggestion
    });
}

fn clean(text: &str) -> Result<String, &'static str> {
    let text = text.trim();
    if text.is_empty() {
        Err("✍️ Write a few words. Anything is fine.")
    } else if text.chars().count() > MAX_TEXT {
        Err("📏 That's a bit long for one message. Could you shorten it?")
    } else {
        Ok(text.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory_connection;

    fn cat() -> Catalog {
        Catalog::load(&memory_connection()).unwrap()
    }

    fn topic(cat: &Catalog, key: &str) -> i64 {
        cat.topics.iter().find(|t| t.key == key).unwrap().id
    }

    fn place(cat: &Catalog, key: &str) -> i64 {
        cat.locations.iter().find(|t| t.key == key).unwrap().id
    }

    /// Menu → … → Preview for a cleaning problem in Kitchen 2.
    fn to_preview(cat: &Catalog, identity: Identity) -> Flow {
        let mut f = Flow::default();
        let steps = [
            Action::Open(MenuItem::Share),
            Action::Topic(topic(cat, "cleaning")),
            Action::Topic(topic(cat, "cleaning.skipped_turns")),
            Action::Location(place(cat, "kitchens")),
            Action::Location(place(cat, "kitchen.2")),
            Action::Text("The bathroom often isn't cleaned.".into()),
            Action::Skip,
            Action::Identity(identity),
        ];
        for step in steps {
            assert_eq!(
                apply(&mut f, step.clone(), cat, None),
                Effect::Show,
                "{step:?}"
            );
        }
        if identity == Identity::Floor {
            assert_eq!(f.screen(), &Screen::Floor);
            apply(&mut f, Action::Floor(place(cat, "floor.3")), cat, None);
        }
        assert_eq!(
            apply(
                &mut f,
                Action::Retention(Retention::DeleteAfterPlenum),
                cat,
                None
            ),
            Effect::Show
        );
        f
    }

    #[test]
    fn the_share_flow_collects_a_complete_draft() {
        let cat = cat();
        let mut f = to_preview(&cat, Identity::Floor);
        assert_eq!(f.screen(), &Screen::Preview);
        assert_eq!(f.draft.topic, Some(topic(&cat, "cleaning.skipped_turns")));
        assert_eq!(f.draft.location, Some(place(&cat, "kitchen.2")));
        assert_eq!(f.draft.floor, Some(place(&cat, "floor.3")));
        assert_eq!(f.draft.retention, Some(Retention::DeleteAfterPlenum));
        assert!(f.draft.for_plenum);
        apply(&mut f, Action::TogglePlenum, &cat, None);
        assert!(!f.draft.for_plenum);
        assert_eq!(apply(&mut f, Action::Submit, &cat, None), Effect::Submit);
    }

    #[test]
    fn a_known_floor_is_not_asked_again() {
        let cat = cat();
        let mut f = to_preview(&cat, Identity::Anonymous);
        // Back to identity, choose floor, with the floor known already.
        while f.screen() != &Screen::Identity {
            apply(&mut f, Action::Back, &cat, None);
        }
        apply(
            &mut f,
            Action::Identity(Identity::Floor),
            &cat,
            Some(place(&cat, "floor.1")),
        );
        assert_eq!(f.screen(), &Screen::Retention);
    }

    #[test]
    fn back_goes_one_step_and_keeps_what_was_entered() {
        let cat = cat();
        let mut f = to_preview(&cat, Identity::Anonymous);
        let depth = f.stack.len();
        apply(&mut f, Action::Back, &cat, None);
        assert_eq!(f.screen(), &Screen::Retention);
        assert_eq!(f.stack.len(), depth - 1);
        // Back to the text: what was written is still there and can be kept.
        while f.screen() != &Screen::Text {
            apply(&mut f, Action::Back, &cat, None);
        }
        assert!(f.draft.body.is_some());
        assert_eq!(apply(&mut f, Action::Skip, &cat, None), Effect::Show);
        assert_eq!(f.screen(), &Screen::Suggestion);
        // Back all the way stops at the menu.
        for _ in 0..20 {
            apply(&mut f, Action::Back, &cat, None);
        }
        assert_eq!(f.stack, [Screen::Menu]);
    }

    #[test]
    fn cancel_from_every_screen_returns_to_a_fresh_menu() {
        let cat = cat();
        let mut reached = Vec::new();
        let full = to_preview(&cat, Identity::Floor);
        for depth in 1..=full.stack.len() {
            let mut f = full.clone();
            f.stack.truncate(depth);
            reached.push(f.screen().clone());
            assert_eq!(apply(&mut f, Action::Cancel, &cat, None), Effect::Cancel);
            assert_eq!(f, Flow::default());
        }
        for item in [
            MenuItem::Manage,
            MenuItem::Privacy,
            MenuItem::Checkins,
            MenuItem::Ideas,
        ] {
            let mut f = Flow::default();
            apply(&mut f, Action::Open(item), &cat, None);
            reached.push(f.screen().clone());
            assert_eq!(apply(&mut f, Action::Cancel, &cat, None), Effect::Cancel);
            assert_eq!(f, Flow::default());
        }
        assert!(reached.contains(&Screen::Preview));
        assert!(reached.contains(&Screen::Floor));
        assert!(reached.contains(&Screen::ManageEnter));
    }

    #[test]
    fn ideas_skip_the_change_step_and_bad_input_is_a_hint() {
        let cat = cat();
        let mut f = Flow::default();
        apply(&mut f, Action::Open(MenuItem::Share), &cat, None);
        apply(&mut f, Action::Topic(topic(&cat, "idea")), &cat, None);
        apply(&mut f, Action::NoLocation, &cat, None);
        assert_eq!(f.screen(), &Screen::Text);
        assert!(matches!(
            apply(&mut f, Action::Text("   ".into()), &cat, None),
            Effect::Hint(_)
        ));
        assert!(matches!(
            apply(&mut f, Action::Text("x".repeat(MAX_TEXT + 1)), &cat, None),
            Effect::Hint(_)
        ));
        apply(
            &mut f,
            Action::Text("Clean the bathrooms twice per week".into()),
            &cat,
            None,
        );
        // An idea: asked whether to share it on the board, never assumed.
        assert_eq!(f.screen(), &Screen::ShareIdea);
        assert_eq!(f.draft.idea, None);
        apply(&mut f, Action::Share(Share::Public), &cat, None);
        assert_eq!(f.draft.idea, Some(IdeaPlan::New));
        assert_eq!(f.screen(), &Screen::Identity);
        // Text where a button is expected; a subtopic of another topic.
        assert!(matches!(
            apply(&mut f, Action::Text("hi".into()), &cat, None),
            Effect::Hint(_)
        ));
        let mut g = Flow::default();
        apply(&mut g, Action::Open(MenuItem::Share), &cat, None);
        assert!(matches!(
            apply(
                &mut g,
                Action::Topic(topic(&cat, "cleaning.equipment")),
                &cat,
                None
            ),
            Effect::Hint(_)
        ));
    }

    #[test]
    fn a_flow_survives_a_restart() {
        let cat = cat();
        let f = to_preview(&cat, Identity::Named);
        let stored = serde_json::to_string(&f).unwrap();
        assert_eq!(serde_json::from_str::<Flow>(&stored).unwrap(), f);
    }

    /// A check-in half answered, as an earlier release stored it. If this
    /// stops reading, a deploy would throw away everyone's answers in
    /// progress: give the new field a default instead of changing this.
    const STORED_CHECKIN: &str = r#"{"stack":[{"screen":"menu"},{"screen":"checkin_intro"},{"screen":"checkin_question","index":1},{"screen":"checkin_comment","index":1}],"draft":{"topic":null,"location":null,"body":null,"suggestion":null,"identity":null,"floor":null,"retention":null,"for_plenum":true,"checkin":{"run":7,"answers":{"0":{"value":3,"choices":[],"text":null,"comment":null,"skipped":false},"1":{"value":null,"choices":[11,12],"text":null,"comment":"often","skipped":false}},"identity":"anonymous","ideas":{}},"idea":null}}"#;

    fn stored_checkin() -> Flow {
        let mut flow = Flow::default();
        flow.draft.checkin = Some(CheckinDraft {
            run: 7,
            answers: BTreeMap::from([
                (
                    0,
                    Answer {
                        value: Some(3),
                        ..Default::default()
                    },
                ),
                (
                    1,
                    Answer {
                        choices: vec![11, 12],
                        comment: Some("often".into()),
                        ..Default::default()
                    },
                ),
            ]),
            identity: Some(Identity::Anonymous),
            ideas: BTreeMap::new(),
        });
        flow.push(Screen::CheckinIntro);
        flow.push(Screen::CheckinQuestion { index: 1 });
        flow.push(Screen::CheckinComment { index: 1 });
        flow
    }

    #[test]
    fn a_saved_flow_from_an_earlier_release_still_reads() {
        assert_eq!(Flow::restore(STORED_CHECKIN), stored_checkin());
        assert_eq!(
            serde_json::to_string(&stored_checkin()).unwrap(),
            STORED_CHECKIN,
            "what is stored changed: old rows must still read (see above)"
        );
        // Rows from before a field existed: the field takes its default.
        let older = r#"{"stack":[{"screen":"menu"},{"screen":"checkin_intro"}],"draft":{"checkin":{"run":7}}}"#;
        let flow = Flow::restore(older);
        assert_eq!(flow.run(), Some(7));
        assert_eq!(flow.screen(), &Screen::CheckinIntro);
        assert!(flow.draft.for_plenum);
    }

    #[test]
    fn a_saved_flow_that_no_longer_reads_keeps_its_check_in() {
        // A screen this version doesn't know: back to the intro, answers kept.
        let unknown = STORED_CHECKIN.replace("checkin_comment", "checkin_photo");
        let flow = Flow::restore(&unknown);
        assert_eq!(flow.stack, vec![Screen::Menu, Screen::CheckinIntro]);
        let kept = flow.draft.checkin.unwrap();
        assert_eq!(kept.run, 7);
        assert_eq!(
            kept.answers,
            stored_checkin().draft.checkin.unwrap().answers
        );
        // Nothing to save: the menu, as before.
        assert_eq!(Flow::restore("{\"stack\":7}"), Flow::default());
        assert_eq!(Flow::restore("not json"), Flow::default());
    }

    // ── Check-ins ──────────────────────────────────────────────────────────

    use crate::checkins::tests::started;
    use crate::plenums::tests::{at, with_default_rule};

    /// The starting cleaning run (scale, multiple, optional text).
    fn cleaning_run() -> (Catalog, Run) {
        let now = at(2026, 11, 2, 12);
        let mut conn = with_default_rule(now);
        let run = started(&mut conn, now, Default::default());
        (Catalog::load(&conn).unwrap(), run)
    }

    fn step(f: &mut Flow, a: Action, cat: &Catalog, run: &Run, own_floor: Option<i64>) -> Effect {
        apply_in(
            f,
            a,
            &Env {
                cat,
                own_floor,
                run: Some(run),
                similar: None,
                pages: 0,
                survey: false,
            },
        )
    }

    fn to_checkin_preview(cat: &Catalog, run: &Run) -> Flow {
        let mut f = Flow::default();
        apply(&mut f, Action::Open(MenuItem::Checkins), cat, None);
        assert_eq!(
            step(&mut f, Action::OpenRun(run.id), cat, run, None),
            Effect::Show
        );
        assert_eq!(step(&mut f, Action::Begin, cat, run, None), Effect::Show);
        assert_eq!(step(&mut f, Action::Value(3), cat, run, None), Effect::Show);
        let [a, b] = [0, 1].map(|i| run.questions[1].choices[i].version_id);
        assert_eq!(
            step(&mut f, Action::Toggle(a), cat, run, None),
            Effect::Refresh
        );
        assert_eq!(
            step(&mut f, Action::Toggle(b), cat, run, None),
            Effect::Refresh
        );
        assert_eq!(
            step(&mut f, Action::Toggle(a), cat, run, None),
            Effect::Refresh
        );
        assert_eq!(step(&mut f, Action::Next, cat, run, None), Effect::Show);
        assert_eq!(
            step(
                &mut f,
                Action::Text("Fewer, clearer tasks.".into()),
                cat,
                run,
                None
            ),
            Effect::Show
        );
        // "What would you change?": offered as an idea, kept in the answers.
        assert_eq!(f.screen(), &Screen::CheckinShareIdea { index: 2 });
        assert_eq!(
            step(&mut f, Action::Share(Share::Private), cat, run, None),
            Effect::Show
        );
        assert!(f.draft.checkin.as_ref().unwrap().ideas.is_empty());
        assert_eq!(f.screen(), &Screen::CheckinIdentity);
        assert_eq!(
            step(
                &mut f,
                Action::Identity(Identity::Anonymous),
                cat,
                run,
                None
            ),
            Effect::Show
        );
        f
    }

    #[test]
    fn a_check_in_is_answered_step_by_step() {
        let (cat, run) = cleaning_run();
        let mut f = to_checkin_preview(&cat, &run);
        assert_eq!(f.screen(), &Screen::CheckinPreview);
        let c = f.draft.checkin.as_ref().unwrap();
        assert_eq!(c.answers[&0].value, Some(3));
        assert_eq!(
            c.answers[&1].choices,
            [run.questions[1].choices[1].version_id]
        );
        assert_eq!(c.answers[&2].text.as_deref(), Some("Fewer, clearer tasks."));
        assert_eq!(c.identity, Some(Identity::Anonymous));
        assert_eq!(
            step(&mut f, Action::Submit, &cat, &run, None),
            Effect::SubmitCheckin
        );
    }

    #[test]
    fn optional_questions_skip_and_required_ones_dont() {
        let (cat, run) = cleaning_run();
        let mut f = Flow::default();
        apply(&mut f, Action::Open(MenuItem::Checkins), &cat, None);
        step(&mut f, Action::OpenRun(run.id), &cat, &run, None);
        step(&mut f, Action::Begin, &cat, &run, None);
        // Q1 (scale) is required: no skipping, no out-of-range value.
        assert!(matches!(
            step(&mut f, Action::Skip, &cat, &run, None),
            Effect::Hint(_)
        ));
        assert!(matches!(
            step(&mut f, Action::Value(6), &cat, &run, None),
            Effect::Hint(_)
        ));
        step(&mut f, Action::Value(5), &cat, &run, None);
        // Q2 and Q3 are optional.
        assert_eq!(step(&mut f, Action::Skip, &cat, &run, None), Effect::Show);
        assert!(matches!(
            step(&mut f, Action::Value(1), &cat, &run, None),
            Effect::Hint(_)
        ));
        assert_eq!(step(&mut f, Action::Skip, &cat, &run, None), Effect::Show);
        let c = f.draft.checkin.as_ref().unwrap();
        assert!(c.answers[&1].skipped && c.answers[&2].skipped);
        // Floor needs the floor first.
        step(&mut f, Action::Identity(Identity::Floor), &cat, &run, None);
        assert_eq!(f.screen(), &Screen::Floor);
        let floor = cat.floors()[1].id;
        step(&mut f, Action::Floor(floor), &cat, &run, None);
        assert_eq!(f.screen(), &Screen::CheckinPreview);
    }

    #[test]
    fn check_in_back_keeps_answers_and_cancel_drops_them() {
        let (cat, run) = cleaning_run();
        let full = to_checkin_preview(&cat, &run);
        let mut f = full.clone();
        while f.screen() != &(Screen::CheckinQuestion { index: 0 }) {
            assert_eq!(step(&mut f, Action::Back, &cat, &run, None), Effect::Show);
        }
        assert_eq!(f.draft.checkin.as_ref().unwrap().answers[&0].value, Some(3));
        step(&mut f, Action::Value(4), &cat, &run, None);
        assert_eq!(f.draft.checkin.as_ref().unwrap().answers[&0].value, Some(4));
        for depth in 1..=full.stack.len() {
            let mut f = full.clone();
            f.stack.truncate(depth);
            assert_eq!(
                step(&mut f, Action::Cancel, &cat, &run, None),
                Effect::Cancel
            );
            assert_eq!(f, Flow::default());
        }
        // Without its run (expired, restarted on another run): no crash, a hint.
        let mut f = full.clone();
        assert!(matches!(
            apply(&mut f, Action::Submit, &cat, None),
            Effect::Hint(_)
        ));
    }

    #[test]
    fn the_code_screen_forgets_the_code_on_request() {
        let cat = cat();
        let mut f = Flow::default();
        f.push(Screen::CodeSaved);
        assert!(!f.in_progress(), "nothing to tidy away");
        assert_eq!(
            apply(&mut f, Action::CodeSaved, &cat, None),
            Effect::ForgetCode
        );
        assert_eq!(f, Flow::default());
        let mut f = Flow::default();
        f.push(Screen::CodeSaved);
        assert_eq!(apply(&mut f, Action::Home, &cat, None), Effect::Show);
        assert_eq!(f, Flow::default());
    }
}
