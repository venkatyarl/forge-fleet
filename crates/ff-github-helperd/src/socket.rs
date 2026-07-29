use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::Path,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::protocol::{MAX_FRAME_BYTES, Request, Response};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PeerIdentity {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
    pub start_time: u64,
    pub executable_sha256: String,
    pub cgroup: String,
}

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("socket activation contract violated")]
    Activation,
    #[error("unsafe socket")]
    UnsafeSocket,
    #[error("peer identity rejected")]
    Peer,
    #[error("invalid frame")]
    Frame,
    #[error("I/O error")]
    Io,
}

pub fn activated_listener(expected_path: &Path) -> Result<UnixListener, SocketError> {
    if std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse().ok())
        != Some(std::process::id())
        || std::env::var("LISTEN_FDS").as_deref() != Ok("1")
    {
        return Err(SocketError::Activation);
    }
    validate_socket(expected_path)?;
    // SAFETY: systemd's socket activation ABI assigns the sole declared fd to 3.
    let listener = unsafe { UnixListener::from_raw_fd(3) };
    Ok(listener)
}

pub fn validate_socket(path: &Path) -> Result<(), SocketError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SocketError::UnsafeSocket)?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != 0
        || metadata.permissions().mode() & 0o777 != 0o660
    {
        return Err(SocketError::UnsafeSocket);
    }
    Ok(())
}

pub fn peer_identity(stream: &UnixStream) -> Result<PeerIdentity, SocketError> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: writable ucred and size are correctly provided for a live socket.
    if unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    } != 0
    {
        return Err(SocketError::Peer);
    }
    let pidfd = pidfd_open(cred.pid)?;
    let first_stat =
        fs::read_to_string(format!("/proc/{}/stat", cred.pid)).map_err(|_| SocketError::Peer)?;
    let start_time = parse_start_time(&first_stat)?;
    let cgroup =
        fs::read_to_string(format!("/proc/{}/cgroup", cred.pid)).map_err(|_| SocketError::Peer)?;
    if !cgroup
        .lines()
        .any(|line| line.ends_with("/forgefleet-scheduler.service"))
    {
        return Err(SocketError::Peer);
    }
    let exe_path = format!("/proc/{}/exe", cred.pid);
    let metadata = fs::metadata(&exe_path).map_err(|_| SocketError::Peer)?;
    if metadata.uid() != 0 || metadata.permissions().mode() & 0o022 != 0 {
        return Err(SocketError::Peer);
    }
    let mut exe = File::open(exe_path).map_err(|_| SocketError::Peer)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut exe, &mut hasher).map_err(|_| SocketError::Peer)?;
    ensure_alive(pidfd.as_raw_fd())?;
    let second_stat =
        fs::read_to_string(format!("/proc/{}/stat", cred.pid)).map_err(|_| SocketError::Peer)?;
    if parse_start_time(&second_stat)? != start_time {
        return Err(SocketError::Peer);
    }
    Ok(PeerIdentity {
        pid: cred.pid,
        uid: cred.uid,
        gid: cred.gid,
        start_time,
        executable_sha256: hex::encode(hasher.finalize()),
        cgroup,
    })
}

fn pidfd_open(pid: i32) -> Result<OwnedFd, SocketError> {
    // SAFETY: pidfd_open takes scalar arguments and returns a new owned fd.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as RawFd };
    if fd < 0 {
        return Err(SocketError::Peer);
    }
    // SAFETY: successful pidfd_open returned a unique owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn ensure_alive(fd: RawFd) -> Result<(), SocketError> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: pollfd points to one initialized entry.
    if unsafe { libc::poll(&mut pollfd, 1, 0) } != 0 {
        return Err(SocketError::Peer);
    }
    Ok(())
}

fn parse_start_time(stat: &str) -> Result<u64, SocketError> {
    let end = stat.rfind(')').ok_or(SocketError::Peer)?;
    stat[end + 2..]
        .split_whitespace()
        .nth(19)
        .and_then(|v| v.parse().ok())
        .ok_or(SocketError::Peer)
}

pub fn read_request(stream: &mut UnixStream) -> Result<Request, SocketError> {
    let mut length = [0; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| SocketError::Io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(SocketError::Frame);
    }
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame).map_err(|_| SocketError::Io)?;
    serde_json::from_slice(&frame).map_err(|_| SocketError::Frame)
}

pub fn write_response(stream: &mut UnixStream, response: &Response) -> Result<(), SocketError> {
    let frame = serde_json::to_vec(response).map_err(|_| SocketError::Frame)?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(SocketError::Frame);
    }
    stream
        .write_all(&(frame.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&frame))
        .map_err(|_| SocketError::Io)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parser_handles_spaces_in_comm() {
        let mut fields = vec!["S"; 20];
        fields[19] = "4242";
        let stat = format!("7 (name with spaces) {}", fields.join(" "));
        assert_eq!(parse_start_time(&stat).unwrap(), 4242);
    }
}
