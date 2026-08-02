//! Minimal implementation of the GZ302EA private EC mailbox.
//!
//! Ports and command encodings match the publicly documented AsusSAIO /
//! HealthyTable mailbox used by MyASUS fan testing. This file does not derive
//! from a vendor or third-party driver.

use std::io;
use std::thread;
use std::time::Duration;

use thiserror::Error;

pub const DATA_PORT: u16 = 0x25c;
pub const COMMAND_STATUS_PORT: u16 = 0x25d;

const PREAMBLE: u8 = 0xff;
const COMMAND_VERSION: u8 = 0xbb;
const COMMAND_TABLE: u8 = 0xdd;
/// Version transactions carry a one-byte payload; without it the EC never sets OBF.
const VERSION_PAYLOAD: u8 = 0x50;
const TABLE_READ: u8 = 0x02;
const TABLE_WRITE: u8 = 0x82;
/// Table reads are three payload bytes: selector, register, pad.
const TABLE_READ_PAD: u8 = 0x00;
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
const STATUS_INPUT_FULL: u8 = 1 << 1;
/// Vendor mailbox polls ~1000 times at 100 µs (~100 ms wall timeout).
const POLL_LIMIT: u32 = 1000;
const POLL_DELAY: Duration = Duration::from_micros(100);
const TRANSACTION_RETRIES: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Register {
    Count = 0x30,
    GlobalMode = 0x31,
    FanSelect = 0x32,
    RpmLow = 0x33,
    RpmHigh = 0x34,
    Duty = 0x35,
}

pub trait PortIo {
    fn read_u8(&mut self, port: u16) -> io::Result<u8>;
    fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()>;
}

#[derive(Debug, Error)]
pub enum EcError {
    #[error("port I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("EC mailbox timed out waiting for {0}")]
    Timeout(&'static str),
    #[error("EC reports {0} fans; two are required")]
    FanCount(u8),
}

pub struct EcMailbox<P> {
    io: P,
    poll_limit: u32,
    poll_delay: Duration,
}

impl<P: PortIo> EcMailbox<P> {
    pub fn new(io: P) -> Self {
        Self {
            io,
            poll_limit: POLL_LIMIT,
            poll_delay: POLL_DELAY,
        }
    }

    #[cfg(test)]
    fn with_poll(io: P, poll_limit: u32, poll_delay: Duration) -> Self {
        Self {
            io,
            poll_limit,
            poll_delay,
        }
    }

    fn status(&mut self) -> Result<u8, EcError> {
        Ok(self.io.read_u8(COMMAND_STATUS_PORT)?)
    }

    fn wait_input_empty(&mut self) -> Result<(), EcError> {
        for _ in 0..self.poll_limit {
            if self.status()? & STATUS_INPUT_FULL == 0 {
                return Ok(());
            }
            thread::sleep(self.poll_delay);
        }
        Err(EcError::Timeout("input buffer"))
    }

    fn wait_output_full(&mut self) -> Result<(), EcError> {
        for _ in 0..self.poll_limit {
            if self.status()? & STATUS_OUTPUT_FULL != 0 {
                return Ok(());
            }
            thread::sleep(self.poll_delay);
        }
        Err(EcError::Timeout("output buffer"))
    }

    fn drain_output(&mut self) -> Result<(), EcError> {
        for _ in 0..self.poll_limit {
            if self.status()? & STATUS_OUTPUT_FULL == 0 {
                return Ok(());
            }
            let _ = self.io.read_u8(DATA_PORT)?;
            thread::sleep(self.poll_delay);
        }
        Err(EcError::Timeout("output drain"))
    }

    fn command(&mut self, value: u8) -> Result<(), EcError> {
        self.wait_input_empty()?;
        Ok(self.io.write_u8(COMMAND_STATUS_PORT, value)?)
    }

    fn data(&mut self, value: u8) -> Result<(), EcError> {
        self.wait_input_empty()?;
        Ok(self.io.write_u8(DATA_PORT, value)?)
    }

    fn transaction_once(
        &mut self,
        command: u8,
        payload: &[u8],
        want_result: bool,
    ) -> Result<Option<u8>, EcError> {
        self.drain_output()?;
        self.command(PREAMBLE)?;
        self.command(command)?;
        for byte in payload {
            self.data(*byte)?;
        }
        // The EC finishes accepting the transaction only after IBF clears.
        self.wait_input_empty()?;
        if !want_result {
            return Ok(None);
        }
        self.wait_output_full()?;
        Ok(Some(self.io.read_u8(DATA_PORT)?))
    }

    fn transaction(
        &mut self,
        command: u8,
        payload: &[u8],
        want_result: bool,
    ) -> Result<Option<u8>, EcError> {
        let mut last = EcError::Timeout("output buffer");
        for _ in 0..TRANSACTION_RETRIES {
            match self.transaction_once(command, payload, want_result) {
                Ok(value) => return Ok(value),
                Err(error) => last = error,
            }
        }
        Err(last)
    }

    pub fn version(&mut self) -> Result<u8, EcError> {
        self.transaction(COMMAND_VERSION, &[VERSION_PAYLOAD], true)?
            .ok_or(EcError::Timeout("output buffer"))
    }

    pub fn read(&mut self, register: Register) -> Result<u8, EcError> {
        self.transaction(
            COMMAND_TABLE,
            &[TABLE_READ, register as u8, TABLE_READ_PAD],
            true,
        )?
        .ok_or(EcError::Timeout("output buffer"))
    }

    pub fn write(&mut self, register: Register, value: u8) -> Result<(), EcError> {
        self.transaction(COMMAND_TABLE, &[TABLE_WRITE, register as u8, value], false)
            .map(|_| ())
    }

    pub fn probe(&mut self) -> Result<Probe, EcError> {
        let version = self.version()?;
        let fan_count = self.read(Register::Count)?;
        if fan_count != 2 {
            return Err(EcError::FanCount(fan_count));
        }
        Ok(Probe { version, fan_count })
    }

    pub fn set_global_mode(&mut self, enabled: bool) -> Result<(), EcError> {
        self.write(Register::GlobalMode, u8::from(enabled))
    }

    pub fn rpm(&mut self, fan: u8) -> Result<u16, EcError> {
        self.write(Register::FanSelect, fan)?;
        let low = self.read(Register::RpmLow)?;
        let high = self.read(Register::RpmHigh)?;
        Ok(u16::from_le_bytes([low, high]))
    }

    pub fn set_duty(&mut self, fan: u8, duty: u8) -> Result<(), EcError> {
        // Select → write duty → reselect. Mode changes have been observed to
        // clear the active fan selection; re-asserting keeps both fans honest.
        self.write(Register::FanSelect, fan)?;
        self.write(Register::Duty, duty)?;
        self.write(Register::FanSelect, fan)
    }

    pub fn into_inner(self) -> P {
        self.io
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Probe {
    pub version: u8,
    pub fan_count: u8,
}

pub struct LinuxPortIo;

impl LinuxPortIo {
    pub fn acquire() -> io::Result<Self> {
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            // SAFETY: ioperm is called for exactly the two mailbox ports. The
            // service unit grants CAP_SYS_RAWIO and no other device capability.
            if unsafe { libc::ioperm(DATA_PORT.into(), 2, 1) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self)
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "port I/O is supported only on Linux x86",
            ))
        }
    }
}

impl PortIo for LinuxPortIo {
    fn read_u8(&mut self, port: u16) -> io::Result<u8> {
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            let value: u8;
            // SAFETY: acquire() granted this process access to both accepted
            // ports, and the match prevents access outside that pair.
            if !matches!(port, DATA_PORT | COMMAND_STATUS_PORT) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "port is outside the EC mailbox",
                ));
            }
            unsafe {
                core::arch::asm!(
                    "in al, dx",
                    in("dx") port,
                    out("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            Ok(value)
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            let _ = port;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "port I/O is supported only on Linux x86",
            ))
        }
    }

    fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            if !matches!(port, DATA_PORT | COMMAND_STATUS_PORT) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "port is outside the EC mailbox",
                ));
            }
            // SAFETY: acquire() granted access and the port was restricted
            // above. The instruction has no memory side effects.
            unsafe {
                core::arch::asm!(
                    "out dx, al",
                    in("dx") port,
                    in("al") value,
                    options(nomem, nostack, preserves_flags)
                );
            }
            Ok(())
        }
        #[cfg(not(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64"))))]
        {
            let _ = (port, value);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "port I/O is supported only on Linux x86",
            ))
        }
    }
}

impl Drop for LinuxPortIo {
    fn drop(&mut self) {
        #[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
        {
            // SAFETY: revoking the same range is always safe. There is no
            // useful recovery action if the kernel rejects revocation.
            unsafe {
                libc::ioperm(DATA_PORT.into(), 2, 0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    #[derive(Debug, Eq, PartialEq)]
    enum Op {
        Read(u16),
        Write(u16, u8),
    }

    #[derive(Default)]
    struct MockIo {
        reads: VecDeque<u8>,
        ops: Vec<Op>,
    }

    impl MockIo {
        fn with_reads(reads: impl IntoIterator<Item = u8>) -> Self {
            Self {
                reads: reads.into_iter().collect(),
                ops: Vec::new(),
            }
        }
    }

    impl PortIo for MockIo {
        fn read_u8(&mut self, port: u16) -> io::Result<u8> {
            self.ops.push(Op::Read(port));
            self.reads
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "mock reads empty"))
        }

        fn write_u8(&mut self, port: u16, value: u8) -> io::Result<()> {
            self.ops.push(Op::Write(port, value));
            Ok(())
        }
    }

    #[test]
    fn version_transaction_sends_payload_and_drains() {
        // drain status clear, then IBF clears for preamble/cmd/payload/finish,
        // then OBF set + version byte.
        let io = MockIo::with_reads([0, 0, 0, 0, 0, STATUS_OUTPUT_FULL, 0x19]);
        let mut ec = EcMailbox::with_poll(io, 4, Duration::ZERO);
        assert_eq!(ec.version().unwrap(), 0x19);
        assert_eq!(
            ec.into_inner().ops,
            [
                Op::Read(COMMAND_STATUS_PORT),
                Op::Read(COMMAND_STATUS_PORT),
                Op::Write(COMMAND_STATUS_PORT, PREAMBLE),
                Op::Read(COMMAND_STATUS_PORT),
                Op::Write(COMMAND_STATUS_PORT, COMMAND_VERSION),
                Op::Read(COMMAND_STATUS_PORT),
                Op::Write(DATA_PORT, VERSION_PAYLOAD),
                Op::Read(COMMAND_STATUS_PORT),
                Op::Read(COMMAND_STATUS_PORT),
                Op::Read(DATA_PORT),
            ]
        );
    }

    #[test]
    fn table_read_includes_pad_byte() {
        // drain + preamble + cmd + 3 payload + post-payload IBF + OBF + value
        let io = MockIo::with_reads([0, 0, 0, 0, 0, 0, 0, STATUS_OUTPUT_FULL, 2]);
        let mut ec = EcMailbox::with_poll(io, 4, Duration::ZERO);
        assert_eq!(ec.read(Register::Count).unwrap(), 2);
        let writes: Vec<_> = ec
            .into_inner()
            .ops
            .into_iter()
            .filter(|op| matches!(op, Op::Write(_, _)))
            .collect();
        assert_eq!(
            writes,
            [
                Op::Write(COMMAND_STATUS_PORT, PREAMBLE),
                Op::Write(COMMAND_STATUS_PORT, COMMAND_TABLE),
                Op::Write(DATA_PORT, TABLE_READ),
                Op::Write(DATA_PORT, Register::Count as u8),
                Op::Write(DATA_PORT, TABLE_READ_PAD),
            ]
        );
    }

    #[test]
    fn table_write_uses_write_selector_and_whitelisted_register() {
        // drain + preamble + cmd + 3 payload + post-payload IBF
        let io = MockIo::with_reads([0; 7]);
        let mut ec = EcMailbox::with_poll(io, 4, Duration::ZERO);
        ec.write(Register::Duty, 204).unwrap();
        let writes: Vec<_> = ec
            .into_inner()
            .ops
            .into_iter()
            .filter(|op| matches!(op, Op::Write(_, _)))
            .collect();
        assert_eq!(
            writes,
            [
                Op::Write(COMMAND_STATUS_PORT, PREAMBLE),
                Op::Write(COMMAND_STATUS_PORT, COMMAND_TABLE),
                Op::Write(DATA_PORT, TABLE_WRITE),
                Op::Write(DATA_PORT, Register::Duty as u8),
                Op::Write(DATA_PORT, 204),
            ]
        );
    }

    #[test]
    fn busy_input_times_out_without_writing() {
        let io = MockIo::with_reads(vec![STATUS_INPUT_FULL; 8]);
        let mut ec = EcMailbox::with_poll(io, 2, Duration::ZERO);
        assert!(matches!(
            ec.version(),
            Err(EcError::Timeout("input buffer"))
        ));
        assert!(!ec
            .into_inner()
            .ops
            .iter()
            .any(|op| matches!(op, Op::Write(_, _))));
    }
}
