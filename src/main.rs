mod args;
mod version;

use crate::version::ToVString;
use args::CliArgs;
use args::VersionBump;
use clap::Parser;
use colored::Colorize;
use futures::future::join_all;
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
use octocrab::Octocrab;
use octocrab::models::pulls::PullRequest;
use semver::Version;
use std::fmt;
use std::fmt::Display;
use std::fmt::Write as FmtWrite;
use std::io;
use std::io::Write;
use std::path::Path;

#[tokio::main]
async fn main() -> MietteResult<()> {
    let mut cli_args = CliArgs::parse();

    if cli_args.debug {
        tracing_subscriber::fmt::init();
        tracing::info!("Running in debug mode!");
    }

    let repo = repository_from_path(&cli_args.path())?;
    let (repo_owner, repo_name) = github_owner_and_repo(&repo)?;

    // Check gh token if PR tags are requested
    let token = if cli_args.use_pr {
        let Some(token) = cli_args.gh_token.take().or_else(|| get_gh_token().ok()) else {
            println!(
                "{}",
                "❌ No GitHub token provided!
Please provide a GitHub token using the --gh-token option
or set the GH_TOKEN environment variable.

Example:
    export GH_TOKEN=your_token_here

See: https://github.com/settings/tokens for more info."
                    .red()
            );
            return Ok(());
        };
        Some(token)
    } else {
        None
    };

    // Check branch
    let head_ref = repo.head().into_diagnostic()?;
    if head_ref.is_branch() {
        if let Some(branch_name) = head_ref.shorthand() {
            // Notify user main/master is not selected
            if !["main", "master"].contains(&branch_name) {
                println!(
                    "{}",
                    format!("Note: You are on branch '{branch_name}', not 'main' or 'master'!\n")
                        .yellow()
                );
            }
            // No need to confirm if:
            if !cli_args.dry_run // dryrun
                && (cli_args.bump.is_some() || cli_args.tag.is_some()) // no bump
                && !confirm_continue("Are you sure you want to create a tag on this branch?")
            {
                return Ok(());
            }
        }
    }

    let mut git_fetch_task = None;
    if !cli_args.no_fetch {
        git_fetch_task = Some(tokio::task::spawn_blocking({
            let repo = repository_from_path(&cli_args.path())
                .expect("If we opened repo once without panic, we can do it again (hopefully)");
            move || git_fetch(&repo)
        }));
        tracing::info!("Git fetch future created!");

        // If there is no prs to be fetched await now
        if !cli_args.use_pr {
            git_fetch_task
                .as_mut()
                .unwrap()
                .await
                .expect("Failed to handle blocking thread!")?;
            tracing::info!("Git fetch future awaited!");
        }
    }

    let Some((latest_tag, latest_version)) = latest_tag(&repo) else {
        println!("No tags found! Please create the first tag manually!");
        return Ok(());
    };

    // Get commits between the tag and head
    let commits = commits_between_tag_and_head(&repo, &latest_tag)?;

    let prs = if let Some(token) = token
        && cli_args.use_pr
    {
        let commit_hashes = commits.iter().map(|c| c.id().to_string());

        let fetch_prs_task = fetch_prs(&token, &repo_owner, &repo_name, commit_hashes);
        tracing::info!("Fetch PRs future created!");
        if let Some(git_fetch) = git_fetch_task {
            let (prs_res, git_fetch_res) = tokio::join!(fetch_prs_task, git_fetch);
            git_fetch_res.unwrap()?;
            tracing::info!("Git fetch future awaited!");
            let res = Some(prs_res?);
            tracing::info!("Fetch PRs future awaited!");
            res
        } else {
            let res = Some(fetch_prs_task.await?);
            tracing::info!("Fetch PRs future awaited!");
            res
        }
    } else {
        None
    };

    // Make nice messages "<SHA:7> <commit summary>"
    let commit_msgs = commits.iter().map(|c| {
        let mut msg = String::new();
        let summary = c.summary().unwrap_or_default();

        // Write SHA if requested
        if cli_args.use_sha {
            write!(
                msg,
                "{} ",
                c.id().to_string().chars().take(7).collect::<String>()
            )
            .expect("Should never fail!");
        }

        write!(msg, "{summary}").expect("Should never fail");

        if let Some(prs) = &prs {
            if let Some(pr_num) = prs.iter().find_map(|(commit_sha, pr_num)| {
                if *commit_sha == c.id().to_string() {
                    Some(pr_num)
                } else {
                    None
                }
            }) {
                write!(msg, " (#{pr_num})").expect("Should never fail");
            } else {
                write!(msg, " (N/A)").expect("Should not fail");
            }
        }
        msg
    });

    // If we want to bump
    let (new_tag, new_version) = if let Some(overridden_tag) = cli_args.tag {
        if !cli_args.dry_run {
            let new_tag = create_tag(
                &repo,
                &overridden_tag,
                &generate_changelog(commit_msgs.clone()),
            )?;
            (Some(new_tag), Some(overridden_tag))
        } else {
            (None, Some(overridden_tag))
        }
    } else if let Some(bump) = cli_args.bump {
        let new_version = bump_version(&latest_version, &bump).to_v_string();
        let new_tag = if !cli_args.dry_run {
            Some(create_tag(
                &repo,
                &new_version,
                &generate_changelog(commit_msgs.clone()),
            )?)
        } else {
            None
        };
        (new_tag, Some(new_version))
    } else {
        (None, None)
    };

    print_info(
        &latest_tag,
        &latest_version.to_v_string(),
        new_tag.as_ref(),
        new_version.as_deref(),
        commit_msgs,
    );

    Ok(())
}

fn github_owner_and_repo(repo: &Repository) -> MietteResult<(String, String)> {
    let binding = repo.find_remote("origin").into_diagnostic()?;
    let url = binding
        .url()
        .ok_or_else(|| miette!("No url!"))?
        .trim_end_matches(".git");

    url.strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .and_then(|s| s.split_once('/'))
        .map(|(owner, repo)| (owner.to_string(), repo.to_string()))
        .ok_or_else(|| miette!("Failed to get repo owner and name"))
}

fn get_gh_token() -> MietteResult<String> {
    std::env::var("GH_TOKEN").into_diagnostic()
}

async fn fetch_prs(
    token: &str,
    owner: &str,
    repo_name: &str,
    commit_shas: impl Iterator<Item = String>,
) -> MietteResult<Vec<(String, u64)>> {
    let octocrab = Octocrab::builder()
        .personal_token(token)
        .build()
        .into_diagnostic()?;

    // Prepare all requests as futures
    let fetches = commit_shas.into_iter().map(|sha| {
        let octocrab = octocrab.clone();
        let owner = owner.to_string();
        let repo_name = repo_name.to_string();
        async move {
            octocrab
                .get::<Vec<PullRequest>, _, _>(
                    format!("/repos/{owner}/{repo_name}/commits/{sha}/pulls"),
                    None::<&()>,
                )
                .await
                .map(|pulls| {
                    pulls
                        .into_iter()
                        .map(|pr| (sha.clone(), pr.number))
                        .collect::<Vec<(String, u64)>>()
                })
                .unwrap_or_default()
        }
    });

    // Run all fetches concurrently
    let results = join_all(fetches).await;

    // Flatten all PR numbers into a single Vec
    let pr_numbers: Vec<(String, u64)> = results.into_iter().flatten().collect();
    Ok(pr_numbers)
}

fn make_ssh_callbacks<'a>() -> MietteResult<RemoteCallbacks<'a>> {
    let mut callbacks = RemoteCallbacks::new();
    callbacks.credentials({
        move |_url, username_from_url, _allowed_types| {
            let username = username_from_url.unwrap();
            tracing::info!("git username: {username}");
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
        .revparse_single(&format!("refs/tags/{tag_name}"))
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

fn create_tag<'a>(
    repo: &'a Repository,
    new_version: &str,
    changelog: &str,
) -> MietteResult<Tag<'a>> {
    // Get HEAD commit
    // let obj = repo
    //     .head()
    //     .into_diagnostic()?
    //     .peel(ObjectType::Commit)
    //     .into_diagnostic()?;
    // let commit = obj.as_commit().ok_or(miette!("HEAD is not a commit"))?;

    // Tagger signature (from git config or fallback)
    // let tagger = repo.signature().into_diagnostic()?;
    // let tag_name = format!("{new_version}");
    // Create the tag
    // let tag_oid = repo
    //     .tag(&tag_name, &obj, &tagger, "testiii", false)
    //     .into_diagnostic()?;

    // Return the tag object
    // let tag_obj = repo.find_tag(tag_oid).into_diagnostic()?;

    // git2-rs does not support signing tags yet!
    // https://github.com/rust-lang/git2-rs/issues/1039
    // Temporarily use process
    use std::process::Command;
    let status = Command::new("git")
        .args([
            "tag",
            "-a",
            new_version,
            "-s",
            "-m",
            &format!("Release {new_version}\n\n{changelog}"),
        ])
        .status()
        .into_diagnostic()?;

    if !status.success() {
        return Err(miette!("Failed to create the signed tag"));
    }

    let reference = repo
        .revparse_single(&format!("refs/tags/{new_version}"))
        .into_diagnostic()?;
    let tag_obj = reference.peel_to_tag().into_diagnostic()?;

    Ok(tag_obj)
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

fn generate_changelog(commit_msgs: impl Iterator<Item = String>) -> String {
    let mut change_log = String::new();
    let mut first = true;
    for msg in commit_msgs {
        if first {
            write!(&mut change_log, "Changelog:").expect("Should never panic!");
            first = false;
        }
        write!(&mut change_log, "\n - {msg}").expect("Should never panic!");
    }
    change_log
}

enum MsgType {
    New,
    Latest,
}

impl Display for MsgType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Latest => "Latest",
            Self::New => "New",
        };
        write!(f, "{s}",)
    }
}

fn generate_tag_msg(msg_type: MsgType, tag: &Tag, version: &str) -> String {
    let mut msg = String::new();

    writeln!(msg, "{msg_type} tag:\n  SHA: {}", tag.id()).expect("Should never fail");
    writeln!(msg, "  Version: {version}").expect("Should never fail");
    msg
}

fn print_changelog(commit_msgs: impl Iterator<Item = String>) {
    let changelog = generate_changelog(commit_msgs);
    if !changelog.is_empty() {
        println!("Commits in the new tag:");
        println!("\n{changelog}",);
    } else {
        println!("No new commits since the latest tag.")
    }
}

fn print_info(
    latest_tag: &Tag,
    latest_version: &str,
    new_tag: Option<&Tag>,
    new_version: Option<&str>,
    commit_msgs: impl Iterator<Item = String>,
) {
    let latest_tag = generate_tag_msg(MsgType::Latest, latest_tag, latest_version);
    println!("{latest_tag}");

    if let Some(new_version) = new_version {
        if let Some(new_tag) = new_tag {
            let new_tag = generate_tag_msg(MsgType::New, new_tag, new_version);
            println!("{new_tag}");
            print_changelog(commit_msgs);
        } else {
            println!("New version: {new_version}\n");
            println!("Command: \ngit tag -a {new_version} -s -m \"Release {new_version}\n");
            print!("Changelog:");
            for msg in commit_msgs {
                print!("\n- {msg}");
            }
            println!("\"")
        }
    } else {
        print_changelog(commit_msgs);
    }
}
