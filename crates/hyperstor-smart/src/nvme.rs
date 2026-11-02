//! NVMe admin commands through the kernel's passthrough ioctl
//! (`NVME_IOCTL_ADMIN_CMD`, linux/nvme_ioctl.h): Get Log Page for the
//! SMART / health log, and Identify Controller for its temperature
//! thresholds. Issued on the namespace's block device, which the kernel
//! routes to its controller; reading needs `CAP_SYS_ADMIN`.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;

/// `struct nvme_passthru_cmd` (72 bytes).
#[repr(C)]
#[derive(Default)]
struct PassthruCmd {
    opcode: u8,
    flags: u8,
    rsvd1: u16,
    nsid: u32,
    cdw2: u32,
    cdw3: u32,
    metadata: u64,
    addr: u64,
    metadata_len: u32,
    data_len: u32,
    cdw10: u32,
    cdw11: u32,
    cdw12: u32,
    cdw13: u32,
    cdw14: u32,
    cdw15: u32,
    timeout_ms: u32,
    result: u32,
}

/// `_IOWR('N', 0x41, struct nvme_passthru_cmd)`.
const NVME_IOCTL_ADMIN_CMD: libc::c_ulong = (3 << 30) | (72 << 16) | (0x4e << 8) | 0x41;

const GET_LOG_PAGE: u8 = 0x02;
const IDENTIFY: u8 = 0x06;
const SMART_LOG: u32 = 0x02;

/// Runs one admin command that reads `buf.len()` bytes into `buf`.
fn admin(f: &File, opcode: u8, nsid: u32, cdw10: u32, buf: &mut [u8]) -> std::io::Result<()> {
    let mut cmd = PassthruCmd {
        opcode,
        nsid,
        addr: buf.as_mut_ptr() as u64,
        data_len: buf.len() as u32,
        cdw10,
        timeout_ms: 5000,
        ..Default::default()
    };
    // SAFETY: `cmd` is a valid `nvme_passthru_cmd` for this ioctl number,
    // living across the call; its `addr` and `data_len` describe `buf`,
    // which is borrowed mutably for the call and is what the kernel writes
    // into (a read command: no data goes to the drive). The descriptor is
    // `f`'s, open for the call's length.
    let r = unsafe { libc::ioctl(f.as_raw_fd(), NVME_IOCTL_ADMIN_CMD, &mut cmd) };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if r > 0 {
        return Err(std::io::Error::other(format!(
            "the drive answered status {r:#x}"
        )));
    }
    Ok(())
}

/// The SMART / health log (512 bytes) and the controller's warning and
/// critical temperature thresholds (kelvin) of the drive at `path`.
pub fn health(path: &Path) -> std::io::Result<(Vec<u8>, u16, u16)> {
    let f = File::open(path)?;
    let mut log = vec![0u8; 512];
    // NUMDL: dwords to read, less one; the log for the whole controller.
    let cdw10 = ((512 / 4 - 1) << 16) | SMART_LOG;
    admin(&f, GET_LOG_PAGE, 0xffff_ffff, cdw10, &mut log)?;
    let mut id = vec![0u8; 4096];
    // CNS 1: Identify Controller. Its thresholds are a nicety: a drive
    // that refuses it still has a log.
    let (wc, cc) = match admin(&f, IDENTIFY, 0, 1, &mut id) {
        Ok(()) => (
            u16::from_le_bytes([id[266], id[267]]),
            u16::from_le_bytes([id[268], id[269]]),
        ),
        Err(_) => (0, 0),
    };
    Ok((log, wc, cc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_has_the_kernels_layout() {
        assert_eq!(std::mem::size_of::<PassthruCmd>(), 72);
        assert_eq!(NVME_IOCTL_ADMIN_CMD, 0xc048_4e41);
    }
}
