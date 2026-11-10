//! One-shot descriptor transfer between CineUSB and its Homebrew-launched helper.
use std::fs::File;
use std::io;
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;

fn message(byte: &mut u8, data: &mut libc::iovec, control: &mut [usize; 8]) -> libc::msghdr {
    *data = libc::iovec { iov_base: (byte as *mut u8).cast(), iov_len: 1 };
    let mut msg: libc::msghdr = unsafe { zeroed() };
    msg.msg_iov = data;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as _) } as usize;
    msg
}

#[allow(dead_code)] // The storage binary uses the sending half, the app uses the receiving half.
pub fn send(socket: &UnixStream, file: &File) -> io::Result<()> {
    let mut byte = 1;
    let mut control = [0; 8];
    let mut data = unsafe { zeroed() };
    let msg = message(&mut byte, &mut data, &mut control);
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&msg);
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as _) as usize;
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        libc::CMSG_DATA(header).cast::<libc::c_int>().write_unaligned(file.as_raw_fd());
        if libc::sendmsg(socket.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) != 1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[allow(dead_code)]
pub fn receive(socket: &UnixStream) -> io::Result<File> {
    let mut byte = 0;
    let mut control = [0; 8];
    let mut data = unsafe { zeroed() };
    let mut msg = message(&mut byte, &mut data, &mut control);
    unsafe {
        let count = libc::recvmsg(socket.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if count < 0 { return Err(io::Error::last_os_error()); }
        let header = libc::CMSG_FIRSTHDR(&msg);
        if header.is_null() || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "missing USB descriptor"));
        }
        // Close every descriptor delivered, even if the helper returned an invalid packet.
        let bytes = (*header).cmsg_len.saturating_sub(libc::CMSG_LEN(0) as usize);
        let descriptors = std::slice::from_raw_parts(libc::CMSG_DATA(header).cast::<libc::c_int>(),
            bytes / size_of::<libc::c_int>());
        let mut files = descriptors.iter().map(|&fd| File::from_raw_fd(fd));
        let first = files.next();
        files.for_each(drop);
        if count != 1 || byte != 1 || msg.msg_flags & libc::MSG_CTRUNC != 0 || descriptors.len() != 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid USB descriptor packet"));
        }
        Ok(first.expect("one descriptor"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_an_open_read_only_descriptor() {
        let original = File::open(std::env::current_exe().unwrap()).unwrap();
        let (client, helper) = UnixStream::pair().unwrap();
        send(&helper, &original).unwrap();
        let file = receive(&client).unwrap();
        assert_eq!(file.metadata().unwrap().len(), original.metadata().unwrap().len());
        assert_eq!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) } & libc::O_ACCMODE, libc::O_RDONLY);
        assert_ne!(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC, 0);
    }
}
