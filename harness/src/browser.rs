//! Opening a sign-in page in the user's browser, through the OS: `xdg-open` (Linux and other
//! Unixes), `open` (macOS), `rundll32 url.dll,FileProtocolHandler` (Windows). The URL is always one
//! argument and no shell is involved, so nothing in it can be read as a command. `start` isn't
//! used on Windows: it is a `cmd` built-in, so it would need a shell.

use std::process::{Command, Stdio};

/// The command that opens `url`, or `None` for anything but an https URL.
pub(crate) fn command(url: &str) -> Option<Command> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let mut command = if cfg!(target_os = "windows") {
        let mut command = Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    command.arg(url);
    // The TUI owns the terminal: the opener's output must not land on it.
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    Some(command)
}

/// Opens `url` in the browser. `false` means it couldn't, and the user should copy the URL.
pub(crate) fn open(url: &str) -> bool {
    match command(url) {
        Some(mut command) => match command.spawn() {
            Ok(mut child) => {
                // Reap it in the background so it doesn't linger as a zombie.
                std::thread::spawn(move || child.wait());
                true
            }
            Err(_) => false,
        },
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_url_is_one_argument_and_no_shell_is_involved() {
        let url = "https://openrouter.ai/auth?callback_url=http%3A%2F%2Flocalhost%3A1234%2Fcallback&code_challenge=abc&code_challenge_method=S256";
        let command = command(url).expect("an https URL");
        let program = command.get_program().to_string_lossy().into_owned();
        assert!(["xdg-open", "open", "rundll32"].contains(&program.as_str()), "{program}");
        assert!(!["sh", "bash", "cmd", "cmd.exe", "powershell"].contains(&program.as_str()));
        let args: Vec<String> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.last().map(String::as_str), Some(url), "the URL is passed whole, as the last argument");
    }

    #[test]
    fn only_plain_https_urls_are_opened() {
        for url in
            ["http://example.com", "file:///etc/passwd", "https://a b", "https://x\n;rm -rf ~", "javascript:alert(1)"]
        {
            assert!(command(url).is_none(), "{url:?}");
        }
    }
}
