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
    Comment, CreatePr, Forge, ListedPr, MergeState, Mergeable, PrState,
    Protection, PullRequest, PullRequestUpdate, PushSpec, RepoMergeSettings,
    ReviewDecision, SquashMerge,
};
use crate::git_remote::GitRemote;

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
        *self.login.borrow_mut() = Some(login.clone());
        Ok(login)
    }
}

#[async_trait(?Send)]
impl Forge for GitHubForge {
    async fn get_pull_request(&self, number: u64) -> Result<PullRequest> {
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
        self.remote.fetch_objects(&[pr.base_oid, pr.head_oid])?;
        Ok(pr)
    }

    async fn create_pull_request(&self, req: CreatePr) -> Result<u64> {
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
        let result = self
            .api
            .pulls(&self.owner, &self.repo)
            .merge(number)
            .title(req.title)
            .message(req.message)
            .sha(req.expected_head.to_string())
            .method(MergeMethod::Squash)
            .send()
            .await;

        let merge = match result {
            Ok(merge) => merge,
            Err(e) => {
                let advice = match status_code(&e) {
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
                };
                return Err(Error::from(e)).wrap_err(format!(
                    "could not squash-merge #{number}: {advice}"
                ));
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
            return Ok(None);
        };
        Ok(Some(Oid::from_str(&oid_str)?))
    }

    async fn push(&self, specs: &[PushSpec]) -> Result<()> {
        let refspecs: Vec<String> = specs.iter().map(refspec).collect();
        let has_metadata =
            specs.iter().any(|s| s.label.is_some() || s.context.is_some());
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
            .wrap_err(
                "the push was rejected. If this was a fast-forward push, somebody \
                 else has pushed to the branch: run `nspr sync` and try again.",
            )
    }

    async fn unused_branch_name(&self, preferred: &str) -> Result<String> {
        if self.branch_oid(preferred).await?.is_none() {
            return Ok(preferred.to_string());
        }
        for suffix in 1.. {
            let candidate = format!("{preferred}-{suffix}");
            if self.branch_oid(&candidate).await?.is_none() {
                return Ok(candidate);
            }
        }
        unreachable!()
    }

    async fn fetch_commit(&self, oid: Oid) -> Result<()> {
        self.remote.fetch_objects(&[oid])
    }

    async fn list_own_comments(&self, number: u64) -> Result<Vec<Comment>> {
        let login = self.viewer_login().await?;
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
        let route =
            format!("/repos/{}/{}/issues/comments/{id}", self.owner, self.repo);
        self.api
            .patch::<octocrab::models::issues::Comment, _, _>(
                route,
                Some(&serde_json::json!({ "body": body })),
            )
            .await
            .wrap_err_with(|| format!("could not update comment {id}"))?;
        Ok(())
    }

    async fn delete_comment(&self, id: u64) -> Result<()> {
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

        Ok(data
            .search
            .nodes
            .into_iter()
            .flatten()
            .map(listed_pr_from)
            .collect())
    }

    async fn find_pull_request_by_head(
        &self,
        head: &str,
    ) -> Result<Option<PullRequest>> {
        let query_filter =
            format!("repo:{}/{} is:pr head:\"{head}\"", self.owner, self.repo);
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
                let pr = self.get_pull_request(node.number).await?;
                Ok(Some(pr))
            }
            None => Ok(None),
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
                                "could not extend github stack #{} with {delta:?}: {e}; will recreate",
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
        let settings = match self
            .api
            .get::<RepoSettingsResponse, _, _>(route, None::<&()>)
            .await
        {
            Ok(resp) => RepoMergeSettings {
                allow_squash_merge: resp.allow_squash_merge,
                allow_merge_commit: resp.allow_merge_commit,
                allow_rebase_merge: resp.allow_rebase_merge,
                squash_uses_pr_description: resp.squash_merge_commit_title
                    == "PR_TITLE"
                    && resp.squash_merge_commit_message != "COMMIT_MESSAGES",
            },
            Err(e) => {
                debug!("could not query repo merge settings: {e}");
                RepoMergeSettings::default()
            }
        };

        let _ = self.merge_settings.set(settings);
        Ok(settings)
    }
}

impl GitHubForge {
    async fn list_remote_stacks(&self) -> Result<Option<Vec<RemoteStack>>> {
        let route =
            format!("/repos/{}/{}/stacks?per_page=100", self.owner, self.repo);
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
        if let Err(e) = self.api._post(route, None::<&()>).await {
            debug!("could not unstack #{stack_number}: {e}");
        }
    }

    async fn unstack_pr_if_stacked(&self, pr_number: u64) {
        let route = format!(
            "/repos/{}/{}/stacks?pull_request={pr_number}",
            self.owner, self.repo
        );
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

fn status_code(error: &octocrab::Error) -> Option<u16> {
    match error {
        octocrab::Error::GitHub { source, .. } => {
            Some(source.status_code.as_u16())
        }
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

/// `": a; b"`, or nothing at all when there is nothing to report.
fn error_suffix(errors: &[String]) -> String {
    if errors.is_empty() {
        String::new()
    } else {
        format!(": {}", errors.join("; "))
    }
}

fn pull_request_from(node: PullRequestNode) -> Result<PullRequest> {
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
        mergeable: mergeable_from(node.mergeable.as_deref()),
        merge_state: merge_state_from(node.merge_state_status.as_deref()),
        auto_merge: node.auto_merge_request.is_some(),
        draft: node.is_draft,
    })
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
}
