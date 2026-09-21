//! Test helpers for building throwaway git repositories.
//!
//! These exist so the stack tests can construct precise commit graphs and then
//! assert on what GitHub *would* render for them, without any network access.

use git2::{Oid, Repository};

pub struct TestRepo {
    dir: tempfile::TempDir,
}

impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}

impl TestRepo {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path()).expect("git init");
        {
            let mut cfg = repo.config().expect("config");
            cfg.set_str("user.name", "Test User").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
        }
        drop(repo);
        Self { dir }
    }

    pub fn open(&self) -> Repository {
        Repository::open(self.dir.path()).expect("open repo")
    }

    /// Build a tree containing exactly `files`.
    pub fn tree_with(&self, files: &[(&str, &str)]) -> Oid {
        let repo = self.open();
        let mut builder = repo.treebuilder(None).expect("treebuilder");
        for (path, content) in files {
            assert!(
                !path.contains('/'),
                "tree_with only supports top-level files; got {path:?}"
            );
            let blob = repo.blob(content.as_bytes()).expect("blob");
            builder.insert(path, blob, 0o100644).expect("insert");
        }
        builder.write().expect("write tree")
    }

    /// Create a commit whose tree contains exactly `files`.
    pub fn commit(
        &self,
        message: &str,
        files: &[(&str, &str)],
        parents: &[Oid],
    ) -> Oid {
        let repo = self.open();
        let tree_oid = self.tree_with(files);
        let tree = repo.find_tree(tree_oid).expect("find tree");
        let sig = repo.signature().expect("signature");
        let parent_commits: Vec<_> = parents
            .iter()
            .map(|oid| repo.find_commit(*oid).expect("find parent"))
            .collect();
        let parent_refs: Vec<_> = parent_commits.iter().collect();
        repo.commit(None, &sig, &sig, message, &tree, &parent_refs)
            .expect("commit")
    }

    /// Convenience for a single-file commit.
    pub fn commit_file(
        &self,
        message: &str,
        path: &str,
        content: &str,
        parents: &[Oid],
    ) -> Oid {
        self.commit(message, &[(path, content)], parents)
    }

    /// Point a branch ref at a commit.
    pub fn set_branch(&self, name: &str, oid: Oid) {
        let repo = self.open();
        repo.reference(
            &format!("refs/heads/{name}"),
            oid,
            true,
            "testutil set_branch",
        )
        .expect("set branch");
    }
}
