// `node:fs` and `node:fs/promises`, in a temporary directory under `/tmp`, which the wasm runner
// preopens. Error shapes are Node's.

import fs, {
  closeSync,
  constants,
  createReadStream,
  createWriteStream,
  existsSync,
  mkdirSync,
  openSync,
  promises,
  readdirSync,
  readFileSync,
  readSync,
  rmSync,
  statSync,
  writeFileSync,
  writeSync,
} from "node:fs";
import fsp from "node:fs/promises";
import { pipeline } from "node:stream/promises";
import { inspect } from "node:util";
import { eq, deepEq, throws } from "./harness.mjs";

const wasm = process.arch === "wasm32";
const dir = fs.mkdtempSync("/tmp/starling-fs-");
const file = `${dir}/file.txt`;
const sorted = (names) => [...names].sort();
const failure = (fn) => {
  try {
    fn();
  } catch (error) {
    return error;
  }
  throw new Error("expected an error");
};

try {
  // Reading and writing whole files.
  eq(/^\/tmp\/starling-fs-[A-Za-z0-9]{6}$/.test(dir), true, "mkdtemp appends six characters");
  writeFileSync(file, "héllo");
  eq(readFileSync(file, "utf8"), "héllo", "writeFileSync and readFileSync with an encoding");
  eq(readFileSync(file, { encoding: "hex" }), "68c3a96c6c6f", "encoding option");
  eq(Buffer.isBuffer(readFileSync(file)), true, "readFileSync returns a Buffer without an encoding");
  fs.appendFileSync(file, Buffer.from(" world"));
  eq(readFileSync(new URL(`file://${file}`), "utf8"), "héllo world", "appendFileSync, and file: URLs");
  writeFileSync(file, "x", { flag: "a" });
  eq(readFileSync(Buffer.from(file), "latin1").length, 13, "the flag option, and Buffer paths");
  eq(existsSync(file) && !existsSync(`${dir}/missing`) && !existsSync({}), true, "existsSync");

  // Stats.
  const stats = statSync(file);
  eq(stats instanceof fs.Stats, true, "Stats");
  eq(stats.isFile() && !stats.isDirectory() && statSync(dir).isDirectory(), true, "isFile and isDirectory");
  eq(stats.size, 13, "size");
  eq(stats.mode & constants.S_IFMT, constants.S_IFREG, "the mode's type bits");
  eq(stats.mtime instanceof Date && stats.mtime === stats.mtime, true, "mtime is a cached Date");
  eq(stats.mtime.getTime(), Math.round(stats.mtimeMs), "mtime matches mtimeMs");
  eq(statSync(`${dir}/missing`, { throwIfNoEntry: false }), undefined, "throwIfNoEntry");

  // Errors.
  const missing = failure(() => readFileSync(`${dir}/missing`));
  eq(missing.message, `ENOENT: no such file or directory, open '${dir}/missing'`, "error message");
  deepEq(Object.keys(missing), ["errno", "code", "syscall", "path"], "error properties");
  eq(missing.errno, -2, "errno");
  const renamed = failure(() => fs.renameSync(`${dir}/a`, `${dir}/b`));
  deepEq([renamed.code, renamed.syscall, renamed.dest], ["ENOENT", "rename", `${dir}/b`], "dest");
  throws(() => readFileSync(123.5), "ERR_INVALID_ARG_TYPE", "path types");
  throws(() => openSync(file, "q"), "ERR_INVALID_ARG_VALUE", "flags");
  throws(() => readFileSync(file, "bogus"), "ERR_INVALID_ARG_VALUE", "encodings");
  eq(failure(() => readFileSync(dir)).code, "EISDIR", "reading a directory");
  eq(failure(() => writeFileSync(file, "", { flag: "wx" })).code, "EEXIST", "exclusive flags");

  // Directories.
  eq(mkdirSync(`${dir}/a/b/c`, { recursive: true }), `${dir}/a`, "recursive mkdir returns the first directory");
  eq(mkdirSync(`${dir}/a/b/c`, { recursive: true }), undefined, "recursive mkdir of an existing directory");
  eq(failure(() => mkdirSync(`${dir}/a`)).code, "EEXIST", "mkdir of an existing directory");
  const notDir = failure(() => mkdirSync(`${file}/x/y`, { recursive: true }));
  deepEq([notDir.code, notDir.path], ["ENOTDIR", `${file}/x/y`], "recursive mkdir below a file names the path");
  writeFileSync(`${dir}/a/b/inner.txt`, "");
  deepEq(sorted(readdirSync(dir)), ["a", "file.txt"], "readdirSync");
  deepEq(sorted(readdirSync(dir, { recursive: true })), ["a", "a/b", "a/b/c", "a/b/inner.txt", "file.txt"], "recursive readdir");
  const entries = readdirSync(dir, { withFileTypes: true }).sort((x, y) => (x.name < y.name ? -1 : 1));
  deepEq(entries.map((entry) => [entry.name, entry.isDirectory(), entry.isFile(), entry.parentPath]), [
    ["a", true, false, dir],
    ["file.txt", false, true, dir],
  ], "Dirent");
  eq(entries[0] instanceof fs.Dirent, true, "Dirent instances");
  eq(failure(() => fs.rmdirSync(`${dir}/a`)).code, "ENOTEMPTY", "rmdir of a non-empty directory");
  const isDirectory = failure(() => rmSync(`${dir}/a`));
  deepEq([isDirectory.name, isDirectory.code, isDirectory.syscall], ["SystemError", "ERR_FS_EISDIR", "rm"], "rm of a directory");
  // `errno`, `syscall` and `path` are accessors over `info`, and `dest` is absent without one.
  eq(Object.getOwnPropertyDescriptor(isDirectory, "errno").get !== undefined, true, "errno is an accessor");
  eq("dest" in isDirectory, false, "a SystemError without a dest");
  eq(inspect(isDirectory).startsWith("SystemError [ERR_FS_EISDIR]: "), true, "the stack header of a SystemError has its code");
  isDirectory.path = "elsewhere";
  eq(Buffer.isBuffer(isDirectory.info.path), true, "setting path stores a Buffer in info");
  eq(isDirectory.path, "elsewhere", "path reads info.path as a string");
  eq(failure(() => readdirSync(file)).code, "ENOTDIR", "readdir of a file");

  // Copying, renaming, removing, times and access.
  fs.copyFileSync(file, `${dir}/copy.txt`);
  eq(readFileSync(`${dir}/copy.txt`, "utf8"), readFileSync(file, "utf8"), "copyFileSync");
  eq(failure(() => fs.copyFileSync(file, `${dir}/copy.txt`, constants.COPYFILE_EXCL)).code, "EEXIST", "COPYFILE_EXCL");
  fs.renameSync(`${dir}/copy.txt`, `${dir}/moved.txt`);
  fs.utimesSync(`${dir}/moved.txt`, 1000, new Date(2000_000));
  eq(statSync(`${dir}/moved.txt`).mtimeMs, 2000_000, "utimesSync");
  fs.truncateSync(`${dir}/moved.txt`, 2);
  eq(readFileSync(`${dir}/moved.txt`, "utf8"), "h\ufffd", "truncateSync");
  fs.accessSync(`${dir}/moved.txt`, constants.R_OK | constants.W_OK);
  eq(failure(() => fs.accessSync(`${dir}/missing`)).syscall, "access", "accessSync");
  fs.unlinkSync(`${dir}/moved.txt`);
  rmSync(`${dir}/missing`, { force: true });
  eq(existsSync(`${dir}/moved.txt`), false, "unlinkSync");
  eq(fs.realpathSync(`${dir}/a/b/..`), fs.realpathSync(`${dir}/a`), "realpathSync");
  eq(fs.realpathSync.native(dir), fs.realpathSync(dir), "realpathSync.native");
  if (!wasm) {
    fs.symlinkSync(file, `${dir}/link`);
    eq(fs.lstatSync(`${dir}/link`).isSymbolicLink(), true, "lstat of a symlink");
    eq(fs.readlinkSync(`${dir}/link`), file, "readlinkSync");
    eq(fs.realpathSync(`${dir}/link`), fs.realpathSync(file), "realpath follows symlinks");
  }

  // File descriptors.
  const fd = openSync(`${dir}/fd.txt`, "w+");
  eq(fd >= 3, true, "descriptors start at 3");
  eq(writeSync(fd, "abcdef"), 6, "writeSync of a string");
  eq(writeSync(fd, Buffer.from("XY"), 0, 2, 1), 2, "positional writeSync");
  const buffer = Buffer.alloc(4);
  eq(readSync(fd, buffer, 0, 4, 0), 4, "positional readSync");
  eq(buffer.toString(), "aXYd", "positional writes keep the file position");
  eq(fs.fstatSync(fd).size, 6, "fstatSync");
  fs.ftruncateSync(fd, 3);
  eq(readSync(fd, buffer, { position: 0 }), 3, "readSync with options and ftruncateSync");
  const window = Buffer.alloc(6, ".");
  eq(readSync(fd, window, 2, 2, 1), 2, "readSync into an offset of the buffer");
  eq(window.toString(), "..XY..", "readSync writes only the bytes read");
  eq(writeSync(fd, new Uint8Array([0x41, 0x42, 0x43]), 1, 1, 0), 1, "writeSync of a slice");
  eq(readFileSync(`${dir}/fd.txt`, "latin1"), "BXY", "writeSync writes only the slice");
  closeSync(fd);
  eq(failure(() => closeSync(fd)).code, "EBADF", "closing twice");
  eq(failure(() => readSync(fd, buffer)).syscall, "read", "reading a closed descriptor");

  // Callbacks.
  const callback = (run) => new Promise((resolve) => run((...args) => resolve(args)));
  deepEq(await callback((done) => fs.readFile(file, "utf8", done)), [null, "héllo worldx"], "readFile with a callback");
  deepEq(await callback((done) => fs.stat(`${dir}/missing`, { throwIfNoEntry: false }, done)), [null, undefined], "stat with throwIfNoEntry");
  deepEq(await callback((done) => fs.exists(file, done)), [true], "exists passes no error");
  const [error] = await callback((done) => fs.readFile(`${dir}/missing`, done));
  deepEq([error.code, error.path], ["ENOENT", `${dir}/missing`], "callback errors");
  const [openError, callbackFd] = await callback((done) => fs.open(`${dir}/cb.txt`, "w+", done));
  eq(openError, null, "open with a callback");
  const written = await callback((done) => fs.write(callbackFd, Buffer.from("data"), 0, 4, null, done));
  deepEq([written[0], written[1], written[2].toString()], [null, 4, "data"], "write passes the count and buffer");
  const read = await callback((done) => fs.read(callbackFd, Buffer.alloc(4), 0, 4, 0, done));
  deepEq([read[0], read[1], read[2].toString()], [null, 4, "data"], "read passes the count and buffer");
  deepEq(await callback((done) => fs.close(callbackFd, done)), [null], "close with a callback");
  const noCallbackFd = openSync(file, "r");
  eq(fs.close(noCallbackFd), undefined, "close without a callback");
  throws(() => fs.close("x"), "ERR_INVALID_ARG_TYPE", "close without a callback validates the descriptor");
  throws(() => fs.close(noCallbackFd, null), "ERR_INVALID_ARG_TYPE", "close with a callback that isn't a function");
  await new Promise((resolve) => setTimeout(resolve, 50));
  eq(failure(() => fs.fstatSync(noCallbackFd)).code, "EBADF", "close without a callback closes the descriptor");
  throws(() => fs.readFile(file), "ERR_INVALID_ARG_TYPE", "callback functions need a callback");

  // Descriptors from callback opens, with callback and synchronous functions. On wasm, these
  // are asynchronous files that the synchronous functions block on.
  const [, cbFd] = await callback((done) => fs.open(`${dir}/cb.txt`, "r+", done));
  eq(writeSync(cbFd, "ab"), 2, "writeSync to a callback descriptor");
  await callback((done) => fs.write(cbFd, "cd", null, "utf8", done));
  const sequential = Buffer.alloc(8);
  deepEq((await callback((done) => fs.read(cbFd, sequential, 0, 8, null, done))).slice(0, 2), [null, 0], "writes advance the position");
  eq(readSync(cbFd, sequential, 0, 8, 0), 4, "readSync from a callback descriptor");
  eq(sequential.toString("utf8", 0, 4), "abcd", "positional reads");
  const [, readerFd] = await callback((done) => fs.open(`${dir}/cb.txt`, done));
  const parts = [];
  for (let i = 0; i < 3; i++) {
    const [, count, chunk] = await callback((done) => fs.read(readerFd, Buffer.alloc(3), 0, 3, null, done));
    parts.push(chunk.toString("utf8", 0, count));
  }
  deepEq(parts, ["abc", "d", ""], "reads advance the position");
  deepEq(await callback((done) => fs.close(readerFd, done)), [null], "close of a callback descriptor");
  eq(fs.fstatSync(cbFd).size, 4, "fstatSync of a callback descriptor");
  deepEq(await callback((done) => fs.ftruncate(cbFd, 1, done)), [null], "ftruncate with a callback");
  deepEq(await callback((done) => fs.fsync(cbFd, done)), [null], "fsync with a callback");
  deepEq(await callback((done) => fs.fdatasync(cbFd, done)), [null], "fdatasync with a callback");
  deepEq(await callback((done) => fs.futimes(cbFd, 1000, 3000, done)), [null], "futimes with a callback");
  const [, cbStats] = await callback((done) => fs.fstat(cbFd, done));
  deepEq([cbStats.size, cbStats.mtimeMs], [1, 3000_000], "fstat with a callback");
  fs.ftruncateSync(cbFd, 0);
  deepEq(await callback((done) => fs.readFile(cbFd, done)), [null, Buffer.alloc(0)], "readFile of a descriptor at its end");
  closeSync(cbFd);
  eq(failure(() => fs.fstatSync(cbFd)).code, "EBADF", "closeSync of a callback descriptor");
  await callback((done) => fs.appendFile(`${dir}/cb.txt`, "appended", done));
  deepEq(await callback((done) => fs.truncate(`${dir}/cb.txt`, 3, done)), [null], "truncate with a callback");
  eq(readFileSync(`${dir}/cb.txt`, "utf8"), "app", "appendFile and truncate with callbacks");
  deepEq(await callback((done) => fs.utimes(`${dir}/cb.txt`, 1000, 4000, done)), [null], "utimes with a callback");
  eq(statSync(`${dir}/cb.txt`).mtimeMs, 4000_000, "utimes sets the time");

  // The callback and promise functions report what the synchronous functions report.
  const rejection = (promise) =>
    promise.then(
      () => {
        throw new Error("expected a rejection");
      },
      (error) => error,
    );
  const shape = (error) => [error.message, error.code, error.errno, error.syscall, error.path];
  const parity = async (name, sync, promised) => deepEq(shape(await rejection(promised())), shape(failure(sync)), name);
  await parity("stat of a missing file", () => statSync(`${dir}/missing`), () => fsp.stat(`${dir}/missing`));
  await parity("lstat through a file", () => fs.lstatSync(`${file}/x`), () => fsp.lstat(`${file}/x`));
  await parity("stat through a file", () => statSync(`${file}/x`), () => fsp.stat(`${file}/x`));
  await parity("readFile of a directory", () => readFileSync(dir), () => fsp.readFile(dir));
  await parity("readFile of a missing file", () => readFileSync(`${dir}/missing`), () => fsp.readFile(`${dir}/missing`));
  await parity("open with wx", () => openSync(file, "wx"), () => fsp.open(file, "wx"));
  await parity("readdir of a file", () => readdirSync(file), () => fsp.readdir(file));
  await parity("readdir of a missing directory", () => readdirSync(`${dir}/missing`), () => fsp.readdir(`${dir}/missing`));
  await parity("access of a missing file", () => fs.accessSync(`${dir}/missing`), () => fsp.access(`${dir}/missing`));
  await parity("writeFile in a missing directory", () => writeFileSync(`${dir}/missing/x`, "a"), () => fsp.writeFile(`${dir}/missing/x`, "a"));
  await parity("truncate of a missing file", () => fs.truncateSync(`${dir}/missing`), () => fsp.truncate(`${dir}/missing`));
  await parity("utimes of a missing file", () => fs.utimesSync(`${dir}/missing`, 1, 1), () => fsp.utimes(`${dir}/missing`, 1, 1));
  const syncReadOnly = openSync(file, "r");
  const asyncReadOnly = await fsp.open(file, "r");
  await parity("write to a read-only descriptor", () => writeSync(syncReadOnly, "x"), () => asyncReadOnly.write("x"));
  closeSync(syncReadOnly);
  await asyncReadOnly.close();
  const syncWriteOnly = openSync(`${dir}/cb.txt`, "a");
  const asyncWriteOnly = await fsp.open(`${dir}/cb.txt`, "a");
  await parity("read from a write-only descriptor", () => readSync(syncWriteOnly, Buffer.alloc(1)), () => asyncWriteOnly.read(Buffer.alloc(1)));
  closeSync(syncWriteOnly);
  await asyncWriteOnly.close();
  const statFields = (stats) => [stats.size, stats.mode, stats.nlink, stats.ino, stats.dev, stats.blocks, stats.atimeMs, stats.mtimeMs, stats.ctimeMs, stats.birthtimeMs];
  deepEq(statFields(await fsp.stat(file)), statFields(statSync(file)), "promise stat matches statSync");
  deepEq(statFields(await fsp.lstat(dir)), statFields(fs.lstatSync(dir)), "promise lstat matches lstatSync");
  mkdirSync(`${dir}/sub/deeper`, { recursive: true });
  writeFileSync(`${dir}/sub/deeper/leaf.txt`, "");
  deepEq(await fsp.readdir(dir, { recursive: true }), readdirSync(dir, { recursive: true }), "promise readdir matches readdirSync");
  const kinds = (entries) => entries.map((entry) => [entry.name, entry.isFile(), entry.isDirectory(), entry.parentPath]);
  deepEq(kinds(await fsp.readdir(dir, { withFileTypes: true })), kinds(readdirSync(dir, { withFileTypes: true })), "promise Dirents match");
  deepEq(await callback((done) => fs.exists(`${dir}/missing`, done)), [false], "exists of a missing file");
  rmSync(`${dir}/sub`, { recursive: true });
  if (wasm) {
    // The runner preopens the working directory as `/cwd` and as `.`, and relative paths resolve
    // against `/`, which the `.` preopen contains.
    const manifest = readFileSync("/cwd/Cargo.toml", "utf8");
    eq(await fsp.readFile("Cargo.toml", "utf8"), manifest, "relative paths use the . preopen");
    eq(await fsp.readFile("./Cargo.toml", "utf8"), manifest, "a leading ./ is dropped");
    eq(await fsp.readFile("/Cargo.toml", "utf8"), manifest, "absolute paths use the . preopen");
    eq(await fsp.readFile("/cwd/Cargo.toml", "utf8"), manifest, "/cwd paths use the /cwd preopen");
    eq((await fsp.stat("/cwd")).isDirectory(), true, "a preopen itself");
    await parity("an escaping path", () => statSync("/cwd/../x"), () => fsp.stat("/cwd/../x"));
  }

  // Promises.
  eq(fsp.readFile, promises.readFile, "fs.promises and node:fs/promises share functions");
  await fsp.writeFile(`${dir}/promise.txt`, "promised");
  eq(await fsp.readFile(`${dir}/promise.txt`, "utf8"), "promised", "promise readFile and writeFile");
  deepEq(sorted(await fsp.readdir(dir)).includes("promise.txt"), true, "promise readdir");
  eq((await fsp.stat(`${dir}/promise.txt`)).size, 8, "promise stat");
  eq((await fsp.readFile(`${dir}/missing`).catch((error) => error)).code, "ENOENT", "promise rejections");
  eq((await fsp.readFile(123.5).catch((error) => error)).code, "ERR_INVALID_ARG_TYPE", "argument errors reject");
  const handle = await fsp.open(`${dir}/promise.txt`, "r+");
  eq(typeof handle.fd, "number", "FileHandle");
  const { bytesRead, buffer: handleBuffer } = await handle.read(Buffer.alloc(3), 0, 3, 0);
  deepEq([bytesRead, handleBuffer.toString()], [3, "pro"], "FileHandle.read");
  eq((await handle.write("P", 0)).bytesWritten, 1, "FileHandle.write");
  eq(await handle.readFile("utf8"), "Promised", "positional reads and writes keep the position");
  eq((await handle.stat()).size, 8, "FileHandle.stat");
  eq(await fsp.readFile(handle, "utf8"), "", "readFile of a FileHandle");
  await handle.close();
  eq(handle.fd, -1, "close resets the descriptor");
  eq(await fsp.readFile(`${dir}/promise.txt`, "utf8"), "Promised", "FileHandle writes");

  // A FileHandle's streams reference its descriptor and end when it closes, so a closed
  // handle's stream can't write to a file that reuses the descriptor.
  const streamed = await fsp.open(`${dir}/streamed.txt`, "w");
  const handleStream = streamed.createWriteStream();
  const handleEvents = [];
  for (const event of ["finish", "close", "error"]) {
    handleStream.on(event, (error) => handleEvents.push(`${event} ${error?.code ?? ""}`));
  }
  await new Promise((resolve) => handleStream.write("first", resolve));
  const closing = streamed.close();
  deepEq(
    [streamed.fd >= 0, handleStream.writableEnded, streamed.close() === closing],
    [true, true, true],
    "a FileHandle closed under a stream keeps its descriptor until the stream closes",
  );
  await closing;
  writeFileSync(`${dir}/other.txt`, "original");
  const other = await fsp.open(`${dir}/other.txt`, "r+");
  const late = await new Promise((resolve) => handleStream.write("CLOBBER", resolve));
  await other.close();
  deepEq(
    [...handleEvents, late?.code],
    ["finish ", "close ", "ERR_STREAM_WRITE_AFTER_END"],
    "a write after the FileHandle closed",
  );
  deepEq(
    [streamed.fd, readFileSync(`${dir}/streamed.txt`, "utf8"), readFileSync(`${dir}/other.txt`, "utf8")],
    [-1, "first", "original"],
    "a stream of a closed FileHandle doesn't write to another file",
  );
  const reading = await fsp.open(`${dir}/streamed.txt`, "r");
  const readEvents = [];
  const handleReader = reading.createReadStream();
  for (const event of ["end", "close", "error"]) {
    handleReader.on(event, (error) => readEvents.push(`${event} ${error?.code ?? ""}`));
  }
  await reading.close();
  deepEq([readEvents, handleReader.destroyed], [["close "], true], "closing a FileHandle destroys its read stream");
  await fsp.rm(`${dir}/a`, { recursive: true });
  eq(existsSync(`${dir}/a`), false, "promise rm");

  // Copying directory trees.
  mkdirSync(`${dir}/tree/sub`, { recursive: true });
  writeFileSync(`${dir}/tree/one.txt`, "1");
  writeFileSync(`${dir}/tree/sub/two.txt`, "2");
  fs.cpSync(`${dir}/tree`, `${dir}/copied`, { recursive: true });
  eq(readFileSync(`${dir}/copied/sub/two.txt`, "utf8"), "2", "cpSync copies directories");
  const seen = [];
  const keep = (src) => {
    seen.push(src.slice(dir.length));
    return !src.endsWith("two.txt");
  };
  fs.cpSync(`${dir}/tree`, `${dir}/filtered`, { recursive: true, filter: keep });
  deepEq([existsSync(`${dir}/filtered/one.txt`), existsSync(`${dir}/filtered/sub/two.txt`)], [true, false], "cpSync filter");
  deepEq(sorted(seen), ["/tree", "/tree/one.txt", "/tree/sub", "/tree/sub/two.txt"], "cpSync calls filter on each entry");
  const deep = `${dir}/deep/${"d/".repeat(200)}`;
  eq(mkdirSync(deep, { recursive: true }), `${dir}/deep`, "recursive mkdir of 200 levels");
  writeFileSync(`${deep}leaf.txt`, "leaf");
  fs.cpSync(`${dir}/deep`, `${dir}/deep-copy`, { recursive: true });
  eq(readFileSync(`${dir}/deep-copy/${"d/".repeat(200)}leaf.txt`, "utf8"), "leaf", "cpSync of 200 levels");
  fs.cpSync(`${dir}/deep`, `${dir}/deep-filtered`, { recursive: true, filter: () => true });
  eq(readFileSync(`${dir}/deep-filtered/${"d/".repeat(200)}leaf.txt`, "utf8"), "leaf", "cpSync of 200 levels with a filter");
  const notRecursive = failure(() => fs.cpSync(`${dir}/tree`, `${dir}/x`));
  deepEq([notRecursive.name, notRecursive.code], ["Error", "ERR_FS_EISDIR"], "cpSync of a directory without recursive");
  const exists = failure(() => fs.cpSync(`${dir}/tree/one.txt`, `${dir}/copied/one.txt`, { force: false, errorOnExist: true }));
  deepEq([exists.name, exists.code, exists.errno, exists.syscall], ["SystemError", "ERR_FS_CP_EEXIST", 17, "cp"], "errorOnExist");
  throws(() => fs.cpSync("a", "b", { filter: async () => true }), "ERR_INVALID_RETURN_VALUE", "cpSync filters can't be async");
  throws(() => fs.cpSync("a", "b", { dereference: true, verbatimSymlinks: true }), "ERR_INCOMPATIBLE_OPTION_PAIR", "incompatible options");
  throws(() => fs.cpSync("a", "b", { mode: 8 }), "ERR_OUT_OF_RANGE", "the mode option");
  await fsp.cp(`${dir}/tree`, `${dir}/promised`, { recursive: true, filter: async (src) => !src.endsWith("one.txt") });
  deepEq([existsSync(`${dir}/promised/one.txt`), readFileSync(`${dir}/promised/sub/two.txt`, "utf8")], [false, "2"], "fs.promises.cp with an async filter");
  const self = await fsp.cp(`${dir}/tree`, `${dir}/tree/sub/self`, { recursive: true }).catch((error) => error);
  deepEq([self.name, self.code, self.errno, self.path], ["SystemError", "ERR_FS_CP_EINVAL", 22, `${dir}/tree/sub/self`], "copying a directory into itself");
  eq((await fsp.cp("a", "b", { recursive: 1 }).catch((error) => error)).code, "ERR_INVALID_ARG_TYPE", "cp option errors reject");
  deepEq(await callback((done) => fs.cp(`${dir}/tree/one.txt`, `${dir}/cb/one.txt`, done)), [null, undefined], "cp with a callback");
  eq(readFileSync(`${dir}/cb/one.txt`, "utf8"), "1", "cp creates the destination's parent");
  throws(() => fs.cp("a", "b", {}), "ERR_INVALID_ARG_TYPE", "cp needs a callback");

  // Streams.
  eq(fs.FileReadStream === fs.ReadStream && fs.FileWriteStream === fs.WriteStream, true, "stream aliases");
  const out = createWriteStream(`${dir}/stream.txt`);
  const events = [];
  out.on("open", (opened) => events.push(typeof opened));
  out.on("ready", () => events.push("ready"));
  eq(out.pending, true, "pending before open");
  out.write("hello ");
  out.end("streams");
  await new Promise((resolve) => out.on("close", resolve));
  deepEq(events, ["number", "ready"], "open and ready");
  eq(out.bytesWritten, 13, "bytesWritten");
  const chunks = [];
  const input = createReadStream(`${dir}/stream.txt`, { start: 2, end: 8, highWaterMark: 4, encoding: "utf8" });
  for await (const chunk of input) chunks.push(chunk);
  deepEq(chunks, ["llo ", "str"], "ReadStream with start, end and highWaterMark");
  eq(input.bytesRead, 7, "bytesRead");
  // Assigned fields change what the streams do, as in Node.js.
  const moved = createReadStream(`${dir}/stream.txt`, { highWaterMark: 4 });
  moved.on("open", () => Object.assign(moved, { pos: 6, bytesRead: 100 }));
  const movedChunks = [];
  for await (const chunk of moved) movedChunks.push(chunk);
  deepEq([Buffer.concat(movedChunks).toString(), moved.bytesRead, moved.pos], ["streams", 107, 13], "assigning pos and bytesRead");
  const extended = createReadStream(`${dir}/stream.txt`, { start: 0, end: 1 });
  extended.on("open", () => { extended.end = 4; });
  const extendedChunks = [];
  for await (const chunk of extended) extendedChunks.push(chunk);
  eq(Buffer.concat(extendedChunks).toString(), "hello", "assigning end");
  const positioned = createWriteStream(`${dir}/positioned.txt`, { start: 0 });
  await new Promise((resolve) => positioned.on("open", resolve));
  positioned.pos = 2;
  positioned.end("X");
  await new Promise((resolve) => positioned.on("close", resolve));
  eq(readFileSync(`${dir}/positioned.txt`, "latin1"), "\0\0X", "assigning a WriteStream's pos");
  const repositioned = createReadStream(`${dir}/stream.txt`, { start: 0 });
  repositioned.on("open", () => { repositioned.pos = null; });
  const repositionedChunks = [];
  for await (const chunk of repositioned) repositionedChunks.push(chunk);
  eq(Buffer.concat(repositionedChunks).toString(), "hello streams", "a ReadStream whose pos is set to null reads on");
  const misplaced = createReadStream(`${dir}/stream.txt`, { start: 0 });
  misplaced.on("open", () => { misplaced.pos = "x"; });
  misplaced.resume();
  eq((await new Promise((resolve) => misplaced.on("error", resolve))).code, "ERR_OUT_OF_RANGE", "a ReadStream whose pos is set to a string errors");

  // graceful-fs replaces `open` with one that opens the file and assigns `fd`.
  function GracefulReadStream(...args) {
    fs.ReadStream.call(this, ...args);
  }
  Object.setPrototypeOf(GracefulReadStream.prototype, fs.ReadStream.prototype);
  GracefulReadStream.prototype.open = function () {
    fs.open(this.path, this.flags, this.mode, (error, fd) => {
      if (error) {
        this.destroy();
        this.emit("error", error);
      } else {
        this.fd = fd;
        this.emit("open", fd);
        this.read();
      }
    });
  };
  function GracefulWriteStream(...args) {
    fs.WriteStream.call(this, ...args);
  }
  Object.setPrototypeOf(GracefulWriteStream.prototype, fs.WriteStream.prototype);
  GracefulWriteStream.prototype.open = function () {
    fs.open(this.path, this.flags, this.mode, (error, fd) => {
      if (error) {
        this.destroy();
        this.emit("error", error);
      } else {
        this.fd = fd;
        this.emit("open", fd);
      }
    });
  };
  const gracefulOut = new GracefulWriteStream(`${dir}/graceful.txt`);
  gracefulOut.write("graceful ");
  gracefulOut.end("streams");
  await new Promise((resolve) => gracefulOut.on("close", resolve));
  eq(gracefulOut.bytesWritten, 16, "a WriteStream with a replaced open writes");
  const gracefulIn = new GracefulReadStream(`${dir}/graceful.txt`, { highWaterMark: 5 });
  const gracefulEvents = [];
  gracefulIn.on("open", (fd) => gracefulEvents.push(fd === gracefulIn.fd));
  gracefulIn.on("ready", () => gracefulEvents.push("ready"));
  const gracefulChunks = [];
  for await (const chunk of gracefulIn) gracefulChunks.push(chunk.toString());
  deepEq(gracefulChunks, ["grace", "ful s", "tream", "s"], "a ReadStream with a replaced open reads");
  deepEq(gracefulEvents, [true], "a replaced open emits open but not ready");
  const gracefulMissing = new GracefulReadStream(`${dir}/missing.txt`);
  eq((await new Promise((resolve) => gracefulMissing.on("error", resolve))).code, "ENOENT", "a replaced open's error");
  await pipeline(createReadStream(`${dir}/stream.txt`), createWriteStream(`${dir}/piped.txt`));
  eq(readFileSync(`${dir}/piped.txt`, "utf8"), "hello streams", "pipeline between file streams");
  const broken = createReadStream(`${dir}/missing`);
  eq((await new Promise((resolve) => broken.on("error", resolve))).code, "ENOENT", "ReadStream open errors");
  const appended = createWriteStream(`${dir}/stream.txt`, { flags: "a" });
  appended.end("!");
  await new Promise((resolve) => appended.on("finish", resolve));
  eq(readFileSync(`${dir}/stream.txt`, "utf8"), "hello streams!", "WriteStream flags");

  // Watching, which polls.
  const nextChange = (watcher) => new Promise((resolve) => watcher.once("change", (...args) => resolve(args)));
  const watched = `${dir}/watched`;
  mkdirSync(watched);
  const watcher = fs.watch(watched);
  eq(process.getActiveResourcesInfo().includes("FSEventWrap"), true, "watchers are active resources");
  let change = nextChange(watcher);
  writeFileSync(`${watched}/new.txt`, "x");
  deepEq(await change, ["rename", "new.txt"], "a new file is a rename");
  change = nextChange(watcher);
  writeFileSync(`${watched}/new.txt`, "xy");
  deepEq(await change, ["change", "new.txt"], "a modified file is a change");
  change = nextChange(watcher);
  writeFileSync(`${watched}/brief.txt`, "");
  fs.unlinkSync(`${watched}/brief.txt`);
  deepEq(await change, ["rename", null], "a file created and removed between polls has no filename");
  change = nextChange(watcher);
  fs.unlinkSync(`${watched}/new.txt`);
  deepEq(await change, ["rename", "new.txt"], "a removed file is a rename");
  const closed = new Promise((resolve) => watcher.on("close", resolve));
  watcher.close();
  watcher.close();
  await closed;
  eq(watcher._handle, null, "close drops the handle");

  const fileWatcher = fs.watch(`${dir}/stream.txt`, { encoding: "buffer" });
  // The watcher keeps reporting to its own callback after `_handle.onchange` is replaced.
  fileWatcher._handle.onchange = () => {};
  change = nextChange(fileWatcher);
  fs.appendFileSync(`${dir}/stream.txt`, "?");
  const [fileEvent, fileName] = await change;
  deepEq([fileEvent, Buffer.isBuffer(fileName), String(fileName)], ["change", true, "stream.txt"], "watching a file, with Buffer filenames");
  fileWatcher.close();

  const tree = fs.watch(watched, { recursive: true, ignore: ["*.log", /skip/] });
  const treeEvents = [];
  tree.on("change", (...args) => treeEvents.push(args));
  mkdirSync(`${watched}/sub`);
  writeFileSync(`${watched}/sub/inner.txt`, "");
  writeFileSync(`${watched}/sub/noise.log`, "");
  writeFileSync(`${watched}/skip.txt`, "");
  await new Promise((resolve) => setTimeout(resolve, 300));
  tree.close();
  const sep = process.platform === "win32" ? "\\" : "/";
  deepEq(treeEvents, [["rename", "sub"], ["rename", `sub${sep}inner.txt`]], "recursive watching with ignore patterns");
  throws(() => fs.watch(watched, { ignore: "" }), "ERR_INVALID_ARG_VALUE", "empty ignore patterns");
  throws(() => fs.watch(`${dir}/missing`), "ENOENT", "watching a missing file");
  eq(fs.watch(`${dir}/missing`, { throwIfNoEntry: false })._handle !== null, true, "throwIfNoEntry");

  const ac = new AbortController();
  const iterator = fsp.watch(watched, { signal: ac.signal });
  const first = iterator.next();
  writeFileSync(`${watched}/iterated.txt`, "");
  deepEq((await first).value, { eventType: "rename", filename: "iterated.txt" }, "fsPromises.watch yields changes");
  ac.abort();
  eq((await iterator.next().catch((error) => error)).name, "AbortError", "aborting ends fsPromises.watch");
  deepEq(await iterator.next(), { value: undefined, done: true }, "an ended iterator is done");
  eq((await fsp.watch(watched, { maxQueue: "x" }).next().catch((error) => error)).code, "ERR_INVALID_ARG_TYPE", "fsPromises.watch validates on next()");

  const polled = `${dir}/polled.txt`;
  const statChanges = [];
  const statChanged = new Promise((resolve) =>
    fs.watchFile(polled, { interval: 10 }, (current, previous) => {
      statChanges.push([current.size, previous.size, current.mtimeMs === 0]);
      if (statChanges.length === 2) resolve();
      else writeFileSync(polled, "abc");
    }),
  );
  await statChanged;
  deepEq(statChanges, [[0, 0, true], [3, 0, false]], "watchFile reports zeroed stats for a missing file, then its creation");
  const stopped = fs.watchFile(polled, () => {});
  let stopEvents = 0;
  stopped.on("stop", () => stopEvents++);
  fs.unwatchFile(polled);
  // Calling the handle's `onchange` with a status that isn't a number doesn't abort.
  const called = fs.watchFile(`${dir}/called.txt`, () => {});
  called._handle.onchange("x");
  fs.unwatchFile(`${dir}/called.txt`);
  eq(stopEvents, 0, "stop is emitted later");
  await new Promise((resolve) => setImmediate(resolve));
  eq(stopEvents, 1, "unwatchFile stops the watcher");

  // Argument checks that follow Node.js.
  {
    const file = `${dir}/checks`;
    writeFileSync(file, "hello");
    throws(() => openSync(file, 1.5), "ERR_OUT_OF_RANGE", "a fractional numeric flag");
    throws(() => fs.truncateSync(file, -(2 ** 60)), "ERR_OUT_OF_RANGE", "a length below MIN_SAFE_INTEGER");
    throws(() => mkdirSync(`${dir}/m`, { recursive: 1 }), "ERR_INVALID_ARG_TYPE", "a non-boolean recursive");
    throws(() => readdirSync(dir, { recursive: 1 }), "ERR_INVALID_ARG_TYPE", "a non-boolean readdir recursive");
    throws(() => rmSync(`${dir}/none`, { force: "yes" }), "ERR_INVALID_ARG_TYPE", "a non-boolean force");
    throws(() => rmSync(`${dir}/none`, { force: true, maxRetries: -1 }), "ERR_OUT_OF_RANGE", "a negative maxRetries");
    throws(() => fs.symlinkSync("a", `${dir}/link`, "bogus"), "ERR_INVALID_ARG_VALUE", "an unknown symlink type");
    throws(() => statSync(`${dir}/none`, { throwIfNoEntry: 0 }), "ENOENT", "only false keeps stat from throwing");
    eq(fs.accessSync(file, 0.5), undefined, "access truncates the mode");
    eq(throws(() => fs.accessSync(file, 8), "ERR_OUT_OF_RANGE", "a mode above 7").message, "mode is out of range: >= 0 && <= 7", "access mode message");
    throws(() => fs.accessSync(file, "1"), "ERR_INVALID_ARG_TYPE", "a string mode");
    const fd = openSync(file, "r+");
    try {
      throws(() => writeSync(fd, "abc", null, "hex"), "ERR_INVALID_ARG_VALUE", "odd hex data");
      eq(writeSync(fd, Buffer.from("ab"), 0, 2, -5), 2, "a negative position writes at the current position");
      eq(readSync(fd, Buffer.alloc(4), 0, 1.5, 0), 1, "the read length is truncated");
      eq(throws(() => readSync(fd, Buffer.alloc(4), 0, 5, 0), "ERR_OUT_OF_RANGE", "a long read").message.includes("It must be <= 4"), true, "the read length range");
      eq(throws(() => fs.futimesSync(fd, "x", 1), "ERR_INVALID_ARG_TYPE", "a bad atime").message.includes('"atime"'), true, "futimes names atime");
    } finally {
      closeSync(fd);
    }
    eq(fs.realpathSync(""), process.cwd(), "realpath of the empty path");
    const order = [];
    fs.exists(123, (exists) => order.push(`cb ${exists}`));
    order.push("after");
    deepEq(order, ["cb false", "after"], "exists calls back before returning for an invalid path");
    deepEq(Object.getOwnPropertyNames(fs.Dirent.prototype).filter((name) => name !== "constructor").slice(0, 2), ["isDirectory", "isFile"], "Dirent method order");
    eq(Object.getOwnPropertyDescriptor(fs.Stats.prototype, "atime").enumerable, true, "Stats dates are enumerable");
    if (existsSync("/dev/null")) {
      fs.copyFileSync("/dev/null", `${dir}/null-copy`);
      eq(readFileSync(`${dir}/null-copy`, "utf8"), "", "copying a device copies its reported size");
    }
    const handle = await promises.open(file, "r");
    try {
      eq(Object.getPrototypeOf(await handle.read()), null, "FileHandle read results have no prototype");
    } finally {
      await handle.close();
    }
  }
} finally {
  rmSync(dir, { recursive: true, force: true });
}
eq(existsSync(dir), false, "cleanup");
