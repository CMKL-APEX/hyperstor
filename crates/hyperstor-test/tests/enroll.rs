//! A management client, as glidex's control plane is (decisions.md D60,
//! D122, D123): it enrolls with a mgmt token, manages volumes through the
//! cluster API with the certificate it got, and writes and reads volumes
//! through the data path with `hyperstorctl write-volume`/`read-volume`.

use hyperstor_test::vhost::Service;
use hyperstor_test::{Cluster, Via};
use std::path::Path;

fn token(c: &Cluster, args: &[&str]) -> String {
    let r = c.ctl(&[&["token", "create"], args].concat()).ok();
    r.out.lines().next().unwrap().trim().to_string()
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i / 4096 % 251) as u8 + 1).collect()
}

#[test]
fn a_management_client_enrolls_and_manages_volumes() {
    let c = Cluster::all_in_one("enroll", 3);
    // A mgmt token names its identity; other roles take no name.
    c.ctl(&["token", "create", "--role", "mgmt", "--name", "Bad Name"])
        .expect(1);
    c.ctl(&["token", "create", "--role", "node", "--name", "x"])
        .expect(1);
    let t = token(&c, &["--role", "mgmt", "--name", "glidex"]);
    let file = c.scratch.path("mgmt.token");
    std::fs::write(&file, &t).unwrap();
    let list = c.ctl(&["token", "list", "--json"]).ok();
    assert!(list.out.contains(r#""name":"glidex""#), "{}", list.out);
    // Enrolled: a key made here, a certificate naming the identity.
    let dir = c.scratch.path("glidex-pki");
    let e = c
        .ctl_via(
            Via::Plain,
            &[
                "enroll",
                "--file",
                file.to_str().unwrap(),
                "--out",
                dir.to_str().unwrap(),
            ],
        )
        .ok();
    assert!(e.has("/mgmt/glidex"), "{}", e.out);
    for f in ["admin.crt", "admin.key", "root.crt"] {
        assert!(dir.join(f).exists(), "{f}");
    }
    // The identity printed is the cluster's answer, and the certificate's.
    let cert = std::fs::read_to_string(dir.join("admin.crt")).unwrap();
    let who = hyperstor_pki::identity_in(&cert).unwrap();
    assert_eq!(who.role, hyperstor_pki::Role::Mgmt);
    assert!(
        e.has(&format!("enrolled as {} in cluster", who.uri())),
        "{}",
        e.out
    );
    // Single-use.
    let again = c.scratch.path("again");
    c.ctl_via(
        Via::Plain,
        &[
            "enroll",
            "--file",
            file.to_str().unwrap(),
            "--out",
            again.to_str().unwrap(),
        ],
    )
    .expect(1);
    // The identity manages volumes through the API.
    let mgmt = |args: &[&str]| c.ctl_via(Via::Tls(&dir), args);
    let made = mgmt(&["create-volume", "--size", "8M", "--name", "image"]).ok();
    let id = made.out.split_whitespace().nth(1).unwrap().to_string();
    let grown = mgmt(&["expand-volume", "--id", &id, "--size", "16M"]).ok();
    assert!(
        grown.has(&format!("is {} bytes", 16 << 20)),
        "{}",
        grown.out
    );
    mgmt(&["expand-volume", "--id", &id, "--size", "4M"]).expect(1);
    // An image written into it, zeros skipped, through the API's map
    // address; a part read back straight from the map service.
    let mut image = pattern(3 << 20);
    image[(1 << 20)..(2 << 20)].fill(0);
    image.truncate(image.len() - 100);
    let src = c.scratch.path("image.raw");
    std::fs::write(&src, &image).unwrap();
    let w = mgmt(&[
        "write-volume",
        "--id",
        &id,
        "--file",
        src.to_str().unwrap(),
        "--sparse",
    ])
    .ok();
    assert!(w.has("1048576 bytes of zeros skipped"), "{}", w.out);
    let out = c.scratch.path("back.raw");
    let size = (3u64 << 20).to_string();
    let read = |map: bool, offset: &str, size: &str, out: &Path| {
        let mut args = vec![
            "read-volume",
            "--id",
            &id,
            "--offset",
            offset,
            "--size",
            size,
            "--out",
            out.to_str().unwrap(),
        ];
        if map {
            args.extend(["--map", &c.map]);
        }
        mgmt(&args).ok();
        std::fs::read(out).unwrap()
    };
    let back = read(true, "0", &size, &out);
    let mut want = image.clone();
    want.resize(3 << 20, 0);
    assert!(back == want, "read back differs (padded with zeros)");
    // A partition table's region, edited and written back (glidex's
    // extend-root on a clone, D123), through a map service named by its
    // members as glidex has them: one that doesn't answer, then this one.
    let members = format!("127.0.0.1:1,{}", c.map);
    let head = read(false, "0", "4096", &out);
    let mut edited = head.clone();
    edited[510] = 0x55;
    let edit = c.scratch.path("head.raw");
    std::fs::write(&edit, &edited).unwrap();
    mgmt(&[
        "write-volume",
        "--id",
        &id,
        "--file",
        edit.to_str().unwrap(),
        "--map",
        &members,
    ])
    .ok();
    assert_eq!(read(true, "0", "4096", &out)[510], 0x55);
    // Not into a volume another client has attached: its lease is held.
    let svc = Service::start(&c, "enroll-vhost", &[]);
    assert!(svc.attach(&id, "attach").get("ok").is_some());
    let held = mgmt(&[
        "write-volume",
        "--id",
        &id,
        "--file",
        edit.to_str().unwrap(),
        "--map",
        &c.map,
    ]);
    assert!(held.code != 0 && held.has("attached by"), "{}", held.out);
    svc.stop();
    // Whole blocks inside the volume only.
    let odd = c.scratch.path("odd.raw");
    mgmt(&[
        "read-volume",
        "--id",
        &id,
        "--offset",
        "512",
        "--size",
        "4096",
        "--out",
        odd.to_str().unwrap(),
        "--map",
        &c.map,
    ])
    .expect(1);
}
