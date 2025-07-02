use std::path::PathBuf;

use clap::{Parser, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "tagge_rs")]
#[command(about = "Semantic versioning and tagging CLI tool for Git repos", long_about = None)]
pub struct CliArgs {
    #[arg(
        value_enum,
        help = " by patch (e.g. v1.0.0 -> v1.0.1)\n by minor (e.g. v1.0.0 -> v1.1.0)\n by major (e.g. v1.0.0 -> v2.0.0)\n"
    )]
    pub bump: Option<VersionBump>,

    /// Use commit SHA hash in changelog
    #[arg(short = 's', long)]
    pub use_sha: bool,

    /// Use PR tags in changelog
    #[arg(short = 'r', long)]
    pub use_pr: bool,

    /// Dry run, only print the tag command, do not create a tag
    #[arg(short, long)]
    pub dry_run: bool,

    /// Path to the Git repository (default: current directory)
    #[arg(short, long, default_value_t = String::from("."))]
    pub path: String,

    /// Skip fetching git tags
    #[arg(long)]
    pub no_fetch: bool,

    /// Github token for fetching information about pull requests / commits
    #[arg(long)]
    pub gh_token: Option<String>,

    /// Add additional debug logging
    #[arg(long)]
    pub debug: bool,
}

impl CliArgs {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(&self.path)
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, ValueEnum)]
pub enum VersionBump {
    Patch,
    Minor,
    Major,
}

// /// Add a pre-release label (e.g. alpha, beta, rc)
// #[arg(long)]
// pub pre: Option<String>,

// /// Add build metadata (e.g. +001)
// #[arg(long)]
// pub metadata: Option<String>,

// /// Override the auto-calculated tag
// #[arg(long)]
// pub tag: Option<String>,

// /// Override the generated tag message
// #[arg(long)]
// pub message: Option<String>,

// /// Git revision or tag to start changelog from
// #[arg(long, default_value = "last-tag")]
// pub from: String,

// /// Include PRs in the changelog
// #[arg(long)]
// pub pr: bool,
