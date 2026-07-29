use std::{
    fs,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, PermissionsExt},
            net::UnixStream,
        },
    },
    path::Path,
};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    protocol::{MAX_FRAME_BYTES, decode_frame},
    service::PeerIdentity,
};

#[derive(Debug, Error)]
pub enum SocketError {
    #[error("socket I/O failed")]
    Io,
    #[error("scheduler peer identity could not be established")]
    PeerIdentity,
    #[error("invalid protocol frame")]
    InvalidFrame,
    #[error("unsafe socket path")]
    UnsafePath,
}

/// Resolve the kernel-authenticated Unix peer and hash its pinned executable.
///
/// The executable is opened through `/proc/<pid>/exe` immediately after
/// SO_PEERCRED. Deployments additionally pin the returned digest in service
/// configuration; caller-provided process metadata is never trusted.
pub fn peer_identity(stream: &UnixStream) -> Result<PeerIdentity, SocketError> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `credentials` and `length` are valid writable pointers with the
    // exact sizes required by SO_PEERCRED, and stream owns a live socket fd.
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if result != 0 || length as usize != std::mem::size_of::<libc::ucred>() {
        return Err(SocketError::PeerIdentity);
    }
    let executable = fs::read(format!("/proc/{}/exe", credentials.pid))
        .map_err(|_| SocketError::PeerIdentity)?;
    Ok(PeerIdentity {
        uid: credentials.uid,
        gid: credentials.gid,
        executable_sha256: hex::encode(Sha256::digest(executable)),
    })
}

pub fn read_request(stream: &mut UnixStream) -> Result<crate::protocol::Envelope, SocketError> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|_| SocketError::Io)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(SocketError::InvalidFrame);
    }
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame).map_err(|_| SocketError::Io)?;
    decode_frame(&frame).map_err(|_| SocketError::InvalidFrame)
}

pub fn write_response(
    stream: &mut UnixStream,
    response: &crate::protocol::Response,
) -> Result<(), SocketError> {
    let frame = serde_json::to_vec(response).map_err(|_| SocketError::InvalidFrame)?;
    if frame.len() > MAX_FRAME_BYTES {
        return Err(SocketError::InvalidFrame);
    }
    stream
        .write_all(&(frame.len() as u32).to_be_bytes())
        .and_then(|_| stream.write_all(&frame))
        .map_err(|_| SocketError::Io)
}

pub fn validate_socket_file(path: &Path) -> Result<(), SocketError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| SocketError::UnsafePath)?;
    if !metadata.file_type().is_socket() || metadata.permissions().mode() & 0o077 != 0 {
        return Err(SocketError::UnsafePath);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{DenialCode, Response};

    #[test]
    fn framing_round_trip_and_kernel_peer_identity() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let response = Response::Denied {
            code: DenialCode::InvalidRequest,
        };
        write_response(&mut client, &response).unwrap();
        let mut length = [0; 4];
        server.read_exact(&mut length).unwrap();
        let mut body = vec![0; u32::from_be_bytes(length) as usize];
        server.read_exact(&mut body).unwrap();
        assert_eq!(serde_json::from_slice::<Response>(&body).unwrap(), response);

        let peer = peer_identity(&server).unwrap();
        assert_eq!(peer.uid, unsafe { libc::geteuid() });
        assert_eq!(peer.executable_sha256.len(), 64);
    }
}
