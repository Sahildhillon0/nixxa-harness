//! Per-OS locations for Nixxa's own files. Every OS has its case here, and each case is tested
//! on every OS.

use std::ffi::OsString;
use std::path::PathBuf;

/// The operating systems Nixxa places files for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    Linux,
    Mac,
    Windows,
}

impl Platform {
    /// The OS this binary was built for. Other Unixes follow the Linux (XDG) layout.
    pub(crate) const fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Mac
        } else {
            Self::Linux
        }
    }
}

/// Where Nixxa's logs go: `<data dir>/nixxa/logs`.
pub(crate) fn log_dir() -> Option<PathBuf> {
    data_dir(Platform::current(), &|name| std::env::var_os(name)).map(|dir| dir.join("nixxa").join("logs"))
}

/// The per-user data directory:
/// * Linux: `$XDG_DATA_HOME`, else `~/.local/share`;
/// * macOS: `~/Library/Application Support`;
/// * Windows: `%LOCALAPPDATA%`.
///
/// Relative or empty values are ignored, as the XDG spec requires.
pub(crate) fn data_dir(os: Platform, env: &dyn Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let var = |name: &str| env(name).map(PathBuf::from).filter(|path| path.is_absolute());
    match os {
        Platform::Linux => var("XDG_DATA_HOME").or_else(|| var("HOME").map(|home| home.join(".local").join("share"))),
        Platform::Mac => var("HOME").map(|home| home.join("Library").join("Application Support")),
        Platform::Windows => var("LOCALAPPDATA"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// An absolute path in this OS's syntax, so `is_absolute` holds on every OS.
    fn abs(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    fn env(vars: &[(&str, PathBuf)]) -> impl Fn(&str) -> Option<OsString> + use<> {
        let vars: Vec<(String, PathBuf)> = vars.iter().map(|(k, v)| ((*k).to_string(), v.clone())).collect();
        move |name| vars.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone().into_os_string())
    }

    #[test]
    fn linux_prefers_xdg_data_home() {
        let env = env(&[("XDG_DATA_HOME", abs("xdg")), ("HOME", abs("home"))]);
        assert_eq!(data_dir(Platform::Linux, &env), Some(abs("xdg")));
    }

    #[test]
    fn linux_falls_back_to_local_share() {
        let env = env(&[("XDG_DATA_HOME", PathBuf::from("relative")), ("HOME", abs("home"))]);
        assert_eq!(data_dir(Platform::Linux, &env), Some(abs("home").join(".local").join("share")));
    }

    #[test]
    fn macos_uses_application_support() {
        let env = env(&[("HOME", abs("home"))]);
        assert_eq!(data_dir(Platform::Mac, &env), Some(abs("home").join("Library").join("Application Support")));
    }

    #[test]
    fn windows_uses_local_app_data() {
        let env = env(&[("LOCALAPPDATA", abs("local")), ("HOME", abs("home"))]);
        assert_eq!(data_dir(Platform::Windows, &env), Some(abs("local")));
    }

    #[test]
    fn nothing_set_means_no_data_dir() {
        let env = env(&[]);
        for os in [Platform::Linux, Platform::Mac, Platform::Windows] {
            assert_eq!(data_dir(os, &env), None);
        }
    }

    #[test]
    fn logs_live_under_nixxa() {
        if let Some(dir) = log_dir() {
            assert!(dir.ends_with(Path::new("nixxa").join("logs")));
        }
    }
}
