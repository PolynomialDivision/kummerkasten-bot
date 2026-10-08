//! The bot's own small markup: `**bold**`, `*italic*`, `~~struck~~`,
//! `` `code` ``, line breaks; a backslash makes the next character literal. Everything is
//! HTML-escaped, and messages never mention anyone — residents' text is
//! shown inside screens (the preview), and an `@user:server` in it must not
//! ping that person. (`mxbot_common::format::mentionify` would.)

use mxbot_common::matrix_sdk::ruma::events::{room::message::RoomMessageEventContent, Mentions};

/// Escape what this markup would interpret, for text from residents.
pub fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '`' | '\\' | '~') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `(plain body, HTML)`.
pub fn render(text: &str) -> (String, String) {
    let mut plain = String::with_capacity(text.len());
    let mut html = String::with_capacity(text.len() + 32);
    let mut bold = false;
    let mut italic = false;
    let mut struck = false;
    let mut code = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    plain.push(next);
                    escape(&mut html, next);
                }
            }
            '`' => {
                code = !code;
                html.push_str(if code { "<code>" } else { "</code>" });
            }
            '*' if !code && chars.peek() == Some(&'*') => {
                chars.next();
                bold = !bold;
                html.push_str(if bold { "<strong>" } else { "</strong>" });
            }
            '*' if !code => {
                italic = !italic;
                html.push_str(if italic { "<em>" } else { "</em>" });
            }
            '~' if !code && chars.peek() == Some(&'~') => {
                chars.next();
                struck = !struck;
                html.push_str(if struck { "<del>" } else { "</del>" });
            }
            '\n' => {
                plain.push('\n');
                html.push_str("<br>");
            }
            c => {
                plain.push(c);
                escape(&mut html, c);
            }
        }
    }
    // Close whatever was left open by unbalanced markers.
    for (open, tag) in [
        (code, "</code>"),
        (struck, "</del>"),
        (italic, "</em>"),
        (bold, "</strong>"),
    ] {
        if open {
            html.push_str(tag);
        }
    }
    (plain, html)
}

fn escape(html: &mut String, c: char) {
    match c {
        '&' => html.push_str("&amp;"),
        '<' => html.push_str("&lt;"),
        '>' => html.push_str("&gt;"),
        '"' => html.push_str("&quot;"),
        '\'' => html.push_str("&#39;"),
        c => html.push(c),
    }
}

/// A notice (no notification sound by default) that mentions nobody.
pub fn notice(text: &str) -> RoomMessageEventContent {
    let (plain, html) = render(text);
    let mut content = RoomMessageEventContent::notice_html(plain, html);
    content.mentions = Some(Mentions::new());
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markup_renders_and_user_text_stays_literal() {
        let (_, html) = render("~~Sunday~~ ➡️ **Monday**");
        assert_eq!(html, "<del>Sunday</del> ➡️ <strong>Monday</strong>");
        // Residents' tildes stay tildes.
        assert_eq!(render(&literal("~~x~~")).1, "~~x~~");
        let (plain, html) = render("📮 **Ready**\n*soon* `C7MJ-4P2K`");
        assert_eq!(plain, "📮 Ready\nsoon C7MJ-4P2K");
        assert_eq!(
            html,
            "📮 <strong>Ready</strong><br><em>soon</em> <code>C7MJ-4P2K</code>"
        );
        let user = "<b>hi</b> **not bold** @bob:example.org a*b";
        let (plain, html) = render(&format!("“{}”", literal(user)));
        assert_eq!(plain, format!("“{user}”"));
        assert!(html.contains("&lt;b&gt;hi&lt;/b&gt; **not bold**"));
        assert!(!html.contains("<strong>"));
        // Nobody is mentioned, whatever the text says.
        let content = notice(&literal(user));
        let json = serde_json::to_value(&content).unwrap();
        assert_eq!(json["m.mentions"], serde_json::json!({}));
        assert_eq!(json["msgtype"], "m.notice");
    }

    #[test]
    fn unbalanced_markers_are_closed() {
        let (_, html) = render("**open *also");
        assert_eq!(html, "<strong>open <em>also</em></strong>");
    }
}
