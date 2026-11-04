# Switch updates

The Switch build (an NRO started from hbmenu) updates itself from GitHub releases of
[`JohnUzoka/horizon`](https://github.com/JohnUzoka/horizon/releases), following the release
updater protocol of Skate 3's Rust engine, with versions where Skate 3 has build numbers
(`release.json` schema 2). The code is `crates/horizon-client/src/switch_update.rs`. Other
platforms don't use it; they have the launcher.

## What the player sees

1. The title's version line, bottom left, names the release: `Horizon 26.3 (Rust), Switch 0.1.0`,
   or `Switch dev build` for a build that isn't a release.
2. With the first title screen of a launch, a background thread asks GitHub's API for the
   repository's releases. Without a connection the check fails quietly. Only the log records it.
3. If there's a newer Switch release, a toast says so, and the title's Realms button (disabled on
   the Switch) becomes **Update to 0.1.1** (the version on offer).
4. Pressing it asks first: **Update Horizon?**, with the download's size and a note that worlds
   and settings stay, and **Update** or **Cancel**.
5. **Update** downloads the release's `horizon.nro` next to the running one as
   `horizon.nro.download`, with the percentage on the button. The download is checked, then the
   running NRO is renamed to `horizon.nro.old` and the new one takes its name. A toast says the
   version is installed.
6. The button becomes **Quit to finish the update**. When the game exits through the homebrew
   loader, the loader starts the new NRO right away (`envSetNextLoad`). When the exit goes through
   AM to HOME instead, start Horizon again from hbmenu. That happens when heap pages are still held,
   for example by an open socket (`horizon-nx` `release_horizon_services`); so far that is the usual case.
7. The new version's first launch shows **Horizon updated: Now on 0.1.1**. A
   `horizon.nro.updated` marker, left by the install and removed as it's read, tells it so. The old
   NRO, if started instead, doesn't claim it.
8. If anything fails, a toast gives the reason, the log has the details, and the button changes to
   **Retry the update**, which retries without asking again.

The game files prepared next to the NRO (`.local/`, `saves/`, `options.txt`) are never touched.
An update replaces only the NRO.

**Rolling back:** delete `horizon.nro` and rename `horizon.nro.old` to `horizon.nro`. Only one
older NRO is kept.

## The protocol

- **Source.** Releases come only from `https://api.github.com/repos/JohnUzoka/horizon/releases`,
  read 100 per page. A page with fewer than 100 releases is the last. A list longer than 10 pages
  is an error rather than a partial answer.
- **Versions.** `major.minor.patch`: three whole numbers, each written without leading zeros,
  nothing after them (no `-rc1`). They compare part by part, so 0.10.0 is above 0.9.0. 0.0.0 is
  the development build's and never a release.
- **Which releases count.** Each release must meet all of these:
  - It is published: not a draft, not a pre-release, and it has `published_at`.
  - It is tagged `v<version>`, as `v0.1.0`.
  - It has three assets, all in the `uploaded` state: `horizon.nro`, `horizon.nro.sha256` and
    `release.json`.
  - Each asset's `browser_download_url` is exactly
    `https://github.com/JohnUzoka/horizon/releases/download/<tag>/<name>`.
- **Which release wins.** The highest version above the running one, provided its `release.json`
  checks out. If it doesn't, the next highest is tried.
- **`release.json`** must match its release exactly:
  `{"schema":2,"target":"switch","repository":"JohnUzoka/horizon","version":"0.1.0","tag":"v0.1.0","revision":"<40 lowercase hex digits>"}`.
  The revision is the commit the NRO was built from.
- **`horizon.nro.sha256`** is `sha256sum`'s line: 64 hex digits, then `horizon.nro`.
- **Download.** The NRO must:
  - match that SHA-256;
  - match GitHub's own `digest` of the asset (`sha256:<hex>`), when the API gives one;
  - be at most 256 MiB;
  - have `NRO0` at offset 0x10.
- **Transport.**
  - HTTPS only, redirects included (`ureq` with `https_only`; asset downloads redirect to
    `release-assets.githubusercontent.com`).
  - Connect timeout 15 s, read timeout 30 s.
  - No token is ever sent. GitHub's unauthenticated limit is 60 API requests an hour per address,
    and a check makes one or two.

A build knows its version from `HORIZON_SWITCH_VERSION` at compile time. A development build has
none and counts as 0.0.0, so any release updates it.

## Making a release

The port's workshop (`horizon-port/scripts/hx-release.sh VERSION`) does the build:

1. It refuses to run unless the clone is committed and its HEAD is on the fork's release branch
   (`switch`, or `HX_RELEASE_BRANCH`'s). It also refuses if the tag `vVERSION` already exists.
2. It builds the NRO with `HORIZON_SWITCH_VERSION=VERSION` and packs it with the NACP version
   `VERSION`.
3. It writes `horizon.nro`, `horizon.nro.sha256` and `release.json` to `~/switch-release/vVERSION/`
   and keeps the ELF as `~/nro-elf/horizon-client-vVERSION`.
4. It adds `horizon-switch-VERSION.zip` for a first install: `switch/horizon/horizon.nro` plus the
   `client.txt` without which the NRO runs a dedicated server.
5. It tags the commit it built `vVERSION` and pushes the tag. The tag stays on that commit, which
   is `release.json`'s revision, even when the branch moves on.

Write the release notes beside them (`RELEASE-NOTES.md`). Publish the four files as a GitHub release
on the existing tag `vVERSION`, with the notes as its description. It must not be a pre-release.
The script prints the steps and the equivalent `gh release create` command.

A development build counts as 0.0.0, so the first release can already be tested: a Switch running a
development build offers it, installs it, and its next launch says it was updated and names the
version on the title.
