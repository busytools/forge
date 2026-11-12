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

use std::io::SeekFrom;
use std::sync::Arc;

use serde_json::{Value, json};
#[cfg(target_os = "android")]
use tauri::Manager as _;
use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

use crate::bridge::Bridge;
use crate::socket::Socket;

/// The binary kind a file chunk crosses as (`Kind::FileChunk` on the server).
const FILE_CHUNK_TAG: u8 = 6;

/// One frame's payload cap, a bound of its own rather than the dictation
/// frame's 16 KiB: an upload is bulk bytes, and 64 KiB keeps the per-frame
/// overhead small without a large allocation per message.
const CHUNK: usize = 64 * 1024;

/// The most one share may carry, enforced here and on the server.
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

/// What the Kotlin half answers: the copied files, or why there are none.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Shared {
    #[serde(default)]
    pub files: Vec<SharedFile>,
    #[serde(default)]
    pub error: Option<String>,
}

/// One upload's progress, as the sheet draws it.
///
/// `begin` comes first: the upload's id, which a retry needs to resume it,
/// whether the server still held the previous attempt, and the offsets it
/// already has. `progress` follows once per frame with the bytes of one file
/// that have crossed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ShareProgress {
    Begin { upload_id: u64, resumed: bool, received: Vec<u64> },
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

/// Upload one share to a seat's inbox, answering the absolute paths the
/// server wrote.
///
/// `upload_id` is the previous attempt's, when there was one: the shell asks
/// the server whether it still holds that upload and continues from its
/// offsets, or begins a new one. The answer's `paths` are what the prompt
/// names, and they are only answered once the server has renamed every
/// partial into place.
#[tauri::command]
pub async fn client_share_upload(
    bridge: tauri::State<'_, Arc<Bridge>>,
    channel: tauri::ipc::Channel<ShareProgress>,
    seat: Value,
    files: Vec<SharedFile>,
    upload_id: Option<u64>,
) -> Result<Value, String> {
    let bridge = Arc::clone(bridge.inner());
    upload(&bridge, &channel, seat, files, upload_id).await
}

async fn upload(
    bridge: &Bridge,
    channel: &tauri::ipc::Channel<ShareProgress>,
    seat: Value,
    files: Vec<SharedFile>,
    upload_id: Option<u64>,
) -> Result<Value, String> {
    if files.is_empty() {
        return Err("there is nothing to upload".to_owned());
    }
    let total: u64 = files.iter().map(|file| file.size).sum();
    if total > MAX_TOTAL {
        return Err(format!(
            "the share is {} MiB; one share carries at most {} MiB",
            total / (1024 * 1024),
            MAX_TOTAL / (1024 * 1024),
        ));
    }
    let socket = bridge.socket().ok_or_else(|| "the socket is not open".to_owned())?;
    // **A server that does not hold the upload is not a failure**: a restart
    // or the one-hour window took it, and the share starts over.
    let mut held = None;
    if let Some(id) = upload_id
        && let Ok(offsets) = resume(&socket, &seat, id).await
    {
        held = Some((id, offsets));
    }
    let (id, received, resumed) = if let Some((id, offsets)) = held {
        (id, offsets, true)
    } else {
        (begin(&socket, &seat, &files).await?, vec![0; files.len()], false)
    };
    let _ =
        channel.send(ShareProgress::Begin { upload_id: id, resumed, received: received.clone() });
    for (index, file) in files.iter().enumerate() {
        let start = received.get(index).copied().unwrap_or(0);
        send_file(&socket, channel, id, index, file, start).await?;
    }
    let paths = end(&socket, &seat, id).await?;
    Ok(json!({ "paths": paths }))
}

/// Begin an upload, answering the id it was given.
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
async fn end(socket: &Socket, seat: &Value, id: u64) -> Result<Vec<String>, String> {
    let body = answer(
        socket
            .dispatch(json!({ "file_upload_end": { "key": seat, "upload_id": id } }), true)
            .await?,
    )?;
    Ok(strings(body.get("paths")))
}

/// One file's bytes, from `start` to its end, one frame at a time.
///
/// **Each frame waits for its write.** The socket's queue is unbounded, so a
/// driver that only posted frames would hold a whole share in memory on a
/// phone; waiting on the write paces the upload to the link.
async fn send_file(
    socket: &Socket,
    channel: &tauri::ipc::Channel<ShareProgress>,
    id: u64,
    index: usize,
    file: &SharedFile,
    start: u64,
) -> Result<(), String> {
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
        let _ = channel.send(ShareProgress::Progress {
            file: usize::from(index),
            sent: offset,
            total: len,
        });
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
fn answer(value: Option<Value>) -> Result<Value, String> {
    match value {
        Some(Value::String(why)) => Err(why),
        Some(value @ Value::Object(_)) => Ok(value),
        Some(other) => Err(format!("the server answered something unexpected: {other}")),
        None => Err("the server answered nothing".to_owned()),
    }
}

/// A JSON array of strings, dropping anything that is not one.
fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// The Kotlin handle: the share the activity was launched with.
#[cfg(target_os = "android")]
pub struct Reader(tauri::plugin::PluginHandle<tauri::Wry>);

#[cfg(target_os = "android")]
impl Reader {
    async fn take(&self) -> Result<Shared, String> {
        self.0.run_mobile_plugin_async("take", ()).await.map_err(|why| why.to_string())
    }
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
    let _ = app.emit("client://share", shared);
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

    /// A paths list keeps only what is a path.
    #[test]
    fn a_paths_list_drops_what_is_not_a_string() {
        assert_eq!(
            strings(Some(&json!(["a", 1, null, "b"]))),
            vec!["a".to_owned(), "b".to_owned()]
        );
        assert!(strings(None).is_empty());
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

        let empty: Shared =
            serde_json::from_value(json!({ "files": [], "error": "the provider refused it" }))
                .expect("a refused share");
        assert!(empty.files.is_empty());
        assert_eq!(empty.error.as_deref(), Some("the provider refused it"));
    }
}
