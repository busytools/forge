# The share sheet

The page a share opens: the files another app handed forge, the seat they go to, and the words
that ride with them. It is a page of its own rather than an overlay, so the hardware Back closes
it through the same history every page uses. The drawing it is held against is
[web-share.html](./web-share.html), beside this page.

**Android is the only intake in this cut.** The share sheet lists forge for `ACTION_SEND` and
`ACTION_SEND_MULTIPLE`; the desktop app and the web client draw no door to this page. The page
itself is still reachable by URL on those clients, where with no Android share behind it the
sheet draws "this share carried no files" and cannot upload.

## What it draws

| Part | Shows | Read from |
|---|---|---|
| File list | each shared file's display name and size, and the share's total against the 200 MB cap | the shell's share intake |
| Seat | one seat from the roster, the reader's last seat preselected, the roster's first when there is no last one | the home's `agents` |
| Box | the words that go with the files, with the composer's own dictation control | the reader, and the seat's `composer` notice |
| Progress | one line naming the file and its percentage, a bar labelled by that line, and - only when the send had a previous upload - whether it is resuming or starting over | the upload's own channel |
| Ending | the reason and a retry, or the seat it was sent to and the paths the prompt names | the upload's answer, and the prompt's dispatch |

**The upload comes first and the prompt second.** The agent is messaged only after the server has
answered the paths it wrote, so a failed upload sends nothing. A retry continues from the offsets
the server already holds while it still holds the upload, and starts over when it does not - a
restart or the one-hour grace window took it - and the sheet says which of the two it is doing.

**The prompt is ordinary text**: the reader's words, a blank line, then one `file: <absolute
path>` line per file. Nothing is attached, and the chat draws it as a normal user message; the
agent reads the files with its own tools.

## Where the files land

`<app-support>/inbox/<org>/<project>/<YYYYMMDD-HHMMSS>-<name>`, where `<app-support>` is forge's
machine-local app-support directory. The seat's org and project choose the directory; the label
does not, because a project's agents share its inbox. The name is the shared file's display name
sanitised, and a collision gets a suffix. **Files are kept until the person or the agent deletes
them** - nothing sweeps the inbox - and a partial is
`.forge-upload-<upload-id>-<file-index>.partial` until the upload ends: a name only the server
makes, so half-written bytes never appear as a shared file, one share's partial cannot collide
with another's final name, and a shared file whose own name ends in `.partial` is never mistaken
for one. The final name is linked into place only where nothing holds it.

**The bounds are the server's and the app's alike**: one share carries at most 200 MB over 256
files, every file carries at least one byte, and the app checks the published sizes and then the
bytes that land against the same wall. The server also holds at most eight live uploads, 1,024
live partial files and 1 GiB of declared bytes at once, refusing a begin past any of them, and
paces begins to one a second because every landed file is permanent. A restart loses the
registry, so the sweep takes the partials it orphaned by their age on disk as well as the entries
it still holds.

## The intake

The Kotlin half reads the shared `content://` URIs through the ContentResolver and copies each
into the app's cache under its display name, because a provider's grant is scoped to the
activity and would not survive a later upload. The page never sees a URI: it is handed absolute
cache paths, and the shell reads those bytes.

**A cold start's share is the activity's launch intent**, which the page asks for once; a share
into a running app arrives as an event and opens the sheet over whatever the reader was on. Both
answer the same payload: a share that carried no file is drawn as its neutral line, one that could
not be read is drawn with the intake's reason, and neither opens on an empty form. **A share that
arrives before the page announces its listener is buffered** and answered by the page's first ask,
and the answered share is remembered for this process only: a reload does not prompt twice, while
a share that re-delivers a launch intent after a process restart prompts again rather than a
genuine re-share being lost. **A second share while the sheet is open replaces it**: the sheet is
keyed on the share, so it draws the new files' idle form rather than the last share's sent state,
and the page is already on its route rather than a second history entry.

## The states

The sheet draws the idle form with its files, seat and box; an upload in flight with the line and
the bar; a failure with the server's own reason and a retry; the sent ending with the seat and the
paths; "this share carried no files" for a share that carried none - a SEND with no stream, or the
route visited with no share behind it; the intake's reason for a share that could not be read; "no
seat is running to send to" when the roster is empty; and the refused home read when the fleet
would not answer.

**"Starting over from the first byte" is drawn only after an attempt.** A first send has no
previous upload to restart, so the sheet does not claim one.

## The box

The box is the composer's shared field with the composer's own dictation control: one take, one
landing, the same `DictateControl`, `TakeCard` and `Landing` every surface that dictates uses. A
take is addressed to the picked seat, so the picker is disabled while a take runs; a landed
take's words go to the end of the box, once.

## Touch

Every control is a 44px target under `@media (pointer: coarse)`, the seat picker is a native
select so a finger gets the platform's own chooser, and no affordance exists only on hover. The
page is pushed onto history, so the hardware Back closes it.
