use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::AGUL_LAUNCH_FORMAT;
use super::host_tools::{HostTools, SYSTEM_SKILL_PREFIX};
use super::plugin::{self, PluginCapability, PluginCommand, PluginTool};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LaunchFile {
    format: String,
    instructions: String,
    #[serde(default)]
    skills: Option<String>,
    #[serde(default)]
    plugins: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct Launch {
    pub(crate) path: PathBuf,
    pub(crate) instructions: PathBuf,
    pub(crate) skills: Option<PathBuf>,
    pub(crate) plugins: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct Skill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) path: PathBuf,
}

impl Skill {
    pub(crate) fn activation(&self) -> Result<String, ProjectError> {
        fs::read_to_string(&self.path).map_err(|error| {
            ProjectError::new(format!("could not read {}: {error}", self.path.display()))
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Project {
    pub(crate) workspace: PathBuf,
    pub(crate) launch: Option<Launch>,
    pub(crate) instructions: Vec<(PathBuf, String)>,
    pub(crate) skills: Vec<Skill>,
    pub(crate) plugin_tools: Vec<PluginTool>,
    pub(crate) plugin_commands: Vec<PluginCommand>,
    pub(crate) plugin_capabilities: Vec<PluginCapability>,
    host_tools: HostTools,
}

impl Project {
    pub(crate) fn canonical_workspace(
        workspace: impl AsRef<Path>,
    ) -> Result<PathBuf, ProjectError> {
        absolute_directory(workspace.as_ref())
    }

    pub(crate) fn discover(
        workspace: impl AsRef<Path>,
        explicit_launch: Option<&Path>,
    ) -> Result<Self, ProjectError> {
        let workspace = Self::canonical_workspace(workspace)?;
        let launch = match explicit_launch {
            Some(path) => Some(read_launch(&absolutize(&workspace, path))?),
            None => discover_launch(&workspace)?
                .map(|path| read_launch(&path))
                .transpose()?,
        };
        let instructions = discover_instructions(&workspace, launch.as_ref())?;
        let skills = discover_skills(&workspace, launch.as_ref())?;
        let plugins =
            plugin::discover(launch.as_ref().and_then(|launch| launch.plugins.as_deref()))
                .map_err(|error| ProjectError::new(error.to_string()))?;
        let host_tools = HostTools::discover();
        Ok(Self {
            workspace,
            launch,
            instructions,
            skills,
            plugin_tools: plugins.tools,
            plugin_commands: plugins.commands,
            plugin_capabilities: plugins.capabilities,
            host_tools,
        })
    }

    pub(crate) fn system_prompt(&self) -> String {
        let mut prompt = format!(
            "You are Agul, a coding agent working in {}. Complete the user's task directly. Inspect the project, use the available tools, make the requested changes, and run the relevant checks. Keep going after a tool error by reading the result and trying a better action.",
            self.workspace.display()
        );
        if !self.instructions.is_empty() {
            prompt.push_str("\n\nProject instructions:\n");
            for (path, body) in &self.instructions {
                prompt.push_str(&format!("\n--- {} ---\n{}\n", path.display(), body.trim()));
            }
        }
        if !self.skills.is_empty() {
            prompt.push_str("\n\nAvailable Skills (activate one with @skill:<name>):\n");
            for skill in &self.skills {
                prompt.push_str(&format!("- {}: {}\n", skill.name, skill.description));
            }
        }
        if let Some(launch) = &self.launch {
            prompt.push_str(&format!("\n\nPrepared launch: {}", launch.path.display()));
        }
        if let Some(host_tools) = self.host_tools.prompt() {
            prompt.push_str("\n\n");
            prompt.push_str(&host_tools);
        }
        prompt
    }

    pub(crate) fn skill(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    pub(crate) fn skill_summaries(&self) -> Vec<(String, String)> {
        self.skills
            .iter()
            .map(|skill| (skill.name.clone(), skill.description.clone()))
            .chain(self.host_tools.skill_summaries())
            .collect()
    }

    pub(crate) fn activate_skills<'a>(
        &self,
        input: &str,
        names: impl IntoIterator<Item = &'a str>,
    ) -> Result<String, ProjectError> {
        let names = names.into_iter().collect::<Vec<_>>();
        if names.is_empty() {
            return Ok(input.to_string());
        }
        let mut expanded = input.to_string();
        let mut activated = HashSet::new();
        expanded.push_str("\n\nActivated Skills:\n");
        for name in names {
            if !activated.insert(name) {
                continue;
            }
            let activation = match self.skill(name) {
                Some(skill) => skill.activation()?,
                None => self
                    .host_tools
                    .activation(name)
                    .ok_or_else(|| ProjectError::new(format!("Skill not found: {name}")))?,
            };
            expanded.push_str(&format!("\n--- @skill:{name} ---\n{}\n", activation.trim()));
        }
        Ok(expanded)
    }

    /// Expand explicit `@skill:name` references for non-TUI callers such as
    /// ARI. `@@skill:name` remains ordinary text.
    pub(crate) fn activate_references(&self, input: &str) -> Result<String, ProjectError> {
        let names = skill_references(input);
        self.activate_skills(&input.replace("@@", "@"), names.iter().map(String::as_str))
    }
}

pub(crate) fn skill_references(input: &str) -> Vec<String> {
    input
        .split_whitespace()
        .filter_map(|token| token.strip_prefix("@skill:"))
        .filter(|value| {
            !value.is_empty() && value.len() <= 255 && value.chars().all(is_skill_reference_char)
        })
        .map(str::to_string)
        .collect()
}

pub(crate) fn is_skill_reference_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':' | '/')
}

fn discover_launch(workspace: &Path) -> Result<Option<PathBuf>, ProjectError> {
    for directory in workspace.ancestors() {
        let candidate = directory.join(".agents/runtime/launch.json");
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }
    Ok(user_home().and_then(|home| {
        let candidate = home.join(".agents/runtime/launch.json");
        candidate.is_file().then_some(candidate)
    }))
}

fn read_launch(path: &Path) -> Result<Launch, ProjectError> {
    let bytes = fs::read(path).map_err(|error| {
        ProjectError::new(format!("could not read {}: {error}", path.display()))
    })?;
    let value: LaunchFile = serde_json::from_slice(&bytes).map_err(|error| {
        ProjectError::new(format!("could not parse {}: {error}", path.display()))
    })?;
    if value.format != AGUL_LAUNCH_FORMAT {
        return Err(ProjectError::new(format!(
            "{} format must be {AGUL_LAUNCH_FORMAT}",
            path.display()
        )));
    }
    let root = path.parent().unwrap_or_else(|| Path::new("."));
    Ok(Launch {
        path: path.to_path_buf(),
        instructions: root.join(value.instructions),
        skills: value.skills.map(|value| root.join(value)),
        plugins: value.plugins.map(|value| root.join(value)),
    })
}

fn discover_instructions(
    workspace: &Path,
    launch: Option<&Launch>,
) -> Result<Vec<(PathBuf, String)>, ProjectError> {
    let mut instructions = Vec::new();
    if let Some(path) = launch.map(|launch| launch.instructions.as_path()) {
        instructions.push((path.to_path_buf(), read_text(path)?));
    }
    if launch.is_some_and(|launch| !is_user_launch(launch)) {
        return Ok(instructions);
    }
    for directory in workspace.ancestors() {
        for candidate in [
            directory.join(".agents/AGENTS.md"),
            directory.join("AGENTS.md"),
        ] {
            if candidate.is_file()
                && !instructions
                    .iter()
                    .any(|(path, _)| same_path(path, &candidate))
            {
                instructions.push((candidate.clone(), read_text(&candidate)?));
                return Ok(instructions);
            }
        }
    }
    Ok(instructions)
}

fn discover_skills(workspace: &Path, launch: Option<&Launch>) -> Result<Vec<Skill>, ProjectError> {
    let home = user_home();
    let user_launch = discover_user_launch(home.as_deref(), launch)?;
    discover_skills_from(workspace, launch, user_launch.as_ref(), home.as_deref())
}

fn discover_user_launch(
    home: Option<&Path>,
    active_launch: Option<&Launch>,
) -> Result<Option<Launch>, ProjectError> {
    let Some(home) = home else {
        return Ok(None);
    };
    if let Some(launch) = active_launch.filter(|launch| is_user_launch_at(launch, home)) {
        return Ok(Some(launch.clone()));
    }
    let candidate = home.join(".agents/runtime/launch.json");
    candidate
        .is_file()
        .then(|| read_launch(&candidate))
        .transpose()
}

fn discover_skills_from(
    workspace: &Path,
    launch: Option<&Launch>,
    user_launch: Option<&Launch>,
    home: Option<&Path>,
) -> Result<Vec<Skill>, ProjectError> {
    let mut roots = Vec::new();
    if let Some(path) = launch
        .filter(|launch| home.is_none_or(|home| !is_user_launch_at(launch, home)))
        .and_then(|launch| launch.skills.as_deref())
    {
        roots.push(path.to_path_buf());
    }
    for directory in workspace.ancestors() {
        let root = directory.join(".agents/skills");
        let is_user_raw = home.is_some_and(|home| same_path(&root, &home.join(".agents/skills")));
        if root.is_dir() && !is_user_raw {
            roots.push(root);
        }
    }
    if let Some(path) = user_launch.and_then(|launch| launch.skills.as_deref()) {
        roots.push(path.to_path_buf());
    }
    if let Some(home) = home {
        roots.extend([
            home.join(".codex/skills"),
            home.join(".claude/skills"),
            home.join(".agents/skills"),
        ]);
    }

    let mut names = HashSet::new();
    let mut skills = Vec::new();
    for root in roots {
        collect_skills(&root, 0, &mut names, &mut skills)?;
    }
    skills.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(skills)
}

fn is_user_launch(launch: &Launch) -> bool {
    user_home().is_some_and(|home| is_user_launch_at(launch, &home))
}

fn is_user_launch_at(launch: &Launch, home: &Path) -> bool {
    launch.path.starts_with(home.join(".agents"))
}

fn collect_skills(
    directory: &Path,
    depth: usize,
    names: &mut HashSet<String>,
    skills: &mut Vec<Skill>,
) -> Result<(), ProjectError> {
    if depth > 4 || !directory.is_dir() {
        return Ok(());
    }
    let skill_file = directory.join("SKILL.md");
    if skill_file.is_file() {
        let body = read_text(&skill_file)?;
        let fallback = directory
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("skill");
        let (name, description) = skill_metadata(&body, fallback);
        if !name.starts_with(SYSTEM_SKILL_PREFIX) && names.insert(name.clone()) {
            skills.push(Skill {
                name,
                description,
                path: skill_file,
            });
        }
        return Ok(());
    }
    let mut children = fs::read_dir(directory)
        .map_err(|error| {
            ProjectError::new(format!("could not list {}: {error}", directory.display()))
        })?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    children.sort_by_key(|entry| entry.file_name());
    for child in children {
        collect_skills(&child.path(), depth + 1, names, skills)?;
    }
    Ok(())
}

fn skill_metadata(body: &str, fallback: &str) -> (String, String) {
    let frontmatter = body
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(frontmatter, _)| frontmatter)
        .unwrap_or_default();
    let field = |name: &str| {
        frontmatter.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.trim() == name).then(|| value.trim().trim_matches(['\'', '"']).to_string())
        })
    };
    let name = field("name")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string());
    let description = field("description")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "Reusable instructions".to_string());
    (name, description)
}

fn absolute_directory(path: &Path) -> Result<PathBuf, ProjectError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .map_err(|error| {
                ProjectError::new(format!("could not read current directory: {error}"))
            })?
            .join(path)
    };
    if !path.is_dir() {
        return Err(ProjectError::new(format!(
            "workspace is not a directory: {}",
            path.display()
        )));
    }
    fs::canonicalize(&path)
        .map_err(|error| ProjectError::new(format!("could not open {}: {error}", path.display())))
}

fn absolutize(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

fn read_text(path: &Path) -> Result<String, ProjectError> {
    fs::read_to_string(path)
        .map_err(|error| ProjectError::new(format!("could not read {}: {error}", path.display())))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn user_home() -> Option<PathBuf> {
    env::var_os("USERPROFILE")
        .or_else(|| env::var_os("HOME"))
        .map(PathBuf::from)
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectError(String);

impl ProjectError {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ProjectError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ProjectError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_thin_agulater_launch_and_project_skill() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".agents/runtime")).unwrap();
        fs::create_dir_all(root.path().join(".agents/skills/review")).unwrap();
        fs::create_dir_all(root.path().join(".agents/skills/reserved")).unwrap();
        fs::write(root.path().join(".agents/AGENTS.md"), "Keep it small.").unwrap();
        fs::write(root.path().join("AGENTS.md"), "Compatibility shim only.").unwrap();
        fs::write(
            root.path().join(".agents/skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review changes\n---\nDo the review.\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".agents/skills/reserved/SKILL.md"),
            "---\nname: system/rg\ndescription: User replacement\n---\nOverride.\n",
        )
        .unwrap();
        fs::write(
            root.path().join(".agents/runtime/launch.json"),
            r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","skills":"../skills"}"#,
        )
        .unwrap();

        let mut project = Project::discover(root.path(), None).unwrap();
        project.host_tools = HostTools::from_names(&["rg", "fzf"]);
        assert_eq!(project.instructions.len(), 1);
        assert_eq!(project.instructions[0].1, "Keep it small.");
        assert!(project.skills.iter().any(|skill| skill.name == "review"));
        assert!(!project.skills.iter().any(|skill| skill.name == "system/rg"));
        assert!(
            project
                .skill("review")
                .unwrap()
                .activation()
                .unwrap()
                .contains("Do the review")
        );
        assert!(
            project
                .activate_references("use @skill:review")
                .unwrap()
                .contains("Do the review")
        );
        assert_eq!(
            project
                .activate_references("literal @@skill:review")
                .unwrap(),
            "literal @skill:review"
        );
        assert!(
            project
                .activate_references("use @skill:system/rg")
                .unwrap()
                .contains("rg PATTERN")
        );
        let prompt = project.system_prompt();
        assert!(prompt.contains("system/rg"));
        assert!(prompt.contains("system/fzf"));
    }

    #[test]
    fn launch_v2_rejects_legacy_and_runtime_specific_fields() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".agents/runtime")).unwrap();
        fs::write(root.path().join(".agents/AGENTS.md"), "Help.").unwrap();
        let launch = root.path().join(".agents/runtime/launch.json");

        fs::write(
            &launch,
            r#"{"format":"agul/launch/v1","instructions":"../AGENTS.md"}"#,
        )
        .unwrap();
        assert!(Project::discover(root.path(), None).is_err());

        fs::write(
            &launch,
            r#"{"format":"agul/launch/v2","instructions":"../AGENTS.md","specialists":"specialists.json"}"#,
        )
        .unwrap();
        assert!(Project::discover(root.path(), None).is_err());
    }

    #[test]
    fn project_and_user_prepared_skills_follow_the_frozen_precedence() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("projects/workspace");
        let home = root.path().join("home");
        let project_runtime = workspace.join(".agents/runtime");
        let project_prepared = project_runtime.join("prepared-skills");
        let project_raw = workspace.join(".agents/skills");
        let ancestor_raw = root.path().join("projects/.agents/skills");
        let user_runtime = home.join(".agents/runtime");
        let user_prepared = user_runtime.join("prepared-skills");
        let codex = home.join(".codex/skills");
        let claude = home.join(".claude/skills");
        let user_raw = home.join(".agents/skills");
        fs::create_dir_all(&project_runtime).unwrap();
        fs::create_dir_all(&user_runtime).unwrap();
        fs::write(
            project_runtime.join("launch.json"),
            r#"{"format":"agul/launch/v2","instructions":"instructions.md","skills":"prepared-skills"}"#,
        )
        .unwrap();
        fs::write(
            user_runtime.join("launch.json"),
            r#"{"format":"agul/launch/v2","instructions":"instructions.md","skills":"prepared-skills"}"#,
        )
        .unwrap();

        for root in [
            &project_prepared,
            &project_raw,
            &ancestor_raw,
            &user_prepared,
            &codex,
            &claude,
            &user_raw,
        ] {
            write_test_skill(root, "project-prepared-wins");
        }
        for root in [
            &project_raw,
            &ancestor_raw,
            &user_prepared,
            &codex,
            &claude,
            &user_raw,
        ] {
            write_test_skill(root, "project-raw-wins");
        }
        for root in [&ancestor_raw, &user_prepared, &codex, &claude, &user_raw] {
            write_test_skill(root, "ancestor-raw-wins");
        }
        for root in [&user_prepared, &codex, &claude, &user_raw] {
            write_test_skill(root, "user-prepared-wins");
        }
        for root in [&codex, &claude, &user_raw] {
            write_test_skill(root, "codex-wins");
        }
        for root in [&claude, &user_raw] {
            write_test_skill(root, "claude-wins");
        }
        write_test_skill(&user_raw, "user-raw-only");
        write_test_skill(&user_prepared, "user-prepared-only");

        let project_launch = read_launch(&project_runtime.join("launch.json")).unwrap();
        let user_launch = read_launch(&user_runtime.join("launch.json")).unwrap();
        let skills = discover_skills_from(
            &workspace,
            Some(&project_launch),
            Some(&user_launch),
            Some(&home),
        )
        .unwrap();
        let selected = |name: &str| {
            skills
                .iter()
                .find(|skill| skill.name == name)
                .map(|skill| skill.path.clone())
                .unwrap()
        };

        assert!(selected("project-prepared-wins").starts_with(&project_prepared));
        assert!(selected("project-raw-wins").starts_with(&project_raw));
        assert!(selected("ancestor-raw-wins").starts_with(&ancestor_raw));
        assert!(selected("user-prepared-wins").starts_with(&user_prepared));
        assert!(selected("codex-wins").starts_with(&codex));
        assert!(selected("claude-wins").starts_with(&claude));
        assert!(selected("user-raw-only").starts_with(&user_raw));
        assert!(selected("user-prepared-only").starts_with(&user_prepared));
    }

    fn write_test_skill(root: &Path, name: &str) {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: Test {name}\n---\n{name}\n"),
        )
        .unwrap();
    }
}
