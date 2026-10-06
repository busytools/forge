# forge client

The app that connects to a running forge and draws it. Svelte 5 on Vite,
with a Tauri shell for the desktop and Android.

It is not a second forge. Every client connects to a forge someone else
started, and the socket carries everything the pages draw.

## Running it

```sh
npm install
npm run dev      # dev server on http://localhost:1420
npm run build    # dist/
npm run test     # vitest
npm run mutate   # Stryker over the modules named in stryker.conf.json
npm run typecheck
```

`1420` is fixed rather than defaulted, because the Tauri shell points its
`devUrl` at it.

A `node_modules` from before the client's own update existed fails seven test
files at import until `npm install` refreshes it: the update calls are what
brought `@tauri-apps/api` in.

`just client-dev` runs the app itself in development: the window over that
dev server, reloading on a frontend edit, with nothing installed and no
bundle built.

## The shell

`src-tauri/` is a Tauri 2 app that wraps the built bundle. The crate is
its own cargo workspace root, so the repo's cargo gates and CI's cargo jobs
do not reach it; the Unicode punctuation gate and the client's Prettier
step do.

```sh
npm run tauri dev     # dev server plus the app window
npm run tauri build   # forge.app and a dmg, under src-tauri/target/release/bundle
```

Which build needs `dist/` is worth knowing. A bare `cargo build` does not,
because it never resolves `frontendDist` at all. The CLI build does, and
runs `npm run build` first to produce it. The dev window loads the dev
server instead and needs none.

Three recipes, and the difference is the bundle. `just client-tauri-check`
is the routine gate before handing a change here over: it compiles the
shipping configuration, which resolves `dist/` and validates the
identifier, without producing a bundle, so it is quiet.
`just client-tauri-bundle` adds `forge.app` and the dmg, and is the only
one that produces the dmg. It is also the only one that opens a Finder
window, because the dmg step mounts the image and lays the mounted volume
out with AppleScript. `just client-release` builds the app bundle alone
and installs it over `/Applications/forge.app`; it is the client half of
`just release`, and `--bundles app` is what keeps the dmg step out of it.

Two traps sit between a hand-typed command and a working one, and the
recipes here carry the working form. The CLI reads `CI` from the environment
as its own boolean `--ci` flag, so a `CI` holding anything but `true` or
`false` - `0` is what forge's sessions set - stops the build before it
starts; passing `--ci` explicitly overrides it. And `npm` swallows a bare
`--`, so a cargo flag has to arrive as `run tauri -- build -- <flags>`.

A start has no terminal to report to, so it writes to
`~/Library/Logs/dev.vedhavyas.forge/forge.log` instead. That file is
named after the product rather than the crate, and it is appended to
rather than rewritten, so read the last line: `forge client started`
means it reached the window, and `failed to start` means it did not and
carries the reason. A start that fails before the logger exists writes
that line itself, because nothing else would.

The icon is the `panes` mark from `src/brand.ts`, on the `--bg` ground in
the `--accent` colour. Render the mark to a 1024 PNG and pass that to
`npx tauri icon` - the `icons/icon.png` the generator writes is a 512 and
will not reproduce the `.icns` on its own, so it is not committed. The
five files `bundle.icon` names are, and the first `.png` in that list is
the one the codegen embeds, so deleting `icons/32x32.png` is a hard
compile failure rather than a smaller bundle.

The version is this crate's own rather than the workspace's: it sits in this
crate's `Cargo.toml`, because the shell is its own workspace root and the
workspace bump cannot reach it. `just release` bumps it to the release
version, so the tag and the installed app carry the same number.

## The client's own update

The desktop shell checks for a newer release once at launch: one request to
`releases/latest/download/latest.json` on `busytools/forge`, from the
`plugins.updater` block in `tauri.conf.json`. The home header draws a
`client ↑ v1.0.116 available` line when there is one; clicking downloads and
installs it, and the line then offers the restart that finishes the swap.
Nothing restarts on its own. A browser tab against the same forge draws none
of it - there is no shell to update.

The bundles are signed with a minisign keypair of their own, separate from
the app's codesign and from the Android release keystore:

```sh
mkdir -p ~/.tauri && chmod 700 ~/.tauri
npm run tauri -- signer generate -w ~/.tauri/forge-updater.key
```

The public half is inlined in `tauri.conf.json` (`plugins.updater.pubkey`).
The private key and its password live only under `~/.tauri/` - never in the
repo - and the release recipes read them through
`~/.tauri/forge-updater.properties` (`keyFile`, `keyPassword`).

**Back all three files up.** Losing the private key or its password means no
future release can be signed for the desktops that exist: every installed
client would need a manual reinstall onto a new key. The same discipline the
release keystore carries, for the same reason.

`bundle.createUpdaterArtifacts` makes the app bundle target emit
`forge.app.tar.gz` and its `.sig` beside it. The release recipes fail rather
than skip when the key is missing, the way the Android half fails on its
keystore, because a release that publishes no signed tarball is a release
no client can update from. And `client-release` reads the built pair back
before swapping anything in: the CLI signs with whatever key the environment
names and only warns when that is not the pinned one, so
`scripts/check_updater_signature.py` compares the signature's key id with
`tauri.conf.json`'s pubkey and the signed version with the release's, the way
`client-android-release` reads the APK's signer back.

A release publishes five assets: `forge.app.tar.gz` and its `.sig`, the arm64
APK, the web archive, and `latest.json` - the manifest all three halves read.
`scripts/update_manifest.py` writes the manifest with the `.sig`'s content
(only its trailing newline trimmed, because the plugin base64-decodes the
value whole), the web archive's sha256, and the release URLs; the `android`
and `web` blocks sit at the top level rather than inside `platforms`, because
every `platforms` entry must carry both a `url` and a `signature` or the
whole file fails to parse. `just publish <version>` creates the release for a
tag already on origin, and `just release` runs it itself as its last step.

The phone updates from the same manifest. Its top-level `version` is what the
app compares against its own, and the `android` block beside `platforms`
carries the APK's `url` (the desktop parser ignores that key). The download
is checked before it can reach the installer - its signer against this
install's own signer, which is the release keystore's on a release build and
is exactly the identity the system installer checks anyway, and its
`versionName` against the manifest's version. A file that fails is deleted
rather than reused; a file that passes is kept in the cache, so handing it to
the installer again is a tap with no second download. `REQUEST_INSTALL_PACKAGES`
is what the handoff needs, the installer prompt is the confirmation, and no
silent path exists for a sideloaded app. The Android side is
`app/src/main/java/dev/vedhavyas/forge/UpdatePlugin.kt` plus the manifest
permission, both project source that `tauri android init` would regenerate
away - re-apply them the way the section below describes.

## The web image

The same build, served to browsers: a client-only image under `client/docker/`
that carries the built `dist/` and serves it from a volume. There is no server
half in it - a page connects to a forge somewhere else.

```sh
npm --prefix client run build
docker build -f client/docker/Dockerfile -t forge-web client
docker run -p 8080:8080 -v forge-web:/srv/forge-web forge-web
```

The image ships with the build inside it, so an empty volume is seeded on
first start. A small poller then keeps the volume at the published release:
it reads the same `latest.json` every other half reads, takes the `web`
block's version, url and sha256, verifies the archive before extracting it,
and flips a `current` symlink - an update lands on the next request and
nothing ever restarts. A failed poll is not fatal; the container keeps
serving what it has. The manifest is written beside the app, and the page
reads it same-origin to draw which build it is serving and what is published.

`Cache-Control: no-cache` plus an ETag is a rule, not a default: a swapped
build must not hide behind a cache. It is pinned in `docker/nginx.conf` and
in `docker/test_serving.sh`, which runs against the built image;
`docker/test_poller.sh` pins the poller's swap, its same-version skip and its
refusal of an archive that does not match the manifest's sha256.

The image is published to `ghcr.io/busytools/forge-web` on a release tag,
under `v<version>` and a moving `latest`.

## The Android target

The same shell builds for Android through Tauri's own CLI. It wants the
Android SDK with an NDK, and a JDK 17 or newer, and the raw command below
reads `ANDROID_HOME` or `ANDROID_SDK_ROOT`, `NDK_HOME` and `JAVA_HOME`:

```sh
export ANDROID_HOME=/path/to/sdk     # a root with cmdline-tools in it
export NDK_HOME="$ANDROID_HOME/ndk/<version>"
export JAVA_HOME=/path/to/jdk
```

`just client-android-release` resolves those itself rather than requiring
them, because a fresh shell may export none of the three while the CLI's own
fallback lands on an empty `~/Library/Android/sdk` first: it takes an SDK
that holds `platforms/` or an `ndk/`, the NDK inside it, and java from
`PATH`, prints what it found, and fails with this section's name when it
genuinely cannot.

`src-tauri/gen/android/` is the Gradle project `tauri android init`
generates, and it is committed: the manifest, the Kotlin activity and the
Gradle files are project source rather than build output, so its four
local edits - the manifest's mic permissions and its
`REQUEST_INSTALL_PACKAGES`, the activity's back handling, the update plugin
(`app/src/main/java/dev/vedhavyas/forge/UpdatePlugin.kt`) and the release
signing block in `app/build.gradle.kts` - survive a clean clone. Re-running
`tauri android init` overwrites them, so re-apply them after one. The debug
APK is one command, and it builds the frontend first the same way the
desktop build does:

```sh
npm run tauri -- android build --debug --apk --ci
```

It lands at
`src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`,
signed with the debug keystore under `~/.android/`, and side-loads onto a
phone. `--target aarch64` builds one ABI rather than all four.

The manifest declares `RECORD_AUDIO` and `MODIFY_AUDIO_SETTINGS`: wry's
`WebChromeClient` requests both together for a `getUserMedia` prompt, so one
missing from the manifest denies the whole request.

### The release APK

`just release <version>` stages a release-signed APK at
`src-tauri/target/release/bundle/android/forge-<version>-arm64.apk`, and
`just client-android-release <version>` runs that half alone; the release's
publish step attaches the file. Release builds sign
with a keystore at `~/.android/forge-release.keystore`, minted once:

```sh
keytool -genkeypair -v -keystore ~/.android/forge-release.keystore \
  -alias forge -keyalg RSA -keysize 4096 -validity 10000 -dname "CN=forge release"
chmod 600 ~/.android/forge-release.keystore
```

and named in `src-tauri/gen/android/app/key.properties`, which is gitignored
like the keystore itself; neither is ever committed:

```properties
storeFile=/Users/you/.android/forge-release.keystore
storePassword=<the password you chose>
keyAlias=forge
keyPassword=<the same password>
```

`storeFile` must be an absolute path: a relative one resolves inside the
checkout. The keystore itself (and any `*.jks`) is gitignored at any depth
under `gen/android/`.

The recipe refuses before building when either file is missing, and reads
the version, the ABI and the signer back off the built APK afterwards. Keep
both files backed up: the keystore is the app's identity, and every future
release must be signed by it to upgrade in place. One one-time note: the
first release-signed install needs the debug-signed build uninstalled from
the phone first, since Android cannot replace an app across signatures.

## What is here

- `src/wire/` - the server's shapes, copied from the fixtures on
  `origin/forge-server-socket`. The pages type their props against these,
  so the two halves cannot disagree about what a home looks like.
- `src/components/` - the pieces every page draws with: the row, the band
  card, the lifecycle mark, the brand mark, the icon sprite. One theme, one
  copy of each.

**The sprite is inlined by the shell on every page and nothing yet points at
it.** No page in the base slice draws an icon, so `Icon.svelte` has no
caller and removing `<Sprite />` from `Shell.svelte` would break nothing a
test sees. It is here because the plan's salvage set names it and because
the session and composer tasks run in parallel and cannot edit this shared
base. The first icon belongs to Task 5's inspector, and that is where the
sprite stops being dead weight.

- `src/theme.ts` - the palettes and typeface stacks the names resolve to.
  The greeting carries the NAMES; the values live here, and there is no
  client-side reader of `forge.toml` by any path.
- `src/assets/` - `web.css` and `sprite.svg`, taken from `crates/forge-web`
  before that crate is deleted. The fonts live in `public/fonts/`.

`forge.toml` is the server's, and the client never reads it: `[web] mark`,
`[web] theme` and `[web] font` arrive in the greeting. `ClientSettings`
carries a name for each, not a value.

## Every cell the server's home draws

The four reads this file used to list as missing now cross on a project
row, and the home draws all four: `work` fills `.row .where`, `tasks`
fills `.row .what` with its status chip and its artifact link, `would_bind`
decides the refusal's second arm, and `forge_version_short` carries the
header's version. The fixture's `projects[]` entries are
`{project, work, tasks, crons, would_bind, chip}`; `crates/forge-web/src/home.rs`
is still the port's source for the markup.

**The unseen marks cross too**, as `unseen` - a list of slots. The server's
`Live` owns that fact and a client cannot reconstruct it from the records,
so it is carried rather than derived, and the client draws the state when
it is handed one.

## Two the snapshot carries and nothing draws

`service_status` and `fatal_error` DO cross on the home subject, and no
page reads either. So a forge that is exiting or that failed at startup
draws as a healthy fleet. Dropped on purpose for now rather than by
oversight: both are a view of their own, and the home's band is not where
they belong.

Two more ride the home row and no page in this slice draws them: `crons`,
which is the inspector's schedules section, and `chip`, which is the
account a row binds. Both belong to pages that do not exist yet.

## Where a cold load lands

The app opens on the address it last connected to, which it keeps in the
webview's own store. `src/connect/remembered.ts` is the store,
`src/connect/boot.ts` is the launch: read the address, try it, and land on
the home when something answers. When nothing does, or when there is nothing
remembered, the door comes back carrying the address that was tried and the
reason it did not answer.

A launch with nothing to open on rewrites `/` to `/connect`, which is the
door's own URL, so a reload lands in the same place. What is kept is the
address that last WORKED - a failed one is not forgotten, since that is the
one a person is about to fix, and retrying it is what the next launch is for.

**The connection is booted on any route; the ROUTE moves only from the
root.** A deep link is how a seat stays reachable and `/fixture` draws
without a server, so both keep the page they were addressed at and take the
socket that page reads. Only `/` lands on the home or the door, because it is
the one URL that names no page of its own.

`/connect` is where a failed launch is easiest to see: the door is already on
screen when the attempt lands, so the reason arrives on it after the fact
rather than opening it. That is why the door follows the shell's failure
instead of seeding it once.
