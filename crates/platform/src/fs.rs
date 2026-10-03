// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Filesystem support that differs between native and WASI targets: error codes, file metadata,
//! positional reads and writes, the open flag constants, and running blocking operations off the
//! JS thread.
//!
//! Filesystem operations themselves go through `std::fs`, which uses the host's filesystem
//! natively and the component's preopened directories on WASI. On WASI, [`wasi`] has
//! asynchronous versions of some of them.

#[cfg(target_arch = "wasm32")]
pub mod wasi;

use std::fs::{File, Metadata};
use std::io;
use std::time::SystemTime;

/// A filesystem or socket error as Node.js reports it: the POSIX error name, such as `ENOENT`, the
/// negated errno, and libuv's description of the error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsError {
    pub code: &'static str,
    pub errno: i32,
    pub description: &'static str,
}

/// The errors [`FsError`] distinguishes: the name, the Linux errno, and libuv's description.
const ERRORS: [(&str, i32, &str); 63] = [
    ("EPERM", 1, "operation not permitted"),
    ("ENOENT", 2, "no such file or directory"),
    ("EINTR", 4, "interrupted system call"),
    ("EIO", 5, "i/o error"),
    ("EBADF", 9, "bad file descriptor"),
    ("EAGAIN", 11, "resource temporarily unavailable"),
    ("ENOMEM", 12, "not enough memory"),
    ("EACCES", 13, "permission denied"),
    ("EBUSY", 16, "resource busy or locked"),
    ("EEXIST", 17, "file already exists"),
    ("EXDEV", 18, "cross-device link not permitted"),
    ("ENODEV", 19, "no such device"),
    ("ENOTDIR", 20, "not a directory"),
    ("EISDIR", 21, "illegal operation on a directory"),
    ("EINVAL", 22, "invalid argument"),
    ("ENFILE", 23, "file table overflow"),
    ("EMFILE", 24, "too many open files"),
    ("ETXTBSY", 26, "text file is busy"),
    ("EFBIG", 27, "file too large"),
    ("ENOSPC", 28, "no space left on device"),
    ("ESPIPE", 29, "invalid seek"),
    ("EROFS", 30, "read-only file system"),
    ("EMLINK", 31, "too many links"),
    ("ENAMETOOLONG", 36, "name too long"),
    ("ENOSYS", 38, "function not implemented"),
    ("ENOTEMPTY", 39, "directory not empty"),
    ("ELOOP", 40, "too many symbolic links encountered"),
    ("EPIPE", 32, "broken pipe"),
    ("ENOTSUP", 95, "operation not supported on socket"),
    ("EADDRINUSE", 98, "address already in use"),
    ("EADDRNOTAVAIL", 99, "address not available"),
    ("ENETUNREACH", 101, "network is unreachable"),
    ("ECONNABORTED", 103, "software caused connection abort"),
    ("ECONNRESET", 104, "connection reset by peer"),
    ("ENOTCONN", 107, "socket is not connected"),
    ("ETIMEDOUT", 110, "connection timed out"),
    ("ECONNREFUSED", 111, "connection refused"),
    ("EHOSTUNREACH", 113, "host is unreachable"),
    ("ECANCELED", 125, "operation canceled"),
    ("ESRCH", 3, "no such process"),
    ("ENXIO", 6, "no such device or address"),
    ("E2BIG", 7, "argument list too long"),
    ("EFAULT", 14, "bad address in system call argument"),
    ("ENOTTY", 25, "inappropriate ioctl for device"),
    ("ERANGE", 34, "result too large"),
    ("ENODATA", 61, "no data available"),
    ("EPROTO", 71, "protocol error"),
    ("EOVERFLOW", 75, "value too large for defined data type"),
    ("EILSEQ", 84, "illegal byte sequence"),
    ("ENOTSOCK", 88, "socket operation on non-socket"),
    ("EDESTADDRREQ", 89, "destination address required"),
    ("EMSGSIZE", 90, "message too long"),
    ("EPROTOTYPE", 91, "protocol wrong type for socket"),
    ("ENOPROTOOPT", 92, "protocol not available"),
    ("EPROTONOSUPPORT", 93, "protocol not supported"),
    ("ESOCKTNOSUPPORT", 94, "socket type not supported"),
    ("EAFNOSUPPORT", 97, "address family not supported"),
    ("ENETDOWN", 100, "network is down"),
    ("ENOBUFS", 105, "no buffer space available"),
    ("EISCONN", 106, "socket is already connected"),
    (
        "ESHUTDOWN",
        108,
        "cannot send after transport endpoint shutdown",
    ),
    ("EHOSTDOWN", 112, "host is down"),
    ("EALREADY", 114, "connection already in progress"),
];

impl FsError {
    /// The error named `code`, with the host's errno natively and Linux's on WASI. Unknown names
    /// give `EIO`.
    pub fn from_code(code: &str) -> Self {
        let (code, linux, description) = ERRORS
            .iter()
            .copied()
            .find(|(name, ..)| *name == code)
            .unwrap_or(("EIO", 5, "i/o error"));
        FsError {
            code,
            errno: -host_errno(code).unwrap_or(linux),
            description,
        }
    }

    /// The error for `error`, by its OS error number, or by its kind if it has none. Natively,
    /// an OS error number missing from [`ERRORS`] gives the code and description
    /// `Unknown system error -N`, as libuv names it.
    pub fn from_io(error: &io::Error) -> Self {
        let os_code = error
            .raw_os_error()
            .map(|errno| (errno, code_of_os_error(errno)));
        #[cfg(not(target_arch = "wasm32"))]
        if let Some((errno, None)) = os_code {
            return Self::unknown(errno);
        }
        let code = os_code
            .and_then(|(_, code)| code)
            .unwrap_or_else(|| match error.kind() {
                io::ErrorKind::NotFound => "ENOENT",
                io::ErrorKind::PermissionDenied => "EACCES",
                io::ErrorKind::AlreadyExists => "EEXIST",
                io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => "EINVAL",
                io::ErrorKind::Unsupported => "ENOSYS",
                io::ErrorKind::TimedOut => "ETIMEDOUT",
                io::ErrorKind::ConnectionRefused => "ECONNREFUSED",
                io::ErrorKind::ConnectionReset => "ECONNRESET",
                io::ErrorKind::ConnectionAborted => "ECONNABORTED",
                io::ErrorKind::BrokenPipe => "EPIPE",
                io::ErrorKind::AddrInUse => "EADDRINUSE",
                io::ErrorKind::AddrNotAvailable => "EADDRNOTAVAIL",
                io::ErrorKind::NotConnected => "ENOTCONN",
                _ => "EIO",
            });
        Self::from_code(code)
    }

    /// The error for the OS error number `errno`, which [`ERRORS`] lacks.
    #[cfg(not(target_arch = "wasm32"))]
    fn unknown(errno: i32) -> Self {
        use std::collections::HashMap;
        use std::sync::{Mutex, OnceLock};
        // The names are leaked once per error number, of which there are few.
        static NAMES: OnceLock<Mutex<HashMap<i32, &'static str>>> = OnceLock::new();
        let mut names = NAMES
            .get_or_init(Default::default)
            .lock()
            .expect("the lock is never poisoned");
        let name = *names
            .entry(errno)
            .or_insert_with(|| format!("Unknown system error -{errno}").leak());
        FsError {
            code: name,
            errno: -errno,
            description: name,
        }
    }
}

impl From<io::Error> for FsError {
    fn from(error: io::Error) -> Self {
        Self::from_io(&error)
    }
}

/// The error name for the OS error number `errno`.
#[cfg(not(target_arch = "wasm32"))]
fn code_of_os_error(errno: i32) -> Option<&'static str> {
    ERRORS
        .iter()
        .map(|(name, ..)| *name)
        .find(|name| host_errno(name) == Some(errno))
}

/// The error name for the wasi-libc error number `errno`, which follows WASI's numbering.
#[cfg(target_arch = "wasm32")]
fn code_of_os_error(errno: i32) -> Option<&'static str> {
    Some(match errno {
        2 | 76 => "EACCES",
        3 => "EADDRINUSE",
        4 => "EADDRNOTAVAIL",
        13 => "ECONNABORTED",
        14 => "ECONNREFUSED",
        15 => "ECONNRESET",
        23 => "EHOSTUNREACH",
        40 => "ENETUNREACH",
        53 => "ENOTCONN",
        64 => "EPIPE",
        73 => "ETIMEDOUT",
        6 => "EAGAIN",
        8 => "EBADF",
        10 => "EBUSY",
        11 => "ECANCELED",
        20 => "EEXIST",
        22 => "EFBIG",
        27 => "EINTR",
        28 => "EINVAL",
        29 => "EIO",
        31 => "EISDIR",
        32 => "ELOOP",
        33 => "EMFILE",
        34 => "EMLINK",
        37 => "ENAMETOOLONG",
        41 => "ENFILE",
        43 => "ENODEV",
        44 => "ENOENT",
        48 => "ENOMEM",
        51 => "ENOSPC",
        52 => "ENOSYS",
        54 => "ENOTDIR",
        55 => "ENOTEMPTY",
        58 => "ENOTSUP",
        63 => "EPERM",
        69 => "EROFS",
        70 => "ESPIPE",
        74 => "ETXTBSY",
        75 => "EXDEV",
        1 => "E2BIG",
        5 => "EAFNOSUPPORT",
        7 => "EALREADY",
        17 => "EDESTADDRREQ",
        21 => "EFAULT",
        25 => "EILSEQ",
        30 => "EISCONN",
        35 => "EMSGSIZE",
        38 => "ENETDOWN",
        42 => "ENOBUFS",
        50 => "ENOPROTOOPT",
        57 => "ENOTSOCK",
        59 => "ENOTTY",
        60 => "ENXIO",
        61 => "EOVERFLOW",
        65 => "EPROTO",
        66 => "EPROTONOSUPPORT",
        67 => "EPROTOTYPE",
        68 => "ERANGE",
        71 => "ESRCH",
        _ => return None,
    })
}

/// The host's errno for the error named `code`.
#[cfg(not(target_arch = "wasm32"))]
fn host_errno(code: &str) -> Option<i32> {
    Some(match code {
        "EPERM" => libc::EPERM,
        "ENOENT" => libc::ENOENT,
        "EINTR" => libc::EINTR,
        "EIO" => libc::EIO,
        "EBADF" => libc::EBADF,
        "EAGAIN" => libc::EAGAIN,
        "ENOMEM" => libc::ENOMEM,
        "EACCES" => libc::EACCES,
        "EBUSY" => libc::EBUSY,
        "EEXIST" => libc::EEXIST,
        "EXDEV" => libc::EXDEV,
        "ENODEV" => libc::ENODEV,
        "ENOTDIR" => libc::ENOTDIR,
        "EISDIR" => libc::EISDIR,
        "EINVAL" => libc::EINVAL,
        "ENFILE" => libc::ENFILE,
        "EMFILE" => libc::EMFILE,
        "ETXTBSY" => libc::ETXTBSY,
        "EFBIG" => libc::EFBIG,
        "ENOSPC" => libc::ENOSPC,
        "ESPIPE" => libc::ESPIPE,
        "EROFS" => libc::EROFS,
        "EMLINK" => libc::EMLINK,
        "ENAMETOOLONG" => libc::ENAMETOOLONG,
        "ENOSYS" => libc::ENOSYS,
        "ENOTEMPTY" => libc::ENOTEMPTY,
        "ELOOP" => libc::ELOOP,
        "ENOTSUP" => libc::ENOTSUP,
        "ECANCELED" => libc::ECANCELED,
        "EPIPE" => libc::EPIPE,
        "EADDRINUSE" => libc::EADDRINUSE,
        "EADDRNOTAVAIL" => libc::EADDRNOTAVAIL,
        "ENETUNREACH" => libc::ENETUNREACH,
        "ECONNABORTED" => libc::ECONNABORTED,
        "ECONNRESET" => libc::ECONNRESET,
        "ENOTCONN" => libc::ENOTCONN,
        "ETIMEDOUT" => libc::ETIMEDOUT,
        "ECONNREFUSED" => libc::ECONNREFUSED,
        "EHOSTUNREACH" => libc::EHOSTUNREACH,
        "ESRCH" => libc::ESRCH,
        "ENXIO" => libc::ENXIO,
        "E2BIG" => libc::E2BIG,
        "EFAULT" => libc::EFAULT,
        "ENOTTY" => libc::ENOTTY,
        "ERANGE" => libc::ERANGE,
        "ENODATA" => libc::ENODATA,
        "EPROTO" => libc::EPROTO,
        "EOVERFLOW" => libc::EOVERFLOW,
        "EILSEQ" => libc::EILSEQ,
        "ENOTSOCK" => libc::ENOTSOCK,
        "EDESTADDRREQ" => libc::EDESTADDRREQ,
        "EMSGSIZE" => libc::EMSGSIZE,
        "EPROTOTYPE" => libc::EPROTOTYPE,
        "ENOPROTOOPT" => libc::ENOPROTOOPT,
        "EPROTONOSUPPORT" => libc::EPROTONOSUPPORT,
        "ESOCKTNOSUPPORT" => libc::ESOCKTNOSUPPORT,
        "EAFNOSUPPORT" => libc::EAFNOSUPPORT,
        "ENETDOWN" => libc::ENETDOWN,
        "ENOBUFS" => libc::ENOBUFS,
        "EISCONN" => libc::EISCONN,
        "ESHUTDOWN" => libc::ESHUTDOWN,
        "EHOSTDOWN" => libc::EHOSTDOWN,
        "EALREADY" => libc::EALREADY,
        _ => return None,
    })
}

/// WASI reports Linux's errno, which is the table's.
#[cfg(target_arch = "wasm32")]
fn host_errno(_code: &str) -> Option<i32> {
    None
}

/// The kind of a filesystem entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    BlockDevice,
    CharDevice,
    Fifo,
    Socket,
    Unknown,
}

impl FileKind {
    /// The kind of an entry of type `file_type`.
    pub fn of(file_type: std::fs::FileType) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if file_type.is_block_device() {
                return FileKind::BlockDevice;
            }
            if file_type.is_char_device() {
                return FileKind::CharDevice;
            }
            if file_type.is_fifo() {
                return FileKind::Fifo;
            }
            if file_type.is_socket() {
                return FileKind::Socket;
            }
        }
        if file_type.is_symlink() {
            FileKind::Symlink
        } else if file_type.is_dir() {
            FileKind::Directory
        } else if file_type.is_file() {
            FileKind::File
        } else {
            FileKind::Unknown
        }
    }

    /// The `S_IFMT` bits of the kind.
    fn mode_bits(self) -> u32 {
        match self {
            FileKind::File => 0o100000,
            FileKind::Directory => 0o040000,
            FileKind::Symlink => 0o120000,
            FileKind::BlockDevice => 0o060000,
            FileKind::CharDevice => 0o020000,
            FileKind::Fifo => 0o010000,
            FileKind::Socket => 0o140000,
            FileKind::Unknown => 0,
        }
    }
}

/// The metadata of a filesystem entry, with the fields of Node's `fs.Stats`. Times are in
/// milliseconds since the Unix epoch.
///
/// WASI and Windows report no mode bits, owners, device or inode numbers, or block counts. There,
/// `mode` is `0o644` for files (`0o444` for read-only files on Windows), `0o755` for directories
/// and `0o777` for symlinks, with the kind's `S_IFMT` bits, `uid`, `gid`, `dev`, `ino` and `rdev`
/// are 0, `nlink` is 1, `blksize` is 4096, `blocks` counts 512-byte blocks of `size`, and
/// `ctime_ms` is `mtime_ms`. `birthtime_ms` is `ctime_ms` where the host reports no creation
/// time.
#[derive(Clone, Copy, Debug)]
pub struct Stat {
    pub kind: FileKind,
    pub dev: u64,
    pub mode: u32,
    pub nlink: u64,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u64,
    pub blksize: u64,
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime_ms: f64,
    pub mtime_ms: f64,
    pub ctime_ms: f64,
    pub birthtime_ms: f64,
}

/// The milliseconds since the Unix epoch of `time`, or `None` if `time` is an error.
fn millis(time: io::Result<SystemTime>) -> Option<f64> {
    let time = time.ok()?;
    Some(match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => after.as_secs_f64() * 1000.0,
        Err(before) => -before.duration().as_secs_f64() * 1000.0,
    })
}

impl From<&Metadata> for Stat {
    #[cfg(unix)]
    fn from(metadata: &Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let time = |secs: i64, nanos: i64| secs as f64 * 1000.0 + nanos as f64 / 1e6;
        let ctime_ms = time(metadata.ctime(), metadata.ctime_nsec());
        Stat {
            kind: FileKind::of(metadata.file_type()),
            dev: metadata.dev(),
            mode: metadata.mode(),
            nlink: metadata.nlink(),
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev(),
            blksize: metadata.blksize(),
            ino: metadata.ino(),
            size: metadata.size(),
            blocks: metadata.blocks(),
            atime_ms: time(metadata.atime(), metadata.atime_nsec()),
            mtime_ms: time(metadata.mtime(), metadata.mtime_nsec()),
            ctime_ms,
            birthtime_ms: millis(metadata.created()).unwrap_or(ctime_ms),
        }
    }

    #[cfg(not(unix))]
    fn from(metadata: &Metadata) -> Self {
        Stat::without_modes(
            FileKind::of(metadata.file_type()),
            metadata.len(),
            cfg!(windows) && metadata.permissions().readonly(),
            millis(metadata.accessed()),
            millis(metadata.modified()),
            millis(metadata.created()),
        )
    }
}

impl Stat {
    /// The metadata of an entry of `kind` and `size` on a host that reports no mode bits or
    /// owners, with the placeholders documented on [`Stat`]. `read_only` gives files mode `0o444`.
    /// Times are in milliseconds since the Unix epoch, and `None` where the host reports none.
    #[cfg(not(unix))]
    fn without_modes(
        kind: FileKind,
        size: u64,
        read_only: bool,
        accessed: Option<f64>,
        modified: Option<f64>,
        created: Option<f64>,
    ) -> Self {
        let permissions = match kind {
            FileKind::Directory => 0o755,
            FileKind::Symlink => 0o777,
            _ if read_only => 0o444,
            _ => 0o644,
        };
        let mtime_ms = modified.unwrap_or(0.0);
        Stat {
            kind,
            dev: 0,
            mode: kind.mode_bits() | permissions,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            ino: 0,
            size,
            blocks: size.div_ceil(512),
            atime_ms: accessed.unwrap_or(mtime_ms),
            mtime_ms,
            ctime_ms: mtime_ms,
            birthtime_ms: created.unwrap_or(mtime_ms),
        }
    }

    /// The metadata of a character device without times, for the standard streams.
    pub fn char_device() -> Self {
        Stat {
            kind: FileKind::CharDevice,
            dev: 0,
            mode: FileKind::CharDevice.mode_bits() | 0o620,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            ino: 0,
            size: 0,
            blocks: 0,
            atime_ms: 0.0,
            mtime_ms: 0.0,
            ctime_ms: 0.0,
            birthtime_ms: 0.0,
        }
    }
}

/// Read from `file` at `position` into `buffer`, without changing the file's position.
pub fn read_at(file: &File, buffer: &mut [u8], position: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_at(file, buffer, position)
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        // `seek_read` moves the file position, which is restored afterwards.
        let current = (&*file).stream_position()?;
        let result = std::os::windows::fs::FileExt::seek_read(file, buffer, position);
        (&*file).seek(SeekFrom::Start(current))?;
        result
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = file;
        let current = file.stream_position()?;
        file.seek(SeekFrom::Start(position))?;
        let result = file.read(buffer);
        file.seek(SeekFrom::Start(current))?;
        result
    }
}

/// Write `data` to `file` at `position`, without changing the file's position, and return the
/// number of bytes written.
pub fn write_at(file: &File, data: &[u8], position: u64) -> io::Result<usize> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_at(file, data, position)
    }
    #[cfg(windows)]
    {
        use std::io::{Seek, SeekFrom};
        // `seek_write` moves the file position, which is restored afterwards.
        let current = (&*file).stream_position()?;
        let result = std::os::windows::fs::FileExt::seek_write(file, data, position);
        (&*file).seek(SeekFrom::Start(current))?;
        result
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = file;
        let current = file.stream_position()?;
        file.seek(SeekFrom::Start(position))?;
        let result = file.write(data);
        file.seek(SeekFrom::Start(current))?;
        result
    }
}

/// Check that the calling process may access `path` with `mode`, a combination of `R_OK` (4),
/// `W_OK` (2) and `X_OK` (1), or that `path` exists for `F_OK` (0). On Unix, this is access(2).
/// On WASI, which reports no permissions, only existence is checked. On Windows, `W_OK` is
/// checked against the read-only attribute, and the other bits only for existence.
pub fn access(path: &str, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = std::ffi::CString::new(path)?;
        // SAFETY: `path` is a NUL-terminated string that outlives the call.
        match unsafe { libc::access(path.as_ptr(), mode as libc::c_int) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
    #[cfg(not(unix))]
    {
        let metadata = std::fs::metadata(path)?;
        if cfg!(windows) && mode & 2 != 0 && metadata.permissions().readonly() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        Ok(())
    }
}

/// The metadata of the standard stream `fd`, which is 0, 1 or 2. Without Unix file descriptors,
/// it's [`Stat::char_device`].
pub fn stdio_stat(fd: i32) -> io::Result<Stat> {
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // SAFETY: `fd` is a standard stream, and `ManuallyDrop` keeps the `File` from closing it.
        let file = std::mem::ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        Ok(Stat::from(&file.metadata()?))
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Ok(Stat::char_device())
    }
}

/// Set the access and modification times of the file at `path`, following symlinks. `None`
/// leaves a time unchanged. On Unix, this is utimensat(2), which needs no access to the file's
/// contents. Elsewhere, the file is opened for reading to set the times.
pub fn set_times(
    path: &str,
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        let timespec = |time: Option<SystemTime>| {
            let Some(time) = time else {
                return libc::timespec {
                    tv_sec: 0,
                    tv_nsec: libc::UTIME_OMIT,
                };
            };
            let (secs, nanos) = match time.duration_since(SystemTime::UNIX_EPOCH) {
                Ok(after) => (after.as_secs() as i64, i64::from(after.subsec_nanos())),
                Err(before) => {
                    let before = before.duration();
                    match before.subsec_nanos() {
                        0 => (-(before.as_secs() as i64), 0),
                        nanos => (
                            -(before.as_secs() as i64) - 1,
                            1_000_000_000 - i64::from(nanos),
                        ),
                    }
                }
            };
            libc::timespec {
                tv_sec: secs as libc::time_t,
                tv_nsec: nanos as libc::c_long,
            }
        };
        let times = [timespec(accessed), timespec(modified)];
        let path = std::ffi::CString::new(path)?;
        // SAFETY: `path` is a NUL-terminated string and `times` holds two timespecs, both of
        // which outlive the call.
        match unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), 0) } {
            0 => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    }
    #[cfg(not(unix))]
    {
        let mut times = std::fs::FileTimes::new();
        if let Some(accessed) = accessed {
            times = times.set_accessed(accessed);
        }
        if let Some(modified) = modified {
            times = times.set_modified(modified);
        }
        File::open(path)?.set_times(times)
    }
}

/// The open flag constants of `fs.constants`, with the host's values natively and Linux x86-64's
/// on WASI.
pub fn open_flags() -> Vec<(&'static str, i32)> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        #[allow(unused_mut)]
        let mut flags = vec![
            ("O_RDONLY", libc::O_RDONLY),
            ("O_WRONLY", libc::O_WRONLY),
            ("O_RDWR", libc::O_RDWR),
            ("O_CREAT", libc::O_CREAT),
            ("O_EXCL", libc::O_EXCL),
            ("O_NOCTTY", libc::O_NOCTTY),
            ("O_TRUNC", libc::O_TRUNC),
            ("O_APPEND", libc::O_APPEND),
            ("O_DIRECTORY", libc::O_DIRECTORY),
            ("O_NOFOLLOW", libc::O_NOFOLLOW),
            ("O_SYNC", libc::O_SYNC),
            ("O_DSYNC", libc::O_DSYNC),
            ("O_NONBLOCK", libc::O_NONBLOCK),
        ];
        #[cfg(target_os = "linux")]
        flags.extend([("O_NOATIME", libc::O_NOATIME), ("O_DIRECT", libc::O_DIRECT)]);
        #[cfg(target_os = "macos")]
        flags.push(("O_SYMLINK", libc::O_SYMLINK));
        flags
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        vec![
            ("O_RDONLY", 0),
            ("O_WRONLY", 1),
            ("O_RDWR", 2),
            ("O_CREAT", 64),
            ("O_EXCL", 128),
            ("O_NOCTTY", 256),
            ("O_TRUNC", 512),
            ("O_APPEND", 1024),
            ("O_DIRECTORY", 65536),
            ("O_NOATIME", 262144),
            ("O_NOFOLLOW", 131072),
            ("O_SYNC", 1052672),
            ("O_DSYNC", 4096),
            ("O_DIRECT", 16384),
            ("O_NONBLOCK", 2048),
        ]
    }
}

/// Run the blocking operation `op` on one of four worker threads and return its result. The
/// calling thread keeps running.
#[cfg(not(target_arch = "wasm32"))]
pub async fn offload<T: Send + 'static>(op: impl FnOnce() -> T + Send + 'static) -> T {
    let (sender, receiver) = futures_channel::oneshot::channel();
    workers::submit(Box::new(move || {
        let _ = sender.send(op());
    }));
    receiver.await.expect("worker threads run every job")
}

/// Run the blocking operation `op` on the calling thread when the returned future is first
/// polled, and return its result.
#[cfg(target_arch = "wasm32")]
pub async fn offload<T>(op: impl FnOnce() -> T) -> T {
    op()
}

/// The worker threads of [`offload`].
#[cfg(not(target_arch = "wasm32"))]
mod workers {
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Arc, Mutex, OnceLock};

    type Job = Box<dyn FnOnce() + Send>;

    const WORKERS: usize = 4;

    /// The sending end of the job queue, created with the worker threads on first use.
    static QUEUE: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();

    /// Queue `job` for the first idle worker thread.
    pub(super) fn submit(job: Job) {
        let queue = QUEUE.get_or_init(|| {
            let (sender, receiver) = channel::<Job>();
            let receiver = Arc::new(Mutex::new(receiver));
            for index in 0..WORKERS {
                let receiver = receiver.clone();
                std::thread::Builder::new()
                    .name(format!("fs-worker-{index}"))
                    .spawn(move || run(&receiver))
                    .expect("spawning a worker thread succeeds");
            }
            Mutex::new(sender)
        });
        queue
            .lock()
            .expect("the queue's lock is never poisoned")
            .send(job)
            .expect("worker threads never exit");
    }

    /// Run jobs from `receiver` until the queue closes.
    fn run(receiver: &Mutex<Receiver<Job>>) {
        loop {
            let job = receiver
                .lock()
                .expect("the queue's lock is never poisoned")
                .recv();
            match job {
                Ok(job) => job(),
                Err(_) => return,
            }
        }
    }
}
