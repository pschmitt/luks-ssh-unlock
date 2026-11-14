//! Which terminal shows the picture, and may tmux carry the image to it?
//!
//! Nothing here runs a program or trusts a path: it only classifies strings
//! (terminal names, the `XTVERSION` answer tmux keeps for each client, the
//! environment of a client process) and phrases the reasons a picture cannot
//! be shown. The tmux queries themselves are in [`super::transport`].
//!
//! Inside tmux the terminal that matters is the one of the *attached
//! client*. The pane's own environment says nothing about it (the tmux server
//! may have been started in another terminal, and `TERM` there is `screen` or
//! `tmux` whatever the client runs), so only client information counts.

/// Set to `kitty` to treat the terminal as Kitty-graphics capable whatever it
/// says (for a terminal that cannot be recognised), or to `off` to never draw
/// pictures. Inside tmux, `allow-passthrough` is still required.
pub const OVERRIDE_ENV: &str = "LUKS_SSH_UNLOCK_GRAPHICS";

/// The longest terminal name or version string that is looked at.
pub const MAX_NAME: usize = 128;

/// The most of a client's environment that is read.
pub const MAX_ENVIRON: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Override {
    Auto,
    Force,
    Off,
}

pub fn parse_override(value: Option<&str>) -> Override {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("kitty" | "on" | "1" | "true" | "yes" | "force") => Override::Force,
        Some("off" | "none" | "0" | "false" | "no") => Override::Off,
        _ => Override::Auto,
    }
}

pub fn override_from_env() -> Override {
    parse_override(std::env::var(OVERRIDE_ENV).ok().as_deref())
}

/// A terminal name (`TERM`, or the name tmux has for a client) that belongs to
/// a terminal with the Kitty graphics protocol.
pub fn classify_name(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    [
        ("kitty", "kitty"),
        ("wezterm", "WezTerm"),
        ("ghostty", "Ghostty"),
        ("konsole", "Konsole"),
    ]
    .into_iter()
    .find(|(needle, _)| name.contains(needle))
    .map(|(_, label)| label)
}

/// What a terminal's environment variables say about it. `get` looks a
/// variable up; a variable that is present (even empty) counts for the ones
/// that only exist inside that terminal.
pub fn classify_vars<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<&'static str> {
    let present = |name: &str| get(name).is_some();
    if present("KITTY_WINDOW_ID") {
        return Some("kitty");
    }
    if present("WEZTERM_PANE") || present("WEZTERM_EXECUTABLE") {
        return Some("WezTerm");
    }
    if present("GHOSTTY_RESOURCES_DIR") || present("GHOSTTY_BIN_DIR") {
        return Some("Ghostty");
    }
    if present("KONSOLE_VERSION") || present("KONSOLE_DBUS_SERVICE") {
        return Some("Konsole");
    }
    get("TERM_PROGRAM")
        .and_then(classify_name)
        .or_else(|| get("TERM").and_then(classify_name))
}

/// The same, for a `/proc/<pid>/environ` block (`NAME=value` entries separated
/// by NUL bytes).
pub fn classify_environ(environ: &[u8]) -> Option<&'static str> {
    let mut vars: Vec<(&str, &str)> = Vec::new();
    for entry in environ.split(|b| *b == 0) {
        let Ok(entry) = std::str::from_utf8(entry) else {
            continue;
        };
        if let Some((name, value)) = entry.split_once('=') {
            vars.push((name, value));
        }
    }
    classify_vars(|name| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| *v))
}

/// The terminal this process runs in, outside tmux.
pub fn classify_process_env() -> Option<&'static str> {
    let vars: Vec<(String, String)> = [
        "KITTY_WINDOW_ID",
        "WEZTERM_PANE",
        "WEZTERM_EXECUTABLE",
        "GHOSTTY_RESOURCES_DIR",
        "GHOSTTY_BIN_DIR",
        "KONSOLE_VERSION",
        "KONSOLE_DBUS_SERVICE",
        "TERM_PROGRAM",
        "TERM",
    ]
    .into_iter()
    .filter_map(|n| std::env::var_os(n).map(|v| (n.to_string(), v.to_string_lossy().into_owned())))
    .collect();
    classify_vars(|name| {
        vars.iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    })
}

/// One tmux client that shows the pane's window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Client {
    pub pid: u32,
    /// `#{client_termname}`: what `TERM` was when the client started.
    pub term: String,
    /// `#{client_termtype}`: the terminal's own `XTVERSION` answer, if any.
    pub version: String,
    /// What the client process's environment says (read from its owner's
    /// `/proc` entry), when it could be read.
    pub environment: Option<&'static str>,
}

impl Client {
    /// The Kitty-graphics terminal this client is, if it can be told.
    pub fn terminal(&self) -> Option<&'static str> {
        classify_name(&self.term)
            .or_else(|| classify_name(&self.version))
            .or(self.environment)
    }
}

/// What tmux said about the pane and its clients.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Probe {
    /// The effective `allow-passthrough` of the pane (`on`, `off`, `all`).
    pub passthrough: String,
    pub clients: Vec<Client>,
}

impl Probe {
    /// `on` and `all` carry images from the visible pane; `all` also from
    /// hidden ones.
    pub fn passthrough_enabled(&self) -> bool {
        matches!(self.passthrough.as_str(), "on" | "all")
    }

    /// The first client that is a Kitty-graphics terminal.
    pub fn terminal(&self) -> Option<&'static str> {
        self.clients.iter().find_map(Client::terminal)
    }

    /// Can pictures be shown? `Err` says why not, ready for the pane.
    pub fn verdict(&self, forced: bool) -> Result<(), String> {
        if !self.passthrough_enabled() {
            return Err(format!(
                "tmux does not pass images through: `allow-passthrough` is `{}` for this pane. Run `tmux set -p allow-passthrough on` (or put `set -g allow-passthrough on` in tmux.conf), then press Ctrl+R.",
                printable(&self.passthrough)
            ));
        }
        if forced || self.terminal().is_some() {
            return Ok(());
        }
        let what = match self.clients.as_slice() {
            [] => "no tmux client shows this window".to_string(),
            [c] => format!(
                "the tmux client reports TERM `{}`{}",
                printable(&c.term),
                if c.version.is_empty() {
                    " and no terminal version".to_string()
                } else {
                    format!(" and version `{}`", printable(&c.version))
                }
            ),
            many => format!(
                "the tmux clients report TERM {}",
                many.iter()
                    .map(|c| format!("`{}`", printable(&c.term)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        Err(format!(
            "The terminal behind tmux is not known to show images ({what}). It needs the Kitty graphics protocol (kitty, WezTerm, Ghostty or Konsole); if it has it, set {OVERRIDE_ENV}=kitty and press Ctrl+R."
        ))
    }
}

/// Why a terminal outside tmux is not used for pictures.
pub fn outside_reason(term: &str) -> String {
    let term = if term.is_empty() {
        "is not set".to_string()
    } else {
        format!("is `{}`", printable(term))
    };
    format!(
        "This terminal is not known to show images (TERM {term}). It needs the Kitty graphics protocol (kitty, WezTerm, Ghostty or Konsole); if it has it, set {OVERRIDE_ENV}=kitty and press Ctrl+R."
    )
}

/// A value from tmux made safe to show: printable ASCII only, and short.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_graphic() || c == ' ' {
                c
            } else {
                '?'
            }
        })
        .take(MAX_NAME)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(term: &str, version: &str, environment: Option<&'static str>) -> Client {
        Client {
            pid: 1,
            term: term.into(),
            version: version.into(),
            environment,
        }
    }

    fn probe(passthrough: &str, clients: Vec<Client>) -> Probe {
        Probe {
            passthrough: passthrough.into(),
            clients,
        }
    }

    #[test]
    fn names_of_kitty_graphics_terminals_are_recognised() {
        assert_eq!(classify_name("xterm-kitty"), Some("kitty"));
        assert_eq!(classify_name("WezTerm 20240203"), Some("WezTerm"));
        assert_eq!(classify_name("xterm-ghostty"), Some("Ghostty"));
        assert_eq!(classify_name("konsole-256color"), Some("Konsole"));
        // The generic names say nothing: they are what tmux and ssh hand on.
        for generic in ["screen-256color", "xterm-256color", "tmux-256color", ""] {
            assert_eq!(classify_name(generic), None, "{generic}");
        }
    }

    #[test]
    fn a_generic_term_is_resolved_by_version_or_environment() {
        let generic = "screen-256color";
        assert_eq!(client(generic, "", None).terminal(), None);
        assert_eq!(
            client(generic, "kitty(0.35.2)", None).terminal(),
            Some("kitty")
        );
        assert_eq!(
            client("xterm-256color", "", Some("Ghostty")).terminal(),
            Some("Ghostty")
        );
    }

    #[test]
    fn environments_name_their_terminal() {
        let env = b"HOME=/h\0KITTY_WINDOW_ID=3\0TERM=xterm-256color\0";
        assert_eq!(classify_environ(env), Some("kitty"));
        let env = b"TERM_PROGRAM=ghostty\0TERM=xterm-256color\0";
        assert_eq!(classify_environ(env), Some("Ghostty"));
        let env = b"WEZTERM_PANE=0\0";
        assert_eq!(classify_environ(env), Some("WezTerm"));
        let env = b"KONSOLE_VERSION=240802\0";
        assert_eq!(classify_environ(env), Some("Konsole"));
        // Plain xterm, truncated or binary entries: nothing to go on.
        assert_eq!(classify_environ(b"TERM=xterm-256color\0"), None);
        assert_eq!(
            classify_environ(b"TERM_PROGRAM=Apple_Terminal\0\xff\xfe\0"),
            None
        );
        assert_eq!(classify_environ(b""), None);
    }

    #[test]
    fn passthrough_on_and_all_both_count() {
        let kitty = vec![client("xterm-kitty", "", None)];
        assert_eq!(probe("on", kitty.clone()).verdict(false), Ok(()));
        assert_eq!(probe("all", kitty.clone()).verdict(false), Ok(()));
        let off = probe("off", kitty.clone()).verdict(false).unwrap_err();
        assert!(
            off.contains("allow-passthrough") && off.contains("`off`"),
            "{off}"
        );
        assert!(probe("", kitty).verdict(false).is_err());
    }

    #[test]
    fn an_unknown_terminal_is_reported_with_what_tmux_said() {
        let p = probe("on", vec![client("screen-256color", "", None)]);
        let reason = p.verdict(false).unwrap_err();
        assert!(reason.contains("screen-256color"), "{reason}");
        assert!(reason.contains(OVERRIDE_ENV), "{reason}");
        // The override accepts it, but never past a closed passthrough.
        assert_eq!(p.verdict(true), Ok(()));
        let closed = probe("off", vec![client("screen-256color", "", None)]);
        assert!(closed.verdict(true).is_err());
    }

    #[test]
    fn any_capable_client_is_enough() {
        let p = probe(
            "on",
            vec![
                client("xterm-256color", "", None),
                client("xterm-256color", "WezTerm 2024", None),
            ],
        );
        assert_eq!(p.verdict(false), Ok(()));
        assert_eq!(p.terminal(), Some("WezTerm"));
    }

    #[test]
    fn the_override_values() {
        assert_eq!(parse_override(None), Override::Auto);
        assert_eq!(parse_override(Some("")), Override::Auto);
        assert_eq!(parse_override(Some("Kitty")), Override::Force);
        assert_eq!(parse_override(Some("off")), Override::Off);
        assert_eq!(parse_override(Some("nonsense")), Override::Auto);
    }

    #[test]
    fn the_outside_reason_names_term_and_override() {
        let reason = outside_reason("xterm-256color");
        assert!(reason.contains("xterm-256color") && reason.contains(OVERRIDE_ENV));
        assert!(outside_reason("").contains("not set"));
        assert!(!outside_reason("a\x1bb").contains('\x1b'));
    }

    #[test]
    fn text_from_tmux_cannot_carry_escapes() {
        assert_eq!(printable("a\x1b[31mb"), "a?[31mb");
        assert_eq!(printable(&"x".repeat(500)).len(), MAX_NAME);
    }
}
