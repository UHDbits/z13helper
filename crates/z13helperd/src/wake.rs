//! Self-pipe that lets producer threads interrupt the socket pump's `poll`,
//! so the pump can sleep until a deadline instead of polling on a timer.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;

static WAKER: OnceLock<UnixStream> = OnceLock::new();

/// Install the process-wide write end. Returns the nonblocking read end for
/// the pump and a second writer for signal handlers.
pub fn install() -> io::Result<(UnixStream, UnixStream)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let signal_writer = writer.try_clone()?;
    if WAKER.set(writer).is_err() {
        return Err(io::Error::other("wake pipe already installed"));
    }
    Ok((reader, signal_writer))
}

/// Wake the pump. A full pipe already holds a pending wake, and before
/// `install` there is no pump to wake.
pub fn notify() {
    if let Some(mut writer) = WAKER.get() {
        let _ = writer.write(&[1]);
    }
}

/// Discard pending wake bytes; the pump re-checks every source afterwards.
pub fn drain(mut reader: &UnixStream) {
    let mut buffer = [0_u8; 64];
    while matches!(reader.read(&mut buffer), Ok(read) if read > 0) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_wakes_the_reader_and_drain_clears_it() {
        let (reader, _signal) = install().unwrap();
        notify();
        notify();
        let mut byte = [0_u8; 1];
        assert_eq!((&reader).read(&mut byte).unwrap(), 1);
        drain(&reader);
        assert_eq!(
            (&reader).read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(install().is_err());
    }
}
