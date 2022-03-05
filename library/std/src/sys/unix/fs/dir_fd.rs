use super::{cstr, lstat, Dir, DirEntry, ReadDir};
use crate::ffi::{CStr, CString};
use crate::io;
use crate::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd};
use crate::os::unix::prelude::{BorrowedFd, OwnedFd};
use crate::path::{Path, PathBuf};
use crate::sys::{cvt, cvt_r};

#[cfg(not(all(target_os = "macos", target_arch = "x86_64"),))]
use libc::{fdopendir, openat, unlinkat};
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
use macos_weak::{fdopendir, openat, unlinkat};

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
mod macos_weak {
    use crate::sys::weak::weak;
    use libc::{c_char, c_int, DIR};

    fn get_openat_fn() -> Option<unsafe extern "C" fn(c_int, *const c_char, c_int) -> c_int> {
        weak!(fn openat(c_int, *const c_char, c_int) -> c_int);
        openat.get()
    }

    pub fn has_openat() -> bool {
        get_openat_fn().is_some()
    }

    pub unsafe fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int) -> c_int {
        get_openat_fn().map(|openat| openat(dirfd, pathname, flags)).unwrap_or_else(|| {
            crate::sys::unix::os::set_errno(libc::ENOSYS);
            -1
        })
    }

    pub unsafe fn fdopendir(fd: c_int) -> *mut DIR {
        weak!(fn fdopendir(c_int) -> *mut DIR, "fdopendir$INODE64");
        fdopendir.get().map(|fdopendir| fdopendir(fd)).unwrap_or_else(|| {
            crate::sys::unix::os::set_errno(libc::ENOSYS);
            crate::ptr::null_mut()
        })
    }

    pub unsafe fn unlinkat(dirfd: c_int, pathname: *const c_char, flags: c_int) -> c_int {
        weak!(fn unlinkat(c_int, *const c_char, c_int) -> c_int);
        unlinkat.get().map(|unlinkat| unlinkat(dirfd, pathname, flags)).unwrap_or_else(|| {
            crate::sys::unix::os::set_errno(libc::ENOSYS);
            -1
        })
    }
}

pub fn openat_nofollow_dironly(parent_fd: Option<BorrowedFd<'_>>, p: &CStr) -> io::Result<OwnedFd> {
    let fd = cvt_r(|| unsafe {
        openat(
            parent_fd.map(|fd| fd.as_raw_fd()).unwrap_or(libc::AT_FDCWD),
            p.as_ptr(),
            libc::O_CLOEXEC | libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_DIRECTORY,
        )
    })?;
    // SAFETY: file descriptor was opened in this fn
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(any(
    target_os = "solaris",
    target_os = "illumos",
    target_os = "haiku",
    target_os = "vxworks",
))]
fn is_dir(_ent: &DirEntry) -> Option<bool> {
    None
}

#[cfg(not(any(
    target_os = "solaris",
    target_os = "illumos",
    target_os = "haiku",
    target_os = "vxworks",
)))]
fn is_dir(ent: &DirEntry) -> Option<bool> {
    match ent.entry.d_type {
        libc::DT_UNKNOWN => None,
        libc::DT_DIR => Some(true),
        _ => Some(false),
    }
}

struct OpenDir<'a> {
    readdir: ReadDir,
    fd: BorrowedFd<'a>,
    name: CString,
}

impl OpenDir<'_> {
    // Opens the entry as a directory and returns Ok(Some(Opendir)), if parent_fd + name denotes a directory.
    // Otherwise tries to unlink and returns Ok(None) if successful. The path supposed to specify the
    // root deletion directory is not unlinked.
    fn open_or_unlink(
        parent_fd: Option<BorrowedFd<'_>>,
        name: CString,
    ) -> io::Result<Option<Self>> {
        // try to open as a directory
        let fd = match openat_nofollow_dironly(parent_fd, &name) {
            Ok(fd) => fd,
            Err(err) if err.raw_os_error() == Some(libc::ENOTDIR) => {
                // not a directory - unlink and return
                return match parent_fd {
                    // unlink...
                    Some(parent_fd) => {
                        cvt(unsafe { unlinkat(parent_fd.as_raw_fd(), name.as_ptr(), 0) })?;
                        Ok(None)
                    }
                    // ...unless this was supposed to be the deletion root directory
                    None => Err(err),
                };
            }
            Err(err) => return Err(err),
        };

        // open the directory passing ownership of the fd
        let ptr = unsafe { fdopendir(fd.as_raw_fd()) };
        if ptr.is_null() {
            return Err(io::Error::last_os_error());
        }
        let dirp = Dir(ptr);
        // file descriptor is automatically closed by Dir::drop() now, so give up ownership
        let fd = fd.into_raw_fd();
        // a valid root is not needed because we do not call any functions involving the full path
        // of the DirEntrys.
        let dummy_root = PathBuf::new();
        let readdir = ReadDir::new(dirp, dummy_root);
        // SAFETY: fd lifetime is tied to dirp
        let fd = unsafe { BorrowedFd::borrow_raw(fd) };
        Ok(Some(Self { readdir, fd, name }))
    }
}

fn remove_dir_all_loop(p: &Path) -> io::Result<()> {
    let mut ancestors = Vec::<OpenDir<'_>>::new();
    let mut current = OpenDir::open_or_unlink(None, cstr(p)?)?.unwrap();
    loop {
        while let Some(child) = current.readdir.next() {
            let child = child?;
            if let Some(false) = is_dir(&child) {
                // just unlink files
                cvt(unsafe { unlinkat(current.fd.as_raw_fd(), child.name_cstr().as_ptr(), 0) })?;
            } else {
                // try to open the entry as directory, unlink it if it is not
                if let Some(child) =
                    OpenDir::open_or_unlink(Some(current.fd), child.name_cstr().into())?
                {
                    // recurse into the newly opened directory
                    let parent = current;
                    current = child;
                    ancestors.push(parent);
                }
            }
        }

        // unlink the directory after removing its contents
        let parent_fd =
            ancestors.last().map(|open_dir| open_dir.fd.as_raw_fd()).unwrap_or(libc::AT_FDCWD);
        cvt(unsafe { unlinkat(parent_fd, current.name.as_ptr(), libc::AT_REMOVEDIR) })?;

        // go up to the parent directory if we are not done
        match ancestors.pop() {
            Some(parent) => current = parent,
            None => return Ok(()),
        }
    }
}

pub fn remove_dir_all_modern(p: &Path) -> io::Result<()> {
    // We cannot just call remove_dir_all_loop() here because that would not delete a passed
    // symlink. remove_dir_all_loop() does not descend into symlinks and does not delete p
    // if it is a file.
    let attr = lstat(p)?;
    if attr.file_type().is_symlink() {
        crate::fs::remove_file(p)
    } else {
        remove_dir_all_loop(p)
    }
}

#[cfg(not(all(target_os = "macos", target_arch = "x86_64")))]
pub fn remove_dir_all(p: &Path) -> io::Result<()> {
    remove_dir_all_modern(p)
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
pub fn remove_dir_all(p: &Path) -> io::Result<()> {
    if macos_weak::has_openat() {
        // openat() is available with macOS 10.10+, just like unlinkat() and fdopendir()
        remove_dir_all_modern(p)
    } else {
        // fall back to classic implementation
        crate::sys_common::fs::remove_dir_all(p)
    }
}
