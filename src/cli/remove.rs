use std::path::PathBuf;

use clap::Parser;
use rattler_conda_types::{PackageName, Platform};

use crate::{project::SpecType, Project};

/// Remove the depedency from the project
#[derive(Debug, Default, Parser)]
pub struct Args {
    /// List of dependencies you wish to remove from the project
    #[arg(required = true)]
    pub deps: Vec<PackageName>,

    /// The path to 'pixi.toml'
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,

    /// Whether dependency is a host dependency
    #[arg(long, conflicts_with = "build")]
    pub host: bool,

    /// Whether dependency is a build dependency
    #[arg(long, conflicts_with = "host")]
    pub build: bool,

    /// The platform for which the dependency should be removed
    #[arg(long, short)]
    pub platform: Option<Platform>,
}

pub fn execute(args: Args) -> miette::Result<()> {
    let mut project = Project::load_or_else_discover(args.manifest_path.as_deref())?;
    let deps = args.deps;
    let spec_type = if args.host {
        SpecType::Host
    } else if args.build {
        SpecType::Build
    } else {
        SpecType::Run
    };

    let results = deps
        .iter()
        .map(|dep| {
            if let Some(p) = &args.platform {
                project.remove_target_dependency(dep, &spec_type, p)
            } else {
                project.remove_dependency(dep, &spec_type)
            }
        })
        .collect::<Vec<_>>();

    let _ = results
        .iter()
        .filter(|&result| result.is_ok())
        .map(|result| {
            if let Ok((removed, spec)) = result {
                eprintln!("Removed {} {}", removed, spec);
            }
        })
        .collect::<Vec<_>>();

    match spec_type {
        SpecType::Build => eprintln!("Removed these as build dependencies."),
        SpecType::Host => eprintln!("Removed these as host dependencies."),
        _ => (),
    };

    if let Some(p) = &args.platform {
        eprintln!(
            "Removed these only for platform: {}",
            console::style(p.as_str()).bold()
        )
    }

    let _ = results
        .iter()
        .filter(|&result| result.is_err())
        .map(|result| {
            if let Err(e) = result {
                eprintln!("{e}");
            }
        })
        .collect::<Vec<_>>();

    Ok(())
}
