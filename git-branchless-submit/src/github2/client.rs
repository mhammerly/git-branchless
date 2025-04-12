//! Wrapper around GitHub's `gh` command line tool.

use eyre::Context;
use lib::core::effects::{Effects, OperationType};
use lib::git::SerializedNonZeroOid;
use lib::try_exit_code;
use lib::util::ExitCode;
use lib::util::EyreExitOr;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use tempfile::NamedTempFile;
use tracing::{debug, instrument, warn};

/// Executable name for the GitHub CLI.
pub const GH_EXE: &str = "gh";

#[allow(missing_docs)]
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PullRequestInfo {
    #[serde(rename = "number")]
    pub number: usize,
    #[serde(rename = "url")]
    pub url: String,
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "headRefOid")]
    pub head_ref_oid: SerializedNonZeroOid,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: String,
    #[serde(rename = "closed")]
    pub closed: bool,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "title")]
    pub title: String,
    #[serde(rename = "body")]
    pub body: String,
}

/// Interface for interacting with GitHub.
pub trait GitHubClient: Debug {
    /// Fetch a list of pull requests for the current repository. If `head_ref_prefix` is provided,
    /// the PR list will be filtered to PRs whose head refs match the pattern. Returns a mapping
    /// from a remote branch name to pull request information.
    fn query_repo_pull_requests(
        &self,
        effects: &Effects,
        head_ref_prefix: Option<&str>,
    ) -> EyreExitOr<HashMap<String, Vec<PullRequestInfo>>>;

    /// Create a GitHub issue with the given title and body. Returns the issue's URL.
    ///
    /// GitHub issues and pull requests draw from the same pool of IDs, and
    /// issues can be converted into pull requests. We can create an issue,
    /// use the ID to create a short branch name for a change, and then
    /// convert the issue to a PR with the same ID.
    fn create_issue(&self, effects: &Effects, title: &str, body: &str) -> EyreExitOr<String>;

    /// Convert the given issue to a GitHub pull request using the given `HEAD`
    /// and base ref. Returns the PR's URL.
    fn create_pull_request_from_issue(
        &self,
        effects: &Effects,
        issue_num: usize,
        head_ref_name: &str,
        base_ref_name: &str,
        draft: bool,
    ) -> EyreExitOr<String>;

    /// Update the specified pull request's title, body, and base ref.
    fn update_pull_request(
        &self,
        effects: &Effects,
        pr_num: usize,
        base_ref_name: &str,
        title: &str,
        body: &str,
    ) -> EyreExitOr<()>;
}

/// Regular implementation of [`GitHubClient`]. Wraps the `gh` command line
/// tool.
#[derive(Debug)]
pub struct RealGitHubClient {}
impl RealGitHubClient {
    fn query_current_repo_slug(&self, effects: &Effects) -> EyreExitOr<String> {
        let args = [
            "repo",
            "view",
            "--json",
            "owner,name",
            "--jq",
            ".owner.login + \"/\" + .name",
        ];
        self.run_gh_utf8(effects, &args)
    }

    #[instrument]
    fn write_body_file(&self, body: &str) -> eyre::Result<NamedTempFile> {
        use std::io::Write;
        let mut body_file = NamedTempFile::new()?;
        body_file.write_all(body.as_bytes())?;
        body_file.flush()?;
        Ok(body_file)
    }

    #[instrument]
    fn run_gh_utf8(&self, effects: &Effects, args: &[&str]) -> EyreExitOr<String> {
        let stdout = try_exit_code!(self.run_gh_raw(effects, args)?);
        let result = match std::str::from_utf8(&stdout) {
            Ok(result) => Ok(result.trim().to_owned()),
            Err(err) => {
                writeln!(
                    effects.get_output_stream(),
                    "Could not parse output from `gh` as UTF-8: {err}",
                )?;
                Err(ExitCode(1))
            }
        };
        Ok(result)
    }

    #[instrument]
    fn run_gh_raw(&self, effects: &Effects, args: &[&str]) -> EyreExitOr<Vec<u8>> {
        let exe_invocation = format!("{} {}", GH_EXE, args.join(" "));
        debug!(?exe_invocation, "Invoking gh");
        let (effects, _progress) =
            effects.start_operation(OperationType::RunTests(Arc::new(exe_invocation.clone())));

        let child = Command::new(GH_EXE)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Invoking `gh` command-line executable")?;
        let output = child
            .wait_with_output()
            .context("Waiting for `gh` invocation")?;
        if !output.status.success() {
            writeln!(
                effects.get_output_stream(),
                "Call to `{exe_invocation}` failed",
            )?;
            writeln!(effects.get_output_stream(), "Stdout:")?;
            writeln!(
                effects.get_output_stream(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            )?;
            writeln!(effects.get_output_stream(), "Stderr:")?;
            writeln!(
                effects.get_output_stream(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            )?;
            return Ok(Err(ExitCode::try_from(output.status)?));
        }
        Ok(Ok(output.stdout))
    }
}

impl GitHubClient for RealGitHubClient {
    fn query_repo_pull_requests(
        &self,
        effects: &Effects,
        head_ref_prefix: Option<&str>,
    ) -> EyreExitOr<HashMap<String, Vec<PullRequestInfo>>> {
        let args = [
            "pr",
            "list",
            "--author",
            "@me",
            "--json",
            "number,url,headRefName,headRefOid,baseRefName,closed,isDraft,title,body",
        ];
        let raw_result = try_exit_code!(self.run_gh_raw(effects, &args)?);
        let pull_request_infos: Vec<PullRequestInfo> =
            serde_json::from_slice(&raw_result).wrap_err("Deserializing output from gh pr list")?;
        let pull_request_infos: HashMap<String, Vec<PullRequestInfo>> = pull_request_infos
            .into_iter()
            .fold(HashMap::new(), |mut acc, pr| {
                let head_ref_name = pr.head_ref_name.clone();
                if head_ref_prefix.map_or(true, |prefix| head_ref_name.starts_with(prefix)) {
                    acc.entry(pr.head_ref_name.clone()).or_default().push(pr);
                }
                acc
            });

        Ok(Ok(pull_request_infos))
    }

    fn create_issue(&self, effects: &Effects, title: &str, body: &str) -> EyreExitOr<String> {
        let body_file = self.write_body_file(body)?;
        let args = [
            "issue",
            "create",
            "--title",
            title,
            "--body-file",
            body_file.path().to_str().unwrap(),
        ];
        self.run_gh_utf8(effects, &args)
    }

    fn create_pull_request_from_issue(
        &self,
        effects: &Effects,
        issue_num: usize,
        head_ref_name: &str,
        base_ref_name: &str,
        draft: bool,
    ) -> EyreExitOr<String> {
        // See relevant GitHub REST API docs:
        // https://docs.github.com/en/rest/pulls/pulls?apiVersion=2022-11-28#create-a-pull-request
        let slug = try_exit_code!(self.query_current_repo_slug(effects)?);
        let endpoint = format!("/repos/{}/pulls", slug);
        let issue_field = format!("issue={}", issue_num);
        let head_field = format!("head={}", head_ref_name);
        let base_field = format!("base={}", base_ref_name);
        let draft_field = format!("draft={}", draft);
        let args = [
            "api",
            &endpoint,
            "-X",
            "POST",
            "-F",
            &issue_field,
            "-F",
            &head_field,
            "-F",
            &base_field,
            "-F",
            &draft_field,
            "-q",
            ".html_url", // jq syntax to get the url out of the response
        ];
        self.run_gh_utf8(effects, &args)
    }

    fn update_pull_request(
        &self,
        effects: &Effects,
        pr_num: usize,
        base_ref_name: &str,
        title: &str,
        body: &str,
    ) -> EyreExitOr<()> {
        let body_file = self.write_body_file(body)?;
        let args = [
            "pr",
            "edit",
            &pr_num.to_string(),
            "--base",
            &base_ref_name,
            "--title",
            &title,
            "--body-file",
            body_file.path().to_str().unwrap(),
        ];
        try_exit_code!(self.run_gh_raw(effects, &args)?);
        Ok(Ok(()))
    }
}

/// A mock client representing the remote Github repository and server.
#[derive(Debug)]
pub struct MockGitHubClient {
    /// The path to the remote repository on disk.
    pub remote_repo_path: PathBuf,
}
impl GitHubClient for MockGitHubClient {
    fn query_repo_pull_requests(
        &self,
        _effects: &Effects,
        _head_ref_prefix: Option<&str>,
    ) -> EyreExitOr<HashMap<String, Vec<PullRequestInfo>>> {
        Ok(Ok(HashMap::new()))
    }

    fn create_issue(&self, _effects: &Effects, _title: &str, _body: &str) -> EyreExitOr<String> {
        Ok(Ok("".to_string()))
    }

    fn create_pull_request_from_issue(
        &self,
        _effects: &Effects,
        _issue_num: usize,
        _head_ref_name: &str,
        _base_ref_name: &str,
        _draft: bool,
    ) -> EyreExitOr<String> {
        Ok(Ok("".to_string()))
    }

    fn update_pull_request(
        &self,
        _effects: &Effects,
        _pr_num: usize,
        _base_ref_name: &str,
        _title: &str,
        _body: &str,
    ) -> EyreExitOr<()> {
        Ok(Ok(()))
    }
}
