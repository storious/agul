use std::env;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

#[cfg(any(unix, test))]
use std::fs;

use serde::Deserialize;

use super::process::{ProcessLimits, ProcessTree, run_process};

pub(super) const SYSTEM_SKILL_PREFIX: &str = "system/";
const DECLARED_HOST_TOOLS_ENV: &str = "AGUL_HOST_TOOLS";
const AUTO_HOST_TOOLS: [&str; 3] = ["rg", "fzf", "git"];
const AGULATER: &str = "agulater";
const AGENTKUBE: &str = "agentkube";
const CATALOG_LIST_FORMAT: &str = "agulater/catalog-list/v1";
const CATALOG_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_CATALOG_STDOUT_BYTES: usize = 64 * 1024;
const MAX_CATALOG_STDERR_BYTES: usize = 4 * 1024;
static ECOSYSTEM_AVAILABILITY: OnceLock<EcosystemAvailability> = OnceLock::new();

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct EcosystemAvailability {
    agulater: bool,
    agentkube: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct HostTools {
    names: Vec<String>,
}

impl HostTools {
    pub(super) fn discover() -> Self {
        let path = env::var_os("PATH");
        let declared = env::var_os(DECLARED_HOST_TOOLS_ENV);
        let path_extensions = env::var_os("PATHEXT");
        let ecosystem = cached_ecosystem_availability(
            &ECOSYSTEM_AVAILABILITY,
            path.as_deref(),
            path_extensions.as_deref(),
            agentkube_catalog_registered,
        );
        Self::from_discovery(
            path.as_deref(),
            declared.as_deref(),
            path_extensions.as_deref(),
            ecosystem,
        )
    }

    #[cfg(test)]
    #[cfg(test)]
    fn discover_from(
        path: Option<&OsStr>,
        declared: Option<&OsStr>,
        path_extensions: Option<&OsStr>,
        catalog_probe: impl FnOnce(&Path) -> bool,
    ) -> Self {
        let ecosystem = discover_ecosystem_availability(path, path_extensions, catalog_probe);
        Self::from_discovery(path, declared, path_extensions, ecosystem)
    }

    fn from_discovery(
        path: Option<&OsStr>,
        declared: Option<&OsStr>,
        path_extensions: Option<&OsStr>,
        ecosystem: EcosystemAvailability,
    ) -> Self {
        let mut names = Vec::new();
        if let Some(path) = path {
            for name in AUTO_HOST_TOOLS {
                if executable_in_path(path, name, path_extensions).is_some() {
                    push_unique(&mut names, name);
                }
            }
        }
        if ecosystem.agulater {
            push_unique(&mut names, AGULATER);
        }
        if let Some(declared) = declared.and_then(OsStr::to_str) {
            for name in declared
                .split(',')
                .map(str::trim)
                .filter(|name| valid_name(name) && !reserved_ecosystem_name(name))
            {
                push_unique(&mut names, name);
            }
        }
        if ecosystem.agentkube {
            push_unique(&mut names, AGENTKUBE);
        }
        Self { names }
    }

    pub(super) fn prompt(&self) -> Option<String> {
        if self.names.is_empty() {
            return None;
        }
        let mut prompt = "System Skills (activate one with @skill:<name>):\n".to_string();
        for name in &self.names {
            prompt.push_str(&format!(
                "- {SYSTEM_SKILL_PREFIX}{name}: {}\n",
                description(name)
            ));
        }
        Some(prompt)
    }

    pub(super) fn skill_summaries(&self) -> Vec<(String, String)> {
        self.names
            .iter()
            .map(|name| {
                (
                    format!("{SYSTEM_SKILL_PREFIX}{name}"),
                    description(name).to_string(),
                )
            })
            .collect()
    }

    pub(super) fn activation(&self, name: &str) -> Option<String> {
        let tool = name.strip_prefix(SYSTEM_SKILL_PREFIX)?;
        let available = self
            .names
            .iter()
            .find(|available| available.eq_ignore_ascii_case(tool))?;
        let executable = if available.eq_ignore_ascii_case(AGENTKUBE) {
            AGULATER
        } else {
            available
        };
        Some(format!(
            "Use the host executable `{executable}` through the built-in shell tool when it is the clearest way to complete the task. {}",
            usage_hint(available)
        ))
    }

    #[cfg(test)]
    pub(super) fn from_names(names: &[&str]) -> Self {
        Self {
            names: names.iter().map(|name| (*name).to_string()).collect(),
        }
    }
}

fn description(name: &str) -> &'static str {
    match name {
        "rg" => "fast repository search and file listing",
        "fzf" => "non-interactive fuzzy filtering over explicit input",
        "git" => "repository status, diff, history, and user-requested version-control actions",
        AGULATER => "install, update, and prepare Agul and optional components",
        AGENTKUBE => {
            "find optional AgentKube Skills, Plugins, and prepared agents through Agulater"
        }
        _ => "user-declared host executable",
    }
}

fn usage_hint(name: &str) -> &'static str {
    match name {
        "rg" => "Prefer `rg PATTERN` for text search and `rg --files` for file listing.",
        "fzf" => {
            "Shell stdin is null, so pipe explicit input, for example `rg --files | fzf --filter QUERY`."
        }
        "git" => "Inspect status and diffs before changing repository state.",
        AGULATER => {
            "Use `agulater --help` for current syntax. Local queries may run directly; get one brief confirmation before downloading, installing, or updating. When an installed Skill path is returned, read its `SKILL.md` and use it in the current task."
        }
        AGENTKUBE => {
            "Use `agulater catalog search QUERY --json` to search the registered AgentKube catalog and `agulater add agentkube:<id> --user` to install a selected extension. AgentKube is content, not another CLI. Get one brief confirmation before the install."
        }
        _ => "Use only options appropriate to the user's request.",
    }
}

fn push_unique(names: &mut Vec<String>, name: &str) {
    if !names
        .iter()
        .any(|existing| existing.eq_ignore_ascii_case(name))
    {
        names.push(name.to_string());
    }
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'+'))
}

fn reserved_ecosystem_name(name: &str) -> bool {
    name.eq_ignore_ascii_case(AGULATER) || name.eq_ignore_ascii_case(AGENTKUBE)
}

fn cached_ecosystem_availability(
    cache: &OnceLock<EcosystemAvailability>,
    path: Option<&OsStr>,
    path_extensions: Option<&OsStr>,
    catalog_probe: impl FnOnce(&Path) -> bool,
) -> EcosystemAvailability {
    *cache.get_or_init(|| discover_ecosystem_availability(path, path_extensions, catalog_probe))
}

fn discover_ecosystem_availability(
    path: Option<&OsStr>,
    path_extensions: Option<&OsStr>,
    catalog_probe: impl FnOnce(&Path) -> bool,
) -> EcosystemAvailability {
    let Some(agulater) = path.and_then(|path| executable_in_path(path, AGULATER, path_extensions))
    else {
        return EcosystemAvailability::default();
    };
    EcosystemAvailability {
        agulater: true,
        agentkube: catalog_probe(&agulater),
    }
}

fn executable_in_path(
    path: &OsStr,
    name: &str,
    path_extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    env::split_paths(path)
        .find_map(|directory| executable_in_directory(&directory, name, path_extensions))
}

#[cfg(windows)]
fn executable_in_directory(
    directory: &Path,
    name: &str,
    path_extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    let mut extensions = path_extensions
        .and_then(OsStr::to_str)
        .into_iter()
        .flat_map(|value| value.split(';'))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.starts_with('.') {
                value.to_string()
            } else {
                format!(".{value}")
            }
        })
        .collect::<Vec<_>>();
    for required in [".exe", ".com", ".cmd", ".bat"] {
        if !extensions
            .iter()
            .any(|extension| extension.eq_ignore_ascii_case(required))
        {
            extensions.push(required.to_string());
        }
    }

    std::iter::once(name.to_string())
        .chain(
            extensions
                .into_iter()
                .map(|extension| format!("{name}{extension}")),
        )
        .map(|candidate| directory.join(candidate))
        .find(|candidate| candidate.is_file())
}

#[cfg(unix)]
fn executable_in_directory(
    directory: &Path,
    name: &str,
    _path_extensions: Option<&OsStr>,
) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    let candidate = directory.join(name);
    fs::metadata(&candidate)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .then_some(candidate)
}

#[derive(Deserialize)]
struct CatalogList {
    format: String,
    catalogs: Vec<CatalogRegistration>,
}

#[derive(Deserialize)]
struct CatalogRegistration {
    id: String,
}

fn agentkube_catalog_registered(program: &Path) -> bool {
    let mut command = Command::new(program);
    command
        .args(["catalog", "list", "--json"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut tree) = ProcessTree::prepare(&mut command) else {
        return false;
    };
    let Ok(mut child) = command.spawn() else {
        return false;
    };
    if tree.assign(&mut child).is_err() {
        return false;
    }
    let Ok(output) = run_process(
        &mut child,
        tree,
        None,
        ProcessLimits::new(
            CATALOG_PROBE_TIMEOUT,
            MAX_CATALOG_STDOUT_BYTES,
            MAX_CATALOG_STDERR_BYTES,
        ),
    ) else {
        return false;
    };
    if output.timed_out
        || output.stdout_truncated
        || !output.status.is_some_and(|status| status.success())
    {
        return false;
    }
    catalog_output_has_agentkube(&output.stdout)
}

fn catalog_output_has_agentkube(output: &[u8]) -> bool {
    serde_json::from_slice::<CatalogList>(output).is_ok_and(|list| {
        list.format == CATALOG_LIST_FORMAT
            && list.catalogs.iter().any(|catalog| catalog.id == AGENTKUBE)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_only_known_path_tools_and_valid_explicit_names() {
        let root = tempfile::tempdir().unwrap();
        create_executable(root.path(), "rg");
        create_executable(root.path(), "fzf");
        let path = env::join_paths([root.path()]).unwrap();

        let tools = HostTools::discover_from(
            Some(&path),
            Some(OsStr::new("custom-tool,rg,bad/name,custom-tool")),
            None,
            |_| false,
        );
        assert_eq!(tools.names, ["rg", "fzf", "custom-tool"]);
        let prompt = tools.prompt().unwrap();
        assert!(prompt.contains("system/rg"));
        assert!(prompt.contains("system/fzf"));
        assert!(!prompt.contains("bad/name"));
        assert!(
            tools
                .activation("system/custom-tool")
                .unwrap()
                .contains("custom-tool")
        );
        assert!(
            tools
                .activation("system/fzf")
                .unwrap()
                .contains("fzf --filter")
        );
        assert_eq!(tools.activation("system/missing"), None);
    }

    #[test]
    fn exposes_agulater_and_registered_agentkube_without_new_tools() {
        let root = tempfile::tempdir().unwrap();
        create_executable(root.path(), AGULATER);
        let path = env::join_paths([root.path()]).unwrap();

        let tools = HostTools::discover_from(
            Some(&path),
            Some(OsStr::new("agulater,agentkube")),
            Some(OsStr::new(".EXE;.CMD;.BAT")),
            |_| true,
        );

        assert_eq!(tools.names, [AGULATER, AGENTKUBE]);
        let prompt = tools.prompt().unwrap();
        assert!(prompt.contains("system/agulater"));
        assert!(prompt.contains("system/agentkube"));
        let activation = tools.activation("system/agentkube").unwrap();
        assert!(activation.contains("agulater catalog search"));
        assert!(activation.contains("not another CLI"));
    }

    #[test]
    fn hides_agentkube_when_catalog_probe_fails() {
        let root = tempfile::tempdir().unwrap();
        create_executable(root.path(), AGULATER);
        let path = env::join_paths([root.path()]).unwrap();

        let tools = HostTools::discover_from(Some(&path), None, None, |_| false);

        assert_eq!(tools.names, [AGULATER]);
        assert_eq!(tools.activation("system/agentkube"), None);
    }

    #[test]
    fn skips_catalog_probe_when_agulater_is_absent() {
        let tools = HostTools::discover_from(None, None, None, |_| {
            panic!("catalog probe must not run without Agulater")
        });

        assert!(tools.names.is_empty());
    }

    #[test]
    fn caches_agulater_and_catalog_discovery_for_the_process() {
        let root = tempfile::tempdir().unwrap();
        create_executable(root.path(), AGULATER);
        let path = env::join_paths([root.path()]).unwrap();
        let cache = OnceLock::new();
        let probes = std::cell::Cell::new(0);

        let first = cached_ecosystem_availability(&cache, Some(&path), None, |_| {
            probes.set(probes.get() + 1);
            true
        });
        let second = cached_ecosystem_availability(&cache, None, None, |_| {
            probes.set(probes.get() + 1);
            false
        });

        assert_eq!(
            first,
            EcosystemAvailability {
                agulater: true,
                agentkube: true,
            }
        );
        assert_eq!(second, first);
        assert_eq!(probes.get(), 1);
    }

    #[test]
    fn accepts_only_the_versioned_agentkube_catalog_report() {
        assert!(catalog_output_has_agentkube(
            br#"{"format":"agulater/catalog-list/v1","catalogs":[{"id":"agentkube","cached":false}]}"#
        ));
        assert!(!catalog_output_has_agentkube(
            br#"{"format":"agulater/catalog-list/v1","catalogs":[{"id":"other"}]}"#
        ));
        assert!(!catalog_output_has_agentkube(
            br#"{"format":"agentkube/catalog/v1","catalogs":[{"id":"agentkube"}]}"#
        ));
        assert!(!catalog_output_has_agentkube(b"not json"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_detection_includes_command_scripts() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("agulater.cmd"), b"@echo off\r\n").unwrap();
        let path = env::join_paths([root.path()]).unwrap();

        assert_eq!(
            executable_in_path(&path, AGULATER, Some(OsStr::new(".EXE"))),
            Some(root.path().join("agulater.cmd"))
        );
    }

    #[cfg(windows)]
    fn create_executable(directory: &Path, name: &str) {
        fs::write(directory.join(format!("{name}.exe")), b"fixture").unwrap();
    }

    #[cfg(unix)]
    fn create_executable(directory: &Path, name: &str) {
        use std::os::unix::fs::PermissionsExt;

        let path = directory.join(name);
        fs::write(&path, b"fixture").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
