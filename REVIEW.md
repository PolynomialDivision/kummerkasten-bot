# Kummerkasten-Bot – Code- und UX-Review

Stand: 2026-10-17  
Umfang: statische Prüfung der Rust-Implementierung, Matrix-Interaktionen, Datenmodell und Nutzerführung. Keine Änderungen am Bot oder an Live-Daten.

## Kurzfazit

Die Produktidee und der Datenschutz-Anspruch sind ungewöhnlich durchdacht: getrennte private Chats, Vorschau vor Speicherung, explizite Löschfristen, Ideen getrennt von Feedback und aggregierte Ergebnisse. Das README beschreibt eine klare, überwiegend tappbare Oberfläche. Die Komplexität ist inzwischen allerdings hoch (rund 17.000 Zeilen Rust in 29 Modulen); besonders die Verarbeitung in `dm.rs`, `engine.rs` und `flow.rs` verdient sorgfältige Prüfung auf Zustands- und Berechtigungsfehler.

„Kritisch“ bedeutet hier, dass der Punkt potenziell Datenschutz oder korrekte Zuordnung/Verarbeitung gefährdet; „Hoch“ steht für einen wesentlichen Bedienungs- oder Betriebsfehler; „Mittel“ für eine relevante UX-/Wartbarkeitsverbesserung. Ich habe kein Finding als kritisch eingestuft; ein hoher Datenschutzpunkt braucht aber zeitnahe Aufmerksamkeit.

## Findings

### 1. Hoch – Chat-Löschung kann als erledigt gelten, obwohl Inhalte zurückbleiben

`src/scheduler.rs:276-322` entfernt einen Eintrag aus `dm_events`, sobald das Redact fünfmal fehlgeschlagen ist. Damit kann ein späterer Retry die Nachricht nicht mehr erreichen. Noch gravierender: `src/dm.rs:420-429` löscht beim Verlassen des privaten Chats den kompletten Event-Index und lässt den Bot unmittelbar danach den Raum verlassen. Die ursprünglichen Matrix-Events bleiben auf dem Homeserver bestehen. `src/lifecycle.rs:78-85` markiert bei Plenumabschluss zwar alle Chat-Ereignisse zur Löschung, aber der Worker kann die Zusage nicht erfüllen, wenn er sie nicht redigieren kann.

Das widerspricht den Aussagen, dass Nachrichten beim Aufräumen entfernt werden und nach fünf Fehlversuchen kein Inhalt verbleibt. Besonders bei privaten Schilderungen ist das ein Datenschutz- und Vertrauensproblem. **Änderungsvorschlag:** Event-IDs erst nach bestätigter Redaction aus der Warteschlange entfernen; Fehler mit wachsendem Abstand dauerhaft erneut versuchen und für Admins sichtbar machen. Beim Verlassen den Bot erst dann aus dem DM entfernen, wenn die Löschwarteschlange abgearbeitet ist; falls Matrix das nicht zulässt, die Einschränkung klar benennen und den Löschstatus nicht als erfolgreich darstellen. DB-Löschung schützt außerdem nicht vor WAL/Backups; dieser Unterschied wird in `src/db.rs:72-76` bereits korrekt erklärt.

### 2. Hoch – Bearbeitete Menüs zeigen Knöpfe, die nicht als Reaktionen verfügbar sind

`src/engine.rs:672-675` bearbeitet den bestehenden Screen bei `Effect::Refresh`; auch eine geänderte Ideen-Unterstützung wird als Edit ausgegeben (`src/engine.rs:695-698`). In `src/dm.rs:179-184` werden Reaktionen aber nur für neu gesendete Screens gesetzt; bei `Kind::Edit` wird `seed` übersprungen. Reaktionen auf die vorherige Ansicht bleiben zwar am ursprünglichen Matrix-Event, aber neue Optionen aus dem Edit werden nicht angelegt. Zum Beispiel kann die Ideenansicht nach 👍 nun ↩️ „Take my 👍 back“ zeigen, ohne eine ↩️-Reaktion darunter zu haben. Auch neu eingeblendete Vorwärts-/Rückwärtsnavigation kann fehlen.

**Änderungsvorschlag:** Nach einem Edit die jetzt benötigten Reaktions-Keys am selben Original-Event ergänzen. Vorher prüfen, welche Keys bereits vorhanden sind, oder das Setzen idempotent behandeln. Reaktionslöschung für konsumierte Taps weiter nutzen, da die Reaktion am Original-Event bleibt und sonst dieselbe Taste nicht erneut gedrückt werden kann. Tests sollten eine Ansicht mit verändertem Button-Set durchspielen und prüfen, dass jeder sichtbare Button benutzbar ist.

### 3. Mittel bis hoch – Fast jeder Schritt sendet eine neue Bot-Nachricht statt den Screen zu bearbeiten

`src/engine.rs:670-671` lässt normale Flow-Schritte auf `Effect::Show` fallen; am Ende wird immer `Msg::screen` erzeugt (`src/engine.rs:801-804`). Damit entstehen beim Teilen oder Beantworten eines Check-ins für jede Auswahl neue Nachrichten. Alte Screens bleiben bis zum Ende des Flows oder bis zum Plenum in der Chat-Historie und werden erst dann redigiert. `Msg::Edit` wird im normalen Ablauf nur bei `Effect::Refresh` und ausgewählten Ideen-Aktionen verwendet. Das ist verständlich als Verlauf, aber für einen täglich genutzten privaten Chat unnötig laut und macht es schwer, den aktuellen Schritt auf dem Handy zu finden.

**Änderungsvorschlag:** Den aktuellen interaktiven Screen als eine Nachricht führen und für Auswahl-/Textschritte editieren. Nur Bestätigungen, Löschcodes und echte Hinweise als zusätzliche Nachrichten senden. Für jeden Edit neue Reaktionen ergänzen (siehe Finding 2); konsumierte Reaktionen am Original redigieren. Die Texteingaben der Nutzer weiterhin nie bearbeiten. Intro und Startmenü dürfen als einmalige Einführung getrennt bleiben, könnten aber auch in ein kurzes Onboarding mit einem klaren aktuellen Menü übergehen.

### 4. Mittel – Check-in- und Plenum-Benachrichtigungen sind at-most-once und können verloren gehen

`src/scheduler.rs:190-221` nimmt fällige Check-in-Batches aus der DB, bevor es Nachrichten versendet; `src/checkins.rs:790-806` markiert diese als bereits gesendet. Der Summary-Pfad setzt `announced_at` vor dem Matrix-Post (`src/summary.rs:25-40`), und Update-Einträge werden ebenfalls vor dem Senden gelöscht (`src/summary.rs:53-72`). Ein Prozessabsturz oder Matrix-Fehler nach dieser Markierung verliert die Benachrichtigung dauerhaft. Die Module dokumentieren das ausdrücklich als „missed message, never two“, also ist es eine bewusste Zuverlässigkeitsentscheidung.

**Änderungsvorschlag:** Für wichtige Check-in- und Summary-Nachrichten eine kleine persistente Outbox mit `pending/sending/sent`, begrenzten Retries und Matrix-Transaction-IDs verwenden. Ein Neustart sollte ausstehende Posts erneut versuchen. Falls genau-einmal-Versand technisch nicht möglich ist, idempotente Wiederholung mit Duplikat-Erkennung bevorzugen, statt still Nachrichten zu verlieren.

### 5. Mittel – Es fehlt eine klare Bewohner-Hilfe bzw. ein `!help`-Fallback

Das Hauptmenü ist kurz und auf dem Handy gut scannbar (`src/screens.rs:943-965`). Die Navigation ist aber ausschließlich über Reaktionen und eingegebene Auswahlzahlen erklärt. `src/dm.rs:280-284` erkennt `!menu`/`!start`/`menu` global, aber kein `!help`. Auf einem Textschritt wird `!help` als Inhalt interpretiert (`src/flow.rs:404-416`), also potenziell in ein Feedback übernommen. Neue Bewohner sehen einmalig eine längere Datenschutz-Einführung und anschließend eine eigene Menü-Nachricht (`src/dm.rs:393-400`), was im Vergleich zur klaren tappbaren UI etwas textlastig wirkt.

**Änderungsvorschlag:** `!help` global als kurze Hilfe oder Rückkehr zum Menü behandeln, bevor Freitext an den Flow geht. Zusätzlich im Menü einen kleinen Hinweis wie „Tap a button · `!menu` anytime“ oder ein ❓ Help-Menü anbieten. Hilfetext auf 1–2 Bildschirmhöhen begrenzen; Datenschutzdetails können hinter Privacy bleiben.

## Positive Befunde

- Die Residenten-Berechtigung basiert auf aktueller Server-Mitgliedschaft in aktiv autorisierten Räumen und schlägt bei Matrix-Abfragefehlern fehlgeschlossen (`src/access.rs`, `src/dm.rs:109-121`).
- Admin-`!kk` wird im Einstieg nur im als Admin-DM erkannten Pfad ausgeführt; normale private Chats geben für Admin-Befehle eine Sperrmeldung aus (`src/main.rs:40-55`, `src/main.rs:165-174`).
- Freitext wird als Literal gerendert und `m.mentions` ist leer; normale Ansichten pingen niemanden (`src/markup.rs:3-5`, `src/markup.rs:90-112`).
- Anonyme Submission-Datensätze speichern laut Schema weder Matrix-ID noch Etage; Löschcodes liegen als HMAC vor. Check-in-Teilnahmen werden ebenfalls über HMAC getrennt von Antwortdatensätzen referenziert (`migrations/001_initial.sql`, `src/secrets.rs`, `src/submissions.rs`, `src/checkins.rs`).
- Ergebnisse unterdrücken kleine und ableitbare Etagen-Gruppen; es gibt dafür gezielte Tests in `src/stats.rs`.
- Terminregeln und Plena liegen als Datenbankzustand vor und werden beim Start materialisiert; der Scheduler ist restart-orientiert und Datum allein schließt ein Plenum standardmäßig nicht.

## Was bereits geprüft wurde

- Projektstruktur, Einstieg und Dispatcher in `src/main.rs`.
- Dokumentierte Residenten- und Admin-Flows in `README.md`.
- Der Bot antwortet auf Events nur in beigetretenen Räumen und behandelt `!kk` außerhalb des Admin-DM-Pfads gesondert.

## Umfang und Grenzen

Die Prüfung war statisch; ich habe weder Tests noch Build ausgeführt. Die aufgelisteten Fehler ergeben sich direkt aus den betreffenden Kontrollpfaden. Eine Laufzeitprüfung sollte vor einer Änderung insbesondere Matrix-Room-Power-Levels, Redaction-Antworten, Homeserver-Historie nach Verlassen eines DM und die Reaktionsdarstellung in Element/FluffyChat verifizieren. Es wurden keine Quellcodedateien geändert und keine Live-Daten berührt.

## Umsetzungsstand (2026-10-18)

| Punkt | Stand |
|---|---|
| 1. Löschung | umgesetzt: fehlgeschlagene Redactions mit wachsendem Abstand (1 min … 6 h) erneut, nie aufgegeben solange das Event existiert (404 = erledigt); verlassene Chats erst redigiert, dann verlassen (`leaving_rooms`); `!kk round` zeigt hängende Löschungen |
| 2. Knöpfe nach Edit | umgesetzt: Reaktionen werden nach jedem Edit abgeglichen (gemeinsamer Anfang bleibt, Rest neu in Textreihenfolge), Ledger in `screens` |
| 3. Ein Screen | umgesetzt mit Mischregel: bearbeiten, solange der Screen die letzte Nachricht und Teil des Flows ist, sonst frischer Screen unten und der alte wird sofort redigiert; Quittungen werden nie bearbeitet; Edit-Fehler → frischer Screen |
| 4. Benachrichtigungen | umgesetzt: Outbox (`outbox.rs`) für Notices, Reminder, Ankündigungen, Zusammenfassung und Updates; im selben Transaktionsschritt wie die Markierung eingereiht, Retries mit fester Matrix-Transaction-ID; pro-Person-Zeilen nur während des Versands |
| 5. `!help` | umgesetzt: `!help`/`!hilfe`/`help`/`hilfe` vor dem Flow abgefangen, zeigt Kurzhilfe und den aktuellen Schritt erneut; Hinweis im Menü |
| zusätzlich | `!kk survey <key> choice <n> emoji <c> <emoji|none>`; Warnung bei doppelten, fehlenden, reservierten oder verwechselbaren Emojis |
