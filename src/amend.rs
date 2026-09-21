//! `nspr amend`: bring the local commit messages back in line with GitHub.
//!
//! The flow this exists for: you open a pull request, a reviewer suggests a
//! better title, you edit it in the web UI. Now the local commit and the pull
//! request disagree, and the next `nspr diff --update-message` would quietly
//! undo the reviewer's wording. Pulling the edits down instead makes the
//! commit message the thing that lands, which is what a squash merge will use.
//!
//! The trailer block is never touched. It is nspr's own bookkeeping and has no
//! counterpart on GitHub.

use color_eyre::eyre::Result;

use crate::forge::Forge;
use crate::git::Git;
use crate::stack::Stack;
use crate::trailers::CommitMessage;

#[derive(Debug, Clone)]
pub struct Amended {
    pub number: u64,
    pub old_subject: String,
    pub new_subject: String,
}

/// Rewrite each layer's commit message from its pull request.
///
/// Returns only the layers that actually changed, so the caller can say
/// nothing at all when there was nothing to do.
pub async fn amend(
    git: &Git,
    forge: &dyn Forge,
    stack: &Stack,
) -> Result<Vec<Amended>> {
    git.check_no_uncommitted_changes()?;

    let mut rewrites = Vec::with_capacity(stack.layers.len());
    let mut changed = Vec::new();

    for layer in &stack.layers {
        let mut message = CommitMessage::parse(&git.message_of(layer.commit)?);
        if message.has_legacy_spr_trailer() {
            let pr_label = match layer.pr {
                Some(n) => format!("#{n} (`{}`)", layer.subject()),
                None => format!("`{}`", layer.subject()),
            };
            color_eyre::eyre::bail!(
                "pull request {pr_label} was created by `spr` (detected `Pull Request:` trailer).\n\
                 `nspr` will not modify `spr` pull requests automatically. Run `nspr upgrade` to convert them to native stacked pull requests."
            );
        }

        if let Some(number) = layer.pr {
            let pr = forge.get_pull_request(number).await?;
            forge.fetch_commit(pr.head_oid).await?;
            if crate::upgrade::has_spr_commit(git, pr.head_oid, stack.base)? {
                color_eyre::eyre::bail!(
                    "pull request #{number} (`{}`) was created by `spr` (detected `[spr]` commit).\n\
                     `nspr` will not modify `spr` pull requests automatically. Run `nspr upgrade` to convert them to native stacked pull requests.",
                    layer.subject(),
                );
            }
            let body =
                crate::pr_body::strip_warning(&pr.body).trim().to_string();
            if pr.title != message.subject || body != message.body {
                changed.push(Amended {
                    number,
                    old_subject: message.subject.clone(),
                    new_subject: pr.title.clone(),
                });
                message.subject = pr.title;
                message.body = body;
            }
        }

        // Every layer is listed, changed or not: `rewrite_messages` needs the
        // whole chain because rewriting one commit reparents everything above
        // it.
        rewrites.push((layer.commit, message.render()));
    }

    if !changed.is_empty() {
        git.rewrite_messages(stack.base, &rewrites)?;
    }
    Ok(changed)
}
