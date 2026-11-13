//! The share sheet: files another app shared with forge, and the upload that
//! puts them in a seat's inbox.
//!
//! **Android's share sheet is the only intake in this cut.** The Kotlin half
//! (`SharePlugin`) copies each shared content URI into the app's cache, where
//! the grant cannot expire under a later upload, and answers the page with
//! absolute paths. The upload is the shell's: on the phone the shell holds the
//! socket, so the page never touches file bytes - it names the seat, the text
//! and the cached paths, and this reads each file in 64 KiB frames.
//!
//! **A dropped upload resumes.** The server keeps a partial upload for an hour
//! and answers `file_upload_resume` with the offsets it already holds; a
//! retry continues from them, and one the server no longer holds restarts
//! from zero. The page is told which happened, because a sheet that silently
//! restarted a 200 MB share is a sheet that lied about what it was doing.

#[cfg(any(target_os = "android", test))]
use std::io::SeekFrom;
use std::sync::Arc;

use serde_json::Value;
#[cfg(any(target_os = "android", test))]
use serde_json::json;
#[cfg(target_os = "android")]
use tauri::Manager as _;
#[cfg(any(target_os = "android", test))]
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::bridge::Bridge;
#[cfg(any(target_os = "android", test))]
use crate::socket::Socket;

/// **The driver below is the Android half's.** A host build carries the
/// command so the invoke surface is the same everywhere, and refuses it by
/// name; only the Android build and the tests construct any of this.
///
/// The binary kind a file chunk crosses as (`Kind::FileChunk` on the server).
#[cfg(any(target_os = "android", test))]
const FILE_CHUNK_TAG: u8 = 6;

/// One frame's payload cap, a bound of its own rather than the dictation
/// frame's 16 KiB: an upload is bulk bytes, and 64 KiB keeps the per-frame
/// overhead small without a large allocation per message.
#[cfg(any(target_os = "android", test))]
const CHUNK: usize = 64 * 1024;

/// The most one share may carry, enforced here and on the server.
#[cfg(any(target_os = "android", test))]
const MAX_TOTAL: u64 = 200 * 1024 * 1024;

/// One file the share handed the app: the cache path, its display name, the
/// bytes the copy holds, and the provider's type when it published one.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct SharedFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    #[serde(default)]
    pub mime: Option<String>,
}

/// What the Kotlin half answers: the copied files, why there are none, or -
/// when a share carried no stream at all - that it was a share of nothing.
///
/// **`empty` is its own fact, not an error**: a SEND with no stream draws the
/// sheet's neutral line, while `error` is a share that could not be read.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Shared {
    #[serde(default)]
    pub files: Vec<SharedFile>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub empty: bool,
}

/// One upload's progress, as the sheet draws it.
///
/// `begin` comes first: the upload's id, which a retry needs to resume it,
/// **whether this send had a previous upload to resume at all**
/// (`attempted`), whether the server still held it (`resumed`), and the
/// offsets it already has. A fresh send is `attempted: false`, which is what
/// tells the sheet not to draw "starting over" over its first upload.
/// `progress` follows once per frame with the bytes of one file that have
/// crossed.
/// **The command's own type on every target**, though only the Android build
/// constructs a variant: a host build answers the command with a refusal
/// before any progress exists.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ShareProgress {
    Begin { upload_id: u64, attempted: bool, resumed: bool, received: Vec<u64> },
    Progress { file: usize, sent: u64, total: u64 },
}

/// The files a share launched the app with, taken once per share.
///
/// A cold start's share is the activity's launch intent, which outlives every
/// page load; the Kotlin half answers it once and then answers nothing, so a
/// reload does not open the sheet again.
#[tauri::command]
pub async fn client_share_take(app: tauri::AppHandle) -> Result<Shared, String> {
    #[cfg(target_os = "android")]
    {
        let reader = app
            .try_state::<Arc<Reader>>()
            .ok_or_else(|| "the share plugin is not there".to_owned())?;
        return reader.take().await;
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(Shared::default())
    }
}

/// Tell the Kotlin half the page's own share listener is registered.
///
/// **A warm share is only pushed once the page says this.** The shell's
/// channel is registered at plugin setup, long before the page's `listen`,
/// and a trigger into a webview with no listener is dropped silently - so the
/// Kotlin half buffers until this arrives rather than trusting the channel.
#[tauri::command]
pub async fn client_share_ready(app: tauri::AppHandle) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let reader = app
            .try_state::<Arc<Reader>>()
            .ok_or_else(|| "the share plugin is not there".to_owned())?;
        return reader.ready().await;
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = app;
        Ok(())
    }
}

/// Acknowledge the share the page just received, so its delivery is proven.
///
/// **A push is not proof of receipt**: Tauri drops an emit into a webview with
/// no listener, so the Kotlin half keeps the share buffered until the page's
/// own listener says it arrived. A dropped push then reaches the page through
/// its next ask instead of being marked delivered and lost.
#[tauri::command]
pub async fn client_share_ack(app: tauri::AppHandle, seq: u64) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let reader = app
            .try_state::<Arc<Reader>>()
            .ok_or_else(|| "the share plugin is not there".to_owned())?;
        return reader.ack(seq).await;
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (app, seq);
        Ok(())
    }
}

/// Upload one share to a seat's inbox, answering the absolute paths the
/// server wrote.
///
/// `upload_id` is the previous attempt's, when there was one: the shell asks
/// the server whether it still holds that upload and continues from its
/// offsets, or begins a new one. The answer's `paths` are what the prompt
/// names, and they are only answered once the server has linked every
/// partial into place.
#[tauri::command]
pub async fn client_share_upload(
    app: tauri::AppHandle,
    bridge: tauri::State<'_, Arc<Bridge>>,
    channel: tauri::ipc::Channel<ShareProgress>,
    seat: Value,
    files: Vec<SharedFile>,
    upload_id: Option<u64>,
) -> Result<Value, String> {
    #[cfg(not(target_os = "android"))]
    {
        let _ = (&app, &bridge, &channel, &seat, &files, &upload_id);
        Err("sharing a file needs the Android app".to_owned())
    }
    #[cfg(target_os = "android")]
    {
        // **A page names cached copies, not any file on the machine.** Every
        // path resolves under the share cache root, so a page that went
        // wrong cannot upload the person's home directory.
        let root = share_cache_root(&app)?;
        let mut checked = Vec::with_capacity(files.len());
        for file in files {
            let path = within(&root, std::path::Path::new(&file.path))?;
            checked.push(SharedFile { path: path.display().to_string(), ..file });
        }
        let bridge = Arc::clone(bridge.inner());
        let socket = bridge.socket().ok_or_else(|| "the socket is not open".to_owned())?;
        let mut progress = |update: ShareProgress| {
            let _ = channel.send(update);
        };
        upload(&socket, &mut progress, seat, checked, upload_id).await
    }
}

/// The cache root the share intake copies into: `<app cache>/share`.
#[cfg(target_os = "android")]
fn share_cache_root(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    app.path()
        .app_cache_dir()
        .map(|base| base.join("share"))
        .map_err(|why| format!("the app's cache directory is not there: {why}"))
}

/// `path` resolved inside `root`, or a refusal naming it.
///
/// **Both sides are canonicalised**: a `..` walk or a symlink pointing out
/// resolves outside the root and is refused, however the path was spelled.
/// Only the Android command calls it in a build; a host test still pins it.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn within(root: &std::path::Path, path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let root = root
        .canonicalize()
        .map_err(|why| format!("the share cache at {} is not there: {why}", root.display()))?;
    let path = path
        .canonicalize()
        .map_err(|why| format!("{} could not be resolved: {why}", path.display()))?;
    if path.starts_with(&root) {
        Ok(path)
    } else {
        Err(format!("{} is outside the share cache", path.display()))
    }
}

/// Upload, reporting progress through `progress`: the command's channel, or a
/// test's own collector.
#[cfg(any(target_os = "android", test))]
async fn upload<F>(
    socket: &Socket,
    progress: &mut F,
    seat: Value,
    files: Vec<SharedFile>,
    upload_id: Option<u64>,
) -> Result<Value, String>
where
    F: FnMut(ShareProgress) + Send,
{
    if files.is_empty() {
        return Err("there is nothing to upload".to_owned());
    }
    // **A declared size that cannot be added is refused, not wrapped**: in a
    // release build two 2^63 declarations sum to 0 and the 200 MiB wall with
    // it, so a size wider than the share is refused on its own first.
    let mut total: u64 = 0;
    for file in &files {
        if file.size > MAX_TOTAL {
            return Err(format!(
                "{} is {} bytes; one share carries at most {MAX_TOTAL}",
                file.name, file.size,
            ));
        }
        total = total
            .checked_add(file.size)
            .ok_or_else(|| format!("the share's sizes cannot be added up at {}", file.name))?;
    }
    if total > MAX_TOTAL {
        return Err(format!(
            "the share is {} MB; one share carries at most {} MB",
            total / (1024 * 1024),
            MAX_TOTAL / (1024 * 1024),
        ));
    }
    // **A server that does not hold the upload is not a failure**: a restart
    // or the one-hour window took it, and the share starts over.
    let attempted = upload_id.is_some();
    let mut held = None;
    if let Some(id) = upload_id
        && let Ok(offsets) = resume(socket, &seat, id).await
    {
        held = Some((id, offsets));
    }
    let (id, received, resumed) = if let Some((id, offsets)) = held {
        (id, offsets, true)
    } else {
        (begin(socket, &seat, &files).await?, vec![0; files.len()], false)
    };
    progress(ShareProgress::Begin {
        upload_id: id,
        attempted,
        resumed,
        received: received.clone(),
    });
    for (index, file) in files.iter().enumerate() {
        let start = received.get(index).copied().unwrap_or(0);
        send_file(socket, progress, id, index, file, start).await?;
    }
    let paths = end(socket, &seat, id).await?;
    // **One path per file.** An end that answers a different number names
    // files the prompt would not carry, or misses ones it would; the prompt is
    // the whole delivery, so the count is checked before it is built.
    if paths.len() != files.len() {
        let files_word = if files.len() == 1 { "file" } else { "files" };
        return Err(format!(
            "the server answered {} paths for {} {files_word}",
            paths.len(),
            files.len(),
        ));
    }
    Ok(json!({ "paths": paths }))
}

/// Begin an upload, answering the id it was given.
#[cfg(any(target_os = "android", test))]
async fn begin(socket: &Socket, seat: &Value, files: &[SharedFile]) -> Result<u64, String> {
    let declared: Vec<Value> = files
        .iter()
        .map(|file| json!({ "name": file.name, "size": file.size, "mime": file.mime }))
        .collect();
    let body = answer(
        socket
            .dispatch(json!({ "file_upload_begin": { "key": seat, "files": declared } }), true)
            .await?,
    )?;
    body.get("upload_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| "the server's answer named no upload".to_owned())
}

/// The offsets a held upload already has, or the server's refusal.
#[cfg(any(target_os = "android", test))]
async fn resume(socket: &Socket, seat: &Value, id: u64) -> Result<Vec<u64>, String> {
    let body = answer(
        socket
            .dispatch(json!({ "file_upload_resume": { "key": seat, "upload_id": id } }), true)
            .await?,
    )?;
    Ok(body
        .get("received")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(|row| row.as_u64().unwrap_or(0)).collect())
        .unwrap_or_default())
}

/// Close an upload, answering the paths it wrote.
#[cfg(any(target_os = "android", test))]
async fn end(socket: &Socket, seat: &Value, id: u64) -> Result<Vec<String>, String> {
    let body = answer(
        socket
            .dispatch(json!({ "file_upload_end": { "key": seat, "upload_id": id } }), true)
            .await?,
    )?;
    strings(body.get("paths"))
}

/// One file's bytes, from `start` to its end, one frame at a time.
///
/// **Each frame waits for its write.** The socket's queue is unbounded, so a
/// driver that only posted frames would hold a whole share in memory on a
/// phone; waiting on the write paces the upload to the link.
#[cfg(any(target_os = "android", test))]
async fn send_file<F>(
    socket: &Socket,
    progress: &mut F,
    id: u64,
    index: usize,
    file: &SharedFile,
    start: u64,
) -> Result<(), String>
where
    F: FnMut(ShareProgress) + Send,
{
    let index = u16::try_from(index).map_err(|_| "the share carries too many files".to_owned())?;
    let mut handle = tokio::fs::File::open(&file.path)
        .await
        .map_err(|why| format!("{} could not be opened: {why}", file.name))?;
    let len = handle
        .metadata()
        .await
        .map_err(|why| format!("{} could not be read: {why}", file.name))?
        .len();
    // **The copy is the size the upload declared.** A file that moved under
    // the share is refused rather than sent past the bytes the server will
    // accept, which would fail at the end with a whole upload wasted.
    if len != file.size {
        return Err(format!("{} changed since it was shared", file.name));
    }
    if start > len {
        return Err(format!("{} is shorter than the server's offset", file.name));
    }
    handle
        .seek(SeekFrom::Start(start))
        .await
        .map_err(|why| format!("{} could not be read: {why}", file.name))?;
    let mut offset = start;
    let mut buffer = vec![0u8; CHUNK];
    while offset < len {
        let read = handle
            .read(&mut buffer)
            .await
            .map_err(|why| format!("{} could not be read: {why}", file.name))?;
        if read == 0 {
            break;
        }
        let at = u32::try_from(offset)
            .map_err(|_| format!("{} is past the upload's offset range", file.name))?;
        socket.send_binary_ack(chunk_frame(id, index, at, &buffer[..read])).await?;
        offset += read as u64;
        progress(ShareProgress::Progress { file: usize::from(index), sent: offset, total: len });
    }
    Ok(())
}

/// One file chunk's bytes: the kind tag, the upload, the file, the offset,
/// then the payload.
///
/// **The server writes at this offset and refuses anything else**, so the
/// header is the whole correlation a chunk carries: a frame with a wrong
/// offset is a file with a hole, and one with a wrong upload is another
/// person's bytes.
#[cfg(any(target_os = "android", test))]
fn chunk_frame(id: u64, file: u16, offset: u32, bytes: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(15 + bytes.len());
    frame.push(FILE_CHUNK_TAG);
    frame.extend_from_slice(&id.to_le_bytes());
    frame.extend_from_slice(&file.to_le_bytes());
    frame.extend_from_slice(&offset.to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}

/// A reply body as an answer: an object is the answer, a string is the
/// server's own refusal.
#[cfg(any(target_os = "android", test))]
fn answer(value: Option<Value>) -> Result<Value, String> {
    match value {
        Some(Value::String(why)) => Err(why),
        Some(value @ Value::Object(_)) => Ok(value),
        Some(other) => Err(format!("the server answered something unexpected: {other}")),
        None => Err("the server answered nothing".to_owned()),
    }
}

/// A JSON array of strings, or the refusal that names what was answered
/// instead.
///
/// **A non-string is not dropped**: a path the server answered and this side
/// silently discarded is a file the prompt would not name, which reads as a
/// share that lost one. **An answer with no list at all is not an empty one**:
/// an end that named no paths did not land the share, so treating it as a
/// successful zero-path upload is the same loss in a quieter form.
#[cfg(any(target_os = "android", test))]
fn strings(value: Option<&Value>) -> Result<Vec<String>, String> {
    let rows = value
        .and_then(Value::as_array)
        .ok_or_else(|| "the server's answer named no paths".to_owned())?;
    let mut paths = Vec::with_capacity(rows.len());
    for row in rows {
        match row.as_str() {
            Some(path) => paths.push(path.to_owned()),
            None => return Err(format!("the server's paths carried a non-string: {row}")),
        }
    }
    Ok(paths)
}

/// The Kotlin handle: the share the activity was launched with.
#[cfg(target_os = "android")]
pub struct Reader(tauri::plugin::PluginHandle<tauri::Wry>);

#[cfg(target_os = "android")]
impl Reader {
    async fn take(&self) -> Result<Shared, String> {
        self.0.run_mobile_plugin_async("take", ()).await.map_err(|why| why.to_string())
    }

    async fn ready(&self) -> Result<(), String> {
        self.0.run_mobile_plugin_async::<()>("ready", ()).await.map_err(|why| why.to_string())
    }

    async fn ack(&self, seq: u64) -> Result<(), String> {
        self.0
            .run_mobile_plugin_async::<()>("ack", AckShare { seq })
            .await
            .map_err(|why| why.to_string())
    }
}

/// The delivery one acknowledgement names, field for field with `AckArgs` in
/// the Kotlin plugin.
#[cfg(target_os = "android")]
#[derive(serde::Serialize)]
struct AckShare {
    seq: u64,
}

/// The listener the Kotlin half reports a share into a running app through.
#[cfg(target_os = "android")]
#[derive(serde::Serialize)]
struct RegisterShare {
    event: &'static str,
    handler: tauri::ipc::Channel,
}

/// Register the Kotlin plugin that reads shared files, and the listener its
/// warm shares arrive on.
#[cfg(target_os = "android")]
pub fn init() -> tauri::plugin::TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new(crate::ANDROID_SHARE_PLUGIN)
        .setup(|app, api| {
            let handle = api.register_android_plugin("dev.vedhavyas.forge", "SharePlugin")?;
            let shown = app.clone();
            let channel = tauri::ipc::Channel::new(move |body| {
                if let tauri::ipc::InvokeResponseBody::Json(payload) = body
                    && let Ok(shared) = serde_json::from_str::<Shared>(&payload)
                {
                    push(&shown, &shared);
                }
                Ok(())
            });
            app.manage(Arc::new(Reader(handle.clone())));
            if let Err(why) = handle.run_mobile_plugin::<()>(
                "registerListener",
                RegisterShare { event: "share", handler: channel },
            ) {
                tauri_plugin_log::log::warn!("the share listener was not registered: {why}");
            }
            Ok(())
        })
        .build()
}

/// Hand a share into a running app to the page, which opens the sheet.
#[cfg(target_os = "android")]
fn push(app: &tauri::AppHandle, shared: &Shared) {
    use tauri::Emitter as _;
    // **A share the page never hears is a share lost to a ship it already
    // made**: the emit failing is said rather than discarded, because the
    // only other trace is the sheet that never opened.
    if let Err(why) = app.emit("client://share", shared) {
        tauri_plugin_log::log::warn!("a warm share was not delivered to the page: {why}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A chunk's header is the whole correlation it carries.** The server
    /// writes at this offset or refuses the frame, so a byte-order or field
    /// swap here is a file with a hole rather than an error.
    #[test]
    fn a_file_chunk_names_its_upload_file_and_offset() {
        let frame = chunk_frame(0x0102_0304_0506_0708, 3, 0x0a0b_0c0d, b"hi");
        assert_eq!(
            frame,
            [
                FILE_CHUNK_TAG,
                8,
                7,
                6,
                5,
                4,
                3,
                2,
                1, // the upload, little-endian
                3,
                0, // the file
                13,
                12,
                11,
                10, // the offset
                b'h',
                b'i',
            ],
            "the tag, upload, file, offset, then the payload",
        );
        assert_eq!(FILE_CHUNK_TAG, 6, "the kind the server decodes");
    }

    /// A reply body's two shapes: an object is the answer, a string is the
    /// core's own refusal, and neither is inferred from a field.
    #[test]
    fn a_reply_is_an_object_or_the_servers_refusal() {
        assert!(answer(Some(json!({ "paths": [] }))).is_ok());
        assert_eq!(
            answer(Some(json!("that upload is unknown or expired"))),
            Err("that upload is unknown or expired".to_owned()),
        );
        assert!(answer(None).is_err(), "no answer is not success");
    }

    /// **A paths list is strings, or it is refused**: a member this side
    /// dropped would be a file the prompt never named.
    #[test]
    fn a_paths_list_keeps_its_strings_and_refuses_what_is_not_one() {
        assert_eq!(
            strings(Some(&json!(["a", "b"]))).expect("strings"),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(strings(Some(&json!(["a", 1]))).is_err(), "a non-string is refused");
        assert!(strings(None).is_err(), "an answer with no paths is refused");
    }

    use crate::socket::tests::{Stub, connected, stub};

    /// The next command of one kind the shell put on the wire.
    async fn next_command(stub: &mut Stub, kind: &str) -> Value {
        loop {
            let command = stub.heard_until(|value| value["kind"] == "command").await;
            let variant = command["command"]
                .as_object()
                .and_then(|fields| fields.keys().next().cloned())
                .unwrap_or_default();
            if variant == kind {
                return command;
            }
        }
    }

    /// Answer one command's reply by its own id.
    fn reply(stub: &Stub, command: &Value, body: &Value) {
        let id = command["reply_to"].as_u64().expect("the shell minted a reply id");
        stub.say.send(json!({ "kind": "reply", "reply_to": id, "body": body })).expect("broadcast");
    }

    /// One file chunk's fields, as the wire carries them.
    fn chunk_fields(frame: &[u8]) -> (u64, u16, u32, Vec<u8>) {
        assert_eq!(frame[0], FILE_CHUNK_TAG, "the chunk's kind");
        let id = u64::from_le_bytes(frame[1..9].try_into().expect("eight bytes"));
        let file = u16::from_le_bytes(frame[9..11].try_into().expect("two bytes"));
        let offset = u32::from_le_bytes(frame[11..15].try_into().expect("four bytes"));
        (id, file, offset, frame[15..].to_vec())
    }

    fn seat() -> Value {
        json!({ "org": "TestOrg", "project": "proj", "label": "lead" })
    }

    fn share_of(dir: &std::path::Path, name: &str, bytes: &[u8]) -> SharedFile {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("this test's own file");
        SharedFile {
            path: path.display().to_string(),
            name: name.to_owned(),
            size: bytes.len() as u64,
            mime: None,
        }
    }

    /// **The driver's whole lifecycle over a real socket**: begin, one chunk
    /// of the file at the offset the server expects, end, and the paths it
    /// answers. The chunk's bytes and its header are both asserted, because a
    /// wrong offset is a file with a hole rather than an error.
    #[tokio::test]
    async fn an_upload_begins_chunks_and_ends() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = share_of(dir.path(), "photo.jpg", b"hello world");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                let answer =
                    upload(&socket, &mut |update| progress.push(update), seat(), files, None).await;
                (answer, progress)
            }
        });

        let begin = next_command(&mut stub, "file_upload_begin").await;
        assert_eq!(
            begin["command"]["file_upload_begin"]["files"][0]["name"], "photo.jpg",
            "the file's own name declares it",
        );
        reply(&stub, &begin, &json!({ "upload_id": 7, "paths": ["/inbox/photo.jpg"] }));

        let frame = stub.binary.recv().await.expect("the file's chunk");
        let (id, index, offset, bytes) = chunk_fields(&frame);
        assert_eq!((id, index, offset), (7, 0, 0), "the upload, the file and the first byte");
        assert_eq!(bytes, b"hello world", "the file's own bytes");

        let end = next_command(&mut stub, "file_upload_end").await;
        assert_eq!(end["command"]["file_upload_end"]["upload_id"], 7);
        reply(&stub, &end, &json!({ "paths": ["/inbox/photo.jpg"] }));

        let (answer, progress) = calling.await.expect("the call task");
        assert_eq!(answer.expect("the upload lands"), json!({ "paths": ["/inbox/photo.jpg"] }));
        assert!(
            matches!(
                progress.first(),
                Some(ShareProgress::Begin { upload_id: 7, attempted: false, resumed: false, .. })
            ),
            "a fresh send names its upload and no resume: {progress:?}",
        );
        assert!(
            matches!(
                progress.last(),
                Some(ShareProgress::Progress { file: 0, sent: 11, total: 11 })
            ),
            "and how much of the file has crossed: {progress:?}",
        );
    }

    /// **An end that answers the wrong number of paths is refused**: the
    /// prompt names the paths, so one missing or extra is a share that lies
    /// about what it sent.
    #[tokio::test]
    async fn an_end_that_answers_the_wrong_number_of_paths_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = share_of(dir.path(), "photo.jpg", b"hi");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                upload(&socket, &mut |update| progress.push(update), seat(), files, None).await
            }
        });
        let begin = next_command(&mut stub, "file_upload_begin").await;
        reply(&stub, &begin, &json!({ "upload_id": 7, "paths": ["/inbox/photo.jpg"] }));
        let _frame = stub.binary.recv().await.expect("the file's chunk");
        let end = next_command(&mut stub, "file_upload_end").await;
        reply(&stub, &end, &json!({ "paths": ["/inbox/a.jpg", "/inbox/b.jpg"] }));
        let why = calling.await.expect("the call task").expect_err("two paths for one file");
        assert!(why.contains("2 paths for 1 file"), "{why}");
    }

    /// **A retry continues from the server's offset.** The held upload says it
    /// has three bytes; the driver's first chunk starts at 3 and carries the
    /// rest, never the bytes the server already has.
    #[tokio::test]
    async fn a_retry_resumes_from_the_servers_offset() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = share_of(dir.path(), "a.bin", b"hello world");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                let answer =
                    upload(&socket, &mut |update| progress.push(update), seat(), files, Some(7))
                        .await;
                (answer, progress)
            }
        });

        let resume = next_command(&mut stub, "file_upload_resume").await;
        assert_eq!(resume["command"]["file_upload_resume"]["upload_id"], 7);
        reply(&stub, &resume, &json!({ "paths": ["/inbox/a.bin"], "received": [3] }));

        let frame = stub.binary.recv().await.expect("the rest of the file");
        let (id, index, offset, bytes) = chunk_fields(&frame);
        assert_eq!((id, index, offset), (7, 0, 3), "the bytes the server already has are skipped");
        assert_eq!(bytes, b"lo world");

        let end = next_command(&mut stub, "file_upload_end").await;
        reply(&stub, &end, &json!({ "paths": ["/inbox/a.bin"] }));
        let (answer, progress) = calling.await.expect("the call task");
        assert!(answer.is_ok());
        assert!(
            matches!(
                progress.first(),
                Some(ShareProgress::Begin { attempted: true, resumed: true, received, .. })
                    if received == &vec![3]
            ),
            "the sheet is told it is resuming: {progress:?}",
        );
    }

    /// **A refused begin is the server's own words**, and no prompt can follow
    /// it: the driver answers the error and sends nothing.
    #[tokio::test]
    async fn a_refused_begin_answers_the_servers_words() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = share_of(dir.path(), "big.bin", b"x");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                upload(&socket, &mut |update| progress.push(update), seat(), files, None).await
            }
        });
        let begin = next_command(&mut stub, "file_upload_begin").await;
        reply(
            &stub,
            &begin,
            &json!("the share is 300000000 bytes; one share carries at most 209715200"),
        );
        let why = calling.await.expect("the call task").expect_err("the begin was refused");
        assert!(why.contains("at most"), "{why}");
    }

    /// **The Kotlin half's payload parses as itself**: an absent `mime` is
    /// none, and a share with no files still carries its reason.
    #[test]
    fn the_share_payload_parses() {
        let shared: Shared = serde_json::from_value(json!({
            "files": [{ "path": "/cache/share/1/photo.jpg", "name": "photo.jpg", "size": 5 }],
            "error": null,
        }))
        .expect("the plugin's own payload");
        assert_eq!(shared.files.len(), 1);
        assert_eq!(shared.files[0].size, 5);
        assert_eq!(shared.files[0].mime, None);
        assert_eq!(shared.error, None);

        let refused: Shared =
            serde_json::from_value(json!({ "files": [], "error": "the provider refused it" }))
                .expect("a refused share");
        assert!(refused.files.is_empty());
        assert_eq!(refused.error.as_deref(), Some("the provider refused it"));
        assert!(!refused.empty, "a refusal is not a share of nothing");

        let nothing: Shared =
            serde_json::from_value(json!({ "files": [], "error": null, "empty": true }))
                .expect("a share of nothing");
        assert!(nothing.files.is_empty());
        assert_eq!(nothing.error, None);
        assert!(nothing.empty, "a SEND with no stream is its own fact");
    }

    /// **Two files cross in one upload**, each at its own index: a driver
    /// that named every chunk file 0 would land the second file's bytes in
    /// the first.
    #[tokio::test]
    async fn an_upload_with_two_files_chunks_both_at_their_own_offsets() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let first = share_of(dir.path(), "a.bin", b"abc");
        let second = share_of(dir.path(), "b.bin", b"def");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![first, second];
            async move {
                let mut progress = Vec::new();
                upload(&socket, &mut |update| progress.push(update), seat(), files, None).await
            }
        });

        let begin = next_command(&mut stub, "file_upload_begin").await;
        assert_eq!(
            begin["command"]["file_upload_begin"]["files"].as_array().map(Vec::len),
            Some(2),
            "both files are declared",
        );
        reply(&stub, &begin, &json!({ "upload_id": 7, "paths": ["/inbox/a.bin", "/inbox/b.bin"] }));

        let one = stub.binary.recv().await.expect("the first file's chunk");
        let two = stub.binary.recv().await.expect("the second file's chunk");
        let (id, index, offset, bytes) = chunk_fields(&one);
        assert_eq!((id, index, offset), (7, 0, 0), "the first file at its own index");
        assert_eq!(bytes, b"abc");
        let (id, index, offset, bytes) = chunk_fields(&two);
        assert_eq!((id, index, offset), (7, 1, 0), "the second file at index one");
        assert_eq!(bytes, b"def");

        let end = next_command(&mut stub, "file_upload_end").await;
        reply(&stub, &end, &json!({ "paths": ["/inbox/a.bin", "/inbox/b.bin"] }));
        let answer = calling.await.expect("the call task").expect("the upload lands");
        assert_eq!(answer, json!({ "paths": ["/inbox/a.bin", "/inbox/b.bin"] }));
    }

    /// **A refused resume starts over at zero**, and the sheet is told the
    /// send had an attempt that the server no longer held: `attempted: true`
    /// with `resumed: false`.
    #[tokio::test]
    async fn a_refused_resume_falls_back_to_a_begin() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let file = share_of(dir.path(), "a.bin", b"abcdef");
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                let answer =
                    upload(&socket, &mut |update| progress.push(update), seat(), files, Some(9))
                        .await;
                (answer, progress)
            }
        });

        let resume = next_command(&mut stub, "file_upload_resume").await;
        assert_eq!(resume["command"]["file_upload_resume"]["upload_id"], 9);
        reply(&stub, &resume, &json!("that upload is unknown or expired; start it again"));

        let begin = next_command(&mut stub, "file_upload_begin").await;
        reply(&stub, &begin, &json!({ "upload_id": 11, "paths": ["/inbox/a.bin"] }));

        let frame = stub.binary.recv().await.expect("the whole file, from zero");
        let (id, index, offset, bytes) = chunk_fields(&frame);
        assert_eq!((id, index, offset), (11, 0, 0), "the new upload starts at the first byte");
        assert_eq!(bytes, b"abcdef", "every byte is sent again");

        let end = next_command(&mut stub, "file_upload_end").await;
        reply(&stub, &end, &json!({ "paths": ["/inbox/a.bin"] }));
        let (answer, progress) = calling.await.expect("the call task");
        assert!(answer.is_ok(), "{answer:?}");
        assert!(
            matches!(
                progress.first(),
                Some(ShareProgress::Begin { attempted: true, resumed: false, .. })
            ),
            "the sheet is told it tried and started over: {progress:?}",
        );
    }

    /// **A file that moved under the share is refused** rather than sent past
    /// the bytes the server accepted, which would fail the end with the whole
    /// upload wasted.
    #[tokio::test]
    async fn a_file_that_changed_since_the_share_is_refused() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let mut file = share_of(dir.path(), "a.bin", b"abcdef");
        // The share says four bytes; the copy on disk holds six.
        file.size = 4;
        let mut stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let calling = tokio::spawn({
            let socket = socket.clone();
            let files = vec![file];
            async move {
                let mut progress = Vec::new();
                upload(&socket, &mut |update| progress.push(update), seat(), files, None).await
            }
        });
        let begin = next_command(&mut stub, "file_upload_begin").await;
        reply(&stub, &begin, &json!({ "upload_id": 7, "paths": ["/inbox/a.bin"] }));
        let why = calling.await.expect("the call task").expect_err("the file changed");
        assert!(why.contains("changed since it was shared"), "{why}");
    }

    /// A share past the byte cap is refused before a single frame goes.
    #[tokio::test]
    async fn an_over_cap_share_is_refused_before_it_is_sent() {
        let file = SharedFile {
            path: "/nowhere".to_owned(),
            name: "big.bin".to_owned(),
            size: MAX_TOTAL + 1,
            mime: None,
        };
        let stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let mut progress = Vec::new();
        let why = upload(&socket, &mut |update| progress.push(update), seat(), vec![file], None)
            .await
            .expect_err("over the cap");
        assert!(why.contains("at most"), "{why}");
        assert!(progress.is_empty(), "nothing was reported to the sheet");
    }

    /// **Two declarations that cannot be added up are refused, not wrapped**:
    /// in a release build two 2^63 sizes sum to 0 and the byte wall with them.
    #[tokio::test]
    async fn two_declarations_that_cannot_be_added_are_refused() {
        let stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let mut progress = Vec::new();
        let files = vec![
            SharedFile { path: "/a".to_owned(), name: "a".to_owned(), size: 1 << 62, mime: None },
            SharedFile { path: "/b".to_owned(), name: "b".to_owned(), size: 1 << 62, mime: None },
        ];
        let why = upload(&socket, &mut |update| progress.push(update), seat(), files, None)
            .await
            .expect_err("2^62");
        assert!(why.contains("at most"), "{why}");
    }

    /// An empty share is refused in its own words, before any command.
    #[tokio::test]
    async fn a_share_with_no_files_is_refused() {
        let stub = stub().await;
        let (socket, _events) = connected(&stub).await;
        let mut progress = Vec::new();
        let why = upload(&socket, &mut |update| progress.push(update), seat(), Vec::new(), None)
            .await
            .expect_err("nothing to upload");
        assert!(why.contains("nothing to upload"), "{why}");
    }

    /// **A page names cached copies, not any file on the machine.** A path
    /// outside the share cache resolves outside the root and is refused.
    #[test]
    fn a_path_outside_the_share_cache_is_refused() {
        let root = tempfile::tempdir().expect("a temp dir");
        let inside = root.path().join("photo.jpg");
        std::fs::write(&inside, b"x").expect("this test's own file");
        assert!(within(root.path(), &inside).is_ok(), "the copy is inside the root");

        let outside = tempfile::NamedTempFile::new().expect("a file elsewhere");
        let why = within(root.path(), outside.path()).expect_err("outside the root");
        assert!(why.contains("outside the share cache"), "{why}");
    }
}
