//! Kernel TLS on a data-path socket (D25; 30-data-path-tls.md §33.9, G68).
//!
//! After a handshake whose session stays ([`crate::tls::caller_kept`],
//! [`crate::tls::answerer_kept`]), the session's keys and record numbers
//! go to the kernel (`TCP_ULP tls`, then `TLS_TX` and `TLS_RX`): from then
//! on every send on the socket leaves as TLS records and every receive
//! returns plaintext, so the reactor's reads and writes stay as they are.
//!
//! Only this module calls the socket options; their structures are
//! `linux/tls.h`'s, which libc doesn't carry.

use std::io;
use std::os::fd::RawFd;

const TCP_ULP: libc::c_int = 31;
const TLS_TX: libc::c_int = 1;
const TLS_RX: libc::c_int = 2;
const TLS_1_3_VERSION: u16 = 0x0304;
const TLS_CIPHER_AES_GCM_128: u16 = 51;
const TLS_CIPHER_AES_GCM_256: u16 = 52;
const TLS_CIPHER_CHACHA20_POLY1305: u16 = 54;

#[repr(C)]
struct Info {
    version: u16,
    cipher_type: u16,
}

/// `struct tls12_crypto_info_aes_gcm_128` (TLS 1.3 too).
#[repr(C)]
struct AesGcm128 {
    info: Info,
    iv: [u8; 8],
    key: [u8; 16],
    salt: [u8; 4],
    rec_seq: [u8; 8],
}

/// `struct tls12_crypto_info_aes_gcm_256`.
#[repr(C)]
struct AesGcm256 {
    info: Info,
    iv: [u8; 8],
    key: [u8; 32],
    salt: [u8; 4],
    rec_seq: [u8; 8],
}

/// `struct tls12_crypto_info_chacha20_poly1305`.
#[repr(C)]
struct Chacha20 {
    info: Info,
    iv: [u8; 12],
    key: [u8; 32],
    rec_seq: [u8; 8],
}

/// Sets one direction's keys. The structure is wiped once the kernel has
/// copied it.
fn set<T>(fd: RawFd, dir: libc::c_int, mut info: T) -> io::Result<()> {
    let len = std::mem::size_of::<T>() as libc::socklen_t;
    // SAFETY: `info` is a `#[repr(C)]` structure of `linux/tls.h` for the
    // cipher its `info` field names, of `len` bytes, alive for the call.
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_TLS,
            dir,
            &info as *const T as *const libc::c_void,
            len,
        )
    };
    let e = io::Error::last_os_error();
    // SAFETY: `info` is plain bytes; zeroing them leaves a valid value.
    unsafe { std::ptr::write_bytes(&mut info as *mut T as *mut u8, 0, len as usize) };
    std::hint::black_box(&info);
    if r == 0 { Ok(()) } else { Err(e) }
}

fn direction(
    fd: RawFd,
    dir: libc::c_int,
    seq: u64,
    secrets: &rustls::ConnectionTrafficSecrets,
) -> io::Result<()> {
    use rustls::ConnectionTrafficSecrets as S;
    let rec_seq = seq.to_be_bytes();
    let info = |cipher_type| Info {
        version: TLS_1_3_VERSION,
        cipher_type,
    };
    match secrets {
        S::Aes128Gcm { key, iv } => {
            let (iv, k) = (iv.as_ref(), key.as_ref());
            set(
                fd,
                dir,
                AesGcm128 {
                    info: info(TLS_CIPHER_AES_GCM_128),
                    iv: iv[4..12].try_into().unwrap(),
                    key: k.try_into().map_err(|_| bad())?,
                    salt: iv[..4].try_into().unwrap(),
                    rec_seq,
                },
            )
        }
        S::Aes256Gcm { key, iv } => {
            let (iv, k) = (iv.as_ref(), key.as_ref());
            set(
                fd,
                dir,
                AesGcm256 {
                    info: info(TLS_CIPHER_AES_GCM_256),
                    iv: iv[4..12].try_into().unwrap(),
                    key: k.try_into().map_err(|_| bad())?,
                    salt: iv[..4].try_into().unwrap(),
                    rec_seq,
                },
            )
        }
        S::Chacha20Poly1305 { key, iv } => set(
            fd,
            dir,
            Chacha20 {
                info: info(TLS_CIPHER_CHACHA20_POLY1305),
                iv: iv.as_ref().try_into().map_err(|_| bad())?,
                key: key.as_ref().try_into().map_err(|_| bad())?,
                rec_seq,
            },
        ),
        _ => Err(bad()),
    }
}

fn bad() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "a cipher kernel TLS doesn't take",
    )
}

/// Hands a kept session to the kernel: `TCP_ULP tls`, then the keys of
/// each direction. On an error the connection is no longer usable (some
/// of it may be set) and must be closed.
pub fn enable(fd: RawFd, secrets: &rustls::ExtractedSecrets) -> io::Result<()> {
    ulp(fd)?;
    direction(fd, TLS_TX, secrets.tx.0, &secrets.tx.1)?;
    direction(fd, TLS_RX, secrets.rx.0, &secrets.rx.1)
}

fn ulp(fd: RawFd) -> io::Result<()> {
    let name = b"tls";
    // SAFETY: `name` is 3 bytes, alive for the call.
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_TCP,
            TCP_ULP,
            name.as_ptr() as *const libc::c_void,
            name.len() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::{Kept, answerer_kept, caller_kept};
    use hyperstor_pki::{Identity, Role};
    use hyperstor_reactor::net::{TcpListener, TcpStream};
    use hyperstor_reactor::{block_on, spawn};
    use std::os::fd::AsRawFd;
    use std::sync::Arc;

    fn configs() -> (Arc<rustls::ClientConfig>, Arc<rustls::ServerConfig>) {
        let root = hyperstor_pki::new_root("7").unwrap();
        let ca = hyperstor_pki::new_intermediate(&root, "7").unwrap();
        let leaf = |role, id: &str| {
            let who = Identity {
                cluster: "7".into(),
                role,
                id: id.into(),
            };
            hyperstor_pki::issue(&ca, &who, &[], &[], 30).unwrap()
        };
        let (c, s) = (leaf(Role::Host, "hv-1"), leaf(Role::Node, "3"));
        (
            Arc::new(hyperstor_pki::data_client_config(&c.cert, &c.key, &root.cert).unwrap()),
            Arc::new(hyperstor_pki::data_server_config(&s.cert, &s.key, &root.cert).unwrap()),
        )
    }

    /// A connected pair whose session stayed TLS: (caller, answerer).
    async fn pair() -> ((TcpStream, Kept), (TcpStream, Kept)) {
        let (client, server) = configs();
        let l = TcpListener::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = l.local_addr().unwrap();
        let accept = spawn(async move {
            let (s, _) = l.accept().await.unwrap();
            let k = answerer_kept(&s, server).await.unwrap();
            (s, k)
        });
        let c = TcpStream::connect(addr).await.unwrap();
        let k = caller_kept(&c, client).await.unwrap();
        ((c, k), accept.await)
    }

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31) ^ seed).collect()
    }

    /// G68: a kept rustls session moves to kernel TLS in both directions,
    /// and the reactor's sends, two-piece sends and receives (into plain
    /// and aligned buffers) carry frames over it unchanged, in sizes from
    /// one byte to 1 MiB, with many sends in flight.
    #[test]
    fn kernel_tls_carries_the_reactors_reads_and_writes() {
        block_on(async {
            let ((c, ck), (s, sk)) = pair().await;
            assert_eq!(ck.peer.who.id, "3");
            assert_eq!(sk.peer.who.id, "hv-1");
            eprintln!("G68: cipher {:?}", std::mem::discriminant(&ck.secrets.tx.1));
            enable(c.as_raw_fd(), &ck.secrets).unwrap();
            enable(s.as_raw_fd(), &sk.secrets).unwrap();
            let (c, s) = (std::rc::Rc::new(c), std::rc::Rc::new(s));
            let sizes = [1usize, 100, 4096, 16 << 10, 16 << 10 | 1, 64 << 10, 1 << 20];
            // The answerer echoes each frame back, each byte flipped.
            let echo = {
                let s = s.clone();
                spawn(async move {
                    for (i, &n) in sizes.iter().enumerate() {
                        let (r, b) = s.read_exact_n(n).await;
                        r.unwrap();
                        assert_eq!(b, pattern(n, i as u8), "frame {i} of {n} bytes");
                        let back: Vec<u8> = b.iter().map(|x| !x).collect();
                        let (head, tail) = back.split_at(n / 3);
                        s.write_all2(head.to_vec(), tail.to_vec()).await.unwrap();
                    }
                })
            };
            // All sent before any is read back.
            let sends: Vec<_> = sizes
                .iter()
                .enumerate()
                .map(|(i, &n)| {
                    let c = c.clone();
                    spawn(async move { c.write_all(pattern(n, i as u8)).await.0.unwrap() })
                })
                .collect();
            for h in sends {
                h.await;
            }
            for (i, &n) in sizes.iter().enumerate() {
                let (r, b) = c.read_exact_n(n).await;
                r.unwrap();
                let want: Vec<u8> = pattern(n, i as u8).iter().map(|x| !x).collect();
                assert_eq!(b, want, "echo {i} of {n} bytes");
            }
            echo.await;
        });
    }

    /// G68: what leaves the socket is TLS records, not the plaintext: the
    /// caller sends through kernel TLS, the answerer reads the raw bytes.
    #[test]
    fn what_leaves_a_kernel_tls_socket_is_tls_records() {
        block_on(async {
            let ((c, ck), (s, _)) = pair().await;
            enable(c.as_raw_fd(), &ck.secrets).unwrap();
            let msg = pattern(1000, 9);
            c.write_all(msg.clone()).await.0.unwrap();
            // One record: 5 bytes of header, then the data, its content
            // type and a 16-byte tag, all encrypted.
            let (r, raw) = s.read_exact_n(5 + 1000 + 1 + 16).await;
            r.unwrap();
            assert_eq!(
                &raw[..3],
                &[0x17, 0x03, 0x03],
                "application data, TLS 1.2 on the wire"
            );
            assert_eq!(u16::from_be_bytes([raw[3], raw[4]]), 1000 + 1 + 16);
            assert!(!raw.windows(32).any(|w| msg.windows(32).any(|m| m == w)));
        });
    }

    /// G68: an io_uring zero-copy send (`SEND_ZC`) on a kernel TLS socket.
    /// The outcome is recorded (01-implementation-notes.md): either the
    /// kernel takes it and the bytes arrive whole, or it refuses it with
    /// `EOPNOTSUPP` and the reactor must not use it there.
    #[test]
    fn a_zero_copy_send_on_kernel_tls() {
        let (std_c, data) = block_on(async {
            let ((c, ck), (s, sk)) = pair().await;
            enable(c.as_raw_fd(), &ck.secrets).unwrap();
            enable(s.as_raw_fd(), &sk.secrets).unwrap();
            (c.into_std(), s.into_std())
        });
        let n = 256 << 10;
        let buf = pattern(n, 5);
        let sender = std::thread::spawn({
            let buf = buf.clone();
            move || {
                let mut ring = io_uring::IoUring::new(8).unwrap();
                let sqe = io_uring::opcode::SendZc::new(
                    io_uring::types::Fd(std_c.as_raw_fd()),
                    buf.as_ptr(),
                    n as u32,
                )
                .build()
                .user_data(1);
                // SAFETY: `buf` and the socket outlive the operation: this
                // thread waits for both its completions below.
                unsafe { ring.submission().push(&sqe).unwrap() };
                let mut sent = 0i64;
                let mut done = false;
                let mut notified = false;
                let ts = io_uring::types::Timespec::new().sec(5);
                let args = io_uring::types::SubmitArgs::new().timespec(&ts);
                let start = std::time::Instant::now();
                while !(done && notified) && start.elapsed().as_secs() < 10 {
                    let _ = ring.submitter().submit_with_args(1, &args);
                    for cqe in ring.completion() {
                        let f = cqe.flags();
                        eprintln!("G68: SEND_ZC cqe result {} flags {f:#x}", cqe.result());
                        if io_uring::cqueue::notif(f) {
                            notified = true;
                        } else {
                            sent += cqe.result() as i64;
                            done = true;
                            notified |= !io_uring::cqueue::more(f);
                        }
                    }
                }
                assert!(done && notified, "SEND_ZC: no completion in 10 s");
                if sent < 0 {
                    // Refused: the reader gets its end of stream at once.
                    let _ = std_c.shutdown(std::net::Shutdown::Write);
                }
                (sent, std_c)
            }
        });
        // Read while it sends (loopback holds less than the send).
        let mut r = data;
        r.set_nonblocking(false).unwrap();
        r.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut got = vec![0u8; n];
        let read = std::io::Read::read_exact(&mut r, &mut got);
        let (sent, _c) = sender.join().unwrap();
        if sent > 0 {
            read.unwrap();
        }
        eprintln!("G68: SEND_ZC on kernel TLS returned {sent}");
        if sent < 0 {
            assert_eq!(
                -sent as i32,
                libc::EOPNOTSUPP,
                "refused, but not as unsupported"
            );
        } else {
            assert_eq!(sent as usize, n);
            assert_eq!(got, buf);
        }
    }
}
