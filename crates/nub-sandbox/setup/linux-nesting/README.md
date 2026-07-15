# Linux direct-nesting setup

Nub's Linux sandbox composes directly: a sandboxed process can launch a stricter sandbox inside itself. That needs nested unprivileged user namespaces, which Ubuntu's default AppArmor policy blocks. This one-time administrator setup opts a dedicated, root-owned copy of the packaged Bubblewrap back into that capability for one exact path, and authorizes one group to run it.

Nub never runs this setup itself. It is human-run, and installs nothing privileged on your behalf.

```sh
# from an installed Nub, the packaged Bubblewrap is at bin/nub-resources/bwrap
sudo ./install.sh --bwrap /path/to/nub-resources/bwrap --user "$USER"
# then start a fresh login so the new group membership applies
```

Pass `--sha256 <hex>` (nub's `NUB_BWRAP_SHA256`) to hard-verify the source bytes before install; otherwise the script prints the installed digest to compare. The version string alone does not pin the bytes, and a mismatched build fails nub's runtime integrity gate rather than at install time.

## What the setup does

The script is idempotent — re-running it re-verifies each step and repairs only what drifted.

```sh
# 1. a system group whose members may run the helper
groupadd --system nub-sandbox

# 2. the packaged, unmodified Bubblewrap 0.11.2, root-owned and group-executable,
#    NOT setuid
install -o root -g nub-sandbox -m 0750  <bwrap>  /usr/libexec/nub/nub-bwrap

# 3. a path-bound AppArmor profile granting userns to THAT path only
install -m 0644  nub-bwrap-userns.apparmor  /etc/apparmor.d/nub-bwrap-userns
apparmor_parser --replace /etc/apparmor.d/nub-bwrap-userns
```

The global `apparmor_restrict_unprivileged_userns` control is left enabled. The setup never disables it, and Nub never recommends disabling it.

## How Nub uses the helper

When a launch must nest, Nub selects this helper for the outermost sandbox — a stock `bwrap` outer cannot transition to the helper's profile under `no_new_privs`, so the helper has to be the level-1 launcher. Before running your program, Nub verifies the helper is the exact root-owned inode, unwritable by group and other, and byte-identical to the packaged build, then runs one bounded nested launch to confirm the host actually nests. If any check fails it fails the launch closed with a precise reason — a missing group membership, a failed integrity check, an unloaded profile, or a host that cannot create user namespaces — and never falls back to a helper that cannot nest.

Single-level sandboxes are unaffected: without this setup Nub still uses the system or bundled Bubblewrap for ordinary confinement, exactly as before.

## Security tradeoff

Read this before running the setup.

The distribution restricts unprivileged user namespaces on purpose: they are a large kernel attack surface. This setup opts back in — but narrowly. The grant applies to one executable at one path, for one group. Every other executable on the host stays under the default restriction, and the global control stays enabled.

The concrete cost: a member of the `nub-sandbox` group can execute a helper that creates unprivileged user namespaces. That is the same capability the distribution withholds by default. Add only the users who run Nub sandboxes, and understand that group membership is what authorizes the capability.

The helper is not setuid and holds no elevated privileges of its own; it is upstream Bubblewrap, pinned by digest, running as the calling user. What the profile changes is only whether the kernel lets that one path create a user namespace.
