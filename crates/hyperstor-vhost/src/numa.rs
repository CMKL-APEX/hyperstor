//! The host's NUMA nodes and CPUs, and the node a guest's memory is bound
//! to (04-decisions.md D290, D292; host check G59).
//!
//! The reactors' CPUs come from `--cpus`, or by default the first CPU of
//! each node with CPUs, which is the core glidex reserves for the host
//! there by default (glidex host-topology.md H7). A guest's node is the
//! memory policy of its memory as `hyperstor-vhost` maps it
//! (`get_mempolicy(MPOL_F_ADDR)`): the hypervisor binds a memfd with
//! `mbind`, and the policy is the memfd's, seen through every mapping of
//! it (G59, 02-benchmarks.md 2026-11-11).

use std::path::Path;

/// The CPUs of a kernel CPU list (`0-3,8,10-11`), in order, without
/// repeats.
pub fn parse_cpulist(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (a, b),
            None => (part, part),
        };
        let a: usize = a.trim().parse().map_err(|_| format!("{part}: not a CPU"))?;
        let b: usize = b.trim().parse().map_err(|_| format!("{part}: not a CPU"))?;
        if b < a || b >= 1 << 16 {
            return Err(format!("{part}: not a CPU range"));
        }
        for c in a..=b {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// The NUMA nodes under `sys` (`/sys/devices/system/node`) and each one's
/// CPUs, by node; nodes without CPUs are left out.
pub fn nodes(sys: &Path) -> Vec<(u32, Vec<usize>)> {
    let Ok(dir) = std::fs::read_dir(sys) else {
        return Vec::new();
    };
    let mut out: Vec<(u32, Vec<usize>)> = dir
        .filter_map(|e| {
            let e = e.ok()?;
            let n: u32 = e.file_name().to_str()?.strip_prefix("node")?.parse().ok()?;
            let list = std::fs::read_to_string(e.path().join("cpulist")).ok()?;
            let cpus = parse_cpulist(&list).ok()?;
            (!cpus.is_empty()).then_some((n, cpus))
        })
        .collect();
    out.sort();
    out
}

/// The kernel's node directory.
pub const SYS_NODES: &str = "/sys/devices/system/node";

/// The node `cpu` is on, if the host says.
pub fn node_of(nodes: &[(u32, Vec<usize>)], cpu: usize) -> Option<u32> {
    nodes
        .iter()
        .find(|(_, cpus)| cpus.contains(&cpu))
        .map(|(n, _)| *n)
}

/// The reactors' CPUs by default (D290): the first CPU of each node with
/// CPUs. None when the host doesn't list its nodes: one reactor, not
/// pinned.
pub fn default_cpus(nodes: &[(u32, Vec<usize>)]) -> Vec<usize> {
    nodes
        .iter()
        .filter_map(|(_, cpus)| cpus.iter().min().copied())
        .collect()
}

/// A guest memory region's policy, as far as placing its disk goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Policy {
    /// Bound or preferred to these nodes.
    Nodes(Vec<u32>),
    /// The default (local allocation) or interleaved: no one node.
    Any,
}

impl Policy {
    /// The one node the memory is on, if it is bound to one.
    pub fn node(&self) -> Option<u32> {
        match self {
            Policy::Nodes(n) if n.len() == 1 => Some(n[0]),
            _ => None,
        }
    }
}

const MPOL_PREFERRED: i32 = 1;
const MPOL_BIND: i32 = 2;
const MPOL_PREFERRED_MANY: i32 = 5;
const MPOL_F_ADDR: libc::c_ulong = 1 << 1;
/// Nodes a policy's mask holds at most, as the kernel's largest
/// `CONFIG_NODES_SHIFT` (10).
const MAX_NODES: usize = 1024;

/// The memory policy of the mapping at `addr` in this process
/// (`get_mempolicy(MPOL_F_ADDR)`): for a shared mapping of a memfd, the
/// memfd's own, which `mbind` set through any mapping of it (G59).
pub fn policy_at(addr: usize) -> std::io::Result<Policy> {
    let mut mode: libc::c_int = 0;
    let mut mask = [0 as libc::c_ulong; MAX_NODES / libc::c_ulong::BITS as usize];
    // SAFETY: get_mempolicy writes one int to `mode` and at most
    // MAX_NODES bits to `mask`, a local of that size; `addr` is only
    // looked up, never dereferenced.
    let r = unsafe {
        libc::syscall(
            libc::SYS_get_mempolicy,
            &mut mode as *mut libc::c_int,
            mask.as_mut_ptr(),
            MAX_NODES as libc::c_ulong,
            addr,
            MPOL_F_ADDR,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // Flags (MPOL_F_STATIC_NODES and the like) ride in the high bits.
    Ok(match mode & 0xff {
        MPOL_BIND | MPOL_PREFERRED | MPOL_PREFERRED_MANY => Policy::Nodes(mask_nodes(&mask)),
        _ => Policy::Any,
    })
}

fn mask_nodes(mask: &[libc::c_ulong]) -> Vec<u32> {
    let bits = libc::c_ulong::BITS as usize;
    (0..mask.len() * bits)
        .filter(|i| mask[i / bits] & (1 << (i % bits)) != 0)
        .map(|i| i as u32)
        .collect()
}

/// Where a disk's connection goes (D292): the reactor on the node its
/// guest's memory is bound to; otherwise, or when no reactor is on that
/// node, the least loaded (the first of equals). `reactors` are the
/// reactors' nodes, `load` the connections each serves.
pub fn place(policy: &Policy, reactors: &[Option<u32>], load: &[usize]) -> usize {
    if let Some(n) = policy.node()
        && let Some(r) = (0..reactors.len())
            .filter(|&r| reactors[r] == Some(n))
            .min_by_key(|&r| load[r])
    {
        return r;
    }
    (0..reactors.len()).min_by_key(|&r| load[r]).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_parse_and_bad_ones_say_why() {
        assert_eq!(
            parse_cpulist("0-3,8,10-11\n"),
            Ok(vec![0, 1, 2, 3, 8, 10, 11])
        );
        assert_eq!(parse_cpulist("5,5,4"), Ok(vec![5, 4]));
        assert_eq!(parse_cpulist(""), Ok(vec![]));
        assert!(parse_cpulist("3-1").is_err());
        assert!(parse_cpulist("a").unwrap_err().contains("not a CPU"));
    }

    #[test]
    fn the_first_cpu_of_each_node_with_cpus_is_the_default() {
        let d = std::env::temp_dir().join(format!("hs-numa-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        for (n, cpus) in [(0, "0-3,8-11"), (1, "4-7,12-15"), (2, "")] {
            let nd = d.join(format!("node{n}"));
            std::fs::create_dir_all(&nd).unwrap();
            std::fs::write(nd.join("cpulist"), format!("{cpus}\n")).unwrap();
        }
        std::fs::create_dir_all(d.join("power")).unwrap();
        let nodes = nodes(&d);
        std::fs::remove_dir_all(&d).unwrap();
        assert_eq!(nodes.len(), 2, "{nodes:?}");
        assert_eq!(default_cpus(&nodes), vec![0, 4]);
        assert_eq!(node_of(&nodes, 13), Some(1));
        assert_eq!(node_of(&nodes, 99), None);
        assert!(default_cpus(&[]).is_empty());
    }

    #[test]
    fn a_disk_goes_to_its_guests_node_or_the_least_loaded() {
        let reactors = [Some(0), Some(1), Some(1)];
        let bound = |n| Policy::Nodes(vec![n]);
        assert_eq!(place(&bound(1), &reactors, &[0, 3, 1]), 2);
        assert_eq!(place(&bound(0), &reactors, &[9, 0, 0]), 0);
        // No reactor on its node, several nodes, or none: least loaded.
        assert_eq!(place(&bound(5), &reactors, &[2, 1, 1]), 1);
        assert_eq!(place(&Policy::Nodes(vec![0, 1]), &reactors, &[2, 3, 1]), 2);
        assert_eq!(place(&Policy::Any, &reactors, &[0, 0, 0]), 0);
        assert_eq!(place(&Policy::Any, &[None], &[4]), 0);
    }

    #[test]
    fn a_memfds_binding_is_seen_through_another_mapping() {
        let nodes = nodes(Path::new(SYS_NODES));
        // SAFETY: memfd_create with a static name; mmap of a fresh memfd
        // of that size; mbind on that mapping with a local mask; munmap of
        // the mappings made here.
        unsafe {
            let fd = libc::memfd_create(c"hs-numa".as_ptr(), libc::MFD_CLOEXEC);
            assert!(fd >= 0);
            let len = 2 << 20;
            assert_eq!(libc::ftruncate(fd, len as libc::off_t), 0);
            let map = |fd| {
                let p = libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                );
                assert_ne!(p, libc::MAP_FAILED);
                p
            };
            let a = map(fd);
            let b = map(fd);
            assert_eq!(policy_at(b as usize).unwrap(), Policy::Any);
            let node = nodes.last().map_or(0, |n| n.0);
            let mut mask = [0 as libc::c_ulong; 16];
            mask[node as usize / 64] |= 1 << (node % 64);
            let r = libc::syscall(
                libc::SYS_mbind,
                a,
                len,
                MPOL_BIND,
                mask.as_ptr(),
                1024 as libc::c_ulong,
                0 as libc::c_uint,
            );
            assert_eq!(r, 0, "{}", std::io::Error::last_os_error());
            assert_eq!(policy_at(b as usize).unwrap(), Policy::Nodes(vec![node]));
            libc::munmap(a, len);
            libc::munmap(b, len);
            libc::close(fd);
        }
    }
}
