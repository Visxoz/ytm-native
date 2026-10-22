#!/usr/bin/env bash
# Release notes from the commits between two releases, grouped by the type
# in each subject: feat, fix and perf. Other types, and the version bump,
# are left out, as they change nothing a user sees.
#
#   release-notes.sh <previous-tag> <tag> [previous-ref] [ref]
#
# The refs default to the tags. Locally this checkout shares its tags with
# ytm-lite, so pass the commits from `git ls-remote --tags ytm-native`.
set -euo pipefail

previous=$1
tag=$2
previous_ref=${3:-$previous}
ref=${4:-$tag}
repo=${GITHUB_REPOSITORY:-Visxoz/ytm-native}

section() {
    local heading=$1 type=$2 lines
    lines=$(git log --format='%s' --reverse "$previous_ref..$ref" \
        | grep -E "^$type: " | sed -E 's/^[a-z]+: /- /' || true)
    if [ -n "$lines" ]; then
        printf '## %s\n\n%s\n\n' "$heading" "$lines"
    fi
}

section "New" feat
section "Fixes" fix
section "Faster" perf

cat <<EOF
## Install

Download \`ytm-native_${tag#v}_x64-setup.exe\` for an installer, or the portable exe to run from anywhere. The app is not code-signed yet, so if Windows says it protected your PC, choose **More info**, then **Run anyway**.

**Full changelog:** https://github.com/$repo/compare/$previous...$tag
EOF
