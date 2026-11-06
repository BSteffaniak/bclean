//! macOS FSEvents change source (CoreServices FFI, dispatch-queue scheduled).
//!
//! The stream is directory-level (`kFSEventStreamCreateFlagFileEvents` is off): each event
//! names a directory whose entries changed. Streams are created with `sinceWhen` set to the
//! persisted event id, so changes made while bclean was not running are replayed; the replay
//! ends with a `HistoryDone` event, which flushes the coalescer immediately.
//!
//! Event ids are only meaningful for a volume while its FSEvents store UUID
//! (`FSEventsCopyUUIDForDevice`) is unchanged, so the cursor records the UUID of every root's
//! volume and roots whose UUID changed are reported for a full rescan.

use std::collections::BTreeMap;
use std::ffi::{CStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};

use super::{
    ChangeBatch, ChangeSource, Pipeline, RawEvent, Roots, WatchBackend, WatchConfig, WatchCursor,
    WatchError, spawn_coalescer,
};

#[allow(non_upper_case_globals, non_snake_case, clippy::upper_case_acronyms)] // Apple names
mod ffi {
    use std::ffi::{c_char, c_void};

    pub type CFAllocatorRef = *const c_void;
    pub type CFArrayRef = *const c_void;
    pub type CFStringRef = *const c_void;
    pub type CFUUIDRef = *const c_void;
    pub type CFIndex = isize;
    pub type FSEventStreamRef = *mut c_void;
    pub type ConstFSEventStreamRef = *const c_void;
    pub type DispatchQueue = *mut c_void;

    pub const kCFStringEncodingUTF8: u32 = 0x0800_0100;

    pub const kFSEventStreamEventIdSinceNow: u64 = 0xFFFF_FFFF_FFFF_FFFF;
    pub const kFSEventStreamCreateFlagNoDefer: u32 = 0x0000_0002;
    pub const kFSEventStreamCreateFlagWatchRoot: u32 = 0x0000_0004;

    pub const kFSEventStreamEventFlagUserDropped: u32 = 0x0000_0002;
    pub const kFSEventStreamEventFlagKernelDropped: u32 = 0x0000_0004;
    pub const kFSEventStreamEventFlagHistoryDone: u32 = 0x0000_0010;
    pub const kFSEventStreamEventFlagRootChanged: u32 = 0x0000_0020;

    /// Opaque `CFArrayCallBacks` (five pointer-sized fields); only its address is used.
    #[repr(C)]
    pub struct CFArrayCallBacks {
        _private: [usize; 5],
    }

    #[repr(C)]
    pub struct FSEventStreamContext {
        pub version: CFIndex,
        pub info: *mut c_void,
        pub retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
        pub release: Option<extern "C" fn(*const c_void)>,
        pub copy_description: Option<extern "C" fn(*const c_void) -> CFStringRef>,
    }

    pub type FSEventStreamCallback = extern "C" fn(
        stream: ConstFSEventStreamRef,
        info: *mut c_void,
        num_events: usize,
        event_paths: *mut c_void,
        event_flags: *const u32,
        event_ids: *const u64,
    );

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        pub static kCFTypeArrayCallBacks: CFArrayCallBacks;
        pub fn CFStringCreateWithBytes(
            alloc: CFAllocatorRef,
            bytes: *const u8,
            num_bytes: CFIndex,
            encoding: u32,
            is_external: u8,
        ) -> CFStringRef;
        pub fn CFArrayCreate(
            alloc: CFAllocatorRef,
            values: *const *const c_void,
            num_values: CFIndex,
            callbacks: *const CFArrayCallBacks,
        ) -> CFArrayRef;
        pub fn CFRelease(cf: *const c_void);
        pub fn CFUUIDCreateString(alloc: CFAllocatorRef, uuid: CFUUIDRef) -> CFStringRef;
        pub fn CFStringGetCString(
            s: CFStringRef,
            buf: *mut c_char,
            size: CFIndex,
            encoding: u32,
        ) -> u8;
    }

    #[link(name = "CoreServices", kind = "framework")]
    unsafe extern "C" {
        pub fn FSEventStreamCreate(
            alloc: CFAllocatorRef,
            callback: FSEventStreamCallback,
            context: *const FSEventStreamContext,
            paths: CFArrayRef,
            since_when: u64,
            latency: f64,
            flags: u32,
        ) -> FSEventStreamRef;
        pub fn FSEventStreamSetDispatchQueue(stream: FSEventStreamRef, queue: DispatchQueue);
        pub fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
        pub fn FSEventStreamStop(stream: FSEventStreamRef);
        pub fn FSEventStreamInvalidate(stream: FSEventStreamRef);
        pub fn FSEventStreamRelease(stream: FSEventStreamRef);
        pub fn FSEventsGetCurrentEventId() -> u64;
        pub fn FSEventsCopyUUIDForDevice(dev: libc::dev_t) -> CFUUIDRef;
    }

    // libdispatch is part of libSystem, which is always linked.
    unsafe extern "C" {
        pub fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> DispatchQueue;
        pub fn dispatch_release(object: *mut c_void);
        pub fn dispatch_sync_f(
            queue: DispatchQueue,
            context: *mut c_void,
            work: extern "C" fn(*mut c_void),
        );
    }
}

/// State shared with the FSEvents callback (owned by a leaked `Box` until shutdown).
struct CallbackState {
    tx: Sender<RawEvent>,
    volume_uuids: BTreeMap<String, String>,
}

/// FSEvents-backed [`ChangeSource`].
pub struct FsEventsSource {
    stream: ffi::FSEventStreamRef,
    queue: ffi::DispatchQueue,
    state: *mut CallbackState,
    roots: Arc<Roots>,
    pipeline: Pipeline,
}

// SAFETY: the raw pointers are only dereferenced on the stream's private serial dispatch queue
// (callbacks and shutdown via `dispatch_sync_f`) or in `Drop` after the stream is released, so
// moving the owner to another thread cannot introduce concurrent access.
unsafe impl Send for FsEventsSource {}

impl FsEventsSource {
    pub fn start(config: &WatchConfig) -> Result<Self, WatchError> {
        for root in &config.roots {
            if !root.is_dir() {
                return Err(WatchError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("watch root {} is not a directory", root.display()),
                )));
            }
        }
        let roots = Arc::new(Roots::new(&config.roots));
        let (tx, pipeline) = spawn_coalescer(Arc::clone(&roots), config)?;
        let volume_uuids = volume_uuids(&roots.originals());

        let mut since = ffi::kFSEventStreamEventIdSinceNow;
        if let Some(WatchCursor::FsEvents {
            event_id,
            volume_uuids: stored,
        }) = &config.since
        {
            let originals = roots.originals();
            let stale: Vec<&PathBuf> = originals
                .iter()
                .filter(|r| {
                    let key = root_key(r);
                    !volume_uuids.contains_key(&key) || stored.get(&key) != volume_uuids.get(&key)
                })
                .collect();
            if stale.len() < originals.len() {
                since = *event_id;
            }
            for root in stale {
                let _ = tx.send(RawEvent::Rescan(root.clone()));
            }
        }

        let state = Box::into_raw(Box::new(CallbackState { tx, volume_uuids }));
        match create_stream(
            &roots.canonicals(),
            state.cast(),
            since,
            config.debounce.as_secs_f64().min(1.0),
        ) {
            Ok((stream, queue)) => Ok(Self {
                stream,
                queue,
                state,
                roots,
                pipeline,
            }),
            Err(e) => {
                // SAFETY: the stream was never started, so no callback can hold `state`.
                drop(unsafe { Box::from_raw(state) });
                Err(e)
            }
        }
    }
}

impl ChangeSource for FsEventsSource {
    fn backend(&self) -> WatchBackend {
        WatchBackend::FsEvents
    }

    fn batches(&self) -> &Receiver<ChangeBatch> {
        &self.pipeline.rx
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn current_cursor(&self) -> Option<WatchCursor> {
        // SAFETY: FSEventsGetCurrentEventId has no preconditions.
        let event_id = unsafe { ffi::FSEventsGetCurrentEventId() };
        Some(WatchCursor::FsEvents {
            event_id,
            volume_uuids: volume_uuids(&self.roots.originals()),
        })
    }
}

impl Drop for FsEventsSource {
    fn drop(&mut self) {
        extern "C" fn shutdown(stream: *mut c_void) {
            // SAFETY: `stream` is the live stream scheduled on this queue; running here
            // serializes shutdown with callbacks, so none is in flight or can follow.
            unsafe {
                ffi::FSEventStreamStop(stream);
                ffi::FSEventStreamInvalidate(stream);
                ffi::FSEventStreamRelease(stream);
            }
        }
        // SAFETY: `queue` and `stream` are valid until released here; dispatch_sync_f blocks
        // until `shutdown` has run on the queue, after which `state` has no other users.
        unsafe {
            ffi::dispatch_sync_f(self.queue, self.stream, shutdown);
            ffi::dispatch_release(self.queue);
            drop(Box::from_raw(self.state));
        }
    }
}

fn root_key(root: &Path) -> String {
    root.to_string_lossy().into_owned()
}

/// FSEvents store UUID of each root's volume (roots without an event store are omitted).
fn volume_uuids(roots: &[PathBuf]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for root in roots {
        let Ok(meta) = std::fs::metadata(root) else {
            continue;
        };
        let Ok(dev) = libc::dev_t::try_from(meta.dev()) else {
            continue;
        };
        // SAFETY: FSEventsCopyUUIDForDevice accepts any device number and returns an owned
        // CFUUID or null.
        let uuid = unsafe { ffi::FSEventsCopyUUIDForDevice(dev) };
        if uuid.is_null() {
            continue;
        }
        // SAFETY: `uuid` is a valid CFUUID; the returned string is owned by us.
        let string = unsafe { ffi::CFUUIDCreateString(std::ptr::null(), uuid) };
        let mut buf = [0 as c_char; 64];
        // SAFETY: `string` is checked for null; `buf` is writable for its full length.
        let ok = !string.is_null()
            && unsafe {
                ffi::CFStringGetCString(
                    string,
                    buf.as_mut_ptr(),
                    buf.len().cast_signed(),
                    ffi::kCFStringEncodingUTF8,
                )
            } != 0;
        if ok {
            // SAFETY: CFStringGetCString succeeded, so `buf` holds a NUL-terminated string.
            let s = unsafe { CStr::from_ptr(buf.as_ptr()) };
            out.insert(root_key(root), s.to_string_lossy().into_owned());
        }
        // SAFETY: both objects are owned (Create/Copy rule) and released exactly once.
        unsafe {
            if !string.is_null() {
                ffi::CFRelease(string);
            }
            ffi::CFRelease(uuid);
        }
    }
    out
}

fn create_stream(
    paths: &[PathBuf],
    info: *mut c_void,
    since: u64,
    latency: f64,
) -> Result<(ffi::FSEventStreamRef, ffi::DispatchQueue), WatchError> {
    let mut cf_paths = Vec::with_capacity(paths.len());
    for p in paths {
        let bytes = p.as_os_str().as_bytes();
        // SAFETY: `bytes` is valid for its length; CF copies it.
        let s = unsafe {
            ffi::CFStringCreateWithBytes(
                std::ptr::null(),
                bytes.as_ptr(),
                bytes.len().cast_signed(),
                ffi::kCFStringEncodingUTF8,
                0,
            )
        };
        if s.is_null() {
            release_all(&cf_paths);
            return Err(WatchError::Backend(format!(
                "cannot encode watch path {}",
                p.display()
            )));
        }
        cf_paths.push(s);
    }
    // SAFETY: `cf_paths` holds valid CFStrings; the array retains them via the CFType callbacks.
    let array = unsafe {
        ffi::CFArrayCreate(
            std::ptr::null(),
            cf_paths.as_ptr(),
            cf_paths.len().cast_signed(),
            &raw const ffi::kCFTypeArrayCallBacks,
        )
    };
    release_all(&cf_paths);
    if array.is_null() {
        return Err(WatchError::Backend("CFArrayCreate failed".into()));
    }
    let context = ffi::FSEventStreamContext {
        version: 0,
        info,
        retain: None,
        release: None,
        copy_description: None,
    };
    // SAFETY: `context` is copied by FSEventStreamCreate; `array` is a valid CFArray of CFStrings.
    let stream = unsafe {
        ffi::FSEventStreamCreate(
            std::ptr::null(),
            callback,
            &raw const context,
            array,
            since,
            latency,
            ffi::kFSEventStreamCreateFlagNoDefer | ffi::kFSEventStreamCreateFlagWatchRoot,
        )
    };
    // SAFETY: the stream retains what it needs; our reference is released once.
    unsafe { ffi::CFRelease(array) };
    if stream.is_null() {
        return Err(WatchError::Backend("FSEventStreamCreate failed".into()));
    }
    // SAFETY: the label is a static NUL-terminated string; a null attr means a serial queue.
    let queue =
        unsafe { ffi::dispatch_queue_create(c"dev.bclean.fsevents".as_ptr(), std::ptr::null()) };
    // SAFETY: `stream` and `queue` are valid; scheduling must precede FSEventStreamStart.
    let started = unsafe {
        ffi::FSEventStreamSetDispatchQueue(stream, queue);
        ffi::FSEventStreamStart(stream)
    };
    if started == 0 {
        // SAFETY: the stream failed to start, so no callbacks can run; release both objects.
        unsafe {
            ffi::FSEventStreamInvalidate(stream);
            ffi::FSEventStreamRelease(stream);
            ffi::dispatch_release(queue);
        }
        return Err(WatchError::Backend("FSEventStreamStart failed".into()));
    }
    Ok((stream, queue))
}

fn release_all(objects: &[*const c_void]) {
    for o in objects {
        // SAFETY: each object is an owned CF reference released exactly once.
        unsafe { ffi::CFRelease(*o) };
    }
}

extern "C" fn callback(
    _stream: ffi::ConstFSEventStreamRef,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    if info.is_null() || num_events == 0 {
        return;
    }
    // SAFETY: `info` is the `CallbackState` installed at creation; it outlives the stream.
    // Without kFSEventStreamCreateFlagUseCFTypes, `event_paths` is a `char **` of
    // `num_events` NUL-terminated strings, and the flag/id arrays have `num_events` entries.
    let (state, paths, flags, ids) = unsafe {
        (
            &*info.cast::<CallbackState>(),
            std::slice::from_raw_parts(event_paths.cast::<*const c_char>(), num_events),
            std::slice::from_raw_parts(event_flags, num_events),
            std::slice::from_raw_parts(event_ids, num_events),
        )
    };
    let mut max_id = 0;
    let mut flush = false;
    for ((path, flag), id) in paths.iter().zip(flags).zip(ids) {
        max_id = max_id.max(*id);
        // SAFETY: each entry is a valid NUL-terminated C string (see above).
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(
            unsafe { CStr::from_ptr(*path) }.to_bytes(),
        ));
        let ev = if flag & ffi::kFSEventStreamEventFlagHistoryDone != 0 {
            flush = true;
            continue;
        } else if flag
            & (ffi::kFSEventStreamEventFlagUserDropped | ffi::kFSEventStreamEventFlagKernelDropped)
            != 0
        {
            RawEvent::RescanAll
        } else if flag & ffi::kFSEventStreamEventFlagRootChanged != 0 {
            // The root itself was moved, deleted, or recreated.
            RawEvent::Rescan(path)
        } else {
            // Plain directory-level events and `MustScanSubDirs` both mean "reconcile this
            // subtree"; dirty paths already carry subtree semantics.
            RawEvent::Dirty(path)
        };
        let _ = state.tx.send(ev);
    }
    if max_id != 0 {
        let _ = state.tx.send(RawEvent::Cursor(WatchCursor::FsEvents {
            event_id: max_id,
            volume_uuids: state.volume_uuids.clone(),
        }));
    }
    if flush {
        let _ = state.tx.send(RawEvent::Flush);
    }
}
