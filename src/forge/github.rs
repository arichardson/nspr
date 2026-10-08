//! The real GitHub backend.
//!
//! # Why the pull-request read is hand-written GraphQL
//!
//! REST exposes neither `mergeStateStatus` nor `autoMergeRequest`, and the
//! `mergeable` it does expose is a lazily-computed tri-state that arrives as
//! `null` on a cold pull request. So the read has to be GraphQL even though
//! everything that *writes* is REST.
//!
//! spr generates its GraphQL types with `graphql_client`, which means checking
//! in GitHub's 1.4 MB `schema.docs.graphql` and running a proc macro over it on
//! every build. For one query with thirteen fields that is a poor trade: the
//! generated types are invisible at the call site, and a recorded response
//! cannot be fed to them without a live schema. Sending the query text and
//! deserialising into the structs below instead keeps the wire shape next to
//! the code that maps it, and lets the tests at the bottom of this file run the
//! real mapping over a checked-in payload.
//!
//! # Objects, not just SHAs
//!
//! The APIs hand out SHAs. The engine immediately reads *trees* and walks
//! history for those SHAs, so anything this module returns as an [`Oid`] has to
//! be backed by a local object first — see [`crate::git_remote::GitRemote`].

use std::cell::RefCell;
use std::sync::Arc;

use async_trait::async_trait;
use color_eyre::eyre::{Error, Result, WrapErr as _, bail, eyre};
use git2::Oid;
use log::debug;
use octocrab::Octocrab;
use octocrab::params::pulls::{MergeMethod, State};
use serde::Deserialize;

use super::{
    CheckCounts, Comment, CreatePr, Forge, ListedPr, MergeState, Mergeable,
    PrState, Protection, PullRequest, PullRequestUpdate, PushSpec,
    RepoMergeSettings, ReviewDecision, ReviewSummary, SquashMerge,
};
use crate::git_remote::{GitRemote, PushRejected};

const PULL_REQUEST_QUERY: &str =
    include_str!("../gql/pullrequest_query.graphql");

const SEARCH_PULL_REQUESTS_QUERY: &str = r#"
query($query: String!) {
  search(query: $query, type: ISSUE, first: 100) {
    nodes {
      ... on PullRequest {
        number
        title
        state
        isDraft
        baseRefName
        headRefName
        reviewDecision
        url
      }
    }
  }
}
"#;

pub struct GitHubForge {
    owner: String,
    repo: String,
    api: Octocrab,
    remote: GitRemote,
    /// Filled in on first use: the login is needed to recognise our own
    /// comments, and an extra `/user` round trip per `nspr diff` is wasteful
    /// when it never changes within a run.
    login: RefCell<Option<String>>,
    merge_settings: std::cell::OnceCell<RepoMergeSettings>,
}

impl GitHubForge {
    /// `token` authenticates both the API and the git transport.
    pub fn new(
        repo: Arc<git2::Repository>,
        owner: impl Into<String>,
        name: impl Into<String>,
        token: String,
    ) -> Result<Self> {
        let owner = owner.into();
        let name = name.into();
        let url = format!("https://github.com/{owner}/{name}.git");
        Self::with_remote_url(repo, owner, name, token, url)
    }

    /// Like [`Self::new`], but pushing and fetching over `url`.
    ///
    /// Some networks only allow git over ssh, and a token that is good enough
    /// for the API may not be usable as an HTTP password (SAML-protected
    /// organisations reject unauthorised tokens at the git layer while still
    /// answering API calls).
    pub fn with_remote_url(
        repo: Arc<git2::Repository>,
        owner: impl Into<String>,
        name: impl Into<String>,
        token: String,
        url: String,
    ) -> Result<Self> {
        let api = Octocrab::builder().personal_token(token.clone()).build()?;
        Ok(Self {
            owner: owner.into(),
            repo: name.into(),
            api,
            remote: GitRemote::new(repo, url, token),
            login: RefCell::new(None),
            merge_settings: std::cell::OnceCell::new(),
        })
    }

    pub fn remote(&self) -> &GitRemote {
        &self.remote
    }

    /// The authenticated user's login.
    ///
    /// Callers building a [`crate::config::Config`] want this too; it is
    /// cached, so asking here costs nothing extra.
    pub async fn viewer_login(&self) -> Result<String> {
        let cached = self.login.borrow().clone();
        if let Some(login) = cached {
            return Ok(login);
        }
        debug!("API GET /user (viewer_login)");
        let login = self
            .api
            .current()
            .user()
            .await
            .wrap_err(
                "could not identify the authenticated user. The token may be \
                 expired; run `gh auth login`.",
            )?
            .login;
        debug!("  -> viewer login = {login}");
        *self.login.borrow_mut() = Some(login.clone());
        Ok(login)
    }

    /// Fetch the authenticated user's login and a branch tip OID in a single
    /// GraphQL request, caching the login for subsequent calls.
    pub async fn viewer_login_and_branch_oid(
        &self,
        branch: &str,
    ) -> Result<(String, Option<Oid>)> {
        let cached = self.login.borrow().clone();
        if let Some(login) = cached {
            let oid = self.branch_oid(branch).await?;
            return Ok((login, oid));
        }

        debug!("API POST /graphql ViewerAndRef(refs/heads/{branch})");
        let body = serde_json::json!({
            "query": VIEWER_AND_BRANCH_OID_QUERY,
            "variables": {
                "owner": self.owner,
                "repo": self.repo,
                "qualifiedName": format!("refs/heads/{branch}"),
            },
        });
        let response: GqlResponse<ViewerAndRefQueryData> =
            self.api.post("/graphql", Some(&body)).await.wrap_err(
                "could not identify the authenticated user. The token may be \
                 expired; run `gh auth login`.",
            )?;
        let errors: Vec<String> =
            response.errors.into_iter().map(|e| e.message).collect();
        let Some(data) = response.data else {
            bail!(
                "could not identify the authenticated user{}. The token may be \
                 expired; run `gh auth login`.",
                error_suffix(&errors)
            );
        };
        let Some(viewer) = data.viewer else {
            bail!(
                "could not identify the authenticated user{}. The token may be \
                 expired; run `gh auth login`.",
                error_suffix(&errors)
            );
        };
        let login = viewer.login;
        debug!("  -> viewer login = {login}");
        *self.login.borrow_mut() = Some(login.clone());

        let oid = match data
            .repository
            .and_then(|r| r.git_ref)
            .and_then(|g| g.target)
            .map(|t| t.oid)
        {
            Some(oid_str) => {
                debug!("  -> branch {branch} = {oid_str}");
                Some(Oid::from_str(&oid_str)?)
            }
            None => {
                debug!("  -> branch {branch} not found");
                None
            }
        };
        Ok((login, oid))
    }

    /// True if `pr.base_oid` is missing locally, `pr.head_oid` is already in
    /// the local object database, and `refs/nspr/root/<pr.number>` records a
    /// local root commit for this pull request's branch so the engine can
    /// derive the branch's merge-base from the root's parent without fetching
    /// an advanced remote base tip.
    fn can_skip_base_oid_fetch(&self, pr: &PullRequest) -> bool {
        if self.remote.has_object(pr.base_oid) {
            return true;
        }
        let repo = self.remote.repo();
        if repo.find_commit(pr.head_oid).is_err() {
            debug!(
                "  -> #{}: head_oid {} missing locally; will fetch base_oid {}",
                pr.number, pr.head_oid, pr.base_oid
            );
            return false;
        }
        let root_ref = crate::refs::root_ref_name(pr.number);
        let Some(root_oid) =
            repo.find_reference(&root_ref).ok().and_then(|r| r.target())
        else {
            debug!(
                "  -> #{}: no local `{root_ref}`; falling back to fetching base_oid {}",
                pr.number, pr.base_oid
            );
            return false;
        };
        if let Ok(root_commit) = repo.find_commit(root_oid)
            && root_commit.parent_count() == 1
            && let Ok(parent_oid) = root_commit.parent_id(0)
            && repo.find_commit(parent_oid).is_ok()
            && (root_oid == pr.head_oid
                || repo
                    .graph_descendant_of(pr.head_oid, root_oid)
                    .unwrap_or(false))
        {
            debug!(
                "  -> #{}: skipping fetch of missing base_oid {} (using local `{root_ref}` = {root_oid})",
                pr.number, pr.base_oid
            );
            true
        } else {
            debug!(
                "  -> #{}: local `{root_ref}` ({root_oid}) not an ancestor of head_oid {}; falling back to fetching base_oid {}",
                pr.number, pr.head_oid, pr.base_oid
            );
            false
        }
    }
}

#[async_trait(?Send)]
impl Forge for GitHubForge {
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest> {
        debug!("API POST /graphql PullRequest(number={number})");
        let body = serde_json::json!({
            "query": PULL_REQUEST_QUERY,
            "variables": {
                "owner": self.owner,
                "name": self.repo,
                "number": number,
            },
        });
        // Deliberately not `Octocrab::graphql`. It deserializes from the
        // response's `data` member rather than the response, so our envelope
        // below would never see `data` or `errors`; and it turns any response
        // carrying errors into a hard failure, discarding the data alongside
        // them, which is exactly the partial success we want to tolerate.
        let response: GqlResponse<QueryData> =
            self.api.post("/graphql", Some(&body)).await.wrap_err_with(
                || format!("could not read pull request #{number} from GitHub"),
            )?;

        let pr = pull_request_from(pull_request_node(response, number)?)?;
        debug!(
            "  -> #{number}: state={:?} base={} ({}) head={} ({}) mergeable={:?} merge_state={:?}",
            pr.state,
            pr.base,
            pr.base_oid,
            pr.head,
            pr.head_oid,
            pr.mergeable,
            pr.merge_state
        );
        let mut to_fetch = vec![pr.head_oid];
        if pr.base_oid != Oid::ZERO_SHA1 && !self.can_skip_base_oid_fetch(&pr) {
            to_fetch.push(pr.base_oid);
        }
        if let Some(mc) = pr.merge_commit {
            to_fetch.push(mc);
        }
        self.remote.fetch_objects(&to_fetch)?;
        Ok(pr)
    }

    async fn get_pull_requests(
        &self,
        numbers: &[u64],
    ) -> Result<Vec<PullRequest>> {
        if numbers.is_empty() {
            return Ok(Vec::new());
        }
        if numbers.len() == 1 {
            return Ok(vec![self.get_pull_request(numbers[0]).await?]);
        }

        debug!("API POST /graphql PullRequests(numbers={numbers:?})");
        let query = build_batch_pull_requests_query(numbers);
        let body = serde_json::json!({
            "query": query,
            "variables": {
                "owner": self.owner,
                "name": self.repo,
            },
        });
        let response: GqlResponse<BatchQueryData> = self
            .api
            .post("/graphql", Some(&body))
            .await
            .wrap_err("could not read pull requests from GitHub")?;

        let nodes = batch_pull_request_nodes(response, numbers)?;
        let mut prs = Vec::with_capacity(nodes.len());
        let mut to_fetch = Vec::with_capacity(nodes.len() * 2);
        for node in nodes {
            let pr = pull_request_from(node)?;
            debug!(
                "  -> #{}: state={:?} base={} ({}) head={} ({}) mergeable={:?} merge_state={:?}",
                pr.number,
                pr.state,
                pr.base,
                pr.base_oid,
                pr.head,
                pr.head_oid,
                pr.mergeable,
                pr.merge_state
            );
            to_fetch.push(pr.head_oid);
            if pr.base_oid != Oid::ZERO_SHA1
                && !self.can_skip_base_oid_fetch(&pr)
            {
                to_fetch.push(pr.base_oid);
            }
            if let Some(mc) = pr.merge_commit {
                to_fetch.push(mc);
            }
            prs.push(pr);
        }
        self.remote.fetch_objects(&to_fetch)?;
        Ok(prs)
    }

    async fn create_pull_request(&self, req: CreatePr) -> Result<u64> {
        debug!(
            "API POST /repos/{}/{}/pulls head={} base={} draft={} title={:?}",
            self.owner, self.repo, req.head, req.base, req.draft, req.title
        );
        let pr = self
            .api
            .pulls(&self.owner, &self.repo)
            .create(req.title, &req.head, &req.base)
            .body(req.body)
            .draft(Some(req.draft))
            .send()
            .await
            .wrap_err_with(|| {
                format!(
                    "could not open a pull request for `{}` against `{}`. If \
                     it already exists, GitHub will say so.",
                    req.head, req.base
                )
            })?;
        debug!("  -> created PR #{}", pr.number);
        Ok(pr.number)
    }

    async fn update_pull_request(
        &self,
        number: u64,
        update: PullRequestUpdate,
    ) -> Result<()> {
        if update.is_empty() {
            return Ok(());
        }
        debug!(
            "API PATCH /repos/{}/{}/pulls/{number} base={:?} state={:?} title_updated={} body_updated={}",
            self.owner,
            self.repo,
            update.base,
            update.state,
            update.title.is_some(),
            update.body.is_some()
        );

        let pulls = self.api.pulls(&self.owner, &self.repo);
        let mut request = pulls.update(number);
        if let Some(title) = update.title {
            request = request.title(title);
        }
        if let Some(body) = update.body {
            request = request.body(body);
        }
        if let Some(base) = update.base {
            self.unstack_pr_if_stacked(number).await;
            request = request.base(base);
        }
        if let Some(state) = update.state {
            request = request.state(match state {
                PrState::Open => State::Open,
                PrState::Closed => State::Closed,
                // Merging is not a state you can PATCH into existence; it is
                // what `merge_pull_request` does.
                PrState::Merged => bail!(
                    "#{number} cannot be marked merged directly; land it \
                     instead."
                ),
            });
        }

        request
            .send()
            .await
            .wrap_err_with(|| format!("could not update #{number}"))?;
        Ok(())
    }

    async fn merge_pull_request(
        &self,
        number: u64,
        req: SquashMerge,
    ) -> Result<Oid> {
        // Title and message are always sent: left out, GitHub falls back to
        // the repository's `squash_merge_commit_message` setting, which may be
        // `COMMIT_MESSAGES` — every `[nspr]` revision commit on the head
        // branch, pasted onto the trunk. `sha` is the compare-and-swap guard:
        // GitHub rejects the merge if the head branch moved since we read it.
        //
        // Right after `git push`, `PATCH base`, or a squash-merge of the layer
        // below, GitHub recomputes `mergeable` asynchronously in the background
        // and can return transient HTTP 405 ("Base branch was modified" /
        // `mergeable == UNKNOWN`) or 409 ("Head branch was modified") for a
        // couple of seconds.
        let retry_delays_ms = [
            500_u64, 1000, 1500, 2000, 2500, 3000, 3000, 3000, 3000, 3000,
            3000, 3000,
        ];
        let mut attempt = 0;
        let merge = loop {
            debug!(
                "API PUT /repos/{}/{}/pulls/{number}/merge (squash, expected_head={}, title={:?}, attempt={})",
                self.owner,
                self.repo,
                req.expected_head,
                req.title,
                attempt + 1
            );
            let result = self
                .api
                .pulls(&self.owner, &self.repo)
                .merge(number)
                .title(req.title.clone())
                .message(req.message.clone())
                .sha(req.expected_head.to_string())
                .method(MergeMethod::Squash)
                .send()
                .await;

            match result {
                Ok(merge) => break merge,
                Err(e) => {
                    let code = status_code(&e);
                    let is_transport_or_5xx = code.is_none()
                        || matches!(code, Some(500 | 502 | 503 | 504));
                    let is_merge_in_progress = github_error_message(&e)
                        .is_some_and(|m| {
                            m.to_ascii_lowercase()
                                .contains("merge already in progress")
                        });

                    match self.get_pull_request(number).await {
                        Ok(pr) if pr.state == PrState::Merged => {
                            debug!(
                                "merge #{number} request returned error ({e}), but PR is Merged on GitHub"
                            );
                            return self.resolve_merged_pr_oid(&pr).await;
                        }
                        Ok(pr) if pr.state == PrState::Open => {
                            if let Some(landed_oid) =
                                self.detect_partial_merge_on_base(&pr).await
                            {
                                debug!(
                                    "merge #{number} request returned error ({e}), but squash commit {landed_oid} is already on `{}`; closing #{number} without retrying merge",
                                    pr.base
                                );
                                let _ = self
                                    .update_pull_request(
                                        number,
                                        PullRequestUpdate {
                                            state: Some(PrState::Closed),
                                            ..Default::default()
                                        },
                                    )
                                    .await;
                                return Ok(landed_oid);
                            }
                            if (is_transport_or_5xx
                                || is_merge_in_progress
                                || (matches!(code, Some(405 | 409))
                                    && (pr.head_oid != req.expected_head
                                        || pr.mergeable == Mergeable::Unknown
                                        || code == Some(409)
                                        || attempt < 4)))
                                && let Some(&delay_ms) =
                                    retry_delays_ms.get(attempt)
                            {
                                debug!(
                                    "merge #{number} got error (code={code:?}, head_oid={}, mergeable={:?}, merge_state={:?}); retrying in {delay_ms}ms",
                                    pr.head_oid, pr.mergeable, pr.merge_state
                                );
                                attempt += 1;
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(delay_ms),
                                )
                                .await;
                                if let Ok(pr_after) =
                                    self.get_pull_request(number).await
                                {
                                    if pr_after.state == PrState::Merged {
                                        debug!(
                                            "merge #{number} completed on GitHub while waiting to retry"
                                        );
                                        return self
                                            .resolve_merged_pr_oid(&pr_after)
                                            .await;
                                    }
                                    if pr_after.state == PrState::Open
                                        && let Some(landed_oid) = self
                                            .detect_partial_merge_on_base(
                                                &pr_after,
                                            )
                                            .await
                                    {
                                        debug!(
                                            "merge #{number} committed {landed_oid} to `{}` while waiting to retry, though PR stayed Open; closing #{number}",
                                            pr_after.base
                                        );
                                        let _ = self
                                            .update_pull_request(
                                                number,
                                                PullRequestUpdate {
                                                    state: Some(
                                                        PrState::Closed,
                                                    ),
                                                    ..Default::default()
                                                },
                                            )
                                            .await;
                                        return Ok(landed_oid);
                                    }
                                }
                                continue;
                            }
                        }
                        Err(probe_err)
                            if is_transport_or_5xx || is_merge_in_progress =>
                        {
                            if let Some(&delay_ms) =
                                retry_delays_ms.get(attempt)
                            {
                                debug!(
                                    "merge #{number} and follow-up probe failed ({probe_err}); retrying in {delay_ms}ms"
                                );
                                attempt += 1;
                                tokio::time::sleep(
                                    std::time::Duration::from_millis(delay_ms),
                                )
                                .await;
                                if let Ok(pr_after) =
                                    self.get_pull_request(number).await
                                {
                                    if pr_after.state == PrState::Merged {
                                        debug!(
                                            "merge #{number} completed on GitHub while recovering from previous error"
                                        );
                                        return self
                                            .resolve_merged_pr_oid(&pr_after)
                                            .await;
                                    }
                                    if pr_after.state == PrState::Open
                                        && let Some(landed_oid) = self
                                            .detect_partial_merge_on_base(
                                                &pr_after,
                                            )
                                            .await
                                    {
                                        debug!(
                                            "merge #{number} committed {landed_oid} to `{}` while recovering from previous error, though PR stayed Open; closing #{number}",
                                            pr_after.base
                                        );
                                        let _ = self
                                            .update_pull_request(
                                                number,
                                                PullRequestUpdate {
                                                    state: Some(
                                                        PrState::Closed,
                                                    ),
                                                    ..Default::default()
                                                },
                                            )
                                            .await;
                                        return Ok(landed_oid);
                                    }
                                }
                                continue;
                            }
                        }
                        _ => {}
                    }
                    let advice = if is_merge_in_progress {
                        "a merge of this pull request is already in progress on \
                         GitHub. Run `nspr land` again in a few seconds."
                    } else {
                        match code {
                            Some(409) => {
                                "the head branch moved since nspr read it, or the \
                                 pull request no longer merges cleanly. Run `nspr \
                                 diff`, then try again."
                            }
                            Some(405) => {
                                "GitHub declined: squash merging may be disabled for \
                                 this repository, or a required review or check is \
                                 still outstanding."
                            }
                            _ => "the merge request failed.",
                        }
                    };
                    return Err(Error::from(e)).wrap_err(format!(
                        "could not squash-merge #{number}: {advice}"
                    ));
                }
            }
        };

        if !merge.merged {
            bail!(
                "GitHub did not merge #{number}: {}",
                merge.message.as_deref().unwrap_or("no reason given")
            );
        }
        let sha = merge.sha.ok_or_else(|| {
            eyre!(
                "#{number} was merged but GitHub did not report the resulting \
                 commit, so the dependent branches cannot be repaired. Run \
                 `nspr sync`."
            )
        })?;
        debug!("  -> merged #{number} as squash commit {sha}");
        Oid::from_str(&sha).map_err(Error::from)
    }

    async fn branch_protection(
        &self,
        branch: &str,
    ) -> Result<Option<Protection>> {
        let route = format!(
            "/repos/{}/{}/branches/{branch}/protection",
            self.owner, self.repo
        );
        debug!("API GET {route}");
        match self
            .api
            .get::<ProtectionResponse, _, _>(route, None::<&()>)
            .await
        {
            Ok(protection) => Ok(Some(Protection {
                dismiss_stale_reviews: protection
                    .required_pull_request_reviews
                    .is_some_and(|reviews| reviews.dismiss_stale_reviews),
                require_up_to_date: protection
                    .required_status_checks
                    .is_some_and(|checks| checks.strict),
            })),
            // 404 is both "not protected" and "you are not an admin here";
            // 403 is the same story with a different code. Protection only
            // tunes warnings, so not knowing must never be fatal — most
            // contributors cannot read this endpoint at all.
            Err(e) if matches!(status_code(&e), Some(403 | 404)) => {
                debug!("branch protection for {branch} unreadable: {e}");
                Ok(None)
            }
            Err(e) => Err(Error::from(e)).wrap_err(format!(
                "could not read branch protection for `{branch}`"
            )),
        }
    }

    async fn branch_oid(&self, branch: &str) -> Result<Option<Oid>> {
        debug!("API POST /graphql Ref(refs/heads/{branch})");
        let body = serde_json::json!({
            "query": BRANCH_OID_QUERY,
            "variables": {
                "owner": self.owner,
                "repo": self.repo,
                "qualifiedName": format!("refs/heads/{branch}"),
            },
        });
        let response: GqlResponse<RefQueryData> =
            self.api.post("/graphql", Some(&body)).await.wrap_err_with(
                || format!("could not look up branch `{branch}` on GitHub"),
            )?;
        let Some(oid_str) = response
            .data
            .and_then(|d| d.repository)
            .and_then(|r| r.git_ref)
            .and_then(|g| g.target)
            .map(|t| t.oid)
        else {
            debug!("  -> branch {branch} not found");
            return Ok(None);
        };
        debug!("  -> branch {branch} = {oid_str}");
        Ok(Some(Oid::from_str(&oid_str)?))
    }

    async fn delete_branch(&self, branch: &str) -> Result<()> {
        debug!(
            "API DELETE /repos/{}/{}/git/refs/heads/{branch}",
            self.owner, self.repo
        );
        let res = self
            .api
            .repos(&self.owner, &self.repo)
            .delete_ref(&octocrab::params::repos::Reference::Branch(
                branch.to_string(),
            ))
            .await;
        match res {
            Ok(()) => Ok(()),
            // GitHub returns 404 or 422 ("Reference does not exist") when the
            // repository's `delete_branch_on_merge` setting already deleted the
            // head branch automatically upon merge.
            Err(e) if matches!(status_code(&e), Some(404 | 422)) => {
                debug!(
                    "branch {branch} already deleted on remote ({:?}): {e}",
                    status_code(&e)
                );
                Ok(())
            }
            Err(e) => Err(Error::from(e))
                .wrap_err(format!("could not delete remote branch `{branch}`")),
        }
    }

    async fn push(&self, specs: &[PushSpec]) -> Result<()> {
        let refspecs: Vec<String> = specs.iter().map(refspec).collect();
        debug!("git push refspecs={refspecs:?}");
        let has_metadata = specs
            .iter()
            .any(|s| s.label.is_some() || s.context.is_some());
        let custom_desc = if has_metadata {
            let targets = specs
                .iter()
                .map(|s| s.label.as_deref().unwrap_or(&s.branch))
                .collect::<Vec<_>>()
                .join(", ");
            match specs.iter().find_map(|s| s.context.as_deref()) {
                Some(ctx) => Some(format!("push ({ctx}): {targets}")),
                None => Some(format!("push: {targets}")),
            }
        } else {
            None
        };
        self.remote
            .push_with_desc(&refspecs, custom_desc.as_deref())
            .map_err(|e| match e.downcast_ref::<PushRejected>() {
                Some(rejected) if rejected.is_non_fast_forward() => e.wrap_err(
                    "the push was rejected because somebody else has pushed \
                     to the branch: run `nspr sync` and try again.",
                ),
                _ => e,
            })
    }

    async fn unused_branch_name(&self, preferred: &str) -> Result<String> {
        if self.branch_oid(preferred).await?.is_none() {
            return Ok(preferred.to_string());
        }
        for suffix in 1.. {
            let candidate = format!("{preferred}-{suffix}");
            debug!(
                "branch `{preferred}` already exists on remote; trying `{candidate}`"
            );
            if self.branch_oid(&candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        unreachable!()
    }

    async fn fetch_commit(&self, oid: Oid) -> Result<()> {
        self.remote.fetch_objects(&[oid])
    }

    async fn fetch_commits(&self, oids: &[Oid]) -> Result<()> {
        self.remote.fetch_objects(oids)
    }

    async fn list_own_comments(&self, number: u64) -> Result<Vec<Comment>> {
        let login = self.viewer_login().await?;
        debug!(
            "API GET /repos/{}/{}/issues/{number}/comments",
            self.owner, self.repo
        );
        let first = self
            .api
            .issues(&self.owner, &self.repo)
            .list_comments(number)
            .per_page(100)
            .send()
            .await
            .wrap_err_with(|| {
                format!("could not read the comments on #{number}")
            })?;
        // A busy pull request runs past one page, and the stack comment is the
        // *oldest* comment nspr wrote, so stopping at the first page would
        // eventually mean posting a second one.
        let all = self.api.all_pages(first).await?;

        Ok(all
            .into_iter()
            .filter(|comment| comment.user.login == login)
            .map(|comment| Comment {
                id: comment.id.0,
                body: comment.body.unwrap_or_default(),
            })
            .collect())
    }

    async fn create_comment(&self, number: u64, body: &str) -> Result<u64> {
        debug!(
            "API POST /repos/{}/{}/issues/{number}/comments",
            self.owner, self.repo
        );
        let comment = self
            .api
            .issues(&self.owner, &self.repo)
            .create_comment(number, body)
            .await
            .wrap_err_with(|| format!("could not comment on #{number}"))?;
        Ok(comment.id.0)
    }

    async fn update_comment(&self, id: u64, body: &str) -> Result<()> {
        // octocrab's `issues().update_comment()` sends POST; GitHub documents
        // this endpoint as PATCH, so the call is made directly.
        //
        // The response body is deliberately not deserialised: nothing uses it,
        // and GitHub has been seen answering with an empty body, which
        // octocrab reports as `EOF while parsing a value` even though the
        // edit went through.
        let route =
            format!("/repos/{}/{}/issues/comments/{id}", self.owner, self.repo);
        debug!("API PATCH {route}");
        let response = self
            .api
            ._patch(route, Some(&serde_json::json!({ "body": body })))
            .await
            .wrap_err_with(|| format!("could not update comment {id}"))?;
        let status = response.status().as_u16();
        let response_body =
            self.api.body_to_string(response).await.unwrap_or_default();
        debug!("  -> HTTP {status} ({} byte body)", response_body.len());

        let current = if (200..300).contains(&status) {
            None
        } else {
            // An error status does not prove the edit was lost, so check
            // before failing the whole run over it.
            self.api
                .issues(&self.owner, &self.repo)
                .get_comment(octocrab::models::CommentId(id))
                .await
                .ok()
                .and_then(|c| c.body)
        };
        comment_edit_result(status, &response_body, current.as_deref(), body)
            .map_err(|reason| eyre!("could not update comment {id}: {reason}"))
    }

    async fn delete_comment(&self, id: u64) -> Result<()> {
        debug!(
            "API DELETE /repos/{}/{}/issues/comments/{id}",
            self.owner, self.repo
        );
        self.api
            .issues(&self.owner, &self.repo)
            .delete_comment(octocrab::models::CommentId(id))
            .await
            .wrap_err_with(|| format!("could not delete comment {id}"))?;
        Ok(())
    }

    /// REST cannot flip `draft`, so this goes through GraphQL.
    ///
    /// Failing to toggle the draft flag must not abort a sync: it is a
    /// precaution around retargeting, not a step the result depends on.
    async fn set_draft(&self, node_id: &str, draft: bool) -> Result<()> {
        if node_id.is_empty() {
            debug!("no node id available; leaving draft state alone");
            return Ok(());
        }
        debug!("API POST /graphql set_draft(node_id={node_id}, draft={draft})");
        let mutation = if draft {
            "mutation($id: ID!) { convertPullRequestToDraft(input: \
             {pullRequestId: $id}) { pullRequest { number } } }"
        } else {
            "mutation($id: ID!) { markPullRequestReadyForReview(input: \
             {pullRequestId: $id}) { pullRequest { number } } }"
        };
        let body = serde_json::json!({
            "query": mutation,
            "variables": { "id": node_id },
        });
        if let Err(e) = self
            .api
            .post::<_, serde_json::Value>("/graphql", Some(&body))
            .await
        {
            debug!("could not set draft={draft} on {node_id}: {e}");
        }
        Ok(())
    }

    async fn list_pull_requests(
        &self,
        author: Option<&str>,
    ) -> Result<Vec<ListedPr>> {
        let query_filter = match author {
            Some(login) => format!(
                "repo:{}/{} is:pr is:open author:{login} archived:false",
                self.owner, self.repo
            ),
            None => format!(
                "repo:{}/{} is:pr is:open archived:false",
                self.owner, self.repo
            ),
        };
        debug!("API POST /graphql SearchPullRequests(query={query_filter:?})");
        let body = serde_json::json!({
            "query": SEARCH_PULL_REQUESTS_QUERY,
            "variables": {
                "query": query_filter,
            },
        });
        let response: GqlResponse<SearchQueryData> = self
            .api
            .post("/graphql", Some(&body))
            .await
            .wrap_err("could not search open pull requests on GitHub")?;

        let errors: Vec<String> =
            response.errors.into_iter().map(|e| e.message).collect();
        let Some(data) = response.data else {
            bail!(
                "GitHub's reply had no `data` member{}. Search query: {query_filter}",
                error_suffix(&errors)
            );
        };

        let prs: Vec<ListedPr> = data
            .search
            .nodes
            .into_iter()
            .flatten()
            .map(listed_pr_from)
            .collect();
        debug!("  -> found {} open PR(s)", prs.len());
        Ok(prs)
    }

    async fn find_pull_request_by_head(
        &self,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let query_filter =
            format!("repo:{}/{} is:pr head:\"{head}\"", self.owner, self.repo);
        debug!(
            "API POST /graphql FindPullRequestByHead(query={query_filter:?})"
        );
        let body = serde_json::json!({
            "query": SEARCH_PULL_REQUESTS_QUERY,
            "variables": {
                "query": query_filter,
            },
        });
        let response: GqlResponse<SearchQueryData> =
            self.api.post("/graphql", Some(&body)).await.wrap_err_with(
                || format!("could not find pull request for head {head}"),
            )?;

        let Some(data) = response.data else {
            return Ok(None);
        };

        let nodes: Vec<SearchPrNode> =
            data.search.nodes.into_iter().flatten().collect();
        let target = nodes
            .iter()
            .find(|n| n.state == "OPEN")
            .or_else(|| nodes.first());

        match target {
            Some(node) => {
                debug!("  -> matched head `{head}` to PR #{}", node.number);
                let pr = self.get_pull_request(node.number).await?;
                Ok(Some(pr))
            }
            None => {
                debug!("  -> no PR found for head `{head}`");
                Ok(None)
            }
        }
    }

    async fn sync_stacks(&self, chains: &[Vec<u64>]) -> Result<()> {
        let valid_chains: Vec<&Vec<u64>> =
            chains.iter().filter(|c| c.len() >= 2).collect();
        if valid_chains.is_empty() {
            return Ok(());
        }

        let Some(mut remote_stacks) = self.list_remote_stacks().await? else {
            return Ok(());
        };

        for desired in valid_chains {
            let overlapping: Vec<RemoteStack> = remote_stacks
                .iter()
                .filter(|s| {
                    s.pull_requests.iter().any(|p| desired.contains(&p.number))
                })
                .cloned()
                .collect();

            if overlapping.len() == 1 {
                let matched = &overlapping[0];
                let current: Vec<u64> =
                    matched.pull_requests.iter().map(|p| p.number).collect();
                if &current == desired || current.ends_with(desired) {
                    continue;
                }
                if desired.starts_with(&current) {
                    let delta = &desired[current.len()..];
                    let route = format!(
                        "/repos/{}/{}/stacks/{}/add",
                        self.owner, self.repo, matched.number
                    );
                    debug!("API POST {route} (pull_requests={delta:?})");
                    let body = serde_json::json!({ "pull_requests": delta });
                    match self
                        .api
                        .post::<_, RemoteStack>(route, Some(&body))
                        .await
                    {
                        Ok(updated) => {
                            eprintln!(
                                "{}",
                                console::style(format!(
                                    "github stack: updated stack #{} ({} PRs)",
                                    updated.number,
                                    desired.len()
                                ))
                                .dim()
                            );
                            if let Some(slot) = remote_stacks
                                .iter_mut()
                                .find(|s| s.number == updated.number)
                            {
                                *slot = updated;
                            }
                            continue;
                        }
                        Err(e) => {
                            debug!(
                                "could not extend github stack #{} with {delta:?}: {e}; falling back to recreating stack",
                                matched.number
                            );
                        }
                    }
                }
            }

            for s in &overlapping {
                self.unstack_remote_stack(s.number).await;
                remote_stacks.retain(|rs| rs.number != s.number);
            }

            let route = format!("/repos/{}/{}/stacks", self.owner, self.repo);
            debug!("API POST {route} (pull_requests={desired:?})");
            let body = serde_json::json!({ "pull_requests": desired });
            match self.api.post::<_, RemoteStack>(route, Some(&body)).await {
                Ok(created) => {
                    eprintln!(
                        "{}",
                        console::style(format!(
                            "github stack: created stack #{} ({} PRs)",
                            created.number,
                            desired.len()
                        ))
                        .dim()
                    );
                    remote_stacks.push(created);
                }
                Err(e) => {
                    debug!(
                        "could not create github stack for {desired:?}: {e}"
                    );
                }
            }
        }

        Ok(())
    }

    async fn repo_merge_settings(&self) -> Result<RepoMergeSettings> {
        if let Some(cached) = self.merge_settings.get() {
            return Ok(*cached);
        }

        #[derive(Deserialize)]
        struct RepoSettingsResponse {
            #[serde(default = "default_true")]
            allow_squash_merge: bool,
            #[serde(default = "default_true")]
            allow_merge_commit: bool,
            #[serde(default = "default_true")]
            allow_rebase_merge: bool,
            #[serde(default)]
            squash_merge_commit_title: String,
            #[serde(default)]
            squash_merge_commit_message: String,
        }
        fn default_true() -> bool {
            true
        }

        let route = format!("/repos/{}/{}", self.owner, self.repo);
        debug!("API GET {route} (repo merge settings)");
        let settings = match self
            .api
            .get::<RepoSettingsResponse, _, _>(route, None::<&()>)
            .await
        {
            Ok(resp) => {
                let s = RepoMergeSettings {
                    allow_squash_merge: resp.allow_squash_merge,
                    allow_merge_commit: resp.allow_merge_commit,
                    allow_rebase_merge: resp.allow_rebase_merge,
                    squash_uses_pr_description: resp.squash_merge_commit_title
                        == "PR_TITLE"
                        && resp.squash_merge_commit_message
                            != "COMMIT_MESSAGES",
                };
                debug!("  -> repo merge settings: {s:?}");
                s
            }
            Err(e) => {
                let defaults = RepoMergeSettings::default();
                debug!(
                    "could not query repo merge settings ({e}); falling back to defaults {defaults:?}"
                );
                defaults
            }
        };

        let _ = self.merge_settings.set(settings);
        Ok(settings)
    }
}

impl GitHubForge {
    async fn resolve_merged_pr_oid(&self, pr: &PullRequest) -> Result<Oid> {
        if let Some(oid) = pr.merge_commit {
            debug!("  -> merged #{} as squash commit {oid}", pr.number);
            return Ok(oid);
        }
        for delay_ms in [200_u64, 500, 1000] {
            debug!(
                "  -> #{} is Merged on GitHub but mergeCommit is not populated yet; waiting {delay_ms}ms",
                pr.number
            );
            tokio::time::sleep(std::time::Duration::from_millis(delay_ms))
                .await;
            if let Ok(refreshed) = self.get_pull_request(pr.number).await
                && let Some(oid) = refreshed.merge_commit
            {
                debug!("  -> merged #{} as squash commit {oid}", pr.number);
                return Ok(oid);
            }
        }
        if let Some(oid) = self.branch_oid(&pr.base).await? {
            debug!(
                "  -> merged #{}; mergeCommit still missing, falling back to `{}` tip {oid}",
                pr.number, pr.base
            );
            return Ok(oid);
        }
        bail!(
            "#{} was merged on GitHub, but the resulting commit could not be \
             determined. Run `nspr sync`.",
            pr.number
        )
    }

    /// Check whether `refs/heads/<pr.base>` already contains the squash commit
    /// for `pr.number` even though GitHub still reports `pr.state == Open`.
    async fn detect_partial_merge_on_base(
        &self,
        pr: &PullRequest,
    ) -> Option<Oid> {
        let base_tip = self.branch_oid(&pr.base).await.ok().flatten()?;
        self.remote.fetch_objects(&[base_tip]).ok()?;
        crate::land::find_landed_commit_on_trunk(
            self.remote.repo(),
            base_tip,
            Oid::ZERO_SHA1,
            pr.number,
        )
        .ok()
        .flatten()
    }

    async fn list_remote_stacks(&self) -> Result<Option<Vec<RemoteStack>>> {
        let route =
            format!("/repos/{}/{}/stacks?per_page=100", self.owner, self.repo);
        debug!("API GET {route}");
        match self
            .api
            .get::<Vec<RemoteStack>, _, _>(route, None::<&()>)
            .await
        {
            Ok(stacks) => Ok(Some(stacks)),
            Err(e) if matches!(status_code(&e), Some(403 | 404)) => {
                debug!("github stacks API unavailable: {e}");
                Ok(None)
            }
            Err(e) => {
                debug!("could not list github stacks: {e}");
                Ok(None)
            }
        }
    }

    async fn unstack_remote_stack(&self, stack_number: u64) {
        let route = format!(
            "/repos/{}/{}/stacks/{stack_number}/unstack",
            self.owner, self.repo
        );
        debug!("API POST {route}");
        if let Err(e) = self.api._post(route, None::<&()>).await {
            debug!("could not unstack #{stack_number}: {e}");
        }
    }

    async fn unstack_pr_if_stacked(&self, pr_number: u64) {
        let route = format!(
            "/repos/{}/{}/stacks?pull_request={pr_number}",
            self.owner, self.repo
        );
        debug!("API GET {route}");
        if let Ok(stacks) = self
            .api
            .get::<Vec<RemoteStack>, _, _>(route, None::<&()>)
            .await
        {
            for s in stacks {
                self.unstack_remote_stack(s.number).await;
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteStack {
    number: u64,
    #[serde(default)]
    pull_requests: Vec<RemoteStackPr>,
}

#[derive(Debug, Clone, Deserialize)]
struct RemoteStackPr {
    number: u64,
}

/// `force` is honoured by *omitting* the leading `+`: the remote then rejects
/// anything that is not a fast-forward, which is the invariant nspr relies on
/// everywhere except land-time cleanup. Refusing client-side instead would
/// only catch the cases we already know about.
fn refspec(spec: &PushSpec) -> String {
    let branch = &spec.branch;
    match spec.oid {
        None => format!(":refs/heads/{branch}"),
        Some(oid) if spec.force => format!("+{oid}:refs/heads/{branch}"),
        Some(oid) => format!("{oid}:refs/heads/{branch}"),
    }
}

#[cfg(test)]
fn unused_name(preferred: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(preferred) {
        return preferred.to_string();
    }
    for suffix in 1.. {
        let candidate = format!("{preferred}-{suffix}");
        if !taken(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

/// Decide whether a `PATCH /issues/comments/{id}` succeeded.
///
/// Any 2xx counts, whatever the body (it may be empty). For anything else,
/// `current` is the comment's body as re-read afterwards: if it already holds
/// `wanted`, GitHub applied the edit despite the error status. Otherwise the
/// reason names the status and GitHub's message, or says the body was empty.
fn comment_edit_result(
    status: u16,
    response_body: &str,
    current: Option<&str>,
    wanted: &str,
) -> std::result::Result<(), String> {
    if (200..300).contains(&status) {
        return Ok(());
    }
    if current == Some(wanted) {
        debug!(
            "comment edit answered HTTP {status}, but the comment already has the new text"
        );
        return Ok(());
    }
    #[derive(Deserialize)]
    struct ErrorBody {
        message: String,
    }
    let detail = if response_body.trim().is_empty() {
        "empty response".to_string()
    } else {
        serde_json::from_str::<ErrorBody>(response_body)
            .map(|e| e.message)
            .unwrap_or_else(|_| response_body.trim().to_string())
    };
    Err(format!("GitHub returned HTTP {status} ({detail})"))
}

fn status_code(error: &octocrab::Error) -> Option<u16> {
    match error {
        octocrab::Error::GitHub { source, .. } => {
            Some(source.status_code.as_u16())
        }
        _ => None,
    }
}

fn github_error_message(error: &octocrab::Error) -> Option<&str> {
    match error {
        octocrab::Error::GitHub { source, .. } => Some(source.message.as_str()),
        _ => None,
    }
}

#[derive(Debug, Deserialize)]
struct ProtectionResponse {
    #[serde(default)]
    required_status_checks: Option<RequiredStatusChecks>,
    #[serde(default)]
    required_pull_request_reviews: Option<RequiredReviews>,
}

#[derive(Debug, Deserialize)]
struct RequiredStatusChecks {
    /// GitHub's name for "branches must be up to date before merging".
    #[serde(default)]
    strict: bool,
}

#[derive(Debug, Deserialize)]
struct RequiredReviews {
    #[serde(default)]
    dismiss_stale_reviews: bool,
}

#[derive(Debug, Deserialize)]
struct GqlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GqlError>,
}

#[derive(Debug, Deserialize)]
struct GqlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct QueryData {
    repository: Option<RepositoryNode>,
}

#[derive(Debug, Deserialize)]
struct BatchQueryData {
    repository:
        Option<std::collections::HashMap<String, Option<PullRequestNode>>>,
}

#[derive(Debug, Deserialize)]
struct RefQueryData {
    repository: Option<RefRepositoryNode>,
}

#[derive(Debug, Deserialize)]
struct RefRepositoryNode {
    #[serde(rename = "ref")]
    git_ref: Option<RefNode>,
}

#[derive(Debug, Deserialize)]
struct RefNode {
    target: Option<OidNode>,
}

#[derive(Debug, Deserialize)]
struct OidNode {
    oid: String,
}

#[derive(Debug, Deserialize)]
struct ViewerAndRefQueryData {
    viewer: Option<ViewerNode>,
    repository: Option<RefRepositoryNode>,
}

#[derive(Debug, Deserialize)]
struct ViewerNode {
    login: String,
}

const BRANCH_OID_QUERY: &str = r#"
query($owner: String!, $repo: String!, $qualifiedName: String!) {
  repository(owner: $owner, name: $repo) {
    ref(qualifiedName: $qualifiedName) {
      target {
        oid
      }
    }
  }
}
"#;

const VIEWER_AND_BRANCH_OID_QUERY: &str = r#"
query($owner: String!, $repo: String!, $qualifiedName: String!) {
  viewer {
    login
  }
  repository(owner: $owner, name: $repo) {
    ref(qualifiedName: $qualifiedName) {
      target {
        oid
      }
    }
  }
}
"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepositoryNode {
    pull_request: Option<PullRequestNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestNode {
    /// Absent from the recorded payloads checked in for the tests below, and
    /// only needed for the draft mutations, so a miss degrades rather than
    /// failing the whole read.
    #[serde(default)]
    id: String,
    number: u64,
    state: String,
    title: String,
    body: String,
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    /// Current tip of the base branch, not the merge base.
    base_ref_oid: String,
    head_ref_oid: String,
    /// Optional because GitHub answers a field it cannot compute with `null`
    /// plus a top-level error, and because `mergeStateStatus` has needed a
    /// preview media type on some deployments. Either way, degrading to
    /// `Unknown` beats failing the command.
    #[serde(default)]
    mergeable: Option<String>,
    #[serde(default)]
    merge_state_status: Option<String>,
    /// Only its presence matters: a non-null `autoMergeRequest` means
    /// auto-merge is armed.
    #[serde(default)]
    auto_merge_request: Option<serde_json::Value>,
    #[serde(default)]
    merge_commit: Option<OidNode>,
    #[serde(default)]
    latest_opinionated_reviews: Option<ReviewConnectionNode>,
    #[serde(default)]
    commits: Option<CommitConnectionNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewConnectionNode {
    #[serde(default)]
    nodes: Vec<Option<ReviewNode>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewNode {
    #[serde(default)]
    state: String,
    #[serde(default)]
    author: Option<ReviewAuthorNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewAuthorNode {
    #[serde(default)]
    login: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitConnectionNode {
    #[serde(default)]
    nodes: Vec<Option<PullRequestCommitNode>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullRequestCommitNode {
    #[serde(default)]
    commit: Option<CommitNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitNode {
    #[serde(default)]
    status_check_rollup: Option<StatusCheckRollupNode>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusCheckRollupNode {
    #[serde(default)]
    contexts: Option<StatusCheckRollupContextConnection>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatusCheckRollupContextConnection {
    #[serde(default)]
    check_run_counts_by_state: Vec<StateCountNode>,
    #[serde(default)]
    status_context_counts_by_state: Vec<StateCountNode>,
    #[serde(default)]
    nodes: Vec<Option<CheckContextNode>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StateCountNode {
    state: String,
    count: usize,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckContextNode {
    /// Present on `CheckRun`.
    #[serde(default)]
    name: Option<String>,
    /// Present on `CheckRun`.
    #[serde(default)]
    conclusion: Option<String>,
    /// Present on `StatusContext`.
    #[serde(default)]
    context: Option<String>,
    /// Present on `StatusContext`.
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchQueryData {
    search: SearchResultNode,
}

#[derive(Debug, Deserialize)]
struct SearchResultNode {
    nodes: Vec<Option<SearchPrNode>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchPrNode {
    number: u64,
    title: String,
    state: String,
    #[serde(default)]
    is_draft: bool,
    base_ref_name: String,
    head_ref_name: String,
    #[serde(default)]
    review_decision: Option<String>,
    url: String,
}

fn review_decision_from(s: Option<&str>) -> Option<ReviewDecision> {
    match s {
        Some("APPROVED") => Some(ReviewDecision::Approved),
        Some("CHANGES_REQUESTED") => Some(ReviewDecision::ChangesRequested),
        Some("REVIEW_REQUIRED") => Some(ReviewDecision::ReviewRequired),
        _ => None,
    }
}

fn listed_pr_from(node: SearchPrNode) -> ListedPr {
    ListedPr {
        number: node.number,
        title: node.title,
        state: pr_state_from(&node.state),
        draft: node.is_draft,
        base: node.base_ref_name,
        head: node.head_ref_name,
        review_decision: review_decision_from(node.review_decision.as_deref()),
        url: node.url,
    }
}

/// Pull the node out of a response, tolerating errors that left it intact.
///
/// A GraphQL response can carry both `data` and `errors`: one unreadable field
/// should not cost the user their command, so errors are only fatal when the
/// pull request itself did not come back.
///
/// The envelope is peeled one level at a time because the levels fail for
/// different reasons and only one of them is the user's fault. Collapsing them
/// into a single "no such pull request" message cost an afternoon: an octocrab
/// upgrade began handing us the response's `data` member rather than the whole
/// response, and the tool responded by blaming a `Pull-Request:` trailer that
/// had been correct all along.
fn pull_request_node(
    response: GqlResponse<QueryData>,
    number: u64,
) -> Result<PullRequestNode> {
    let errors: Vec<String> =
        response.errors.into_iter().map(|e| e.message).collect();

    let Some(data) = response.data else {
        bail!(
            "GitHub's reply had no `data` member{}. Unless GitHub is having \
             an outage this is an nspr bug: the GraphQL envelope was not the \
             shape we expected.",
            error_suffix(&errors)
        );
    };
    let Some(repository) = data.repository else {
        bail!(
            "GitHub did not return the repository{}. Check that the remote \
             points where you think it does and that your token can read it.",
            error_suffix(&errors)
        );
    };
    let Some(node) = repository.pull_request else {
        bail!(
            "GitHub has no pull request #{number} in this repository{}. \
             Check the `Pull-Request:` trailer on the commit.",
            error_suffix(&errors)
        );
    };

    if !errors.is_empty() {
        debug!("partial GraphQL errors: {}", errors.join("; "));
    }
    Ok(node)
}

const PULL_REQUEST_FIELDS: &str = r#"{
      id
      number
      state
      title
      body
      isDraft
      baseRefName
      headRefName
      baseRefOid
      headRefOid
      mergeable
      mergeStateStatus
      autoMergeRequest {
        enabledAt
      }
      mergeCommit {
        oid
      }
      latestOpinionatedReviews(last: 50) {
        nodes {
          state
          author {
            login
          }
        }
      }
      commits(last: 1) {
        nodes {
          commit {
            statusCheckRollup {
              contexts(first: 100) {
                checkRunCountsByState {
                  state
                  count
                }
                statusContextCountsByState {
                  state
                  count
                }
                nodes {
                  ... on CheckRun {
                    name
                    conclusion
                  }
                  ... on StatusContext {
                    context
                    state
                  }
                }
              }
            }
          }
        }
      }
    }"#;

fn build_batch_pull_requests_query(numbers: &[u64]) -> String {
    let mut query = String::from(
        "query PullRequests($owner: String!, $name: String!) {\n  repository(owner: $owner, name: $name) {\n",
    );
    for (idx, number) in numbers.iter().enumerate() {
        query.push_str(&format!(
            "    pr_{idx}: pullRequest(number: {number}) {PULL_REQUEST_FIELDS}\n"
        ));
    }
    query.push_str("  }\n}\n");
    query
}

fn batch_pull_request_nodes(
    response: GqlResponse<BatchQueryData>,
    numbers: &[u64],
) -> Result<Vec<PullRequestNode>> {
    let errors: Vec<String> =
        response.errors.into_iter().map(|e| e.message).collect();

    let Some(data) = response.data else {
        bail!(
            "GitHub's reply had no `data` member{}. Unless GitHub is having \
             an outage this is an nspr bug: the GraphQL envelope was not the \
             shape we expected.",
            error_suffix(&errors)
        );
    };
    let Some(mut repository) = data.repository else {
        bail!(
            "GitHub did not return the repository{}. Check that the remote \
             points where you think it does and that your token can read it.",
            error_suffix(&errors)
        );
    };

    let mut out = Vec::with_capacity(numbers.len());
    for (idx, &number) in numbers.iter().enumerate() {
        let key = format!("pr_{idx}");
        let Some(node) = repository.remove(&key).flatten() else {
            bail!(
                "GitHub has no pull request #{number} in this repository{}. \
                 Check the `Pull-Request:` trailer on the commit.",
                error_suffix(&errors)
            );
        };
        out.push(node);
    }

    if !errors.is_empty() {
        debug!("partial GraphQL errors: {}", errors.join("; "));
    }
    Ok(out)
}

/// `": a; b"`, or nothing at all when there is nothing to report.
fn error_suffix(errors: &[String]) -> String {
    if errors.is_empty() {
        String::new()
    } else {
        format!(": {}", errors.join("; "))
    }
}

fn pull_request_from(node: PullRequestNode) -> Result<PullRequest> {
    let merge_commit = match node.merge_commit {
        Some(c) => Some(Oid::from_str(&c.oid)?),
        None => None,
    };
    let checks = checks_from(node.commits);
    let reviews = reviews_from(node.latest_opinionated_reviews);
    Ok(PullRequest {
        number: node.number,
        node_id: node.id,
        state: pr_state_from(&node.state),
        title: node.title,
        body: node.body,
        base: node.base_ref_name,
        head: node.head_ref_name,
        base_oid: Oid::from_str(&node.base_ref_oid)?,
        head_oid: Oid::from_str(&node.head_ref_oid)?,
        merge_commit,
        mergeable: mergeable_from(node.mergeable.as_deref()),
        merge_state: merge_state_from(node.merge_state_status.as_deref()),
        auto_merge: node.auto_merge_request.is_some(),
        draft: node.is_draft,
        checks,
        reviews,
    })
}

fn reviews_from(reviews: Option<ReviewConnectionNode>) -> ReviewSummary {
    let mut summary = ReviewSummary::default();
    let Some(reviews) = reviews else {
        return summary;
    };
    for node in reviews.nodes.into_iter().flatten() {
        let login = node
            .author
            .map(|a| a.login)
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| "ghost".to_string());
        match node.state.as_str() {
            "APPROVED" => {
                if !summary.approved_by.contains(&login) {
                    summary.approved_by.push(login);
                }
            }
            "CHANGES_REQUESTED" => {
                if !summary.changes_requested_by.contains(&login) {
                    summary.changes_requested_by.push(login);
                }
            }
            _ => {}
        }
    }
    summary
}

fn checks_from(commits: Option<CommitConnectionNode>) -> Option<CheckCounts> {
    let contexts = commits?
        .nodes
        .into_iter()
        .flatten()
        .last()?
        .commit?
        .status_check_rollup?
        .contexts?;

    let mut counts = CheckCounts::default();
    for entry in contexts.check_run_counts_by_state {
        match entry.state.as_str() {
            "SUCCESS" | "NEUTRAL" | "SKIPPED" | "COMPLETED" => {
                counts.passed += entry.count;
            }
            "FAILURE" | "TIMED_OUT" | "ACTION_REQUIRED" | "STARTUP_FAILURE"
            | "STALE" => {
                counts.failed += entry.count;
            }
            "IN_PROGRESS" | "QUEUED" | "PENDING" | "WAITING" | "REQUESTED" => {
                counts.pending += entry.count;
            }
            // Excluded so cancelled runs from `cancel-in-progress` workflows
            // neither inflate the denominator nor show up as failures.
            "CANCELLED" => {}
            _ => {}
        }
    }
    for entry in contexts.status_context_counts_by_state {
        match entry.state.as_str() {
            "SUCCESS" => counts.passed += entry.count,
            "FAILURE" | "ERROR" => counts.failed += entry.count,
            "PENDING" | "EXPECTED" => counts.pending += entry.count,
            _ => {}
        }
    }
    for node in contexts.nodes.into_iter().flatten() {
        if let (Some(name), Some(conclusion)) = (node.name, node.conclusion) {
            if matches!(
                conclusion.as_str(),
                "FAILURE"
                    | "TIMED_OUT"
                    | "ACTION_REQUIRED"
                    | "STARTUP_FAILURE"
                    | "STALE"
            ) && !counts.failed_names.contains(&name)
            {
                counts.failed_names.push(name);
            }
        } else if let (Some(context), Some(state)) = (node.context, node.state)
            && matches!(state.as_str(), "FAILURE" | "ERROR")
            && !counts.failed_names.contains(&context)
        {
            counts.failed_names.push(context);
        }
    }

    (counts.total() > 0).then_some(counts)
}

fn pr_state_from(state: &str) -> PrState {
    match state {
        "OPEN" => PrState::Open,
        "MERGED" => PrState::Merged,
        _ => PrState::Closed,
    }
}

fn mergeable_from(mergeable: Option<&str>) -> Mergeable {
    match mergeable {
        Some("MERGEABLE") => Mergeable::Mergeable,
        Some("CONFLICTING") => Mergeable::Conflicting,
        // `UNKNOWN` means GitHub has not finished computing the merge commit
        // yet; it is a "ask again shortly", not a verdict.
        _ => Mergeable::Unknown,
    }
}

fn merge_state_from(status: Option<&str>) -> MergeState {
    match status {
        // `UNSTABLE` is a failing *non-required* check and `HAS_HOOKS` a
        // pre-receive hook: both still merge, so neither is `Blocked`.
        Some("CLEAN" | "UNSTABLE" | "HAS_HOOKS") => MergeState::Clean,
        // GitHub only ever reports this when the repository requires branches
        // to be up to date. Everywhere else a stacked pull request whose base
        // has advanced still reads `CLEAN`.
        Some("BEHIND") => MergeState::Behind,
        Some("BLOCKED" | "DRAFT") => MergeState::Blocked,
        Some("DIRTY") => MergeState::Dirty,
        _ => MergeState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const SAMPLE: &str = include_str!("../gql/pullrequest_query_sample.json");

    fn parse(payload: &str) -> GqlResponse<QueryData> {
        serde_json::from_str(payload).unwrap()
    }

    #[test]
    fn deserialises_a_recorded_payload() {
        let node = pull_request_node(parse(SAMPLE), 4242).unwrap();
        let pr = pull_request_from(node).unwrap();

        assert_eq!(pr.number, 4242);
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.title, "Add the widget cache");
        assert_eq!(pr.base, "users/someone/add-the-widget-index");
        assert_eq!(pr.head, "users/someone/add-the-widget-cache");
        assert_eq!(
            pr.base_oid,
            Oid::from_str("0123456789abcdef0123456789abcdef01234567").unwrap()
        );
        assert_eq!(
            pr.head_oid,
            Oid::from_str("89abcdef0123456789abcdef0123456789abcdef").unwrap()
        );
        assert_eq!(pr.mergeable, Mergeable::Mergeable);
        assert_eq!(pr.merge_state, MergeState::Behind);
        assert!(pr.auto_merge);
        assert!(!pr.draft);
    }

    #[test]
    fn absent_auto_merge_request_means_auto_merge_is_off() {
        let payload = SAMPLE.replace(
            "\"autoMergeRequest\": {\n          \"enabledAt\": \
             \"2026-01-02T03:04:05Z\"\n        }",
            "\"autoMergeRequest\": null",
        );
        let node = pull_request_node(parse(&payload), 4242).unwrap();
        assert!(!pull_request_from(node).unwrap().auto_merge);
    }

    #[test]
    fn a_field_level_error_does_not_lose_the_pull_request() {
        let payload = SAMPLE.replace(
            "\"mergeStateStatus\": \"BEHIND\"",
            "\"mergeStateStatus\": null",
        );
        let payload = payload.trim_end().trim_end_matches('}').to_string()
            + ",\"errors\":[{\"message\":\"mergeStateStatus unavailable\"}]}";

        let node = pull_request_node(parse(&payload), 4242).unwrap();
        let pr = pull_request_from(node).unwrap();
        assert_eq!(pr.merge_state, MergeState::Unknown);
        assert_eq!(pr.number, 4242);
    }

    #[test]
    fn a_missing_pull_request_reports_the_graphql_errors() {
        let payload = r#"{
            "data": { "repository": null },
            "errors": [{ "message": "Could not resolve to a Repository" }]
        }"#;
        let error = pull_request_node(parse(payload), 7)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("Could not resolve to a Repository"),
            "{error}"
        );

        let payload =
            r#"{ "data": { "repository": { "pullRequest": null } } }"#;
        let error = pull_request_node(parse(payload), 7)
            .unwrap_err()
            .to_string();
        assert!(error.contains("#7"), "{error}");
    }

    /// The shape `Octocrab::graphql` hands back: the `data` member on its own,
    /// with the envelope already stripped. Blaming the user's trailer for this
    /// is what made the real bug take an afternoon to find.
    #[test]
    fn a_stripped_envelope_is_not_reported_as_a_missing_pull_request() {
        let full: serde_json::Value = serde_json::from_str(SAMPLE).unwrap();
        let stripped = full["data"].to_string();

        let error = pull_request_node(parse(&stripped), 4242)
            .unwrap_err()
            .to_string();
        assert!(
            !error.contains("Pull-Request:"),
            "a stripped envelope must not blame the trailer: {error}"
        );
        assert!(error.contains("nspr bug"), "{error}");
    }

    #[test]
    fn maps_merge_state_status() {
        assert_eq!(merge_state_from(Some("CLEAN")), MergeState::Clean);
        assert_eq!(merge_state_from(Some("UNSTABLE")), MergeState::Clean);
        assert_eq!(merge_state_from(Some("HAS_HOOKS")), MergeState::Clean);
        assert_eq!(merge_state_from(Some("BEHIND")), MergeState::Behind);
        assert_eq!(merge_state_from(Some("BLOCKED")), MergeState::Blocked);
        assert_eq!(merge_state_from(Some("DRAFT")), MergeState::Blocked);
        assert_eq!(merge_state_from(Some("DIRTY")), MergeState::Dirty);
        assert_eq!(merge_state_from(Some("UNKNOWN")), MergeState::Unknown);
        assert_eq!(
            merge_state_from(Some("SOMETHING_NEW")),
            MergeState::Unknown
        );
        assert_eq!(merge_state_from(None), MergeState::Unknown);
    }

    #[test]
    fn maps_mergeability() {
        assert_eq!(mergeable_from(Some("MERGEABLE")), Mergeable::Mergeable);
        assert_eq!(mergeable_from(Some("CONFLICTING")), Mergeable::Conflicting);
        assert_eq!(mergeable_from(Some("UNKNOWN")), Mergeable::Unknown);
        assert_eq!(mergeable_from(None), Mergeable::Unknown);
    }

    #[test]
    fn maps_pull_request_state() {
        assert_eq!(pr_state_from("OPEN"), PrState::Open);
        assert_eq!(pr_state_from("MERGED"), PrState::Merged);
        assert_eq!(pr_state_from("CLOSED"), PrState::Closed);
    }

    #[test]
    fn only_forced_pushes_get_a_plus() {
        let oid =
            Oid::from_str("89abcdef0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(
            refspec(&PushSpec::fast_forward("topic", oid)),
            format!("{oid}:refs/heads/topic")
        );
        assert_eq!(
            refspec(&PushSpec::forced("topic", oid)),
            format!("+{oid}:refs/heads/topic")
        );
        assert_eq!(refspec(&PushSpec::delete("topic")), ":refs/heads/topic");
    }

    #[test]
    fn suffixes_until_the_name_is_free() {
        let taken: HashSet<&str> =
            ["users/me/cache", "users/me/cache-1", "users/me/cache-2"].into();
        let taken = |name: &str| taken.contains(name);

        assert_eq!(unused_name("users/me/other", taken), "users/me/other");
        assert_eq!(unused_name("users/me/cache", taken), "users/me/cache-3");
    }

    #[test]
    fn maps_review_decision() {
        assert_eq!(
            review_decision_from(Some("APPROVED")),
            Some(ReviewDecision::Approved)
        );
        assert_eq!(
            review_decision_from(Some("CHANGES_REQUESTED")),
            Some(ReviewDecision::ChangesRequested)
        );
        assert_eq!(
            review_decision_from(Some("REVIEW_REQUIRED")),
            Some(ReviewDecision::ReviewRequired)
        );
        assert_eq!(review_decision_from(Some("OTHER")), None);
        assert_eq!(review_decision_from(None), None);
    }

    #[test]
    fn parses_search_pr_node() {
        let json = r#"{
            "number": 42,
            "title": "Widget trait",
            "state": "OPEN",
            "isDraft": false,
            "baseRefName": "main",
            "headRefName": "users/alice/widget",
            "reviewDecision": "APPROVED",
            "url": "https://github.com/o/r/pull/42"
        }"#;
        let node: SearchPrNode = serde_json::from_str(json).unwrap();
        let pr = listed_pr_from(node);
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Widget trait");
        assert_eq!(pr.state, PrState::Open);
        assert!(!pr.draft);
        assert_eq!(pr.base, "main");
        assert_eq!(pr.head, "users/alice/widget");
        assert_eq!(pr.review_decision, Some(ReviewDecision::Approved));
        assert_eq!(pr.url, "https://github.com/o/r/pull/42");
    }

    #[test]
    fn parses_status_check_rollup_counts_by_state() {
        let json = r#"{
            "nodes": [{
                "commit": {
                    "statusCheckRollup": {
                        "contexts": {
                            "checkRunCountsByState": [
                                { "state": "SUCCESS", "count": 7 },
                                { "state": "SKIPPED", "count": 1 },
                                { "state": "NEUTRAL", "count": 1 },
                                { "state": "IN_PROGRESS", "count": 1 },
                                { "state": "FAILURE", "count": 2 },
                                { "state": "CANCELLED", "count": 3 }
                            ],
                            "statusContextCountsByState": [
                                { "state": "SUCCESS", "count": 1 },
                                { "state": "PENDING", "count": 1 },
                                { "state": "ERROR", "count": 1 }
                            ],
                            "nodes": [
                                { "name": "linux-x64", "conclusion": "SUCCESS" },
                                { "name": "clang-debian", "conclusion": "FAILURE" },
                                { "name": "win-x64", "conclusion": "TIMED_OUT" },
                                { "context": "bazel-build", "state": "ERROR" }
                            ]
                        }
                    }
                }
            }]
        }"#;
        let commits: CommitConnectionNode = serde_json::from_str(json).unwrap();
        let counts = checks_from(Some(commits)).unwrap();
        assert_eq!(
            counts,
            CheckCounts {
                passed: 10,
                failed: 3,
                pending: 2,
                failed_names: vec![
                    "clang-debian".to_string(),
                    "win-x64".to_string(),
                    "bazel-build".to_string(),
                ],
            }
        );
        assert_eq!(counts.total(), 15);
    }

    #[test]
    fn parses_latest_opinionated_reviews() {
        let json = r#"{
            "nodes": [
                { "state": "APPROVED", "author": { "login": "alice" } },
                { "state": "CHANGES_REQUESTED", "author": { "login": "bob" } },
                { "state": "APPROVED", "author": { "login": "carol" } }
            ]
        }"#;
        let conn: ReviewConnectionNode = serde_json::from_str(json).unwrap();
        let summary = reviews_from(Some(conn));
        assert_eq!(
            summary,
            ReviewSummary {
                approved_by: vec!["alice".to_string(), "carol".to_string()],
                changes_requested_by: vec!["bob".to_string()],
            }
        );
    }

    #[test]
    fn deserialises_a_batched_payload() {
        let q = build_batch_pull_requests_query(&[101, 202]);
        assert!(q.contains("pr_0: pullRequest(number: 101)"), "{q}");
        assert!(q.contains("pr_1: pullRequest(number: 202)"), "{q}");

        let sample_val: serde_json::Value =
            serde_json::from_str(SAMPLE).unwrap();
        let pr0 = sample_val["data"]["repository"]["pullRequest"].clone();
        let mut pr1 = pr0.clone();
        pr1["number"] = serde_json::json!(4243);
        pr1["title"] = serde_json::json!("Second PR");

        let batch_json = serde_json::json!({
            "data": {
                "repository": {
                    "pr_0": pr0,
                    "pr_1": pr1,
                }
            }
        });
        let resp: GqlResponse<BatchQueryData> =
            serde_json::from_value(batch_json).unwrap();
        let nodes = batch_pull_request_nodes(resp, &[4242, 4243]).unwrap();
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].number, 4242);
        assert_eq!(nodes[1].number, 4243);
        assert_eq!(nodes[1].title, "Second PR");
    }

    /// The bug this guards against: a successful edit whose response has no
    /// body used to abort `nspr diff` with `EOF while parsing a value`.
    #[test]
    fn comment_edit_with_success_status_and_empty_body_succeeds() {
        assert_eq!(comment_edit_result(200, "", None, "new"), Ok(()));
        assert_eq!(comment_edit_result(204, "", None, "new"), Ok(()));
    }

    #[test]
    fn comment_edit_with_error_status_succeeds_if_the_edit_landed() {
        assert_eq!(comment_edit_result(502, "", Some("new"), "new"), Ok(()));
    }

    #[test]
    fn comment_edit_with_error_status_and_empty_body_names_the_status() {
        assert_eq!(
            comment_edit_result(502, "", Some("old"), "new"),
            Err("GitHub returned HTTP 502 (empty response)".to_string())
        );
        assert_eq!(
            comment_edit_result(502, "  \n", None, "new"),
            Err("GitHub returned HTTP 502 (empty response)".to_string())
        );
    }

    #[test]
    fn comment_edit_with_error_status_reports_githubs_message() {
        let body = r#"{"message":"Validation Failed","documentation_url":"x"}"#;
        assert_eq!(
            comment_edit_result(422, body, Some("old"), "new"),
            Err("GitHub returned HTTP 422 (Validation Failed)".to_string())
        );
        assert_eq!(
            comment_edit_result(500, "<html>oops</html>", None, "new"),
            Err("GitHub returned HTTP 500 (<html>oops</html>)".to_string())
        );
    }
}
