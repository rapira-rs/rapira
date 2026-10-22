# Vouch setup

Vouch checks contributor access for `rapira-rs/rapira`. Maintainers manage [the list](../VOUCHED.td) through signed commits. See [contributor access](../../CONTRIBUTING.md#contributor-access) for the approval process.

## Workflows

- `Vouch PR` closes PRs from unknown or blocked external contributors.
- `Vouch Issue` closes issues from blocked external contributors. Other users can open issues and request contributor access.
- Both workflows read the list from the repository's default branch. The PR workflow uses a message from that branch for unknown contributors. Blocked contributors receive Vouch's standard message.
- Vouch exempts accounts with repository write, maintain, or admin access. It also exempts bot logins that end in `[bot]`.

The workflows use `GITHUB_TOKEN`. They need contents read permission and write permission for the contribution type they close. This setup needs no GitHub App or additional secret. The [Vouch cookbook](https://github.com/mitchellh/vouch/blob/v1.5.0/COOKBOOK.md) describes App tokens for automated list updates that must bypass branch rules.

## Activate outside contributions

Merge the workflows, list, request form, and contributor instructions into `main` before you open PR creation to outside users.

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

Confirm the first outside contribution through the Actions run log. An unknown author should receive a closure comment. After a maintainer adds the author to the list on `main`, the author can reopen the PR.

## Operation

Vouch controls whether a contribution stays open. GitHub still requires maintainer approval for CI from external contributors under the `all_external_contributors` policy. A Vouch approval does not approve a CI run. The existing build workflows run independently of Vouch.

A successful Vouch job can mean that it closed a contribution. Its success status is not a merge-approval check. GitHub permissions and branch rules control who can merge.

List changes apply to future check events. Close existing contributions manually after blocking a user. Check the workflow log if Vouch closes a PR from an approved author: a failed list read can make the author appear unknown.

To suspend outside PR creation, restore the repository policy:

```sh
gh api --method PATCH repos/rapira-rs/rapira \
  -f pull_request_creation_policy=collaborators_only
```

Organization interaction restrictions and Actions policies can also apply. Review them with an organization owner if outside users remain blocked after the repository policy changes.
