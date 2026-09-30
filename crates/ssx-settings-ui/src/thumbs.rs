//! Lazily decoded, bounded thumbnails for the history grid.
//!
//! Two parts:
//!
//! * [`Cache`]: an LRU map from entry id to a state (`Pending`, `Ready(T)`, `Missing`). It is
//!   generic over `T` (the GPU texture in the window, a number in the tests) and bounded: when
//!   it grows past its capacity the least recently *used* entry is dropped, preferring ones
//!   that are not still being decoded. The grid asks for the visible entries only, so what is
//!   scrolled away is what gets evicted.
//! * [`Loader`]: two worker threads that turn a request into RGBA pixels. The stored PNG
//!   thumbnail is read from the database (`History::get`, on the worker, so the UI thread
//!   never waits for SQLite); an entry without one falls back to decoding and shrinking the
//!   local image file, with size limits so a stray 200 MB file cannot stall a worker.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{self, Receiver, Sender},
    },
};

use ssx_core::history::{EntryKind, History};

/// Longest edge of a decoded thumbnail in pixels (what the grid can use).
pub const MAX_EDGE: u32 = 320;
/// Largest image file decoded as a fallback thumbnail.
pub const MAX_FALLBACK_BYTES: u64 = 40 * 1024 * 1024;

/// What the cache knows about one entry's thumbnail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum State<T> {
    /// A worker is on it.
    Pending,
    /// Decoded.
    Ready(T),
    /// There is nothing to show; the text says why (shown as a tooltip).
    Missing(String),
}

/// A bounded least-recently-used cache.
#[derive(Debug)]
pub struct Cache<T> {
    cap: usize,
    map: HashMap<i64, State<T>>,
    /// Oldest use first.
    order: VecDeque<i64>,
}

impl<T> Cache<T> {
    /// A cache holding at most `cap` entries (at least 1).
    pub fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), map: HashMap::new(), order: VecDeque::new() }
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// `true` when empty.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The capacity.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    fn touch(&mut self, id: i64) {
        if let Some(pos) = self.order.iter().position(|x| *x == id) {
            self.order.remove(pos);
        }
        self.order.push_back(id);
    }

    /// The state of `id`, marking it as recently used.
    pub fn get(&mut self, id: i64) -> Option<&State<T>> {
        if self.map.contains_key(&id) {
            self.touch(id);
        }
        self.map.get(&id)
    }

    /// The state without marking it used.
    pub fn peek(&self, id: i64) -> Option<&State<T>> {
        self.map.get(&id)
    }

    /// Whether `id` is present in any state.
    pub fn contains(&self, id: i64) -> bool {
        self.map.contains_key(&id)
    }

    /// Records `state` for `id`, evicting the least recently used entries beyond the capacity.
    pub fn set(&mut self, id: i64, state: State<T>) {
        self.map.insert(id, state);
        self.touch(id);
        while self.map.len() > self.cap {
            // Prefer to drop something that is not being decoded.
            let victim = self
                .order
                .iter()
                .copied()
                .find(|i| !matches!(self.map.get(i), Some(State::Pending)) && *i != id)
                .or_else(|| self.order.iter().copied().find(|i| *i != id));
            match victim {
                Some(v) => {
                    self.map.remove(&v);
                    self.order.retain(|x| *x != v);
                }
                None => break,
            }
        }
    }

    /// Forgets `id` (the entry was deleted).
    pub fn remove(&mut self, id: i64) {
        self.map.remove(&id);
        self.order.retain(|x| *x != id);
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.map.clear();
        self.order.clear();
    }
}

/// What a worker needs to make a thumbnail.
#[derive(Debug, Clone)]
pub struct Request {
    /// The history entry.
    pub id: i64,
    /// Its kind (only images fall back to the file).
    pub kind: EntryKind,
    /// Its local file, for the fallback.
    pub path: Option<PathBuf>,
}

/// Decoded pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// `width * height * 4` bytes, straight alpha.
    pub rgba: Vec<u8>,
}

/// A worker's answer.
#[derive(Debug, Clone)]
pub struct Response {
    /// The entry.
    pub id: i64,
    /// The pixels, or why there are none.
    pub result: Result<Decoded, String>,
}

/// Decodes PNG (or any supported image) bytes and shrinks them to at most [`MAX_EDGE`].
pub fn decode_bytes(bytes: &[u8]) -> Result<Decoded, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("cannot decode: {e}"))?;
    Ok(shrink(img.into_rgba8()))
}

fn shrink(img: image::RgbaImage) -> Decoded {
    let (w, h) = img.dimensions();
    let img = if w.max(h) > MAX_EDGE {
        image::imageops::thumbnail(&img, scaled(w, h, MAX_EDGE).0, scaled(w, h, MAX_EDGE).1)
    } else {
        img
    };
    Decoded { width: img.width(), height: img.height(), rgba: img.into_raw() }
}

/// `(w, h)` scaled so the longer side is `edge` (at least 1 px each).
pub fn scaled(w: u32, h: u32, edge: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (1, 1);
    }
    let f = f64::from(edge) / f64::from(w.max(h));
    (((f64::from(w) * f).round() as u32).max(1), ((f64::from(h) * f).round() as u32).max(1))
}

/// Makes the thumbnail of one request: the stored one, else the file.
pub fn make(history: &History, req: &Request) -> Result<Decoded, String> {
    match history.get(req.id) {
        Ok(Some(e)) => {
            if let Some(blob) = e.thumbnail.filter(|b| !b.is_empty()) {
                return decode_bytes(&blob);
            }
        }
        Ok(None) => return Err("the entry no longer exists".to_owned()),
        Err(e) => return Err(e.to_string()),
    }
    if req.kind != EntryKind::Image {
        return Err("no preview stored for this kind of entry".to_owned());
    }
    let path = req.path.as_ref().ok_or_else(|| "no preview and no local file".to_owned())?;
    let meta = std::fs::metadata(path).map_err(|_| "the file is gone and no preview was stored".to_owned())?;
    if meta.len() > MAX_FALLBACK_BYTES {
        return Err("the file is too large to preview".to_owned());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read the file: {e}"))?;
    decode_bytes(&bytes)
}

/// Decodes thumbnails on worker threads.
pub struct Loader {
    tx: Option<Sender<Request>>,
    rx: Receiver<Response>,
    in_flight: HashSet<i64>,
    handles: Vec<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Loader").field("in_flight", &self.in_flight.len()).finish()
    }
}

impl Loader {
    /// Starts `threads` workers reading from `history`. `wake` is called after each answer.
    pub fn new(history: Arc<History>, threads: usize, wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (res_tx, res_rx) = mpsc::channel::<Response>();
        let req_rx = Arc::new(Mutex::new(req_rx));
        let wake = Arc::new(wake);
        let mut handles = Vec::new();
        for i in 0..threads.max(1) {
            let (rx, tx, h, w) = (req_rx.clone(), res_tx.clone(), history.clone(), wake.clone());
            if let Ok(handle) = std::thread::Builder::new().name(format!("ssx-thumb-{i}")).spawn(move || {
                loop {
                    let next = rx.lock().unwrap_or_else(PoisonError::into_inner).recv();
                    let Ok(req) = next else { break };
                    let result = make(&h, &req);
                    if tx.send(Response { id: req.id, result }).is_err() {
                        break;
                    }
                    w();
                }
            }) {
                handles.push(handle);
            }
        }
        Self { tx: Some(req_tx), rx: res_rx, in_flight: HashSet::new(), handles }
    }

    /// Asks for a thumbnail unless one is already on its way. Returns `true` if queued.
    pub fn request(&mut self, req: Request) -> bool {
        if !self.in_flight.insert(req.id) {
            return false;
        }
        match &self.tx {
            Some(tx) if tx.send(req.clone()).is_ok() => true,
            _ => {
                self.in_flight.remove(&req.id);
                false
            }
        }
    }

    /// Collects what the workers have finished.
    pub fn drain(&mut self) -> Vec<Response> {
        let mut out = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            self.in_flight.remove(&r.id);
            out.push(r);
        }
        out
    }

    /// Number of requests not yet answered.
    pub fn pending(&self) -> usize {
        self.in_flight.len()
    }

    /// Waits until everything asked for has been answered (tests).
    pub fn wait_all(&mut self, timeout: std::time::Duration) -> Vec<Response> {
        let end = std::time::Instant::now() + timeout;
        let mut out = Vec::new();
        while self.pending() > 0 && std::time::Instant::now() < end {
            match self.rx.recv_timeout(std::time::Duration::from_millis(20)) {
                Ok(r) => {
                    self.in_flight.remove(&r.id);
                    out.push(r);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        out
    }
}

impl Drop for Loader {
    fn drop(&mut self) {
        self.tx = None;
        // Workers finish the request they are on and exit; do not block the UI on them.
        for h in self.handles.drain(..) {
            drop(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ssx_core::history::{NewEntry, ThumbnailOptions, thumbnail_from_image};

    use super::*;

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([rgb[0], rgb[1], rgb[2], 255]));
        thumbnail_from_image(&img, ThumbnailOptions::default()).unwrap()
    }

    #[test]
    fn cache_evicts_the_least_recently_used() {
        let mut c = Cache::new(3);
        for i in 1..=3 {
            c.set(i, State::Ready(i));
        }
        assert!(c.get(1).is_some(), "touch 1 so 2 is the oldest");
        c.set(4, State::Ready(4));
        assert_eq!(c.len(), 3);
        assert!(!c.contains(2), "2 was evicted");
        assert!(c.contains(1) && c.contains(3) && c.contains(4));
    }

    #[test]
    fn pending_entries_are_evicted_last() {
        let mut c: Cache<u8> = Cache::new(2);
        c.set(1, State::Pending);
        c.set(2, State::Ready(2));
        c.set(3, State::Ready(3));
        assert!(c.contains(1), "the oldest is pending, so the next-oldest goes instead");
        assert!(!c.contains(2));
    }

    #[test]
    fn the_newly_inserted_entry_is_never_the_victim() {
        let mut c: Cache<u8> = Cache::new(1);
        c.set(1, State::Pending);
        c.set(2, State::Pending);
        assert!(c.contains(2));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn peek_does_not_change_the_order_and_remove_forgets() {
        let mut c = Cache::new(2);
        c.set(1, State::Ready(1));
        c.set(2, State::Ready(2));
        assert_eq!(c.peek(1), Some(&State::Ready(1)));
        c.set(3, State::Ready(3));
        assert!(!c.contains(1), "peeking did not protect 1");
        c.remove(2);
        assert!(!c.contains(2));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(Cache::<u8>::new(0).capacity(), 1);
    }

    proptest! {
        #[test]
        fn the_cache_never_exceeds_its_capacity(cap in 1usize..20, ops in proptest::collection::vec((0i64..50, 0u8..3), 0..200)) {
            let mut c = Cache::new(cap);
            for (id, kind) in ops {
                match kind {
                    0 => c.set(id, State::Pending),
                    1 => c.set(id, State::Ready(id)),
                    _ => { let _ = c.get(id); }
                }
                prop_assert!(c.len() <= cap);
                prop_assert_eq!(c.order.len(), c.map.len());
            }
        }
    }

    #[test]
    fn scaling_keeps_the_aspect_ratio() {
        assert_eq!(scaled(640, 320, 320), (320, 160));
        assert_eq!(scaled(100, 400, 200), (50, 200));
        assert_eq!(scaled(1, 5000, 320), (1, 320));
        assert_eq!(scaled(0, 10, 320), (1, 1));
    }

    #[test]
    fn decoding_shrinks_large_images_only() {
        let big = {
            let img = image::RgbaImage::from_pixel(1000, 500, image::Rgba([10, 200, 30, 255]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        let d = decode_bytes(&big).unwrap();
        assert_eq!((d.width, d.height), (MAX_EDGE, MAX_EDGE / 2));
        assert_eq!(d.rgba.len(), (d.width * d.height * 4) as usize);
        let small = decode_bytes(&png(40, 30, [1, 2, 3])).unwrap();
        assert_eq!((small.width, small.height), (40, 30));
        assert_eq!(&small.rgba[..4], &[1, 2, 3, 255]);
        assert!(decode_bytes(b"not an image").unwrap_err().contains("cannot decode"));
    }

    fn history_with_entries() -> (Arc<History>, Vec<i64>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let h = Arc::new(History::open_in_memory().unwrap());
        let mut ids = Vec::new();
        // 0: stored thumbnail
        let mut e = NewEntry::new(EntryKind::Image);
        e.thumbnail = Some(png(60, 40, [200, 10, 10]));
        ids.push(h.insert(&e).unwrap());
        // 1: no thumbnail, file exists
        let file = dir.path().join("shot.png");
        std::fs::write(&file, png(30, 30, [10, 10, 200])).unwrap();
        let mut e = NewEntry::new(EntryKind::Image);
        e.local_path = Some(file);
        ids.push(h.insert(&e).unwrap());
        // 2: no thumbnail, file gone
        let mut e = NewEntry::new(EntryKind::Image);
        e.local_path = Some(dir.path().join("gone.png"));
        ids.push(h.insert(&e).unwrap());
        // 3: a text entry
        ids.push(h.insert(&NewEntry::new(EntryKind::Text)).unwrap());
        (h, ids, dir)
    }

    #[test]
    fn make_prefers_the_stored_thumbnail_then_the_file_then_explains() {
        let (h, ids, dir) = history_with_entries();
        let req = |i: usize, kind, path: Option<PathBuf>| Request { id: ids[i], kind, path };
        let d = make(&h, &req(0, EntryKind::Image, None)).unwrap();
        assert_eq!(&d.rgba[..4], &[200, 10, 10, 255]);
        let d = make(&h, &req(1, EntryKind::Image, Some(dir.path().join("shot.png")))).unwrap();
        assert_eq!(&d.rgba[..4], &[10, 10, 200, 255]);
        let e = make(&h, &req(2, EntryKind::Image, Some(dir.path().join("gone.png")))).unwrap_err();
        assert!(e.contains("gone"), "{e}");
        let e = make(&h, &req(3, EntryKind::Text, None)).unwrap_err();
        assert!(e.contains("no preview"), "{e}");
        let e = make(&h, &Request { id: 9999, kind: EntryKind::Image, path: None }).unwrap_err();
        assert!(e.contains("no longer exists"), "{e}");
    }

    #[test]
    fn a_huge_fallback_file_is_refused_not_decoded() {
        let dir = tempfile::tempdir().unwrap();
        let h = History::open_in_memory().unwrap();
        let big = dir.path().join("big.png");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(MAX_FALLBACK_BYTES + 1).unwrap();
        let mut e = NewEntry::new(EntryKind::Image);
        e.local_path = Some(big.clone());
        let id = h.insert(&e).unwrap();
        let err = make(&h, &Request { id, kind: EntryKind::Image, path: Some(big) }).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn the_loader_decodes_off_thread_and_deduplicates_requests() {
        let (h, ids, dir) = history_with_entries();
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let w = woken.clone();
        let mut l = Loader::new(h, 2, move || {
            w.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        assert!(l.request(Request { id: ids[0], kind: EntryKind::Image, path: None }));
        assert!(!l.request(Request { id: ids[0], kind: EntryKind::Image, path: None }), "already on its way");
        assert!(l.request(Request {
            id: ids[1],
            kind: EntryKind::Image,
            path: Some(dir.path().join("shot.png"))
        }));
        assert!(l.request(Request { id: ids[3], kind: EntryKind::Text, path: None }));
        let mut got = l.wait_all(std::time::Duration::from_secs(20));
        got.sort_by_key(|r| r.id);
        assert_eq!(got.len(), 3);
        assert!(got[0].result.is_ok() && got[1].result.is_ok() && got[2].result.is_err());
        assert_eq!(l.pending(), 0);
        assert!(l.drain().is_empty());
        // asking again after the answer is allowed
        assert!(l.request(Request { id: ids[0], kind: EntryKind::Image, path: None }));
        let _ = l.wait_all(std::time::Duration::from_secs(20));
        assert!(woken.load(std::sync::atomic::Ordering::SeqCst) >= 4);
    }
}
