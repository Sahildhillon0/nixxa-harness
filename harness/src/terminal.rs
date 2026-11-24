//! The user's terminal pane (`ctrl+t`, D-066): state, keys and drawing. No I/O here; `tui`
//! connects it to the sandbox's terminal.
//!
//! What the shell prints goes through a VT100 emulator (`vt100`) into a grid of cells, and only
//! the cells are drawn. Escape sequences (clipboard writes, titles, hyperlinks, the bell, mode
//! switches) change the emulator's state or are dropped there; none reaches the real terminal.
//! A cell whose text `safe::line` would escape (a bidi override, a control character) is drawn
//! as `�`, and the frame is still scrubbed as a backstop.
//!
//! Keys: while the user types in it, every key goes to the shell (`ctrl+c` and `ctrl+t`
//! included) except `ctrl+]`, which leaves it for the pane's own keys: scrolling, sharing, and
//! `ctrl+t` to put the details pane back.

use std::borrow::Cow;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nixxa_contracts::TerminalSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

use crate::safe;

/// Lines of scrollback kept.
const SCROLLBACK: usize = 5_000;
/// The size used until the pane is first drawn.
const FIRST_SIZE: TerminalSize = TerminalSize { cols: 80, rows: 24 };
/// Lines shared when no earlier prompt is found above the cursor.
const SHARE_FALLBACK: usize = 10;

/// Where the terminal is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum State {
    /// None has been opened (or the last one ended and the pane was hidden).
    Closed,
    /// Asked for; the sandbox hasn't answered yet.
    Opening,
    Open,
    /// It ended, or couldn't open: why.
    Ended(String),
}

/// A range of screen rows chosen to share with the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Share {
    pub(crate) first: usize,
    pub(crate) last: usize,
}

/// What a key did in the pane's own (not typing) keys.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PaneKey {
    Nothing,
    /// Send these bytes to the shell (and go back to typing).
    Send(Vec<u8>),
    /// Put this text in the message input.
    Share(String),
    /// Not one of the pane's keys.
    NotMine,
}

/// The terminal pane.
pub(crate) struct TerminalPane {
    /// Shown in place of the details pane.
    pub(crate) shown: bool,
    /// Keys go to the shell (while the pane has focus).
    pub(crate) typing: bool,
    pub(crate) state: State,
    parser: vt100::Parser,
    size: TerminalSize,
    /// The size changed and the shell hasn't been told.
    resized: bool,
    /// Rows chosen to share, while choosing.
    pub(crate) share: Option<Share>,
}

impl Default for TerminalPane {
    fn default() -> Self {
        Self {
            shown: false,
            typing: false,
            state: State::Closed,
            parser: parser(FIRST_SIZE),
            size: FIRST_SIZE,
            resized: false,
            share: None,
        }
    }
}

fn parser(size: TerminalSize) -> vt100::Parser {
    vt100::Parser::new(size.rows.max(1), size.cols.max(1), SCROLLBACK)
}

impl TerminalPane {
    /// Whether a terminal is running or starting.
    pub(crate) fn is_live(&self) -> bool {
        matches!(self.state, State::Open | State::Opening)
    }

    /// Starts showing a new terminal: a blank screen at the pane's size. Returns the size to
    /// open it with.
    pub(crate) fn opening(&mut self) -> TerminalSize {
        self.state = State::Opening;
        self.parser = parser(self.size);
        self.resized = false;
        self.share = None;
        self.size
    }

    pub(crate) fn opened(&mut self) {
        if self.state == State::Opening {
            self.state = State::Open;
        }
    }

    pub(crate) fn ended(&mut self, reason: String) {
        self.state = State::Ended(reason);
        self.typing = false;
        self.share = None;
    }

    /// Output from the shell.
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// The pane's inside area for this frame: the emulator takes its size, and the shell is
    /// told on [`take_resize`](Self::take_resize).
    pub(crate) fn layout(&mut self, inner: Rect) {
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let size = TerminalSize { cols: inner.width, rows: inner.height };
        if size != self.size {
            self.size = size;
            self.resized = true;
            self.parser.set_size(size.rows, size.cols);
        }
    }

    /// The size to tell the shell, once per change, while a terminal is running.
    pub(crate) fn take_resize(&mut self) -> Option<TerminalSize> {
        if self.resized && self.state == State::Open {
            self.resized = false;
            return Some(self.size);
        }
        None
    }

    /// The bytes a key sends to the shell, as an xterm would; `None` for keys with no meaning
    /// there.
    pub(crate) fn key_bytes(&self, key: KeyEvent) -> Option<Vec<u8>> {
        let mods = key.modifiers;
        let (ctrl, alt, shift) = (
            mods.contains(KeyModifiers::CONTROL),
            mods.contains(KeyModifiers::ALT),
            mods.contains(KeyModifiers::SHIFT),
        );
        let app_cursor = self.parser.screen().application_cursor();
        // xterm's modifier parameter: 1 + shift + 2·alt + 4·ctrl.
        let param = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
        let csi_letter = |letter: char| -> Vec<u8> {
            if param > 1 {
                format!("\x1b[1;{param}{letter}").into_bytes()
            } else if app_cursor {
                format!("\x1bO{letter}").into_bytes()
            } else {
                format!("\x1b[{letter}").into_bytes()
            }
        };
        let tilde = |n: u8| -> Vec<u8> {
            if param > 1 { format!("\x1b[{n};{param}~").into_bytes() } else { format!("\x1b[{n}~").into_bytes() }
        };
        let with_alt = |bytes: Vec<u8>| if alt { [vec![0x1b], bytes].concat() } else { bytes };
        Some(match key.code {
            // ctrl+alt is AltGr on Windows: the character itself.
            KeyCode::Char(c) if ctrl && alt => c.to_string().into_bytes(),
            KeyCode::Char(c) if ctrl => with_alt(vec![control_byte(c)?]),
            KeyCode::Char(c) => with_alt(c.to_string().into_bytes()),
            KeyCode::Enter => with_alt(b"\r".to_vec()),
            KeyCode::Tab => b"\t".to_vec(),
            KeyCode::BackTab => b"\x1b[Z".to_vec(),
            KeyCode::Backspace if ctrl => vec![0x08],
            KeyCode::Backspace => with_alt(vec![0x7f]),
            KeyCode::Esc => vec![0x1b],
            KeyCode::Up => csi_letter('A'),
            KeyCode::Down => csi_letter('B'),
            KeyCode::Right => csi_letter('C'),
            KeyCode::Left => csi_letter('D'),
            KeyCode::Home => csi_letter('H'),
            KeyCode::End => csi_letter('F'),
            KeyCode::Insert => tilde(2),
            KeyCode::Delete => tilde(3),
            KeyCode::PageUp => tilde(5),
            KeyCode::PageDown => tilde(6),
            KeyCode::F(n @ 1..=4) => format!("\x1bO{}", char::from(b'P' + n - 1)).into_bytes(),
            KeyCode::F(n @ 5..=12) => tilde([15, 17, 18, 19, 20, 21, 23, 24][usize::from(n - 5)]),
            _ => return None,
        })
    }

    /// Pasted text as the shell should get it: newlines as Enter, every other control character
    /// (`ESC` above all, C1 controls too) dropped as xterm does, so no paste can end a bracket
    /// early or type a key sequence; bracketed if the program asked for it.
    pub(crate) fn paste_bytes(&self, text: &str) -> Vec<u8> {
        let text: String = text
            .replace("\r\n", "\r")
            .replace('\n', "\r")
            .chars()
            .filter(|c| !c.is_control() || matches!(c, '\r' | '\t'))
            .collect();
        if self.parser.screen().bracketed_paste() {
            format!("\x1b[200~{text}\x1b[201~").into_bytes()
        } else {
            text.into_bytes()
        }
    }

    /// Back to the live screen (typing, or output the user should see).
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.parser.set_scrollback(0);
    }

    /// Rows scrolled back from the live screen.
    pub(crate) fn scrolled(&self) -> usize {
        self.parser.screen().scrollback()
    }

    fn scroll_by(&mut self, rows: isize) {
        let now = self.parser.screen().scrollback();
        self.parser.set_scrollback(now.saturating_add_signed(rows));
    }

    /// The pane's own keys (focused, not typing): scrolling, sharing, and back to typing.
    pub(crate) fn pane_key(&mut self, key: KeyEvent) -> PaneKey {
        if self.share.is_some() {
            return self.share_key(key);
        }
        let page = isize::try_from(self.size.rows.max(2) - 1).unwrap_or(1);
        match key.code {
            // Twice sends it to the shell (vim's jump to tag).
            _ if is_leave(key) && self.state == State::Open => {
                self.typing = true;
                self.scroll_to_bottom();
                return PaneKey::Send(vec![0x1d]);
            }
            KeyCode::Char('i') | KeyCode::Enter if self.state == State::Open => {
                self.typing = true;
                self.scroll_to_bottom();
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(-1),
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Home | KeyCode::Char('g') => self.scroll_by(isize::MAX),
            KeyCode::End | KeyCode::Char('G') => self.scroll_to_bottom(),
            KeyCode::Char('s') => self.share = self.last_command(),
            _ => return PaneKey::NotMine,
        }
        PaneKey::Nothing
    }

    fn share_key(&mut self, key: KeyEvent) -> PaneKey {
        let Some(share) = self.share.as_mut() else { return PaneKey::Nothing };
        let bottom = usize::from(self.size.rows).saturating_sub(1);
        let end = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Up if end => share.last = share.last.saturating_sub(1).max(share.first),
            KeyCode::Down if end => share.last = (share.last + 1).min(bottom),
            KeyCode::Up => share.first = share.first.saturating_sub(1),
            KeyCode::Down => share.first = (share.first + 1).min(share.last),
            KeyCode::Char('a') => *share = Share { first: 0, last: bottom },
            KeyCode::Esc => self.share = None,
            KeyCode::Enter => {
                let text = self.shared_text();
                self.share = None;
                return text.map_or(PaneKey::Nothing, PaneKey::Share);
            }
            _ => {}
        }
        PaneKey::Nothing
    }

    /// The rows on screen, as text.
    pub(crate) fn rows(&self) -> Vec<String> {
        let screen = self.parser.screen();
        screen.rows(0, self.size.cols).collect()
    }

    /// The last command and its output on screen: from the previous line that starts with the
    /// prompt the cursor sits after, down to the line above the cursor. Without one, the last
    /// few lines above the cursor.
    fn last_command(&self) -> Option<Share> {
        let rows = self.rows();
        let (row, col) = self.parser.screen().cursor_position();
        let row = usize::from(row);
        let last = row.checked_sub(1)?;
        let prompt: String = rows.get(row)?.chars().take(usize::from(col)).collect();
        let prompt = prompt.trim_end();
        let first = (!prompt.is_empty())
            .then(|| (0..row).rev().find(|r| rows[*r].starts_with(prompt)))
            .flatten()
            .unwrap_or_else(|| row.saturating_sub(SHARE_FALLBACK));
        Some(Share { first, last })
    }

    /// The chosen rows as a quoted block for the message input, every line made safe.
    fn shared_text(&self) -> Option<String> {
        let share = self.share?;
        let rows = self.rows();
        let lines: Vec<String> = rows
            .get(share.first..=share.last.min(rows.len().saturating_sub(1)))?
            .iter()
            .map(|r| safe::line(r.trim_end()))
            .collect();
        let body = lines.join("\n");
        let fence = if body.contains("```") { "~~~~" } else { "```" };
        Some(format!("From my terminal:\n{fence}\n{body}\n{fence}\n"))
    }

    /// Draws the screen into `area` (the pane's inside). Only cell text is drawn, each cell's
    /// made safe; `color` false (`NO_COLOR`) keeps the attributes but drops the colours.
    pub(crate) fn render(&self, buf: &mut Buffer, area: Rect, color: bool) {
        let screen = self.parser.screen();
        for row in 0..area.height {
            let chosen = self.share.is_some_and(|s| (s.first..=s.last).contains(&usize::from(row)));
            for col in 0..area.width {
                let Some(cell) = screen.cell(row, col) else { continue };
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = cell_style(cell, color);
                if chosen {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                if let Some(target) = buf.cell_mut((area.x + col, area.y + row)) {
                    target.set_symbol(&cell_text(&cell.contents())).set_style(style);
                }
            }
        }
    }

    /// Where to put the real cursor, inside `area`, while typing.
    pub(crate) fn cursor(&self, area: Rect) -> Option<(u16, u16)> {
        let screen = self.parser.screen();
        if !self.typing || self.state != State::Open || screen.hide_cursor() || screen.scrollback() > 0 {
            return None;
        }
        let (row, col) = screen.cursor_position();
        (row < area.height && col < area.width).then(|| (area.x + col, area.y + row))
    }
}

/// `ctrl+]`, which leaves the terminal. Terminals without the newer keyboard protocols send it
/// as byte 0x1d, which arrives as `ctrl+5`.
pub(crate) fn is_leave(key: KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && !key.modifiers.contains(KeyModifiers::ALT)
        && matches!(key.code, KeyCode::Char(']' | '5'))
}

/// The byte `ctrl` plus a key sends.
fn control_byte(c: char) -> Option<u8> {
    Some(match c.to_ascii_lowercase() {
        c @ 'a'..='z' => c as u8 - b'a' + 1,
        ' ' | '@' | '2' => 0,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '-' | '/' | '7' => 0x1f,
        '?' | '8' => 0x7f,
        _ => return None,
    })
}

/// A cell's text as drawn: a space when empty, `�` when it holds anything `safe::line` would
/// escape (a bidi control joined to the cell, a control character).
fn cell_text(contents: &str) -> Cow<'_, str> {
    if contents.is_empty() {
        Cow::Borrowed(" ")
    } else if safe::line(contents) != contents {
        Cow::Borrowed("\u{FFFD}")
    } else {
        Cow::Borrowed(contents)
    }
}

fn cell_style(cell: &vt100::Cell, color: bool) -> Style {
    let mut style = Style::new();
    if color {
        if let Some(fg) = convert(cell.fgcolor()) {
            style = style.fg(fg);
        }
        if let Some(bg) = convert(cell.bgcolor()) {
            style = style.bg(bg);
        }
    }
    for (on, modifier) in [
        (cell.bold(), Modifier::BOLD),
        (cell.italic(), Modifier::ITALIC),
        (cell.underline(), Modifier::UNDERLINED),
        (cell.inverse(), Modifier::REVERSED),
    ] {
        if on {
            style = style.add_modifier(modifier);
        }
    }
    style
}

fn convert(color: vt100::Color) -> Option<Color> {
    match color {
        vt100::Color::Default => None,
        vt100::Color::Idx(index) => Some(Color::Indexed(index)),
        vt100::Color::Rgb(r, g, b) => Some(Color::Rgb(r, g, b)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(cols: u16, rows: u16) -> TerminalPane {
        let mut pane = TerminalPane::default();
        pane.layout(Rect::new(0, 0, cols, rows));
        pane.opening();
        pane.opened();
        pane
    }

    /// Every cell's text, row by row.
    fn drawn(pane: &TerminalPane) -> Vec<String> {
        let area = Rect::new(0, 0, pane.size.cols, pane.size.rows);
        let mut buf = Buffer::empty(area);
        pane.render(&mut buf, area, true);
        (0..area.height).map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect()).collect()
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn escape_sequences_change_the_emulator_never_the_screen() {
        let mut pane = pane(60, 6);
        // Clipboard write (OSC 52), title, hyperlink, bell, the alternate screen and back,
        // bracketed paste and mouse modes, a window resize request, a DCS string.
        pane.feed(b"\x1b]52;c;c2VjcmV0LWluLWNsaXA=\x07\x1b]0;evil-title\x07\x1b]2;evil-title-2\x1b\\");
        pane.feed(b"\x1b]8;;https://evil.example/\x1b\\link-text\x1b]8;;\x1b\\ \x07\x07");
        pane.feed(b"\x1b[?1049h\x1b[?1049l\x1b[?2004h\x1b[?1000h\x1b[8;100;300t\x1bPq#0;2;0;0;0\x1b\\ visible\r\n");
        let screen = drawn(&pane);
        let all = screen.concat();
        assert!(all.contains("link-text") && all.contains("visible"), "{screen:?}");
        for hidden in ["c2VjcmV0", "evil-title", "evil.example", "52;", "]8;", "#0;2"] {
            assert!(!all.contains(hidden), "{hidden} reached the screen: {screen:?}");
        }
        assert!(
            all.chars().all(|c| !c.is_control()),
            "a control character reached the screen: {:?}",
            all.chars().filter(|c| c.is_control()).collect::<Vec<_>>()
        );
        // The emulator took the modes; nothing was passed on.
        assert!(pane.parser.screen().bracketed_paste());
        assert_eq!(pane.size, TerminalSize { cols: 60, rows: 6 }, "a program can't resize the pane");
    }

    #[test]
    fn cells_with_bidi_or_control_characters_are_shown_escaped() {
        let mut pane = pane(40, 3);
        // U+202E (right-to-left override) and U+2066 join the cell before them; U+009B is a C1
        // control sent as UTF-8.
        pane.feed("ok a\u{202E}b c\u{2066}d e\u{9B}31mf".as_bytes());
        let row = &drawn(&pane)[0];
        assert!(row.starts_with("ok "), "{row:?}");
        for c in row.chars() {
            assert!(
                !(c.is_control()
                    || matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{80}'..='\u{9F}')),
                "{c:?} reached the screen: {row:?}"
            );
        }
        assert!(row.contains('\u{FFFD}'), "the cells holding them are marked: {row:?}");
    }

    #[test]
    fn keys_are_sent_as_an_xterm_sends_them() {
        let pane = pane(80, 24);
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;
        let cases: [(KeyEvent, &[u8]); 14] = [
            (key(KeyCode::Char('a'), none), b"a"),
            (key(KeyCode::Char('c'), ctrl), b"\x03"),
            (key(KeyCode::Char('t'), ctrl), b"\x14"),
            (key(KeyCode::Char('4'), ctrl), b"\x1c"),
            (key(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb"),
            (key(KeyCode::Char('@'), ctrl | KeyModifiers::ALT), b"@"),
            (key(KeyCode::Enter, none), b"\r"),
            (key(KeyCode::Backspace, none), b"\x7f"),
            (key(KeyCode::Up, none), b"\x1b[A"),
            (key(KeyCode::Left, ctrl), b"\x1b[1;5D"),
            (key(KeyCode::Delete, none), b"\x1b[3~"),
            (key(KeyCode::PageUp, KeyModifiers::SHIFT), b"\x1b[5;2~"),
            (key(KeyCode::F(1), none), b"\x1bOP"),
            (key(KeyCode::F(12), none), b"\x1b[24~"),
        ];
        for (key, want) in cases {
            assert_eq!(pane.key_bytes(key).as_deref(), Some(want), "{key:?}");
        }
        let mut app_mode = self::pane(80, 24);
        app_mode.feed(b"\x1b[?1h");
        assert_eq!(app_mode.key_bytes(key(KeyCode::Up, none)).as_deref(), Some(&b"\x1bOA"[..]), "application cursor");
        assert!(is_leave(key(KeyCode::Char(']'), ctrl)) && is_leave(key(KeyCode::Char('5'), ctrl)));
        assert!(!is_leave(key(KeyCode::Char(']'), none)));
    }

    #[test]
    fn a_paste_is_bracketed_when_asked_and_cannot_close_the_bracket() {
        let mut pane = pane(80, 24);
        assert_eq!(pane.paste_bytes("a\nb"), b"a\rb");
        pane.feed(b"\x1b[?2004h");
        assert_eq!(pane.paste_bytes("x\x1b[201~rm -rf ~\n"), b"\x1b[200~x[201~rm -rf ~\r\x1b[201~".to_vec());
        // A marker nested in a marker doesn't reassemble once the first is gone.
        let nested = pane.paste_bytes("x\x1b[20\x1b[201~1~touch /tmp/pwned\n");
        assert_eq!(nested, b"\x1b[200~x[20[201~1~touch /tmp/pwned\r\x1b[201~".to_vec());
        let ends = nested.windows(6).filter(|w| *w == b"\x1b[201~").count();
        assert_eq!(ends, 1, "exactly one end marker, the real one");
        // Other controls go too; tabs stay.
        assert_eq!(pane.paste_bytes("a\tb\x03\x04\u{9b}c"), b"\x1b[200~a\tbc\x1b[201~".to_vec());
    }

    #[test]
    fn the_shell_is_told_each_new_size_once() {
        let mut pane = TerminalPane::default();
        pane.layout(Rect::new(3, 4, 70, 20));
        assert_eq!(pane.opening(), TerminalSize { cols: 70, rows: 20 }, "it opens at the pane's size");
        assert_eq!(pane.take_resize(), None, "nothing to tell before it is open");
        pane.opened();
        pane.layout(Rect::new(0, 0, 70, 20));
        assert_eq!(pane.take_resize(), None, "same size");
        pane.layout(Rect::new(0, 0, 98, 30));
        assert_eq!(pane.take_resize(), Some(TerminalSize { cols: 98, rows: 30 }));
        assert_eq!(pane.take_resize(), None, "once");
        assert_eq!(pane.parser.screen().size(), (30, 98));
        pane.layout(Rect::new(0, 0, 0, 0));
        assert_eq!(pane.take_resize(), None, "a hidden pane keeps its size");
    }

    #[test]
    fn scrollback_scrolls_and_typing_returns_to_the_bottom() {
        let mut pane = pane(20, 4);
        for n in 0..30 {
            pane.feed(format!("line {n}\r\n").as_bytes());
        }
        assert_eq!(pane.pane_key(key(KeyCode::PageUp, KeyModifiers::NONE)), PaneKey::Nothing);
        assert_eq!(pane.scrolled(), 3);
        assert!(drawn(&pane)[0].starts_with("line 24"), "{:?}", drawn(&pane));
        pane.pane_key(key(KeyCode::Char('g'), KeyModifiers::NONE));
        assert!(pane.scrolled() > 20);
        pane.pane_key(key(KeyCode::Char('i'), KeyModifiers::NONE));
        assert!(pane.typing);
        assert_eq!(pane.scrolled(), 0);
    }

    #[test]
    fn sharing_picks_the_last_command_and_its_output() {
        let mut pane = pane(40, 8);
        pane.feed(b"sandbox:/workspace$ ls\r\nCargo.toml\r\nsrc\r\nsandbox:/workspace$ cargo build\r\n");
        pane.feed("error: \u{202E}evil\r\nsandbox:/workspace$ ".as_bytes());
        assert_eq!(pane.pane_key(key(KeyCode::Char('s'), KeyModifiers::NONE)), PaneKey::Nothing);
        assert_eq!(pane.share, Some(Share { first: 3, last: 4 }));
        // One more line up, then share.
        pane.pane_key(key(KeyCode::Up, KeyModifiers::NONE));
        let PaneKey::Share(text) = pane.pane_key(key(KeyCode::Enter, KeyModifiers::NONE)) else { panic!() };
        assert_eq!(
            text, "From my terminal:\n```\nsrc\nsandbox:/workspace$ cargo build\nerror: <U+202E>evil\n```\n",
            "every line made safe: the bidi override is escaped"
        );
        assert!(pane.share.is_none());
    }

    #[test]
    fn sharing_without_a_prompt_takes_the_last_lines_and_esc_cancels() {
        let mut pane = pane(20, 20);
        for n in 0..15 {
            pane.feed(format!("{n}\r\n").as_bytes());
        }
        pane.pane_key(key(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(pane.share, Some(Share { first: 5, last: 14 }));
        pane.pane_key(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(pane.share.is_none());
    }
}
