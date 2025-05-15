mod args;

use args::CliArgs;
use args::VersionBump;
use clap::Parser;
use colored::Colorize;
use git2::Commit;
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
use std::io;
use std::io::Write;
use std::path::Path;

fn main() -> MietteResult<()> {
    let cli_args = CliArgs::parse();

    if cli_args.debug {
        tracing_subscriber::fmt::init();
        tracing::info!("Running in debug mode!");
    }

    let repo = repository_from_path(&cli_args.path())?;

    // Check branch
    let head_ref = repo.head().into_diagnostic()?;
    if head_ref.is_branch() {
        if let Some(branch_name) = head_ref.shorthand() {
            // Notify user main/master is not selected
            if !["main", "master"].contains(&branch_name) {
                println!(
                    "{}",
                    format!(
                        "Note: You are on branch '{}', not 'main' or 'master'!\n",
                        branch_name
                    )
                    .yellow()
                );
            }
            // No need to confirm if:
            if !cli_args.dry_run // dryrun
                && cli_args.bump.is_some() // no bump
                && !confirm_continue("Are you sure you want to create a tag on this branch?")
            {
                return Ok(());
            }
        }
    }

    if !cli_args.no_fetch {
        git_fetch(&repo)?;
    }

    let Some((latest_tag, latest_version)) = latest_tag(&repo) else {
        println!("No tags found! Please create the first tag manually!");
        return Ok(());
    };

    // If we want to bump
    let new_version = cli_args
        .bump
        .as_ref()
        .map(|bump| bump_version(&latest_version, bump));

    // Get commits between the tag and head
    let commits = commits_between_tag_and_head(&repo, &latest_tag)?;

    // Make nice messages "<SHA:7> <commit summary>" TODO PR
    let commit_msgs = commits.iter().map(|c| {
        let summary = c.summary().unwrap_or_default();
        format!(
            "{} {}",
            c.id().to_string().chars().take(7).collect::<String>(),
            summary
        )
    });

    let head_ref = repo.head().into_diagnostic()?;
    let mut in_master = true;
    if head_ref.is_branch() {
        if let Some(branch_name) = head_ref.shorthand() {
            if !["main", "master"].contains(&branch_name) {
                println!(
                    "{}",
                    format!(
                        "Note: You are on branch '{}', not 'main' or 'master'!\n",
                        branch_name
                    )
                    .yellow()
                );
                in_master = false;
            }
        }
    }

    println!("Latest tag:\n  SHA: {}", latest_tag.id());
    println!("  Version: v{}\n", latest_version);

    if let Some(new_version) = new_version {
        if in_master {
            println!("New version: v{}\n", new_version);
            println!("Command: \ngit tag -a v{new_version} -s -m \"Release v{new_version}\n");
            print!("Changelog:");
            for msg in commit_msgs {
                print!("\n- {}", msg);
            }
            println!("\"")
        }
    } else {
        println!("Commits:");
        for msg in commit_msgs {
            println!("  {}", msg);
        }
    }

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

fn repository_from_path(path: &Path) -> MietteResult<Repository> {
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

fn git_fetch(repo: &Repository) -> MietteResult<()> {
    tracing::info!("Performing git fetch to get latest tags!");
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
    Ok(())
}

fn latest_tag(repo: &Repository) -> Option<(Tag, Version)> {
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
    tracing::info!("Found tag name: {}", tag_name);
    // Find the Tag object by name
    let reference = repo
        .revparse_single(&format!("refs/tags/{}", tag_name))
        .ok()?;
    let tag = reference.peel_to_tag().ok()?; // annotated tags only (git tag -a)
    Some((tag, version))
}

fn bump_version(latest_version: &Version, bump: &VersionBump) -> Version {
    let mut new_version = latest_version.clone();
    match bump {
        VersionBump::Major => {
            new_version.major += 1;
            new_version.minor = 0;
            new_version.patch = 0;
        }
        VersionBump::Minor => {
            new_version.minor += 1;
            new_version.patch = 0;
        }
        VersionBump::Patch => {
            new_version.patch += 1;
        }
    }
    new_version
}

fn commits_between_tag_and_head<'a>(
    repo: &'a Repository,
    tag: &Tag,
) -> MietteResult<Vec<Commit<'a>>> {
    let tag_commit = tag.target_id();
    let head = repo
        .head()
        .ok()
        .and_then(|h| h.target())
        .ok_or(miette!("Failed to get HEAD!"))?;

    let mut revwalk = repo.revwalk().into_diagnostic()?;
    revwalk.push(head).into_diagnostic()?;
    revwalk.hide(tag_commit).into_diagnostic()?;

    let mut commits = Vec::new();
    for oid_result in revwalk {
        let Ok(oid) = oid_result else { continue };
        if let Ok(commit) = repo.find_commit(oid) {
            commits.push(commit);
        }
    }
    Ok(commits)
}

fn confirm_continue(question: &str) -> bool {
    let mut input = String::with_capacity(5);
    loop {
        print!("{question} (y/N): ");
        io::stdout().flush().expect("Failed to flush stdout!");
        input.clear();
        io::stdin()
            .read_line(&mut input)
            .expect("Unable to read Stdin");
        let answer = input.trim().to_lowercase();
        if answer.is_empty() || ["n", "no"].contains(&answer.as_str()) {
            println!("Aborted!");
            return false;
        }
        if ["y", "yes"].contains(&answer.as_str()) {
            println!();
            return true;
        }
        input.clear();
    }
}


