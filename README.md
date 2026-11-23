# CI for the patch branches

This branch holds only the workflow that checks this fork's patch branches.
It shares no history with them, so a patch branch's tree stays exactly the
upstream tree plus its patches, and `main` stays a mirror of upstream `main`.

A patch branch is named `tonbo/<upstream base commit>` and carries one commit
per patch on top of that upstream commit. Branches are never rewritten: a
move to a new upstream base is a new branch.

`.github/workflows/patch-branches.yml` runs on GitHub-hosted arm64 runners
when this branch is pushed: clippy on the `vmm` crate and the unit tests of
the code the patches touch (the virtio-blk device, io_uring, the rate limiter
and guest memory), for each branch in its matrix. These run without KVM.
Upstream's full unit test suite and its integration tests need `/dev/kvm` and
upstream's development container, and are not run here.

To check a patch branch's new tip, re-run the latest run. To check a new
branch, add it to the workflow's matrix.
