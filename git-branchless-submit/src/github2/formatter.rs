//! Functions for building a GitHub PR comment with a section displaying the stacks that the PR is
//! part of.

use super::types::{StackState, StackedChange};
use lib::git::NonZeroOid;
use tracing::warn;

/// Magic string marking the beginning of a section of a commit message where stack information is
/// injected. Not visible in GitHub due to it being an HTML comment.
pub const PR_BODY_SECTION_START: &str = "<!-- %%% BRANCHLESS STACK INFO START %%% -->";

/// Magic string marking the end of a section of a commit message where stack information is
/// injected. Not visible in GitHub due to it being an HTML comment.
pub const PR_BODY_SECTION_END: &str = "<!-- %%% BRANCHLESS STACK INFO END %%% -->";

/// Build an HTML list with an item for each pull request in this stack. The current PR is
/// emphasized with an arrow emoji.
///
/// GitHub renders PR URLs nicely if they are in lists. This function builds an HTML list so that
/// it can be rendered inside a table cell.
pub fn table_cell_for_stack<'a, I>(stack: I, current_oid: NonZeroOid) -> String
where
    I: Iterator<Item = &'a StackedChange>,
{
    let mut cell = "<ul>".to_string();
    for change in stack {
        cell.push_str("<li>");
        if change.oid == current_oid {
            cell.push_str("➡️");
        }
        cell.push_str(change.pr_url.as_ref().unwrap_or(&"<unknown>".to_string()));
        cell.push_str("</li>");
    }
    cell.push_str("</ul>");
    cell
}

/// Builds a Markdown table displaying all of the stacks that the passed-in commit is in.
///
/// Each stack that the commit is in is rendered side-by-side in a table. Each stack is given a
/// column with a header cell (e.g. "Stack 1") and a data cell wherein the entire stack is
/// rendered. GitHub's comment renderer adjusts the width of each column and wraps text based on
/// the content of each cell so this works reasonably well for a small number of stacks.
pub fn build_stack_markdown_table_for_oid(oid: NonZeroOid, stack_state: &StackState) -> String {
    let mut relevant_stacks = stack_state
        .stacks
        .iter()
        .filter(|stack| stack.contains(&oid))
        .collect::<Vec<_>>();

    if relevant_stacks.is_empty() {
        warn!(?oid, "Commit is supposedly not part of any stacks");
        return "".to_string();
    }

    // Sort by stack length so it will look cleaner in a table.
    relevant_stacks.sort_by_key(|b| std::cmp::Reverse(b.len()));

    // Set up the table header
    let mut table = "|".to_string();
    for i in 0..relevant_stacks.len() {
        table.push_str(&format!(" Stack {} |", i + 1));
    }
    table.push_str("\n|");
    for _ in 0..relevant_stacks.len() {
        table.push_str("-|");
    }
    table.push('\n');

    table.push('|');
    for stack in &relevant_stacks {
        let stack_iter = stack
            .iter()
            .flat_map(|stack_oid| stack_state.commits.get(stack_oid).into_iter());
        table.push_str(&table_cell_for_stack(stack_iter, oid));
        table.push('|');
    }

    table
}

/// Create the pull request body for a commit by appending a table displaying the stacks that the
/// commit is in. TODO better comment
pub fn create_pr_body_for_oid(
    oid: NonZeroOid,
    stack_state: &StackState,
    expand_details: bool,
) -> String {
    let Some(stack_entry) = stack_state.commits.get(&oid) else {
        warn!(?oid, "Unable to find stack entry for commit");
        return "".to_string();
    };

    // TODO: i never put the branchless section into the local pr body so this shouldn't be
    // necessary after all?
    let body = &stack_entry.body;
    let stack_info_start = body.find(PR_BODY_SECTION_START).unwrap_or(body.len());
    let stack_info_end = body
        .find(PR_BODY_SECTION_END)
        .map(|idx| idx + PR_BODY_SECTION_END.len())
        .unwrap_or(body.len());

    let body_before_stack_info = &body[..stack_info_start];
    let body_after_stack_info = &body[stack_info_end..];

    let stack_info_section = build_stack_markdown_table_for_oid(oid, stack_state);

    let details_attrs = if expand_details { " open" } else { "" };
    format!(
        "{}{}\n<details{}><summary> Stack info:</summary>\n\n{}\n\n</details>\n{}{}",
        body_before_stack_info,
        PR_BODY_SECTION_START,
        details_attrs,
        stack_info_section, // pr section
        PR_BODY_SECTION_END,
        body_after_stack_info,
    )
}
