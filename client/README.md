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
its own cargo workspace root, so the repo's cargo gates and CI's other
cargo jobs do not reach it: CI's `client-tauri` job runs its clippy and
tests, and the `cargo fmt --check` job runs the rustfmt check against its
manifest. `just check` reaches it through
`client-tauri-fmt` (the shell's own `cargo fmt --check`, which `just fmt`
runs too), `client-tauri-clippy` and `client-tauri-test` (the crate's own
clippy and nextest run) and `client-android-warnings` (the crate compiled for
`aarch64-linux-android` with warnings denied), and the Unicode punctuation
gate and the client's Prettier step do too.

```sh
npm run tauri dev     # dev server plus the app window
npm run tauri build   # forge.app and a dmg, under src-tauri/target/release/bundle
```

Which build needs `dist/` is worth knowing. A bare `cargo build` does not,
because it never resolves `frontendDist` at all. The CLI build does, and
runs `npm run build` first to produce it. The dev window loads the dev
server instead and needs none.

The recipes differ by what they cover. `just client-tauri-clippy` and
`just client-tauri-test` are the crate's lints and its tests alone.
`just client-tauri-check` is the routine gate before handing a change here
over: it runs those tests, then compiles the shipping configuration, which
resolves `dist/` and validates the identifier, without producing a bundle,
so it is quiet. `just client-tauri-bundle` adds `forge.app` and the dmg,
and is the only one that produces the dmg. It is also the only one that
opens a Finder window, because the dmg step mounts the image and lays the
mounted volume out with AppleScript. `just client-release` builds the app bundle alone
and installs it over `/Applications/forge.app`; it is the client half of
`just release`, and `--bundles app` is what keeps the dmg step out of it.

Two traps sit between a hand-typed command and a working one, and the
recipes here carry the working form. The CLI reads `CI` from the environment
as its own boolean `--ci` flag, so a `CI` holding anything but `true` or
`false` - `0` is what forge's sessions set - stops the build before it
starts; passing `--ci` explicitly overrides it. And `npm` swallows a bare
`--`, so a cargo flag has to arrive as `run tauri -- build -- <flags>`.

## The browser host

The client hosts the browser a session's `browser_*` tools drive: it owns
the browser process, its profile and the driver, and answers the asks the
socket routes to it. `just vendor-browser-stack` fetches and verifies the
driver into the gitignored `src-tauri/browser-stack/` directory - node
and `@playwright/mcp` - and `bundle.resources` carries that tree into the
bundle, so nothing downloads at first use. **The engine is the app's own
Chromium**: `just bundle-cef` lays the CEF framework and its five helper
apps into the built `forge.app`, and each profile's browser is a windowless
host process launched from that bundle - a hand-off's Open, or the strip's
show, draws its frames as the full-window takeover. No installed browser is
needed, and a bundle built without the framework refuses the browser doors
by name. The bundling recipes run both steps themselves;
`just client-tauri-check` does not, because it copies no resources.

`src-tauri/src/browser/` is the host, and four things in it are worth
knowing before changing them:

- **The browser host is launched detached, and it dies with the client that
  launched it**: the client claims each profile it launches or adopts, reaps
  its hosts on exit, and sweeps at boot for the ones a dead client left. The
  profile under the app's data directory is what survives a restart: the CEF
  cache path is the profile itself, and the host's DevTools port and ring
  name are recorded beside it. The client picks the port and writes the
  record before it waits, so a host that dies mid-launch is swept rather
  than left holding the profile's singleton lock.
- **The driver is upstream's own** `@playwright/mcp`, spawned as a child
  process and spoken to as an MCP client through `rmcp`, pointed at the
  browser's CDP endpoint with `--no-webmcp` (without which the tool surface
  would depend on what the open page chooses to expose).
- **The capability is declared only where a host is really there**
  (`canHost`, `src/browser/host.ts`): the subscribe carries `browser: true`
  from the shell and never from a page opened outside it, because an ask
  routed to a client that cannot serve it arrives as a session's tool call
  failing.
- **A named profile is a browser of its own.** `profile: "name"` on a tool
  call launches (or attaches to) a browser on its OWN data directory, and the
  driver attaches to it exactly as it attaches to the shared one. Logins,
  cookies and sessions persist natively, the session that opened it owns it
  (another session is refused by name until it is released), and a hand-off
  naming it raises THAT profile's window on THAT profile's page - a CAPTCHA
  solved in the right session, not a lookalike in the wrong one.

`client/src-tauri/tests/cef_host.rs` pins the host's own decisions with a
stand-in binary - the record, the ring's name, the endpoint wait, the kill
by pid, the sweep that respects another client's claim and the one that
cleans a stale record. It is macOS-gated because the engine is.
`client/src-tauri/tests/bridge_live.rs` drives the whole chain - a real
session's `browser_navigate` answered by the vendored driver while no page
is drawing - and is `#[ignore]`d because it needs a scratch forge and the
vendored stack; run it where both are present:

```sh
cargo nextest run --manifest-path client/src-tauri/Cargo.toml --run-ignored ignored-only
```

A start has no terminal to report to, so it writes to
`~/Library/Logs/dev.vedhavyas.forge/forge.log` instead. That file is
named after the product rather than the crate, and it is appended to
rather than rewritten, so read the last line: `forge client started`
means it reached the window, and `failed to start` means it did not and
carries the reason. A start that fails before the logger exists writes
that line itself, because nothing else would.

The page's frame cost goes to
`~/Library/Logs/<identifier>/forge-perf.log` (`<identifier>` the shell's
bundle identifier - `dev.vedhavyas.forge` for the installed app), one
JSON line per record: `run_started` when a page attaches, `summary` every
ten seconds (frames, how many were slow, the worst gap), and a `frame` line
for each frame at or over 50 ms with the surface and seat it happened
on, plus `longtask` lines naming the work that blocked. A slow frame also
carries the Web Worker heartbeat: `worker_beats` (the worker's
one-a-second ticks that reached the page across the gap),
`worker_max_late_ms` (the WORST lateness among them, omitted when no beat
landed - a blocked main thread queues the ticks and delivers them in one
burst), `worker_since_beat_ms` (how long before the frame the last tick
arrived, absent until the first tick: that first arrival calibrates the
clocks and reports a lateness of zero), `worker_expected_beats` (how many
ticks the gap was due at `worker_tick_ms`), and `worker_tick_ms` itself.
**Read the count AND the lateness - both axes decide:**
`worker_beats` about equal to `worker_expected_beats` with a LOW
`worker_max_late_ms` is a page that ran on its usual clock while frames
stopped (a compositing or visibility gate, not a busy thread); the same
count with the worst lateness APPROACHING the gap is the busy-main-thread
burst, the ticks queued and delivered on release; about one beat against
a far larger expected count is the suspended shape (a resumed page fires
its one overdue interval); zero against at least one expected means
nothing ticked. A slow frame's record is written one macrotask after the
gap, so the ticks queued during a blocked gap - which the browser
delivers after the frame callback that measured it - are on the record
rather than reading as a page that stopped. A worker that fails to load
writes one `heartbeat` record with `heartbeat: false` (its `name`
carrying the event's own error, or the event's type where a plain Event
has no error) and no beat fields from then on; a page with no Web Worker
says `heartbeat: false` on its run line, so absence never reads as zero;
a run line saying `heartbeat: true` with no beat field ever appearing is
a worker that loaded but never ticked. **Measured in headless Chromium
155:** a 2,500 ms block produced a record with a 2,999 ms gap, 3 beats,
worst lateness 2,452 ms and 2 expected - the burst on the record, under
both axes above; and in a page whose frames and own timers the engine had
throttled, the worker kept its one-second cadence. That engine runs no
rAF headless, so those captures drove the frame loop on a timer, and the
ordering claim (block, queued messages, deferred record) is scoped to
that measured engine - Chromium's task queues - not asserted for WebKit.
**The packaged app is not exercised by any of this:** the captures ran a
standalone page with the probe's logic copied, and the Tauri/WKWebView
path - where the log actually lives - has no reproducer here; running
the packaged window is what the read-only rig rule forbids for this
watch. The custom-scheme worker-load question
(tauri-apps/tauri#9975) is exactly what a `heartbeat: false` record will
answer from the live log after the next release. **Not measured:** the
tick rate in an occluded or locked WKWebView (this repo measured frames
stopping there, not a worker's timer), so a silent heartbeat there is read
against `worker_since_beat_ms` and the display state rather than assumed
to be a stopped page. A seat's open
writes its own records beside those: `switch` when its conversation
opens (the column taking the seat on - a click, a deep link, a reload, a
reconnect), `drawn` when the seat's terminal state is on screen, carrying
which state it was - `conversation` or `empty` is the seat's own answer;
`waking` (the waking line and the not-running line) or `refused` is the
column saying something else - and `quiet` when the frame loop is back to
a normal cadence. An open can carry two drawings: a placeholder
(`waking`/`refused`) does not end it, and the content's own drawing
follows with the total time since the switch; only a
`conversation`/`empty` drawing is followed by `quiet`, and never one
between a placeholder and the content. The last two records carry the
time since the switch. Slow frames are what the file is for; a quiet log
is a client that is keeping up.

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
`plugins.updater` block in `tauri.conf.json`. The home header draws an
`↑ v1.0.116 available` notice beside the `client` build pair when there is
one; clicking downloads and
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
under `v<version>` and a moving `latest`. The org's packages start private
and a workflow token cannot change that, so the first publish needs one
manual step before an unauthenticated pull works: the package's settings
(Package settings -> Change visibility) set to public.

One poller per volume: two would fight over the same symlink and staging
directory, and the one-container contract is the shape this ships in.

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
`REQUEST_INSTALL_PACKAGES`, the activity's back handling and the window
insets it hands the page, the update plugin
(`app/src/main/java/dev/vedhavyas/forge/UpdatePlugin.kt`) and the release
signing block in `app/build.gradle.kts` - survive a clean clone. The browser
host's own files sit beside them as project source too -
`app/src/main/java/dev/vedhavyas/forge/BrowserPlugin.kt` and `NodeHost.kt`,
`app/src/main/cpp/` (the JNI shim the in-app libnode needs) and
`app/src/main/assets/forge-browser/` (the driver's bootstrap); only
`jniLibs/*.so` and `cpp/nodejs-mobile/` are vendored rather than committed,
put there by `just vendor-browser-stack-android`. Re-running
`tauri android init` overwrites the generated files, so re-apply the edits
after one. The debug
APK is one command, and it builds the frontend first the same way the
desktop build does:

```sh
npm run tauri -- android build --debug --apk --ci --target aarch64
```

It lands at
`src-tauri/gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk`,
signed with the debug keystore under `~/.android/`, and side-loads onto a
phone. **`--target aarch64` is required, not a preference**: the vendored
libnode is arm64-only, so the other ABIs have nothing to link against. The
build fetches the SDK's CMake for the JNI shim on first run; the SDK, a JDK
17+, and `rustup target add aarch64-linux-android` are the prerequisites.
On a host that is not macOS, `just vendor-browser-stack`'s desktop half
refuses (its node tarball is pinned for Apple Silicon) - run
`just vendor-browser-stack-android` alone and build with the CLI directly.

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

- `src/wire/` - the server's shapes, typed against the fixtures under
  `src/dev/` and pinned to the server's own copies by `salvage.test.ts`.
  `src/wire/README.md` carries the provenance and every re-sync since. The
  pages type their props against these, so the two halves cannot disagree
  about what a home looks like.
- `src/components/` - the pieces every page draws with: the row, the band
  card, the lifecycle mark, the brand mark, the icon sprite. One theme, one
  copy of each.

**The sprite is inlined by the shell on every page, and every icon a page
draws is a `<use>` reference to it.** `Sprite.svelte` inlines it once per
page and rows and pages draw through `Icon.svelte`, which is what lets a
page's own rule colour an icon by the row it sits in. The set is Lucide's
(MIT) with one exception: the Slack brand mark ships beside it, because a
logo is the one glyph Lucide deliberately does not carry. `Sprite.test.ts`
holds the sprite to what the session and composer drawings ask for, so an
icon a page draws and the sprite lacks fails the suite rather than drawing
as a gap.

- `src/theme.ts` - the palettes and typeface stacks the names resolve to.
  The greeting carries the NAMES; the values live here, and there is no
  client-side reader of `forge.toml` by any path.
- `src/assets/` - `web.css` and `sprite.svg`, salvaged from the retired
  web-view crate before its deletion. The fonts live in `public/fonts/`.

`forge.toml` is the server's, and the client never reads it: `[client] mark`,
`[client] theme` and `[client] font` arrive in the greeting. `ClientSettings`
carries a name for each, not a value.

## Every cell the server's home draws

The four reads this file used to list as missing now cross on a project
row, and the home draws all four: `work` fills `.row .where`, `tasks`
fills `.row .what` with its status chip and its artifact link, `would_bind`
decides the refusal's second arm, and `forge_version_short` carries the
header's version. The fixture's `projects[]` entries are
`{project, work, tasks, crons, would_bind, chip}`; the retired web-view
crate's `home.rs` was the port's source for the markup.

**The unseen marks cross too**, as `unseen` - a list of slots. The server's
`Live` owns that fact and a client cannot reconstruct it from the records,
so it is carried rather than derived, and the client draws the state when
it is handed one.

## The status the snapshot carries, and the frames that draw it

`service_status` and `fatal_error` cross on the home subject. Both now draw
live: the service report and the core's fatal are keyless frames that ride
every connection, so each open conversation draws them as they arrive
rather than reading them back off the snapshot. The home's own
`fatal_error` row draws above the header - the read a view attaching late
finds, which for a fatal is the wind-down window before the process goes.
`service_status`'s field itself stays undrawn: the report is live-only by
design, and a page opened later has nothing to replay it from. A forge that
failed at startup draws nothing anywhere - no client ever reached its
socket, and the connect screen is all there is.

Two more ride the home row and this page does not draw them: `crons` and
`chip`. Each is drawn elsewhere: the schedules row on the session page's
strip keeps the seat's own set (a cron names the seat that created it),
and the account a row chips shows on the session page's rail footer,
beside what it is costing. Neither is a gap in this page.

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
