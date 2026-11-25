# Tonbo's Firecracker fork

This repository is a fork of
[firecracker-microvm/firecracker](https://github.com/firecracker-microvm/firecracker)
that carries a short stack of patches on top of upstream `main`. Everything
else here is upstream's; `README.md` and `docs/` describe Firecracker itself.

Questions, bug reports and design discussion about Firecracker belong
upstream, in its issues and pull requests. Issues and discussions are closed
on this fork for that reason. The patches below are meant to go upstream, and
each one that has an upstream issue or pull request names it.

## Branches and tags

- `upstream-main` mirrors upstream `main`. It only ever fast-forwards to an
  upstream commit and never carries a commit of ours.
- `main` is `upstream-main` plus the patch stack: the stack is exactly
  `upstream-main..main`, linear, one logical patch per commit (or a short
  series when the patch is someone else's series, kept under its author's
  name).
- `tonbo-YYYYMMDD.N` are annotated tags on states of `main` that a build may
  pin. A tag is never moved or deleted, so the commits of every build stay
  reachable whatever happens to the branch later.
- `tonbo/<upstream commit>` branches are earlier stacks on older upstream
  commits, kept as a record. `tonbo/ci` holds the workflow that checks them.

A ruleset forbids force-pushes and deletions on `main`, `upstream-main` and
`tonbo/*`; the only exception is the move of `main` to a newer upstream,
described below. Another ruleset forbids moving or deleting `tonbo-*` tags,
without exception.

## The patch stack

In order, on top of `upstream-main`:

1. **This file and `.github/workflows/tonbo.yml`.** The only files the fork
   adds outside the source tree.
2. **A full snapshot leaves all-zero blocks of its memory file as holes.**
   When the memory file reads as zeros before the dump (new, or truncated
   because its size changed), a full snapshot seeks over each all-zero 4 KiB
   block instead of writing it. The file has the same bytes and is sparse.
   No upstream issue yet.
3. **virtio-blk discard on the async (io_uring) engine.** The commits of
   upstream pull request
   [#5935](https://github.com/firecracker-microvm/firecracker/pull/5935),
   taken at its head `23b7ce51354f`, under their authors' names.
4. **A block device's rate limiter re-arms 10 ms after a dry bucket** instead
   of 100 ms, so a throttled request waits about 10 ms past its tokens. The
   net and entropy limiters keep 100 ms. No upstream issue yet.
5. **Discard remaps after a file-backed restore are bounded to whole 2 MiB
   windows.** The edge pages of a discard are zeroed in place, so the number
   of mapping splits is bounded by the guest memory size. No upstream issue
   yet. It is last so that it can be dropped without touching the others.

Not in the stack yet: several data segments per virtio-blk request
(`VIRTIO_BLK_F_SEG_MAX`), upstream issue
[#6441](https://github.com/firecracker-microvm/firecracker/issues/6441).

## Checks

`.github/workflows/tonbo.yml` runs on every push to `main` and to `wip/**`
scratch branches, on GitHub-hosted
arm64 runners: the stack is linear on `upstream-main` and every commit is
signed off, our commit messages pass upstream's gitlint rules, the tree is
formatted, every commit of the stack passes clippy by itself, and the unit
tests of the code the patches touch pass (the virtio-blk device, io_uring,
the rate limiter, guest memory). These run without KVM. Upstream's full unit
and integration tests need `/dev/kvm` and upstream's development container
and are not run here. Upstream's own workflows are disabled in this fork.

## Moving to a newer upstream

The stack is rebased onto the new base, and builds only ever pin tags:

1. Tag the current tip of `main` if it has no tag yet
   (`tonbo-YYYYMMDD.N`), so the old stack stays reachable.
2. Pick an upstream `main` commit whose required upstream CI is green and
   fast-forward `upstream-main` to it:
   `git push origin <commit>:refs/heads/upstream-main`.
3. On a scratch branch (`wip/...`, which the workflow also checks), rebase
   the stack onto `upstream-main`, one commit per patch as above. Drop a
   patch that upstream has merged, and replace a series taken from an
   upstream pull request with that pull request's current commits.
4. Wait for the checks on the scratch branch, and review the move with
   `git range-diff <old base>..<old tag> upstream-main..<new tip>`: it shows
   each patch against its earlier version, which is the whole review.
5. Tag the new tip:
   `git tag -a tonbo-YYYYMMDD.N -m "<upstream commit>, <patch list>"` and
   `git push origin tonbo-YYYYMMDD.N`.
6. Only then move `main`. This is the one push that is not a fast-forward:
   an organization admin sets the branch ruleset's enforcement to disabled,
   runs `git push --force-with-lease=main:<old tip> origin <new tip>:main`,
   and sets it back to active. The ruleset has no standing bypass, so
   nothing else can rewrite `main`. Delete the scratch branch.

Why a rebase: the stack stays a short list of commits on top of an upstream
commit, so a sync is reviewed patch by patch, a build knows exactly what it
adds to upstream, and each patch can be sent upstream as it is. Merging
upstream into `main` would avoid the forced push but blur the patches into
merge resolutions over time. The cost of the rebase, a rewritten branch, is
carried by the tags: every state that was ever built keeps its tag, and
tags are never moved or deleted.

Commits of ours are signed off and written for upstream readers. Commits
taken from someone else keep their author and message, with a
`(cherry picked from commit ...)` line.
