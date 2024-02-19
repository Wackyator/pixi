use crate::cli::add::{add_conda_specs_to_project, add_pypi_specs_to_project};
use crate::project::manifest::PyPiRequirement;
use crate::Project;
use crate::{config::get_default_author, consts};
use clap::Parser;
use itertools::Itertools;
use miette::IntoDiagnostic;
use minijinja::{context, Environment};
use rattler_conda_types::{Channel, ChannelConfig, MatchSpec, Platform};
use regex::Regex;
use rip::types::PackageName;
use serde::Deserialize;
use std::fs::File;
use std::io::{Error, ErrorKind, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::{fs, path::PathBuf};

/// Creates a new project
#[derive(Parser, Debug)]
pub struct Args {
    /// Where to place the project (defaults to current path)
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Channels to use in the project.
    #[arg(short, long = "channel", id = "channel", conflicts_with = "env_file")]
    pub channels: Option<Vec<String>>,

    /// Platforms that the project supports.
    #[arg(short, long = "platform", id = "platform")]
    pub platforms: Vec<String>,

    /// Environment.yml file to bootstrap the project.
    #[arg(short = 'i', long = "import")]
    pub env_file: Option<PathBuf>,
}

#[derive(Deserialize, Debug)]
pub struct CondaEnvFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    channels: Vec<String>,
    dependencies: Vec<CondaEnvDep>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
pub enum CondaEnvDep {
    Conda(String),
    Pip { pip: Vec<String> },
}

/// The default channels to use for a new project.
const DEFAULT_CHANNELS: &[&str] = &["conda-forge"];

/// The pixi.toml template
///
/// This uses a template just to simplify the flexibility of emitting it.
const PROJECT_TEMPLATE: &str = r#"[project]
name = "{{ name }}"
version = "{{ version }}"
description = "Add a short description here"
{%- if author %}
authors = ["{{ author[0] }} <{{ author[1] }}>"]
{%- endif %}
channels = [{%- if channels %}"{{ channels|join("\", \"") }}"{%- endif %}]
platforms = ["{{ platforms|join("\", \"") }}"]

[tasks]

[dependencies]

"#;

const GITIGNORE_TEMPLATE: &str = r#"# pixi environments
.pixi

"#;

const GITATTRIBUTES_TEMPLATE: &str = r#"# GitHub syntax highlighting
pixi.lock linguist-language=YAML

"#;

pub async fn execute(args: Args) -> miette::Result<()> {
    let env = Environment::new();
    let dir = get_dir(args.path).into_diagnostic()?;
    let manifest_path = dir.join(consts::PROJECT_MANIFEST);
    let gitignore_path = dir.join(".gitignore");
    let gitattributes_path = dir.join(".gitattributes");

    // Check if the project file doesn't already exist. We don't want to overwrite it.
    if fs::metadata(&manifest_path).map_or(false, |x| x.is_file()) {
        miette::bail!("{} already exists", consts::PROJECT_MANIFEST);
    }

    // Fail silently if it already exists or cannot be created.
    fs::create_dir_all(&dir).ok();

    let (name, channels, conda_deps, pip_deps) = if let Some(env_file) = args.env_file {
        let env_info = read_env_yml(env_file)?;
        let name = env_info.name;
        let channels = parse_channels(env_info.channels);
        let (conda_deps, pip_deps, mut extra_channels) = parse_dependencies(env_info.dependencies)?;
        extra_channels.extend(
            channels
                .into_iter()
                .map(|c| Arc::new(Channel::from_str(c, &ChannelConfig::default()).unwrap())),
        );
        let mut channels: Vec<_> = extra_channels
            .into_iter()
            .unique()
            .map(|c| c.name().to_string())
            .collect();
        if channels.is_empty() {
            channels = DEFAULT_CHANNELS
                .iter()
                .copied()
                .map(ToOwned::to_owned)
                .collect()
        }
        (name, channels, conda_deps, pip_deps)
    } else {
        let name = dir
            .file_name()
            .ok_or_else(|| {
                miette::miette!(
                    "Cannot get file or directory name from the path: {}",
                    dir.to_string_lossy()
                )
            })?
            .to_string_lossy()
            .to_string();

        let channels = if let Some(channels) = args.channels {
            channels
        } else {
            DEFAULT_CHANNELS
                .iter()
                .copied()
                .map(ToOwned::to_owned)
                .collect()
        };

        (name, channels, vec![], vec![])
    };

    let version = "0.1.0";
    let author = get_default_author();
    let platforms = if args.platforms.is_empty() {
        vec![Platform::current().to_string()]
    } else {
        args.platforms
    };

    let rv = env
        .render_named_str(
            consts::PROJECT_MANIFEST,
            PROJECT_TEMPLATE,
            context! {
                name,
                version,
                author,
                channels,
                platforms
            },
        )
        .unwrap();

    if conda_deps.is_empty() && pip_deps.is_empty() {
        fs::write(&manifest_path, rv).into_diagnostic()?;
    } else {
        let mut project = Project::from_str(&dir, &rv)?;

        add_conda_specs_to_project(
            &mut project,
            conda_deps,
            crate::SpecType::Run,
            true,
            true,
            &vec![],
        )
        .await?;

        add_pypi_specs_to_project(&mut project, pip_deps, &vec![], true, true).await?;

        project.save()?;
    }

    // create a .gitignore if one is missing
    if let Err(e) = create_or_append_file(&gitignore_path, GITIGNORE_TEMPLATE) {
        tracing::warn!(
            "Warning, couldn't update '{}' because of: {}",
            gitignore_path.to_string_lossy(),
            e
        );
    }

    // create a .gitattributes if one is missing
    if let Err(e) = create_or_append_file(&gitattributes_path, GITATTRIBUTES_TEMPLATE) {
        tracing::warn!(
            "Warning, couldn't update '{}' because of: {}",
            gitattributes_path.to_string_lossy(),
            e
        );
    }

    // Emit success
    eprintln!(
        "{}Initialized project in {}",
        console::style(console::Emoji("✔ ", "")).green(),
        dir.display()
    );

    Ok(())
}

// When the specific template is not in the file or the file does not exist.
// Make the file and append the template to the file.
fn create_or_append_file(path: &Path, template: &str) -> std::io::Result<()> {
    let file = fs::read_to_string(path).unwrap_or_default();

    if !file.contains(template) {
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?
            .write_all(template.as_bytes())?;
    }
    Ok(())
}

fn get_dir(path: PathBuf) -> Result<PathBuf, Error> {
    if path.components().count() == 1 {
        Ok(std::env::current_dir().unwrap_or_default().join(path))
    } else {
        path.canonicalize().map_err(|e| match e.kind() {
            ErrorKind::NotFound => Error::new(
                ErrorKind::NotFound,
                format!(
                    "Cannot find '{}' please make sure the folder is reachable",
                    path.to_string_lossy()
                ),
            ),
            _ => Error::new(
                ErrorKind::InvalidInput,
                "Cannot canonicalize the given path",
            ),
        })
    }
}

type PipReq = (PackageName, PyPiRequirement);
type ParsedDependencies = (Vec<MatchSpec>, Vec<PipReq>, Vec<Arc<Channel>>);
fn parse_dependencies(deps: Vec<CondaEnvDep>) -> miette::Result<ParsedDependencies> {
    let mut conda_deps = vec![];
    let mut pip_deps = vec![];
    let mut picked_up_channels = vec![];
    for dep in deps {
        match dep {
            CondaEnvDep::Conda(d) => {
                let match_spec = MatchSpec::from_str(&d).into_diagnostic()?;
                if let Some(channel) = match_spec.clone().channel {
                    picked_up_channels.push(channel);
                }
                conda_deps.push(match_spec);
            }
            CondaEnvDep::Pip { pip } => pip_deps.extend(
                pip.into_iter()
                    .map(|mut dep| {
                        let re = Regex::new(r"/([^/]+)\.git").unwrap();
                        if let Some(caps) = re.captures(dep.as_str()) {
                            dep = caps.get(1).unwrap().as_str().to_string();
                        }
                        let req = pep508_rs::Requirement::from_str(&dep).into_diagnostic()?;
                        let name = rip::types::PackageName::from_str(req.name.as_str())?;
                        let requirement = PyPiRequirement::from(req);
                        Ok((name, requirement))
                    })
                    .collect::<miette::Result<Vec<_>>>()?,
            ),
        }
    }

    if !pip_deps.is_empty() {
        conda_deps.push(MatchSpec::from_str("pip").into_diagnostic()?);
    }

    Ok((conda_deps, pip_deps, picked_up_channels))
}

fn parse_channels(channels: Vec<String>) -> Vec<String> {
    let mut new_channels = vec![];
    for channel in channels {
        if channel == "defaults" {
            // https://docs.anaconda.com/free/working-with-conda/reference/default-repositories/#active-default-channels
            new_channels.push("main".to_string());
            new_channels.push("r".to_string());
            new_channels.push("msys2".to_string());
        } else {
            let channel = channel.trim();
            if !channel.is_empty() {
                new_channels.push(channel.to_string());
            }
        }
    }
    new_channels
}

fn read_env_yml(path: PathBuf) -> miette::Result<CondaEnvFile> {
    serde_yaml::from_reader(File::open(path).into_diagnostic()?).into_diagnostic()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::init::get_dir;
    use itertools::Itertools;
    use rattler_conda_types::ChannelConfig;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    #[test]
    fn test_parse_conda_env_file() {
        let example_conda_env_file = r#"
        name: pixi_example_project
        channels:
          - conda-forge
        dependencies:
          - python
          - pytorch::torchvision
          - conda-forge::pytest
          - wheel=0.31.1
          - pip:
            - requests
            - git+https://git@github.com/fsschneider/DeepOBS.git@develop#egg=deepobs
            - torch==1.8.1
        "#;
        let conda_env_file_data: CondaEnvFile =
            serde_yaml::from_str(example_conda_env_file).unwrap();

        assert_eq!(conda_env_file_data.name, "pixi_example_project");
        assert_eq!(
            conda_env_file_data.channels,
            vec!["conda-forge".to_string()]
        );

        let (conda_deps, pip_deps, mut channels) =
            parse_dependencies(conda_env_file_data.dependencies).unwrap();

        channels.extend(
            conda_env_file_data
                .channels
                .into_iter()
                .map(|c| Arc::new(Channel::from_str(c, &ChannelConfig::default()).unwrap())),
        );
        let channels = channels.into_iter().unique().collect::<Vec<_>>();

        assert_eq!(
            channels,
            vec![
                Arc::new(Channel::from_str("pytorch", &ChannelConfig::default()).unwrap()),
                Arc::new(Channel::from_str("conda-forge", &ChannelConfig::default()).unwrap())
            ],
        );

        println!("{conda_deps:?}");
        assert_eq!(
            conda_deps,
            vec![
                MatchSpec::from_str("python").unwrap(),
                MatchSpec::from_str("pytorch::torchvision").unwrap(),
                MatchSpec::from_str("conda-forge::pytest").unwrap(),
                MatchSpec::from_str("wheel=0.31.1").unwrap(),
                MatchSpec::from_str("pip").unwrap(),
            ]
        );

        assert_eq!(
            pip_deps,
            vec![
                (
                    PackageName::from_str("requests").unwrap(),
                    PyPiRequirement {
                        version: None,
                        extras: None,
                        index: None,
                    }
                ),
                (
                    PackageName::from_str("DeepOBS").unwrap(),
                    PyPiRequirement {
                        version: None,
                        extras: None,
                        index: None,
                    },
                ),
                (
                    PackageName::from_str("torch").unwrap(),
                    PyPiRequirement {
                        version: pep440_rs::VersionSpecifiers::from_str("==1.8.1").ok(),
                        extras: None,
                        index: None,
                    }
                ),
            ]
        );
    }

    #[test]
    fn test_get_name() {
        assert_eq!(
            get_dir(PathBuf::from(".")).unwrap(),
            std::env::current_dir().unwrap()
        );
        assert_eq!(
            get_dir(PathBuf::from("test_folder")).unwrap(),
            std::env::current_dir().unwrap().join("test_folder")
        );
        assert_eq!(
            get_dir(std::env::current_dir().unwrap()).unwrap(),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
    }

    #[test]
    fn test_get_name_panic() {
        match get_dir(PathBuf::from("invalid/path")) {
            Ok(_) => panic!("Expected error, but got OK"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::NotFound),
        }
    }

    #[test]
    fn test_create_or_append_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("test_file.txt");
        let template = "Test Template";

        fn read_file_content(path: &Path) -> String {
            let mut file = std::fs::File::open(path).unwrap();
            let mut content = String::new();
            file.read_to_string(&mut content).unwrap();
            content
        }

        // Scenario 1: File does not exist.
        create_or_append_file(&file_path, template).unwrap();
        assert_eq!(read_file_content(&file_path), template);

        // Scenario 2: File exists but doesn't contain the template.
        create_or_append_file(&file_path, "New Content").unwrap();
        assert!(read_file_content(&file_path).contains(template));
        assert!(read_file_content(&file_path).contains("New Content"));

        // Scenario 3: File exists and already contains the template.
        let original_content = read_file_content(&file_path);
        create_or_append_file(&file_path, template).unwrap();
        assert_eq!(read_file_content(&file_path), original_content);

        // Scenario 4: Path is a folder not a file, give an error.
        assert!(create_or_append_file(dir.path(), template).is_err());

        dir.close().unwrap();
    }

    #[test]
    fn test_import_from_env_yamls() {
        let test_files_path = Path::new(&env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("environment_yamls");

        let entries = match fs::read_dir(test_files_path) {
            Ok(entries) => entries,
            Err(e) => panic!("Failed to read directory: {}", e),
        };

        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry.expect("Failed to read directory entry");
            paths.push(entry.path());
        }

        for path in paths {
            let env_info = read_env_yml(path.clone()).unwrap();
            // Try `cargo insta test` to run all at once
            let snapshot_name = format!(
                "test_import_from_env_yaml.{}",
                path.file_name().unwrap().to_string_lossy()
            );

            insta::assert_debug_snapshot!(
                snapshot_name,
                (
                    parse_dependencies(env_info.dependencies).unwrap(),
                    parse_channels(env_info.channels),
                    env_info.name
                )
            );
        }
    }
}
