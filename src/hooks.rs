//! Declarative hook configuration and deterministic hook selection.
//!
//! This module loads hook descriptions only. It does not execute commands or
//! dispatch lifecycle events.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{LegError, Result};

/// Environment variable naming the highest-precedence hook configuration file.
pub const HOOKS_CONFIG_ENV: &str = "LEG_HOOKS_CONFIG";

/// The six event names supported by the declarative hooks v1 contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HookEvent {
    /// Before a tool is dispatched.
    PreToolUse,
    /// After a tool is dispatched.
    PostToolUse,
    /// At the start of a turn.
    TurnStart,
    /// At the end of a turn.
    TurnEnd,
    /// At the start of a session.
    SessionStart,
    /// At the end of a session.
    SessionEnd,
}

impl HookEvent {
    /// The exact v1 configuration spelling for this event.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PreToolUse => "PreToolUse",
            Self::PostToolUse => "PostToolUse",
            Self::TurnStart => "TurnStart",
            Self::TurnEnd => "TurnEnd",
            Self::SessionStart => "SessionStart",
            Self::SessionEnd => "SessionEnd",
        }
    }

    /// Whether this event can be narrowed to a tool name.
    pub const fn is_tool_event(self) -> bool {
        matches!(self, Self::PreToolUse | Self::PostToolUse)
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "PreToolUse" => Some(Self::PreToolUse),
            "PostToolUse" => Some(Self::PostToolUse),
            "TurnStart" => Some(Self::TurnStart),
            "TurnEnd" => Some(Self::TurnEnd),
            "SessionStart" => Some(Self::SessionStart),
            "SessionEnd" => Some(Self::SessionEnd),
            _ => None,
        }
    }
}

/// One validated declarative hook entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSpec {
    /// Stable, case-sensitive identifier unique within the selected file.
    pub id: String,
    /// Event this hook matches.
    pub event: HookEvent,
    /// Optional exact tool-name matcher, or `*` for all tools.
    pub matcher: Option<String>,
    /// Executable path, resolved relative to the selected file when not absolute.
    pub command: PathBuf,
    /// Whether selection may return this hook.
    pub enabled: bool,
}

/// A loaded, validated hook file in its original array order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSet {
    hooks: Vec<HookSpec>,
}

impl HookSet {
    /// Returns the hooks matching `event` and, for tool events, `tool`.
    ///
    /// Results preserve file order. A missing matcher or `*` matches every
    /// tool; an exact matcher requires the same case-sensitive tool name.
    pub fn select(&self, event: HookEvent, tool: Option<&str>) -> Vec<&HookSpec> {
        self.hooks
            .iter()
            .filter(|hook| {
                hook.enabled
                    && hook.event == event
                    && match hook.matcher.as_deref() {
                        None | Some("*") => true,
                        Some(matcher) => tool == Some(matcher),
                    }
            })
            .collect()
    }

    /// All validated entries, including disabled entries, in file order.
    pub fn entries(&self) -> &[HookSpec] {
        &self.hooks
    }
}

/// A candidate returned by [`crate::config::LegConfig::select_hooks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectedHook<'a> {
    /// The compatibility hook configured through `LEG_PRETOOL_HOOK`.
    LegacyPreTool(&'a Path),
    /// A declarative hook selected from the v1 file.
    Declarative(&'a HookSpec),
}

/// Paths considered by hook configuration discovery, in precedence order.
///
/// `explicit` is `LEG_HOOKS_CONFIG`; `project` is `<cwd>/.leg/hooks.json`;
/// `user` is the platform's per-user configuration file. Discovery selects the
/// first existing source and never merges files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookConfigPaths {
    /// Explicit path from `LEG_HOOKS_CONFIG`.
    pub explicit: Option<PathBuf>,
    /// Project file under the current working directory.
    pub project: Option<PathBuf>,
    /// Optional per-user configuration file.
    pub user: Option<PathBuf>,
}

/// Resolves hook configuration source paths without reading the filesystem.
///
/// `lookup` is injectable so platform path resolution and precedence can be
/// tested without changing process environment variables or the real home.
pub fn discover_hook_config_paths(
    cwd: &Path,
    lookup: impl Fn(&str) -> Option<String>,
) -> HookConfigPaths {
    let explicit = non_empty(lookup(HOOKS_CONFIG_ENV)).map(PathBuf::from);
    let project = Some(cwd.join(".leg").join("hooks.json"));
    let user = user_config_path(lookup);
    HookConfigPaths {
        explicit,
        project,
        user,
    }
}

/// Loads the first available source using the supplied file reader.
///
/// A missing implicit project or user file falls through to the next source.
/// An explicit path that is missing, any other read error, or a malformed
/// selected file is fatal and never falls through. With no file, returns an
/// empty set.
pub fn load_hook_set(
    paths: &HookConfigPaths,
    mut read_file: impl FnMut(&Path) -> io::Result<String>,
) -> Result<HookSet> {
    let candidates = [
        (paths.explicit.as_deref(), true),
        (paths.project.as_deref(), false),
        (paths.user.as_deref(), false),
    ];

    for (path, explicit) in candidates {
        let Some(path) = path else {
            continue;
        };
        match read_file(path) {
            Ok(contents) => return parse_hook_file(path, &contents),
            Err(error) if !explicit && error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(LegError::Config(format!(
                    "could not read hooks config {}: {error}",
                    path.display()
                )));
            }
        }
    }

    Ok(HookSet::default())
}

fn user_config_path(lookup: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        non_empty(lookup("APPDATA")).map(|root| PathBuf::from(root).join("leg").join("hooks.json"))
    }
    #[cfg(not(windows))]
    {
        if let Some(root) = non_empty(lookup("XDG_CONFIG_HOME")) {
            Some(PathBuf::from(root).join("leg").join("hooks.json"))
        } else {
            non_empty(lookup("HOME")).map(|root| {
                PathBuf::from(root)
                    .join(".config")
                    .join("leg")
                    .join("hooks.json")
            })
        }
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHookFile {
    version: u64,
    hooks: Vec<RawHookSpec>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHookSpec {
    id: String,
    event: String,
    #[serde(default)]
    matcher: Option<String>,
    command: String,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

fn default_enabled() -> bool {
    true
}

fn parse_hook_file(path: &Path, contents: &str) -> Result<HookSet> {
    let raw: RawHookFile = serde_json::from_str(contents).map_err(|error| {
        LegError::Config(format!("invalid hooks config {}: {error}", path.display()))
    })?;
    if raw.version != 1 {
        return Err(config_error(
            path,
            format!(
                "unsupported hooks config version {}; expected 1",
                raw.version
            ),
        ));
    }

    let config_dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut ids = HashSet::with_capacity(raw.hooks.len());
    let mut hooks = Vec::with_capacity(raw.hooks.len());
    for raw_hook in raw.hooks {
        if !valid_id(&raw_hook.id) {
            return Err(config_error(
                path,
                format!(
                    "hook id {:?} must match [A-Za-z0-9][A-Za-z0-9._-]{{0,127}}",
                    raw_hook.id
                ),
            ));
        }
        if !ids.insert(raw_hook.id.clone()) {
            return Err(config_error(
                path,
                format!("duplicate hook id {:?}", raw_hook.id),
            ));
        }
        let event = HookEvent::parse(&raw_hook.event)
            .ok_or_else(|| config_error(path, format!("unknown event {:?}", raw_hook.event)))?;
        if let Some(matcher) = &raw_hook.matcher {
            if !event.is_tool_event() {
                return Err(config_error(
                    path,
                    format!("event {} does not accept a tool matcher", event.as_str()),
                ));
            }
            if !valid_matcher(matcher) {
                return Err(config_error(
                    path,
                    format!("invalid tool matcher {matcher:?}"),
                ));
            }
        }
        if raw_hook.command.trim().is_empty() {
            return Err(config_error(
                path,
                format!("hook {:?} has an empty command", raw_hook.id),
            ));
        }
        let command = PathBuf::from(raw_hook.command);
        let command = if command.is_absolute() {
            command
        } else {
            config_dir.join(command)
        };
        hooks.push(HookSpec {
            id: raw_hook.id,
            event,
            matcher: raw_hook.matcher,
            command,
            enabled: raw_hook.enabled,
        });
    }

    Ok(HookSet { hooks })
}

fn valid_id(id: &str) -> bool {
    let mut bytes = id.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        && id.len() <= 128
}

fn valid_matcher(matcher: &str) -> bool {
    matcher == "*"
        || (!matcher.is_empty()
            && matcher.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':')
            }))
}

fn config_error(path: &Path, message: String) -> LegError {
    LegError::Config(format!(
        "invalid hooks config {}: {message}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::config::LegConfig;

    fn source_paths(
        explicit: Option<&str>,
        project: Option<&str>,
        user: Option<&str>,
    ) -> HookConfigPaths {
        HookConfigPaths {
            explicit: explicit.map(PathBuf::from),
            project: project.map(PathBuf::from),
            user: user.map(PathBuf::from),
        }
    }

    fn file(id: &str) -> String {
        format!(
            r#"{{"version":1,"hooks":[{{"id":"{id}","event":"PreToolUse","command":"check"}}]}}"#
        )
    }

    fn load_from_map(paths: &HookConfigPaths, contents: &[(&str, &str)]) -> Result<HookSet> {
        let contents: HashMap<PathBuf, String> = contents
            .iter()
            .map(|(path, body)| (PathBuf::from(path), (*body).to_string()))
            .collect();
        load_hook_set(paths, |path| {
            contents
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "test config not found"))
        })
    }

    #[test]
    fn first_found_source_wins_and_sources_are_not_merged() {
        let paths = source_paths(Some("env.json"), Some("project.json"), Some("user.json"));
        let selected = load_from_map(
            &paths,
            &[
                ("env.json", &file("env")),
                ("project.json", &file("project")),
                ("user.json", &file("user")),
            ],
        )
        .unwrap();
        assert_eq!(selected.entries()[0].id, "env");

        let paths = source_paths(None, Some("project.json"), Some("user.json"));
        let selected = load_from_map(
            &paths,
            &[
                ("project.json", &file("project")),
                ("user.json", &file("user")),
            ],
        )
        .unwrap();
        assert_eq!(selected.entries()[0].id, "project");
    }

    #[test]
    fn missing_implicit_sources_fall_through_but_missing_explicit_path_fails() {
        let paths = source_paths(None, Some("project.json"), Some("user.json"));
        let selected = load_from_map(&paths, &[("user.json", &file("user"))]).unwrap();
        assert_eq!(selected.entries()[0].id, "user");

        let empty = load_from_map(&source_paths(None, Some("project.json"), None), &[]).unwrap();
        assert!(empty.entries().is_empty());

        let error = load_from_map(
            &source_paths(
                Some("missing.json"),
                Some("project.json"),
                Some("user.json"),
            ),
            &[],
        )
        .unwrap_err();
        assert!(format!("{error}").contains("missing.json"));
    }

    #[test]
    fn selected_empty_file_suppresses_lower_precedence_sources() {
        let paths = source_paths(None, Some("project.json"), Some("user.json"));
        let empty = r#"{"version":1,"hooks":[]}"#;
        let selected = load_from_map(
            &paths,
            &[("project.json", empty), ("user.json", &file("user"))],
        )
        .unwrap();
        assert!(selected.entries().is_empty());
    }

    #[test]
    fn environment_paths_cover_platform_user_path_and_missing_fallback() {
        let cwd = Path::new("workspace");
        let blank_env = |_: &str| None;
        let paths = discover_hook_config_paths(cwd, blank_env);
        assert_eq!(paths.project, Some(cwd.join(".leg").join("hooks.json")));
        assert!(paths.explicit.is_none());
        assert!(paths.user.is_none());

        let paths = discover_hook_config_paths(cwd, |key| {
            (key == HOOKS_CONFIG_ENV).then(|| String::from(" explicit.json "))
        });
        assert_eq!(paths.explicit, Some(PathBuf::from("explicit.json")));

        #[cfg(windows)]
        {
            let paths = discover_hook_config_paths(cwd, |key| {
                (key == "APPDATA").then(|| String::from(r"C:\Users\test\AppData\Roaming"))
            });
            assert_eq!(
                paths.user,
                Some(PathBuf::from(
                    r"C:\Users\test\AppData\Roaming\leg\hooks.json"
                ))
            );
        }
        #[cfg(not(windows))]
        {
            let paths = discover_hook_config_paths(cwd, |key| match key {
                "XDG_CONFIG_HOME" => Some(String::from("/tmp/config")),
                _ => None,
            });
            assert_eq!(
                paths.user,
                Some(PathBuf::from("/tmp/config/leg/hooks.json"))
            );

            let paths = discover_hook_config_paths(cwd, |key| match key {
                "HOME" => Some(String::from("/home/test")),
                _ => None,
            });
            assert_eq!(
                paths.user,
                Some(PathBuf::from("/home/test/.config/leg/hooks.json"))
            );

            let paths = discover_hook_config_paths(cwd, |key| match key {
                "XDG_CONFIG_HOME" => Some(String::from("  ")),
                "HOME" => Some(String::from("/home/fallback")),
                _ => None,
            });
            assert_eq!(
                paths.user,
                Some(PathBuf::from("/home/fallback/.config/leg/hooks.json"))
            );
        }
    }

    #[test]
    fn invalid_config_and_duplicate_ids_fail_the_whole_file() {
        let invalid = [
            r#"{"version":2,"hooks":[]}"#,
            r#"{"version":1,"hooks":[{"id":"bad id","event":"PreToolUse","command":"x"}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"Unknown","command":"x"}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"PreToolUse","matcher":"" ,"command":"x"}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"PreToolUse","matcher":"bash|read","command":"x"}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"PreToolUse","matcher":"bash|read","command":"x","enabled":false}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"SessionStart","matcher":"*","command":"x"}]}"#,
            r#"{"version":1,"hooks":[{"id":"x","event":"PreToolUse","command":" "}]}"#,
            r#"{"version":1,"hooks":[],"unknown":true}"#,
            "",
        ];
        for contents in invalid {
            assert!(
                parse_hook_file(Path::new("hooks.json"), contents).is_err(),
                "accepted invalid config: {contents}"
            );
        }

        let duplicate = r#"{"version":1,"hooks":[{"id":"same","event":"PreToolUse","command":"a"},{"id":"same","event":"PreToolUse","command":"b","enabled":false}]}"#;
        assert!(parse_hook_file(Path::new("hooks.json"), duplicate).is_err());
    }

    #[test]
    fn invalid_selected_project_file_does_not_fall_through_to_user_file() {
        let paths = source_paths(None, Some("project.json"), Some("user.json"));
        let error = load_from_map(
            &paths,
            &[
                ("project.json", r#"{"version":1,"hooks":[{"id":"bad"}]}"#),
                ("user.json", &file("user")),
            ],
        )
        .unwrap_err();
        assert!(format!("{error}").contains("project.json"));
    }

    #[test]
    fn accepts_only_the_six_exact_v1_event_names() {
        let events = [
            ("PreToolUse", HookEvent::PreToolUse),
            ("PostToolUse", HookEvent::PostToolUse),
            ("TurnStart", HookEvent::TurnStart),
            ("TurnEnd", HookEvent::TurnEnd),
            ("SessionStart", HookEvent::SessionStart),
            ("SessionEnd", HookEvent::SessionEnd),
        ];
        for (name, event) in events {
            assert_eq!(HookEvent::parse(name), Some(event));
            assert_eq!(event.as_str(), name);
        }
        assert_eq!(HookEvent::parse("pretooluse"), None);
    }

    #[test]
    fn selection_matches_events_and_tools_in_file_order_and_skips_disabled() {
        let contents = r#"{"version":1,"hooks":[
            {"id":"all","event":"PreToolUse","command":"all"},
            {"id":"read","event":"PreToolUse","matcher":"read","command":"read"},
            {"id":"wild","event":"PreToolUse","matcher":"*","command":"wild"},
            {"id":"off","event":"PreToolUse","command":"off","enabled":false},
            {"id":"post-bash","event":"PostToolUse","matcher":"bash","command":"post-bash"},
            {"id":"session","event":"SessionStart","command":"session"}
        ]}"#;
        let hooks = parse_hook_file(Path::new("config/hooks.json"), contents).unwrap();
        let selected = hooks.select(HookEvent::PreToolUse, Some("read"));
        assert_eq!(
            selected
                .iter()
                .map(|hook| hook.id.as_str())
                .collect::<Vec<_>>(),
            ["all", "read", "wild"]
        );
        assert_eq!(
            hooks
                .select(HookEvent::PreToolUse, Some("bash"))
                .iter()
                .map(|hook| hook.id.as_str())
                .collect::<Vec<_>>(),
            ["all", "wild"]
        );
        assert_eq!(
            hooks
                .select(HookEvent::PreToolUse, Some("Read"))
                .iter()
                .map(|hook| hook.id.as_str())
                .collect::<Vec<_>>(),
            ["all", "wild"]
        );
        assert_eq!(
            hooks.select(HookEvent::PostToolUse, Some("bash"))[0].id,
            "post-bash"
        );
        assert_eq!(hooks.select(HookEvent::SessionStart, None)[0].id, "session");
        assert_eq!(
            hooks.entries()[0].command,
            PathBuf::from("config/hooks.json")
                .parent()
                .unwrap()
                .join("all")
        );
    }

    #[test]
    fn legacy_hook_is_first_in_combined_pretool_selection() {
        let mut config = LegConfig::from_lookup(|key| match key {
            "ANTHROPIC_API_KEY" => Some("secret".to_string()),
            "LEG_PRETOOL_HOOK" => Some("legacy-hook".to_string()),
            _ => None,
        })
        .unwrap();
        config.hooks = parse_hook_file(Path::new("hooks.json"), &file("declarative")).unwrap();

        let selected = config.select_hooks(HookEvent::PreToolUse, Some("read"));
        assert!(matches!(selected[0], SelectedHook::LegacyPreTool(_)));
        assert!(matches!(selected[1], SelectedHook::Declarative(hook) if hook.id == "declarative"));
        assert_eq!(config.select_hooks(HookEvent::SessionStart, None).len(), 0);
    }
}
