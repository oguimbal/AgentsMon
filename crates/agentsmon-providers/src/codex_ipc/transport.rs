//! Nonblocking Unix I/O with an overall deadline (including partial frames).
use super::protocol::{Fault, MAX_FRAME};
use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{
    ffi::OsStrExt,
    fs::{FileTypeExt, MetadataExt},
    net::UnixStream,
};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn pause(deadline: Instant, stop: &AtomicBool, phase: &'static str) -> Result<(), Fault> {
    if stop.load(Ordering::Relaxed) {
        return Err(Fault::io(
            phase,
            io::Error::from(io::ErrorKind::Interrupted),
        ));
    }
    if Instant::now() >= deadline {
        return Err(Fault::timeout(phase));
    }
    std::thread::sleep(Duration::from_millis(5));
    Ok(())
}

pub(super) fn connect(
    path: &Path,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<UnixStream, Fault> {
    let check = || -> io::Result<()> {
        let dir = fs::symlink_metadata(path.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
        let socket = fs::symlink_metadata(path)?;
        // Do not create/chmod/remove Codex's socket. Reject symlinks and sockets
        // belonging to another account, before exposing even metadata to it.
        let uid = unsafe { libc::geteuid() };
        if !dir.is_dir()
            || dir.uid() != uid
            || dir.mode() & 0o077 != 0
            || !socket.file_type().is_socket()
            || socket.uid() != uid
            || socket.mode() & 0o077 != 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    };
    check().map_err(|e| Fault::io("connect", e))?;
    let bytes = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.contains(&0) || bytes.len() >= address.sun_path.len() {
        return Err(Fault::io("connect", io::ErrorKind::InvalidInput.into()));
    }
    address.sun_family = libc::AF_UNIX as _;
    for (dst, src) in address.sun_path.iter_mut().zip(bytes) {
        *dst = *src as _;
    }
    let length =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
    #[cfg(target_os = "macos")]
    {
        address.sun_len = length as _;
    }
    let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw < 0 {
        return Err(Fault::io("connect", io::Error::last_os_error()));
    }
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(Fault::io("connect", io::Error::last_os_error()));
    }
    let stream = UnixStream::from(fd);
    stream
        .set_nonblocking(true)
        .map_err(|e| Fault::io("connect", e))?;
    let result = unsafe {
        libc::connect(
            stream.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            length,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if !matches!(
            error.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EAGAIN)
        ) {
            return Err(Fault::io("connect", error));
        }
        loop {
            pause(deadline, stop, "connect")?;
            if let Some(error) = stream.take_error().map_err(|e| Fault::io("connect", e))? {
                return Err(Fault::io("connect", error));
            }
            if stream.peer_addr().is_ok() {
                break;
            }
        }
    }
    Ok(stream)
}

fn transfer(
    stream: &mut UnixStream,
    bytes: &mut [u8],
    deadline: Instant,
    stop: &AtomicBool,
    phase: &'static str,
    write: bool,
) -> Result<(), Fault> {
    let mut offset = 0;
    while offset < bytes.len() {
        if Instant::now() >= deadline {
            return Err(Fault::timeout(phase));
        }
        if stop.load(Ordering::Relaxed) {
            return Err(Fault::io(phase, io::ErrorKind::Interrupted.into()));
        }
        let result = if write {
            stream.write(&bytes[offset..])
        } else {
            stream.read(&mut bytes[offset..])
        };
        match result {
            Ok(0) => return Err(Fault::io(phase, io::ErrorKind::UnexpectedEof.into())),
            Ok(n) => offset += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => pause(deadline, stop, phase)?,
            Err(e) => return Err(Fault::io(phase, e)),
        }
    }
    Ok(())
}

pub(super) fn read(
    stream: &mut UnixStream,
    deadline: Instant,
    stop: &AtomicBool,
    phase: &'static str,
) -> Result<Vec<u8>, Fault> {
    let mut header = [0; 4];
    transfer(stream, &mut header, deadline, stop, phase, false)?;
    let len = u32::from_le_bytes(header) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(Fault::protocol(
            "frame_size",
            phase,
            format!("received_bytes={len}; max_bytes={MAX_FRAME}"),
        ));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| {
        Fault::protocol("allocation_limit", phase, format!("requested_bytes={len}"))
    })?;
    bytes.resize(len, 0);
    transfer(stream, &mut bytes, deadline, stop, phase, false)?;
    Ok(bytes)
}

pub(super) fn write(
    stream: &mut UnixStream,
    value: serde_json::Value,
    deadline: Instant,
    stop: &AtomicBool,
    phase: &'static str,
) -> Result<(), Fault> {
    // Value is allowed for OUR outgoing metadata, never for incoming payloads.
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|_| Fault::protocol("encode_error", phase, "outgoing metadata encoding failed"))?;
    transfer(
        stream,
        &mut (bytes.len() as u32).to_le_bytes(),
        deadline,
        stop,
        phase,
        true,
    )?;
    transfer(stream, &mut bytes, deadline, stop, phase, true)
}
