//! MIT-SHM `GetImage` without SysV shared memory and without `unsafe`.
//!
//! The classic MIT-SHM flow (`shmget` + `shmat`) needs libc calls that cannot be made from
//! safe Rust. MIT-SHM 1.2 (2014, Xorg and XWayland both have it) can instead *attach a file
//! descriptor*: we create a `memfd`, hand a duplicate to the server with `ShmAttachFd`, the
//! server `mmap`s it and writes `ShmGetImage` output straight into it, and we read the
//! result back with `pread` on our own descriptor. `memfd` is tmpfs-backed page cache, so
//! `pread` sees the server's writes coherently and the pixels never travel through the
//! socket; the price is one memcpy instead of a mapping. Remote (TCP) connections cannot
//! pass descriptors, so attaching fails and the caller falls back to plain `GetImage`.
//! Only Linux/Android have `memfd_create`; elsewhere SHM is simply reported unavailable.

use crate::session::Session;

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use std::{fs::File, os::unix::fs::FileExt};

    use x11rb::{
        connection::Connection,
        protocol::{
            shm::{ConnectionExt as _, Seg},
            xproto::ImageFormat,
        },
    };

    use crate::{
        error::{X11Error, X11Result},
        session::Session,
    };

    #[derive(Debug)]
    pub(crate) struct Segment {
        seg: Seg,
        file: File,
        pub size: usize,
    }

    #[derive(Debug, Default)]
    pub(crate) enum ShmState {
        #[default]
        Untried,
        Ready(Segment),
        Disabled,
    }

    /// Largest segment we ever create; bigger images are fetched in bands.
    pub(crate) const DEFAULT_SEGMENT_CAP: usize = 16 * 1024 * 1024;

    impl Session {
        pub(crate) fn shm_available(&self) -> bool {
            self.config.use_shm
                && self.ext.shm_fd
                && !matches!(
                    *self.shm.lock().unwrap_or_else(|p| p.into_inner()),
                    ShmState::Disabled
                )
        }

        pub(crate) fn disable_shm(&self) {
            let mut state = self.shm.lock().unwrap_or_else(|p| p.into_inner());
            if let ShmState::Ready(seg) = std::mem::take(&mut *state) {
                self.detach(&seg);
            }
            *state = ShmState::Disabled;
        }

        fn detach(&self, seg: &Segment) {
            // Best effort: the server also drops the segment when we disconnect.
            let _ = self.conn.shm_detach(seg.seg);
        }

        fn new_segment(&self, size: usize) -> X11Result<Segment> {
            let fd = rustix::fs::memfd_create("ssx-x11-shm", rustix::fs::MemfdFlags::CLOEXEC)
                .map_err(|e| X11Error::Connection(format!("memfd_create: {e}")))?;
            let file = File::from(fd);
            file.set_len(size as u64)
                .map_err(|e| X11Error::Connection(format!("sizing shm segment: {e}")))?;
            let server_fd = file
                .try_clone()
                .map_err(|e| X11Error::Connection(format!("duplicating shm fd: {e}")))?;
            let seg = self.conn.generate_id()?;
            self.conn
                .shm_attach_fd(seg, server_fd, false)?
                .check()
                .map_err(|e| X11Error::from_reply("ShmAttachFd", e))?;
            Ok(Segment { seg, file, size })
        }

        /// Fetches `rows` scanlines starting at (`x`,`y`) through shared memory. `bytes`
        /// is the exact size of the band; it must not exceed the segment cap.
        pub(crate) fn shm_get_image(
            &self,
            drawable: u32,
            x: i16,
            y: i16,
            width: u16,
            rows: u16,
            bytes: usize,
        ) -> X11Result<Vec<u8>> {
            let mut state = self.shm.lock().unwrap_or_else(|p| p.into_inner());
            let need_new = match &*state {
                ShmState::Ready(seg) => seg.size < bytes,
                _ => true,
            };
            if need_new {
                if let ShmState::Ready(old) = std::mem::take(&mut *state) {
                    self.detach(&old);
                }
                // Round up so slightly larger follow-up requests reuse the segment.
                let size = bytes.max(1 << 20).min(self.segment_cap().max(bytes));
                *state = ShmState::Ready(self.new_segment(size)?);
            }
            let ShmState::Ready(seg) = &*state else {
                return Err(X11Error::Malformed("shm segment vanished"));
            };
            self.conn
                .shm_get_image(
                    drawable,
                    x,
                    y,
                    width,
                    rows,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    seg.seg,
                    0,
                )?
                .reply()
                .map_err(|e| X11Error::from_reply("ShmGetImage", e))?;
            let mut buf = vec![0u8; bytes];
            seg.file
                .read_exact_at(&mut buf, 0)
                .map_err(|e| X11Error::Connection(format!("reading shm segment: {e}")))?;
            Ok(buf)
        }

        pub(crate) fn segment_cap(&self) -> usize {
            self.config.max_chunk_bytes.unwrap_or(DEFAULT_SEGMENT_CAP)
        }

        pub(crate) fn release_shm(&self) {
            let mut state = self.shm.lock().unwrap_or_else(|p| p.into_inner());
            if let ShmState::Ready(seg) = &*state {
                self.detach(seg);
            }
            *state = ShmState::Untried;
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use crate::{
        error::{X11Error, X11Result},
        session::Session,
    };

    #[derive(Debug, Default)]
    pub(crate) struct ShmState;

    impl Session {
        pub(crate) fn shm_available(&self) -> bool {
            false
        }
        pub(crate) fn disable_shm(&self) {}
        pub(crate) fn release_shm(&self) {}
        pub(crate) fn segment_cap(&self) -> usize {
            0
        }
        pub(crate) fn shm_get_image(
            &self,
            _drawable: u32,
            _x: i16,
            _y: i16,
            _width: u16,
            _rows: u16,
            _bytes: usize,
        ) -> X11Result<Vec<u8>> {
            Err(X11Error::Connection("MIT-SHM needs memfd_create (Linux/Android)".into()))
        }
    }
}

pub(crate) use imp::ShmState;

impl Drop for Session {
    fn drop(&mut self) {
        self.release_shm();
    }
}
