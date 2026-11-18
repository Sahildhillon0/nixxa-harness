//! Makes untrusted text safe to put on the terminal.
//!
//! Everything the TUI shows that it did not write itself (agent text, command output,
//! knowledge results, proposed commands, reasons, ids) can contain control characters. Written
//! raw, they reach the host terminal: escape sequences can retitle the window, write the
//! clipboard (OSC 52) or clear the screen, and a `\r` or bidi override can make an approval
//! prompt show something other than what will run. So every such string goes through
//! [`line`] or [`lines`], which turn those characters into visible stand-ins, and
//! [`scrub`] checks the whole frame as a backstop.

use ratatui::buffer::Buffer;

/// Spaces a tab expands to.
const TAB: &str = "    ";
/// What a C1 control or anything [`scrub`] catches is shown as.
const REPLACEMENT: char = '\u{FFFD}';

/// Bidirectional formatting characters that can visually reorder text.
fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// One line of untrusted text, safe to draw: tabs become spaces, C0 controls and DEL become caret
/// notation (`^[`, `^M`, `^J`, `^?`), C1 controls become `�`, and bidi controls become
/// `<U+XXXX>`. Newlines are controls here too; use [`lines`] for multi-line text.
pub(crate) fn line(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\t' => out.push_str(TAB),
            '\u{7F}' => out.push_str("^?"),
            c if (c as u32) < 0x20 => {
                out.push('^');
                out.push(char::from(u8::try_from(c as u32 + 0x40).unwrap_or(b'?')));
            }
            c if c.is_control() => out.push(REPLACEMENT),
            c if is_bidi_control(c) => out.push_str(&format!("<U+{:04X}>", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Multi-line untrusted text, safe to draw, one entry per line. `\n` and `\r\n` end lines; a
/// lone `\r` stays visible as `^M`.
pub(crate) fn lines(text: &str) -> Vec<String> {
    text.split('\n').map(|l| line(l.strip_suffix('\r').unwrap_or(l))).collect()
}

/// Backstop: replaces any cell that still holds a control or bidi character.
pub(crate) fn scrub(buffer: &mut Buffer) {
    for cell in &mut buffer.content {
        if cell.symbol().chars().any(|c| c.is_control() || is_bidi_control(c)) {
            cell.set_char(REPLACEMENT);
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Rect;

    use super::*;

    #[test]
    fn controls_become_visible() {
        assert_eq!(line("a\x1b]0;x\x07b"), "a^[]0;x^Gb");
        assert_eq!(line("run\rrm -rf /"), "run^Mrm -rf /");
        assert_eq!(line("a\nb"), "a^Jb");
        assert_eq!(line("del\x7f"), "del^?");
        assert_eq!(line("c1\u{9b}2J"), "c1\u{FFFD}2J");
        assert_eq!(line("a\tb"), "a    b");
    }

    #[test]
    fn bidi_controls_are_named() {
        assert_eq!(line("ls \u{202E}fdp.exe"), "ls <U+202E>fdp.exe");
        assert_eq!(line("\u{2066}x\u{2069}"), "<U+2066>x<U+2069>");
    }

    #[test]
    fn plain_text_is_untouched() {
        assert_eq!(line("cargo test — ✓ ünïcode"), "cargo test — ✓ ünïcode");
    }

    #[test]
    fn lines_split_on_newlines_and_keep_lone_carriage_returns() {
        assert_eq!(lines("a\r\nb\nc\rd"), vec!["a", "b", "c^Md"]);
    }

    #[test]
    fn scrub_replaces_leftover_controls() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 3, 1));
        buffer[(0, 0)].set_symbol("\x1b");
        buffer[(1, 0)].set_symbol("\u{202E}");
        buffer[(2, 0)].set_symbol("x");
        scrub(&mut buffer);
        let symbols: Vec<_> = buffer.content.iter().map(|c| c.symbol().to_string()).collect();
        assert_eq!(symbols, vec!["\u{FFFD}", "\u{FFFD}", "x"]);
    }
}
