#!/bin/bash

stub_sha=$(git cherry master -v | head -n 1 | cut -d ' ' -f 2)
git checkout matt-build || true
git reset --hard $stub_sha || true

# if git branchless is installed, get rid of the old stack
git hide 'descendants(.) - .' || true

for branch in gh-forge-improvements gpg-support
do
    merge_base=$(git merge-base master $branch)
    git cherry-pick $merge_base..$branch
done
