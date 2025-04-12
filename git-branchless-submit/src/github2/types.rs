//! Assorted types for use across other modules.

use crate::CommitStatus;
use indexmap::map::IndexMap;
use lib::git::NonZeroOid;
use std::collections::HashMap;

/// Represents a pull request base which may or may not have a branch yet.
///
/// If `--create` is provided, each PR's base should be its parent commit. However, that commit may
/// not already have a branch. We can use its `NonZeroOid` as a placeholder until a branch is
/// created for it.
#[derive(Debug, PartialEq, Clone)]
pub enum RefOrOid {
    /// The Oid of a commit that we have not yet created a branch for.
    Oid(NonZeroOid),

    /// An already-existing branch name which could be the repo's default branch or another
    /// branchless-managed branch.
    Branch(String),
}

/// [`StackedChange`] collects all of the information known about a local commit and its
/// remote status.
#[derive(Debug)]
pub struct StackedChange {
    /// The current local oid for this change.
    pub oid: NonZeroOid,

    /// Information about the change that the [`crate::Forge`] trait expects to pass in and out of
    /// its trait methods. We store the same information more richly but maintain this to satisfy
    /// the interface.
    pub commit_status: CommitStatus,

    /// Whether the local change should be pushed to the remote.
    pub should_push: bool,

    /// Whether the pull request's title, body, or base ref should be updated.
    pub should_update_pr: bool,

    /// Pull request URL for this change (if one has been opened).
    pub pr_url: Option<String>,

    /// Pull request number for this change (if a PR has been opened).
    pub pr_number: Option<usize>,

    /// The base ref that is (or will be) used for this pull request.
    ///
    /// If `--create` was passed on the CLI and this change's parent was previously unsubmitted,
    /// this field will be an OID until we can make a branch for the the unsubmitted commit.
    pub pr_base: RefOrOid,

    /// Commit message summary which corresponds to the title of the pull request.
    pub title: String,

    /// Commit message body which corresponds to the body of the pull request.
    ///
    /// Before updating the pull request, a section will be injected into the body which links to
    /// other pull requests in the stack(s) containing this PR.
    pub body: String,
}

/// Information about the changes/stacks that this invocation of `git submit` is operating on. May
/// include additional commits if revset passed to `git commit` is disjointed or not connected to
/// the default branch.
#[derive(Debug, Default)]
pub struct StackState {
    /// Information about all of the relevant commits for this invocation of `git submit`. It
    /// maintains the order the elements were inserted in, so if a commit is inserted before its
    /// children it will be in topological order.
    pub commits: IndexMap<NonZeroOid, StackedChange>,

    /// A list of linear stacks between the main branch and all of the heads in the revset. Some
    /// commits may appear in more than one stack in the case of a forked stack.
    pub stacks: Vec<Vec<NonZeroOid>>,

    /// Mapping from an OID to a single branchless-managed branch name.
    pub oids_to_branchless_refs: HashMap<NonZeroOid, String>,
}
