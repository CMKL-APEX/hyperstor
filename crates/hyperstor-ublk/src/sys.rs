//! The ublk uAPI (`include/uapi/linux/ublk_cmd.h`, Linux 6.8): the
//! structures shared with the driver and its command numbers. The FFI of
//! this crate is here and in `ctrl` and `queue`, each `unsafe` with its
//! reason.

/// Control commands, ioctl-encoded (`UBLK_U_CMD_*`).
pub const CMD_GET_DEV_INFO: u32 = ior(0x02, size_of::<CtrlCmd>());
pub const CMD_ADD_DEV: u32 = iowr(0x04, size_of::<CtrlCmd>());
pub const CMD_DEL_DEV: u32 = iowr(0x05, size_of::<CtrlCmd>());
pub const CMD_START_DEV: u32 = iowr(0x06, size_of::<CtrlCmd>());
pub const CMD_STOP_DEV: u32 = iowr(0x07, size_of::<CtrlCmd>());
pub const CMD_SET_PARAMS: u32 = iowr(0x08, size_of::<CtrlCmd>());

/// Queue commands (`UBLK_U_IO_*`).
pub const IO_FETCH_REQ: u32 = iowr(0x20, size_of::<IoCmd>());
pub const IO_COMMIT_AND_FETCH_REQ: u32 = iowr(0x21, size_of::<IoCmd>());

/// A fetch completed with a request to serve.
pub const IO_RES_OK: i32 = 0;

/// Device flags.
pub const F_CMD_IOCTL_ENCODE: u64 = 1 << 6;

/// Request operations (`UBLK_IO_OP_*`), the low byte of `op_flags`.
pub const IO_OP_READ: u8 = 0;
pub const IO_OP_WRITE: u8 = 1;
pub const IO_OP_FLUSH: u8 = 2;

pub const PARAM_TYPE_BASIC: u32 = 1;

/// Queue descriptors are mapped at this offset of `/dev/ublkcN`, one
/// area of this size per queue (`UBLKSRV_CMD_BUF_OFFSET`, and
/// `UBLK_MAX_QUEUE_DEPTH` descriptors rounded up to pages).
pub const CMD_BUF_OFFSET: u64 = 0;
pub const MAX_QUEUE_DEPTH: usize = 4096;

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | ((b'u' as u32) << 8) | nr
}

const fn ior(nr: u32, size: usize) -> u32 {
    ioc(2, nr, size)
}

const fn iowr(nr: u32, size: usize) -> u32 {
    ioc(3, nr, size)
}

/// `struct ublksrv_ctrl_cmd`, in the 80-byte command area of a 128-byte
/// entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CtrlCmd {
    pub dev_id: u32,
    /// `u16::MAX` unless the command is for one queue.
    pub queue_id: u16,
    pub len: u16,
    pub addr: u64,
    pub data: u64,
    pub dev_path_len: u16,
    pub pad: u16,
    pub reserved: u32,
}

/// `struct ublksrv_ctrl_dev_info`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct DevInfo {
    pub nr_hw_queues: u16,
    pub queue_depth: u16,
    pub state: u16,
    pub pad0: u16,
    pub max_io_buf_bytes: u32,
    pub dev_id: u32,
    pub ublksrv_pid: i32,
    pub pad1: u32,
    pub flags: u64,
    pub ublksrv_flags: u64,
    pub owner_uid: u32,
    pub owner_gid: u32,
    pub reserved1: u64,
    pub reserved2: u64,
}

/// `struct ublksrv_io_desc`: one request, written by the driver into the
/// queue's mapped area at its tag.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct IoDesc {
    pub op_flags: u32,
    pub nr_sectors: u32,
    pub start_sector: u64,
    pub addr: u64,
}

/// `struct ublksrv_io_cmd`, in the 16-byte command area of an entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct IoCmd {
    pub q_id: u16,
    pub tag: u16,
    pub result: i32,
    /// The request's buffer in this process.
    pub addr: u64,
}

impl IoCmd {
    pub fn bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..2].copy_from_slice(&self.q_id.to_ne_bytes());
        b[2..4].copy_from_slice(&self.tag.to_ne_bytes());
        b[4..8].copy_from_slice(&self.result.to_ne_bytes());
        b[8..16].copy_from_slice(&self.addr.to_ne_bytes());
        b
    }
}

impl CtrlCmd {
    pub fn bytes(&self) -> [u8; 80] {
        let mut b = [0u8; 80];
        b[0..4].copy_from_slice(&self.dev_id.to_ne_bytes());
        b[4..6].copy_from_slice(&self.queue_id.to_ne_bytes());
        b[6..8].copy_from_slice(&self.len.to_ne_bytes());
        b[8..16].copy_from_slice(&self.addr.to_ne_bytes());
        b[16..24].copy_from_slice(&self.data.to_ne_bytes());
        b[24..26].copy_from_slice(&self.dev_path_len.to_ne_bytes());
        b
    }
}

/// `struct ublk_param_basic`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ParamBasic {
    pub attrs: u32,
    pub logical_bs_shift: u8,
    pub physical_bs_shift: u8,
    pub io_opt_shift: u8,
    pub io_min_shift: u8,
    pub max_sectors: u32,
    pub chunk_sectors: u32,
    pub dev_sectors: u64,
    pub virt_boundary_mask: u64,
}

/// `struct ublk_param_discard`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ParamDiscard {
    pub discard_alignment: u32,
    pub discard_granularity: u32,
    pub max_discard_sectors: u32,
    pub max_write_zeroes_sectors: u32,
    pub max_discard_segments: u16,
    pub reserved0: u16,
}

/// `struct ublk_param_devt`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ParamDevt {
    pub char_major: u32,
    pub char_minor: u32,
    pub disk_major: u32,
    pub disk_minor: u32,
}

/// `struct ublk_param_zoned`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct ParamZoned {
    pub max_open_zones: u32,
    pub max_active_zones: u32,
    pub max_zone_append_sectors: u32,
    pub reserved: [u8; 20],
}

/// `struct ublk_params`.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Params {
    pub len: u32,
    pub types: u32,
    pub basic: ParamBasic,
    pub discard: ParamDiscard,
    pub devt: ParamDevt,
    pub zoned: ParamZoned,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::offset_of;

    /// Sizes and offsets as the C header lays them out, and command numbers
    /// as its `_IOR`/`_IOWR` macros compute them.
    #[test]
    fn layouts_and_command_numbers_match_the_header() {
        assert_eq!(size_of::<CtrlCmd>(), 32);
        assert_eq!(offset_of!(CtrlCmd, addr), 8);
        assert_eq!(offset_of!(CtrlCmd, dev_path_len), 24);
        assert_eq!(size_of::<DevInfo>(), 64);
        assert_eq!(offset_of!(DevInfo, flags), 24);
        assert_eq!(offset_of!(DevInfo, owner_uid), 40);
        assert_eq!(size_of::<IoDesc>(), 24);
        assert_eq!(size_of::<IoCmd>(), 16);
        assert_eq!(size_of::<ParamBasic>(), 32);
        assert_eq!(size_of::<ParamDiscard>(), 20);
        assert_eq!(size_of::<ParamDevt>(), 16);
        assert_eq!(size_of::<ParamZoned>(), 32);
        assert_eq!(offset_of!(Params, basic), 8);
        assert_eq!(offset_of!(Params, discard), 40);
        assert_eq!(offset_of!(Params, devt), 60);
        assert_eq!(offset_of!(Params, zoned), 76);
        assert_eq!(size_of::<Params>(), 112);
        assert_eq!(CMD_ADD_DEV, 0xc020_7504);
        assert_eq!(CMD_GET_DEV_INFO, 0x8020_7502);
        assert_eq!(IO_FETCH_REQ, 0xc010_7520);
        assert_eq!(IO_COMMIT_AND_FETCH_REQ, 0xc010_7521);
    }

    #[test]
    fn commands_encode_in_field_order() {
        let c = IoCmd {
            q_id: 1,
            tag: 2,
            result: -5,
            addr: 0x1122_3344_5566_7788,
        };
        // SAFETY: IoCmd is repr(C), 16 bytes, no padding.
        let raw: [u8; 16] = unsafe { std::mem::transmute(c) };
        assert_eq!(c.bytes(), raw);
        let k = CtrlCmd {
            dev_id: 3,
            queue_id: u16::MAX,
            len: 64,
            addr: 0xdead_beef,
            data: 42,
            dev_path_len: 0,
            pad: 0,
            reserved: 0,
        };
        // SAFETY: CtrlCmd is repr(C), 32 bytes, no padding.
        let raw: [u8; 32] = unsafe { std::mem::transmute(k) };
        assert_eq!(&k.bytes()[..32], &raw);
        assert!(k.bytes()[32..].iter().all(|b| *b == 0));
    }
}
