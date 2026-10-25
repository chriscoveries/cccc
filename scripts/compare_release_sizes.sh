#!/usr/bin/env bash
# A fixed-source experiment. This controller is not part of the source changes.
set -euo pipefail

export GIT_TERMINAL_PROMPT=0
repo_root=$(pwd -P)
measurement_root=$(mktemp -d "${RUNNER_TEMP:?}/release-size.XXXXXXXX")
results="$repo_root/measurements"
mkdir "$results"
worktrees=()
cleanup() {
    local checkout failed=0
    for checkout in "${worktrees[@]}"; do
        timeout 30 /usr/bin/git -c credential.helper='!/usr/bin/gh auth git-credential' -C "$repo_root" worktree remove --force "$checkout" || failed=1
    done
    (( failed == 0 )) || return 1
    rm -rf -- "$measurement_root"
}
trap cleanup EXIT

git_read() {
    timeout 30 /usr/bin/git -c credential.helper='!/usr/bin/gh auth git-credential' "$@"
}

base=8b2d0b0fb984189f49337206e285bb61349f4457
refs=(
    "$base"
    14fd981f0f2a280aec128061d8ffeffba740d63f
    2636e88052a4271faaad1a93a1c8d72cb2e35cf3
    c605cfcd036d99af2772d057c4b80571094e68e0
)
labels=(base weekday-parser revision-tests context-startup)
web_tree=$(git_read rev-parse "$base:web")
lock_blob=$(git_read rev-parse "$base:Cargo.lock")
[[ $(git_read rev-parse HEAD:web) == "$web_tree" ]]
[[ $(git_read rev-parse HEAD:Cargo.lock) == "$lock_blob" ]]
{
    cat /etc/os-release
    rustc --version --verbose
    cargo --version
    ldd --version
    strip --version
    uname -m
    printf 'controller=%s\nweb_tree=%s\nlock_blob=%s\n' "$(git_read rev-parse HEAD)" "$web_tree" "$lock_blob"
    tar --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner -cf - -C web/dist . | sha256sum
} > "$results/environment.txt"
printf 'label\tsha\tunstripped_bytes\tstripped_bytes\tsha256\tbuild_seconds\n' > "$results/sizes.tsv"

checkout="$measurement_root/source"
git_read worktree add --detach "$checkout" "$base"
worktrees+=("$checkout")
for i in "${!refs[@]}"; do
    ref=${refs[$i]}
    label=${labels[$i]}
    [[ $(git_read rev-parse "$ref:web") == "$web_tree" ]]
    [[ $(git_read rev-parse "$ref:Cargo.lock") == "$lock_blob" ]]
    git_read -C "$checkout" checkout --detach "$ref"
    mkdir -p "$checkout/web/dist"
    cp -a "$repo_root/web/dist/." "$checkout/web/dist/"
    touch "$checkout/web/dist/index.html"
    (
        cd "$checkout"
        export CARGO_TARGET_DIR="$repo_root/target"
        /usr/bin/time -f '%e' -o "$results/$label-seconds.txt" \
            timeout --kill-after=30 1800 cargo build --release --locked --features standalone -p cccc --bin cccc \
            > "$results/$label-build.log" 2>&1
        binary="$CARGO_TARGET_DIR/release/cccc"
        unstripped=$(stat -c %s "$binary")
        cp "$binary" "$measurement_root/$label-stripped"
        strip "$measurement_root/$label-stripped"
        stripped=$(stat -c %s "$measurement_root/$label-stripped")
        digest=$(sha256sum "$measurement_root/$label-stripped")
        digest=${digest%% *}
        "$measurement_root/$label-stripped" --version > "$results/$label-version.txt"
        size -A "$measurement_root/$label-stripped" > "$results/$label-sections.txt"
        printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$label" "$ref" "$unstripped" "$stripped" "$digest" \
            "$(cat "$results/$label-seconds.txt")" >> "$results/sizes.tsv"
    )
done
cat "$results/sizes.tsv"
