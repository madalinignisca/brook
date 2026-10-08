# Contributing to Brook

Brook is licensed under AGPL-3.0-or-later. Copyright is held by each contributor for their
own changes (Copyright 2026 Madalin Ignisca and Brook contributors). By contributing, you
license your change under AGPL-3.0-or-later and keep your copyright.

## Developer Certificate of Origin

Every commit in a pull request must be signed off under the
[Developer Certificate of Origin 1.1](https://developercertificate.org/):

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

### How to sign off

Add `-s` to `git commit`:

```
git commit -s -m "api: rotate session tokens on login"
```

This adds a line `Signed-off-by: Your Name <you@example.com>` to the message. The email
must be the commit author's email. CI (`.github/workflows/dco.yml`) fails a pull request
if any commit lacks that line. Merge commits are not checked.

To fix a branch whose commits lack the sign-off:

```
git rebase --signoff origin/main
git push --force-with-lease
```

## Working rules

Start from an issue you opened, and name the branch `<issue number>-<short-name>` (the
issue's "Create a branch" button does this). The full rules are in
[CLAUDE.md](CLAUDE.md) section 8 and the quality standard is in [docs/QUALITY.md](docs/QUALITY.md).

## New source files

Start each new source file with the two SPDX lines, in the file's comment syntax:

```
SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
SPDX-License-Identifier: AGPL-3.0-or-later
```
