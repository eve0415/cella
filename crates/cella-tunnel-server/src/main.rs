//! Container-side tunnel server binary.
//!
//! Creates Unix sockets for SSH agent and git credential forwarding,
//! multiplexes traffic over stdin/stdout to the host daemon.
//!
//! Also doubles as a git credential helper via the `credential-helper` subcommand.
//!
//! No external dependencies — uses only std for minimal binary size.

use std::collections::HashMap;
use std::io::{self, BufWriter, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Inline mux protocol (duplicated from cella-tunnel/src/mux.rs for standalone)
// ---------------------------------------------------------------------------

const MAX_PAYLOAD_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum FrameType {
    Data = 0x01,
    Open = 0x02,
    Close = 0x03,
    Heartbeat = 0x04,
    HeartbeatAck = 0x05,
}

impl FrameType {
    const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Data),
            0x02 => Some(Self::Open),
            0x03 => Some(Self::Close),
            0x04 => Some(Self::Heartbeat),
            0x05 => Some(Self::HeartbeatAck),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ChannelKind {
    SshAgent = 0x01,
    Credential = 0x02,
}

#[allow(clippy::struct_field_names)]
struct Frame {
    channel: u32,
    frame_type: FrameType,
    payload: Vec<u8>,
}

fn write_frame<W: Write>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    let payload_len = frame.payload.len();
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "payload too large",
        ));
    }

    #[allow(clippy::cast_possible_truncation)]
    let length = 5u32 + payload_len as u32;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&frame.channel.to_be_bytes())?;
    writer.write_all(&[frame.frame_type as u8])?;
    if !frame.payload.is_empty() {
        writer.write_all(&frame.payload)?;
    }
    writer.flush()
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let length = u32::from_be_bytes(len_buf) as usize;

    if length < 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too short",
        ));
    }

    let payload_len = length - 5;
    if payload_len > MAX_PAYLOAD_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload too large",
        ));
    }

    let mut chan_buf = [0u8; 4];
    reader.read_exact(&mut chan_buf)?;
    let channel = u32::from_be_bytes(chan_buf);

    let mut type_buf = [0u8; 1];
    reader.read_exact(&mut type_buf)?;
    let frame_type = FrameType::from_byte(type_buf[0])
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unknown frame type"))?;

    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        reader.read_exact(&mut payload)?;
    }

    Ok(Some(Frame {
        channel,
        frame_type,
        payload,
    }))
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

type ChannelMap = Arc<Mutex<HashMap<u32, std::sync::mpsc::Sender<Vec<u8>>>>>;
type StdoutWriter = Arc<Mutex<BufWriter<io::Stdout>>>;

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Check for credential-helper subcommand
    if args.len() >= 2 && args[1] == "credential-helper" {
        run_credential_helper(&args[2..]);
        return;
    }

    // Parse --ssh-agent and --credential socket paths
    let mut ssh_agent_path = None;
    let mut credential_path = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--ssh-agent" => {
                i += 1;
                ssh_agent_path = args.get(i).cloned();
            }
            "--credential" => {
                i += 1;
                credential_path = args.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    run_server(ssh_agent_path.as_deref(), credential_path.as_deref());
}

// ---------------------------------------------------------------------------
// Server mode — socket listeners + mux over stdin/stdout
// ---------------------------------------------------------------------------

fn run_server(ssh_agent_path: Option<&str>, credential_path: Option<&str>) {
    let writer: StdoutWriter = Arc::new(Mutex::new(BufWriter::new(io::stdout())));
    let channels: ChannelMap = Arc::new(Mutex::new(HashMap::new()));
    let next_channel = Arc::new(AtomicU32::new(1));

    // Spawn SSH agent socket listener
    if let Some(path) = ssh_agent_path {
        let listener = bind_socket(path);
        let writer = Arc::clone(&writer);
        let channels = Arc::clone(&channels);
        let next_channel = Arc::clone(&next_channel);
        std::thread::spawn(move || {
            accept_loop(
                listener,
                ChannelKind::SshAgent,
                &writer,
                &channels,
                &next_channel,
            );
        });
    }

    // Spawn credential proxy socket listener
    if let Some(path) = credential_path {
        let listener = bind_socket(path);
        let writer = Arc::clone(&writer);
        let channels = Arc::clone(&channels);
        let next_channel = Arc::clone(&next_channel);
        std::thread::spawn(move || {
            accept_loop(
                listener,
                ChannelKind::Credential,
                &writer,
                &channels,
                &next_channel,
            );
        });
    }

    // Main thread: read frames from stdin and dispatch
    let mut stdin = io::stdin().lock();
    loop {
        match read_frame(&mut stdin) {
            Ok(Some(frame)) => dispatch_frame(frame, &writer, &channels),
            Ok(None) => {
                // EOF on stdin — tunnel broken, exit cleanly
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("cella-tunnel-server: stdin read error: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn bind_socket(path: &str) -> UnixListener {
    // Clean up stale socket
    let _ = std::fs::remove_file(path);

    let listener = UnixListener::bind(path).unwrap_or_else(|e| {
        eprintln!("cella-tunnel-server: failed to bind {path}: {e}");
        std::process::exit(1);
    });

    // Set socket permissions to 0o666 (accessible by all users in container)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666));
    }

    listener
}

#[allow(clippy::needless_pass_by_value)]
fn accept_loop(
    listener: UnixListener,
    kind: ChannelKind,
    writer: &StdoutWriter,
    channels: &ChannelMap,
    next_channel: &AtomicU32,
) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                handle_connection(stream, kind, next_channel, writer, channels);
            }
            Err(e) => {
                eprintln!("cella-tunnel-server: accept error: {e}");
            }
        }
    }
}

fn handle_connection(
    stream: UnixStream,
    kind: ChannelKind,
    next_channel: &AtomicU32,
    writer: &StdoutWriter,
    channels: &ChannelMap,
) {
    let channel_id = next_channel.fetch_add(1, Ordering::Relaxed);

    // Send OPEN frame
    {
        let mut w = writer
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = write_frame(
            &mut *w,
            &Frame {
                channel: channel_id,
                frame_type: FrameType::Open,
                payload: vec![kind as u8],
            },
        );
    }

    // Create channel for receiving data from host
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    channels
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(channel_id, tx);

    // Clone stream for write thread
    let write_stream = match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cella-tunnel-server: failed to clone stream: {e}");
            channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&channel_id);
            return;
        }
    };

    // Write-to-socket thread: receives data from host via channel, writes to socket
    std::thread::spawn(move || {
        let mut ws = write_stream;
        loop {
            if let Ok(payload) = rx.recv() {
                if ws.write_all(&payload).is_err() {
                    break;
                }
            } else {
                // Channel closed (CLOSE received from host, tx dropped)
                let _ = ws.shutdown(Shutdown::Write);
                break;
            }
        }
    });

    // Read-from-socket thread: reads from socket, sends DATA frames to host
    let writer_clone = Arc::clone(writer);
    let channels_clone = Arc::clone(channels);
    std::thread::spawn(move || {
        let mut rs = stream;
        let mut buf = vec![0u8; 32768];
        loop {
            match rs.read(&mut buf) {
                Ok(0) | Err(_) => {
                    // Socket closed by client — send CLOSE to host
                    {
                        let mut w = writer_clone
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner);
                        let _ = write_frame(
                            &mut *w,
                            &Frame {
                                channel: channel_id,
                                frame_type: FrameType::Close,
                                payload: Vec::new(),
                            },
                        );
                    }
                    channels_clone
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&channel_id);
                    break;
                }
                Ok(n) => {
                    let mut w = writer_clone
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let _ = write_frame(
                        &mut *w,
                        &Frame {
                            channel: channel_id,
                            frame_type: FrameType::Data,
                            payload: buf[..n].to_vec(),
                        },
                    );
                }
            }
        }
    });
}

fn dispatch_frame(frame: Frame, writer: &StdoutWriter, channels: &ChannelMap) {
    match frame.frame_type {
        FrameType::Data => {
            let map = channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(tx) = map.get(&frame.channel) {
                let _ = tx.send(frame.payload);
            }
        }
        FrameType::Close => {
            // Drop the sender, causing the write-to-socket thread to exit
            channels
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&frame.channel);
        }
        FrameType::Heartbeat => {
            // Respond with HeartbeatAck
            let mut w = writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = write_frame(
                &mut *w,
                &Frame {
                    channel: 0,
                    frame_type: FrameType::HeartbeatAck,
                    payload: Vec::new(),
                },
            );
        }
        FrameType::HeartbeatAck | FrameType::Open => {
            // HeartbeatAck: ignored on container side
            // Open: container doesn't receive OPEN from host
        }
    }
}

// ---------------------------------------------------------------------------
// Credential helper subcommand
// ---------------------------------------------------------------------------

fn run_credential_helper(args: &[String]) {
    let operation = args.first().map_or("get", String::as_str);

    // Read credential data from stdin (git pipes key=value lines)
    let mut input = String::new();
    let _ = io::stdin().read_to_string(&mut input);

    // Build request: operation\nfields\n\n
    let mut request = format!("{operation}\n{input}");
    if !request.ends_with("\n\n") {
        if !request.ends_with('\n') {
            request.push('\n');
        }
        request.push('\n');
    }

    // Connect to tunnel server's credential socket
    let socket_path = "/tmp/cella-credential-proxy.sock";
    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cella: credential socket unavailable: {e}");
            std::process::exit(1);
        }
    };

    stream.set_read_timeout(Some(Duration::from_secs(30))).ok();
    let _ = stream.write_all(request.as_bytes());

    // Read response (blocks until host responds and tunnel server shuts down write side)
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);

    if !response.is_empty() {
        print!("{response}");
    }
}
