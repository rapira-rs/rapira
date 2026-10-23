# Vouch setup

Vouch checks contributor access for `rapira-rs/rapira`. Maintainers manage [the list](../VOUCHED.td) through signed commits. See [contributor access](../../CONTRIBUTING.md#contributor-access) for the approval process.

## Workflow

- The `Vouch PR` workflow closes PRs from authors who are not on the list and have no write, maintain, or admin access.
- The workflow reads the list and the closure message from the repository's default branch.
- Vouch exempts bot logins that end in `[bot]`.

The workflow uses `GITHUB_TOKEN` with contents read and pull requests write permissions. This setup needs no GitHub App or additional secret. The [Vouch cookbook](https://github.com/mitchellh/vouch/blob/v1.5.0/COOKBOOK.md) describes App tokens for automated list updates that must bypass branch rules.

## Activate outside contributions

Merge the workflow, list, request form, and contributor instructions into `main` before you open PR creation to outside users.

The repository needs an applicable [Actions event policy](https://docs.github.com/en/actions/reference/security/securely-using-pull_request_target#default-policy-for-pull_request_target) that allows `pull_request_target`. The `Allow Vouch PR checks` policy is defined in [actions-policy.json](actions-policy.json). It permits that event only for `.github/workflows/vouch-pr.yml`.

If the policy is absent, create it from the repository root:

```sh
gh api --method POST \
  -H 'X-GitHub-Api-Version: 2026-03-10' \
  repos/rapira-rs/rapira/actions/policies \
  --input .github/vouch/actions-policy.json
```

Inspect existing policies with `gh api 'repos/rapira-rs/rapira/actions/policies?has_parents=true'`. To update an existing policy, use `--method PUT` and append its ID to the API path. An organization or enterprise restriction can still block the event.

Set the repository PR creation policy to `all`:

```sh
gh api --method PATCH repos/rapira-rs/rapira \
  -f pull_request_creation_policy=all \
  --jq '{has_pull_requests,pull_request_creation_policy}'
```

The `collaborators_only` policy blocks outside PR creation before Vouch can check the author. A Vouch approval cannot override that GitHub setting. See [GitHub PR access settings](https://docs.github.com/en/repositories/managing-your-repositorys-settings-and-features/enabling-features-for-your-repository/disabling-pull-requests).

Confirm the first outside contribution through the Actions run log. An unknown author should receive a closure comment.

## Operation

Vouch controls whether a contribution stays open. The existing build workflows run independently of Vouch.

A successful Vouch job can mean that it closed a contribution. Its success status is not a merge-approval check. GitHub permissions and branch rules control who can merge.

Check the workflow log if Vouch closes a PR from an approved author: a failed list read can make the author appear unknown. Reopen the PR to run the check again.

To suspend outside PR creation, restore the repository policy:

```sh
gh api --method PATCH repos/rapira-rs/rapira \
  -f pull_request_creation_policy=collaborators_only
```

Organization interaction restrictions and Actions policies can also apply. Review them with an organization owner if outside users remain blocked after the repository policy changes.
