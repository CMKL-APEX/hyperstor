//! Writes device images for checking libblkid's Hyperstor prober
//! (packaging/libblkid, scripts/build-libblkid-deb.sh): `v2.img`, a device
//! formatted as this build formats; `v1.img`, one formatted without feature
//! flags, as builds before v0.0.8 did; and `bad.img`, `v2.img` with one byte
//! of its superblock changed, which the prober must not take.
//!
//! `cargo run -p hyperstor-engine --example blkid_images -- DIR`

use hyperstor_engine::{BlockDev, DeviceConfig, Features, FileDev, device};
use std::sync::Arc;

/// Device 1002 of the cluster with this id.
pub const DEVICE: u128 = 1002;
pub const CLUSTER: u128 = 0x0123_4567_89ab_cdef_0011_2233_4455_6677;

fn image(path: &std::path::Path, features: Features) {
    let _ = std::fs::remove_file(path);
    let dev: Arc<dyn BlockDev> =
        Arc::new(FileDev::open(path, false, Some(8 << 20)).expect("create the image"));
    device::format(
        &dev,
        &DeviceConfig {
            device_id: DEVICE,
            cluster_id: CLUSTER,
            shards: 1,
            features,
            ..Default::default()
        },
    )
    .expect("format");
}

fn main() {
    let dir = std::path::PathBuf::from(std::env::args().nth(1).expect("usage: blkid_images DIR"));
    std::fs::create_dir_all(&dir).expect("create DIR");
    image(&dir.join("v2.img"), Features::current());
    image(&dir.join("v1.img"), Features::default());
    let mut bad = std::fs::read(dir.join("v2.img")).expect("read v2.img");
    // A byte of the cluster id, in block 0 and its copy in block 1.
    bad[60] ^= 0x40;
    bad[4096 + 60] ^= 0x40;
    std::fs::write(dir.join("bad.img"), bad).expect("write bad.img");
}
