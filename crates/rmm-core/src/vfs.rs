//! Linux VFS mode (`rmm vfs`): instead of hard links in Data, a FUSE mount over the game's Data
//! folder shows vanilla Data with the enabled mods and overwrite/ on top: the same winners a
//! deploy links, case-insensitively. Anything a program writes goes to overwrite/ (a file is
//! copied there before its first change), so mods and vanilla files stay as they are. Root files
//! and plugins.txt deploy as usual. The view is fixed at mount time: remount after changing the
//! profile. A deleted file disappears from the view until the next mount; only overwrite/ files
//! are really deleted.
use crate::{Instance, path_key};
use anyhow::{Context, Result, bail};
use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo, LockOwner,
    MountOption, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry,
    ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

const TTL: Duration = Duration::from_secs(1);
/// In the instance while a mount is up: the pid of the process serving it.
pub const MARKER: &str = "vfs.pid";

/// The pid serving a mount of this instance, if one is running.
pub fn mounted_by(inst: &Instance) -> Option<u32> {
    let pid: u32 = fs::read_to_string(inst.root.join(MARKER)).ok()?.trim().parse().ok()?;
    Path::new(&format!("/proc/{pid}")).exists().then_some(pid)
}

struct Node {
    name: OsString,
    parent: u64,
    /// The file shown; None for a folder.
    src: Option<PathBuf>,
    /// Lowercased name -> inode.
    children: BTreeMap<String, u64>,
}

struct Tree {
    nodes: Vec<Node>,
    open: HashMap<u64, File>,
    next_fh: u64,
}

struct Fs {
    overwrite: PathBuf,
    uid: u32,
    gid: u32,
    since: SystemTime,
    tree: Mutex<Tree>,
    /// Keeps the folder under the mount readable at /proc/self/fd/N.
    _under: File,
}

fn key(name: &OsStr) -> String {
    name.to_string_lossy().to_lowercase()
}

fn err(e: std::io::Error) -> Errno {
    Errno::from(e)
}

impl Tree {
    fn node(&self, ino: u64) -> Option<&Node> {
        self.nodes.get(ino.checked_sub(1)? as usize)
    }
    fn node_mut(&mut self, ino: u64) -> &mut Node {
        &mut self.nodes[ino as usize - 1]
    }
    fn child(&self, parent: u64, name: &OsStr) -> Option<u64> {
        self.node(parent)?.children.get(&key(name)).copied()
    }
    fn add(&mut self, parent: u64, name: &OsStr, src: Option<PathBuf>) -> u64 {
        self.nodes.push(Node { name: name.to_owned(), parent, src, children: BTreeMap::new() });
        let ino = self.nodes.len() as u64;
        self.node_mut(parent).children.insert(key(name), ino);
        ino
    }
    /// Path from the Data root.
    fn rel(&self, mut ino: u64) -> PathBuf {
        let mut parts = Vec::new();
        while ino != 1 {
            let n = &self.nodes[ino as usize - 1];
            parts.push(n.name.clone());
            ino = n.parent;
        }
        parts.iter().rev().collect()
    }
    /// Put `src` at `rel`, making folders on the way; a later call for the same path wins.
    fn place(&mut self, rel: &str, src: PathBuf) {
        let mut at = 1;
        let parts: Vec<&str> = rel.split('/').filter(|p| !p.is_empty()).collect();
        for (i, part) in parts.iter().enumerate() {
            let last = i + 1 == parts.len();
            at = match self.child(at, OsStr::new(part)) {
                Some(c) if last => {
                    self.node_mut(c).src = Some(src.clone());
                    c
                }
                Some(c) => c,
                None => self.add(at, OsStr::new(part), last.then(|| src.clone())),
            };
        }
    }
}

impl Fs {
    fn attr(&self, t: &Tree, ino: u64) -> Result<FileAttr, Errno> {
        let n = t.node(ino).ok_or(Errno::ENOENT)?;
        let (kind, perm, size, mtime) = match &n.src {
            Some(src) => {
                let m = fs::metadata(src).map_err(err)?;
                (FileType::RegularFile, 0o644, m.len(), m.modified().unwrap_or(self.since))
            }
            None => (FileType::Directory, 0o755, 0, self.since),
        };
        Ok(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm,
            nlink: if kind == FileType::Directory { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }

    /// The file's copy in overwrite/, made on first change.
    fn copy_up(&self, t: &mut Tree, ino: u64) -> Result<PathBuf, Errno> {
        let src = t.node(ino).and_then(|n| n.src.clone()).ok_or(Errno::EISDIR)?;
        if src.starts_with(&self.overwrite) {
            return Ok(src);
        }
        let dest = self.overwrite.join(t.rel(ino));
        fs::create_dir_all(dest.parent().unwrap()).map_err(err)?;
        fs::copy(&src, &dest).map_err(err)?;
        if let Ok(m) = fs::metadata(&src).and_then(|m| m.modified()) {
            let _ = File::options().write(true).open(&dest).and_then(|f| f.set_modified(m));
        }
        t.node_mut(ino).src = Some(dest.clone());
        Ok(dest)
    }

    fn new_fh(t: &mut Tree, f: File) -> u64 {
        t.next_fh += 1;
        t.open.insert(t.next_fh, f);
        t.next_fh
    }
}

impl Filesystem for Fs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let t = self.tree.lock().unwrap();
        match t.child(parent.0, name).ok_or(Errno::ENOENT).and_then(|c| self.attr(&t, c)) {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let t = self.tree.lock().unwrap();
        match self.attr(&t, ino.0) {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let mut t = self.tree.lock().unwrap();
        let file = t.node(ino.0).is_some_and(|n| n.src.is_some());
        let r = (|| {
            if file && (size.is_some() || mtime.is_some()) {
                let path = self.copy_up(&mut t, ino.0)?;
                let f = File::options().write(true).open(path).map_err(err)?;
                if let Some(s) = size {
                    f.set_len(s).map_err(err)?;
                }
                if let Some(m) = mtime {
                    f.set_modified(match m {
                        TimeOrNow::SpecificTime(m) => m,
                        TimeOrNow::Now => SystemTime::now(),
                    })
                    .map_err(err)?;
                }
            }
            self.attr(&t, ino.0)
        })();
        match r {
            Ok(a) => reply.attr(&TTL, &a),
            Err(e) => reply.error(e),
        }
    }

    fn readdir(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, offset: u64, mut reply: ReplyDirectory) {
        let t = self.tree.lock().unwrap();
        let Some(n) = t.node(ino.0).filter(|n| n.src.is_none()) else { return reply.error(Errno::ENOTDIR) };
        let mut entries: Vec<(u64, FileType, OsString)> = vec![(ino.0, FileType::Directory, ".".into()), (n.parent.max(1), FileType::Directory, "..".into())];
        for &c in n.children.values() {
            let child = &t.nodes[c as usize - 1];
            entries.push((c, if child.src.is_some() { FileType::RegularFile } else { FileType::Directory }, child.name.clone()));
        }
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(ino), i as u64 + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let mut t = self.tree.lock().unwrap();
        let write = !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY);
        let r = (|| {
            let path = if write { self.copy_up(&mut t, ino.0)? } else { t.node(ino.0).and_then(|n| n.src.clone()).ok_or(Errno::EISDIR)? };
            let f = File::options().read(true).write(write).open(path).map_err(err)?;
            Ok::<_, Errno>(Fs::new_fh(&mut t, f))
        })();
        match r {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn read(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, offset: u64, size: u32, _flags: OpenFlags, _lock: Option<LockOwner>, reply: ReplyData) {
        let t = self.tree.lock().unwrap();
        let Some(f) = t.open.get(&fh.0) else { return reply.error(Errno::EBADF) };
        let mut buf = vec![0u8; size as usize];
        let mut got = 0;
        while got < buf.len() {
            match f.read_at(&mut buf[got..], offset + got as u64) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(e) => return reply.error(err(e)),
            }
        }
        reply.data(&buf[..got]);
    }

    fn write(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _wflags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let t = self.tree.lock().unwrap();
        let Some(f) = t.open.get(&fh.0) else { return reply.error(Errno::EBADF) };
        match f.write_all_at(data, offset) {
            Ok(()) => reply.written(data.len() as u32),
            Err(e) => reply.error(err(e)),
        }
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lock: LockOwner, reply: ReplyEmpty) {
        reply.ok();
    }

    fn fsync(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        let t = self.tree.lock().unwrap();
        match t.open.get(&fh.0).map(File::sync_all) {
            Some(Err(e)) => reply.error(err(e)),
            _ => reply.ok(),
        }
    }

    fn release(&self, _req: &Request, _ino: INodeNo, fh: FileHandle, _flags: OpenFlags, _lock: Option<LockOwner>, _flush: bool, reply: ReplyEmpty) {
        self.tree.lock().unwrap().open.remove(&fh.0);
        reply.ok();
    }

    fn create(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let mut t = self.tree.lock().unwrap();
        let r = (|| {
            if t.node(parent.0).is_none_or(|n| n.src.is_some()) {
                return Err(Errno::ENOTDIR);
            }
            let path = self.overwrite.join(t.rel(parent.0)).join(name);
            fs::create_dir_all(path.parent().unwrap()).map_err(err)?;
            let f = File::options().read(true).write(true).create(true).truncate(true).open(&path).map_err(err)?;
            let ino = match t.child(parent.0, name) {
                Some(c) => {
                    t.node_mut(c).src = Some(path);
                    c
                }
                None => t.add(parent.0, name, Some(path)),
            };
            let fh = Fs::new_fh(&mut t, f);
            Ok((self.attr(&t, ino)?, fh))
        })();
        match r {
            Ok((a, fh)) => reply.created(&TTL, &a, Generation(0), FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }

    fn mkdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let mut t = self.tree.lock().unwrap();
        let r = (|| {
            if t.child(parent.0, name).is_some() {
                return Err(Errno::EEXIST);
            }
            fs::create_dir_all(self.overwrite.join(t.rel(parent.0)).join(name)).map_err(err)?;
            let ino = t.add(parent.0, name, None);
            self.attr(&t, ino)
        })();
        match r {
            Ok(a) => reply.entry(&TTL, &a, Generation(0)),
            Err(e) => reply.error(e),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut t = self.tree.lock().unwrap();
        let Some(c) = t.child(parent.0, name) else { return reply.error(Errno::ENOENT) };
        let Some(src) = t.nodes[c as usize - 1].src.clone() else { return reply.error(Errno::EISDIR) };
        if src.starts_with(&self.overwrite)
            && let Err(e) = fs::remove_file(&src)
        {
            return reply.error(err(e));
        }
        t.node_mut(parent.0).children.remove(&key(name));
        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let mut t = self.tree.lock().unwrap();
        let Some(c) = t.child(parent.0, name) else { return reply.error(Errno::ENOENT) };
        let n = &t.nodes[c as usize - 1];
        if n.src.is_some() {
            return reply.error(Errno::ENOTDIR);
        }
        if !n.children.is_empty() {
            return reply.error(Errno::ENOTEMPTY);
        }
        let _ = fs::remove_dir(self.overwrite.join(t.rel(c)));
        t.node_mut(parent.0).children.remove(&key(name));
        reply.ok();
    }

    fn rename(&self, _req: &Request, parent: INodeNo, name: &OsStr, newparent: INodeNo, newname: &OsStr, flags: RenameFlags, reply: ReplyEmpty) {
        let mut t = self.tree.lock().unwrap();
        let r = (|| {
            let c = t.child(parent.0, name).ok_or(Errno::ENOENT)?;
            if t.nodes[c as usize - 1].src.is_none() {
                // Programs fall back to copying a folder and deleting the old one.
                return Err(Errno::EXDEV);
            }
            if t.node(newparent.0).is_none_or(|n| n.src.is_some()) {
                return Err(Errno::ENOTDIR);
            }
            let existing = t.child(newparent.0, newname);
            if let Some(e) = existing {
                if flags.contains(RenameFlags::RENAME_NOREPLACE) {
                    return Err(Errno::EEXIST);
                }
                if t.nodes[e as usize - 1].src.is_none() {
                    return Err(Errno::EISDIR);
                }
            }
            let from = self.copy_up(&mut t, c)?;
            let to = self.overwrite.join(t.rel(newparent.0)).join(newname);
            fs::create_dir_all(to.parent().unwrap()).map_err(err)?;
            fs::rename(&from, &to).map_err(err)?;
            t.node_mut(parent.0).children.remove(&key(name));
            let moved = t.node_mut(c);
            (moved.parent, moved.name, moved.src) = (newparent.0, newname.to_owned(), Some(to));
            t.node_mut(newparent.0).children.insert(key(newname), c);
            Ok(())
        })();
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // The space where writes go.
        let path = std::ffi::CString::new(self.overwrite.as_os_str().as_bytes()).unwrap_or_default();
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(path.as_ptr(), &mut s) } == 0 {
            reply.statfs(s.f_blocks, s.f_bfree, s.f_bavail, s.f_files, s.f_ffree, s.f_bsize as u32, s.f_namemax as u32, s.f_frsize as u32);
        } else {
            reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
        }
    }
}

/// A mounted Data folder; dropping it unmounts.
pub struct Mounted {
    session: Option<BackgroundSession>,
    marker: PathBuf,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        if let Some(s) = self.session.take() {
            let _ = s.umount_and_join();
        }
        let _ = fs::remove_file(&self.marker);
    }
}

/// Deploy the root files and plugins.txt (taking any linked Data files back out), then mount the
/// merged Data folder.
pub fn mount(inst: &Instance) -> Result<Mounted> {
    let data = inst.data_dir();
    if inst.root.starts_with(&data) {
        bail!("the instance can't live inside the Data folder it mounts over");
    }
    if let Some(pid) = mounted_by(inst) {
        bail!("Data is already mounted by rmm (process {pid})");
    }
    fs::create_dir_all(&data)?;
    crate::deploy::deploy_with(inst, false, false)?;
    let profile = inst.profile()?;
    let under = File::open(&data).with_context(|| format!("cannot open {}", data.display()))?;
    let under_path = PathBuf::from(format!("/proc/self/fd/{}", std::os::fd::AsRawFd::as_raw_fd(&under)));
    let overwrite = inst.mod_dir(crate::instance::OVERWRITE);
    fs::create_dir_all(&overwrite)?;
    let mut t = Tree { nodes: vec![Node { name: OsString::new(), parent: 1, src: None, children: BTreeMap::new() }], open: HashMap::new(), next_fh: 0 };
    // Vanilla first, then the winners on top (overwrite/ is the last layer of the file map).
    for e in walkdir::WalkDir::new(&data).min_depth(1).sort_by_file_name() {
        let e = e?;
        let rel = e.path().strip_prefix(&data)?.to_string_lossy().replace('\\', "/");
        if e.file_type().is_dir() {
            let parent = Path::new(&rel).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
            let at = parent.split('/').filter(|p| !p.is_empty()).try_fold(1, |a, p| t.child(a, OsStr::new(p)));
            if let Some(at) = at
                && t.child(at, e.file_name()).is_none()
            {
                t.add(at, e.file_name(), None);
            }
        } else {
            t.place(&rel, under_path.join(&rel));
        }
    }
    let map = crate::conflicts::file_map(inst, &profile)?;
    let mut winners: Vec<(String, (String, String))> = map.winners().into_iter().filter(|(_, (_, src))| !src.starts_with("root/")).collect();
    winners.sort_by_key(|w| path_key(&w.1.1));
    for (_, (mod_id, src)) in winners {
        t.place(&src, inst.mod_dir(&mod_id).join(&src));
    }
    let meta = fs::metadata(&overwrite)?;
    let fs_ = Fs { overwrite, uid: meta.uid(), gid: meta.gid(), since: SystemTime::now(), tree: Mutex::new(t), _under: under };
    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("rmm".into()), MountOption::Subtype("rmm".into())];
    let session = fuser::spawn_mount(fs_, &data, &config).with_context(|| format!("cannot mount over {} (is fusermount3 installed?)", data.display()))?;
    let marker = inst.root.join(MARKER);
    fs::write(&marker, std::process::id().to_string())?;
    Ok(Mounted { session: Some(session), marker })
}

/// `rmm vfs`: mount, then wait for Ctrl+C (or SIGTERM/SIGHUP) and unmount.
pub fn serve(inst: &Instance, ready: impl FnOnce(&Path)) -> Result<()> {
    // Blocked before the FUSE threads start so they inherit it: the signal then reaches sigwait
    // here instead of killing the process with Data still mounted.
    let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
    unsafe {
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    let mounted = mount(inst)?;
    ready(&inst.data_dir());
    let mut sig = 0;
    unsafe { libc::sigwait(&set, &mut sig) };
    drop(mounted);
    Ok(())
}
