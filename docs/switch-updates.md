# Switch updates

The Switch build (an NRO started from hbmenu) updates itself from GitHub releases of
[`JohnUzoka/horizon`](https://github.com/JohnUzoka/horizon/releases), following the release
updater protocol 1 of Skate 3's Rust engine. The code is `crates/horizon-client/src/switch_update.rs`.
Other platforms don't use it; they have the launcher.

## What the player sees

1. With the first title screen of a launch, a background thread asks GitHub's API for the
   repository's releases. Without a connection the check fails quietly. Only the log records it.
2. If there's a newer Switch build, a toast says so, and the title's Realms button (disabled on the
   Switch) becomes **Update to build N**.
3. Pressing it downloads the release's `horizon.nro` next to the running one as
   `horizon.nro.download`, with the percentage on the button. The download is checked, then the
   running NRO is renamed to `horizon.nro.old` and the new one takes its name.
4. The button becomes **Quit to finish the update**. When the game exits through the homebrew
   loader, the loader starts the new NRO right away (`envSetNextLoad`). When the exit goes through
   AM to HOME instead, start Horizon again from hbmenu. That happens when heap pages are still held,
   for example by an open socket (`horizon-nx` `release_horizon_services`).
5. If anything fails, a toast gives the reason, the log has the details, and the button changes to
   **Retry the update**.

The game files prepared next to the NRO (`.local/`, `saves/`, `options.txt`) are never touched.
An update replaces only the NRO.

**Rolling back:** delete `horizon.nro` and rename `horizon.nro.old` to `horizon.nro`. Only one
older NRO is kept.

## The protocol

- **Source.** Releases come only from `https://api.github.com/repos/JohnUzoka/horizon/releases`,
  read 100 per page. A page with fewer than 100 releases is the last. A list longer than 10 pages
  is an error rather than a partial answer.
- **Which releases count.** Each release must meet all of these:
  - It is published: not a draft, not a pre-release, and it has `published_at`.
  - It is tagged `switch-<build>`. The build is a whole number from 1, written without leading
    zeros.
  - It has three assets, all in the `uploaded` state: `horizon.nro`, `horizon.nro.sha256` and
    `release.json`.
  - Each asset's `browser_download_url` is exactly
    `https://github.com/JohnUzoka/horizon/releases/download/<tag>/<name>`.
- **Which release wins.** The highest build above the running one, provided its `release.json`
  checks out. If it doesn't, the next highest is tried.
- **`release.json`** must match its release exactly:
  `{"schema":1,"target":"switch","repository":"JohnUzoka/horizon","build":N,"tag":"switch-N","revision":"<40 lowercase hex digits>"}`.
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

A build knows its number from `HORIZON_SWITCH_BUILD` at compile time. A development build has none
and counts as build 0, so any release updates it.

## Making a release

The port's workshop (`horizon-port/scripts/hx-release.sh BUILD`) does the build:

1. It refuses to run unless the clone is committed and its HEAD is on the fork's `switch` branch.
   It also refuses if the tag `switch-BUILD` already exists.
2. It builds the NRO with `HORIZON_SWITCH_BUILD=BUILD` and packs it with the NACP version
   `build BUILD`.
3. It writes `horizon.nro`, `horizon.nro.sha256` and `release.json` to `~/switch-release/switch-BUILD/`
   and keeps the ELF as `~/nro-elf/horizon-client-switch-BUILD`.

Publish those three files as a GitHub release tagged `switch-BUILD`, targeting the `switch`
branch. It must not be a pre-release. The script prints the steps and the equivalent
`gh release create` command.

The first release ships the updater, so it can't test it. The second release is the test: a Switch
running build 1 should offer build 2, install it, and start it.
