# Multiple projects on one Claude docker-local worker

A result adapter retains `permission_digest` as its independently installed
primary plan. Operators may install up to 31 additional distinct, exact plan
digests in `additional_permission_digests` on the Claude docker-local profile.
An omitted or empty list preserves existing canonical definition bytes. Other
adapter profiles reject a nonempty list.

Each entry hashes the whole canonical manifest permission plan, including
requested, effective, required, and omitted permissions. Install a new immutable
adapter definition and matching runtime/harness qualification references when
changing this list. Keep every project's catalog permissions and ceiling scoped
to that project and repository. The list grants no authority: admission, retained
execution scope, material delivery, and live grants still control execution.

The result channel pins the actual admitted plan from the retained original
manifest. Result installation checks that same plan after validating it against
the installed adapter. An unlisted plan, duplicate entry, or changed permission
field is refused. Sharing a worker does not require sharing project permissions.
