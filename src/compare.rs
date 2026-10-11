//! `nspr compare`: compare a local commit against its upstream pull request.
//!
//! Computes both the commit message diff (comparing the local commit's cleaned
//! message against the pull request's title and description on GitHub) and the
//! code interdiff (projecting the upstream pull request's patch onto the local
//! dependency tree so rebasing onto an advanced trunk does not introduce noise).

use std::io::{IsTerminal as _, Write as _};
use std::process::Command;

use color_eyre::eyre::{Result, WrapErr as _, bail};
use console::style;
use git2::Oid;

use crate::config::Config;
use crate::engine::{self, SyncOptions};
use crate::forge::{Forge, PrState, PullRequest};
use crate::git::Git;
use crate::patch_id::tree_patch_id;
use crate::review_diff::{render_tree_diff, review_base};
use crate::stack::{Dep, LayerSelection, Stack};
use crate::trailers::CommitMessage;

/// Entry name inserted into temporary comparison trees when the commit message
/// differs from the upstream pull request title/description.
pub const COMMIT_MSG_ENTRY: &str = "COMMIT_EDITMSG";

#[derive(Debug, Clone)]
pub struct LayerComparison {
    pub index: usize,
    pub commit: Oid,
    pub subject: String,
    pub pr_number: Option<u64>,
    pub pr_state: Option<PrState>,
    pub upstream_message: Option<String>,
    pub local_message: String,
    pub message_differs: bool,
    pub github_message_edited: bool,
    pub patch_differs: bool,
    /// Tree representing the upstream pull request projected onto the local
    /// dependency base (plus `COMMIT_EDITMSG` when `message_differs` is true).
    /// `None` if projecting onto the local dependency base had merge conflicts.
    pub upstream_compare_tree: Option<Oid>,
    /// Tree representing the local commit's effective tree (plus `COMMIT_EDITMSG`
    /// when `message_differs` is true).
    pub local_compare_tree: Oid,
    pub upstream_base_tree: Option<Oid>,
    pub upstream_head_tree: Option<Oid>,
    pub local_base_tree: Oid,
    pub local_head_tree: Oid,
}

impl LayerComparison {
    pub fn has_differences(&self) -> bool {
        self.message_differs || self.patch_differs
    }

    /// Render the unified diff between `upstream_compare_tree` and
    /// `local_compare_tree` using libgit2 (useful for tests and non-git callers).
    pub fn render_plain_diff(&self, git: &Git) -> Result<String> {
        if !self.has_differences() {
            return Ok(String::new());
        }
        let Some(upstream_tree) = self.upstream_compare_tree else {
            bail!(
                "upstream patch conflicts with local base tree; use `git range-diff`"
            );
        };
        render_tree_diff(git.repo(), upstream_tree, self.local_compare_tree)
    }
}

fn insert_commit_msg_blob(
    git: &Git,
    base_tree_oid: Oid,
    message: &str,
) -> Result<Oid> {
    let repo = git.repo();
    let blob_oid = repo.blob(message.as_bytes())?;
    let base_tree = repo.find_tree(base_tree_oid)?;
    let mut builder = repo.treebuilder(Some(&base_tree))?;
    builder.insert(COMMIT_MSG_ENTRY, blob_oid, 0o100644)?;
    Ok(builder.write()?)
}

fn upstream_clean_message(
    layer_msg: &CommitMessage,
    pr: &PullRequest,
) -> String {
    let clean_body = crate::pr_body::strip_warning(&pr.body);
    let mut upstream_cm = layer_msg.clone();
    upstream_cm.update_from_pr(&pr.title, &clean_body);
    upstream_cm.clean_for_branch()
}

fn pr_was_edited_on_forge(
    git: &Git,
    layer_msg: &CommitMessage,
    pr: &PullRequest,
) -> bool {
    let baseline = crate::refs::get_message(git, pr.number)
        .map(|m| CommitMessage::parse(&m))
        .unwrap_or_else(|| layer_msg.clone());
    let clean_body = crate::pr_body::strip_warning(&pr.body);
    let clean_body_trimmed = clean_body.trim();
    pr.title.trim() != baseline.subject.trim()
        || (clean_body_trimmed != baseline.clean_body_for_pr().trim()
            && clean_body_trimmed != baseline.body.trim())
}

/// Compare the selected layers of `stack` against their upstream pull requests.
pub async fn compare_selection(
    git: &Git,
    forge: &dyn Forge,
    stack: &Stack,
    selection: &LayerSelection,
) -> Result<Vec<LayerComparison>> {
    let mut stack = stack.clone();
    let opts = SyncOptions {
        selection: selection.clone(),
        ..Default::default()
    };
    engine::resolve_external_deps(forge, &mut stack, &opts).await?;
    let trees = stack.trees_for(git, selection)?;
    let prs = engine::gather_for(forge, &stack, &opts).await?;

    let mut out = Vec::new();
    for (i, layer) in stack.layers.iter().enumerate() {
        if !selection.contains(i) {
            continue;
        }
        let local_base_tree = trees.dep[i];
        let local_head_tree = trees.effective[i];
        let local_message = layer.message.clean_for_branch();

        let Some(pr) = &prs[i] else {
            out.push(LayerComparison {
                index: i,
                commit: layer.commit,
                subject: layer.subject().to_string(),
                pr_number: layer.pr,
                pr_state: None,
                upstream_message: None,
                local_message,
                message_differs: false,
                github_message_edited: false,
                patch_differs: false,
                upstream_compare_tree: None,
                local_compare_tree: local_head_tree,
                upstream_base_tree: None,
                upstream_head_tree: None,
                local_base_tree,
                local_head_tree,
            });
            continue;
        };

        let upstream_message = upstream_clean_message(&layer.message, pr);
        let message_differs =
            engine::pr_message_differs_from(pr, &layer.message);
        let github_message_edited = if message_differs {
            pr_was_edited_on_forge(git, &layer.message, pr)
        } else {
            false
        };

        let has_base_oid = pr.base_oid != Oid::ZERO_SHA1
            && git.repo().find_commit(pr.base_oid).is_ok();
        let remote_base_tip = if has_base_oid {
            pr.base_oid
        } else {
            match layer.dep {
                Dep::Main | Dep::ExternalPr(..) => stack.base,
                Dep::Layer(j) => {
                    prs[j].as_ref().map(|p| p.head_oid).unwrap_or(stack.base)
                }
            }
        };

        let mb = review_base(git.repo(), remote_base_tip, pr.head_oid)?;
        let upstream_base_tree = git.tree_of(mb)?;
        let upstream_head_tree = git.tree_of(pr.head_oid)?;

        let shown_patch_id =
            tree_patch_id(git.repo(), upstream_base_tree, upstream_head_tree)?;
        let desired_patch_id =
            tree_patch_id(git.repo(), local_base_tree, local_head_tree)?;
        let patch_differs = shown_patch_id != desired_patch_id;

        // When the code patch is unchanged, use `local_head_tree` for both sides
        // so that a commit-message-only change shows only `COMMIT_EDITMSG`.
        let projected_upstream_code_tree = if !patch_differs {
            Some(local_head_tree)
        } else if upstream_base_tree == local_base_tree {
            Some(upstream_head_tree)
        } else {
            let index = git.merge_trees(
                upstream_base_tree,
                local_base_tree,
                upstream_head_tree,
            )?;
            if index.has_conflicts() {
                None
            } else {
                Some(git.write_index(index)?)
            }
        };

        let (upstream_compare_tree, local_compare_tree) = if message_differs {
            let up = match projected_upstream_code_tree {
                Some(tree) => {
                    Some(insert_commit_msg_blob(git, tree, &upstream_message)?)
                }
                None => None,
            };
            let loc =
                insert_commit_msg_blob(git, local_head_tree, &local_message)?;
            (up, loc)
        } else {
            (projected_upstream_code_tree, local_head_tree)
        };

        out.push(LayerComparison {
            index: i,
            commit: layer.commit,
            subject: layer.subject().to_string(),
            pr_number: Some(pr.number),
            pr_state: Some(pr.state),
            upstream_message: Some(upstream_message),
            local_message,
            message_differs,
            github_message_edited,
            patch_differs,
            upstream_compare_tree,
            local_compare_tree,
            upstream_base_tree: Some(upstream_base_tree),
            upstream_head_tree: Some(upstream_head_tree),
            local_base_tree,
            local_head_tree,
        });
    }

    Ok(out)
}

/// Print the comparison results for one or more layers, invoking `git` to
/// render colored diffs (and paging when stdout is a TTY and `!no_pager`).
pub fn print_comparisons(
    git: &Git,
    config: &Config,
    comparisons: &[LayerComparison],
    no_pager: bool,
) -> Result<()> {
    for (idx, comp) in comparisons.iter().enumerate() {
        if idx > 0 {
            println!();
        }
        let short_commit = git
            .short_id(comp.commit)
            .unwrap_or_else(|_| comp.commit.to_string());
        let Some(number) = comp.pr_number else {
            println!(
                "{} {} \"{}\" has no upstream pull request yet (run `nspr diff` to create one).",
                style("○").green().bold(),
                style(&short_commit).dim(),
                comp.subject
            );
            continue;
        };

        let pr_link = config.pull_request_link(number, format!("#{number}"));
        if comp.pr_state.is_none() {
            println!(
                "{} {} ({}) \"{}\" was not found on GitHub.",
                style("⚠").yellow().bold(),
                style(&pr_link).bold(),
                style(&short_commit).dim(),
                comp.subject
            );
            continue;
        }

        if !comp.has_differences() {
            println!(
                "{} {} ({}) \"{}\" is identical to upstream.",
                style("●").green(),
                style(&pr_link).bold(),
                style(&short_commit).dim(),
                comp.subject
            );
            continue;
        }

        let what_changed = match (comp.message_differs, comp.patch_differs) {
            (true, true) if comp.github_message_edited => {
                "commit message (edited on GitHub) and diff differ"
            }
            (true, true) => "commit message and diff differ",
            (true, false) if comp.github_message_edited => {
                "commit message differs (edited on GitHub)"
            }
            (true, false) => "commit message differs",
            (false, true) => "diff differs",
            (false, false) => unreachable!(),
        };

        println!(
            "{} {} ({}) \"{}\" — {}",
            style("◉").yellow().bold(),
            style(&pr_link).bold(),
            style(&short_commit).dim(),
            comp.subject,
            style(what_changed).yellow()
        );
        let _ = std::io::stdout().flush();

        run_git_diff_for_layer(git, comp, no_pager)?;
    }
    Ok(())
}

fn run_git_diff_for_layer(
    git: &Git,
    comp: &LayerComparison,
    no_pager: bool,
) -> Result<()> {
    let use_pager = !no_pager && std::io::stdout().is_terminal();
    let repo_dir = git.repo().workdir().unwrap_or_else(|| git.repo().path());

    if let Some(upstream_tree) = comp.upstream_compare_tree {
        let mut cmd = Command::new("git");
        cmd.current_dir(repo_dir);
        if use_pager {
            cmd.arg("--paginate");
        } else {
            cmd.arg("--no-pager");
        }
        cmd.args([
            "diff",
            "--src-prefix=upstream/",
            "--dst-prefix=local/",
            &upstream_tree.to_string(),
            &comp.local_compare_tree.to_string(),
        ]);
        let status = cmd.status().wrap_err("failed to invoke `git diff`")?;
        if !status.success() {
            bail!("`git diff` exited with status {status}");
        }
        return Ok(());
    }

    // Fallback when 3-way projection onto the local dependency base conflicted:
    // synthesize 1-parent commits for upstream and local and invoke `git range-diff`.
    let (Some(up_base_tree), Some(up_head_tree)) =
        (comp.upstream_base_tree, comp.upstream_head_tree)
    else {
        return Ok(());
    };
    let up_msg = comp
        .upstream_message
        .as_deref()
        .unwrap_or(&comp.local_message);
    let up_base_commit = git.create_derived_commit(
        comp.commit,
        "upstream base",
        up_base_tree,
        &[],
    )?;
    let up_head_commit = git.create_derived_commit(
        comp.commit,
        up_msg,
        up_head_tree,
        &[up_base_commit],
    )?;
    let loc_base_commit = git.create_derived_commit(
        comp.commit,
        "local base",
        comp.local_base_tree,
        &[],
    )?;
    let loc_head_commit = git.create_derived_commit(
        comp.commit,
        &comp.local_message,
        comp.local_head_tree,
        &[loc_base_commit],
    )?;

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir);
    if use_pager {
        cmd.arg("--paginate");
    } else {
        cmd.arg("--no-pager");
    }
    cmd.args([
        "range-diff",
        "--creation-factor=100",
        &format!("{up_base_commit}..{up_head_commit}"),
        &format!("{loc_base_commit}..{loc_head_commit}"),
    ]);
    let status = cmd.status().wrap_err("failed to invoke `git range-diff`")?;
    if !status.success() {
        bail!("`git range-diff` exited with status {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    use crate::engine::{FixedPrompter, sync_stack};
    use crate::forge::PullRequestUpdate;
    use crate::forge::fake::FakeForge;
    use crate::testutil::TestRepo;

    fn block_on<F: std::future::Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, f)
    }

    fn set_head_to(t: &TestRepo, oid: Oid) {
        t.set_branch("work", oid);
        t.open().set_head("refs/heads/work").unwrap();
    }

    fn setup_single_pr_repo() -> (TestRepo, Git, FakeForge, Config, Oid) {
        let t = TestRepo::new();
        let base_oid = t.commit("initial", &[("base.txt", "base\n")], &[]);
        t.set_branch("main", base_oid);
        let c1 = t.commit(
            "Add feature A\n\nInitial body.\n",
            &[("base.txt", "base\n"), ("a.txt", "v1\n")],
            &[base_oid],
        );
        set_head_to(&t, c1);

        let git = Git::new(t.open());
        let forge = FakeForge::new(Rc::new(t.open()), "main", base_oid);
        let config =
            Config::new("o".into(), "r".into(), "main".into(), "tester".into());

        let mut stack = Stack::discover(&git, base_oid, "main").unwrap();
        block_on(sync_stack(
            &git,
            &forge,
            &config,
            &mut stack,
            &SyncOptions::default(),
            &FixedPrompter("init".into()),
        ))
        .unwrap();

        (t, git, forge, config, base_oid)
    }

    #[test]
    fn unchanged_layer_has_no_differences() {
        let (_t, git, forge, config, base_oid) = setup_single_pr_repo();
        let stack = Stack::discover(&git, base_oid, "main").unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::All,
        ))
        .unwrap();

        assert_eq!(comps.len(), 1);
        assert!(!comps[0].has_differences());
        assert!(!comps[0].message_differs);
        assert!(!comps[0].patch_differs);
        assert_eq!(comps[0].render_plain_diff(&git).unwrap(), "");
        print_comparisons(&git, &config, &comps, true).unwrap();
    }

    #[test]
    fn detects_commit_message_and_patch_changes() {
        let (t, git, forge, config, base_oid) = setup_single_pr_repo();
        let stack = Stack::discover(&git, base_oid, "main").unwrap();
        let pr_num = stack.layers[0].pr.unwrap();

        // Edit only the commit message locally.
        let mut msg = stack.layers[0].message.clone();
        msg.subject = "Add feature A (revised)".to_string();
        msg.body = "Updated body.".to_string();
        git.rewrite_messages(
            base_oid,
            &[(stack.layers[0].commit, msg.render())],
        )
        .unwrap();

        let stack = Stack::discover(&git, base_oid, "main").unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::one(0),
        ))
        .unwrap();
        assert!(comps[0].message_differs);
        assert!(!comps[0].github_message_edited);
        assert!(!comps[0].patch_differs);
        let msg_diff = comps[0].render_plain_diff(&git).unwrap();
        assert!(msg_diff.contains("-Add feature A"), "{msg_diff}");
        assert!(msg_diff.contains("+Add feature A (revised)"), "{msg_diff}");
        assert!(msg_diff.contains("-Initial body."), "{msg_diff}");
        assert!(msg_diff.contains("+Updated body."), "{msg_diff}");
        assert!(!msg_diff.contains("a.txt"), "{msg_diff}");

        // Also modify `a.txt` locally.
        let c1_v2 = t.commit(
            &msg.render(),
            &[("base.txt", "base\n"), ("a.txt", "v2\n")],
            &[base_oid],
        );
        set_head_to(&t, c1_v2);

        let stack = Stack::discover(&git, base_oid, "main").unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::one(0),
        ))
        .unwrap();
        assert!(comps[0].message_differs);
        assert!(comps[0].patch_differs);
        let full_diff = comps[0].render_plain_diff(&git).unwrap();
        assert!(
            full_diff.contains("+Add feature A (revised)"),
            "{full_diff}"
        );
        assert!(full_diff.contains("-v1"), "{full_diff}");
        assert!(full_diff.contains("+v2"), "{full_diff}");
        print_comparisons(&git, &config, &comps, true).unwrap();

        // Edit the PR title on GitHub and verify `github_message_edited` is set.
        block_on(forge.update_pull_request(
            pr_num,
            PullRequestUpdate {
                title: Some("Edited on GitHub".into()),
                ..Default::default()
            },
        ))
        .unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::one(0),
        ))
        .unwrap();
        assert!(comps[0].message_differs);
        assert!(comps[0].github_message_edited);
    }

    #[test]
    fn rebased_stack_projects_upstream_patch_onto_new_base_without_noise() {
        let (t, git, forge, _config, base_oid) = setup_single_pr_repo();
        let stack = Stack::discover(&git, base_oid, "main").unwrap();
        let msg_rendered = stack.layers[0].message.render();

        // Advance `main` with an unrelated file `other.txt`.
        let new_base = t.commit(
            "upstream commit",
            &[("base.txt", "base\n"), ("other.txt", "unrelated\n")],
            &[base_oid],
        );
        t.set_branch("main", new_base);

        // Rebase the layer onto `new_base` without changing `a.txt`.
        let rebased_unchanged = t.commit(
            &msg_rendered,
            &[
                ("base.txt", "base\n"),
                ("other.txt", "unrelated\n"),
                ("a.txt", "v1\n"),
            ],
            &[new_base],
        );
        set_head_to(&t, rebased_unchanged);

        let stack = Stack::discover(&git, new_base, "main").unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::one(0),
        ))
        .unwrap();
        assert!(
            !comps[0].has_differences(),
            "pure rebase onto advanced main should report no diff changes"
        );

        // Now modify `a.txt` on the rebased commit; `other.txt` must not leak into the interdiff.
        let rebased_modified = t.commit(
            &msg_rendered,
            &[
                ("base.txt", "base\n"),
                ("other.txt", "unrelated\n"),
                ("a.txt", "v2\n"),
            ],
            &[new_base],
        );
        set_head_to(&t, rebased_modified);

        let stack = Stack::discover(&git, new_base, "main").unwrap();
        let comps = block_on(compare_selection(
            &git,
            &forge,
            &stack,
            &LayerSelection::one(0),
        ))
        .unwrap();
        assert!(comps[0].patch_differs);
        let diff = comps[0].render_plain_diff(&git).unwrap();
        assert!(diff.contains("-v1"), "{diff}");
        assert!(diff.contains("+v2"), "{diff}");
        assert!(!diff.contains("other.txt"), "{diff}");
    }
}
