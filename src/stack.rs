//! The stack model.
//!
//! The stack is the linear run of commits between the trunk and `HEAD`. Local
//! order is always linear (so `git rebase -i` keeps working), but the
//! *declared* dependency graph may be a DAG: a layer can use a `Depends-On:`
//! trailer to stack on an earlier layer other than its immediate predecessor,
//! or directly on the trunk.
//!
//! That is what allows independent layers to land out of order.

use std::collections::{HashMap, HashSet};

use color_eyre::eyre::{Result, bail, eyre};
use git2::Oid;

use crate::git::Git;
use crate::trailers::{CommitMessage, DEPENDS_ON, PULL_REQUEST};

/// A `Depends-On:` value exactly as written in the commit message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepSpec {
    /// `Depends-On: main` — stack directly on the trunk.
    Main,
    /// `Depends-On: #101` or a pull request URL.
    Pr(u64),
    /// `Depends-On: a1b2c3d` — a local commit, resolved at submit time.
    Commit(String),
}

/// A dependency resolved against the current stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dep {
    /// Base is the trunk.
    Main,
    /// Base is the head branch of another layer in this stack.
    Layer(usize),
    /// Referenced a pull request that is not in the local stack.
    ///
    /// Resolved by the engine against the pull request's state: a **merged**
    /// reference becomes [`Dep::Main`]; anything else is an error. Crucially it
    /// must *not* silently fall back to "previous layer", which would re-stack
    /// the commit onto a sibling the author explicitly disclaimed.
    ExternalPr(u64),
}

/// Name of the synthetic base branch used on the forge when a pull request has
/// multiple open dependencies in the stack.
pub fn synthetic_base_branch(head_branch: &str) -> String {
    format!("{head_branch}.base")
}

/// Which layers in a [`Stack`] an operation should act on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum LayerSelection {
    #[default]
    All,
    Only(Vec<usize>),
}

impl LayerSelection {
    pub fn one(index: usize) -> Self {
        Self::Only(vec![index])
    }

    pub fn range(range: std::ops::RangeInclusive<usize>) -> Self {
        Self::Only(range.collect())
    }

    pub fn from_indices(indices: impl IntoIterator<Item = usize>) -> Self {
        let mut v: Vec<usize> = indices.into_iter().collect();
        v.sort_unstable();
        v.dedup();
        Self::Only(v)
    }

    pub fn contains(&self, index: usize) -> bool {
        match self {
            Self::All => true,
            Self::Only(indices) => indices.contains(&index),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Layer {
    /// The local commit this layer represents.
    pub commit: Oid,
    /// The local parent of `commit`. May differ from the declared dependency.
    pub parent: Oid,
    pub message: CommitMessage,
    pub pr: Option<u64>,
    /// As written in the commit message, if present (first entry when multiple
    /// dependencies are declared).
    pub dep_spec: Option<DepSpec>,
    /// All `Depends-On:` entries as written in the commit message.
    pub dep_specs: Vec<DepSpec>,
    /// Effective single dependency for the GitHub `base` branch. Defaults to
    /// the previous layer; when 2+ unsubsumed `Dep::Layer` dependencies exist
    /// across independent branches, the pull request targets
    /// [`synthetic_base_branch`] until its dependencies form a single linear
    /// chain.
    pub dep: Dep,
    /// All resolved dependencies in declared order.
    pub deps: Vec<Dep>,
    /// External pull request numbers verified as merged during `resolve_external_deps`.
    pub merged_pr_deps: Vec<u64>,
}

impl Layer {
    pub fn subject(&self) -> &str {
        &self.message.subject
    }

    /// Indices of all layers in `stack.layers` that this layer directly depends on.
    pub fn layer_deps(&self) -> Vec<usize> {
        let mut out = Vec::new();
        for &d in &self.deps {
            if let Dep::Layer(j) = d
                && !out.contains(&j)
            {
                out.push(j);
            }
        }
        if out.is_empty()
            && let Dep::Layer(j) = self.dep
        {
            out.push(j);
        }
        out
    }

    /// True when this layer depends on 2 or more non-linear open branches in
    /// the current stack.
    ///
    /// Because GitHub pull requests only have a single `base` branch, a layer
    /// whose dependencies span multiple open branches targets a synthetic merge
    /// base branch ([`synthetic_base_branch`]) carrying the 3-way merge of its
    /// dependencies until the dependency chain is linear again.
    pub fn has_multiple_layer_deps(&self) -> bool {
        self.layer_deps().len() >= 2
    }

    /// External pull request numbers that this layer declares in `Depends-On:`.
    pub fn external_pr_deps(&self) -> Vec<u64> {
        let mut out = Vec::new();
        for &d in &self.deps {
            if let Dep::ExternalPr(n) = d
                && !out.contains(&n)
            {
                out.push(n);
            }
        }
        if out.is_empty()
            && let Dep::ExternalPr(n) = self.dep
        {
            out.push(n);
        }
        out
    }

    /// True if this layer sits directly on the trunk with no unmerged layer
    /// dependencies, so it can be landed at the bottom of a stack.
    pub fn is_root_landable(&self) -> bool {
        self.dep == Dep::Main && self.layer_deps().is_empty()
    }

    /// Compute the effective single `Dep` (used for the GitHub `base` branch)
    /// from the resolved `deps` list:
    /// - If there is exactly 1 `Dep::Layer(j)`, returns `Dep::Layer(j)`.
    /// - If there are 2+ `Dep::Layer(_)`s, returns `Dep::Main` (since GitHub
    ///   only supports a single base branch, the PR targets `main` until all
    ///   but one dependency have landed).
    /// - If there are no `Dep::Layer(_)`s, returns the first `Dep::ExternalPr(n)`
    ///   if any, or `Dep::Main`.
    pub fn compute_effective_dep(deps: &[Dep]) -> Dep {
        let mut layer_deps = Vec::new();
        for &d in deps {
            if let Dep::Layer(j) = d
                && !layer_deps.contains(&j)
            {
                layer_deps.push(j);
            }
        }
        if layer_deps.len() == 1 {
            return Dep::Layer(layer_deps[0]);
        }
        if layer_deps.len() >= 2 {
            return Dep::Main;
        }
        for &d in deps {
            if let Dep::ExternalPr(n) = d {
                return Dep::ExternalPr(n);
            }
        }
        Dep::Main
    }
}

#[derive(Debug, Clone)]
pub struct Stack {
    /// Name of the trunk branch (e.g. `main`).
    pub trunk: String,
    /// `parent(C_1)`. Must be an ancestor of the trunk.
    pub base: Oid,
    /// Bottom-up; always a valid topological order of the dependency DAG.
    pub layers: Vec<Layer>,
}

/// Per-layer trees, indexed by layer.
#[derive(Debug, Clone)]
pub struct Trees {
    /// What the layer's head branch should contain.
    pub effective: Vec<Oid>,
    /// What the layer's base should contain, i.e. the left-hand side of the
    /// patch the pull request ought to display.
    pub dep: Vec<Oid>,
}

/// Parse a `Depends-On:` value.
///
/// Permissive on input, canonical on storage: we accept `main`, `#101`, `101`,
/// a full pull request URL, or a local commit-ish, and always write back
/// `#101` or `main`.
pub fn parse_dep_spec(value: &str, trunk: &str) -> Result<DepSpec> {
    let value = value.trim();

    if value.eq_ignore_ascii_case(trunk)
        || value.eq_ignore_ascii_case("main")
        || value.eq_ignore_ascii_case("master")
        || value.eq_ignore_ascii_case("trunk")
    {
        return Ok(DepSpec::Main);
    }

    if let Some(caps) = lazy_regex::regex!(r"^#?\s*(\d+)$").captures(value) {
        return Ok(DepSpec::Pr(caps[1].parse()?));
    }

    if let Some(caps) = lazy_regex::regex!(
        r"^https?://[^/]+/[\w\-.]+/[\w\-.]+/pull/(\d+)(?:[/?#].*)?$"
    )
    .captures(value)
    {
        return Ok(DepSpec::Pr(caps[1].parse()?));
    }

    if lazy_regex::regex!(r"^[0-9a-fA-F]{7,40}$").is_match(value) {
        return Ok(DepSpec::Commit(value.to_string()));
    }

    Err(eyre!(
        "cannot parse `{DEPENDS_ON}: {value}`; expected `main`, `#123`, a pull \
         request URL, or a commit hash"
    ))
}

/// Parse one or more `Depends-On:` values separated by commas or whitespace
/// (e.g. `Depends-On: #102, #103`).
pub fn parse_dep_specs(value: &str, trunk: &str) -> Result<Vec<DepSpec>> {
    let value = value.trim();
    if value.is_empty() {
        bail!(
            "cannot parse `{DEPENDS_ON}: {value}`; expected `main`, `#123`, a pull \
             request URL, or a commit hash"
        );
    }
    let mut specs = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Ok(spec) = parse_dep_spec(part, trunk) {
            specs.push(spec);
        } else {
            for token in part.split_whitespace() {
                specs.push(parse_dep_spec(token, trunk)?);
            }
        }
    }
    if specs.is_empty() {
        bail!(
            "cannot parse `{DEPENDS_ON}: {value}`; expected `main`, `#123`, a pull \
             request URL, or a commit hash"
        );
    }
    Ok(specs)
}

/// Extract a pull request number from a `Pull-Request:` trailer value.
pub fn parse_pr_ref(value: &str) -> Option<u64> {
    let value = value.trim();
    if let Some(caps) = lazy_regex::regex!(r"^#?\s*(\d+)$").captures(value) {
        return caps[1].parse().ok();
    }
    lazy_regex::regex!(
        r"^https?://[^/]+/[\w\-.]+/[\w\-.]+/pull/(\d+)(?:[/?#].*)?$"
    )
    .captures(value)
    .and_then(|caps| caps[1].parse().ok())
}

impl Stack {
    /// Discover the stack between `trunk_oid` and `HEAD`.
    pub fn discover(git: &Git, trunk_oid: Oid, trunk: &str) -> Result<Self> {
        log::debug!("discovering local stack since {trunk} ({trunk_oid})");
        let oids = git.commits_since(trunk_oid)?;
        if oids.is_empty() {
            log::debug!("no commits found above {trunk_oid}");
            return Ok(Self {
                trunk: trunk.to_string(),
                base: trunk_oid,
                layers: Vec::new(),
            });
        }

        let base = git.parent_of(oids[0])?;
        if !git.is_ancestor(base, trunk_oid)? {
            bail!(
                "the stack is based on {}, which is not an ancestor of {trunk}.\n\
                 Run `git pull --rebase` so the stack sits on a commit that \
                 exists upstream.",
                git.short_id(base)?
            );
        }

        let mut layers = Vec::with_capacity(oids.len());
        for oid in oids {
            let message = CommitMessage::parse(&git.message_of(oid)?);
            let pr = message.get(PULL_REQUEST).and_then(parse_pr_ref);
            let mut dep_specs = Vec::new();
            for raw in message.get_all(DEPENDS_ON) {
                dep_specs.extend(parse_dep_specs(raw, trunk)?);
            }
            let dep_spec = dep_specs.first().cloned();
            layers.push(Layer {
                commit: oid,
                parent: git.parent_of(oid)?,
                message,
                pr,
                dep_spec,
                dep_specs,
                dep: Dep::Main, // placeholder; set by resolve_deps
                deps: Vec::new(),
                merged_pr_deps: Vec::new(),
            });
        }

        let mut stack = Self {
            trunk: trunk.to_string(),
            base,
            layers,
        };
        stack.resolve_deps(git)?;
        log::debug!(
            "discovered {} layer(s) based on {base}",
            stack.layers.len()
        );
        Ok(stack)
    }

    /// Resolve every layer's `dep_specs` into `deps` and `dep`, and validate the graph.
    fn resolve_deps(&mut self, git: &Git) -> Result<()> {
        let by_pr: HashMap<u64, usize> = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.pr.map(|pr| (pr, i)))
            .collect();

        let mut resolved = Vec::with_capacity(self.layers.len());
        for (i, layer) in self.layers.iter().enumerate() {
            let mut deps = Vec::new();
            if layer.dep_specs.is_empty() {
                let d = if i == 0 { Dep::Main } else { Dep::Layer(i - 1) };
                deps.push(d);
            } else {
                for spec in &layer.dep_specs {
                    let d = match spec {
                        DepSpec::Main => Dep::Main,
                        DepSpec::Pr(n) => match by_pr.get(n) {
                            Some(&j) => Dep::Layer(j),
                            None => Dep::ExternalPr(*n),
                        },
                        DepSpec::Commit(prefix) => {
                            let oid = git
                                .repo()
                                .revparse_single(prefix)
                                .map_err(|_| {
                                    eyre!(
                                        "`{DEPENDS_ON}: {prefix}` does not name a \
                                         commit in this repository"
                                    )
                                })?
                                .id();
                            match self
                                .layers
                                .iter()
                                .position(|l| l.commit == oid)
                            {
                                Some(j) => Dep::Layer(j),
                                None => bail!(
                                    "`{DEPENDS_ON}: {prefix}` resolves to {}, which is \
                                     not part of this stack",
                                    git.short_id(oid)?
                                ),
                            }
                        }
                    };
                    // Forward references and self-references would make the
                    // topological order invalid, and cycles impossible to submit.
                    if let Dep::Layer(j) = d
                        && j >= i
                    {
                        bail!(
                            "`{}` declares a dependency on `{}`, which comes later in \
                             the stack. Reorder the commits so dependencies come first.",
                            self.layers[i].subject(),
                            self.layers[j].subject(),
                        );
                    }
                    if !deps.contains(&d) {
                        deps.push(d);
                    }
                }
            }

            let dep = Layer::compute_effective_dep(&deps);
            resolved.push((dep, deps));
        }

        for (layer, (dep, deps)) in self.layers.iter_mut().zip(resolved) {
            layer.dep = dep;
            layer.deps = deps;
        }
        self.reduce_transitive_deps();
        Ok(())
    }

    /// Transitive reduction of `layer.deps`: if a layer declares dependencies on
    /// both `u` and `v` where `v` already transitively depends on `u` (`u` is an
    /// ancestor of `v` in the dependency DAG), `u` is redundant because `v`'s
    /// effective tree already includes `u`. Removing subsumed `Dep::Layer(u)`
    /// entries ensures that whenever a layer's open dependencies form a single
    /// linear chain, `layer_deps()` has length 1 and the pull request targets
    /// the tip of that chain directly rather than a synthetic `.base` branch.
    pub fn reduce_transitive_deps(&mut self) {
        let n = self.layers.len();
        let mut ancestors: Vec<HashSet<usize>> = vec![HashSet::new(); n];
        for i in 0..n {
            let mut anc = HashSet::new();
            for &d in &self.layers[i].deps {
                if let Dep::Layer(j) = d {
                    anc.insert(j);
                    anc.extend(ancestors[j].iter().copied());
                }
            }
            ancestors[i] = anc;
            let current_layers = self.layers[i].layer_deps();
            if current_layers.len() >= 2 {
                self.layers[i].deps.retain(|d| match *d {
                    Dep::Layer(u) => !current_layers
                        .iter()
                        .any(|&v| v != u && ancestors[v].contains(&u)),
                    _ => true,
                });
            }
            self.layers[i].dep =
                Layer::compute_effective_dep(&self.layers[i].deps);
        }
    }

    /// The tree the layer's head branch should have.
    ///
    /// For the common case where a layer depends on its immediate predecessor,
    /// this is just the local commit's tree and no merge is performed.
    pub fn effective_tree(&self, git: &Git, i: usize) -> Result<Oid> {
        let mut cache = HashMap::new();
        self.effective_tree_cached(git, i, &mut cache)
    }

    fn effective_tree_cached(
        &self,
        git: &Git,
        i: usize,
        cache: &mut HashMap<usize, Oid>,
    ) -> Result<Oid> {
        if let Some(oid) = cache.get(&i) {
            return Ok(*oid);
        }

        let layer = &self.layers[i];
        let merge_base_tree = self.merge_base_tree_cached(git, i, cache)?;
        let local_parent_tree = git.tree_of(layer.parent)?;
        let own_tree = git.tree_of(layer.commit)?;

        // Fast path: the declared base already matches the local parent, so the
        // layer's own tree is already correct. This is every layer in a plain
        // linear stack.
        let tree = if merge_base_tree == local_parent_tree {
            own_tree
        } else {
            log::debug!(
                "computing 3-way tree merge for layer {} ({:?}) onto declared dependency",
                i,
                layer.subject()
            );
            let index =
                git.merge_trees(local_parent_tree, merge_base_tree, own_tree)?;
            if index.has_conflicts() {
                let files: Vec<String> = index
                    .conflicts()?
                    .filter_map(|c| c.ok())
                    .filter_map(|c| {
                        c.our.or(c.their).or(c.ancestor).map(|e| {
                            String::from_utf8_lossy(&e.path).into_owned()
                        })
                    })
                    .collect();
                bail!(
                    "`{}` does not apply on top of its declared dependency \
                     (conflicts in {}).\nIt probably depends on a layer between \
                     them; adjust its `{DEPENDS_ON}:` trailer.",
                    layer.subject(),
                    if files.is_empty() {
                        "<unknown>".to_string()
                    } else {
                        files.join(", ")
                    },
                );
            }
            git.write_index(index)?
        };

        cache.insert(i, tree);
        Ok(tree)
    }

    /// Compute the combined dependency tree onto which layer `i`'s commit delta
    /// (`local_parent -> commit`) should be applied.
    ///
    /// When layer `i` declares multiple open `Dep::Layer` dependencies, their
    /// effective trees are 3-way merged over `tree(self.base)` so layer `i`'s
    /// branch contains the union of all its dependencies' changes plus its own.
    fn merge_base_tree_cached(
        &self,
        git: &Git,
        i: usize,
        cache: &mut HashMap<usize, Oid>,
    ) -> Result<Oid> {
        let layer_deps = self.layers[i].layer_deps();
        match layer_deps.as_slice() {
            [] => git.tree_of(self.base),
            [j] => self.effective_tree_cached(git, *j, cache),
            [first, rest @ ..] => {
                let trunk_tree = git.tree_of(self.base)?;
                let mut combined =
                    self.effective_tree_cached(git, *first, cache)?;
                for &j in rest {
                    let dep_tree = self.effective_tree_cached(git, j, cache)?;
                    if dep_tree == combined || dep_tree == trunk_tree {
                        continue;
                    }
                    let index =
                        git.merge_trees(trunk_tree, combined, dep_tree)?;
                    if index.has_conflicts() {
                        let files: Vec<String> = index
                            .conflicts()?
                            .filter_map(|c| c.ok())
                            .filter_map(|c| {
                                c.our.or(c.their).or(c.ancestor).map(|e| {
                                    String::from_utf8_lossy(&e.path)
                                        .into_owned()
                                })
                            })
                            .collect();
                        bail!(
                            "declared dependencies of `{}` conflict with each other \
                             (conflicts in {}).",
                            self.layers[i].subject(),
                            if files.is_empty() {
                                "<unknown>".to_string()
                            } else {
                                files.join(", ")
                            },
                        );
                    }
                    combined = git.write_index(index)?;
                }
                Ok(combined)
            }
        }
    }

    /// Compute the combined dependency tree of layer `i` from precomputed
    /// `effective` trees.
    pub fn merge_base_tree_from_effective(
        &self,
        git: &Git,
        i: usize,
        effective: &[Oid],
    ) -> Result<Oid> {
        let layer_deps = self.layers[i].layer_deps();
        match layer_deps.as_slice() {
            [] => git.tree_of(self.base),
            [j] => Ok(effective[*j]),
            [first, rest @ ..] => {
                let trunk_tree = git.tree_of(self.base)?;
                let mut combined = effective[*first];
                for &j in rest {
                    let dep_tree = effective[j];
                    if dep_tree == combined || dep_tree == trunk_tree {
                        continue;
                    }
                    let index =
                        git.merge_trees(trunk_tree, combined, dep_tree)?;
                    if index.has_conflicts() {
                        bail!("dependency trees conflict");
                    }
                    combined = git.write_index(index)?;
                }
                Ok(combined)
            }
        }
    }

    /// The tree the layer's *base* should have, i.e. the effective tree of
    /// whatever it depends on.
    ///
    /// This is the left-hand side of the patch the pull request ought to
    /// display, and it is computed purely from local state — deliberately not
    /// from the remote branch tip, which may be stale.
    pub fn dep_tree(&self, git: &Git, i: usize) -> Result<Oid> {
        let mut cache = HashMap::new();
        self.base_tree_cached(git, i, &mut cache)
    }

    fn base_tree_cached(
        &self,
        git: &Git,
        i: usize,
        cache: &mut HashMap<usize, Oid>,
    ) -> Result<Oid> {
        self.merge_base_tree_cached(git, i, cache)
    }

    /// Boolean mask of layers included in `selection` together with all of
    /// their transitive `Dep::Layer` dependencies.
    pub fn needed_layers(&self, selection: &LayerSelection) -> Vec<bool> {
        let n = self.layers.len();
        let mut needed = vec![false; n];
        for i in (0..n).rev() {
            if selection.contains(i) || needed[i] {
                needed[i] = true;
                for j in self.layers[i].layer_deps() {
                    needed[j] = true;
                }
            }
        }
        needed
    }

    /// Effective tree and dependency tree for every layer, in stack order.
    ///
    /// Computed with a single shared cache, so a deep DAG costs one merge per
    /// layer rather than one per layer per query.
    pub fn all_trees(&self, git: &Git) -> Result<Trees> {
        self.trees_for(git, &LayerSelection::All)
    }

    /// Effective tree and dependency tree for the layers selected by
    /// `selection` (and their transitive `Dep::Layer` ancestors). Unselected
    /// layers that no selected layer depends on are filled with their raw
    /// local commit/parent trees so a conflict in an unrelated layer lower in
    /// the branch does not block `--cherry-pick` or updating another stack.
    pub fn trees_for(
        &self,
        git: &Git,
        selection: &LayerSelection,
    ) -> Result<Trees> {
        let n = self.layers.len();
        let needed = self.needed_layers(selection);

        let mut cache = HashMap::new();
        let mut effective = Vec::with_capacity(n);
        let mut dep = Vec::with_capacity(n);
        for (i, &is_needed) in needed.iter().enumerate() {
            if is_needed {
                dep.push(self.base_tree_cached(git, i, &mut cache)?);
                effective.push(self.effective_tree_cached(git, i, &mut cache)?);
            } else {
                dep.push(git.tree_of(self.layers[i].parent)?);
                effective.push(git.tree_of(self.layers[i].commit)?);
            }
        }
        Ok(Trees { effective, dep })
    }

    /// Like [`Self::all_trees`], but falls back to raw local parent/commit
    /// trees for any layer that fails to merge onto its declared dependency, so
    /// read-only `nspr status` can still render the stack table.
    pub fn all_trees_lenient(&self, git: &Git) -> Result<Trees> {
        let n = self.layers.len();
        let mut cache = HashMap::new();
        let mut effective = Vec::with_capacity(n);
        let mut dep = Vec::with_capacity(n);
        for i in 0..n {
            let d = match self.base_tree_cached(git, i, &mut cache) {
                Ok(tree) => tree,
                Err(e) => {
                    log::debug!(
                        "base_tree_cached failed for layer {i} ({e}); falling back to local parent tree"
                    );
                    git.tree_of(self.layers[i].parent)?
                }
            };
            let eff = match self.effective_tree_cached(git, i, &mut cache) {
                Ok(tree) => tree,
                Err(e) => {
                    log::debug!(
                        "effective_tree_cached failed for layer {i} ({e}); falling back to local commit tree"
                    );
                    let own = git.tree_of(self.layers[i].commit)?;
                    cache.insert(i, own);
                    own
                }
            };
            dep.push(d);
            effective.push(eff);
        }
        Ok(Trees { effective, dep })
    }

    /// Layers that transitively depend on `i`, in stack order.
    ///
    /// This is the set that `land` must retarget and repair; siblings are left
    /// alone.
    pub fn dependents_of(&self, i: usize) -> Vec<usize> {
        let mut marked: HashSet<usize> = HashSet::from([i]);
        let mut out = Vec::new();
        for (j, layer) in self.layers.iter().enumerate().skip(i + 1) {
            if let Dep::Layer(k) = layer.dep
                && marked.contains(&k)
            {
                marked.insert(j);
                out.push(j);
            }
        }
        out
    }

    /// Direct dependents of `i` only.
    pub fn direct_dependents_of(&self, i: usize) -> Vec<usize> {
        self.layers
            .iter()
            .enumerate()
            .filter(|(_, l)| l.dep == Dep::Layer(i))
            .map(|(j, _)| j)
            .collect()
    }

    /// Ordered bottom-to-top pull request number chains (`len >= 2`) suitable
    /// for registering with GitHub's native Stacks API (`/repos/{owner}/{repo}/stacks`).
    pub fn pr_chains(&self) -> Vec<Vec<u64>> {
        self.pr_chains_for(&LayerSelection::All)
    }

    /// Ordered bottom-to-top pull request number chains (`len >= 2`), optionally
    /// restricted to layers selected by `selection`.
    pub fn pr_chains_for(&self, selection: &LayerSelection) -> Vec<Vec<u64>> {
        let mut visited = vec![false; self.layers.len()];
        let mut chains = Vec::new();
        for start in 0..self.layers.len() {
            if visited[start] || self.layers[start].pr.is_none() {
                continue;
            }
            if !selection.contains(start) {
                continue;
            }
            let is_root = match self.layers[start].dep {
                Dep::Main | Dep::ExternalPr(_) => true,
                Dep::Layer(p) => visited[p] || self.layers[p].pr.is_none(),
            };
            if !is_root {
                continue;
            }
            let mut chain = Vec::new();
            let mut cur = Some(start);
            while let Some(idx) = cur {
                if visited[idx] {
                    break;
                }
                let Some(pr_num) = self.layers[idx].pr else {
                    break;
                };
                visited[idx] = true;
                chain.push(pr_num);
                cur =
                    self.direct_dependents_of(idx).into_iter().find(|&child| {
                        !visited[child] && self.layers[child].pr.is_some()
                    });
            }
            if chain.len() >= 2 {
                chains.push(chain);
            }
        }
        chains
    }

    /// All layers grouped into connected components of the dependency graph
    /// (including layers that do not have a pull request yet).
    pub fn components(&self) -> Vec<Vec<usize>> {
        let n = self.layers.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }

        for (i, layer) in self.layers.iter().enumerate() {
            for j in layer.layer_deps() {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a] = b;
            }
        }

        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut root_to_component: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            let root = find(&mut parent, i);
            match root_to_component.get(&root) {
                Some(&slot) => components[slot].push(i),
                None => {
                    root_to_component.insert(root, components.len());
                    components.push(vec![i]);
                }
            }
        }
        components
    }

    /// Return the connected component of layers containing `index`.
    pub fn component_of(&self, index: usize) -> Vec<usize> {
        self.components()
            .into_iter()
            .find(|c| c.contains(&index))
            .unwrap_or_default()
    }

    /// True if `component` is a simple linear chain rooted at the trunk (or an
    /// external pull request) where each subsequent layer depends on the
    /// immediately preceding layer in `component`.
    pub fn is_component_linear(&self, component: &[usize]) -> bool {
        let Some(&first) = component.first() else {
            return true;
        };
        if component
            .iter()
            .any(|&i| self.layers[i].has_multiple_layer_deps())
        {
            return false;
        }
        if !matches!(self.layers[first].dep, Dep::Main | Dep::ExternalPr(_)) {
            return false;
        }
        for window in component.windows(2) {
            if self.layers[window[1]].dep != Dep::Layer(window[0]) {
                return false;
            }
        }
        true
    }

    /// Layers grouped into connected components of the dependency graph,
    /// counting only layers that have a pull request.
    ///
    /// Two layers belong to the same component when one depends on the other,
    /// directly or through a chain of other layers. A component of one is a
    /// standalone pull request: it shares nothing with its neighbours in the
    /// local commit order and must not be presented to reviewers as if it did.
    pub fn pr_components(&self) -> Vec<Vec<usize>> {
        let n = self.layers.len();
        let mut parent: Vec<usize> = (0..n).collect();

        fn find(parent: &mut [usize], mut i: usize) -> usize {
            while parent[i] != i {
                parent[i] = parent[parent[i]];
                i = parent[i];
            }
            i
        }

        for (i, layer) in self.layers.iter().enumerate() {
            if self.layers[i].pr.is_none() {
                continue;
            }
            for j in layer.layer_deps() {
                if self.layers[j].pr.is_some() {
                    let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                    parent[a] = b;
                }
            }
        }

        let mut components: Vec<Vec<usize>> = Vec::new();
        let mut root_to_component: HashMap<usize, usize> = HashMap::new();
        for i in 0..n {
            if self.layers[i].pr.is_none() {
                continue;
            }
            let root = find(&mut parent, i);
            match root_to_component.get(&root) {
                Some(&slot) => components[slot].push(i),
                None => {
                    root_to_component.insert(root, components.len());
                    components.push(vec![i]);
                }
            }
        }
        components
    }

    /// True if layer `i` either depends on another layer in the stack or has
    /// other layers depending on it. An independent root (`Dep::Main`) with
    /// no dependents is standalone (`false`).
    pub fn is_layer_stacked(&self, i: usize) -> bool {
        self.layers[i].dep != Dep::Main
            || !self.layers[i].layer_deps().is_empty()
            || self.layers.iter().any(|l| l.layer_deps().contains(&i))
    }

    /// True if the stack has no branches: the bottom layer sits on the trunk
    /// and every subsequent layer depends on the layer immediately below it.
    pub fn is_linear(&self) -> bool {
        let all: Vec<usize> = (0..self.layers.len()).collect();
        self.is_component_linear(&all)
    }

    /// Rewrite `Depends-On:` trailers on any surviving layer whose dependency
    /// would otherwise change when `removed` commits disappear from the linear
    /// branch. Returns the (possibly rewritten) commit OIDs for all layers.
    pub fn rewrite_deps_for_removal(
        &self,
        git: &Git,
        removed: &HashSet<usize>,
        rewrite_explicit_pr_refs: bool,
    ) -> Result<Vec<Oid>> {
        if removed.is_empty() {
            return Ok(self.layers.iter().map(|l| l.commit).collect());
        }

        let mut messages: Vec<CommitMessage> =
            Vec::with_capacity(self.layers.len());
        for layer in &self.layers {
            messages.push(CommitMessage::parse(&git.message_of(layer.commit)?));
        }

        let mut any_rewritten = false;
        let mut prev_survivor: Option<usize> = None;

        for (i, layer) in self.layers.iter().enumerate() {
            if removed.contains(&i) {
                continue;
            }

            if layer.dep_specs.len() >= 2 {
                let mut any_entry_needs_rewrite = false;
                let mut new_entries: Vec<String> = Vec::new();
                for (spec, &dep) in layer.dep_specs.iter().zip(&layer.deps) {
                    let dep_was_removed =
                        matches!(dep, Dep::Layer(p) if removed.contains(&p));
                    let entry_needs_rewrite = match spec {
                        DepSpec::Commit(_) => dep_was_removed,
                        DepSpec::Pr(_) => {
                            rewrite_explicit_pr_refs && dep_was_removed
                        }
                        DepSpec::Main => false,
                    };
                    if entry_needs_rewrite {
                        any_entry_needs_rewrite = true;
                        if !rewrite_explicit_pr_refs
                            && let Dep::Layer(p) = dep
                            && let Some(n) = self.layers[p].pr
                        {
                            let s = format!("#{n}");
                            if !new_entries.contains(&s) {
                                new_entries.push(s);
                            }
                            continue;
                        }
                        let mut inherited = dep;
                        while let Dep::Layer(p) = inherited {
                            if removed.contains(&p) {
                                inherited = self.layers[p].dep;
                            } else {
                                break;
                            }
                        }
                        let s = match inherited {
                            Dep::Main => continue,
                            Dep::ExternalPr(n) => format!("#{n}"),
                            Dep::Layer(p) => match self.layers[p].pr {
                                Some(n) => format!("#{n}"),
                                None => git.short_id(self.layers[p].commit)?,
                            },
                        };
                        if !new_entries.contains(&s) {
                            new_entries.push(s);
                        }
                    } else {
                        let s = match spec {
                            DepSpec::Main => self.trunk.clone(),
                            DepSpec::Pr(n) => format!("#{n}"),
                            DepSpec::Commit(c) => c.clone(),
                        };
                        if !new_entries.contains(&s) {
                            new_entries.push(s);
                        }
                    }
                }
                if any_entry_needs_rewrite {
                    let spec_str = if new_entries.is_empty() {
                        self.trunk.clone()
                    } else {
                        new_entries.join(", ")
                    };
                    if messages[i].get(DEPENDS_ON) != Some(spec_str.as_str()) {
                        messages[i].set(DEPENDS_ON, &spec_str);
                        any_rewritten = true;
                    }
                }
                prev_survivor = Some(i);
                continue;
            }

            let mut inherited = layer.dep;
            while let Dep::Layer(p) = inherited {
                if removed.contains(&p) {
                    inherited = self.layers[p].dep;
                } else {
                    break;
                }
            }

            let implicit = match prev_survivor {
                None => Dep::Main,
                Some(prev) => Dep::Layer(prev),
            };

            let dep_was_removed =
                matches!(layer.dep, Dep::Layer(p) if removed.contains(&p));

            let needs_rewrite = match &layer.dep_spec {
                None => inherited != implicit,
                Some(DepSpec::Commit(_)) => dep_was_removed,
                Some(DepSpec::Pr(_)) => {
                    rewrite_explicit_pr_refs && dep_was_removed
                }
                Some(DepSpec::Main) => false,
            };

            if needs_rewrite {
                let spec_str = match inherited {
                    Dep::Main => self.trunk.clone(),
                    Dep::ExternalPr(n) => format!("#{n}"),
                    Dep::Layer(p) => match self.layers[p].pr {
                        Some(n) => format!("#{n}"),
                        None => git.short_id(self.layers[p].commit)?,
                    },
                };
                if messages[i].get(DEPENDS_ON) != Some(spec_str.as_str()) {
                    messages[i].set(DEPENDS_ON, &spec_str);
                    any_rewritten = true;
                }
            }

            prev_survivor = Some(i);
        }

        if any_rewritten {
            let pairs: Vec<(Oid, String)> = self
                .layers
                .iter()
                .zip(&messages)
                .map(|(l, m)| (l.commit, m.render()))
                .collect();
            git.rewrite_messages(self.base, &pairs)
        } else {
            Ok(self.layers.iter().map(|l| l.commit).collect())
        }
    }

    /// Drop `removed` layers and rebase the surviving commits onto `onto`,
    /// rewriting `Depends-On:` trailers on any surviving layer whose dependency
    /// would otherwise change when the removed commits disappear from the
    /// linear branch.
    pub fn rebase_without(
        &self,
        git: &Git,
        removed: &HashSet<usize>,
        onto: Oid,
        rewrite_explicit_pr_refs: bool,
    ) -> Result<()> {
        let source_commits = self.rewrite_deps_for_removal(
            git,
            removed,
            rewrite_explicit_pr_refs,
        )?;
        let unlanded: Vec<Oid> = source_commits
            .into_iter()
            .enumerate()
            .filter(|(i, _)| !removed.contains(i))
            .map(|(_, oid)| oid)
            .collect();

        git.rebase_commits(&unlanded, onto)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_dep_spec_forms() {
        assert_eq!(parse_dep_spec("main", "main").unwrap(), DepSpec::Main);
        assert_eq!(parse_dep_spec("MAIN", "main").unwrap(), DepSpec::Main);
        assert_eq!(parse_dep_spec("trunk", "trunk").unwrap(), DepSpec::Main);
        assert_eq!(parse_dep_spec("#101", "main").unwrap(), DepSpec::Pr(101));
        assert_eq!(parse_dep_spec("101", "main").unwrap(), DepSpec::Pr(101));
        assert_eq!(
            parse_dep_spec("https://github.com/o/r/pull/101", "main").unwrap(),
            DepSpec::Pr(101)
        );
        assert_eq!(
            parse_dep_spec("a1b2c3d", "main").unwrap(),
            DepSpec::Commit("a1b2c3d".into())
        );
        assert!(parse_dep_spec("not a ref", "main").is_err());
    }

    #[test]
    fn parse_dep_specs_multiple() {
        assert_eq!(
            parse_dep_specs("#101, #102", "main").unwrap(),
            vec![DepSpec::Pr(101), DepSpec::Pr(102)]
        );
        assert_eq!(
            parse_dep_specs("#101 #102", "main").unwrap(),
            vec![DepSpec::Pr(101), DepSpec::Pr(102)]
        );
    }

    #[test]
    fn parse_pr_ref_forms() {
        assert_eq!(parse_pr_ref("#42"), Some(42));
        assert_eq!(parse_pr_ref("42"), Some(42));
        assert_eq!(parse_pr_ref("https://github.com/o/r/pull/42"), Some(42));
        assert_eq!(
            parse_pr_ref("https://github.com/o/r/pull/42#issue-1"),
            Some(42)
        );
        assert_eq!(parse_pr_ref("nonsense"), None);
    }
}
