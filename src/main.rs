mod args;

use args::CliArgs;
use clap::Parser;
use git2::Cred;
use git2::FetchOptions;
use git2::RemoteCallbacks;
use git2::Repository;
use git2::Tag;
use miette::Context;
use miette::IntoDiagnostic;
use miette::Result as MietteResult;
use miette::miette;
use semver::Version;
use std::path::Path;

fn main() -> MietteResult<()> {
    let cli_args = CliArgs::parse();

    if cli_args.debug {
        tracing_subscriber::fmt::init();
        tracing::info!("Running in debug mode!");
    }

    let repo = get_repository(&cli_args.path())?;

    if !cli_args.no_fetch {
        println!("Performing git fetch to get latest tags!");
        let mut origin = repo
            .find_remote("origin")
            .into_diagnostic()
            .wrap_err("Could not find git remote origin!")?;

        // Prepare callback authentication.
        let callbacks = make_ssh_callbacks()?;

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        // Fetch tags
        origin
            .fetch(
                &[
                    "refs/tags/*:refs/tags/*",
                    "refs/heads/*:refs/remotes/origin/*",
                ],
                Some(&mut fetch_options),
                None,
            )
            .into_diagnostic()?;
    }

    let (latest_tag, latest_version) = get_latest_tag(&repo).ok_or(miette!("No tags found!"))?;

    println!("Latest tag:\n  SHA: {}", latest_tag.id());
    println!("  Version: {}", latest_version);

    Ok(())
}

fn make_ssh_callbacks<'a>() -> MietteResult<RemoteCallbacks<'a>> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials({
        move |_url, username_from_url, _allowed_types| {
            let username = username_from_url.unwrap();
            Cred::ssh_key_from_agent(username)
        }
    });

    Ok(callbacks)
}

fn get_repository(path: &Path) -> MietteResult<Repository> {
    match Repository::open(path) {
        Ok(repo) => Ok(repo),
        Err(err) => {
            tracing::info!("Failed to get repo {}: {}", path.display(), err);
            Err(miette!(
                help = "Please check the path.",
                "Repository not found in: {}",
                path.display()
            ))
        }
    }
}

fn get_latest_tag(repo: &Repository) -> Option<(Tag, Version)> {
    let tag_names = repo.tag_names(None).ok()?;
    let mut latest: Option<(Version, &str)> = None;

    for tag_name in tag_names.iter().flatten() {
        let tag_str = tag_name.trim_start_matches('v');
        if let Ok(ver) = Version::parse(tag_str) {
            match &latest {
                Some((latest_ver, _)) if &ver <= latest_ver => {}
                _ => latest = Some((ver, tag_name)),
            }
        }
    }

    let (version, tag_name) = latest?;
    // Find the Tag object by name
    let reference = repo
        .revparse_single(&format!("refs/tags/{}", tag_name))
        .ok()?;
    let tag = reference.peel_to_tag().ok()?; // Only annotated tags, not lightweight

    Some((tag, version))
}
