---
name: nspr
description: >-
  How to manage stacked GitHub pull requests with `nspr` (one PR per commit,
  tracked via `Pull-Request:` commit trailers). Use whenever creating, updating,
  rebasing, amending, rewording, squashing/folding fixups into, reordering, or
  landing commits on a branch that is (or will be) a PR stack, and whenever
  investigating stale/duplicate/orphaned PRs. Always use this for upstream LLVM
  (github.com/llvm/llvm-project) pull requests.
---

# Stacked pull requests with `nspr`

`nspr` (`~/.cargo/bin/nspr`) turns each commit on the current branch into one
GitHub pull request, based on the PR for the commit below it. It is the tool
used for **all upstream LLVM (`llvm/llvm-project`) PRs** — never open, retarget,
or close stack PRs by hand with `gh pr create` / the web UI.

## How nspr tracks commits → PRs

* nspr identifies which PR a commit belongs to **only** through the trailer at
  the end of the commit message:

  ```text
  Pull-Request: https://github.com/llvm/llvm-project/pull/225131
  ```

* Legacy `spr` PRs with `Pull Request:` (space instead of hyphen) or `[spr]`
  branch commits are detected as `spr (run nspr upgrade)` in `nspr status` and
  can be converted with `nspr upgrade`.
* After creating PRs, nspr amends the local commits to add `Pull-Request:`
  trailers (the branch reflog shows `nspr rewrote commit messages`).
* Head branches are named `users/<github-user>/<slugified-commit-subject>`. If a
  name is already taken, nspr appends `-1`, `-2`, … — a `-1` branch for a PR you
  thought already existed is a red flag that a trailer was lost.
* PR descriptions on GitHub are the commit message **without** `Pull-Request:` /
  `Depends-On:` trailers (other trailers like `Fixes:` are kept); when a stack
  has 2+ PRs and `stack_comments` is enabled, nspr also maintains a
  `<!-- nspr:stack -->` comment listing the stack.
* `Depends-On: main` marks the root of an independent stack (created via
  `nspr diff -c` / `--cherry-pick` for a single commit or `nspr diff --new-stack`
  for a new stack on the same branch). `Depends-On: #<N>` (or multiple
  comma-separated PRs `Depends-On: #<A>, #<B>` / multiple `Depends-On:` lines)
  stacks a commit on non-adjacent PRs via a 3-way tree merge.

## The #1 pitfall: losing the `Pull-Request:` trailer

If a commit loses its trailer, `nspr` no longer knows which PR it belongs to.
When `nspr diff` sees a commit without a `Pull-Request:` trailer whose natural
branch (`users/<github-user>/<slug>`) already has an open PR on GitHub, it
prints a warning and interactively prompts whether to **relink** the commit to
that existing PR, open a new PR (on a `…-1` branch), or abort. However, in
`--no-prompt` mode or if the commit subject also changed (changing the slug),
losing a trailer can still open a **new duplicate PR** and leave the old PR
orphaned. (This happened earlier with llvm/llvm-project#225132 → duplicate
#225140.)

Trailers get lost when a commit message is replaced wholesale, e.g.:

* `git commit --amend -F <file>` / `-m` with a message that lacks the trailer,
* `reword`, `squash`, `fixup -C`, or `amend!` commits during interactive rebase,
* copying the message from the GitHub PR description or from a pre-nspr
  version of the commit,
* scripted rebases (`git rebase --exec …`) that amend commits — verify even if
  they use `--no-edit`.

Rules:

* **Whenever you rewrite a commit message, keep the existing
  `Pull-Request:` trailer verbatim as the last paragraph.** Read it with
  `git log -1 --format=%B <commit>` before rewriting, and include it in any
  message file you pass to `git commit --amend -F`.
* `git commit --fixup=<commit>` + `git rebase -i --autosquash` (plain `fixup`)
  keeps the target commit's message and trailer — prefer this for code changes.
  Do not fold fixups unless the user asked; they often want to review them first.
* **After every rebase/amend/reword on a stack**, verify all trailers are still
  present and unchanged before the user runs `nspr diff`:

  ```bash
  git log --format='%h %s%n    %(trailers:key=Pull-Request,valueonly)' <trunk>..HEAD
  ```

  Every commit that already has a PR must still show its URL; only genuinely new
  commits may be empty. Compare against the pre-rewrite state
  (`git reflog`, or `<branch>@{1}`) if unsure.
* Never add, change, or reorder trailers to "fix" PR numbers by guessing; ask the
  user.

## Commands

Run `nspr` commands from the repo root. They talk to GitHub (and push over SSH,
automatically falling back to HTTPS with `gh auth token` credentials if
`$SSH_AUTH_SOCK` is disconnected or has no keys loaded), so run them outside the
sandbox. Prefer letting the user run `diff`/`land`/`close` unless they asked you
to push.

Global flags (accepted before or after any subcommand):
* `--remote <REMOTE>` (default: `origin`) — Git remote pointing at GitHub.
* `-v` / `--verbose` — Print detailed decisions (and reviewer/check details in `status`).
* `-P` / `--no-pager` — Disable paging (by default `nspr compare` pipes to `git --paginate` when stdout is a TTY; agents should pass `-P` / `--no-pager` if running in a PTY).

| Command | Purpose |
|---|---|
| `nspr status [-v]` | Show the stack, CI check counts (`N/M checks`), review counts (`N approved`, `M changes requested`), and what `nspr diff` would do (`ok`, `modified`, `restack`, `new`, `spr (run nspr upgrade)`, `landable`). Pass `-v` / `--verbose` to list reviewer logins and failed CI check names on indented lines under each PR. |
| `nspr compare [COMMIT_OR_PR \| -a]` | *(Aliases: `show`, `interdiff`)* Compare a local commit (`HEAD` by default, a commit hash/rev `<COMMIT>` or `#<PR>`, or `-a`/`--all` for the whole stack) against its last version upstream on GitHub. Prints a colored `git diff` of both the commit message (`COMMIT_EDITMSG`) and code changes (projecting upstream onto the local base tree so rebasing onto newer `main` does not add noise). |
| `nspr diff [COMMIT_OR_PR]` (default) | Create/update PRs. Updates append an incremental commit to the PR branch (preserving review history on squash-only repos) unless a base change or merge conflict requires `restacked` (force-push).<br>**Targeting flags:**<br>• `<COMMIT_OR_PR>` or `--pr <COMMIT_OR_PR>`: create/update **only** that single commit or PR (pass a git commit hash/rev like `a1b2c3d` or `HEAD~1`, or `#1234` / URL).<br>• `--from <COMMIT_OR_PR>` / `--to <COMMIT_OR_PR>`: create/update **only** the inclusive commit range `[--from, --to]` (omit `--from` for bottom of branch through `--to`; omit `--to` for `--from` through `HEAD`). Accepts git commit hashes/revs (preferred, works for unsubmitted commits too) or PR numbers.<br>• `-c` / `--cherry-pick`: submit only `HEAD` as an independent PR on trunk (`Depends-On: main`).<br>• `--new-stack`: start a new independent stack on trunk at the first unsubmitted commit (or `HEAD`).<br>• `-a` / `--all`: push all independent stacks on the branch.<br>**Other flags:** `-n`/`--dry-run` preview plan without mutating git or GitHub, `--update-message` overwrite GitHub-edited PR title/body from the local commit, `-m <msg>` update description, `--no-prompt`, `--draft`. |
| `nspr upgrade` | Convert legacy `spr` PRs (`[spr]` commits / `Pull Request:` trailers) in the stack to native `nspr` stacked PRs. Pass `--update-message` to also overwrite PR titles/bodies that were edited on GitHub. |
| `nspr sync` | Rebase onto trunk and reconcile PRs merged elsewhere. Also needed after merging a stack PR via the web UI. |
| `nspr land [PR]` | Squash-merge a PR (`<PR>` or `--pr <PR>`, `-c`/`--cherry-pick` for independent `HEAD` PR, `-b`/`--bottom` for lowest ready PR, `-a`/`--all` for all ready PRs bottom-up, `-m <msg>` custom squash body). Re-running `nspr land` after an interrupted merge automatically completes post-merge stack repair and local rebase. |
| `nspr amend` | Copy titles/descriptions edited on GitHub back into local commits. |
| `nspr close <PR>` | Abandon a PR and restack what depended on it (use this instead of closing on GitHub). Works even if the commit was already dropped locally. |
| `nspr list [-a]` | List open PR stacks (`-a` includes all authors). |
| `nspr patch <PR>` | Fetch a PR and its stack dependencies into a local branch (`pr/<number>`, `-b`/`--branch <name>`, `--no-checkout`). |
| `nspr completions [--static] <shell>` | Generate shell completion scripts (`bash`, `zsh`, `fish`, `elvish`, `powershell`). |

## How multi-stack branches, range updates, restacking, and `nspr land` work

* **Updating a single commit/PR or a commit range (`<commit>`, `--from`, `--to`)**:
  * When asked to push/update a specific commit/PR or subset of commits on a
    branch that also has other modified/unsubmitted commits, **always scope
    `nspr diff`** rather than running bare `nspr diff` (which updates the entire
    current stack and can touch or comment on unrelated PRs). Prefer passing git
    commit hashes directly (works for both new/unsubmitted commits and existing
    PRs without looking up trailers; `#<PR>` is also accepted):
    * Single commit (or PR): `nspr diff <commit-sha>` (or `nspr diff HEAD~2`, `nspr diff #1234`).
    * Inclusive commit range: `nspr diff --from <start-sha> --to <end-sha>` (or `nspr diff --from HEAD~3 --to HEAD~1`, `nspr diff --from #1234 --to #1236`).
    * Bottom of branch up to a commit: `nspr diff --to <end-sha>` / `nspr diff --to HEAD~1` (useful when `HEAD` is a local WIP commit you don't want to open a PR for yet).
    * From a commit up to `HEAD`: `nspr diff --from <start-sha>`.
  * Before pushing, you can preview the exact difference against GitHub with
    `nspr -P compare <commit-sha>` (or `nspr -P compare --all`) and dry-run the
    push plan with `nspr diff -n --from <start-sha> --to <end-sha>`.
* **Multiple stacks in one branch**:
  * `nspr status` groups independent stacks (`Depends-On: main`) into separate
    blocks.
  * By default, `nspr diff` updates only the current stack (and warns if other
    stacks on the branch were skipped); pass `nspr diff --all` (`-a`) to push all
    stacks on the branch.
  * `nspr diff --cherry-pick` (`-c`) only computes trees and updates for `HEAD`,
    so unrelated conflicts or `fixup!` commits lower in the branch do not block it.
* **Commit message updates**:
  * In `preserveCommitHistory` mode (the default on squash-only repos like
    `llvm/llvm-project`), PR branches use `[nspr] initial commit` as their root
    commit message and track the last-synced message in `refs/nspr/msg/<N>`.
    Amending a commit message locally (or pulling edits via `nspr amend`)
    updates the PR title/body via the GitHub API (`update message` badge) with
    **zero git pushes or force-pushes**.
  * If a PR's title/body has **not** been edited on GitHub since the message was
    last synced, amending the local commit message and running `nspr diff`
    automatically updates the PR title/body on GitHub (`update message` badge).
  * If the PR title/body **was** edited on GitHub, `nspr status` shows
    `message differs` and `nspr diff` preserves the GitHub edits unless you pass
    `--update-message` (or run `nspr amend` first to pull the GitHub edits into
    your local commit).
* **Minimal pushes on `nspr diff`**: Amending a lower commit (even after rebasing
  the local branch onto a newer `main`) only pushes a new commit to the amended
  PR's branch. Upper PRs stay `ok` and are **not** force-pushed (`restack`)
  unless:
  * you pass `nspr diff --all` (`-a`),
  * the repository requires branches to be up to date before merging, or
  * the lower commit's change causes a 3-way merge conflict with an upper PR on
    GitHub (which `nspr status` / `nspr diff` detects automatically).
* **CI-preserving `nspr land --all`**:
  * Bottom-up, `nspr land --all` retargets each ready PR to `main` and merges it
    using its existing `head_oid` (no force-push, so existing CI checks stay
    green) as long as Git's 3-way merge of the PR branch with the new squash
    commit on `main` matches the cherry-picked tree.
  * If two PRs in the stack modified overlapping lines in the same file (or an
    earlier PR was amended without restacking the upper PR), Git's 3-way merge
    over the pre-stack `main` base will conflict; `nspr land --all` detects this
    and automatically repairs the remaining open PRs onto the new squash commit
    before continuing.
  * When `land --all` finishes (or stops at a draft / unapproved / failing-CI
    PR), it repairs any remaining unmerged PRs **once** onto the final squash
    commit, deletes all merged PR branches, and rebases the remaining local
    commits onto `main`.

## Workflow for addressing review comments on a stack

* Fetch comments per PR (`gh api repos/llvm/llvm-project/pulls/<N>/comments`,
  `…/issues/<N>/comments`, `…/pulls/<N>/reviews`). The stack order is in the
  `<!-- nspr:stack -->` comment on each PR (or `nspr status -v`).
* Map each PR to its local commit through the trailer
  (`git log --format='%h %(trailers:key=Pull-Request,valueonly) %s' <trunk>..HEAD`).
* Make changes as `git commit --fixup=<commit>` on top of the branch; fold them
  only when the user asks (`git rebase -i --autosquash <trunk>` or a prepared
  todo list via `git -c sequence.editor="cp <todo>" rebase -i <base>`).
* Check every commit in the stack still builds/passes the relevant tests, not
  only the tip.
* When updating a commit message after folding, write the new message **with the
  original `Pull-Request:` trailer**, then run the trailer check above.

## Investigating stale or duplicate PRs

* `gh pr view <N> --json headRefName,baseRefName,state,createdAt,closedAt` — a
  duplicate usually has the same subject and a `-1` head branch created during an
  `nspr diff`.
* `gh api repos/<owner>/<repo>/issues/<N>/events` — orphaned PRs keep receiving
  only `base_ref_force_pushed` events.
* `git reflog --date=iso <branch>` and `git reflog --date=iso HEAD` show which
  rebase/amend step dropped the trailer (`nspr rewrote commit messages` entries
  are nspr; `rebase (…)` / `commit (amend)` entries are local rewrites).
* Clean up an orphan with `nspr close <N>` if it is still tracked, otherwise close
  it on GitHub and delete its head branch (`git push origin --delete <branch>`) —
  only with the user's approval.
