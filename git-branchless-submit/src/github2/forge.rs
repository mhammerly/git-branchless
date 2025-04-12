//! Implementation of the [`crate::Forge`] trait for GitHub.

use super::types::{RefOrOid, StackState, StackedChange};
use crate::branch_forge::BranchForge;
use crate::github2::client;
use crate::{CommitStatus, CreateStatus, Forge, SubmitOptions, SubmitStatus};
use lib::core::dag::CommitSet;
use lib::core::dag::Dag;
use lib::core::effects::{Effects, OperationType};
use lib::core::eventlog::EventLogDb;
use lib::core::repo_ext::{RepoExt, RepoReferencesSnapshot};
use lib::git::CategorizedReferenceName;
use lib::git::ConfigRead;
use lib::git::GitErrorCode;
use lib::git::GitRunInfo;
use lib::git::RepoError;
use lib::git::{NonZeroOid, Repo};
use lib::try_exit_code;
use lib::util::EyreExitOr;
use std::collections::HashMap;
use std::env;
use std::fmt::Debug;
use std::iter;
use tracing::warn;

/// Testing environment variable. When this is set, the executable will use the
/// mock Github implementation. This should be set to the path of an existing
/// repository that represents the remote/Github.
pub const MOCK_REMOTE_REPO_PATH_ENV_KEY: &str = "BRANCHLESS_SUBMIT_GITHUB_MOCK_REMOTE_REPO_PATH";

fn get_pr_branch_prefix(repo: &Repo) -> eyre::Result<String> {
    repo.get_readonly_config()?
        .get_or_else("branchless.submit.branchPrefix", || {
            "branchless/".to_string()
        })
}

fn get_expand_details_config(repo: &Repo) -> eyre::Result<bool> {
    repo.get_readonly_config()?
        .get_or("branchless.submit.defaultDetailsOpen", true)
}

fn build_branchless_ref_map(
    references_snapshot: &RepoReferencesSnapshot,
    repo: &Repo,
) -> eyre::Result<HashMap<NonZeroOid, String>> {
    let pr_branch_prefix = get_pr_branch_prefix(repo)?;
    let oids_to_branchless_refs = references_snapshot
        .branch_oid_to_names
        .iter()
        .flat_map(|(oid, names)| iter::repeat(oid).zip(names.iter()))
        .map(|(oid, name)| (*oid, CategorizedReferenceName::new(name).render_suffix()))
        .filter(|(_oid, name)| name.starts_with(&pr_branch_prefix))
        .collect();
    Ok(oids_to_branchless_refs)
}

/// The [GitHub](https://en.wikipedia.org/wiki/GitHub) code hosting platform.
/// This forge integrates specifically with the `gh` command-line utility.
#[derive(Debug)]
pub struct GitHubForge<'a> {
    /// Wrapper around side-effectful operations.
    effects: &'a Effects,

    /// Handle to the local git repository.
    repo: &'a Repo,

    /// Handle to a DAG walking the commit tree.
    dag: &'a Dag,

    /// Client for interacting with GitHub.
    client: Box<dyn client::GitHubClient>,

    /// Whether to create branches and pull requests for unsubmitted commits.
    create_unsubmitted: bool,

    /// Handle to a [`BranchForge`] for creating and pushing remote branches.
    branch_forge: BranchForge<'a>,

    /// Cached information about commits that are relevant to the revset that `git submit` was
    /// called for.
    stack_state: StackState,
}

impl<'a> GitHubForge<'a> {
    /// Factory function to create a [`GitHubForge`].
    #[allow(clippy::new_ret_no_self)]
    pub fn new(
        effects: &'a Effects,
        repo: &'a Repo,
        dag: &'a Dag,
        references_snapshot: &'a RepoReferencesSnapshot,
        event_log_db: &'a EventLogDb<'a>,
        git_run_info: &'a GitRunInfo,
        create_unsubmitted: bool,
    ) -> eyre::Result<Box<dyn Forge + 'a>> {
        let client: Box<dyn client::GitHubClient> = match env::var(MOCK_REMOTE_REPO_PATH_ENV_KEY) {
            Ok(path) => Box::new(client::MockGitHubClient {
                remote_repo_path: path.into(),
            }),
            Err(_) => Box::new(client::RealGitHubClient {}),
        };

        let branch_forge = BranchForge {
            effects,
            git_run_info,
            dag,
            repo,
            event_log_db,
            references_snapshot,
        };

        // Create a map of OIDs to a single branch name that starts with the branchless prefix.
        let oids_to_branchless_refs = build_branchless_ref_map(references_snapshot, repo)?;

        Ok(Box::new(GitHubForge {
            effects,
            repo,
            dag,
            client,
            create_unsubmitted,
            branch_forge,
            stack_state: StackState {
                oids_to_branchless_refs,
                ..Default::default()
            },
        }))
    }

    fn find_pr_for_oid<'b: 'a>(
        &'a self,
        oid: &NonZeroOid,
        pull_requests_per_branch: &'b HashMap<String, Vec<client::PullRequestInfo>>,
    ) -> Option<&client::PullRequestInfo> {
        let pr_branch = self.stack_state.oids_to_branchless_refs.get(oid);

        let pr = pr_branch
            .and_then(|branch| pull_requests_per_branch.get(branch))
            .and_then(|prs| prs.first());

        pr
    }
}

impl Forge for GitHubForge<'_> {
    fn query_status(
        &mut self,
        commit_set: CommitSet,
    ) -> EyreExitOr<HashMap<NonZeroOid, CommitStatus>> {
        let references_snapshot = self.repo.get_references_snapshot()?;
        let main_branch_oid = CommitSet::from(references_snapshot.main_branch_oid);
        let main_branch_name = lib::core::config::get_main_branch_name(self.repo)?;
        let default_remote = self.repo.get_default_push_remote()?;

        // Find pull requests for branches that start with the configured prefix (or `branchless/`
        // by default).
        let pr_branch_prefix = get_pr_branch_prefix(self.repo)?;
        let pull_requests_per_branch = try_exit_code!(self
            .client
            .query_repo_pull_requests(self.effects, Some(&pr_branch_prefix))?);

        // The user may have run `git submit 'draft()'` which includes multiple separate subgraphs.
        // Separate the subgraphs from each other.
        // TODO: we might not actually have to do this. it should always be the case that the last
        // element in the topo-sorted set is a HEAD even if the set contains disconnected
        // components. It is probably simpler if that's the case to call `heads()` and iterate over
        // the heads, grabbing a linear ancestry for each head. no need to pop/remove things from
        // any sets.
        let mut subgraphs: Vec<Vec<NonZeroOid>> = self
            .dag
            .get_connected_components(&commit_set)?
            .iter()
            .flat_map(|set| self.dag.sort(set))
            .collect::<Vec<Vec<NonZeroOid>>>();

        for subgraph in subgraphs.iter_mut() {
            // Since `subgraph` is topologically sorted, the tail elements will be branch heads. The
            // loop below pops each branch head off the subgraph, visits each commit between the
            // default branch and the head, and removes each commit as it's visited. The algorithm
            // assumes there are no merge commits in the subgraph.
            //
            // After the first loop iteration, the first branch head and all of its ancestors will
            // be removed from the subgraph. If the whole subgraph is a linear stack with a single
            // branch head, the loop finishes there. If the subgraph instead has a forked stack,
            // there will be at least one more branch head remaining in the subgraph as well as any
            // ancestors unique to those heads.
            while let Some(head) = subgraph.pop() {
                // TODO: can i avoid this clone
                let mut nearest_base_ref = RefOrOid::Branch(main_branch_name.clone());
                let mut update_until_head = false;
                let mut update_whole_stack = false;

                // Get commits between the default branch and the current HEAD, with the HEAD
                // included in the result. Commits closest to the default branch come first.
                let linear_stack_oids = self
                    .dag
                    .query_only(CommitSet::from(head), main_branch_oid.clone())?;
                let mut linear_stack_oids = self.dag.commit_set_to_vec(&linear_stack_oids)?;
                linear_stack_oids.reverse();

                for oid in linear_stack_oids.iter() {
                    let commit = self.repo.find_commit_or_fail(*oid)?;
                    let commit_summary =
                        String::from_utf8_lossy(&commit.get_summary()?).into_owned();
                    let commit_body =
                        String::from_utf8_lossy(&commit.get_message_pretty()).into_owned();

                    if let Some(pr) = self.find_pr_for_oid(oid, &pull_requests_per_branch) {
                        let head_ref = RefOrOid::Branch(pr.head_ref_name.clone());

                        // If our local commits have been reordered, or if `--create` was passed
                        // and we have inserted a commit into the stack, this will be true.
                        let base_ref = RefOrOid::Branch(pr.base_ref_name.clone());
                        let base_ref_stale = base_ref != nearest_base_ref;

                        // If we've amended this commit and need to push to the remote branch, this
                        // will be true.
                        let oid_stale = &pr.head_ref_oid.0 != oid;

                        // If we've changed the commit message summary since the last time we
                        // updated the PR, this will be true.
                        // TODO: if we have any stack index stuff in the title, remove it before
                        // comparing with the local commit title
                        let title_stale = commit_summary != pr.title;

                        // If we've amended this commit, presumably we've rebased all its
                        // descendents. The rest of the stack needs a push.
                        update_until_head |= oid_stale;

                        // If we've reordered, inserted, or renamed anything in our stack, we will
                        // need to update the PR body of every PR in the stack to reflect the new
                        // structure.
                        update_whole_stack |= base_ref_stale;

                        // Note: `update_whole_stack` can still become true later, in which case
                        // we will need to change all `UpToDate`s to `NeedsUpdate`s.
                        let submit_status =
                            if title_stale || update_until_head || update_whole_stack {
                                SubmitStatus::NeedsUpdate
                            } else {
                                SubmitStatus::UpToDate
                            };

                        // If this commit is not yet tracked in `self.stack_state`, insert it.
                        let commit_status = CommitStatus {
                            submit_status,
                            local_commit_name: Some(pr.head_ref_name.clone()),
                            remote_name: default_remote.clone(),
                            remote_commit_name: Some(pr.head_ref_name.clone()),
                        };
                        let pr_url = Some(pr.url.clone());
                        let pr_number = Some(pr.number);
                        let pr_body = pr.body.clone();
                        self.stack_state
                            .commits
                            .entry(*oid)
                            .or_insert(StackedChange {
                                oid: *oid,
                                commit_status,
                                should_push: oid_stale,
                                should_update_pr: title_stale || base_ref_stale,
                                pr_url,
                                pr_number,
                                pr_base: nearest_base_ref.clone(),
                                title: commit_summary,
                                // Don't try to sync the PR body with the local commit message. It's
                                // complicated to decide whether the commit message and PR body are
                                // out of sync while ignoring the stack structure section, and
                                // clobbering manual edits to the PR body is irritating anyway.
                                body: pr_body,
                            });

                        // Descendents are presumed to be based on this PR, at least until we find
                        // the next presumed base.
                        nearest_base_ref = head_ref;
                    } else {
                        self.stack_state
                            .commits
                            .entry(*oid)
                            .or_insert(StackedChange {
                                oid: *oid,
                                // We'll be creating our own branch for this commit, even if it already
                                // has some.
                                commit_status: CommitStatus {
                                    submit_status: SubmitStatus::Unsubmitted,
                                    local_commit_name: None,
                                    remote_name: None,
                                    remote_commit_name: None,
                                },
                                should_push: true,
                                should_update_pr: true,
                                pr_url: None,
                                pr_number: None,
                                pr_base: nearest_base_ref.clone(),
                                title: commit_summary,
                                body: commit_body,
                            });

                        // If we are supposed to create PRs for unsubmitted commits, then this
                        // unsubmitted commit represents a structural change to the stack. Existing
                        // PRs will all need to be updated and future commits should use this
                        // commit as a base.
                        if self.create_unsubmitted {
                            update_until_head = true;
                            update_whole_stack = true;
                            nearest_base_ref = RefOrOid::Oid(*oid);
                        }
                    }

                    // Since we've processed this commit now, remove it from the subgraph.
                    if let Some(index_in_subgraph) = subgraph.iter().position(|other| other == oid)
                    {
                        subgraph.remove(index_in_subgraph);
                    }
                }

                // If anything structural changed in our stack, we'll have to change all the
                // `UpToDate`s to `NeedsUpdate` since at least the PR body of each PR will need to
                // be updated.
                if update_whole_stack {
                    for (_, stacked_change) in self
                        .stack_state
                        .commits
                        .iter_mut()
                        .filter(|(oid, _)| linear_stack_oids.contains(oid))
                    {
                        if stacked_change.commit_status.submit_status == SubmitStatus::UpToDate {
                            stacked_change.commit_status.submit_status = SubmitStatus::NeedsUpdate;
                        }
                    }
                }

                // Save the linear stack we just walked. This helps us know how many linear stacks
                // a given commit is in when we are building the stack section of the PR body.
                self.stack_state.stacks.push(linear_stack_oids);
            }
        }

        // We track more information than we actually need to return. Pull out just the oids and
        // the `CommitStatus`es to return.
        let result = self
            .stack_state
            .commits
            .iter()
            .map(|(oid, stacked_change)| (*oid, stacked_change.commit_status.clone()))
            .collect();
        Ok(Ok(result))
    }

    fn create(
        &mut self,
        commits: HashMap<NonZeroOid, CommitStatus>,
        options: &SubmitOptions,
    ) -> EyreExitOr<HashMap<NonZeroOid, CreateStatus>> {
        let pr_branch_prefix = get_pr_branch_prefix(self.repo)?;
        let mut result = HashMap::new();

        // Track progress as we go
        let (_effects, progress) = self
            .effects
            .start_operation(OperationType::CreatePullRequests);
        progress.notify_progress(0, commits.len());

        // We assume all of the commits in `commits` are already tracked in
        // `self.stack_state.commits` which should be in topological order.
        for (oid, stacked_change) in self.stack_state.commits.iter_mut() {
            // If this commit is not tracked or not `Unsubmitted`, skip it.
            if commits
                .get(oid)
                .map(|status| status.submit_status != SubmitStatus::Unsubmitted)
                .unwrap_or(true)
            {
                continue;
            }

            // Create a GitHub issue. Issues and PRs draw from the same ID pool and we can convert
            // this issue into a PR, so this lets us use the PR number as the branch name.
            let new_issue_url = try_exit_code!(self.client.create_issue(
                self.effects,
                &stacked_change.title,
                &stacked_change.body
            )?);

            let Some(last_slash_idx) = new_issue_url.rfind('/') else {
                warn!(?new_issue_url, "Malformed GitHub issue URL");
                continue;
            };
            let Ok(issue_num) = str::parse::<usize>(&new_issue_url[last_slash_idx + 1..]) else {
                warn!(?new_issue_url, "Malformed GitHub issue URL");
                continue;
            };

            // Create the new branch
            let commit = self.repo.find_commit_or_fail(*oid)?;
            let new_branch_name = format!("{}pr{}", pr_branch_prefix, issue_num);
            match self.repo.create_branch(&new_branch_name, &commit, false) {
                Ok(_branch) => {}
                Err(RepoError::CreateBranch { source, name: _ })
                    if source.code() == GitErrorCode::Exists => {}
                Err(err) => return Err(err.into()),
            }

            // Update our `CommitStatus` to satisfy the `Forge` trait API
            stacked_change.commit_status.local_commit_name = Some(new_branch_name.clone());
            stacked_change.commit_status.remote_commit_name = Some(new_branch_name.clone());

            // Push the branch to the remote. We pass along what we get back from `BranchForge` in
            // the return value for this function.
            let single_commit_status = HashMap::from_iter(std::iter::once((
                *oid,
                stacked_change.commit_status.clone(),
            )));
            let created_branch =
                try_exit_code!(self.branch_forge.create(single_commit_status, options)?);
            result.extend(created_branch.into_iter());

            // If our base is an OID, look up the `StackedChange` for it because it should have a
            // branch now.

            let Some(base_ref_name) = (match &stacked_change.pr_base {
                RefOrOid::Branch(branch) => Some(branch),
                RefOrOid::Oid(base_oid) => self.stack_state.oids_to_branchless_refs.get(base_oid),
            }) else {
                warn!(?stacked_change.pr_base, "PR base is not known");
                continue;
            };

            // Convert the issue into a PR
            let pr_url = try_exit_code!(self.client.create_pull_request_from_issue(
                self.effects,
                issue_num,
                &new_branch_name,
                base_ref_name,
                options.draft,
            )?);

            stacked_change.commit_status.submit_status = SubmitStatus::NeedsUpdate;
            stacked_change.pr_url = Some(pr_url);
            stacked_change.pr_number = Some(issue_num);
            stacked_change.should_push = false;

            self.stack_state
                .oids_to_branchless_refs
                .entry(*oid)
                .or_insert(new_branch_name.clone());

            progress.notify_progress_inc(1);
        }

        // Build the arguments we need to call `self.update()` for the PRs we just created.
        let commits_to_update = self
            .stack_state
            .commits
            .iter()
            .filter(|(oid, _stacked_change)| commits.contains_key(oid))
            .map(|(oid, stacked_change)| (*oid, stacked_change.commit_status.clone()))
            .collect();
        try_exit_code!(self.update(commits_to_update, options)?);

        Ok(Ok(result))
    }

    fn update(
        &mut self,
        commits: HashMap<NonZeroOid, CommitStatus>,
        options: &SubmitOptions,
    ) -> EyreExitOr<()> {
        // Track progress as we go.
        let (_effects, progress) = self.effects.start_operation(OperationType::UpdateCommits);
        progress.notify_progress(0, commits.len());
        // Whether the `<details>` tag should be expanded by default or not
        let default_expand_details = get_expand_details_config(self.repo)?;

        // We assume all of the commits in `commits` are already tracked in
        // `self.stack_state.commits` which should be in topological order.
        for (oid, stacked_change) in self.stack_state.commits.iter() {
            // If this commit is not tracked or not `NeedsUpdate`, skip it.
            if commits
                .get(oid)
                .map(|status| status.submit_status != SubmitStatus::NeedsUpdate)
                .unwrap_or(false)
            {
                continue;
            }

            if stacked_change.should_push {
                let single_commit_status = HashMap::from_iter(std::iter::once((
                    *oid,
                    stacked_change.commit_status.clone(),
                )));
                try_exit_code!(self.branch_forge.update(single_commit_status, options)?);
            }

            if stacked_change.should_update_pr {
                let title = &stacked_change.title;
                let body = super::formatter::create_pr_body_for_oid(
                    *oid,
                    &self.stack_state,
                    default_expand_details,
                );
                let Some(base_ref_name) = (match &stacked_change.pr_base {
                    RefOrOid::Branch(branch) => Some(branch),
                    RefOrOid::Oid(base_oid) => {
                        self.stack_state.oids_to_branchless_refs.get(base_oid)
                    }
                }) else {
                    warn!(?stacked_change.pr_base, "PR base is not known");
                    continue;
                };

                let Some(pr_number) = stacked_change.pr_number else {
                    warn!(?stacked_change, "Change is missing PR number");
                    continue;
                };
                try_exit_code!(self.client.update_pull_request(
                    self.effects,
                    pr_number,
                    base_ref_name,
                    title,
                    &body
                )?);
            }

            progress.notify_progress_inc(1);
        }
        Ok(Ok(()))
    }
}
