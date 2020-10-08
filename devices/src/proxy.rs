// Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Runs hardware devices in child processes.

use std::fmt::{self, Display};
use std::time::Duration;
use std::{self, io};

use base::{error, net::UnixSeqpacket, AsRawDescriptor, RawDescriptor};
use libc::{self, pid_t};
use minijail::{self, Minijail};
use msg_socket::{MsgOnSocket, MsgReceiver, MsgSender, MsgSocket};

use crate::{BusAccessInfo, BusDevice};

/// Errors for proxy devices.
#[derive(Debug)]
pub enum Error {
    ForkingJail(minijail::Error),
    Io(io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::Error::*;

        match self {
            ForkingJail(e) => write!(f, "Failed to fork jail process: {}", e),
            Io(e) => write!(f, "IO error configuring proxy device {}.", e),
        }
    }
}

const SOCKET_TIMEOUT_MS: u64 = 2000;

#[derive(Debug, MsgOnSocket)]
enum Command {
    Read {
        len: u32,
        info: BusAccessInfo,
    },
    Write {
        len: u32,
        info: BusAccessInfo,
        data: [u8; 8],
    },
    ReadConfig(u32),
    WriteConfig {
        reg_idx: u32,
        offset: u32,
        len: u32,
        data: [u8; 4],
    },
    Shutdown,
}

#[derive(MsgOnSocket)]
enum CommandResult {
    Ok,
    ReadResult([u8; 8]),
    ReadConfigResult(u32),
}

fn child_proc<D: BusDevice>(sock: UnixSeqpacket, device: &mut D) {
    let mut running = true;
    let sock = MsgSocket::<CommandResult, Command>::new(sock);

    while running {
        let cmd = match sock.recv() {
            Ok(cmd) => cmd,
            Err(err) => {
                error!("child device process failed recv: {}", err);
                break;
            }
        };

        let res = match cmd {
            Command::Read { len, info } => {
                let mut buffer = [0u8; 8];
                device.read(info, &mut buffer[0..len as usize]);
                sock.send(&CommandResult::ReadResult(buffer))
            }
            Command::Write { len, info, data } => {
                let len = len as usize;
                device.write(info, &data[0..len]);
                // Command::Write does not have a result.
                Ok(())
            }
            Command::ReadConfig(idx) => {
                let val = device.config_register_read(idx as usize);
                sock.send(&CommandResult::ReadConfigResult(val))
            }
            Command::WriteConfig {
                reg_idx,
                offset,
                len,
                data,
            } => {
                let len = len as usize;
                device.config_register_write(reg_idx as usize, offset as u64, &data[0..len]);
                // Command::WriteConfig does not have a result.
                Ok(())
            }
            Command::Shutdown => {
                running = false;
                sock.send(&CommandResult::Ok)
            }
        };
        if let Err(e) = res {
            error!("child device process failed send: {}", e);
        }
    }
}

/// Wraps an inner `BusDevice` that is run inside a child process via fork.
///
/// Because forks are very unfriendly to destructors and all memory mappings and file descriptors
/// are inherited, this should be used as early as possible in the main process.
pub struct ProxyDevice {
    sock: MsgSocket<Command, CommandResult>,
    pid: pid_t,
    debug_label: String,
}

impl ProxyDevice {
    /// Takes the given device and isolates it into another process via fork before returning.
    ///
    /// The forked process will automatically be terminated when this is dropped, so be sure to keep
    /// a reference.
    ///
    /// # Arguments
    /// * `device` - The device to isolate to another process.
    /// * `jail` - The jail to use for isolating the given device.
    /// * `keep_rds` - File descriptors that will be kept open in the child.
    pub fn new<D: BusDevice>(
        mut device: D,
        jail: &Minijail,
        mut keep_rds: Vec<RawDescriptor>,
    ) -> Result<ProxyDevice> {
        let debug_label = device.debug_label();
        let (child_sock, parent_sock) = UnixSeqpacket::pair().map_err(Error::Io)?;

        keep_rds.push(child_sock.as_raw_descriptor());
        // Forking here is safe as long as the program is still single threaded.
        let pid = unsafe {
            match jail.fork(Some(&keep_rds)).map_err(Error::ForkingJail)? {
                0 => {
                    device.on_sandboxed();
                    child_proc(child_sock, &mut device);

                    // We're explicitly not using std::process::exit here to avoid the cleanup of
                    // stdout/stderr globals. This can cause cascading panics and SIGILL if a worker
                    // thread attempts to log to stderr after at_exit handlers have been run.
                    // TODO(crbug.com/992494): Remove this once device shutdown ordering is clearly
                    // defined.
                    //
                    // exit() is trivially safe.
                    // ! Never returns
                    libc::exit(0);
                }
                p => p,
            }
        };

        parent_sock
            .set_write_timeout(Some(Duration::from_millis(SOCKET_TIMEOUT_MS)))
            .map_err(Error::Io)?;
        parent_sock
            .set_read_timeout(Some(Duration::from_millis(SOCKET_TIMEOUT_MS)))
            .map_err(Error::Io)?;
        Ok(ProxyDevice {
            sock: MsgSocket::<Command, CommandResult>::new(parent_sock),
            pid,
            debug_label,
        })
    }

    pub fn pid(&self) -> pid_t {
        self.pid
    }

    /// Send a command that does not expect a response from the child device process.
    fn send_no_result(&self, cmd: &Command) {
        let res = self.sock.send(cmd);
        if let Err(e) = res {
            error!(
                "failed write to child device process {}: {}",
                self.debug_label, e,
            );
        }
    }

    /// Send a command and read its response from the child device process.
    fn sync_send(&self, cmd: &Command) -> Option<CommandResult> {
        self.send_no_result(cmd);
        match self.sock.recv() {
            Err(e) => {
                error!(
                    "failed to read result of {:?} from child device process {}: {}",
                    cmd, self.debug_label, e,
                );
                None
            }
            Ok(r) => Some(r),
        }
    }
}

impl BusDevice for ProxyDevice {
    fn debug_label(&self) -> String {
        self.debug_label.clone()
    }

    fn config_register_write(&mut self, reg_idx: usize, offset: u64, data: &[u8]) {
        let len = data.len() as u32;
        let mut buffer = [0u8; 4];
        buffer[0..data.len()].clone_from_slice(data);
        let reg_idx = reg_idx as u32;
        let offset = offset as u32;
        self.send_no_result(&Command::WriteConfig {
            reg_idx,
            offset,
            len,
            data: buffer,
        });
    }

    fn config_register_read(&self, reg_idx: usize) -> u32 {
        let res = self.sync_send(&Command::ReadConfig(reg_idx as u32));
        if let Some(CommandResult::ReadConfigResult(val)) = res {
            val
        } else {
            0
        }
    }

    fn read(&mut self, info: BusAccessInfo, data: &mut [u8]) {
        let len = data.len() as u32;
        if let Some(CommandResult::ReadResult(buffer)) =
            self.sync_send(&Command::Read { len, info })
        {
            let len = data.len();
            data.clone_from_slice(&buffer[0..len]);
        }
    }

    fn write(&mut self, info: BusAccessInfo, data: &[u8]) {
        let mut buffer = [0u8; 8];
        let len = data.len() as u32;
        buffer[0..data.len()].clone_from_slice(data);
        self.send_no_result(&Command::Write {
            len,
            info,
            data: buffer,
        });
    }
}

impl Drop for ProxyDevice {
    fn drop(&mut self) {
        self.sync_send(&Command::Shutdown);
    }
}

/// Note: These tests must be run with --test-threads=1 to allow minijail to fork
/// the process.
#[cfg(test)]
mod tests {
    use super::*;

    /// A simple test echo device that outputs the same u8 that was written to it.
    struct EchoDevice {
        data: u8,
        config: u8,
    }
    impl EchoDevice {
        fn new() -> EchoDevice {
            EchoDevice { data: 0, config: 0 }
        }
    }
    impl BusDevice for EchoDevice {
        fn debug_label(&self) -> String {
            "EchoDevice".to_owned()
        }

        fn write(&mut self, _info: BusAccessInfo, data: &[u8]) {
            assert!(data.len() == 1);
            self.data = data[0];
        }

        fn read(&mut self, _info: BusAccessInfo, data: &mut [u8]) {
            assert!(data.len() == 1);
            data[0] = self.data;
        }

        fn config_register_write(&mut self, _reg_idx: usize, _offset: u64, data: &[u8]) {
            assert!(data.len() == 1);
            self.config = data[0];
        }

        fn config_register_read(&self, _reg_idx: usize) -> u32 {
            self.config as u32
        }
    }

    fn new_proxied_echo_device() -> ProxyDevice {
        let device = EchoDevice::new();
        let keep_fds: Vec<RawDescriptor> = Vec::new();
        let minijail = Minijail::new().unwrap();
        ProxyDevice::new(device, &minijail, keep_fds).unwrap()
    }

    #[test]
    fn test_debug_label() {
        let proxy_device = new_proxied_echo_device();
        assert_eq!(proxy_device.debug_label(), "EchoDevice");
    }

    #[test]
    fn test_proxied_read_write() {
        let mut proxy_device = new_proxied_echo_device();
        let address = BusAccessInfo {
            offset: 0,
            address: 0,
            id: 0,
        };
        proxy_device.write(address, &[42]);
        let mut read_buffer = [0];
        proxy_device.read(address, &mut read_buffer);
        assert_eq!(read_buffer, [42]);
    }

    #[test]
    fn test_proxied_config() {
        let mut proxy_device = new_proxied_echo_device();
        proxy_device.config_register_write(0, 0, &[42]);
        assert_eq!(proxy_device.config_register_read(0), 42);
    }
}
