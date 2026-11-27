//! G69: the cost of encrypting the data path on a reactor
//! (30-data-path-tls.md §33.10, §33.11; D407).
//!
//! `tls-certs DIR` writes a throwaway CA and two leaves to `DIR`;
//! `tls-serve ADDR DIR` answers on `ADDR` as a storage node does, on one
//! reactor; `tls ADDR DIR` calls it from another host, and for each of
//! plaintext after the handshake (a network marked trusted) and kernel
//! TLS, writes and reads of 4 KiB and 1 MiB at queue depths 1 and 16 on one
//! connection: requests a second, MB/s, the median and 99th percentile,
//! and the CPU each end spent per GB moved.
//!
//! Requests are `Ping`s, whose answer a node never looks at: a write
//! carries its data and is answered `Pong`; a read carries the size it
//! wants (8 bytes) and is answered `Pong` with that much. `u64::MAX` asks
//! the server's CPU time so far.

use hyperstor_client::rpc::{self, Answering, CallerTls, Conn, Frame, TlsIdentity};
use hyperstor_proto::Msg;
use hyperstor_reactor::net::{TcpListener, TcpStream};
use hyperstor_reactor::{block_on, spawn};
use std::net::SocketAddr;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

const CPU_ASK: u64 = u64::MAX;

/// User and system CPU time of this process so far.
fn cpu() -> Duration {
    // SAFETY: getrusage fills the zeroed structure it is given.
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `u` is a valid rusage for the call.
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let t = |v: libc::timeval| Duration::new(v.tv_sec as u64, v.tv_usec as u32 * 1000);
    t(u.ru_utime) + t(u.ru_stime)
}

fn identity(dir: &Path, name: &str) -> TlsIdentity {
    let read = |f: &str| std::fs::read_to_string(dir.join(f)).expect("tls-certs DIR first");
    TlsIdentity::from_pem(
        &read(&format!("{name}.crt")),
        &read(&format!("{name}.key")),
        &read("root.crt"),
    )
    .unwrap()
}

/// `tls-certs DIR`: a CA for this benchmark alone, and a `node` and a
/// `host` certificate from it. Nothing of a cluster's.
pub fn certs(dir: &Path) {
    use hyperstor_pki::{Identity, Role};
    std::fs::create_dir_all(dir).unwrap();
    let root = hyperstor_pki::new_root("0").unwrap();
    let ca = hyperstor_pki::new_intermediate(&root, "0").unwrap();
    std::fs::write(dir.join("root.crt"), &root.cert).unwrap();
    for (name, role) in [("node", Role::Node), ("host", Role::Host)] {
        let who = Identity {
            cluster: "0".into(),
            role,
            id: "bench".into(),
        };
        let leaf = hyperstor_pki::issue(&ca, &who, &[], &[], 7).unwrap();
        std::fs::write(
            dir.join(format!("{name}.crt")),
            format!("{}{}", leaf.cert, ca.cert),
        )
        .unwrap();
        std::fs::write(dir.join(format!("{name}.key")), &leaf.key).unwrap();
    }
    println!(
        "wrote {}: root.crt, node.crt/.key, host.crt/.key",
        dir.display()
    );
}

/// `tls-serve ADDR DIR`: serves callers until killed, on one reactor.
pub fn serve(addr: SocketAddr, dir: &Path) {
    let id = identity(dir, "node");
    if let Some(&c) = crate::cpus::for_run(1).first() {
        let _ = hyperstor_reactor::pin_to_cpu(c);
        eprintln!("tls-serve: on CPU {c}");
    }
    block_on(async move {
        let l = TcpListener::bind(addr).unwrap();
        eprintln!("tls-serve: listening on {addr}");
        loop {
            let (s, _) = l.accept().await.unwrap();
            spawn(serve_one(s, id.answering(false, false)));
        }
    });
}

async fn serve_one(s: TcpStream, tls: Answering) {
    let s = Rc::new(s);
    let (tx, rx) = hyperstor_reactor::chan::channel::<Frame>();
    spawn(rpc::writer(s.clone(), rx));
    let data = vec![0x5au8; 1 << 20];
    let tls = Some(tls);
    loop {
        let Ok((h, msg, payload)) = rpc::read_request(&s, None).await else {
            return;
        };
        let reply = match msg {
            Msg::Hello { .. } => Frame::new(
                h.req,
                &Msg::HelloOk {
                    version: hyperstor_proto::VERSION,
                    node: 0,
                    map_epoch: 1,
                },
                &[],
            ),
            Msg::StartTls | Msg::StartTlsOffer { .. } => {
                let offer = match msg {
                    Msg::StartTlsOffer { encrypt } => Some(encrypt),
                    _ => None,
                };
                let no = Msg::Error {
                    code: hyperstor_proto::Code::Invalid,
                    map_epoch: 1,
                    detail: "no".into(),
                };
                if rpc::answer_start_tls(&s, &tx, h.req, &tls, no, offer)
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
            _ if payload.len() == 8 => {
                let n = u64::from_le_bytes(payload[..8].try_into().unwrap());
                if n == CPU_ASK {
                    Frame::new(h.req, &Msg::Pong, &(cpu().as_nanos() as u64).to_le_bytes())
                } else {
                    Frame::new(h.req, &Msg::Pong, &data[..(n as usize).min(data.len())])
                }
            }
            _ => Frame::new(h.req, &Msg::Pong, &[]),
        };
        if tx.send(reply).is_err() {
            return;
        }
    }
}

/// One run's results.
struct Run {
    ops: u64,
    secs: f64,
    lat: Vec<Duration>,
    client_cpu: Duration,
    server_cpu: Duration,
}

async fn server_cpu(c: &Conn) -> Duration {
    let (_, p) = c.start(&Msg::Ping, &CPU_ASK.to_le_bytes()).await.unwrap();
    Duration::from_nanos(u64::from_le_bytes(p[..8].try_into().unwrap()))
}

async fn run(c: &Rc<Conn>, size: usize, write: bool, qd: usize, secs: f64) -> Run {
    let body: Rc<Vec<u8>> = Rc::new(if write {
        vec![0xa5; size]
    } else {
        (size as u64).to_le_bytes().to_vec()
    });
    // Warm up, then measure.
    for _ in 0..qd * 4 {
        c.start(&Msg::Ping, &body).await.unwrap();
    }
    let (c0, s0) = (cpu(), server_cpu(c).await);
    let start = Instant::now();
    let until = start + Duration::from_secs_f64(secs);
    let workers: Vec<_> = (0..qd)
        .map(|_| {
            let (c, body) = (c.clone(), body.clone());
            spawn(async move {
                let mut lat = Vec::new();
                while Instant::now() < until {
                    let t = Instant::now();
                    let (_, p) = c.start(&Msg::Ping, &body).await.unwrap();
                    lat.push(t.elapsed());
                    assert!(write || p.len() == size);
                }
                lat
            })
        })
        .collect();
    let mut lat = Vec::new();
    for w in workers {
        lat.extend(w.await);
    }
    let secs = start.elapsed().as_secs_f64();
    let (c1, s1) = (cpu(), server_cpu(c).await);
    lat.sort();
    Run {
        ops: lat.len() as u64,
        secs,
        lat,
        client_cpu: c1 - c0,
        server_cpu: s1 - s0,
    }
}

/// `tls ADDR DIR`: the table of G69.
pub fn client(addr: SocketAddr, dir: &Path, secs: f64) {
    let id = identity(dir, "host");
    if let Some(&c) = crate::cpus::for_run(1).first() {
        let _ = hyperstor_reactor::pin_to_cpu(c);
        eprintln!("tls: on CPU {c}");
    }
    println!(
        "| Session | Op | Size | QD | ops/s | MB/s | p50 µs | p99 µs | client CPU s/GB | server CPU s/GB |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|");
    block_on(async move {
        let hello = Msg::Hello {
            min_version: hyperstor_proto::OLDEST_VERSION,
            max_version: hyperstor_proto::VERSION,
            id: 1,
            instance: 1,
        };
        for (name, encrypt) in [("plaintext", false), ("kernel TLS", true)] {
            let tls = CallerTls {
                config: id.caller.clone(),
                required: true,
                encrypt,
            };
            let (c, _) = Conn::open(addr, Duration::from_secs(5), &hello, Some(&tls))
                .await
                .unwrap();
            assert_eq!(c.encrypted(), encrypt);
            let c = Rc::new(c);
            for (size, label) in [(4096usize, "4 KiB"), (1 << 20, "1 MiB")] {
                for write in [true, false] {
                    for qd in [1usize, 16] {
                        let r = run(&c, size, write, qd, secs).await;
                        let gb = (r.ops as f64 * size as f64) / 1e9;
                        let pct = |p: f64| {
                            r.lat[((r.lat.len() as f64 * p) as usize).min(r.lat.len() - 1)]
                                .as_secs_f64()
                                * 1e6
                        };
                        println!(
                            "| {name} | {} | {label} | {qd} | {:.0} | {:.0} | {:.0} | {:.0} | {:.2} | {:.2} |",
                            if write { "write" } else { "read" },
                            r.ops as f64 / r.secs,
                            gb * 1e3 / r.secs,
                            pct(0.5),
                            pct(0.99),
                            r.client_cpu.as_secs_f64() / gb,
                            r.server_cpu.as_secs_f64() / gb,
                        );
                    }
                }
            }
            c.kill();
        }
    });
}
