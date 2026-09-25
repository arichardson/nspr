//! Scenario tests driven through the fake forge.
//!
//! Each scenario builds a real commit graph in a temporary repository, runs the
//! engine against [`FakeForge`], and then asks [`crate::review_diff`] what
//! GitHub *would* display for every pull request. Because GitHub's rendering is
//! a pure function of the graph, this verifies the tool's central claim —
//! "each pull request shows exactly its own commit's changes" — without any
//! network access.

use std::future::Future;
use std::rc::Rc;

use git2::Oid;

use crate::config::Config;
use crate::engine::{
    FixedPrompter, LayerAction, LayerOutcome, SyncOptions, sync_stack,
};
use crate::forge::{Forge, fake::FakeForge};
use crate::git::Git;
use crate::guardrails;
use crate::land::{self, LandOptions, LandOutcome, land_layer};
use crate::patch_id::tree_patch_id;
use crate::refs;
use crate::review_diff;
use crate::stack::Stack;
use crate::status;
use crate::sync::{self, SyncReport};
use crate::testutil::TestRepo;
use crate::trailers::CommitMessage;

const TRUNK: &str = "main";

fn block_on<F: Future>(f: F) -> F::Output {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = tokio::task::LocalSet::new();
    local.block_on(&rt, f)
}

#[derive(Debug, Clone)]
struct LayerSpec {
    message: CommitMessage,
    /// What this layer alone changes. Kept separately from `files` so that
    /// reordering, inserting and dropping layers can recompute the chain the
    /// way an interactive rebase would.
    changes: Vec<(String, String)>,
    /// Full tree contents at this layer.
    files: Vec<(String, String)>,
}

struct World {
    t: TestRepo,
    git: Git,
    forge: FakeForge,
    config: Config,
    base_files: Vec<(String, String)>,
    base_oid: Oid,
    layers: Vec<LayerSpec>,
    /// Half-open ranges of `forge.pushes` that happened inside a `land`. Force
    /// pushes are only legitimate there.
    land_windows: Vec<(usize, usize)>,
}

fn owned(files: &[(&str, &str)]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}

fn borrowed(files: &[(String, String)]) -> Vec<(&str, &str)> {
    files
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect()
}

impl World {
    fn new(base_files: &[(&str, &str)]) -> Self {
        let t = TestRepo::new();
        let base_oid = t.commit("initial", base_files, &[]);
        t.set_branch(TRUNK, base_oid);
        {
            let repo = t.open();
            repo.set_head(&format!("refs/heads/{TRUNK}")).unwrap();
        }

        let git = Git::new(t.open());
        let forge = FakeForge::new(Rc::new(t.open()), TRUNK, base_oid);
        let config =
            Config::new("o".into(), "r".into(), TRUNK.into(), "tester".into());

        let w = Self {
            t,
            git,
            forge,
            config,
            base_files: owned(base_files),
            base_oid,
            layers: Vec::new(),
            land_windows: Vec::new(),
        };
        w.sync_worktree();
        w
    }

    /// Make the index and working tree match `HEAD`.
    ///
    /// `land` rebases the local stack for real, which needs a clean worktree;
    /// the harness builds commits straight into the object database, so
    /// without this the repository looks like every file has been deleted.
    fn sync_worktree(&self) {
        let repo = self.t.open();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.reset(head.as_object(), git2::ResetType::Hard, None)
            .unwrap();
    }

    /// Append a layer whose tree is the previous layer's plus `changes`.
    fn add_layer(&mut self, subject: &str, changes: &[(&str, &str)]) {
        self.insert_layer(self.layers.len(), subject, changes);
    }

    /// Insert a layer at `at`, pushing everything above it up a level.
    fn insert_layer(
        &mut self,
        at: usize,
        subject: &str,
        changes: &[(&str, &str)],
    ) {
        self.layers.insert(
            at,
            LayerSpec {
                message: CommitMessage {
                    subject: subject.to_string(),
                    body: String::new(),
                    trailers: Vec::new(),
                },
                changes: owned(changes),
                files: Vec::new(),
            },
        );
        self.rebuild();
    }

    /// Drop layer `i` entirely, as `git rebase -i` with the line deleted would.
    fn drop_layer(&mut self, i: usize) {
        self.layers.remove(i);
        self.rebuild();
    }

    /// Swap two adjacent layers, as reordering the lines in `rebase -i` would.
    fn swap_layers(&mut self, i: usize, j: usize) {
        self.layers.swap(i, j);
        self.rebuild();
    }

    /// Amend layer `i`. Later layers inherit the change through their trees,
    /// exactly as a local `rebase -i` would leave them.
    fn amend_layer(&mut self, i: usize, changes: &[(&str, &str)]) {
        apply(&mut self.layers[i].changes, changes);
        self.rebuild();
    }

    fn set_trailer(&mut self, i: usize, key: &str, value: &str) {
        self.layers[i].message.set(key, value);
        self.rebuild();
    }

    /// Rebuild the local commit chain from the specs.
    fn rebuild(&mut self) {
        // Recompute each layer's tree by replaying the deltas over the base.
        let mut files = self.base_files.clone();
        for layer in self.layers.iter_mut() {
            let changes = borrowed(&layer.changes);
            apply(&mut files, &changes);
            layer.files = files.clone();
        }

        let mut parent = self.base_oid;
        for layer in &self.layers {
            let files = borrowed(&layer.files);
            parent = self.t.commit(&layer.message.render(), &files, &[parent]);
        }
        self.t.set_branch(TRUNK, parent);
        self.sync_worktree();
    }

    /// Advance the trunk on the "remote" and rebase the local stack onto it.
    fn advance_trunk_and_pull(&mut self, changes: &[(&str, &str)]) {
        apply(&mut self.base_files, changes);
        let files = borrowed(&self.base_files);
        let new_trunk =
            self.t.commit("upstream change", &files, &[self.base_oid]);
        block_on(
            self.forge.push(&[crate::forge::PushSpec::fast_forward(
                TRUNK, new_trunk,
            )]),
        )
        .unwrap();
        self.base_oid = new_trunk;
        self.rebuild();
    }

    fn discover(&self) -> Stack {
        // HEAD is the trunk branch locally; the stack is everything above the
        // base commit.
        Stack::discover(&self.git, self.base_oid, TRUNK).unwrap()
    }

    fn sync(&mut self) -> Vec<LayerOutcome> {
        self.sync_with(SyncOptions::default())
    }

    fn sync_with(&mut self, opts: SyncOptions) -> Vec<LayerOutcome> {
        let mut stack = self.discover();
        let prompter = FixedPrompter("update".into());
        let outcomes = block_on(sync_stack(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
            &opts,
            &prompter,
        ))
        .unwrap();
        self.refresh_specs_from_repo();
        outcomes
    }

    fn try_sync(&mut self) -> color_eyre::eyre::Result<Vec<LayerOutcome>> {
        let mut stack = Stack::discover(&self.git, self.base_oid, TRUNK)?;
        let prompter = FixedPrompter("update".into());
        block_on(sync_stack(
            &self.git,
            &self.forge,
            &self.config,
            &mut stack,
            &SyncOptions::default(),
            &prompter,
        ))
    }

    fn land(&mut self, index: usize) -> LandOutcome {
        self.try_land(index).unwrap()
    }

    fn try_land(
        &mut self,
        index: usize,
    ) -> color_eyre::eyre::Result<LandOutcome> {
        let stack = self.discover();
        let start = self.push_count();
        let outcome = block_on(land_layer(
            &self.git,
            &self.forge,
            &self.config,
            &stack,
            index,
            &LandOptions::default(),
        ));
        self.land_windows.push((start, self.push_count()));
        let outcome = outcome?;

        // The trunk has moved and the landed layer is gone from the local
        // chain.
        self.base_oid = outcome.squash;
        self.layers.remove(index);
        self.sync_worktree();
        self.refresh_specs_from_repo();
        Ok(outcome)
    }

    /// `nspr status`: read-only, and asserted to stay that way.
    fn status(&self) -> status::StackStatus {
        let stack = self.discover();
        block_on(status::status(&self.git, &self.forge, &self.config, &stack))
            .unwrap()
    }

    /// Returns how many comments were written, which is the number tests care
    /// about: a write is a notification.
    fn update_stack_comments(&self) -> usize {
        let stack = self.discover();
        block_on(crate::stack_comment::update_all(
            &self.forge,
            &self.config,
            &stack,
        ))
        .unwrap()
    }

    fn comment_on(&self, pr: u64) -> Option<crate::forge::Comment> {
        block_on(self.forge.list_own_comments(pr))
            .unwrap()
            .into_iter()
            .next()
    }

    fn guardrails(&self) -> guardrails::Guardrails {
        let stack = self.discover();
        let trees = stack.all_trees(&self.git).unwrap();
        let prs = block_on(crate::engine::gather(&self.forge, &stack)).unwrap();
        let decision = crate::engine::decide(
            &self.git,
            &stack,
            &prs,
            &trees,
            &SyncOptions::default(),
        )
        .unwrap();
        block_on(guardrails::probe(
            &self.forge,
            &self.config,
            &stack,
            &prs,
            &decision.push,
            false,
        ))
        .unwrap()
    }

    /// `nspr close` on the layer holding pull request `number`.
    fn close(&mut self, number: u64) -> crate::close::CloseOutcome {
        let stack = self.discover();
        let index = stack
            .layers
            .iter()
            .position(|l| l.pr == Some(number))
            .unwrap();
        let outcome = block_on(crate::close::close_layer(
            &self.git,
            &self.forge,
            &self.config,
            &stack,
            index,
        ))
        .unwrap();
        self.layers.remove(index);
        self.sync_worktree();
        self.refresh_specs_from_repo();
        outcome
    }

    /// `nspr amend`, followed by the bookkeeping a real rebase would force.
    fn amend(&mut self) -> Vec<crate::amend::Amended> {
        let stack = self.discover();
        let changed =
            block_on(crate::amend::amend(&self.git, &self.forge, &stack))
                .unwrap();
        self.sync_worktree();
        self.refresh_specs_from_repo();
        changed
    }

    /// `nspr sync`, for the cases where it is expected to refuse.
    fn try_sync_trunk(&mut self) -> color_eyre::eyre::Result<SyncReport> {
        let stack = self.discover();
        block_on(sync::sync_trunk(
            &self.git,
            &self.forge,
            &self.config,
            &stack,
        ))
    }

    /// `nspr sync`: pick up the remote trunk, then repair the branches.
    fn sync_trunk(&mut self) -> SyncReport {
        let report = self.try_sync_trunk().unwrap();

        self.base_oid = report.trunk;
        // Layers whose commit became empty during the rebase have gone.
        let remaining = self.git.commits_since(self.base_oid).unwrap().len();
        while self.layers.len() > remaining {
            let landed = report.merged.first().copied();
            let i = self
                .layers
                .iter()
                .position(|l| {
                    l.message
                        .get(crate::trailers::PULL_REQUEST)
                        .and_then(crate::stack::parse_pr_ref)
                        == landed
                })
                .unwrap_or(0);
            self.layers.remove(i);
        }
        self.sync_worktree();
        self.refresh_specs_from_repo();
        report
    }

    /// Land every layer that is ready, bottom-up. This is `nspr land --all`.
    ///
    /// Invariants are checked *between* lands: by the time the last layer is
    /// gone there is nothing left to be wrong about.
    fn land_all(&mut self) -> Vec<LandOutcome> {
        let mut out = Vec::new();
        while let Some(i) = land::next_landable(&self.discover()) {
            out.push(self.land(i));
            self.assert_invariants();
        }
        out
    }

    /// Pick up trailers the engine wrote back, and any trees that moved
    /// because the stack was rebased.
    ///
    /// The per-layer deltas are re-derived here rather than carried forward:
    /// `land` and the engine's rebases move the trees underneath the harness,
    /// and a stale delta would silently resurrect changes that have already
    /// landed.
    fn refresh_specs_from_repo(&mut self) {
        self.base_files = self.files_of(self.base_oid);
        let oids = self.git.commits_since(self.base_oid).unwrap();
        assert_eq!(oids.len(), self.layers.len());
        let mut prev = self.base_files.clone();
        for (layer, oid) in self.layers.iter_mut().zip(oids) {
            layer.message =
                CommitMessage::parse(&self.git.message_of(oid).unwrap());
            layer.files = files_of_commit(&self.t.open(), oid);
            layer.changes = delta(&prev, &layer.files);
            prev = layer.files.clone();
        }
    }

    fn files_of(&self, oid: Oid) -> Vec<(String, String)> {
        files_of_commit(&self.t.open(), oid)
    }

    /// **The oracle.** For every layer, assert that what GitHub would display
    /// is exactly that layer's own patch.
    fn assert_invariants(&self) {
        let repo = self.t.open();
        let stack = self.discover();

        for (i, layer) in stack.layers.iter().enumerate() {
            let number = layer.pr.unwrap_or_else(|| {
                panic!("layer {i} ({}) has no pull request", layer.subject())
            });
            let pr = block_on(self.forge.get_pull_request(number)).unwrap();
            let base_tip = self
                .forge
                .branch(&pr.base)
                .unwrap_or_else(|| panic!("base branch {} missing", pr.base));
            let head_tip = self
                .forge
                .branch(&pr.head)
                .unwrap_or_else(|| panic!("head branch {} missing", pr.head));

            // What GitHub renders for the pull request...
            let displayed =
                review_diff::displayed_patch_id(&repo, base_tip, head_tip)
                    .unwrap();

            // ...versus the patch this layer is supposed to contribute.
            let expected_base_tree = match layer.dep {
                crate::stack::Dep::Layer(j) => {
                    stack.effective_tree(&self.git, j).unwrap()
                }
                _ => self.git.tree_of(stack.base).unwrap(),
            };
            let expected = tree_patch_id(
                &repo,
                expected_base_tree,
                stack.effective_tree(&self.git, i).unwrap(),
            )
            .unwrap();

            assert_eq!(
                displayed,
                expected,
                "PR #{number} for layer {i} ({}) displays the wrong diff",
                layer.subject()
            );
        }

        self.assert_all_branch_commits_are_linear();
    }

    /// Every commit on every open PR branch above the trunk must have
    /// strictly 1 parent (zero merge commits) so GitHub's native "Merge full
    /// stack" and "Rebase stack" buttons work.
    fn assert_all_branch_commits_are_linear(&self) {
        let repo = self.t.open();
        let stack = self.discover();
        for layer in &stack.layers {
            let Some(number) = layer.pr else { continue };
            let pr = block_on(self.forge.get_pull_request(number)).unwrap();
            let mut cur = self.forge.branch(&pr.head).unwrap();
            while !self.git.is_ancestor(cur, self.base_oid).unwrap() {
                let commit = repo.find_commit(cur).unwrap();
                assert_eq!(
                    commit.parent_count(),
                    1,
                    "PR #{number} ({}) contains non-linear commit {cur} with {} parents",
                    pr.head,
                    commit.parent_count()
                );
                cur = commit.parent_id(0).unwrap();
            }
        }
    }

    fn pr_numbers(&self) -> Vec<u64> {
        self.discover()
            .layers
            .iter()
            .map(|l| l.pr.unwrap())
            .collect()
    }

    fn list(&self) -> Vec<crate::list::PrStack> {
        let prs = block_on(self.forge.list_pull_requests(None)).unwrap();
        crate::list::build_stacks(prs, TRUNK)
    }

    fn patch(
        &mut self,
        number: u64,
        branch_override: Option<&str>,
        no_checkout: bool,
    ) -> color_eyre::eyre::Result<crate::patch::PatchOutcome> {
        let outcome = block_on(crate::patch::patch_layer(
            &self.git,
            &self.forge,
            &self.config,
            self.base_oid,
            number,
            branch_override,
            no_checkout,
        ))?;
        if outcome.checked_out {
            self.sync_worktree();
        }
        Ok(outcome)
    }

    fn push_count(&self) -> usize {
        self.forge.pushes.borrow().len()
    }
}

/// Top-level blobs of a commit's tree, as the harness's file specs.
fn files_of_commit(repo: &git2::Repository, oid: Oid) -> Vec<(String, String)> {
    let tree = repo.find_commit(oid).unwrap().tree().unwrap();
    tree.iter()
        .filter_map(|entry| {
            let name = entry.name().ok()?.to_string();
            let blob = repo.find_blob(entry.id()).ok()?;
            Some((name, String::from_utf8_lossy(blob.content()).into_owned()))
        })
        .collect()
}

fn apply(files: &mut Vec<(String, String)>, changes: &[(&str, &str)]) {
    for (path, content) in changes {
        match files.iter_mut().find(|(p, _)| p == path) {
            Some(entry) => entry.1 = content.to_string(),
            None => files.push((path.to_string(), content.to_string())),
        }
    }
}

/// The inverse of [`apply`]: what `cur` adds to or changes in `prev`.
///
/// Deletions are not represented, because `apply` cannot express them either;
/// no scenario needs a layer that removes a file.
fn delta(
    prev: &[(String, String)],
    cur: &[(String, String)],
) -> Vec<(String, String)> {
    cur.iter()
        .filter(|(path, content)| {
            prev.iter().find(|(p, _)| p == path).map(|(_, c)| c)
                != Some(content)
        })
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Scenario 1: create a linear stack
// ---------------------------------------------------------------------------

#[test]
fn scenario_1_create_three_layer_stack() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);

    let outcomes = w.sync();
    assert_eq!(outcomes.len(), 3);
    assert!(outcomes.iter().all(|o| o.action == LayerAction::Created));

    // Native stacking: each PR's base is the branch below it.
    assert_eq!(outcomes[0].base, TRUNK);
    assert_eq!(outcomes[1].base, outcomes[0].branch);
    assert_eq!(outcomes[2].base, outcomes[1].branch);

    // Branch naming follows users/<login>/<slug>.
    assert_eq!(outcomes[0].branch, "users/tester/layer-one");

    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Scenario 2: amending the bottom layer must not touch the layers above
// ---------------------------------------------------------------------------

#[test]
fn scenario_2_amend_bottom_layer_only_pushes_that_layer() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let before = w.push_count();
    w.amend_layer(0, &[("a.txt", "a2")]);
    let outcomes = w.sync();

    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(
        outcomes[1].action,
        LayerAction::Skipped,
        "layer 2's patch did not change, so it must not be pushed"
    );
    assert_eq!(outcomes[2].action, LayerAction::Skipped);
    assert_eq!(
        w.push_count() - before,
        1,
        "exactly one push expected: CI and approvals on upper layers are \
         preserved"
    );
    assert!(
        !w.forge.pushes.borrow().last().unwrap().force,
        "amending a layer whose base has not moved must be a fast-forward push"
    );

    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Scenario 3: amending a middle layer
// ---------------------------------------------------------------------------

#[test]
fn scenario_3_amend_middle_layer() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let before = w.push_count();
    w.amend_layer(1, &[("b.txt", "b2")]);
    let outcomes = w.sync();

    assert_eq!(outcomes[0].action, LayerAction::Skipped);
    assert_eq!(outcomes[1].action, LayerAction::Updated);
    assert_eq!(outcomes[2].action, LayerAction::Skipped);
    assert_eq!(w.push_count() - before, 1);
    assert!(
        !w.forge.pushes.borrow().last().unwrap().force,
        "amending a middle layer whose base has not moved must be a fast-forward push"
    );

    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Scenario 4: rebase the whole stack onto an advanced trunk
// ---------------------------------------------------------------------------

#[test]
fn scenario_4_rebase_stack_onto_advanced_trunk() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    w.assert_invariants();

    // Someone else lands an unrelated change on the trunk.
    w.advance_trunk_and_pull(&[("other.txt", "upstream")]);
    w.sync_with(SyncOptions {
        sync_all: true,
        ..Default::default()
    });

    // Crucially the pull requests still show only their own changes, not the
    // unrelated upstream commit.
    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Scenario 13: diamond DAG — two layers sharing one dependency
// ---------------------------------------------------------------------------

#[test]
fn scenario_13_diamond_stack() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let prs = w.pr_numbers();
    // Declare that layer three only needs layer one.
    w.set_trailer(2, crate::trailers::DEPENDS_ON, &format!("#{}", prs[0]));
    let outcomes = w.sync();

    // Layer three now targets layer one's branch, not layer two's.
    assert_eq!(outcomes[2].base, outcomes[0].branch);
    assert!(outcomes[2].retargeted);

    // And its diff drops layer two's changes entirely.
    w.assert_invariants();

    let repo = w.t.open();
    let pr3 = block_on(w.forge.get_pull_request(prs[2])).unwrap();
    let base_tip = w.forge.branch(&pr3.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr3.head_oid).unwrap();
    assert!(text.contains("c.txt"), "should show its own file");
    assert!(
        !text.contains("b.txt"),
        "must not show layer two's changes:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 16: forward references are rejected
// ---------------------------------------------------------------------------

#[test]
fn scenario_16_forward_reference_is_rejected() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    let prs = w.pr_numbers();
    // Layer one declares a dependency on layer two, which comes later.
    w.set_trailer(0, crate::trailers::DEPENDS_ON, &format!("#{}", prs[1]));

    let err = w.try_sync().unwrap_err().to_string();
    assert!(
        err.contains("later in the stack"),
        "expected a forward-reference error, got: {err}"
    );
}

// ---------------------------------------------------------------------------
// Depends-On: main makes a layer independent
// ---------------------------------------------------------------------------

#[test]
fn depends_on_main_targets_the_trunk() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    w.set_trailer(1, crate::trailers::DEPENDS_ON, "main");
    let outcomes = w.sync();

    assert_eq!(outcomes[1].base, TRUNK);
    w.assert_invariants();

    let repo = w.t.open();
    let prs = w.pr_numbers();
    let pr2 = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    let base_tip = w.forge.branch(&pr2.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr2.head_oid).unwrap();
    assert!(text.contains("b.txt"));
    assert!(
        !text.contains("a.txt"),
        "a cherry-picked layer must not show its former parent's changes:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Re-running with no local changes is a complete no-op
// ---------------------------------------------------------------------------

#[test]
fn resync_without_changes_pushes_nothing() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    let before = w.push_count();
    let outcomes = w.sync();
    assert!(outcomes.iter().all(|o| o.action == LayerAction::Skipped));
    assert_eq!(w.push_count(), before);
    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// "Behind" only forces a refresh under strict required checks
// ---------------------------------------------------------------------------

/// In a stack, every layer above an amended one is "behind" by construction,
/// and its displayed diff is still correct. Refreshing on that unconditionally
/// would push every layer on every amend — defeating the patch-id gate and
/// discarding CI results and approvals. So it is gated on the repository
/// actually requiring up-to-date branches.
#[test]
fn behind_layers_refresh_only_when_the_repo_requires_up_to_date_branches() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    *w.forge.protection.borrow_mut() = Some(crate::forge::Protection {
        require_up_to_date: true,
        ..Default::default()
    });

    w.amend_layer(0, &[("a.txt", "a2")]);

    // Without the flag, the upper layer is left alone.
    let outcomes = w.sync_with(SyncOptions::default());
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Skipped);

    // With it, the upper layer is refreshed so it can actually be merged.
    w.amend_layer(0, &[("a.txt", "a3")]);
    let outcomes = w.sync_with(SyncOptions {
        refresh_when_behind: true,
        ..Default::default()
    });
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Refreshed);

    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Pushing a layer drags its stale dependencies along with it
// ---------------------------------------------------------------------------

/// The patch-id gate says "only push layers whose patch changed", but that is
/// not quite sufficient. Pushing layer `i` appends a commit whose second parent
/// is the dependency's remote tip, so GitHub renders
/// `diff(tree(dep_tip), effective(i))`. If `dep_tip` is stale, the dependency's
/// un-pushed changes leak into layer `i`'s diff.
///
/// Here layers one and three change and layer two does not. Layer two must be
/// restacked anyway — but as a `Refreshed`, with no prompt, because its own
/// patch is untouched.
#[test]
fn pushing_a_layer_restacks_its_stale_dependencies() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    w.amend_layer(0, &[("a.txt", "a2")]);
    w.amend_layer(2, &[("c.txt", "c2")]);
    let outcomes = w.sync();

    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(
        outcomes[1].action,
        LayerAction::Refreshed,
        "layer two's own patch is unchanged, but layer three cannot be \
         pushed correctly until layer two carries layer one's new content"
    );
    assert_eq!(outcomes[2].action, LayerAction::Updated);

    // Without the restack, layer three's diff would also show `a.txt`.
    w.assert_invariants();

    let repo = w.t.open();
    let prs = w.pr_numbers();
    let pr3 = block_on(w.forge.get_pull_request(prs[2])).unwrap();
    let base_tip = w.forge.branch(&pr3.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr3.head_oid).unwrap();
    assert!(text.contains("c.txt"));
    assert!(
        !text.contains("a.txt"),
        "layer one's change leaked into layer three's diff:\n{text}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 5: squash-land the bottom layer
// ---------------------------------------------------------------------------

#[test]
fn scenario_5_land_bottom_layer_repairs_the_rest() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    // A second revision, so there is a revision chain worth preserving.
    w.amend_layer(1, &[("b.txt", "b2")]);
    w.sync();

    let prs = w.pr_numbers();
    let repo = w.t.open();
    let before: Vec<Oid> = prs[1..]
        .iter()
        .map(|n| {
            let pr = block_on(w.forge.get_pull_request(*n)).unwrap();
            let base = w.forge.branch(&pr.base).unwrap();
            review_diff::displayed_patch_id(&repo, base, pr.head_oid).unwrap()
        })
        .collect();

    let landed = w.land(0);
    assert_eq!(landed.number, prs[0]);
    assert!(landed.warnings.is_empty(), "{:?}", landed.warnings);

    // Only the direct dependent is retargeted; the layer above it keeps its
    // base, because that branch still exists.
    assert_eq!(landed.repaired.len(), 2);
    assert!(landed.repaired[0].retargeted);
    assert!(!landed.repaired[1].retargeted);
    assert!(landed.repaired.iter().all(|r| !r.collapsed));

    // The squash commit carries the local commit's message, not the branch's
    // `[nspr]` revision noise.
    let squash_message = w.git.message_of(landed.squash).unwrap();
    assert!(squash_message.starts_with("Layer one (#101)"));
    assert!(
        !squash_message.contains("[nspr]"),
        "revision noise reached the trunk:\n{squash_message}"
    );

    // The landed branch is gone, and nothing else was closed.
    assert!(!w.forge.branch_exists("users/tester/layer-one"));
    for n in &prs[1..] {
        let pr = block_on(w.forge.get_pull_request(*n)).unwrap();
        assert_eq!(pr.state, crate::forge::PrState::Open);
    }

    // The point of the exercise: the surviving pull requests display exactly
    // what they displayed before, so inline comments re-anchor.
    let after: Vec<Oid> = prs[1..]
        .iter()
        .map(|n| {
            let pr = block_on(w.forge.get_pull_request(*n)).unwrap();
            let base = w.forge.branch(&pr.base).unwrap();
            review_diff::displayed_patch_id(&repo, base, pr.head_oid).unwrap()
        })
        .collect();
    assert_eq!(before, after, "landing changed a dependent's diff");

    w.assert_invariants();

    // And re-syncing afterwards is a no-op: the repair was complete.
    let outcomes = w.sync();
    assert!(
        outcomes.iter().all(|o| o.action == LayerAction::Skipped),
        "{outcomes:?}"
    );
}

// ---------------------------------------------------------------------------
// Scenario 6: land the whole stack
// ---------------------------------------------------------------------------

#[test]
fn scenario_6_land_all() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let landed = w.land_all();
    assert_eq!(landed.len(), 3);
    assert!(
        w.discover().layers.is_empty(),
        "local stack should be empty"
    );

    // Every layer's content is on the trunk, in order.
    let files = w.files_of(w.base_oid);
    for (name, content) in [("a.txt", "a1"), ("b.txt", "b1"), ("c.txt", "c1")] {
        assert!(
            files.contains(&(name.to_string(), content.to_string())),
            "{name} missing from the trunk: {files:?}"
        );
    }

    // Each landed as its own squash commit, rather than being flattened
    // together.
    let subjects: Vec<String> = landed
        .iter()
        .map(|l| w.git.message_of(l.squash).unwrap())
        .map(|m| m.lines().next().unwrap().to_string())
        .collect();
    assert_eq!(
        subjects,
        vec!["Layer one (#101)", "Layer two (#102)", "Layer three (#103)"]
    );

    // Every head branch cleaned up.
    for branch in ["layer-one", "layer-two", "layer-three"] {
        assert!(
            !w.forge.branch_exists(&format!("users/tester/{branch}")),
            "{branch} was not deleted"
        );
    }
    w.assert_all_branch_commits_are_linear();
}

// ---------------------------------------------------------------------------
// Scenario 14: landing the shared dependency of a diamond
// ---------------------------------------------------------------------------

#[test]
fn scenario_14_land_shared_dependency_of_a_diamond() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let prs = w.pr_numbers();
    // Layer three needs only layer one, so the two upper layers are siblings.
    w.set_trailer(2, crate::trailers::DEPENDS_ON, &format!("#{}", prs[0]));
    w.sync();

    let landed = w.land(0);

    // Both siblings are direct dependents, so both retarget at the trunk.
    assert_eq!(landed.repaired.len(), 2);
    assert!(landed.repaired.iter().all(|r| r.retargeted));
    assert!(landed.warnings.is_empty(), "{:?}", landed.warnings);

    w.assert_invariants();

    // Neither sibling picked up the other's changes during the repair.
    let repo = w.t.open();
    for (number, own, foreign) in
        [(prs[1], "b.txt", "c.txt"), (prs[2], "c.txt", "b.txt")]
    {
        let pr = block_on(w.forge.get_pull_request(number)).unwrap();
        let base = w.forge.branch(&pr.base).unwrap();
        let text =
            review_diff::displayed_patch(&repo, base, pr.head_oid).unwrap();
        assert!(text.contains(own), "#{number} lost {own}:\n{text}");
        assert!(
            !text.contains(foreign),
            "#{number} picked up {foreign}:\n{text}"
        );
        assert!(
            !text.contains("a.txt"),
            "#{number} still shows the landed layer:\n{text}"
        );
    }
}

// ---------------------------------------------------------------------------
// Scenario 15: land an independent layer out of order — the DAG payoff
// ---------------------------------------------------------------------------

/// `Layer three` is declared independent, so it can land while `Layer two` is
/// still in review, even though it sits above it locally. Nothing about layers
/// one and two may move.
#[test]
fn scenario_15_land_an_independent_layer_out_of_order() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.add_layer("Layer four", &[("d.txt", "d1")]);
    w.sync();

    let prs = w.pr_numbers();
    w.set_trailer(2, crate::trailers::DEPENDS_ON, "main");
    w.set_trailer(3, crate::trailers::DEPENDS_ON, &format!("#{}", prs[2]));
    w.sync();
    w.assert_invariants();

    let repo = w.t.open();
    let untouched: Vec<Oid> = prs[..2]
        .iter()
        .map(|n| block_on(w.forge.get_pull_request(*n)).unwrap().head_oid)
        .collect();

    let landed = w.land(2);
    assert_eq!(landed.number, prs[2]);
    assert!(landed.warnings.is_empty(), "{:?}", landed.warnings);

    // Only layer four depended on it.
    assert_eq!(landed.repaired.len(), 1);
    assert_eq!(landed.repaired[0].number, prs[3]);
    assert!(landed.repaired[0].retargeted);

    // Layers one and two were not pushed at all.
    for (n, tip) in prs[..2].iter().zip(&untouched) {
        let pr = block_on(w.forge.get_pull_request(*n)).unwrap();
        assert_eq!(pr.head_oid, *tip, "#{n} should not have moved");
    }
    // Layer two is still stacked on layer one, untouched by the land.
    let pr1 = block_on(w.forge.get_pull_request(prs[0])).unwrap();
    let pr2 = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    assert_eq!(pr2.base, pr1.head);

    w.assert_invariants();

    // Layer four now shows only its own change, against the trunk.
    let pr4 = block_on(w.forge.get_pull_request(prs[3])).unwrap();
    assert_eq!(pr4.base, TRUNK);
    let base = w.forge.branch(&pr4.base).unwrap();
    let text = review_diff::displayed_patch(&repo, base, pr4.head_oid).unwrap();
    assert!(text.contains("d.txt"));
    for other in ["a.txt", "b.txt", "c.txt"] {
        assert!(!text.contains(other), "#{} shows {other}:\n{text}", prs[3]);
    }
}

// ---------------------------------------------------------------------------
// Scenario 17: a dependency merges out of band
// ---------------------------------------------------------------------------

/// `Layer three` declares `Depends-On: #101`. Somebody squash-merges `#101`
/// through the GitHub UI while `Layer two` is still in review.
///
/// The trap: after `#101`'s commit drops out of the local stack, `Layer three`
/// becomes the second layer, so the *default* rule ("stack on the previous
/// layer") would silently put it on top of `Layer two` — a dependency its
/// author explicitly disclaimed. It must resolve to the trunk instead, and the
/// trailer must be rewritten to say so, so the question is never asked again.
#[test]
fn scenario_17_dependency_merged_out_of_band_resolves_to_the_trunk() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let prs = w.pr_numbers();
    w.set_trailer(2, crate::trailers::DEPENDS_ON, &format!("#{}", prs[0]));
    w.sync();
    w.assert_invariants();

    // Somebody merges #101 in the GitHub UI, bypassing `nspr land` entirely.
    w.forge.external_squash_merge(prs[0]).unwrap();

    // Guardrail G3 notices, by state rather than by sha.
    let report = w.sync_trunk();
    assert_eq!(report.merged, vec![prs[0]]);
    assert!(report.stranded.is_empty(), "{:?}", report.stranded);
    assert!(report.rebased);

    let stack = w.discover();
    assert_eq!(
        stack.layers.len(),
        2,
        "the landed layer should have dropped"
    );
    assert_eq!(stack.layers[0].subject(), "Layer two");
    assert_eq!(stack.layers[1].subject(), "Layer three");

    // The load-bearing assertion: layer three is *not* re-stacked onto layer
    // two just because it now happens to precede it. At this point the
    // reference is still unresolved — `Stack::discover` cannot know #101's
    // fate without asking the forge — but it is emphatically not `Layer(0)`.
    assert_eq!(stack.layers[1].dep, crate::stack::Dep::ExternalPr(prs[0]));

    let outcomes = w.sync();
    assert_eq!(outcomes[1].base, TRUNK);
    w.assert_invariants();

    // The stale `#101` has been rewritten, so the forge is never consulted
    // about it again.
    let stack = w.discover();
    assert_eq!(
        stack.layers[1].message.get(crate::trailers::DEPENDS_ON),
        Some(TRUNK)
    );
    assert_eq!(stack.layers[1].dep, crate::stack::Dep::Main);

    // And it still shows only its own change.
    let repo = w.t.open();
    let pr = block_on(w.forge.get_pull_request(prs[2])).unwrap();
    let base = w.forge.branch(&pr.base).unwrap();
    let text = review_diff::displayed_patch(&repo, base, pr.head_oid).unwrap();
    assert!(text.contains("c.txt"));
    for other in ["a.txt", "b.txt"] {
        assert!(!text.contains(other), "#{} shows {other}:\n{text}", prs[2]);
    }
}

// ---------------------------------------------------------------------------
// `Depends-On:` is canonicalised on write
// ---------------------------------------------------------------------------

/// Permissive on input, canonical on storage. Whatever the author types is
/// rewritten to `#N` or the trunk's name, so later runs — and human readers —
/// never have to re-derive what a URL or an abbreviated hash pointed at.
#[test]
fn depends_on_is_canonicalised_on_write() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    let prs = w.pr_numbers();
    // A full URL, and a spelling of the trunk that is not its branch name.
    w.set_trailer(
        2,
        crate::trailers::DEPENDS_ON,
        &format!("https://github.com/o/r/pull/{}", prs[0]),
    );
    w.set_trailer(1, crate::trailers::DEPENDS_ON, "master");
    w.sync();

    let stack = w.discover();
    assert_eq!(
        stack.layers[1].message.get(crate::trailers::DEPENDS_ON),
        Some(TRUNK)
    );
    assert_eq!(
        stack.layers[2].message.get(crate::trailers::DEPENDS_ON),
        Some(format!("#{}", prs[0]).as_str())
    );

    w.assert_invariants();

    // Canonicalising must not itself look like a change worth pushing.
    let before = w.push_count();
    let outcomes = w.sync();
    assert!(outcomes.iter().all(|o| o.action == LayerAction::Skipped));
    assert_eq!(w.push_count(), before);
}

// ---------------------------------------------------------------------------
// `nspr status`
// ---------------------------------------------------------------------------

#[test]
fn status_reports_what_diff_would_do() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);

    // Before the first sync, nothing exists yet.
    let s = w.status();
    assert!(s.layers.iter().all(|l| l.state == status::LayerState::New));
    assert!(s.layers.iter().all(|l| l.number.is_none()));

    w.sync();
    let s = w.status();
    assert!(
        s.layers
            .iter()
            .all(|l| l.state == status::LayerState::Current),
        "{:#?}",
        s.layers
    );
    // Only the bottom layer can land.
    assert_eq!(
        s.layers.iter().map(|l| l.landable).collect::<Vec<_>>(),
        vec![true, false, false]
    );

    // Amend the bottom and the top. Status must agree with the engine,
    // including the restack of the middle layer that the patch-id gate alone
    // would not predict.
    w.amend_layer(0, &[("a.txt", "a2")]);
    w.amend_layer(2, &[("c.txt", "c2")]);
    let s = w.status();
    assert_eq!(
        s.layers.iter().map(|l| l.state.clone()).collect::<Vec<_>>(),
        vec![
            status::LayerState::Modified,
            status::LayerState::NeedsRestack,
            status::LayerState::Modified,
        ]
    );

    // Status must not have touched the remote.
    let before = w.push_count();
    w.status();
    assert_eq!(w.push_count(), before);

    // And it agrees with what the sync then actually does.
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Refreshed);
    assert_eq!(outcomes[2].action, LayerAction::Updated);
}

#[test]
fn status_reports_a_merged_layer_rather_than_failing() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    let prs = w.pr_numbers();
    w.forge.external_squash_merge(prs[0]).unwrap();

    // `diff` refuses, because pushing to a merged pull request is pointless...
    let err = w.try_sync().unwrap_err().to_string();
    assert!(err.contains("already been merged"), "{err}");

    // ...but `status` is how you find that out, so it must not refuse.
    let s = w.status();
    assert_eq!(s.layers[0].state, status::LayerState::Merged);
}

// ---------------------------------------------------------------------------
// Local mirror refs
// ---------------------------------------------------------------------------

#[test]
fn head_commits_stay_reachable_through_local_refs() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    let prs = w.pr_numbers();
    assert_eq!(refs::all(&w.git).unwrap(), prs);

    for n in &prs {
        let pr = block_on(w.forge.get_pull_request(*n)).unwrap();
        let oid = w.git.resolve_reference(&refs::ref_name(*n)).unwrap();
        assert_eq!(oid, pr.head_oid, "ref for #{n} is stale");
    }

    // Landing retires the ref for the landed pull request and moves the one
    // for the branch it rewrote.
    let landed = w.land(0);
    assert_eq!(refs::all(&w.git).unwrap(), vec![prs[1]]);
    assert_eq!(
        w.git.resolve_reference(&refs::ref_name(prs[1])).unwrap(),
        landed.repaired[0].new_tip
    );
}

// ---------------------------------------------------------------------------
// Guardrails
// ---------------------------------------------------------------------------

/// `dismiss_stale_reviews` lives on the branch being merged *into*, so in a
/// stack it normally only affects the bottom layer: the layers above target
/// topic branches, which are rarely protected.
#[test]
fn approval_dismissal_is_warned_about_per_base_branch() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    w.forge.protections.borrow_mut().insert(
        TRUNK.to_string(),
        crate::forge::Protection {
            dismiss_stale_reviews: true,
            ..Default::default()
        },
    );

    // Amend both layers so both are due a push.
    w.amend_layer(0, &[("a.txt", "a2")]);
    w.amend_layer(1, &[("b.txt", "b2")]);

    let g = w.guardrails();
    let prs = w.pr_numbers();
    assert_eq!(
        g.warnings.len(),
        1,
        "only the layer based on the trunk is affected: {:?}",
        g.warnings
    );
    assert!(g.warnings[0].contains(&format!("#{}", prs[0])));
    assert!(!g.refresh_when_behind);
}

#[test]
fn auto_merge_on_a_stacked_layer_is_warned_about() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    let prs = w.pr_numbers();
    // On the bottom layer this is harmless: its base *is* the trunk.
    w.forge.set_auto_merge(prs[0], true);
    assert!(w.guardrails().warnings.is_empty());

    // On a stacked layer it would merge into the layer below.
    w.forge.set_auto_merge(prs[1], true);
    let g = w.guardrails();
    assert_eq!(g.warnings.len(), 1);
    assert!(g.warnings[0].contains("flattening"), "{:?}", g.warnings);
}

// ---------------------------------------------------------------------------
// Stack comments
// ---------------------------------------------------------------------------

#[test]
fn every_layer_gets_a_stack_comment_pointing_at_itself() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    assert_eq!(w.update_stack_comments(), 3);

    let prs = w.pr_numbers();
    for (i, pr) in prs.iter().enumerate() {
        let body = w.comment_on(*pr).expect("a comment").body;
        // Every layer appears on every comment: the point is to show a
        // reviewer landing on #102 what the rest of the stack is.
        for other in &prs {
            assert!(body.contains(&format!("#{other}")), "{body}");
        }
        // Exactly one arrow, and it is at the start of this layer's own line.
        assert_eq!(body.matches("➡️").count(), 1, "{body}");
        let marked = body
            .lines()
            .find(|l| l.contains("➡️"))
            .expect("a marked line");
        assert!(marked.contains(&format!("**#{}", prs[i])), "{marked}");
    }
}

/// Every comment edit notifies everyone subscribed to the pull request. A tool
/// that rewrote the table on every `nspr diff` would be muted within a week.
#[test]
fn an_unchanged_stack_rewrites_no_comments() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    assert_eq!(w.update_stack_comments(), 2);
    assert_eq!(w.update_stack_comments(), 0);
    assert_eq!(*w.forge.comment_updates.borrow(), 0);
}

#[test]
fn a_new_layer_updates_the_existing_comments() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    w.update_stack_comments();

    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    // Two rewrites and one fresh comment.
    assert_eq!(w.update_stack_comments(), 3);
    assert_eq!(*w.forge.comment_updates.borrow(), 2);

    let prs = w.pr_numbers();
    let body = w.comment_on(prs[0]).unwrap().body;
    assert!(body.contains(&format!("#{}", prs[2])), "{body}");
}

/// People reply inside these comments. Quietly eating an edit is the sort of
/// thing that makes a tool untrustworthy.
#[test]
fn text_a_human_added_around_the_table_survives() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    w.update_stack_comments();

    let prs = w.pr_numbers();
    let comment = w.comment_on(prs[0]).unwrap();
    let edited =
        format!("Please review the bottom one first.\n\n{}", comment.body);
    block_on(w.forge.update_comment(comment.id, &edited)).unwrap();

    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    w.update_stack_comments();

    let body = w.comment_on(prs[0]).unwrap().body;
    assert!(
        body.starts_with("Please review the bottom one first."),
        "{body}"
    );
    let third = w.pr_numbers()[2];
    assert!(body.contains(&format!("#{third}")), "{body}");
}

/// Siblings that merely happen to be adjacent in the local commit order must
/// not be drawn as though one depends on the other.
#[test]
fn the_comment_renders_a_diamond_as_a_tree() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Base layer", &[("a.txt", "a1")]);
    w.add_layer("Left layer", &[("b.txt", "b1")]);
    w.add_layer("Right layer", &[("c.txt", "c1")]);
    w.sync();

    // Declare the divergence only now: the trailer has to name a pull request
    // that exists.
    let prs = w.pr_numbers();
    w.set_trailer(2, crate::trailers::DEPENDS_ON, &format!("#{}", prs[0]));
    w.sync();
    w.update_stack_comments();

    let body = w.comment_on(prs[0]).unwrap().body;
    let indent = |needle: &str| -> usize {
        let line = body
            .lines()
            .find(|l| l.contains(needle))
            .unwrap_or_else(|| panic!("no line for {needle}: {body}"));
        line.len() - line.trim_start().len()
    };

    // Both siblings sit one level below the layer they share, and level with
    // each other.
    let base = indent(&format!("#{}", prs[0]));
    assert_eq!(indent(&format!("#{}", prs[1])), base + 2);
    assert_eq!(indent(&format!("#{}", prs[2])), base + 2);
}

// ---------------------------------------------------------------------------
// Scenarios 7-9: the stack changes shape
// ---------------------------------------------------------------------------

/// Reordering two layers must retarget both pull requests, not rewrite them.
///
/// The `Pull-Request:` trailer travels with the commit, so a swap moves the
/// pull requests too: #103 is now the lower of the pair and must be based on
/// #101, with #102 sitting on top of it.
#[test]
fn scenario_7_reorder_two_layers_retargets_their_bases() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    let first = w.sync();
    let prs = w.pr_numbers();

    w.swap_layers(1, 2);
    let outcomes = w.sync();

    // The pull requests travelled with their commits.
    assert_eq!(outcomes[1].number, prs[2]);
    assert_eq!(outcomes[2].number, prs[1]);

    // ...and their bases were swapped to match.
    assert_eq!(outcomes[0].base, TRUNK);
    assert_eq!(outcomes[1].base, first[0].branch);
    assert_eq!(outcomes[2].base, first[2].branch);
    assert!(outcomes[1].retargeted);
    assert!(outcomes[2].retargeted);

    // Neither branch was rewritten to achieve it.
    w.assert_all_branch_commits_are_linear();
    w.assert_invariants();
}

/// Inserting a layer must open one new pull request and retarget exactly the
/// one directly above it. The layers below are untouched.
#[test]
fn scenario_8_insert_a_layer_mid_stack() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    let first = w.sync();
    let before = w.push_count();

    w.insert_layer(1, "Inserted layer", &[("mid.txt", "m1")]);
    let outcomes = w.sync();

    assert_eq!(outcomes.len(), 3);
    assert_eq!(outcomes[0].number, first[0].number);
    assert_eq!(outcomes[0].action, LayerAction::Skipped);
    assert_eq!(outcomes[1].action, LayerAction::Created);
    assert_eq!(outcomes[2].number, first[1].number);

    // The new layer slots in between.
    assert_eq!(outcomes[1].base, first[0].branch);
    assert_eq!(outcomes[2].base, outcomes[1].branch);
    assert!(outcomes[2].retargeted);

    // Layer one's patch did not change, so its branch did not move.
    assert_eq!(
        w.forge
            .pushes
            .borrow()
            .iter()
            .skip(before)
            .filter(|p| p.branch == first[0].branch)
            .count(),
        0
    );
    w.assert_all_branch_commits_are_linear();
    w.assert_invariants();
}

/// Dropping a layer leaves its pull request open — abandoning review work
/// silently would be worse than leaving a tab open — but the layer above must
/// be retargeted and must stop showing the dropped changes.
#[test]
fn scenario_9_drop_a_layer() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    let first = w.sync();
    let prs = w.pr_numbers();

    w.drop_layer(1);
    let outcomes = w.sync();

    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[1].number, prs[2]);
    assert_eq!(outcomes[1].base, first[0].branch);
    assert!(outcomes[1].retargeted);

    // The orphan is still open. nspr does not close pull requests behind the
    // author's back.
    let orphan = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    assert_eq!(orphan.state, crate::forge::PrState::Open);

    // Layer three's displayed diff has lost the dropped layer's file.
    let repo = w.t.open();
    let pr3 = block_on(w.forge.get_pull_request(prs[2])).unwrap();
    let base_tip = w.forge.branch(&pr3.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr3.head_oid).unwrap();
    assert!(text.contains("c.txt"));
    assert!(!text.contains("b.txt"), "dropped layer leaked:\n{text}");

    w.assert_all_branch_commits_are_linear();
    w.assert_invariants();
}

// ---------------------------------------------------------------------------
// Scenario 11: somebody merges through the GitHub UI
// ---------------------------------------------------------------------------

/// The squash commit shares no sha with anything local, so detection has to be
/// by pull request *state*. Having detected it, `sync` drops the landed commit
/// and restacks the rest.
#[test]
fn scenario_11_external_squash_merge_is_detected_and_repaired() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    w.forge.external_squash_merge(prs[0]).unwrap();

    let report = w.sync_trunk();
    assert_eq!(report.merged, vec![prs[0]]);
    assert!(report.stranded.is_empty(), "{:?}", report.stranded);
    assert!(report.rebased);

    let stack = w.discover();
    assert_eq!(stack.layers.len(), 2);
    assert_eq!(stack.layers[0].subject(), "Layer two");

    // Layer two now sits on the trunk, and the whole stack is consistent again
    // after one ordinary sync.
    let outcomes = w.sync();
    assert_eq!(outcomes[0].base, TRUNK);
    assert_eq!(outcomes[0].number, prs[1]);
    w.assert_invariants();
}

/// Adding something to a layer after somebody merged it leaves changes that
/// exist only locally. Dropping the commit would destroy them, so `sync` keeps
/// it and says so.
#[test]
fn scenario_11b_a_layer_amended_after_it_merged_is_reported_as_stranded() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    w.forge.external_squash_merge(prs[0]).unwrap();
    // The author had already moved on and added to the layer locally. This
    // applies cleanly on top of the squash, so the rebase leaves a commit
    // behind rather than failing.
    w.amend_layer(0, &[("a-extra.txt", "extra")]);

    let report = w.sync_trunk();
    assert_eq!(report.merged, vec![prs[0]]);
    assert_eq!(
        report.stranded,
        vec![prs[0]],
        "the local change must not be silently discarded"
    );
    assert!(
        w.discover()
            .layers
            .iter()
            .any(|l| l.subject() == "Layer one"),
        "the amended commit must survive the rebase"
    );
}

// ---------------------------------------------------------------------------
// Scenario 12: a conflict must stop everything
// ---------------------------------------------------------------------------

/// If the local stack cannot be replayed onto the new trunk, the only safe
/// thing is to stop. A partial repair would leave some pull requests restacked
/// and others not, which is harder to reason about than not having started.
#[test]
fn scenario_12_a_conflicting_rebase_fails_without_pushing_anything() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    w.forge.external_squash_merge(prs[0]).unwrap();
    // The same file, changed to something else locally: replaying this onto a
    // trunk that already carries `a1` cannot be resolved automatically.
    w.amend_layer(0, &[("a.txt", "a2")]);

    let before = w.push_count();
    let err = w.try_sync_trunk().expect_err("the rebase cannot succeed");
    let text = format!("{err:#}");
    assert!(
        text.contains("nspr sync"),
        "the error should say how to recover:\n{text}"
    );

    assert_eq!(
        w.push_count(),
        before,
        "nothing may be pushed once the repair is known to be impossible"
    );
}

// ---------------------------------------------------------------------------
// amend: bring reviewer edits back into the commit message
// ---------------------------------------------------------------------------

#[test]
fn amend_pulls_edited_titles_back_into_the_commits() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    // A reviewer suggests a better title and the author takes it in the UI.
    w.forge
        .edit_in_ui(prs[0], "Introduce the widget trait", "Groundwork.");

    let changed = w.amend();
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].number, prs[0]);
    assert_eq!(changed[0].new_subject, "Introduce the widget trait");

    let stack = w.discover();
    assert_eq!(stack.layers[0].subject(), "Introduce the widget trait");
    // The layer above was reparented but keeps its own message.
    assert_eq!(stack.layers[1].subject(), "Layer two");

    // The bookkeeping trailer survived: losing it would orphan the pull
    // request and the next `nspr diff` would open a second one.
    assert_eq!(stack.layers[0].pr, Some(prs[0]));

    // Nothing was pushed; amend is a local operation.
    w.assert_all_branch_commits_are_linear();

    // And it is idempotent.
    assert!(w.amend().is_empty());
}

/// After `amend` pulls an edited title/description from GitHub into the local
/// commit, the next `sync` rewrites the initial commit on the PR branch so that
/// a subsequent web UI squash-merge uses the updated message, and any further
/// `sync` is a complete no-op.
#[test]
fn amend_rewrites_branch_commit_message_and_subsequent_sync_is_noop() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    w.forge
        .edit_in_ui(prs[0], "A better title", "New body text.");
    w.amend();

    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    let root_msg = w.git.message_of(outcomes[0].tip).unwrap();
    assert_eq!(root_msg.trim(), "A better title\n\nNew body text.");

    // Once the branch's initial commit matches the new message, re-syncing
    // pushes nothing.
    let before = w.push_count();
    let outcomes = w.sync();
    assert!(outcomes.iter().all(|o| o.action == LayerAction::Skipped));
    assert_eq!(w.push_count(), before);
}

// ---------------------------------------------------------------------------
// close: abandon a layer
// ---------------------------------------------------------------------------

/// Closing the middle of a stack must reparent the layer above it, and that
/// layer must stop showing the abandoned changes.
#[test]
fn close_restacks_the_layer_above_onto_the_one_below() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    let first = w.sync();
    let prs = w.pr_numbers();

    let outcome = w.close(prs[1]);
    assert_eq!(outcome.retargeted, vec![prs[2]]);

    // The closed pull request is closed; its branch is deliberately left in
    // place so it can be reopened.
    let closed = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    assert_eq!(closed.state, crate::forge::PrState::Closed);
    assert!(w.forge.branch(&first[1].branch).is_some());

    // The local commit is gone.
    let stack = w.discover();
    assert_eq!(stack.layers.len(), 2);
    assert_eq!(stack.layers[1].subject(), "Layer three");

    // After the restack, layer three sits on layer one and no longer shows the
    // abandoned file.
    let outcomes = w.sync();
    assert_eq!(outcomes[1].base, first[0].branch);
    w.assert_invariants();

    let repo = w.t.open();
    let pr3 = block_on(w.forge.get_pull_request(prs[2])).unwrap();
    let base_tip = w.forge.branch(&pr3.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr3.head_oid).unwrap();
    assert!(text.contains("c.txt"));
    assert!(!text.contains("b.txt"), "abandoned changes leaked:\n{text}");
}

/// A dependent that named the closed pull request explicitly must have its
/// trailer rewritten, or the next `nspr diff` would refuse to run at all:
/// `Depends-On` pointing at a closed pull request is a hard error.
#[test]
fn close_rewrites_trailers_that_named_the_closed_pull_request() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Layer three explicitly depends on layer two.
    w.set_trailer(2, crate::trailers::DEPENDS_ON, &format!("#{}", prs[1]));
    w.sync();

    w.close(prs[1]);

    let stack = w.discover();
    let three = stack.layers.iter().find(|l| l.pr == Some(prs[2])).unwrap();
    assert_eq!(
        three.dep_spec,
        Some(crate::stack::DepSpec::Pr(prs[0])),
        "the trailer should now name layer one"
    );

    // And the stack is usable again without further intervention.
    w.sync();
    w.assert_invariants();
}

/// Closing the bottom layer leaves its dependent on the trunk.
#[test]
fn close_the_bottom_layer_moves_the_next_one_to_the_trunk() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    let outcome = w.close(prs[0]);
    assert_eq!(outcome.retargeted, vec![prs[1]]);

    let outcomes = w.sync();
    assert_eq!(outcomes[0].base, TRUNK);
    w.assert_invariants();
}

/// nspr never advances `refs/remotes/<remote>/<trunk>` itself — it fetches
/// objects by id, not by refspec — so after anything lands, that ref is stale.
/// Believing it puts the merged layer back in the stack and makes the next
/// `diff` offer to retarget the layer above it onto the branch that was just
/// merged away, undoing the land.
#[test]
fn the_trunk_comes_from_the_remote_not_from_a_stale_tracking_ref() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    let tracking = format!("refs/remotes/origin/{TRUNK}");
    let stale = w.base_oid;
    w.git
        .set_reference(&tracking, stale, "test: last fetch")
        .unwrap();

    w.forge.external_squash_merge(prs[0]).unwrap();
    let moved = w.forge.trunk_oid();
    assert_ne!(moved, stale, "the fake remote should have moved");

    let resolved =
        block_on(sync::resolve_trunk(&w.git, &w.forge, "origin", TRUNK))
            .unwrap();

    assert_eq!(resolved, moved, "should have asked the remote");
    assert_eq!(
        w.git.resolve_reference(&tracking).unwrap(),
        moved,
        "and should have moved the tracking ref to match"
    );
}

/// Being unable to reach the remote is not a reason to refuse to run: the last
/// tip we saw is still the best answer available.
#[test]
fn an_unanswerable_trunk_falls_back_to_the_tracking_ref() {
    let w = World::new(&[("root.txt", "root")]);

    let tracking = "refs/remotes/origin/not-on-the-remote";
    w.git
        .set_reference(tracking, w.base_oid, "test: last fetch")
        .unwrap();

    let resolved = block_on(sync::resolve_trunk(
        &w.git,
        &w.forge,
        "origin",
        "not-on-the-remote",
    ))
    .unwrap();
    assert_eq!(resolved, w.base_oid);

    // With nothing local either, there is genuinely nothing to go on.
    let error = block_on(sync::resolve_trunk(
        &w.git,
        &w.forge,
        "origin",
        "never-heard-of-it",
    ))
    .unwrap_err()
    .to_string();
    assert!(error.contains("git fetch"), "{error}");
}

#[test]
fn list_builds_and_formats_linear_stack_hierarchy() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    let forest = w.list();
    assert_eq!(
        forest.len(),
        1,
        "all 3 layers should be in a single root tree"
    );
    let root = &forest[0].roots[0];
    assert_eq!(root.pr.number, prs[0]);
    assert_eq!(root.children.len(), 1);
    let child = &root.children[0];
    assert_eq!(child.pr.number, prs[1]);
    assert_eq!(child.children.len(), 1);
    let grandchild = &child.children[0];
    assert_eq!(grandchild.pr.number, prs[2]);
    assert_eq!(grandchild.children.len(), 0);

    let rendered = crate::list::format_stacks(&forest);
    assert!(rendered.contains(&format!("#{}", prs[0])));
    assert!(rendered.contains("Layer one"));
    assert!(rendered.contains(&format!("#{}", prs[1])));
    assert!(rendered.contains("Layer two"));
    assert!(rendered.contains(&format!("#{}", prs[2])));
    assert!(rendered.contains("Layer three"));
}

#[test]
fn list_builds_dag_with_multiple_branches() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Base layer", &[("base.txt", "base")]);
    w.sync();
    let pr1 = w.pr_numbers()[0];

    // Branch A based on PR 1
    w.add_layer("Branch A", &[("a.txt", "a")]);
    w.sync();
    let pr_a = w.pr_numbers()[1];

    // Reset local to base layer, and create Branch B also depending on PR 1
    w.drop_layer(1);
    w.add_layer("Branch B", &[("b.txt", "b")]);
    w.set_trailer(1, crate::trailers::DEPENDS_ON, &format!("#{pr1}"));
    w.sync();
    let pr_b = w.pr_numbers()[1];

    let forest = w.list();
    assert_eq!(forest.len(), 1, "root should be PR1");
    let root = &forest[0].roots[0];
    assert_eq!(root.pr.number, pr1);
    assert_eq!(
        root.children.len(),
        2,
        "PR1 should have 2 children: A and B"
    );
    let child_numbers: Vec<u64> =
        root.children.iter().map(|c| c.pr.number).collect();
    assert!(child_numbers.contains(&pr_a));
    assert!(child_numbers.contains(&pr_b));
}

#[test]
fn patch_reconstructs_full_linear_stack_locally() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Reconstruct stack for PR 3 into a new branch
    let outcome = w.patch(prs[2], Some("reconstructed"), false).unwrap();
    assert_eq!(outcome.branch, "reconstructed");
    assert!(outcome.checked_out);
    assert_eq!(outcome.prs, prs);
    assert_eq!(outcome.commits.len(), 3);

    // Current branch should be reconstructed
    assert_eq!(
        w.git.repo().head().unwrap().shorthand().unwrap(),
        "reconstructed"
    );
    assert_eq!(w.git.head().unwrap(), outcome.commits[2]);

    // Trees should match the PR head trees exactly
    for (i, &pr_num) in prs.iter().enumerate() {
        let pr = block_on(w.forge.get_pull_request(pr_num)).unwrap();
        let expected_tree = w.git.tree_of(pr.head_oid).unwrap();
        let reconstructed_tree = w.git.tree_of(outcome.commits[i]).unwrap();
        assert_eq!(
            reconstructed_tree, expected_tree,
            "reconstructed commit tree must match layer {i} PR head tree"
        );
    }

    // Trailers should contain Pull-Request
    let msg = w.git.message_of(outcome.commits[2]).unwrap();
    let parsed = CommitMessage::parse(&msg);
    assert_eq!(
        parsed.get(crate::trailers::PULL_REQUEST),
        Some(format!("https://github.com/o/r/pull/{}", prs[2]).as_str())
    );
}

#[test]
fn patch_honors_no_checkout() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.sync();
    let prs = w.pr_numbers();
    let original_head = w.git.head().unwrap();

    let outcome = w.patch(prs[0], Some("detached"), true).unwrap();
    assert!(!outcome.checked_out);
    assert_eq!(outcome.branch, "detached");
    assert_eq!(w.git.head().unwrap(), original_head, "HEAD must not move");
    assert_eq!(
        w.git.resolve_reference("refs/heads/detached").unwrap(),
        outcome.commits[0]
    );
}

#[test]
fn patch_handles_base_layer_already_landed() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Land PR 1 externally
    w.forge.external_squash_merge(prs[0]).unwrap();
    w.sync_trunk();

    // PR 2 now has base == TRUNK
    let outcome = w.patch(prs[1], Some("from-merged"), false).unwrap();
    assert_eq!(outcome.prs, vec![prs[1]]);
    assert_eq!(outcome.commits.len(), 1);

    // Parent of the reconstructed commit must be the new trunk
    let parent_oid = w
        .git
        .repo()
        .find_commit(outcome.commits[0])
        .unwrap()
        .parent_id(0)
        .unwrap();
    assert_eq!(parent_oid, w.base_oid);
}

#[test]
fn stacked_layers_keep_clean_descriptions_and_sync_native_stack() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    for pr_num in &prs {
        let pr = block_on(w.forge.get_pull_request(*pr_num)).unwrap();
        assert!(
            !pr.body.contains("<!-- nspr:warning -->"),
            "PR #{pr_num} body must stay clean without warning banner: {}",
            pr.body
        );
    }
    assert_eq!(*w.forge.stacks.borrow(), vec![prs]);
}

#[test]
fn non_squash_only_repo_auto_falls_back_to_single_commit_force_push() {
    let mut w = World::new(&[("root.txt", "root")]);
    *w.forge.merge_settings.borrow_mut() = crate::forge::RepoMergeSettings {
        allow_squash_merge: true,
        allow_merge_commit: true,
        allow_rebase_merge: true,
        squash_uses_pr_description: false,
    };

    // 1. Default `PreserveCommitHistory::Auto` falls back to single-commit
    //    force-pushes on updates, keeps PR body clean, and emits a descriptive
    //    CLI guardrail warning suggesting `git config nspr.preserveCommitHistory`.
    w.add_layer("Layer one\n\nBody text.", &[("a.txt", "a1")]);
    w.sync();
    let pr_num = w.pr_numbers()[0];

    w.amend_layer(0, &[("a.txt", "a2")]);
    w.sync();

    let pr = block_on(w.forge.get_pull_request(pr_num)).unwrap();
    assert_eq!(
        pr.body, "Body text.",
        "single-commit force-push mode keeps PR body clean"
    );
    let revisions =
        crate::land::branch_revisions(&w.git, pr.head_oid, w.base_oid).unwrap();
    assert_eq!(
        revisions.len(),
        1,
        "Auto fallback must keep PR branch as a single commit across amends"
    );

    let stack = w.discover();
    let prs = block_on(crate::engine::gather(&w.forge, &stack)).unwrap();
    let g = block_on(guardrails::probe(
        &w.forge,
        &w.config,
        &stack,
        &prs,
        &[false],
        false,
    ))
    .unwrap();
    assert!(
        g.warnings
            .iter()
            .any(|msg| msg.contains("nspr.preserveCommitHistory")),
        "expected guardrail warning suggesting nspr.preserveCommitHistory, got: {:?}",
        g.warnings
    );

    // 2. Setting `PreserveCommitHistory::False` silences the warning.
    w.config.preserve_commit_history =
        crate::config::PreserveCommitHistory::False;
    let g_false = block_on(guardrails::probe(
        &w.forge,
        &w.config,
        &stack,
        &prs,
        &[false],
        false,
    ))
    .unwrap();
    assert!(
        g_false.warnings.is_empty(),
        "expected no warnings when preserveCommitHistory = false, got: {:?}",
        g_false.warnings
    );

    // 3. Setting `PreserveCommitHistory::True` forces incremental `[nspr]`
    //    commits and appends the footer warning to the PR description.
    w.config.preserve_commit_history =
        crate::config::PreserveCommitHistory::True;
    w.amend_layer(0, &[("a.txt", "a3")]);
    w.sync();
    let pr_true = block_on(w.forge.get_pull_request(pr_num)).unwrap();
    let revisions_true =
        crate::land::branch_revisions(&w.git, pr_true.head_oid, w.base_oid)
            .unwrap();
    assert_eq!(
        revisions_true.len(),
        2,
        "preserveCommitHistory = true must append an [nspr] commit"
    );
    assert!(
        pr_true
            .body
            .starts_with("Body text.\n\n<!-- nspr:warning -->\n---"),
        "preserveCommitHistory = true on a non-squash repo must append footer warning, got:\n{}",
        pr_true.body
    );
}

#[test]
fn amend_strips_legacy_warning_without_polluting_local_commits() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Even if a legacy PR had <!-- nspr:warning --> on GitHub, amend strips it.
    let edited_body = "<!-- nspr:warning -->\nOld warning\n<!-- /nspr:warning -->\n\nHuman authored description.";
    w.forge.edit_in_ui(prs[0], "Layer one", edited_body);

    let changed = w.amend();
    assert_eq!(changed.len(), 1);

    let stack = w.discover();
    assert_eq!(stack.layers[0].message.body, "Human authored description.");
    assert!(!stack.layers[0].message.body.contains("nspr:warning"));
}

#[test]
fn cherry_pick_diff_creates_only_head_pr_targeting_trunk() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("WIP layer one", &[("a.txt", "a1")]);
    w.add_layer("WIP layer two", &[("b.txt", "b1")]);
    w.add_layer("Cherry-picked bugfix", &[("c.txt", "c1")]);

    // Simulate `nspr diff --cherry-pick`: set `Depends-On: main` on HEAD and
    // sync with `only_layer: Some(2)`.
    w.set_trailer(2, crate::trailers::DEPENDS_ON, TRUNK);
    let outcomes = w.sync_with(SyncOptions {
        only_layer: Some(2),
        ..Default::default()
    });

    // Only the cherry-picked HEAD layer was pushed and created.
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].index, 2);
    assert_eq!(outcomes[0].action, LayerAction::Created);
    assert_eq!(outcomes[0].base, TRUNK);

    let stack = w.discover();
    assert!(stack.layers[0].pr.is_none());
    assert!(stack.layers[1].pr.is_none());
    assert!(stack.layers[2].pr.is_some());

    // The cherry-picked PR does not receive a web UI merge warning because it
    // targets trunk directly and has no dependents.
    let pr = block_on(w.forge.get_pull_request(outcomes[0].number)).unwrap();
    assert!(!pr.body.contains("<!-- nspr:warning -->"));

    // Its displayed diff contains only its own file (`c.txt`).
    let repo = w.t.open();
    let base_tip = w.forge.branch(&pr.base).unwrap();
    let text =
        review_diff::displayed_patch(&repo, base_tip, pr.head_oid).unwrap();
    assert!(text.contains("c.txt"));
    assert!(!text.contains("a.txt"));
    assert!(!text.contains("b.txt"));

    // Landing the cherry-picked PR merges `c.txt` onto trunk and rebases the
    // remaining WIP commits locally onto the new trunk.
    let landed = w.land(2);
    assert_eq!(landed.number, outcomes[0].number);
    let remaining = w.discover();
    assert_eq!(remaining.layers.len(), 2);
    assert_eq!(remaining.layers[0].subject(), "WIP layer one");
    assert_eq!(remaining.layers[1].subject(), "WIP layer two");
}

#[test]
fn lower_layer_amend_causing_forge_merge_conflict_refreshes_upper_layer() {
    let mut w = World::new(&[("shared.txt", "line1\nline2\nline3\n")]);
    w.add_layer("Layer one", &[("shared.txt", "line1_a\nline2\nline3\n")]);
    w.add_layer("Layer two", &[("shared.txt", "line1_a\nline2\nline3_b\n")]);
    w.sync();

    // Now amend Layer one so it modifies adjacent/overlapping lines in a way
    // where merging new_head_0 into old_head_1 (with ancestor old_head_0)
    // conflicts on GitHub if Layer two's branch is left at old_head_1, even
    // though locally Layer two is rebased cleanly onto Layer one.
    w.amend_layer(0, &[("shared.txt", "line1_a2\nline2_conflict\nline3\n")]);
    w.amend_layer(1, &[("shared.txt", "line1_a2\nline2_conflict\nline3_b\n")]);

    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    // Layer two must be updated/refreshed so GitHub does not see a 3-way merge
    // conflict between new_head_0 and old_head_1.
    assert_ne!(
        outcomes[1].action,
        LayerAction::Skipped,
        "Layer two must be refreshed to prevent merge conflict on forge"
    );
    w.assert_invariants();
}

#[test]
fn initial_pr_commit_and_reanchored_commit_carry_clean_commit_message() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer(
        "Layer one\n\nDetailed explanation for layer one.",
        &[("a.txt", "a1")],
    );
    w.add_layer(
        "Layer two\n\nOriginal body for layer two.",
        &[("b.txt", "b1")],
    );
    let outcomes = w.sync();

    // The initial commit on PR #1's branch carries the real commit message
    // rather than `[nspr] initial version`, so a 1-commit web UI squash merge
    // defaults to the actual commit message.
    let msg1 = w.git.message_of(outcomes[0].tip).unwrap();
    assert_eq!(
        msg1.trim(),
        "Layer one\n\nDetailed explanation for layer one."
    );
    assert!(!msg1.contains("[nspr]"));

    // Edit Layer two's title & description on GitHub and pull it locally via
    // `amend`, then land Layer one. When `land` re-anchors Layer two onto the
    // new squash commit, Layer two's branch commit is updated to the edited
    // commit message.
    let prs = w.pr_numbers();
    w.forge.edit_in_ui(
        prs[1],
        "Layer two renamed",
        "Updated body for layer two.",
    );
    w.amend();

    let landed = w.land(0);
    let repaired_tip = landed.repaired[0].new_tip;
    let msg2 = w.git.message_of(repaired_tip).unwrap();
    assert_eq!(
        msg2.trim(),
        "Layer two renamed\n\nUpdated body for layer two."
    );
}

#[test]
fn editing_local_commit_message_rewrites_first_branch_commit_and_updates_pr() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Original title\n\nOriginal body.", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();

    // First push a code update to Layer one so it has 2 commits on its branch
    // (the initial commit + an update commit).
    w.amend_layer(0, &[("a.txt", "a2")]);
    w.sync();

    // Now edit Layer one's commit message locally (like `git commit --amend`
    // changing the title/body) and run `nspr diff` (`w.sync()`).
    w.layers[0].message = crate::trailers::CommitMessage::parse(&format!(
        "Rewritten title\n\nRewritten body.\n\nPull-Request: #{}",
        w.pr_numbers()[0]
    ));
    w.rebuild();

    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);

    // 1. The GitHub PR title and body were updated automatically.
    let prs = w.pr_numbers();
    let pr1 = block_on(w.forge.get_pull_request(prs[0])).unwrap();
    assert_eq!(pr1.title, "Rewritten title");
    assert!(pr1.body.contains("Rewritten body."));

    // 2. The initial commit on PR #1's branch was rewritten to carry the new
    //    commit message, and the subsequent revision commit was replayed on top.
    let revs = crate::land::branch_revisions(&w.git, pr1.head_oid, w.base_oid)
        .unwrap();
    assert_eq!(revs.len(), 2);
    let first_msg = w.git.message_of(revs[0]).unwrap();
    assert_eq!(first_msg.trim(), "Rewritten title\n\nRewritten body.");

    // 3. Layer two was re-anchored onto PR #1's new tip so its displayed diff
    //    on GitHub still shows only `b.txt`.
    let repo = w.t.open();
    let pr2 = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    let text = review_diff::displayed_patch(&repo, pr1.head_oid, pr2.head_oid)
        .unwrap();
    assert!(text.contains("b.txt"));
    assert!(!text.contains("a.txt"));
}

#[test]
fn upgrade_converts_legacy_spr_stack_into_native_stacked_prs() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one\n\nBody one.", &[("a.txt", "a1")]);
    w.add_layer("Layer two\n\nBody two.", &[("b.txt", "b1")]);

    // Simulate a 2-layer stack created by `spacedentist/spr`:
    // - Remote branches `spr/main/1111` (base `main`) and `spr/main/2222`
    //   (base `spr/main/master.2222`) carrying `[spr] initial version` commits.
    // - Local commits carrying `Pull Request: https://github.com/o/r/pull/101`
    //   (with a space).
    let stack = w.discover();
    let trees = stack.all_trees(&w.git).unwrap();

    let spr_tip_1 = w
        .git
        .synthesize_initial_commit(
            w.base_oid,
            trees.effective[0],
            stack.layers[0].commit,
            "[spr] initial version",
        )
        .unwrap();
    block_on(w.forge.push(&[crate::forge::PushSpec::fast_forward(
        "spr/main/1111",
        spr_tip_1,
    )]))
    .unwrap();
    let pr1_num =
        block_on(w.forge.create_pull_request(crate::forge::CreatePr {
            title: "Layer one".into(),
            body: "Body one.".into(),
            base: TRUNK.into(),
            head: "spr/main/1111".into(),
            draft: false,
        }))
        .unwrap();

    block_on(w.forge.push(&[crate::forge::PushSpec::fast_forward(
        "spr/main/master.2222",
        spr_tip_1,
    )]))
    .unwrap();
    let spr_tip_2 = w
        .git
        .synthesize_initial_commit(
            spr_tip_1,
            trees.effective[1],
            stack.layers[1].commit,
            "[spr] initial version",
        )
        .unwrap();
    block_on(w.forge.push(&[crate::forge::PushSpec::fast_forward(
        "spr/main/2222",
        spr_tip_2,
    )]))
    .unwrap();
    let pr2_num =
        block_on(w.forge.create_pull_request(crate::forge::CreatePr {
            title: "Layer two".into(),
            body: "Body two.".into(),
            base: "spr/main/master.2222".into(),
            head: "spr/main/2222".into(),
            draft: false,
        }))
        .unwrap();

    w.layers[0].message.set(
        crate::trailers::LEGACY_SPR_PULL_REQUEST,
        &format!("https://github.com/o/r/pull/{pr1_num}"),
    );
    w.layers[1].message.set(
        crate::trailers::LEGACY_SPR_PULL_REQUEST,
        &format!("https://github.com/o/r/pull/{pr2_num}"),
    );
    w.rebuild();

    // `status` detects the legacy `spr` pull requests.
    let status = w.status();
    assert_eq!(status.layers[0].state, status::LayerState::LegacySpr);
    assert_eq!(status.layers[1].state, status::LayerState::LegacySpr);

    // None of `diff`, `land`, `sync`, `close`, or `amend` may automatically
    // upgrade or mutate `spr` pull requests before `nspr upgrade` is run.
    let pushes_before = w.push_count();

    let diff_err = w.try_sync().unwrap_err().to_string();
    assert!(
        diff_err.contains("nspr upgrade"),
        "expected upgrade advice from diff, got: {diff_err}"
    );

    let land_err = w.try_land(0).unwrap_err().to_string();
    assert!(
        land_err.contains("nspr upgrade"),
        "expected upgrade advice from land, got: {land_err}"
    );

    let sync_err = w.try_sync_trunk().unwrap_err().to_string();
    assert!(
        sync_err.contains("nspr upgrade"),
        "expected upgrade advice from sync, got: {sync_err}"
    );

    let stack_before = w.discover();
    let close_err = block_on(crate::close::close_layer(
        &w.git,
        &w.forge,
        &w.config,
        &stack_before,
        0,
    ))
    .unwrap_err()
    .to_string();
    assert!(
        close_err.contains("nspr upgrade"),
        "expected upgrade advice from close, got: {close_err}"
    );

    let amend_err =
        block_on(crate::amend::amend(&w.git, &w.forge, &stack_before))
            .unwrap_err()
            .to_string();
    assert!(
        amend_err.contains("nspr upgrade"),
        "expected upgrade advice from amend, got: {amend_err}"
    );

    assert_eq!(w.push_count(), pushes_before);
    assert!(w.forge.branch_exists("spr/main/master.2222"));
    assert!(w.discover().layers[0].message.has_legacy_spr_trailer());

    // Only explicit `upgrade_stack` (`nspr upgrade`) converts both PRs in-place.
    let mut stack = w.discover();
    let upgraded = block_on(crate::upgrade::upgrade_stack(
        &w.git, &w.forge, &w.config, &mut stack,
    ))
    .unwrap();
    w.sync_worktree();
    w.refresh_specs_from_repo();

    assert_eq!(upgraded.len(), 2);
    assert_eq!(upgraded[1].old_base, "spr/main/master.2222");
    assert_eq!(upgraded[1].new_base, "spr/main/1111");
    assert_eq!(
        upgraded[1].deleted_synthetic_base.as_deref(),
        Some("spr/main/master.2222")
    );
    assert!(!w.forge.branch_exists("spr/main/master.2222"));

    // Local commit trailers are normalized to `Pull-Request:` (with hyphen).
    let stack = w.discover();
    assert!(!stack.layers[0].message.has_legacy_spr_trailer());
    assert!(!stack.layers[1].message.has_legacy_spr_trailer());
    assert_eq!(stack.layers[0].pr, Some(pr1_num));
    assert_eq!(stack.layers[1].pr, Some(pr2_num));

    // The remote PR branches no longer contain `[spr]` commits, and instead
    // carry the clean commit messages.
    let pr1 = block_on(w.forge.get_pull_request(pr1_num)).unwrap();
    let pr2 = block_on(w.forge.get_pull_request(pr2_num)).unwrap();
    assert_eq!(pr2.base, "spr/main/1111");
    assert_eq!(
        w.git.message_of(pr1.head_oid).unwrap().trim(),
        "Layer one\n\nBody one."
    );
    assert_eq!(
        w.git.message_of(pr2.head_oid).unwrap().trim(),
        "Layer two\n\nBody two."
    );

    // Subsequent `status` is `Current` ("ok") and `sync` pushes nothing.
    let status = w.status();
    assert_eq!(status.layers[0].state, status::LayerState::Current);
    assert_eq!(status.layers[1].state, status::LayerState::Current);

    let outcomes = w.sync();
    assert!(outcomes.iter().all(|o| o.action == LayerAction::Skipped));
    w.assert_invariants();
}

#[test]
fn linear_revisions_retained_across_amends_and_restacks() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Amend layer two while layer one's tip is unchanged: fast-forward 1-parent commit.
    w.amend_layer(1, &[("b.txt", "b2")]);
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Skipped);
    assert_eq!(outcomes[1].action, LayerAction::Updated);
    assert_eq!(outcomes[2].action, LayerAction::Skipped);

    // Layer two now has 2 linear commits (`b1` and `b2`) on top of layer one.
    let pr1 = block_on(w.forge.get_pull_request(prs[0])).unwrap();
    let pr2 = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    let revs2 =
        crate::land::branch_revisions(&w.git, pr2.head_oid, pr1.head_oid)
            .unwrap();
    assert_eq!(revs2.len(), 2);

    // Amend layer one: fast-forward 1-parent commit on layer one; upper layers skipped.
    w.amend_layer(0, &[("a.txt", "a2")]);
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Skipped);
    assert_eq!(outcomes[2].action, LayerAction::Skipped);

    // Now amend layer two again (`b3`): layer two replays `[b1, b2]` onto layer one's
    // new tip and appends `b3` (giving 3 linear commits), and cascades re-anchoring
    // to layer three so every PR branch in the stack remains 1-parent linear.
    w.amend_layer(1, &[("b.txt", "b3")]);
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Skipped);
    assert_eq!(outcomes[1].action, LayerAction::Updated);
    assert_eq!(outcomes[2].action, LayerAction::Refreshed);
    w.assert_invariants();

    let pr1 = block_on(w.forge.get_pull_request(prs[0])).unwrap();
    let pr2 = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    let pr3 = block_on(w.forge.get_pull_request(prs[2])).unwrap();

    let revs1 = crate::land::branch_revisions(&w.git, pr1.head_oid, w.base_oid)
        .unwrap();
    let revs2 =
        crate::land::branch_revisions(&w.git, pr2.head_oid, pr1.head_oid)
            .unwrap();
    let revs3 =
        crate::land::branch_revisions(&w.git, pr3.head_oid, pr2.head_oid)
            .unwrap();

    assert_eq!(
        revs1.len(),
        2,
        "layer one must retain its 2 linear revisions"
    );
    assert_eq!(
        revs2.len(),
        3,
        "layer two must retain all 3 linear revisions (`b1`, `b2`, `b3`) after re-anchoring"
    );
    assert_eq!(
        revs3.len(),
        1,
        "layer three must be cleanly re-anchored on top of layer two's new tip"
    );
}

// ---------------------------------------------------------------------------
// Retargeting must never widen the displayed diff, not even for an instant.
// ---------------------------------------------------------------------------

/// Assert that `number` only ever displayed `expected`, at any point during the
/// observation window.
///
/// The end state being right is not good enough. GitHub recomputes a pull
/// request's file list on every push and hands it straight to `CODEOWNERS`, and
/// the review requests that come out of that are never withdrawn when the file
/// list shrinks again a moment later.
fn assert_only_ever_displayed(w: &World, number: u64, expected: &[&str]) {
    let expected: Vec<String> =
        expected.iter().map(|s| (*s).to_string()).collect();
    let seen = w.forge.observed_diffs(number);
    assert!(!seen.is_empty(), "PR #{number} was never observed");
    for files in seen {
        assert_eq!(
            files, expected,
            "PR #{number} displayed the wrong files at some point during the \
             command, which is when CODEOWNERS gets consulted"
        );
    }
}

/// Pulling one commit out of a stack and pointing it at a trunk that has moved
/// on is the case that first surfaced this: the head was re-anchored on the new
/// trunk while the pull request still pointed at the layer below, so for a
/// couple of seconds it displayed every upstream commit in between and
/// subscribed all of their owners.
#[test]
fn retargeting_onto_a_moved_trunk_never_displays_the_upstream_commits() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Independent fix", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Somebody else lands work on the trunk while the stack is open.
    w.advance_trunk_and_pull(&[("upstream.txt", "u1")]);

    // The author decides the top layer does not belong in the stack after all,
    // which is what `nspr diff --cherry-pick` does: point it at the trunk and
    // sync that layer alone.
    w.set_trailer(1, crate::trailers::DEPENDS_ON, TRUNK);
    w.forge.clear_diff_observations();
    w.sync_with(SyncOptions {
        only_layer: Some(1),
        ..Default::default()
    });

    assert_only_ever_displayed(&w, prs[1], &["c.txt"]);
    let pr = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    assert!(
        w.git.is_ancestor(w.base_oid, pr.head_oid).unwrap(),
        "single nspr diff run must finish with the retargeted branch on top of the new trunk"
    );
    w.assert_invariants();
}

/// Reordering swaps which branch each pull request is based on, so both ends of
/// the swap are retargeted at a branch that does not contain their old anchor.
#[test]
fn reordering_a_stack_never_displays_the_other_layer() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Unrelated upstream work widens the gap between the trunk the stack was
    // built on and the trunk it is about to be retargeted at.
    w.advance_trunk_and_pull(&[("upstream.txt", "u1")]);

    w.swap_layers(0, 1);
    w.forge.clear_diff_observations();
    w.sync();

    assert_only_ever_displayed(&w, prs[0], &["a.txt"]);
    assert_only_ever_displayed(&w, prs[1], &["b.txt"]);
    assert_only_ever_displayed(&w, prs[2], &["c.txt"]);
    w.assert_invariants();
}

/// A pull request that has been pointed back at the trunk is not part of a
/// stack any more, and a table still claiming it is blocked on the layer below
/// is worse than no table at all.
#[test]
fn a_pull_request_pulled_out_of_the_stack_loses_its_table() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Independent fix", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();
    w.update_stack_comments();

    let comment = w.comment_on(prs[1]).expect("stacked PR gets a table");
    assert!(comment.body.contains(crate::stack_comment::BEGIN));
    // Somebody replied in the same comment.
    block_on(w.forge.update_comment(
        comment.id,
        &format!("{}\n\nPlease take a look.", comment.body),
    ))
    .unwrap();

    w.set_trailer(1, crate::trailers::DEPENDS_ON, TRUNK);
    w.sync();
    w.update_stack_comments();

    let comment = w
        .comment_on(prs[1])
        .expect("the human's text must not be thrown away with the table");
    assert!(!comment.body.contains(crate::stack_comment::BEGIN));
    assert!(comment.body.contains("Please take a look."));

    // Layer one is on its own now too, and its table was the whole comment, so
    // the comment goes away entirely.
    assert!(w.comment_on(prs[0]).is_none());
}

/// `nspr.draftWhileRetargeting` is the belt to the parking braces: the pull
/// request is invisible to `CODEOWNERS` for the whole push-and-retarget window,
/// and comes back out in the state it went in.
#[test]
fn draft_while_retargeting_flips_the_pull_request_back_afterwards() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Independent fix", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();

    w.config.draft_while_retargeting = true;
    w.set_trailer(1, crate::trailers::DEPENDS_ON, TRUNK);
    w.sync();

    assert_eq!(
        *w.forge.draft_toggles.borrow(),
        vec![(prs[1], true), (prs[1], false)],
        "only the retargeted pull request is touched, and it is put back"
    );
    let pr = block_on(w.forge.get_pull_request(prs[1])).unwrap();
    assert!(!pr.draft);
}

/// Nothing happens without the setting, which is the default.
#[test]
fn retargeting_does_not_touch_the_draft_flag_by_default() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Independent fix", &[("c.txt", "c1")]);
    w.sync();

    w.set_trailer(1, crate::trailers::DEPENDS_ON, TRUNK);
    w.sync();

    assert!(w.forge.draft_toggles.borrow().is_empty());
}

/// Re-attaching a PR whose base was accidentally changed to `main` back onto
/// its parent PR branch (`#225129` on `llvm/llvm-project`) retargets `base`
/// before `git push` because `target_remote_tip` is already an ancestor of
/// `pr.head_oid`. That shrinks the displayed diff immediately and finishes in
/// a single `git push` (no parking or second pass).
#[test]
fn reattaching_pr_from_main_onto_parent_layer_uses_single_push() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.sync();
    let prs = w.pr_numbers();

    // Simulate the PR's base on GitHub having been flipped to `main` (while its
    // head commit still sits on top of Layer one's branch).
    block_on(w.forge.update_pull_request(
        prs[1],
        crate::forge::PullRequestUpdate {
            base: Some(TRUNK.to_string()),
            ..Default::default()
        },
    ))
    .unwrap();

    // Now trunk advances, the stack is rebased locally, and layer two is
    // amended (matching `#225129` on `llvm/llvm-project`: `modified,retarget`).
    w.advance_trunk_and_pull(&[("upstream.txt", "u1")]);
    w.amend_layer(1, &[("b.txt", "b2")]);

    let pushes_before = w.push_count();
    w.forge.clear_diff_observations();
    w.sync();

    assert_eq!(
        w.push_count() - pushes_before,
        1,
        "re-attaching onto an unchanged parent layer should only push the modified child layer in a single pass"
    );
    assert_only_ever_displayed(&w, prs[1], &["b.txt"]);
    w.assert_invariants();
}

#[test]
fn amending_bottom_layer_after_local_trunk_rebase_does_not_restack_upper_layers() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();

    // Trunk advances and local stack is rebased onto it, then only Layer one is
    // amended (matching `~/cheri/upstream-llvm-project`).
    w.advance_trunk_and_pull(&[("upstream.txt", "u1")]);
    w.amend_layer(0, &[("a.txt", "a2")]);

    let stack = w.discover();
    let st = block_on(status::status(&w.git, &w.forge, &w.config, &stack)).unwrap();
    assert_eq!(st.layers[0].state, status::LayerState::Modified);
    assert_eq!(st.layers[1].state, status::LayerState::Current);
    assert_eq!(st.layers[2].state, status::LayerState::Current);

    let pushes_before = w.push_count();
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Skipped);
    assert_eq!(outcomes[2].action, LayerAction::Skipped);
    assert_eq!(w.push_count() - pushes_before, 1);
    w.assert_invariants();
}

#[test]
fn reordering_top_layer_down_pushes_each_branch_at_most_once() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer 1 (225126)", &[("f1.txt", "1")]);
    w.add_layer("Layer 2 (225129)", &[("f2.txt", "2")]);
    w.add_layer("Layer 3 (225130)", &[("f3.txt", "3")]);
    w.add_layer("Layer 4 (225131)", &[("f4.txt", "4")]);
    w.add_layer("Layer 5 (225140)", &[("f5.txt", "5")]);
    w.add_layer("Layer 6 (225133)", &[("f6.txt", "6")]);
    w.add_layer("Layer 7 (225127)", &[("f7.txt", "7")]);
    w.sync();
    let prs = w.pr_numbers();

    // Move Layer 7 (index 6) down to index 1 (right above Layer 1), matching
    // the `~/cheri/upstream-llvm-project` stack reorder.
    let top = w.layers.remove(6);
    w.layers.insert(1, top);
    w.rebuild();

    let pushes_before = w.push_count();
    w.forge.clear_diff_observations();
    w.sync();

    // Layer 1 is untouched; each of the 6 moved/restacked layers is pushed
    // exactly once (1 push in Pass 1 for #225127, 5 pushes in Pass 2 for
    // #225129..#225133), rather than pushing the upper 5 layers twice.
    assert_eq!(
        w.push_count() - pushes_before,
        6,
        "each of the 6 affected branches should be pushed at most once"
    );
    assert_only_ever_displayed(&w, prs[6], &["f7.txt"]);
    assert_only_ever_displayed(&w, prs[1], &["f2.txt"]);
    assert_only_ever_displayed(&w, prs[2], &["f3.txt"]);
    assert_only_ever_displayed(&w, prs[3], &["f4.txt"]);
    assert_only_ever_displayed(&w, prs[4], &["f5.txt"]);
    assert_only_ever_displayed(&w, prs[5], &["f6.txt"]);
    w.assert_invariants();
}

#[test]
fn reordering_conflicting_layers_does_not_auto_merge_lower_pr() {
    let mut w = World::new(&[("shared.txt", "line1\nline2\n")]);
    w.add_layer("Layer A", &[("a.txt", "a1")]);
    w.add_layer("Layer B", &[("shared.txt", "line1_b\nline2\n")]);
    w.add_layer("Layer C", &[("shared.txt", "line1_c\nline2_c\n")]);
    w.sync();
    let prs = w.pr_numbers();

    // Swap B and C locally so the order is A -> C -> B, and both B and C modify
    // `shared.txt` on the same lines so `rebase_tree_onto` returns `None`.
    w.swap_layers(1, 2);
    w.amend_layer(1, &[("shared.txt", "line1_c\nline2\n")]);
    w.amend_layer(2, &[("shared.txt", "line1_c\nline2_b\n")]);

    w.sync();

    for &pr_num in &prs {
        let pr = block_on(w.forge.get_pull_request(pr_num)).unwrap();
        assert_eq!(
            pr.state,
            crate::forge::PrState::Open,
            "PR #{pr_num} must not be auto-closed as Merged by GitHub"
        );
    }
    w.assert_invariants();
}

#[test]
fn stripped_pull_request_trailer_prompts_to_relink_or_open_new_pr() {
    struct RelinkChoicePrompter {
        relink: bool,
        warned_pr: std::cell::Cell<Option<u64>>,
    }
    impl crate::engine::Prompter for RelinkChoicePrompter {
        fn update_message(&self, _subject: &str) -> color_eyre::eyre::Result<String> {
            Ok("update".to_string())
        }
        fn confirm_relink_existing_pr(
            &self,
            _subject: &str,
            existing_pr_number: u64,
            _existing_pr_title: &str,
            _branch: &str,
        ) -> color_eyre::eyre::Result<bool> {
            self.warned_pr.set(Some(existing_pr_number));
            Ok(self.relink)
        }
    }

    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer("Layer one", &[("a.txt", "a1")]);
    w.add_layer("Layer two", &[("b.txt", "b1")]);
    w.add_layer("Layer three", &[("c.txt", "c1")]);
    w.sync();
    let prs = w.pr_numbers();
    assert_eq!(prs, vec![101, 102, 103]);

    // Simulate `git commit --amend -m ...` accidentally stripping the
    // `Pull-Request:` trailer from Layer two while amending its content.
    w.layers[1].message.remove(crate::trailers::PULL_REQUEST);
    w.amend_layer(1, &[("b.txt", "b2")]);

    // Case 1: User confirms re-linking (`relink: true`).
    let prompter = RelinkChoicePrompter {
        relink: true,
        warned_pr: std::cell::Cell::new(None),
    };
    let mut stack = w.discover();
    let outcomes = block_on(sync_stack(
        &w.git,
        &w.forge,
        &w.config,
        &mut stack,
        &SyncOptions::default(),
        &prompter,
    ))
    .unwrap();
    w.refresh_specs_from_repo();

    assert_eq!(prompter.warned_pr.get(), Some(102));
    assert_eq!(outcomes[1].number, 102);
    assert_eq!(outcomes[1].action, LayerAction::Updated);
    assert_eq!(w.pr_numbers(), vec![101, 102, 103]);
    w.assert_invariants();

    // Case 2: Strip the trailer again, and this time user declines (`relink: false`)
    // to open a new PR instead.
    w.layers[1].message.remove(crate::trailers::PULL_REQUEST);
    w.amend_layer(1, &[("b.txt", "b3")]);
    let prompter_decline = RelinkChoicePrompter {
        relink: false,
        warned_pr: std::cell::Cell::new(None),
    };
    let mut stack = w.discover();
    let outcomes = block_on(sync_stack(
        &w.git,
        &w.forge,
        &w.config,
        &mut stack,
        &SyncOptions::default(),
        &prompter_decline,
    ))
    .unwrap();
    w.refresh_specs_from_repo();

    assert_eq!(prompter_decline.warned_pr.get(), Some(102));
    assert_eq!(outcomes[1].number, 104);
    assert_eq!(outcomes[1].action, LayerAction::Created);
    assert_eq!(w.pr_numbers(), vec![101, 104, 103]);
}

#[test]
fn land_all_restacked_series_with_overlapping_files_and_async_pr_head_lag() {
    let mut w = World::new(&[("root.txt", "root")]);
    w.add_layer(
        "Layer A",
        &[
            ("a.txt", "a1"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"todo\"\nstep2 = \"todo\"\nstep3 = \"todo\"\n",
            ),
        ],
    );
    w.add_layer(
        "Layer B",
        &[
            ("b.txt", "b1"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-b\"\nstep2 = \"todo\"\nstep3 = \"todo\"\n",
            ),
        ],
    );
    w.add_layer(
        "Layer C",
        &[
            ("c.txt", "c1"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-b\"\nstep2 = \"done-c\"\nstep3 = \"todo\"\n",
            ),
        ],
    );
    w.add_layer(
        "Layer D",
        &[
            ("d.txt", "d1"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-b\"\nstep2 = \"done-c\"\nstep3 = \"done-d\"\n",
            ),
        ],
    );
    w.sync();

    // Reorder B and C (A -> C -> B -> D) and push revision updates to C and B.
    w.swap_layers(1, 2);
    w.amend_layer(
        1,
        &[
            ("c.txt", "c2"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-c-rev2\"\nstep2 = \"todo\"\nstep3 = \"todo\"\n",
            ),
        ],
    );
    w.amend_layer(
        2,
        &[
            ("b.txt", "b2"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-c-rev2\"\nstep2 = \"done-b-rev2\"\nstep3 = \"todo\"\n",
            ),
        ],
    );
    w.amend_layer(
        3,
        &[
            ("d.txt", "d1"),
            (
                "shared.txt",
                "header = \"v1\"\nstep1 = \"done-c-rev2\"\nstep2 = \"done-b-rev2\"\nstep3 = \"done-d\"\n",
            ),
        ],
    );
    w.sync();

    // Advance trunk, rebase locally, and amend only the bottom layer (Layer A),
    // matching the LLVM patch stack workflow.
    w.advance_trunk_and_pull(&[("upstream.txt", "u1")]);
    w.amend_layer(0, &[("a.txt", "a2")]);
    let outcomes = w.sync();
    assert_eq!(outcomes[0].action, LayerAction::Updated);
    assert_eq!(outcomes[1].action, LayerAction::Skipped);
    assert_eq!(outcomes[2].action, LayerAction::Skipped);
    assert_eq!(outcomes[3].action, LayerAction::Skipped);

    // Simulate GitHub returning stale `PullRequest.headRefOid` reads right
    // after each repair push during `nspr land --all`.
    *w.forge.async_pr_head_lag.borrow_mut() = 2;

    let landed = w.land_all();
    assert_eq!(landed.len(), 4);
    assert!(w.discover().layers.is_empty());

    let subjects: Vec<String> = landed
        .iter()
        .map(|l| w.git.message_of(l.squash).unwrap())
        .map(|m| m.lines().next().unwrap().to_string())
        .collect();
    assert_eq!(
        subjects,
        vec![
            "Layer A (#101)",
            "Layer C (#103)",
            "Layer B (#102)",
            "Layer D (#104)"
        ]
    );

    let files = w.files_of(w.base_oid);
    for (name, content) in [
        ("upstream.txt", "u1"),
        ("a.txt", "a2"),
        ("c.txt", "c2"),
        ("b.txt", "b2"),
        ("d.txt", "d1"),
        (
            "shared.txt",
            "header = \"v1\"\nstep1 = \"done-c-rev2\"\nstep2 = \"done-b-rev2\"\nstep3 = \"done-d\"\n",
        ),
    ] {
        assert!(
            files.contains(&(name.to_string(), content.to_string())),
            "{name} missing or wrong on trunk: {files:?}"
        );
    }
}
