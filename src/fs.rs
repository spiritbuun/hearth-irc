use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use fuser::*;

use crate::core::{MAX_WRITE, Model, Node};

pub struct HearthFs {
    model: Arc<Mutex<Model>>,
    uid: u32,
    gid: u32,
}

impl HearthFs {
    pub fn new(model: Arc<Mutex<Model>>) -> Self {
        Self {
            model,
            // SAFETY: getuid/getgid have no preconditions.
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
        }
    }

    fn attr(&self, node: Node, handle: Option<FileHandle>) -> Result<FileAttr, i32> {
        let model = self.model.lock().unwrap();
        let (size, modified) = model.metadata(node, handle.map(|h| h.0))?;
        Ok(FileAttr {
            ino: INodeNo(node.ino()),
            size: size as u64,
            blocks: (size as u64).div_ceil(512),
            atime: modified,
            mtime: modified,
            ctime: model.started,
            crtime: model.started,
            kind: if node.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if node.is_dir() {
                0o500
            } else if node.writable() {
                0o600
            } else {
                0o400
            },
            nlink: if node == Node::Root {
                4
            } else if node.is_dir() {
                2
            } else {
                1
            },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}

fn trace(req: &Request, operation: &str, details: impl std::fmt::Display) {
    log::debug!(
        "op={operation} uid={} pid={} {details}",
        req.uid(),
        req.pid()
    );
}

fn empty(reply: ReplyEmpty, result: Result<(), i32>) {
    match result {
        Ok(()) => reply.ok(),
        Err(e) => reply.error(Errno::from_i32(e)),
    }
}

impl Filesystem for HearthFs {
    fn init(&mut self, req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        trace(req, "init", format_args!("max_write={MAX_WRITE}"));
        config
            .set_max_write(MAX_WRITE as u32)
            .map_err(|_| std::io::Error::other("cannot set maximum write size"))?;
        // No writeback cache or direct-I/O mmap capability: every append must be
        // validated synchronously, and snapshots must not share cached pages.
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        trace(
            req,
            "lookup",
            format_args!("parent={} name={name:?}", parent.0),
        );
        let node = Node::from_ino(parent.0).and_then(|parent| {
            self.model
                .lock()
                .unwrap()
                .lookup(parent, name.to_str().ok_or(libc::ENOENT)?)
        });
        match node.and_then(|node| self.attr(node, None)) {
            Ok(attr) => reply.entry(&Duration::ZERO, &attr, Generation(0)),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        trace(req, "getattr", format_args!("ino={} fh={fh:?}", ino.0));
        match Node::from_ino(ino.0).and_then(|node| self.attr(node, fh)) {
            Ok(attr) => reply.attr(&Duration::ZERO, &attr),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn opendir(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        trace(
            req,
            "opendir",
            format_args!("ino={} flags={flags:#x}", ino.0),
        );
        match Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().open_dir(node)) {
            Ok(id) => reply.opened(FileHandle(id), FopenFlags::empty()),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn releasedir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        empty(
            reply,
            Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().release(node, fh.0)),
        );
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        trace(
            req,
            "readdir",
            format_args!("ino={} offset={offset}", ino.0),
        );
        let model = self.model.lock().unwrap();
        let entries = match Node::from_ino(ino.0).and_then(|node| model.read_dir(node, fh.0)) {
            Ok(entries) => entries,
            Err(e) => {
                reply.error(Errno::from_i32(e));
                return;
            }
        };
        for (i, (node, name)) in entries
            .iter()
            .enumerate()
            .skip(usize::try_from(offset).unwrap_or(usize::MAX))
        {
            let kind = if node.is_dir() {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            if reply.add(INodeNo(node.ino()), (i + 1) as u64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result =
            Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().open(node, flags.0));
        trace(
            req,
            "open",
            format_args!("ino={} flags={flags:#x} result={result:?}", ino.0),
        );
        match result {
            Ok(id) => reply.opened(FileHandle(id), FopenFlags::FOPEN_DIRECT_IO),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn read(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        trace(
            req,
            "read",
            format_args!(
                "ino={} fh={} offset={offset} bytes={size} flags={flags:#x}",
                ino.0, fh.0
            ),
        );
        let model = self.model.lock().unwrap();
        match Node::from_ino(ino.0).and_then(|node| model.read(node, fh.0, offset, size)) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn write(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        // Offsets are intentionally not addresses for this append-only command
        // interface. The kernel's position counts input bytes, while formatted
        // history includes timestamps/nicks and concurrent messages.
        let result = Node::from_ino(ino.0)
            .and_then(|node| self.model.lock().unwrap().write(node, fh.0, data, flags.0));
        trace(
            req,
            "write",
            format_args!(
                "ino={} fh={} offset={offset} bytes={} flags={flags:#x} result={result:?}",
                ino.0,
                fh.0,
                data.len()
            ),
        );
        match result {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(Errno::from_i32(e)),
        }
    }

    fn flush(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let result =
            Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().flush(node, fh.0));
        trace(
            req,
            "flush",
            format_args!("ino={} fh={} result={result:?}", ino.0, fh.0),
        );
        empty(reply, result);
    }

    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        let result =
            Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().flush(node, fh.0));
        trace(
            req,
            "fsync",
            format_args!("ino={} fh={} result={result:?}", ino.0, fh.0),
        );
        // The volatile backend cannot truthfully promise durability.
        empty(reply, result.and(Err(libc::EOPNOTSUPP)));
    }

    fn release(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        trace(
            req,
            "release",
            format_args!("ino={} fh={} flags={flags:#x}", ino.0, fh.0),
        );
        empty(
            reply,
            Node::from_ino(ino.0).and_then(|node| self.model.lock().unwrap().release(node, fh.0)),
        );
    }

    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        trace(
            req,
            "setattr_rejected",
            format_args!("ino={} size={size:?}", ino.0),
        );
        reply.error(Errno::EPERM);
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        trace(
            req,
            "create_rejected",
            format_args!("parent={} name={name:?} flags={flags:#x}", parent.0),
        );
        reply.error(Errno::EPERM);
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        trace(
            req,
            "rename_rejected",
            format_args!(
                "parent={} name={name:?} newparent={} newname={newname:?}",
                parent.0, newparent.0
            ),
        );
        reply.error(Errno::EPERM);
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        trace(
            req,
            "unlink_rejected",
            format_args!("parent={} name={name:?}", parent.0),
        );
        reply.error(Errno::EPERM);
    }

    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        trace(
            req,
            "link_rejected",
            format_args!("ino={} parent={} name={newname:?}", ino.0, newparent.0),
        );
        reply.error(Errno::EPERM);
    }
}
