# The share sheet

The page a share opens: the files another app handed forge, the seat they go to, and the words
that ride with them. It is a page of its own rather than an overlay, so the hardware Back closes
it through the same history every page uses. The drawing it is held against is
[web-share.html](./web-share.html), beside this page.

**Android is the only intake in this cut.** The share sheet lists forge for `ACTION_SEND` and
`ACTION_SEND_MULTIPLE`; the desktop app and the web client draw no door to this page and are
unchanged. Desktop drag-and-drop and the web file picker are the next cut over the same server
mechanism.

## What it draws

| Part | Shows | Read from |
|---|---|---|
| File list | each shared file's display name and size, and the share's total against the 200 MB cap | the shell's share intake |
| Seat | one seat from the roster, the reader's last seat preselected, the roster's first when there is no last one | the home's `agents` |
| Box | the words that go with the files, with the composer's own dictation control | the reader, and the seat's `composer` notice |
| Progress | one line naming the file and its percentage, a bar, and whether the upload is resuming or starting over | the upload's own channel |
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
them** - nothing sweeps the inbox - and a partial carries a `.partial` suffix until the upload
ends, so a half-written file never appears as a shared one.

## The intake

The Kotlin half reads the shared `content://` URIs through the ContentResolver and copies each
into the app's cache under its display name, because a provider's grant is scoped to the
activity and would not survive a later upload. The page never sees a URI: it is handed absolute
cache paths, and the shell reads those bytes.

**A cold start's share is the activity's launch intent**, which the page asks for once; a share
into a running app arrives as an event and opens the sheet over whatever the reader was on. Both
answer the same payload, and a share that carried no files - or one that could not be read - is
said on the sheet rather than drawn as an empty form.

## The box

The box is the composer's shared field with the composer's own dictation control: one take, one
landing, the same `DictateControl`, `TakeCard` and `Landing` every surface that dictates uses. A
take is addressed to the picked seat, so the picker is disabled while a take runs; a landed
take's words go to the end of the box, once.

## Touch

Every control is a 44px target under `@media (pointer: coarse)`, the seat picker is a native
select so a finger gets the platform's own chooser, and no affordance exists only on hover. The
page is pushed onto history, so the hardware Back closes it.
