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

#[derive(Debug, Clone)]
pub struct Layer {
    /// The local commit this layer represents.
    pub commit: Oid,
    /// The local parent of `commit`. May differ from the declared dependency.
    pub parent: Oid,
    pub message: CommitMessage,
    pub pr: Option<u64>,
    /// As written in the commit message, if present.
    pub dep_spec: Option<DepSpec>,
    /// Resolved dependency. Defaults to the previous layer.
    pub dep: Dep,
}

impl Layer {
    pub fn subject(&self) -> &str {
        &self.message.subject
    }
}

#[derive(Debug)]
pub struct Stack {
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
        let oids = git.commits_since(trunk_oid)?;
        if oids.is_empty() {
            return Ok(Self {
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
            let dep_spec = match message.get(DEPENDS_ON) {
                Some(v) => Some(parse_dep_spec(v, trunk)?),
                None => None,
            };
            layers.push(Layer {
                commit: oid,
                parent: git.parent_of(oid)?,
                message,
                pr,
                dep_spec,
                dep: Dep::Main, // placeholder; set by resolve_deps
            });
        }

        let mut stack = Self { base, layers };
        stack.resolve_deps(git)?;
        Ok(stack)
    }

    /// Resolve every layer's `dep_spec` into a [`Dep`], and validate the graph.
    fn resolve_deps(&mut self, git: &Git) -> Result<()> {
        let by_pr: HashMap<u64, usize> = self
            .layers
            .iter()
            .enumerate()
            .filter_map(|(i, l)| l.pr.map(|pr| (pr, i)))
            .collect();

        let mut resolved = Vec::with_capacity(self.layers.len());
        for (i, layer) in self.layers.iter().enumerate() {
            let dep = match &layer.dep_spec {
                None if i == 0 => Dep::Main,
                None => Dep::Layer(i - 1),
                Some(DepSpec::Main) => Dep::Main,
                Some(DepSpec::Pr(n)) => match by_pr.get(n) {
                    Some(&j) => Dep::Layer(j),
                    None => Dep::ExternalPr(*n),
                },
                Some(DepSpec::Commit(prefix)) => {
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
                    match self.layers.iter().position(|l| l.commit == oid) {
                        Some(j) => Dep::Layer(j),
                        None => bail!(
                            "`{DEPENDS_ON}: {prefix}` resolves to {}, which is \
                             not part of this stack",
                            git.short_id(oid)?
                        ),
                    }
                }
            };

            // Forward references and self-references would make the topological
            // order invalid, and cycles impossible to submit.
            if let Dep::Layer(j) = dep
                && j >= i
            {
                bail!(
                    "`{}` declares a dependency on `{}`, which comes later in \
                     the stack. Reorder the commits so dependencies come first.",
                    self.layers[i].subject(),
                    self.layers[j].subject(),
                );
            }

            resolved.push(dep);
        }

        for (layer, dep) in self.layers.iter_mut().zip(resolved) {
            layer.dep = dep;
        }
        Ok(())
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
        let base_tree = self.base_tree_cached(git, i, cache)?;
        let local_parent_tree = git.tree_of(layer.parent)?;
        let own_tree = git.tree_of(layer.commit)?;

        // Fast path: the declared base already matches the local parent, so the
        // layer's own tree is already correct. This is every layer in a plain
        // linear stack.
        let tree = if base_tree == local_parent_tree {
            own_tree
        } else {
            let index =
                git.merge_trees(local_parent_tree, base_tree, own_tree)?;
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
        match self.layers[i].dep {
            Dep::Main | Dep::ExternalPr(_) => git.tree_of(self.base),
            Dep::Layer(j) => self.effective_tree_cached(git, j, cache),
        }
    }

    /// Effective tree and dependency tree for every layer, in stack order.
    ///
    /// Computed with a single shared cache, so a deep DAG costs one merge per
    /// layer rather than one per layer per query.
    pub fn all_trees(&self, git: &Git) -> Result<Trees> {
        let mut cache = HashMap::new();
        let mut effective = Vec::with_capacity(self.layers.len());
        let mut dep = Vec::with_capacity(self.layers.len());
        for i in 0..self.layers.len() {
            dep.push(self.base_tree_cached(git, i, &mut cache)?);
            effective.push(self.effective_tree_cached(git, i, &mut cache)?);
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
        let mut visited = vec![false; self.layers.len()];
        let mut chains = Vec::new();
        for start in 0..self.layers.len() {
            if visited[start] || self.layers[start].pr.is_none() {
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
            if let Dep::Layer(j) = layer.dep
                && self.layers[j].pr.is_some()
            {
                let (a, b) = (find(&mut parent, i), find(&mut parent, j));
                parent[a] = b;
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
        self.layers[i].dep != Dep::Main || !self.dependents_of(i).is_empty()
    }

    /// True if the stack has no branches: the bottom layer sits on the trunk
    /// and every subsequent layer depends on the layer immediately below it.
    pub fn is_linear(&self) -> bool {
        if self.layers.is_empty() {
            return true;
        }
        if self.layers[0].dep != Dep::Main {
            return false;
        }
        for (i, layer) in self.layers.iter().enumerate().skip(1) {
            if layer.dep != Dep::Layer(i - 1) {
                return false;
            }
        }
        true
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
