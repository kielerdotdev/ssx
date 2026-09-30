//! Zero-extra-copy frame hand-off between the client and the helper process.
//!
//! The client writes the frozen desktop once into a **sealed `memfd`** (Linux) or a private
//! temporary file (elsewhere). The helper maps it read-only and the renderer reads it exactly
//! once, converting and pre-dimming in the same pass, so a 4K frame is copied client->kernel
//! once and never copied again in user space. This is the only module besides the Windows
//! backend that uses `unsafe`.
#![allow(unsafe_code)] // memory-mapping a file is inherently unsafe; see the SAFETY notes

use std::{fs::File, io::Write, path::PathBuf};

use memmap2::Mmap;
use ssx_types::Frame;

use crate::protocol::FrameSource;

/// The client's end: owns the memfd / temp file for as long as the helper may read it.
#[derive(Debug)]
pub struct SharedFrame {
    source: FrameSource,
    // Keeps the memfd open (and inheritable) until the helper is done.
    _file: File,
    cleanup: Option<PathBuf>,
}

impl SharedFrame {
    /// Copies `frame`'s bytes into shareable storage.
    pub fn create(frame: &Frame) -> std::io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;

            use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, ftruncate, memfd_create};
            // No CLOEXEC on purpose: the helper must inherit it across exec.
            let fd = memfd_create("ssx-overlay-frame", MemfdFlags::ALLOW_SEALING)?;
            ftruncate(&fd, frame.data().len() as u64)?;
            let mut file = File::from(fd);
            file.write_all(frame.data())?;
            // Once sealed, nobody (including a confused helper) can shrink or modify the
            // pages, which is what makes mapping them sound.
            fcntl_add_seals(
                &file,
                SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW | SealFlags::WRITE,
            )?;
            let raw = file.as_raw_fd();
            Ok(Self { source: FrameSource::Fd(raw), _file: file, cleanup: None })
        }
        #[cfg(not(target_os = "linux"))]
        {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir()
                .join(format!("ssx-overlay-{}-{nanos}.raw", std::process::id()));
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .read(true)
                .open(&path)?;
            file.write_all(frame.data())?;
            file.flush()?;
            Ok(Self { source: FrameSource::Path(path.clone()), _file: file, cleanup: Some(path) })
        }
    }

    /// How the helper finds the pixels.
    pub fn source(&self) -> FrameSource {
        self.source.clone()
    }
}

impl Drop for SharedFrame {
    fn drop(&mut self) {
        if let Some(p) = &self.cleanup {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// The helper's end: a read-only mapping of the frame bytes.
#[derive(Debug)]
pub struct MappedFrame {
    map: Mmap,
}

impl MappedFrame {
    /// Maps the frame named by `source`.
    pub fn open(source: &FrameSource) -> std::io::Result<Self> {
        let file = match source {
            FrameSource::Fd(fd) => {
                // A fresh open file description on the same memfd; works only for a
                // descriptor this process inherited.
                File::open(format!("/proc/self/fd/{fd}"))?
            }
            FrameSource::Path(p) => File::open(p)?,
        };
        // SAFETY: on Linux the file is a memfd sealed against writes and resizing by the
        // client before it spawned us, so the mapped bytes cannot change or disappear. For
        // the temporary-file fallback the client owns a private (0600 by default umask),
        // never-modified-after-creation file; truncating it externally would be a
        // misbehaving same-user process, which is outside the threat model (the helper is
        // the user's own helper) and could at worst crash the helper, not the client.
        let map = unsafe { Mmap::map(&file)? };
        Ok(Self { map })
    }

    /// The mapped bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.map
    }
}

#[cfg(test)]
mod tests {
    use ssx_types::Point;

    use super::*;

    #[test]
    fn shared_frame_round_trips_through_the_mapping() {
        let mut data = vec![0u8; 16 * 8 * 4];
        for (i, b) in data.iter_mut().enumerate() {
            *b = (i * 7 % 251) as u8;
        }
        let mut f = Frame::from_rgba8(16, 8, data.clone()).unwrap();
        f.origin = Point::new(3, 4);
        let shared = SharedFrame::create(&f).unwrap();
        let mapped = MappedFrame::open(&shared.source()).unwrap();
        assert_eq!(mapped.as_slice(), &data[..]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn memfd_is_sealed_against_writes() {
        use std::os::fd::AsRawFd;

        let f = Frame::from_rgba8(4, 4, vec![1; 64]).unwrap();
        let shared = SharedFrame::create(&f).unwrap();
        // Re-opening for write must fail because of F_SEAL_WRITE.
        let path = format!("/proc/self/fd/{}", shared._file.as_raw_fd());
        let w = std::fs::OpenOptions::new().write(true).open(path);
        // Opening for write succeeds on a sealed memfd, but any write is refused.
        if let Ok(mut w) = w {
            assert!(w.write_all(&[9]).is_err(), "sealed memfd accepted a write");
        }
    }

    #[test]
    fn missing_source_is_an_error_not_a_panic() {
        let e = MappedFrame::open(&FrameSource::Path("/definitely/not/here".into()));
        assert!(e.is_err());
    }
}
