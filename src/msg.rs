use nix::cmsg_space;
use nix::errno::Errno;
use nix::sys::socket::{
    recvmsg as unix_recvmsg, sendmsg as unix_sendmsg, ControlMessage, ControlMessageOwned,
    MsgFlags as UnixMsgFlags,
};
use smallvec::SmallVec;
use std::error::Error;
use std::io::{self as stdio, IoSlice, IoSliceMut};
use std::os::fd::AsRawFd;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::task::{self, ready, Poll};
use tokio::io::unix::AsyncFd;
use tokio::io::{self, AsyncReadExt};

/// Taken from libwayland.
/// Since libwayland doesn't do proper MSG_CTRUNC handling,
/// this is pretty much the unchangeable standard for max FDs
/// in one packet.
const MAX_FDS_IN_ONE_CMSG: usize = 28;

/// Unix stream socket that implements [`tokio::io::AsyncRead`]
/// and also keeps track of shared file descriptors.
/// (SCM stands for Socket-level Control Message, by the way.)
#[derive(Debug)]
pub struct SockScm {
    sock_fd: AsyncFd<RawFd>,
    /// Tuple (file descriptor, byte position it was obtained at).
    scm_fds: SmallVec<[(RawFd, usize); MAX_FDS_IN_ONE_CMSG]>,
    /// Current number of bytes read.
    /// (Useful for synchronizing file-descriptor reads.)
    bytepos: usize,
}

// TODO libwayland handles socket demarshal errors
// by emptying out the rest of the sent buffer.
// We should probably copy that.

impl Unpin for SockScm {}

impl SockScm {
    pub fn new(fd: RawFd) -> io::Result<Self> {
        Ok(Self {
            sock_fd: AsyncFd::new(fd)?,
            scm_fds: SmallVec::new(),
            bytepos: 0,
        })
    }

    /// Returns the file descriptors that were read between byte positions
    /// `from_bytepos <= bytepos <= to_bytepos` inclusive.
    /// Also forgets every file descriptor that came before `from_bytepos`.
    fn fds_between<'s>(
        &'s mut self,
        from_bytepos: usize,
        to_bytepos: usize,
    ) -> impl Iterator<Item = (RawFd, usize)> + 's {
        // TODO test
        let mut drain_start = 0;
        let mut drain_end = 0;
        for (i, &(_, bytepos)) in self.scm_fds.iter().enumerate() {
            if bytepos <= from_bytepos {
                drain_start = i;
            }
            if bytepos <= to_bytepos {
                drain_end = i+1;
            }
        }
        dbg!(drain_end);
        self.scm_fds.drain(0..drain_end).skip(drain_start)
    }

    /// Writes everything in `data` to the socket.
    /// `data` is a scatter-gather vector of data.
    async fn write_all<const N: usize>(&self, data: [&[u8]; N], fds: &[RawFd]) -> io::Result<()> {
        let mut guard = self.sock_fd.writable().await?;
        let mut iov = data.map(IoSlice::new);
        let cmsg = ControlMessage::ScmRights(&fds);

        // Because [`std::io::IoSlice::advance_slices`] is unstable,
        // we'll have to do it manually.
        let mut iov_idx = 0;

        loop {
            let res = unix_sendmsg::<()>(
                self.sock_fd.as_raw_fd(),
                &iov[iov_idx..],
                &[cmsg],
                UnixMsgFlags::MSG_DONTWAIT,
                None,
            );
            let written = match res {
                Err(Errno::EWOULDBLOCK) => {
                    guard.clear_ready();
                    guard = self.sock_fd.writable().await?;
                    continue;
                }
                Err(errno) => return Err(errno.into()),
                Ok(len) => len,
            };
            // advance iov by the byte count so we don't rewrite already-written data
            let mut curr = 0;
            for buf in iov[iov_idx..].iter() {
                curr += buf.len();
                if curr > written {
                    break;
                }
                iov_idx += 1;
            }
            if iov_idx >= data.len() {
                // all bytes are written, we're done
                return Ok(());
            }
            // truncate the current IoSlice by the leftover byte count
            iov[iov_idx] = IoSlice::new(&data[iov_idx][curr - written..])
            // repeat
        }
    }
}

impl io::AsyncRead for SockScm {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<stdio::Result<()>> {
        let mut guard = ready!(self.sock_fd.poll_read_ready(cx))?;
        let mut unfilled = buf.initialize_unfilled();
        let mut cmsg_buf = cmsg_space!([RawFd; MAX_FDS_IN_ONE_CMSG]);
        let mut iov = [IoSliceMut::new(&mut unfilled)];
        // This retry-loop is here in case we run into `EWOULDBLOCK`.
        // It's probably not possible for that to actually occur.
        let res = loop {
            let res = unix_recvmsg::<()>(
                self.sock_fd.as_raw_fd(),
                &mut iov,
                Some(&mut cmsg_buf),
                UnixMsgFlags::MSG_DONTWAIT,
            );
            break match res {
                Err(Errno::EWOULDBLOCK) => {
                    // no longer ready
                    guard.clear_ready();
                    guard = ready!(self.sock_fd.poll_read_ready(cx))?;
                    continue;
                }
                Err(errno) => {
                    return Poll::Ready(Err(errno.into()));
                }
                Ok(r) => r,
            };
        };
        let len_read = res.bytes;
        self.bytepos += res.bytes;
        let bytepos = self.bytepos;
        // XXX We don't need to handle `MSG_CTRUNC` here because libwayland doesn't do it either,
        // so implementing proper CTRUNC handling would be pointless.
        // (I should probably document this somewhere other than here.)
        let fd_iter = res
            .cmsgs()
            .filter_map(|cmsg| match cmsg {
                ControlMessageOwned::ScmRights(fd_buf) => {
                    Some(fd_buf.into_iter().map(|fd| (fd, bytepos)))
                }
                _ => None,
            })
            .flatten();
        self.get_mut().scm_fds.extend(fd_iter);
        buf.advance(len_read);
        Poll::Ready(Ok(()))
    }
}

#[derive(Debug, Clone)]
pub struct RawMsg {
    sender_obj_id: u32,
    total_msg_len: u16,
    opcode: u16,
    payload: Vec<u32>,
    fds: Vec<RawFd>,
}

#[derive(Debug)]
pub enum NextMsgError {
    Io(io::Error),
    /// By the Wayland protocol,
    /// all messages must contain a fixed number of `u32`s.
    /// So the length must always be a multiple of 4 bytes.
    /// (Why they didn't just specify the length as the u32 count
    /// instead of the byte count is a great question.)
    LenIsNotAMultipleOf4,
}

impl From<io::Error> for NextMsgError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl std::fmt::Display for NextMsgError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "IO error: {}", e),
            Self::LenIsNotAMultipleOf4 => write!(f, "Provided message length was not a multiple of 4")
        }
    }
}

impl Error for NextMsgError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(v) => Some(v),
            _ => None,
        }
    }
}

pub async fn recv_msg(sock: &mut io::BufReader<SockScm>) -> Result<RawMsg, NextMsgError> {
    // The bytepos that we are starting at from our "frame of reference"
    // for our reader.
    let start_bytepos = sock.get_mut().bytepos - sock.buffer().len();

    let mut eight_bytes = [0u8; 8];
    sock.read_exact(&mut eight_bytes).await?;
    let (first_dword, second_dword) = eight_bytes.split_at(4);
    let sender_obj_id = u32::from_ne_bytes(first_dword.try_into().unwrap());
    let right = u32::from_ne_bytes(second_dword.try_into().unwrap());
    let total_msg_len = (right >> 16) as u16;
    if total_msg_len % 4 != 0 {
        return Err(NextMsgError::LenIsNotAMultipleOf4);
    }
    let opcode = right as u16;
    let mut payload = vec![0u32; (total_msg_len as usize - 8/* for the header */) / 4];
    {
        let bytes = bytemuck::cast_slice_mut(&mut payload);
        sock.read_exact(bytes).await?;
    }

    let end_bytepos = sock.get_mut().bytepos - sock.buffer().len();

    let fds = sock
        .get_mut()
        .fds_between(start_bytepos, end_bytepos)
        .map(|(fd, _)| fd)
        .collect();

    Ok(RawMsg {
        sender_obj_id,
        total_msg_len,
        opcode,
        payload,
        fds,
    })
}

pub async fn send_msg(sock: &mut SockScm, msg: &RawMsg) -> io::Result<()> {
    let header = [
        msg.sender_obj_id,
        ((msg.total_msg_len as u32) << 16) + msg.opcode as u32,
    ];
    let header_bytes: &[u8] = bytemuck::cast_slice(&header);
    let payload_bytes: &[u8] = bytemuck::cast_slice(&msg.payload);
    sock.write_all([header_bytes, payload_bytes], &msg.fds)
        .await
}

#[cfg(test)]
mod tests {
    use std::os::fd::IntoRawFd;

    use super::*;

    use proptest::prelude::*;
    use tokio::{io::{BufReader, AsyncWriteExt}, net::UnixStream};
    use nix::sys::socket::*;

    #[test]
    fn basic_recv() {
        crate::test_harness!(|| {
            let (mut tx, rx) = UnixStream::pair().unwrap();
            let (o1, o2) = socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC
            ).unwrap();
            let mut scm = BufReader::new(SockScm::new(rx.into_std().unwrap().into_raw_fd()).unwrap());

            macro_rules! check {
                ($a:expr, $b:expr) => {
                    let a = $a;
                    let b = $b;
                    let bytes = bytemuck::cast_slice(&a);
                    tx.write_all(&bytes).await.unwrap();
                    let msg = recv_msg(&mut scm).await.unwrap();
                    assert_eq!(msg.sender_obj_id, b.0);
                    assert_eq!(msg.total_msg_len, b.1);
                    assert_eq!(msg.opcode, b.2);
                    assert_eq!(msg.payload, b.3);
                }
            }
            check!(
                vec![0x1, 0xC0001, 0x2],
                (1, 12, 1, vec![0x2])
            );
        });
    }

    #[test]
    fn send_and_recv_match_each_other() {
        crate::test_harness!(init {
            let (o1, o2) = socketpair(
                AddressFamily::Unix,
                SockType::Stream,
                None,
                SockFlag::SOCK_NONBLOCK | SockFlag::SOCK_CLOEXEC
            ).unwrap();
            (o1.into_raw_fd(), o2.into_raw_fd())
        }, |sockpair, (sender_obj_id: u32, payload_len_in_u32 in 0..16380u16, opcode: u16)| {
            let payload_len = payload_len_in_u32 * 4;
            let (fd1, fd2) = sockpair;
            let (s1, s2) = (SockScm::new(fd1).unwrap(), SockScm::new(fd2).unwrap());
            let (mut s1, mut s2) = (BufReader::new(s1), BufReader::new(s2));
            let payload = vec![0xFF_FF_FF_FF; payload_len_in_u32 as usize];
            send_msg(s1.get_mut(), &RawMsg {
                sender_obj_id,
                total_msg_len: payload_len + 8,
                opcode,
                payload: payload.clone(),
                fds: vec![fd1]
            }).await.unwrap();
            let msg = recv_msg(&mut s2).await.unwrap();
            prop_assert_eq!(msg.sender_obj_id, sender_obj_id);
            prop_assert_eq!(msg.total_msg_len, payload_len + 8);
            prop_assert_eq!(msg.opcode, opcode);
            prop_assert_eq!(&msg.payload, &payload);
            // the fd number may be different on the other side
            prop_assert!(msg.fds.len() == 1, "msg.fds: {:?}", msg.fds);
            // close extra fd so we don't accumulate open files
            nix::unistd::close(msg.fds[0]).unwrap();
        });
    }
}
