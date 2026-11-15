//! FUSE backend (Linux: libfuse3/fusermount3; macOS: macFUSE).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, ReplyOpen, ReplyStatfs,
    Request,
};
use parking_lot::Mutex;
use xof_core::engine::ReaderClass;

use crate::process::ProcessClassifier;
use crate::scenery::{normalize, Attr, Node, Scenery};

const TTL: Duration = Duration::from_secs(2);
const ROOT: u64 = 1;

/// Is FUSE usable here?
pub fn available() -> bool {
    if cfg!(target_os = "macos") {
        Path::new("/Library/Filesystems/macfuse.fs").exists()
    } else {
        Path::new("/dev/fuse").exists()
    }
}

#[derive(Default)]
struct Inodes {
    by_ino: HashMap<u64, String>,
    by_path: HashMap<String, u64>,
}

impl Inodes {
    fn get_or_insert(&mut self, path: &str) -> u64 {
        let key = path.to_ascii_lowercase();
        if let Some(i) = self.by_path.get(&key) {
            return *i;
        }
        let mut ino = fnv(&key) | 2; // never 0 or 1
        while self.by_ino.contains_key(&ino) {
            ino = ino.wrapping_add(2);
        }
        self.by_ino.insert(ino, path.to_string());
        self.by_path.insert(key, ino);
        ino
    }
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

struct OpenFile {
    node: Node,
    class: ReaderClass,
}

pub struct FuseFs {
    scenery: Arc<Scenery>,
    classifier: Arc<ProcessClassifier>,
    inodes: Mutex<Inodes>,
    open: Arc<Mutex<HashMap<u64, Arc<OpenFile>>>>,
    next_fh: u64,
    uid: u32,
    gid: u32,
}

impl FuseFs {
    fn path(&self, ino: u64) -> Option<String> {
        if ino == ROOT {
            return Some(String::new());
        }
        self.inodes.lock().by_ino.get(&ino).cloned()
    }

    fn file_attr(&self, ino: u64, a: &Attr) -> FileAttr {
        FileAttr {
            ino,
            size: a.size,
            blocks: a.size.div_ceil(512),
            atime: a.mtime,
            mtime: a.mtime,
            ctime: a.mtime,
            crtime: a.mtime,
            kind: if a.is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if a.is_dir { 0o555 } else { 0o444 },
            nlink: if a.is_dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 128 * 1024,
            flags: 0,
        }
    }
}

impl Filesystem for FuseFs {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(dir) = self.path(parent) else { return reply.error(libc::ENOENT) };
        let name = name.to_string_lossy();
        let path = normalize(&format!("{dir}/{name}"));
        let Some(node) = self.scenery.resolve(&path) else { return reply.error(libc::ENOENT) };
        let Some(attr) = self.scenery.attr(&node) else { return reply.error(libc::ENOENT) };
        let ino = self.inodes.lock().get_or_insert(&path);
        reply.entry(&TTL, &self.file_attr(ino, &attr), 0);
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(path) = self.path(ino) else { return reply.error(libc::ENOENT) };
        let Some(node) = self.scenery.resolve(&path) else { return reply.error(libc::ENOENT) };
        match self.scenery.attr(&node) {
            Some(a) => reply.attr(&TTL, &self.file_attr(ino, &a)),
            None => reply.error(libc::ENOENT),
        }
    }

    fn open(&mut self, req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return reply.error(libc::EROFS);
        }
        let Some(path) = self.path(ino) else { return reply.error(libc::ENOENT) };
        let Some(node) = self.scenery.resolve(&path) else { return reply.error(libc::ENOENT) };
        let class = self.classifier.classify(req.pid());
        self.scenery.on_open(&node, class);
        let keep = self.scenery.cacheable(&node);
        self.next_fh += 1;
        let fh = self.next_fh;
        self.open.lock().insert(fh, Arc::new(OpenFile { node, class }));
        let open_flags = if keep { fuser::consts::FOPEN_KEEP_CACHE } else { 0 };
        reply.opened(fh, open_flags);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let Some(f) = self.open.lock().get(&fh).cloned() else { return reply.error(libc::EBADF) };
        let scenery = self.scenery.clone();
        // Reply from a blocking worker so slow reads never stall the session.
        self.scenery.runtime().spawn_blocking(move || {
            match scenery.read_blocking(&f.node, offset.max(0) as u64, size as u64, f.class) {
                Ok(d) => reply.data(&d),
                Err(e) => {
                    tracing::warn!("read {:?} @{offset}: {e}", f.node);
                    reply.error(libc::EIO)
                }
            }
        });
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: fuser::ReplyEmpty,
    ) {
        self.open.lock().remove(&fh);
        reply.ok();
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let Some(dir) = self.path(ino) else { return reply.error(libc::ENOENT) };
        let mut entries: Vec<(u64, FileType, String)> = vec![(ino, FileType::Directory, ".".into()), (ROOT, FileType::Directory, "..".into())];
        for (name, node) in self.scenery.list(&dir) {
            let child = normalize(&format!("{dir}/{name}"));
            let child_ino = self.inodes.lock().get_or_insert(&child);
            let kind = if node.is_dir() { FileType::Directory } else { FileType::RegularFile };
            entries.push((child_ino, kind, name));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        reply.statfs(1 << 28, 0, 0, 1 << 20, 0, 4096, 255, 4096);
    }
}

/// A mounted FUSE filesystem; unmounts on drop.
pub struct Mount {
    _session: fuser::BackgroundSession,
}

impl Mount {
    pub fn new(scenery: Arc<Scenery>, mount_point: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(mount_point)?;
        let classifier = Arc::new(ProcessClassifier::new(scenery.xplane_processes()));
        let fs = FuseFs {
            scenery,
            classifier,
            inodes: Mutex::new(Inodes::default()),
            open: Arc::new(Mutex::new(HashMap::new())),
            next_fh: 0,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        };
        let mut opts = vec![MountOption::RO, MountOption::FSName("xporthofs".into()), MountOption::NoAtime];
        if cfg!(target_os = "macos") {
            for o in ["noappledouble", "noapplexattr", "nobrowse", "volname=xporthofs"] {
                opts.push(MountOption::CUSTOM(o.into()));
            }
        }
        let session = fuser::spawn_mount2(fs, mount_point, &opts)?;
        if cfg!(target_os = "macos") {
            // Keep Spotlight away from the mount (design §4.5).
            let _ = std::process::Command::new("mdutil").arg("-i").arg("off").arg(mount_point).output();
        }
        let _ = SystemTime::now();
        Ok(Self { _session: session })
    }
}
