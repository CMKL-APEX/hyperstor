//! The memory that reading a request's XML body takes, per byte of the
//! body: counted by this test's own allocator, at its peak while the
//! tree is held. Bodies are S3's largest: DeleteObjects' 1000 keys,
//! CompleteMultipartUpload's 10,000 parts, and a body of nothing but the
//! smallest elements, which costs the most per byte.

use hyperstor_s3::gateway::{COMPLETE_ELEMENTS, DELETE_ELEMENTS};
use hyperstor_s3::xml;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static NOW: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call is passed on to the system's allocator unchanged;
// only the counts are kept beside it.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        // SAFETY: as the caller's contract for `alloc`.
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            let now = NOW.fetch_add(l.size(), Ordering::Relaxed) + l.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        // SAFETY: as the caller's contract for `dealloc`.
        unsafe { System.dealloc(p, l) };
        NOW.fetch_sub(l.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

/// The tests measure one at a time: the counts are the process's.
static ONE: Mutex<()> = Mutex::new(());

/// The peak bytes allocated while reading `body` with at most `max`
/// elements, above what was allocated before; and whether it read.
fn peak(body: &str, max: usize) -> (usize, bool) {
    // A body is read once its elements are counted; the counting is in
    // `xml::parse`, which the gateway calls with DELETE_ELEMENTS or
    // COMPLETE_ELEMENTS.
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let base = NOW.load(Ordering::Relaxed);
    PEAK.store(base, Ordering::Relaxed);
    let r = xml::parse(body, max);
    let top = PEAK.load(Ordering::Relaxed);
    let ok = r.as_ref().is_ok_and(|d| d.root().children().count() > 0);
    drop(r);
    (top - base, ok)
}

fn delete_body() -> String {
    let mut b = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Delete xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><Quiet>true</Quiet>",
    );
    for i in 0..1000 {
        b.push_str(&format!(
            "<Object><Key>logs/2026/11/12/host-{i:04}.log.gz</Key></Object>"
        ));
    }
    b + "</Delete>"
}

fn complete_body() -> String {
    let mut b = String::from(
        "<CompleteMultipartUpload xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\n",
    );
    for n in 1..=10_000 {
        b.push_str(&format!(
            "  <Part>\n    <ETag>&quot;{n:032x}&quot;</ETag>\n    <ChecksumCRC32C>AAAAAA==</ChecksumCRC32C>\n    <PartNumber>{n}</PartNumber>\n  </Part>\n"
        ));
    }
    b + "</CompleteMultipartUpload>"
}

#[test]
fn reading_a_body_takes_about_its_size() {
    for (what, body, max) in [
        ("1000 keys", delete_body(), DELETE_ELEMENTS),
        ("10,000 parts", complete_body(), COMPLETE_ELEMENTS),
    ] {
        let (p, ok) = peak(&body, max);
        assert!(ok, "{what}");
        let per = p as f64 / body.len() as f64;
        eprintln!(
            "{what}: {} body bytes, {p} at the peak, {per:.2} per byte",
            body.len()
        );
        assert!(per < 3.0, "{what}: {per:.2} bytes per body byte");
    }
}

#[test]
fn the_smallest_elements_are_bounded_by_their_count() {
    // `<a/>` is 4 bytes and an element; as many as a completion may hold.
    let n = COMPLETE_ELEMENTS - 1;
    let body = format!("<D>{}</D>", "<a/>".repeat(n));
    let (p, ok) = peak(&body, COMPLETE_ELEMENTS);
    assert!(ok);
    let per_element = p as f64 / COMPLETE_ELEMENTS as f64;
    eprintln!(
        "{} body bytes of elements: {p} at the peak, {:.1} per byte, {per_element:.0} per element",
        body.len(),
        p as f64 / body.len() as f64
    );
    assert!(per_element < 100.0, "{per_element:.0} bytes per element");
    // More than allowed are refused, before the tree grows further.
    let (p, ok) = peak(&body, 1000);
    assert!(!ok);
    assert!(p < 1000 * 100, "{p} bytes to refuse");
}
