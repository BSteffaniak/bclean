//! Linux fanotify change source (filesystem marks, `FAN_REPORT_DFID_NAME`).
//!
//! Requires `CAP_SYS_ADMIN` (filesystem marks) and `CAP_DAC_READ_SEARCH`
//! (`open_by_handle_at`), i.e. effectively root, and Linux 5.9+. A filesystem mark observes
//! every directory on the filesystem without per-directory watches (no inotify watch limit);
//! events outside the watched roots are dropped by the coalescer. Each event carries the
//! file handle of the directory whose entries changed, which is resolved to a path with
//! `open_by_handle_at` + `/proc/self/fd`. No replay: the cursor is always `None`.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crossbeam_channel::{Receiver, Sender};

use super::{
    ChangeBatch, ChangeSource, Pipeline, RawEvent, Roots, WatchBackend, WatchConfig, WatchError,
    spawn_coalescer,
};

const FAN_EVENT_INFO_TYPE_FID: u8 = 1;
const FAN_EVENT_INFO_TYPE_DFID: u8 = 3;
/// `fanotify_event_info_header` (4 bytes) + `__kernel_fsid_t` (8 bytes).
const FID_HANDLE_OFFSET: usize = 12;
/// `struct file_handle` header: `handle_bytes: u32`, `handle_type: i32`.
const FILE_HANDLE_HEADER: usize = 8;

/// fanotify-backed [`ChangeSource`].
pub struct FanotifySource {
    stop: OwnedFd,
    thread: Option<JoinHandle<()>>,
    pipeline: Pipeline,
}

impl FanotifySource {
    pub fn start(config: &WatchConfig) -> Result<Self, WatchError> {
        let roots = Arc::new(Roots::new(&config.roots));
        let init_flags = libc::FAN_CLASS_NOTIF
            | libc::FAN_CLOEXEC
            | libc::FAN_NONBLOCK
            | libc::FAN_REPORT_DFID_NAME;
        let event_flags = (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_LARGEFILE).cast_unsigned();
        // SAFETY: plain syscall with constant flags.
        let fd = unsafe { libc::fanotify_init(init_flags, event_flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: `fd` is a fresh descriptor we own.
        let fan = unsafe { OwnedFd::from_raw_fd(fd) };

        let mask = libc::FAN_CREATE
            | libc::FAN_DELETE
            | libc::FAN_MOVED_FROM
            | libc::FAN_MOVED_TO
            | libc::FAN_MODIFY
            | libc::FAN_ONDIR;
        let mut mount_fds = Vec::new();
        for root in roots.canonicals() {
            let c_root = CString::new(root.as_os_str().as_bytes())
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
            // SAFETY: `fan` is a valid fanotify fd and `c_root` is NUL-terminated.
            let rc = unsafe {
                libc::fanotify_mark(
                    fan.as_raw_fd(),
                    libc::FAN_MARK_ADD | libc::FAN_MARK_FILESYSTEM,
                    mask,
                    libc::AT_FDCWD,
                    c_root.as_ptr(),
                )
            };
            if rc != 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: `c_root` is NUL-terminated.
            let mfd = unsafe {
                libc::open(
                    c_root.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
                )
            };
            if mfd < 0 {
                return Err(io::Error::last_os_error().into());
            }
            // SAFETY: `mfd` is a fresh descriptor we own.
            mount_fds.push(unsafe { OwnedFd::from_raw_fd(mfd) });
        }

        let mut pipe = [0; 2];
        // SAFETY: `pipe` has room for the two descriptors pipe2 writes.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: pipe2 succeeded, so both descriptors are fresh and owned by us.
        let (stop_r, stop_w) =
            unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };

        let (tx, pipeline) = spawn_coalescer(roots, config)?;
        let thread = thread::Builder::new()
            .name("bclean-fanotify".into())
            .spawn(move || run(&fan, &stop_r, &mount_fds, &tx))?;
        Ok(Self {
            stop: stop_w,
            thread: Some(thread),
            pipeline,
        })
    }
}

impl ChangeSource for FanotifySource {
    fn backend(&self) -> WatchBackend {
        WatchBackend::Fanotify
    }

    fn batches(&self) -> &Receiver<ChangeBatch> {
        &self.pipeline.rx
    }
}

impl Drop for FanotifySource {
    fn drop(&mut self) {
        let byte = 1_u8;
        // SAFETY: `stop` is the open write end of the pipe; we write one byte from a local.
        let _ = unsafe { libc::write(self.stop.as_raw_fd(), (&raw const byte).cast(), 1) };
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(fan: &OwnedFd, stop: &OwnedFd, mount_fds: &[OwnedFd], tx: &Sender<RawEvent>) {
    let mut buf = vec![0_u8; 256 * 1024];
    loop {
        let mut fds = [
            libc::pollfd {
                fd: fan.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` is a valid array of two pollfd structs.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            let _ = tx.send(RawEvent::RescanAll);
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                let _ = tx.send(RawEvent::RescanAll);
                return;
            }
            continue;
        }
        loop {
            // SAFETY: `buf` is writable for `buf.len()` bytes.
            let n = unsafe { libc::read(fan.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n < 0 {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => {
                        log::warn!("fanotify read failed: {err}");
                        let _ = tx.send(RawEvent::RescanAll);
                        return;
                    }
                }
            }
            let n = n.cast_unsigned();
            if n == 0 {
                break;
            }
            if !parse_events(&buf[..n], mount_fds, tx) {
                return;
            }
        }
    }
}

/// Parses one `read` worth of events. Returns `false` if the consumer is gone.
fn parse_events(buf: &[u8], mount_fds: &[OwnedFd], tx: &Sender<RawEvent>) -> bool {
    let meta_size = std::mem::size_of::<libc::fanotify_event_metadata>();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut off = 0;
    while off + meta_size <= buf.len() {
        // SAFETY: `off + meta_size <= buf.len()`; read_unaligned tolerates any alignment.
        let meta: libc::fanotify_event_metadata =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off).cast()) };
        let len = meta.event_len as usize;
        let meta_len = usize::from(meta.metadata_len);
        if len < meta_size || meta_len > len || off + len > buf.len() {
            break;
        }
        if meta.fd >= 0 {
            // SAFETY: the kernel handed us this descriptor; we close it exactly once.
            unsafe { libc::close(meta.fd) };
        }
        if meta.vers != libc::FANOTIFY_METADATA_VERSION || meta.mask & libc::FAN_Q_OVERFLOW != 0 {
            if tx.send(RawEvent::RescanAll).is_err() {
                return false;
            }
        } else {
            let mut info = &buf[off + meta_len..off + len];
            while info.len() >= 4 {
                let info_type = info[0];
                let rec_len = usize::from(u16::from_ne_bytes([info[2], info[3]]));
                if rec_len < 4 || rec_len > info.len() {
                    break;
                }
                let rec = &info[..rec_len];
                if matches!(
                    info_type,
                    libc::FAN_EVENT_INFO_TYPE_DFID_NAME
                        | FAN_EVENT_INFO_TYPE_DFID
                        | FAN_EVENT_INFO_TYPE_FID
                ) && rec.len() >= FID_HANDLE_OFFSET + FILE_HANDLE_HEADER
                {
                    let handle = &rec[FID_HANDLE_OFFSET..];
                    let bytes =
                        u32::from_ne_bytes([handle[0], handle[1], handle[2], handle[3]]) as usize;
                    if let Some(handle) = handle.get(..FILE_HANDLE_HEADER + bytes)
                        && seen.insert(handle.to_vec())
                        && let Some(dir) = resolve(handle, mount_fds)
                        && tx.send(RawEvent::Dirty(dir)).is_err()
                    {
                        return false;
                    }
                }
                info = &info[rec_len..];
            }
        }
        off += len;
    }
    true
}

/// Resolves a `struct file_handle` (header + bytes) to a path.
fn resolve(handle: &[u8], mount_fds: &[OwnedFd]) -> Option<PathBuf> {
    // `open_by_handle_at` expects an aligned `struct file_handle`.
    let mut storage = vec![0_u64; handle.len().div_ceil(8)];
    // SAFETY: `storage` holds at least `handle.len()` bytes and does not overlap `handle`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            handle.as_ptr(),
            storage.as_mut_ptr().cast::<u8>(),
            handle.len(),
        );
    };
    for mount in mount_fds {
        // SAFETY: `storage` is an aligned, initialized file_handle whose `handle_bytes` matches
        // the trailing data; `mount` is an open directory on some watched filesystem.
        let fd = unsafe {
            libc::open_by_handle_at(
                mount.as_raw_fd(),
                storage.as_mut_ptr().cast::<libc::file_handle>(),
                libc::O_PATH | libc::O_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is a fresh descriptor we own (closed on drop).
            let owned = unsafe { OwnedFd::from_raw_fd(fd) };
            return std::fs::read_link(format!("/proc/self/fd/{}", owned.as_raw_fd())).ok();
        }
    }
    // Deleted directories (ESTALE) are covered by the parent's DELETE event.
    None
}
