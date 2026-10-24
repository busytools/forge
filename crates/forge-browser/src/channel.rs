//! Frames over a Unix socket: length-prefixed, so one read is one frame.
//!
//! **The server listens and each host connects.** A profile's service binds
//! the socket before spawning its host, and a host that finds no socket
//! keeps painting into its ring and tries again rather than failing.

use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::frame::Frame;

/// The largest frame the channel accepts: a 4K lossless WebP fits under it,
/// and anything past it is a corrupt length rather than a frame.
pub const MAX_FRAME_LEN: usize = 64 * 1024 * 1024;

/// How long a write may block before the host treats the reader as gone. It
/// bounds one syscall; [`write()`] bounds the whole frame besides, so a reader
/// draining a byte at a time cannot stretch a frame out for ever.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the accept loop sleeps while no host is connected. **One Hz**:
/// a host that dies leaves the reader idling until its relaunch, and a
/// hundred wakeups a second for nothing was the cost of a 10 ms poll.
const ACCEPT_POLL: Duration = Duration::from_secs(1);

/// How long a read waits before the loop looks at its stop flag again.
const READ_POLL: Duration = Duration::from_millis(200);

/// How long a frame may stall mid-read before the connection is treated as
/// gone. The host's own write is bounded at two seconds, so five is a wedge.
const STALL: Duration = Duration::from_secs(5);

/// What one [`read_full`] produced.
enum Filled {
    /// Every byte of the buffer arrived.
    Complete,
    /// Nothing arrived and the caller may look at its stop flag.
    Quiet,
    /// The host closed its end.
    Closed,
}

/// Fill `buf` from the stream.
///
/// **Progress survives the poll timeout.** The stream answers a read with
/// nothing available at any point - a frame's body arrives in many chunks,
/// and the reader polls so it can look at its stop flag - so a timeout
/// mid-frame resumes where it left off rather than failing the frame. Only a
/// frame's FIRST byte may report [`Filled::Quiet`]: once a frame has begun,
/// nothing arriving for [`STALL`] is a wedge.
fn read_full(
    stream: &mut UnixStream,
    buf: &mut [u8],
    may_be_quiet: bool,
) -> Result<Filled, String> {
    let mut filled = 0;
    let mut stalled = Duration::ZERO;
    while filled < buf.len() {
        match stream.read(&mut buf[filled..]) {
            Ok(0) => return Ok(Filled::Closed),
            Ok(read) => {
                filled += read;
                stalled = Duration::ZERO;
            }
            Err(why) if matches!(why.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if may_be_quiet && filled == 0 {
                    return Ok(Filled::Quiet);
                }
                stalled += READ_POLL;
                if stalled >= STALL {
                    return Err("a frame stalled mid-read".to_owned());
                }
            }
            Err(why) if why.kind() == ErrorKind::Interrupted => {}
            Err(why) => return Err(format!("the frame was not read: {why}")),
        }
    }
    Ok(Filled::Complete)
}

/// The socket a profile's host writes frames to, under the profile itself.
///
/// **Only a fallback now**: a profile under app-support is deeper than a
/// Unix socket's ~104-byte address allows, so the server passes the short
/// path from [`frame_socket_path`] and this is what a host launched by hand
/// (and the tests) falls back to.
pub fn socket_path(profile: &Path) -> PathBuf {
    profile.join("frames.sock")
}

/// The private directory short sockets live in: `/tmp/forge-<uid>/`.
///
/// **A socket address is about 104 bytes**, and a profile's own directory is
/// deep under app-support, so both the frame socket and the chooser socket
/// live here. `/tmp` is shared, so the directory is created 0700 and checked
/// to be this user's own.
pub fn private_dir(profile: &Path) -> Result<PathBuf, String> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let uid = std::fs::metadata(profile)
        .map_err(|why| {
            format!("the profile directory {} cannot be read: {why}", profile.display())
        })?
        .uid();
    let dir = Path::new("/tmp").join(format!("forge-{uid}"));
    std::fs::create_dir_all(&dir)
        .map_err(|why| format!("the socket directory {} cannot be made: {why}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(|why| {
        format!("the socket directory {} cannot be made private: {why}", dir.display())
    })?;
    let meta = std::fs::metadata(&dir)
        .map_err(|why| format!("the socket directory {} cannot be read: {why}", dir.display()))?;
    if meta.uid() != uid || meta.permissions().mode() & 0o777 != 0o700 {
        return Err(format!(
            "the socket directory {} is not this user's private directory",
            dir.display()
        ));
    }
    Ok(dir)
}

/// The frame socket the server binds for one profile: a short path, with the
/// server's pid so two instances never share one.
pub fn frame_socket_path(profile: &Path, id: u16) -> Result<PathBuf, String> {
    Ok(private_dir(profile)?.join(format!("frames-{}-{id}.sock", std::process::id())))
}

/// Connect to a profile's socket as the writing side.
pub fn connect(path: &Path) -> Result<UnixStream, String> {
    let stream = UnixStream::connect(path)
        .map_err(|why| format!("the frame socket {} did not accept: {why}", path.display()))?;
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|why| format!("the frame socket's write timeout cannot be set: {why}"))?;
    Ok(stream)
}

/// The kind byte a message's body carries: one frame, or one audio packet.
pub const KIND_FRAME: u8 = 1;
pub const KIND_AUDIO: u8 = 2;

/// One audio packet from the page: interleaved 16-bit PCM.
///
/// **PCM, not encoded audio**, because both ends are ours: the host gets
/// float samples from CEF and the client's player takes them directly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audio {
    /// The profile whose page made it.
    pub profile: u16,
    /// The profile's tab whose page made it.
    pub tab: u16,
    /// Monotonic per tab, so a dropped packet is visible.
    pub seq: u32,
    pub sample_rate: u32,
    pub channels: u8,
    /// Interleaved samples, little-endian on the wire.
    pub samples: Vec<i16>,
}

impl Audio {
    /// The header's own length; the samples follow it.
    pub const HEADER_LEN: usize = 13;

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.samples.len() * 2);
        out.extend_from_slice(&self.profile.to_le_bytes());
        out.extend_from_slice(&self.tab.to_le_bytes());
        out.extend_from_slice(&self.seq.to_le_bytes());
        out.extend_from_slice(&self.sample_rate.to_le_bytes());
        out.push(self.channels);
        for sample in &self.samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(format!(
                "an audio packet is at least {} bytes, got {}",
                Self::HEADER_LEN,
                bytes.len()
            ));
        }
        let profile = u16::from_le_bytes([bytes[0], bytes[1]]);
        let tab = u16::from_le_bytes([bytes[2], bytes[3]]);
        let seq = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let sample_rate = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let channels = bytes[12];
        let rest = &bytes[Self::HEADER_LEN..];
        if !rest.len().is_multiple_of(2) {
            return Err("an audio packet carries a whole number of samples".to_owned());
        }
        let samples =
            rest.as_chunks::<2>().0.iter().map(|pair| i16::from_le_bytes(*pair)).collect();
        Ok(Self { profile, tab, seq, sample_rate, channels, samples })
    }
}

/// Write one message: its length, its kind, then the body.
///
/// **The whole message is bounded, not just one syscall.** The stream's own
/// write timeout bounds each `write`; a peer that drains a byte at a time
/// would still hold the message for hours, so the deadline is checked with it
/// and the message is abandoned whole - the next one carries the page.
fn write_message(stream: &mut UnixStream, kind: u8, body: &[u8]) -> Result<(), String> {
    let len = u32::try_from(body.len() + 1)
        .map_err(|why| format!("a message past the channel's length: {why}"))?;
    let mut bytes = Vec::with_capacity(5 + body.len());
    bytes.extend_from_slice(&len.to_le_bytes());
    bytes.push(kind);
    bytes.extend_from_slice(body);
    let deadline = std::time::Instant::now() + WRITE_TIMEOUT;
    while !bytes.is_empty() {
        match stream.write(&bytes) {
            Ok(0) => return Err("the frame socket closed mid-message".to_owned()),
            Ok(written) => {
                bytes.drain(..written);
            }
            Err(why) if why.kind() == ErrorKind::Interrupted => {}
            Err(why) if matches!(why.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if std::time::Instant::now() >= deadline {
                    return Err("the message was not written within its bound".to_owned());
                }
            }
            Err(why) => return Err(format!("the message was not written: {why}")),
        }
        if std::time::Instant::now() >= deadline {
            return Err("the message was not written within its bound".to_owned());
        }
    }
    Ok(())
}

/// Write one frame.
pub fn write(stream: &mut UnixStream, frame: &Frame) -> Result<(), String> {
    write_message(stream, KIND_FRAME, &frame.encode())
}

/// Write one audio packet.
pub fn write_audio(stream: &mut UnixStream, audio: &Audio) -> Result<(), String> {
    write_message(stream, KIND_AUDIO, &audio.encode())
}

/// What one read from a host's socket produced.
#[derive(Debug)]
pub enum Reception {
    /// A whole frame.
    Frame(Frame),
    /// One audio packet.
    Audio(Audio),
    /// Nothing yet: the read timed out, and the loop looks at its stop flag.
    Quiet,
    /// The host closed its end.
    Closed,
}

/// One message from a host's socket, as the server routes it.
#[derive(Debug)]
pub enum ChannelMessage {
    Frame(Frame),
    Audio(Audio),
}

/// Read one message from a host's socket.
pub fn read(stream: &mut UnixStream) -> Result<Reception, String> {
    let mut length = [0_u8; 4];
    match read_full(stream, &mut length, true)? {
        Filled::Complete => {}
        Filled::Quiet => return Ok(Reception::Quiet),
        Filled::Closed => return Ok(Reception::Closed),
    }
    let len = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
    if !(1..=MAX_FRAME_LEN).contains(&len) {
        return Err(format!("a message length of {len} is not a message"));
    }
    let mut body = vec![0_u8; len];
    match read_full(stream, &mut body, false)? {
        Filled::Complete => {}
        // A body that began cannot be quiet: the stall bound answers first.
        Filled::Quiet => return Ok(Reception::Quiet),
        Filled::Closed => return Ok(Reception::Closed),
    }
    let (kind, payload) = body.split_first().ok_or_else(|| "an empty message".to_owned())?;
    match *kind {
        KIND_FRAME => Frame::decode(payload).map(Reception::Frame),
        KIND_AUDIO => Audio::decode(payload).map(Reception::Audio),
        other => Err(format!("a message kind of {other} is not one this channel knows")),
    }
}

/// Bind `path` and hand every frame a host writes to `sink`.
///
/// **The reader runs on its own thread** and is stopped through the returned
/// guard: the server keeps one per open profile, and a closed profile's
/// thread ends rather than waiting on an accept that will never return.
///
/// **Each connection gets a reader of its own.** A host opens one connection
/// per tab, and accepting one stream at a time would leave every tab after
/// the first waiting in the backlog with its frames never read.
pub fn serve<F>(path: &Path, sink: F) -> Result<Reader, String>
where
    F: FnMut(ChannelMessage) + Send + 'static,
{
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)
        .map_err(|why| format!("the frame socket {} cannot be bound: {why}", path.display()))?;
    listener
        .set_nonblocking(true)
        .map_err(|why| format!("the frame socket cannot be made non-blocking: {why}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let own_stop = Arc::clone(&stop);
    let own_path = path.to_path_buf();
    let wake = path.to_path_buf();
    let sink = Arc::new(Mutex::new(sink));
    let thread = std::thread::spawn(move || {
        let mut connections: Vec<JoinHandle<()>> = Vec::new();
        while !own_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let sink = Arc::clone(&sink);
                    let stop = Arc::clone(&own_stop);
                    let path = own_path.clone();
                    connections.push(std::thread::spawn(move || {
                        read_connection(stream, &path, &stop, &sink);
                    }));
                }
                Err(why) if why.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(why) => {
                    eprintln!(
                        "cef host: the frame socket accept failed (event_name \
                         cef_frames_accept_failed, {}): {why}",
                        own_path.display()
                    );
                    break;
                }
            }
        }
        for connection in connections {
            let _ = connection.join();
        }
        let _ = std::fs::remove_file(&own_path);
    });
    Ok(Reader { stop, wake, path: path.to_path_buf(), thread: Some(thread) })
}

/// Read one host connection until it closes or the profile's reader stops.
fn read_connection<F>(mut stream: UnixStream, path: &Path, stop: &AtomicBool, sink: &Mutex<F>)
where
    F: FnMut(ChannelMessage),
{
    if let Err(why) = stream.set_read_timeout(Some(READ_POLL)) {
        eprintln!("cef host: the frame socket's read timeout was refused: {why}");
    }
    // **Blocking, so the timeout is the only bound.** An accepted socket can
    // inherit the listener's non-blocking flag, and then a body read fails
    // the moment no byte is ready.
    if let Err(why) = stream.set_nonblocking(false) {
        eprintln!("cef host: the frame socket cannot be made blocking: {why}");
    }
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        match read(&mut stream) {
            Ok(Reception::Frame(frame)) => {
                (sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner))(
                    ChannelMessage::Frame(frame),
                );
            }
            Ok(Reception::Audio(audio)) => {
                (sink.lock().unwrap_or_else(std::sync::PoisonError::into_inner))(
                    ChannelMessage::Audio(audio),
                );
            }
            Ok(Reception::Quiet) => {}
            Ok(Reception::Closed) => break,
            Err(why) => {
                eprintln!(
                    "cef host: the frame stream ended (event_name cef_frames_read_failed, {}): \
                     {why}",
                    path.display()
                );
                break;
            }
        }
    }
}

/// One profile's frame reader, as the server holds it.
pub struct Reader {
    stop: Arc<AtomicBool>,
    wake: PathBuf,
    path: PathBuf,
    thread: Option<JoinHandle<()>>,
}

impl Reader {
    /// End the reader: the flag stops the loop, and a connection to the
    /// socket wakes an accept already waiting when it was set.
    pub fn stop(mut self) {
        self.end();
    }

    fn end(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = UnixStream::connect(&self.wake);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.end();
    }
}

#[cfg(test)]
mod tests {
    use super::frame_socket_path;

    /// **A profile name can be 64 characters, and its socket must still
    /// bind.** A Unix socket's address is about 104 bytes; the profile tree
    /// under app-support plus a full-length name is far past it, which is
    /// why the frame socket lives in the short private directory (#2539 made
    /// an omitted profile the seat's own, so names are long by default).
    #[test]
    fn a_full_length_profile_name_still_leaves_a_short_socket() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let profile = dir.path().join("p".repeat(64)).join("cef");
        std::fs::create_dir_all(&profile).expect("the profile dir");
        let path = frame_socket_path(&profile, 1).expect("the frame socket path");
        assert!(
            path.as_os_str().len() < 104,
            "{} is {} bytes, past a Unix socket's address",
            path.display(),
            path.as_os_str().len(),
        );
        assert!(
            path.starts_with(std::path::Path::new("/tmp")),
            "the short path is outside the profile tree: {}",
            path.display(),
        );
    }
}
