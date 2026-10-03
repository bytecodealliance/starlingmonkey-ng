// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Asynchronous filesystem operations over `wasi:filesystem`, which suspend the calling task
//! instead of blocking it.
//!
//! Paths resolve against the component's preopened directories the way wasi-libc resolves them
//! for `std::fs`, and errors are the `io::Error`s of wasi-libc's error numbers, so results match
//! those of the synchronous operations on the same paths. Each operation looks the preopened
//! directories up anew: descriptors from an earlier call, such as one made before a Wizer
//! snapshot, are never reused.

use std::future::IntoFuture;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use wasip3::clocks::system_clock::Instant;
use wasip3::filesystem::preopens::get_directories;
use wasip3::filesystem::types::{
    Descriptor, DescriptorFlags, DescriptorStat, DescriptorType, ErrorCode, NewTimestamp,
    OpenFlags, PathFlags,
};
use wit_bindgen::{FutureReader, StreamResult};

use super::{millis, FileKind, Stat};

/// The capacity of each read of a stream that is read to its end.
const CHUNK: usize = 64 * 1024;

/// `code` as the `io::Error` of the error number wasi-libc gives it.
fn error(code: ErrorCode) -> io::Error {
    let errno = match code {
        ErrorCode::Access => 2,
        ErrorCode::Already => 7,
        ErrorCode::BadDescriptor => 8,
        ErrorCode::Busy => 10,
        ErrorCode::Deadlock => 16,
        ErrorCode::Quota => 19,
        ErrorCode::Exist => 20,
        ErrorCode::FileTooLarge => 22,
        ErrorCode::IllegalByteSequence => 25,
        ErrorCode::InProgress => 26,
        ErrorCode::Interrupted => 27,
        ErrorCode::Invalid => 28,
        ErrorCode::Io => 29,
        ErrorCode::IsDirectory => 31,
        ErrorCode::Loop => 32,
        ErrorCode::TooManyLinks => 34,
        ErrorCode::MessageSize => 35,
        ErrorCode::NameTooLong => 37,
        ErrorCode::NoDevice => 43,
        ErrorCode::NoEntry => 44,
        ErrorCode::NoLock => 46,
        ErrorCode::InsufficientMemory => 48,
        ErrorCode::InsufficientSpace => 51,
        ErrorCode::NotDirectory => 54,
        ErrorCode::NotEmpty => 55,
        ErrorCode::NotRecoverable => 56,
        ErrorCode::Unsupported => 58,
        ErrorCode::NoTty => 59,
        ErrorCode::NoSuchDevice => 60,
        ErrorCode::Overflow => 61,
        ErrorCode::NotPermitted => 63,
        ErrorCode::Pipe => 64,
        ErrorCode::ReadOnly => 69,
        ErrorCode::InvalidSeek => 70,
        ErrorCode::TextFileBusy => 74,
        ErrorCode::CrossDevice => 75,
        ErrorCode::Other(_) => 29,
    };
    io::Error::from_raw_os_error(errno)
}

/// `path` made absolute against the current working directory, as wasi-libc does: an empty
/// path, `.` and `./` are the working directory itself, a leading `./` is dropped, and nothing
/// else is normalized.
fn make_absolute(path: &str) -> io::Result<String> {
    if path.starts_with('/') {
        return Ok(path.to_string());
    }
    let cwd = std::env::current_dir()?;
    let cwd = cwd.to_string_lossy();
    if matches!(path, "" | "." | "./") {
        return Ok(cwd.into_owned());
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let separator = if cwd.ends_with('/') { "" } else { "/" };
    Ok(format!("{cwd}{separator}{path}"))
}

/// The name of a preopened directory without its leading `/`, `./` and `.` parts, as wasi-libc
/// registers it.
fn strip_prefixes(mut name: &str) -> &str {
    loop {
        if let Some(rest) = name.strip_prefix('/') {
            name = rest;
        } else if let Some(rest) = name.strip_prefix("./") {
            name = rest;
        } else if name == "." {
            name = "";
        } else {
            return name;
        }
    }
}

/// Whether the preopen name `prefix` names a directory containing `path`, which has no leading
/// `/`: the empty prefix contains every path, and other prefixes match whole path components,
/// ignoring the prefix's trailing `/`s.
fn prefix_matches(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    if !path.as_bytes().starts_with(prefix.as_bytes()) {
        return false;
    }
    let end = prefix.trim_end_matches('/').len();
    matches!(path.as_bytes().get(end), None | Some(b'/'))
}

/// The preopened directory containing `path` and the path relative to it, which is `.` for the
/// directory itself. `path` is made absolute against the working directory, and the preopen with
/// the longest matching name wins, the one listed last among equally long ones. No match gives
/// `ENOENT`.
fn resolve(path: &str) -> io::Result<(Descriptor, String)> {
    let absolute = make_absolute(path)?;
    let absolute = absolute.trim_start_matches('/');
    let mut best: Option<(Descriptor, usize)> = None;
    for (descriptor, name) in get_directories().into_iter().rev() {
        let prefix = strip_prefixes(&name);
        let longer = best
            .as_ref()
            .is_none_or(|(_, length)| prefix.len() > *length);
        if longer && prefix_matches(prefix, absolute) {
            best = Some((descriptor, prefix.len()));
        }
    }
    let Some((descriptor, length)) = best else {
        return Err(error(ErrorCode::NoEntry));
    };
    let relative = absolute[length..].trim_start_matches('/');
    let relative = if relative.is_empty() { "." } else { relative };
    Ok((descriptor, relative.to_string()))
}

/// The kind of an entry of type `kind`. Only files, directories and symlinks are told apart, as
/// `std::fs` on WASI tells them apart.
fn kind_of(kind: &DescriptorType) -> FileKind {
    match kind {
        DescriptorType::RegularFile => FileKind::File,
        DescriptorType::Directory => FileKind::Directory,
        DescriptorType::SymbolicLink => FileKind::Symlink,
        _ => FileKind::Unknown,
    }
}

/// `instant` as a `SystemTime`, or `None` if `SystemTime` can't represent it.
fn system_time(instant: Instant) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(instant.nanoseconds));
    match u64::try_from(instant.seconds) {
        Ok(seconds) => SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds) + nanos),
        Err(_) => SystemTime::UNIX_EPOCH
            .checked_sub(Duration::from_secs(instant.seconds.unsigned_abs()))?
            .checked_add(nanos),
    }
}

/// The milliseconds since the Unix epoch of `instant`, or `None` if `SystemTime` can't represent
/// it. A missing time is the epoch, as wasi-libc reports it.
fn instant_millis(instant: Option<Instant>) -> Option<f64> {
    match instant {
        Some(instant) => millis(Ok(system_time(instant)?)),
        None => Some(0.0),
    }
}

/// `stat` as [`Stat`], with the placeholders `std::fs` metadata has on WASI. As there, the
/// creation time is the status change time.
fn to_stat(stat: DescriptorStat) -> Stat {
    Stat::without_modes(
        kind_of(&stat.type_),
        stat.size,
        false,
        instant_millis(stat.data_access_timestamp),
        instant_millis(stat.data_modification_timestamp),
        instant_millis(stat.status_change_timestamp),
    )
}

/// `time` as a `wasi:filesystem` timestamp, with `None` leaving the timestamp unchanged.
fn new_timestamp(time: Option<SystemTime>) -> NewTimestamp {
    let Some(time) = time else {
        return NewTimestamp::NoChange;
    };
    let (seconds, nanoseconds) = match time.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => (after.as_secs() as i64, after.subsec_nanos()),
        Err(before) => {
            let before = before.duration();
            match before.subsec_nanos() {
                0 => (-(before.as_secs() as i64), 0),
                nanos => (-(before.as_secs() as i64) - 1, 1_000_000_000 - nanos),
            }
        }
    };
    NewTimestamp::Timestamp(Instant {
        seconds,
        nanoseconds,
    })
}

/// The metadata of the entry at `path`, following a final symlink if `follow`.
pub async fn stat(path: &str, follow: bool) -> io::Result<Stat> {
    let (directory, relative) = resolve(path)?;
    let flags = if follow {
        PathFlags::SYMLINK_FOLLOW
    } else {
        PathFlags::empty()
    };
    let stat = directory.stat_at(flags, relative).await.map_err(error)?;
    Ok(to_stat(stat))
}

/// The names and kinds of the entries of the directory `path`, in the order the host lists them,
/// without `.` and `..`.
pub async fn read_dir(path: &str) -> io::Result<Vec<(String, FileKind)>> {
    let (directory, relative) = resolve(path)?;
    let directory = directory
        .open_at(
            PathFlags::SYMLINK_FOLLOW,
            relative,
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ,
        )
        .await
        .map_err(error)?;
    let (mut entries, result) = directory.read_directory();
    let mut listed = Vec::new();
    loop {
        let (status, batch) = entries.read(Vec::with_capacity(64)).await;
        listed.extend(
            batch
                .into_iter()
                .map(|entry| (entry.name, kind_of(&entry.type_))),
        );
        if status == StreamResult::Dropped {
            break;
        }
    }
    result.await.map_err(error)?;
    Ok(listed)
}

/// How to open a file, with the meaning of the `std::fs::OpenOptions` setting of each name.
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenOptions {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
}

impl OpenOptions {
    /// The descriptor flags and open flags of these options, or the `InvalidInput` error
    /// `std::fs::OpenOptions::open` gives for a combination it rejects.
    fn flags(self) -> io::Result<(DescriptorFlags, OpenFlags)> {
        let invalid = |message| Err(io::Error::new(io::ErrorKind::InvalidInput, message));
        if !self.read && !self.write && !self.append {
            return invalid(if self.create || self.create_new || self.truncate {
                "creating or truncating a file requires write or append access"
            } else {
                "must specify at least one of read, write, or append access"
            });
        }
        if !self.write && !self.append && (self.truncate || self.create || self.create_new) {
            return invalid("creating or truncating a file requires write or append access");
        }
        if self.append && self.truncate && !self.create_new {
            return invalid("append and truncate cannot both be enabled");
        }
        let mut descriptor = DescriptorFlags::empty();
        if self.read {
            descriptor |= DescriptorFlags::READ;
        }
        if self.write || self.append {
            descriptor |= DescriptorFlags::WRITE | DescriptorFlags::MUTATE_DIRECTORY;
        }
        let open = if self.create_new {
            OpenFlags::CREATE | OpenFlags::EXCLUSIVE
        } else {
            let mut open = OpenFlags::empty();
            if self.create {
                open |= OpenFlags::CREATE;
            }
            if self.truncate {
                open |= OpenFlags::TRUNCATE;
            }
            open
        };
        Ok((descriptor, open))
    }
}

/// An open file, which closes when dropped.
///
/// Reads and writes without a position use the file's own position, which starts at 0 and
/// advances by the bytes read or written. In append mode, writes without a position go to the
/// end of the file and leave the position unchanged.
#[derive(Debug)]
pub struct File {
    descriptor: Descriptor,
    position: AtomicU64,
    append: bool,
}

impl File {
    /// Open the file at `path` with `options`, following symlinks.
    pub async fn open(path: &str, options: OpenOptions) -> io::Result<File> {
        let (descriptor_flags, open_flags) = options.flags()?;
        let (directory, relative) = resolve(path)?;
        let descriptor = directory
            .open_at(
                PathFlags::SYMLINK_FOLLOW,
                relative,
                open_flags,
                descriptor_flags,
            )
            .await
            .map_err(error)?;
        Ok(File {
            descriptor,
            position: AtomicU64::new(0),
            append: options.append,
        })
    }

    /// Read at most `length` bytes at `position`, or at the file's position for `None`, which
    /// then advances. Fewer bytes are read only at the end of the file. Fails with `ENOMEM` if a
    /// buffer of `length` bytes can't be allocated.
    pub async fn read(&self, length: usize, position: Option<u64>) -> io::Result<Vec<u8>> {
        if length == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| error(ErrorCode::InsufficientMemory))?;
        let offset = position.unwrap_or_else(|| self.position.load(Ordering::Relaxed));
        let (mut stream, result) = self.descriptor.read_via_stream(offset);
        let mut ended = false;
        while bytes.len() < length {
            let status;
            (status, bytes) = stream.read(bytes).await;
            if status == StreamResult::Dropped {
                ended = true;
                break;
            }
        }
        drop(stream);
        if ended {
            result.await.map_err(error)?;
        }
        if position.is_none() {
            self.position
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        }
        Ok(bytes)
    }

    /// Read everything from the file's position to the end of the file, and advance the position
    /// past it. Fails with `ENOMEM` once the bytes read don't fit in memory.
    pub async fn read_to_end(&self) -> io::Result<Vec<u8>> {
        let (mut stream, result) = self
            .descriptor
            .read_via_stream(self.position.load(Ordering::Relaxed));
        let mut bytes = Vec::new();
        loop {
            bytes
                .try_reserve(CHUNK)
                .map_err(|_| error(ErrorCode::InsufficientMemory))?;
            let status;
            (status, bytes) = stream.read(bytes).await;
            if status == StreamResult::Dropped {
                break;
            }
        }
        result.await.map_err(error)?;
        self.position
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    }

    /// Write all of `data` at `position`, or for `None` at the file's position, which then
    /// advances, or in append mode at the end of the file. Empty data is written too, as an empty
    /// stream.
    pub async fn write(&self, data: Vec<u8>, position: Option<u64>) -> io::Result<()> {
        let length = data.len() as u64;
        let (mut writer, reader) = wasip3::wit_stream::new::<u8>();
        let result = match position {
            Some(position) => self.descriptor.write_via_stream(reader, position),
            None if self.append => self.descriptor.append_via_stream(reader),
            None => self
                .descriptor
                .write_via_stream(reader, self.position.load(Ordering::Relaxed)),
        };
        let send = async move {
            if !data.is_empty() {
                // Unwritten bytes mean the host stopped reading, and `result` reports why.
                let _ = writer.write_all(data).await;
            }
        };
        finish(send, result).await?;
        if position.is_none() && !self.append {
            self.position.fetch_add(length, Ordering::Relaxed);
        }
        Ok(())
    }

    /// The metadata of the file.
    pub async fn stat(&self) -> io::Result<Stat> {
        Ok(to_stat(self.descriptor.stat().await.map_err(error)?))
    }

    /// Truncate or extend the file to `size` bytes.
    pub async fn set_size(&self, size: u64) -> io::Result<()> {
        self.descriptor.set_size(size).await.map_err(error)
    }

    /// Flush the file's data and metadata to storage.
    pub async fn sync(&self) -> io::Result<()> {
        self.descriptor.sync().await.map_err(error)
    }

    /// Flush the file's data to storage.
    pub async fn sync_data(&self) -> io::Result<()> {
        self.descriptor.sync_data().await.map_err(error)
    }

    /// Set the file's access and modification times. `None` leaves a time unchanged.
    pub async fn set_times(
        &self,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> io::Result<()> {
        self.descriptor
            .set_times(new_timestamp(accessed), new_timestamp(modified))
            .await
            .map_err(error)
    }
}

/// Run `send`, which writes a stream and then drops its writer, together with `result`, the
/// outcome of the host reading that stream, and return the outcome.
async fn finish(
    send: impl std::future::Future<Output = ()>,
    result: FutureReader<Result<(), ErrorCode>>,
) -> io::Result<()> {
    let ((), result) = futures_lite::future::zip(send, result.into_future()).await;
    result.map_err(error)
}
