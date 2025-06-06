use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::task::Waker;
use async_trait::async_trait;
use crate::arch::PAGE_SIZE;
use crate::fs::inode::Inode;
use crate::result::{Errno, SyscallResult};
use crate::sync::mutex::{AsyncMutex, Mutex};
use crate::fs::ffi::OpenFlags;
use crate::net::Socket;
use crate::process::thread::Audit;

pub struct FileMeta {
    pub inode: Option<Arc<dyn Inode>>,
    pub flags: Mutex<OpenFlags>,
}

impl FileMeta {
    pub fn new(inode: Option<Arc<dyn Inode>>, flags: OpenFlags) -> Self {
        FileMeta {
            inode,
            flags: Mutex::new(flags),
        }
    }
}

/// https://man7.org/linux/man-pages/man2/lseek.2.html
pub enum Seek {
    /// The file offset is set to offset bytes.
    Set(isize),
    /// The file offset is set to its current location plus `offset` bytes.
    Cur(isize),
    /// The file offset is set to the size of the file plus `offset` bytes.
    End(isize),
}

impl TryFrom<(i32, isize)> for Seek {
    type Error = Errno;

    fn try_from((whence, offset): (i32, isize)) -> Result<Self, Self::Error> {
        match whence {
            0 => Ok(Seek::Set(offset)),
            1 => Ok(Seek::Cur(offset)),
            2 => Ok(Seek::End(offset)),
            _ => Err(Errno::EINVAL),
        }
    }
}

#[allow(unused)]
#[async_trait]
pub trait File: Send + Sync {
    fn metadata(&self) -> &FileMeta;

    fn as_socket(self: Arc<Self>) -> SyscallResult<Arc<dyn Socket>> {
        Err(Errno::ENOTSOCK)
    }

    async fn read(&self, buf: &mut [u8]) -> SyscallResult<isize> {
        Err(Errno::EOPNOTSUPP)
    }

    async fn write(&self, buf: &[u8]) -> SyscallResult<isize> {
        Err(Errno::EOPNOTSUPP)
    }

    async fn truncate(&self, size: isize) -> SyscallResult {
        Err(Errno::EOPNOTSUPP)
    }

    async fn sync(&self) -> SyscallResult {
        Err(Errno::EOPNOTSUPP)
    }

    async fn seek(&self, seek: Seek) -> SyscallResult<isize> {
        Err(Errno::EOPNOTSUPP)
    }

    async fn ioctl(&self, request: usize, arg2: usize, arg3: usize, arg4: usize, arg5: usize) -> SyscallResult<i32> {
        Err(Errno::ENOTTY)
    }

    async fn pread(&self, buf: &mut [u8], offset: isize) -> SyscallResult<isize> {
        Err(Errno::EOPNOTSUPP)
    }

    async fn pwrite(&self, buf: &[u8], offset: isize) -> SyscallResult<isize> {
        Err(Errno::EOPNOTSUPP)
    }

    async fn readdir(&self) -> SyscallResult<Option<(usize, Arc<dyn Inode>)>> {
        Err(Errno::ENOTDIR)
    }

    fn pollin(&self, waker: Option<Waker>) -> SyscallResult<bool> {
        Ok(true)
    }

    fn pollout(&self, waker: Option<Waker>) -> SyscallResult<bool> {
        Ok(true)
    }
}

impl dyn File {
    pub async fn read_all(&self) -> SyscallResult<Vec<u8>> {
        self.seek(Seek::Set(0)).await?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; PAGE_SIZE];
        loop {
            let len = self.read(&mut tmp).await?;
            if len == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..len as usize]);
        }
        Ok(buf)
    }
}

pub struct CharacterFile {
    metadata: FileMeta,
    pos: AsyncMutex<isize>,
}

impl CharacterFile {
    pub fn new(metadata: FileMeta) -> Arc<Self> {
        Arc::new(Self {
            metadata,
            pos: AsyncMutex::default(),
        })
    }
}

#[async_trait]
impl File for CharacterFile {
    fn metadata(&self) -> &FileMeta {
        &self.metadata
    }

    async fn read(&self, buf: &mut [u8]) -> SyscallResult<isize> {
        let inode = self.metadata.inode.as_ref().unwrap();
        let mut pos = self.pos.lock().await;
        let count = inode.read(buf, *pos).await?;
        *pos += count;
        Ok(count)
    }

    async fn write(&self, buf: &[u8]) -> SyscallResult<isize> {
        let inode = self.metadata.inode.as_ref().unwrap();
        let mut pos = self.pos.lock().await;
        let count = inode.write(buf, *pos).await?;
        *pos += count;
        Ok(count)
    }

    async fn ioctl(&self, request: usize, value: usize, arg2: usize, arg3: usize, arg4: usize) -> SyscallResult<i32> {
        let inode = self.metadata.inode.as_ref().unwrap();
        inode.ioctl(request, value, arg2, arg3, arg4)
    }
}

pub struct DirFile {
    metadata: FileMeta,
    pos: AsyncMutex<usize>,
    audit: Audit,
}

impl DirFile {
    pub fn new(metadata: FileMeta, audit: Audit) -> Arc<Self> {
        Arc::new(Self {
            metadata,
            pos: AsyncMutex::default(),
            audit,
        })
    }
}

#[async_trait]
impl File for DirFile {
    fn metadata(&self) -> &FileMeta {
        &self.metadata
    }

    async fn readdir(&self) -> SyscallResult<Option<(usize, Arc<dyn Inode>)>> {
        let inode = self.metadata.inode.as_ref().unwrap();
        if inode.metadata().inner.lock().unlinked {
            return Err(Errno::ENOENT);
        }
        let mut pos = self.pos.lock().await;
        let inode = match *pos {
            0 => inode.clone(),
            1 => inode.metadata().parent.clone().and_then(|p| p.upgrade()).unwrap_or(inode.clone()),
            _ => match inode.clone().lookup_idx(*pos - 2, &self.audit).await {
                Ok(inode) => inode,
                Err(Errno::ENOENT) => return Ok(None),
                Err(e) => return Err(e),
            },
        };
        *pos += 1;
        Ok(Some((*pos - 1, inode)))
    }

    async fn read(&self, _buf: &mut [u8]) -> SyscallResult<isize> {
        Err(Errno::EISDIR)
    }
}

pub struct RegularFile {
    metadata: FileMeta,
    pos: AsyncMutex<isize>,
    prw_lock: AsyncMutex<()>,
}

impl RegularFile {
    pub fn new(metadata: FileMeta) -> Arc<Self> {
        Arc::new(Self {
            metadata,
            pos: AsyncMutex::default(),
            prw_lock: AsyncMutex::default(),
        })
    }
}

#[async_trait]
impl File for RegularFile {
    fn metadata(&self) -> &FileMeta {
        &self.metadata
    }

    async fn read(&self, buf: &mut [u8]) -> SyscallResult<isize> {
        let inode = self.metadata.inode.as_ref().unwrap();
        let mut pos = self.pos.lock().await;
        let count = inode.read(buf, *pos).await?;
        *pos += count;
        Ok(count)
    }

    async fn write(&self, buf: &[u8]) -> SyscallResult<isize> {
        let inode = self.metadata.inode.as_ref().unwrap();
        let mut pos = self.pos.lock().await;
        let count = inode.write(buf, *pos).await?;
        *pos += count;
        Ok(count)
    }

    async fn truncate(&self, size: isize) -> SyscallResult {
        let inode = self.metadata.inode.as_ref().unwrap();
        inode.truncate(size).await?;
        // The value of the seek pointer shall not be modified by a call to ftruncate().
        Ok(())
    }

    async fn sync(&self) -> SyscallResult {
        let inode = self.metadata.inode.as_ref().unwrap();
        inode.sync().await?;
        Ok(())
    }

    async fn seek(&self, seek: Seek) -> SyscallResult<isize> {
        let mut pos = self.pos.lock().await;
        *pos = match seek {
            Seek::Set(offset) => {
                if offset < 0 {
                    return Err(Errno::EINVAL);
                }
                offset
            }
            Seek::Cur(offset) => {
                match pos.checked_add(offset) {
                    Some(new_pos) => new_pos,
                    None => return Err(if offset < 0 { Errno::EINVAL } else { Errno::EOVERFLOW }),
                }
            }
            Seek::End(offset) => {
                let size = self.metadata.inode.as_ref().unwrap().metadata().inner.lock().size;
                match size.checked_add(offset) {
                    Some(new_pos) => new_pos,
                    None => return Err(if offset < 0 { Errno::EINVAL } else { Errno::EOVERFLOW }),
                }
            }
        };
        Ok(*pos)
    }

    async fn pread(&self, buf: &mut [u8], offset: isize) -> SyscallResult<isize> {
        let _lock = self.prw_lock.lock().await;
        let old = self.seek(Seek::Cur(0)).await?;
        self.seek(Seek::Set(offset)).await?;
        let ret = self.read(buf).await;
        self.seek(Seek::Set(old)).await?;
        ret
    }

    async fn pwrite(&self, buf: &[u8], offset: isize) -> SyscallResult<isize> {
        let _lock = self.prw_lock.lock().await;
        let old = self.seek(Seek::Cur(0)).await?;
        self.seek(Seek::Set(offset)).await?;
        let ret = self.write(buf).await;
        self.seek(Seek::Set(old)).await?;
        ret
    }
}
