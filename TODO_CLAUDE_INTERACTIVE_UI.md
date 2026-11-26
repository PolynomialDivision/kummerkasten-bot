# TODO for Claude: reduce Kummerkasten DM message clutter with one editable UI screen

## Objective

Refactor resident conversations so the bot keeps one current interactive screen in the private Matrix chat and edits that bot-owned screen as the resident moves through menus, forms, check-ins, results and ideas. This should make the current step easy to find on a phone and prevent a staircase of obsolete bot menus from filling the chat.

Do not edit or redact user messages as part of this UI change. Do not deploy the bot or alter live Unraid state. Work in code, documentation and tests only.

## Current behavior and concrete problem

- In `src/engine.rs`, ordinary successful transitions return `Effect::Show`; the end of `handle_sync` then appends `Msg::screen(...)` (currently around lines 670 and 801–804). `deliver` sends that as a new Matrix event.
- A resident selecting a topic, location, identity, check-in answer or similar option therefore gets another bot message for each step. A multi-step submission or check-in can leave many old bot screens in the visible history. They are inert because only the event saved as `screen_event_id` accepts taps, but they remain visible until the flow is tidied/redacted.
- `Msg::Edit` already exists and `deliver` already sends an `m.replace` edit referencing the saved bot screen. It is currently used only in a few paths such as `Effect::Refresh` and idea support changes.
- `deliver` only seeds reactions when it sends a new non-edit screen (`src/dm.rs`, around lines 179–184). It does not add the reaction keys for an edited screen.
- A Matrix reaction remains related to the original event after the event is edited. The screen's reaction target must continue to be the original bot event ID; an edit event ID is not a new reaction target.
- The reaction handler consumes a tapped reaction by redacting it if the bot's power level permits (`consume` in `src/dm.rs`). This matters because the same user must be able to tap the same emoji again on a later state of the same original event.

## Desired interaction model

1. A new conversation or explicit return to the menu creates one bot-owned interactive screen.
2. A valid tap or user text message advances the flow, then edits that existing bot screen in place.
3. The UI shows the current question/step only. Old menu text should not accumulate in the visible timeline.
4. The user's typed text remains a user event and is never edited by the bot. It may continue to be redacted according to the existing retention behavior; do not weaken that policy.
5. Keep separate bot events for content that should remain a receipt or independent notice: e.g. the deletion-code receipt, successful submission/check-in confirmation, and a short error that should not overwrite the active question. Keep the current screen available after such a receipt only when that is the existing intended flow.
6. A stale reaction on an old screen must not perform an action. Preserve the existing check that taps count only when they target the current saved screen (or a deliberately supported check-in notice).
7. The current screen should remain recoverable after restart. If its saved event cannot be edited (missing, redacted, wrong room, not found, server error), safely create a fresh bot screen, store its event ID, and seed its controls. Do not silently leave the person with a dead UI.

## Matrix reaction behavior: investigate and implement deliberately

Do not assume that editing a message removes or resets reactions. It does not: reactions remain attached to the original event.

For every edit:

- Keep using the original screen event ID as the relation target.
- Ensure every newly displayed reaction key has a usable reaction under that original event.
- Do not create duplicate bot reactions when retrying after a timeout or restarting. Inspect the pinned Matrix SDK and homeserver APIs for a reliable way to list current relations/reactions. If relation lookup is awkward or incomplete, keep a small persistent ledger of bot-seeded keys per screen event and reconcile it with successful sends/redactions where possible.
- When a key disappears from the new view, consider whether its bot-seeded reaction should be redacted to keep the UI tidy. Do this only when supported and safe; do not redact a user's reaction as though it were bot-owned.
- When a resident taps a key, keep consuming/redacting that resident's reaction so the same button can be reused on this same screen after it is edited. Check actual power levels before relying on this. The bot may not be allowed to redact other users' reactions in every room/version/configuration.
- Treat Matrix errors as real outcomes. A failed reaction redaction must not be reported internally as success, and repeated click delivery must remain idempotent through the existing processed-event handling.

If the bot cannot redact a user's consumed reaction, do not leave the UI stuck while showing a reusable-looking button. Choose and document a safe fallback. Preferred fallback order:

1. If possible, provide a supported alternative input (e.g. typed choice number) for every action and make it obvious in the screen.
2. If needed, rotate to a fresh bot-owned screen event so the user can react to the same emoji again, while making all old screens inert and ensuring they remain subject to the normal cleanup policy.
3. If neither works reliably, disable reaction-only controls in that environment and give a clear typed interaction path.

Avoid implementing fallback based only on an assumption about Element or FluffyChat. Verify the Matrix power-level check and the SDK behavior.

## Suggested implementation shape

Keep the flow/domain code independent of Matrix delivery where practical. The engine should express intent clearly, for example:

- `CreateScreen(view)` when there is no valid current screen.
- `EditScreen(view)` for a normal state transition on the current screen.
- `Receipt(text)` / `Notice(text)` for content that deserves a separate event.

Names can differ; preserve readable semantics and avoid branching on incidental message ordering. `dm::deliver` should be the only layer that performs Matrix sends/edits/reactions and updates screen-event bookkeeping.

Implementation should:

- Change ordinary `Effect::Show` transitions to edit the active screen when one exists, including text-entry progression and check-in question changes.
- Preserve separate receipts/notes intentionally; review every existing `Msg::note`, `Msg::receipt`, `Msg::screen` and `Msg::edit` path so no confirmation is lost or duplicated.
- Ensure every successful edit is tracked for eventual redaction under existing retention semantics. Matrix replacement events contain the updated private content too, so track edits as well as originals.
- Keep only the original screen event ID as the reaction target and the authoritative `screen_event_id` in session state.
- After the edit, reconcile the visible action set with the bot's seeded reactions. A changed set must work in both directions; e.g. 👍 support can turn into ↩️ withdraw support and back again.
- Preserve any intentionally separate check-in notice/reminder that can be opened with 📊, even when it is not the current screen. Verify that its reaction path still works.
- Review fresh-menu paths, onboarding, `open_at`, session expiry, cancel, submit, code-saving, check-in completion, results paging and ideas interactions. Do not leave a path that sends a new screen for every step by accident.
- Handle API timeouts safely. If a send/edit may have succeeded although the response was lost, use transaction/idempotency support from the Matrix SDK where available or reconcile from room state before creating another screen.
- Keep `m.mentions` empty for these read-only/editable screens; editing must not re-notify residents.
- Preserve authorization checks and `Kk.ops` serialization. Do not broaden who can use a DM or change admin routing as part of this UI work.

## UX content guidance

- Keep messages compact on narrow screens. Show the current prompt first, then only the choices and navigation that matter now.
- Make the fallback clear: for example, `Tap a button or reply with its number · !menu anytime` if typed-number input is supported for that screen.
- `!menu`/`!start` should still discard the current draft according to current semantics and return to a fresh menu. Decide whether `!help` should be included as a short orientation or be routed to the menu; do not let `!help` accidentally become feedback text on a free-text step. If you implement this while touching input routing, document the behavior.
- Keep the one-time privacy introduction and success/deletion receipts understandable. Do not turn every small tap into a standalone “Thanks” message; edit the current view when the state itself conveys the result.

## Tests and validation

Add meaningful regression tests that exercise behavior, not only helper implementation details:

1. A representative multi-step submission progresses through several button selections while maintaining one logical bot screen; each transition is an edit of that original event, not another screen event.
2. Check-in answers and navigation also edit the current screen. Completion still emits its intended receipt.
3. Free-text input remains a user event; bot delivery only edits bot-owned event IDs.
4. A reaction targeting an older screen is ignored; a reaction on the current screen is handled once.
5. The same reaction key can be used again after consumption and edit, when redaction is permitted.
6. Changing the button set adds new controls and does not claim unavailable controls. Cover a concrete support/withdraw toggle and changing result-page navigation.
7. Repeated delivery/restart does not seed duplicate bot reactions or lose the screen pointer.
8. If reaction redaction is unavailable/fails, the chosen fallback remains usable and does not perform duplicate state changes.
9. If editing fails because the original event is unavailable, the bot creates a usable fresh screen and old screen reactions remain inert.
10. Receipts and independent notices remain readable and are not overwritten; all edit/replacement events are tracked for cleanup.
11. Existing tests around cancellation, expiry, private chat authorization, check-in notice reactions, and screen ownership continue to hold.

Run the project's full Rust tests and clippy after implementation, plus formatting. If Matrix integration tests or a test homeserver are available, verify the same tap/edit/tap-again sequence in Element and FluffyChat. Do not deploy.

## Documentation and review

- Update README resident guidance so it describes the actual one-current-screen behavior and the typed fallback, if present.
- Document limitations if the homeserver does not let the bot redact a user's reaction.
- In the final report, show one before/after flow (e.g. menu → topic → location → text → preview), explain the reaction reset behavior and fallback, identify the files changed, report tests/clippy, and list any remaining client/server limitations.
- Do a skeptical pass: search for every `Msg::screen` and `Msg::edit` call site and justify whether it creates or edits a screen; inspect whether a redacted original event or stale session can strand the user; ensure no ordinary menu interaction still creates a new bot screen unnecessarily.

## Scope boundaries

- Code, tests and documentation only.
- Do not change live Unraid state, deploy, commit, or alter credentials.
- Do not edit user messages.
- Do not remove privacy/access checks or weaken event cleanup to make the UI appear cleaner.
