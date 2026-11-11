use crate::utils::get_sector_size;
use crossbeam_channel::{RecvTimeoutError, Sender};
use rand::prelude::*;
use std::cmp::{max, min};
use std::error::Error;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const SCOOPS_IN_NONCE: u64 = 4096;
const SHABAL256_HASH_SIZE: u64 = 32;
pub const SCOOP_SIZE: u64 = SHABAL256_HASH_SIZE * 2;
const NONCE_SIZE: u64 = SCOOP_SIZE * SCOOPS_IN_NONCE;

#[derive(Clone)]
pub struct Meta {
    pub account_id: u64,
    pub start_nonce: u64,
    pub nonces: u64,
    pub name: String,
}

impl Meta {
    pub fn overlaps_with(&self, other: &Meta) -> bool {
        if self.start_nonce < other.start_nonce + other.nonces
            && other.start_nonce < self.start_nonce + self.nonces
        {
            let overlap = min(
                other.start_nonce + other.nonces,
                self.start_nonce + self.nonces,
            ) - max(self.start_nonce, other.start_nonce);
            warn!(
                "overlap: {} and {} share {} nonces!",
                self.name, other.name, overlap
            );
            true
        } else {
            false
        }
    }
}

pub struct Plot {
    pub meta: Meta,
    pub path: String,
    pub fh: File,
    read_offset: u64,
    use_direct_io: bool,
    sector_size: u64,
    dummy: bool,
}

cfg_if! {
    if #[cfg(unix)] {
        use std::os::unix::fs::OpenOptionsExt;

        const O_DIRECT: i32 = 0o0_040_000;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(O_DIRECT)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .open(path)
        }

    } else {
        use std::os::windows::fs::OpenOptionsExt;

        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        const FILE_FLAG_SEQUENTIAL_SCAN: u32 = 0x0800_0000;
        const FILE_FLAG_RANDOM_ACCESS: u32 = 0x1000_0000;

        pub fn open_using_direct_io<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_NO_BUFFERING)
                .open(path)
        }

        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new()
                .read(true)
                .custom_flags(FILE_FLAG_SEQUENTIAL_SCAN | FILE_FLAG_RANDOM_ACCESS)
                .open(path)
        }
    }
}

impl Plot {
    pub fn new(path: &PathBuf, mut use_direct_io: bool, dummy: bool) -> Result<Plot, Box<dyn Error>> {
        if !path.is_file() {
            return Err(From::from(format!(
                "{} is not a file",
                path.to_str().unwrap()
            )));
        }

        let plot_file = path.file_name().unwrap().to_str().unwrap();
        let parts: Vec<&str> = plot_file.split('_').collect();
        if parts.len() != 3 {
            return Err(From::from("plot file has wrong format"));
        }

        let account_id = parts[0].parse::<u64>()?;
        let start_nonce = parts[1].parse::<u64>()?;
        let nonces = parts[2].parse::<u64>()?;

        let size = fs::metadata(path)?.len();
        let exp_size = nonces * NONCE_SIZE;
        if size != exp_size as u64 {
            return Err(From::from(format!(
                "expected plot size {} but got {}",
                exp_size, size
            )));
        }

        let fh = if use_direct_io {
            open_using_direct_io(path)?
        } else {
            open(path)?
        };

        let plot_file_name = plot_file.to_string();
        let sector_size = get_sector_size(&path.to_str().unwrap().to_owned());
        if use_direct_io && sector_size / 64 > nonces {
            warn!(
                "not enough nonces for using direct io: plot={}",
                plot_file_name
            );
            use_direct_io = false;
        }

        let file_path = path.clone().into_os_string().into_string().unwrap();
        Ok(Plot {
            meta: Meta {
                account_id,
                start_nonce,
                nonces,
                name: plot_file_name,
            },
            fh,
            path: file_path,
            read_offset: 0,
            use_direct_io,
            sector_size,
            dummy,
        })
    }

    /// Opens the plot and positions it at the scoop, giving up after `timeout`.
    /// Reopening a file touches the same drive as reading it, so it gets the
    /// same timeout: a drive that hangs on open would stall the round just as
    /// a hung read does.
    pub fn prepare(
        &mut self,
        worker: &mut ReadWorker,
        scoop: u32,
        timeout: Duration,
    ) -> io::Result<()> {
        self.read_offset = 0;
        let nonces = self.meta.nonces;
        let mut seek_addr = u64::from(scoop) * nonces as u64 * SCOOP_SIZE;

        if self.use_direct_io {
            self.read_offset = self.round_seek_addr(&mut seek_addr);
        }

        // reopening file handles
        self.fh = worker.prepare(&self.path, self.use_direct_io, seek_addr, timeout)?;
        Ok(())
    }

    /// Reads the next chunk, giving up after `timeout` if the drive does not
    /// answer. A read that hangs - a bad sector on a dying drive - used to block
    /// the drive task for good, and the round never closed. The read runs on
    /// the worker thread so the caller can walk away from it; the worker keeps
    /// the buffer it was filling, so the caller must replace that buffer.
    pub fn read(
        &mut self,
        worker: &mut ReadWorker,
        bs: Arc<Mutex<Vec<u8>>>,
        scoop: u32,
        timeout: Duration,
    ) -> Result<(usize, u64, bool), io::Error> {
        let read_offset = self.read_offset;
        let buffer_cap = bs.lock().unwrap().capacity();
        let start_nonce = self.meta.start_nonce + self.read_offset / 64;

        let (bytes_to_read, finished) =
            if read_offset as usize + buffer_cap >= (SCOOP_SIZE * self.meta.nonces) as usize {
                let mut bytes_to_read =
                    (SCOOP_SIZE * self.meta.nonces) as usize - self.read_offset as usize;
                if self.use_direct_io {
                    let r = bytes_to_read % self.sector_size as usize;
                    if r != 0 {
                        bytes_to_read -= r;
                    }
                }

                (bytes_to_read, true)
            } else {
                (buffer_cap as usize, false)
            };

        if !self.dummy {
            // A clone of the handle, not the handle itself: a timed out read
            // keeps its thread, and the plot must stay usable by the next round.
            let fh = self.fh.try_clone()?;
            let seek_addr =
                read_offset as u64 + u64::from(scoop) * self.meta.nonces as u64 * SCOOP_SIZE;
            worker.read(fh, seek_addr, bs, bytes_to_read, timeout)?;
            // interrupt avoider (not implemented)
            // let read_chunk_size_in_nonces = 65536;
            // for i in (0..bytes_to_read).step_by(read_chunk_size_in_nonces) {
            //     self.fh.read_exact(
            //         &mut bs[i..(i + min(read_chunk_size_in_nonces, bytes_to_read - i))],
            //     )?;
            // }
        }
        self.read_offset += bytes_to_read as u64;

        Ok((bytes_to_read, start_nonce, finished))
    }

    pub fn seek_random(&mut self) -> io::Result<u64> {
        let mut rng = rand::rng();
        let rand_scoop = rng.gen_range(0..SCOOPS_IN_NONCE);

        let mut seek_addr = rand_scoop as u64 * self.meta.nonces as u64 * SCOOP_SIZE;
        if self.use_direct_io {
            self.round_seek_addr(&mut seek_addr);
        }

        self.fh.seek(SeekFrom::Start(seek_addr))
    }

    fn round_seek_addr(&mut self, seek_addr: &mut u64) -> u64 {
        let r = *seek_addr % self.sector_size;
        if r != 0 {
            let offset = self.sector_size - r;
            *seek_addr += offset;
            offset
        } else {
            0
        }
    }
}

enum Job {
    Prepare {
        path: String,
        direct_io: bool,
        seek_addr: u64,
        done: Sender<io::Result<File>>,
    },
    Read {
        fh: File,
        seek_addr: u64,
        buffer: Arc<Mutex<Vec<u8>>>,
        len: usize,
        done: Sender<io::Result<()>>,
    },
}

/// Runs the blocking opens and reads of one drive task on a single thread that
/// is reused, so a caller can give up on a call that does not return while a
/// healthy farm still does not pay for a thread per chunk read.
///
/// A call that timed out leaves its thread in the driver for good. That thread
/// and whatever it was reading are abandoned, and the next call starts a fresh
/// worker: a worker that is stuck cannot be reused.
pub struct ReadWorker {
    tx: Option<Sender<Job>>,
}

impl ReadWorker {
    pub fn new() -> ReadWorker {
        ReadWorker { tx: None }
    }

    /// Opens `path` and seeks to `seek_addr`, giving up after `timeout`.
    pub fn prepare(
        &mut self,
        path: &str,
        direct_io: bool,
        seek_addr: u64,
        timeout: Duration,
    ) -> io::Result<File> {
        let (done, result) = crossbeam_channel::bounded(1);
        self.dispatch(Job::Prepare {
            path: path.to_owned(),
            direct_io,
            seek_addr,
            done,
        });
        self.wait(result, timeout)
    }

    /// Reads `len` bytes of `buffer` from `seek_addr`, giving up after `timeout`.
    pub fn read(
        &mut self,
        fh: File,
        seek_addr: u64,
        buffer: Arc<Mutex<Vec<u8>>>,
        len: usize,
        timeout: Duration,
    ) -> io::Result<()> {
        let (done, result) = crossbeam_channel::bounded(1);
        self.dispatch(Job::Read {
            fh,
            seek_addr,
            buffer,
            len,
            done,
        });
        self.wait(result, timeout)
    }

    fn dispatch(&mut self, mut job: Job) {
        loop {
            if self.tx.is_none() {
                self.tx = Some(spawn());
            }

            match self.tx.as_ref().unwrap().send(job) {
                Ok(()) => return,
                // The worker is gone; start a new one and hand the job to it.
                Err(e) => {
                    job = e.into_inner();
                    self.tx = None;
                }
            }
        }
    }

    fn wait<T>(
        &mut self,
        result: crossbeam_channel::Receiver<io::Result<T>>,
        timeout: Duration,
    ) -> io::Result<T> {
        match result.recv_timeout(timeout) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                // The worker is stuck in the driver and keeps the memory it was
                // working on. Drop it and let the next call start a new one.
                self.tx = None;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("the drive did not answer within {}ms", timeout.as_millis()),
                ))
            }
            Err(RecvTimeoutError::Disconnected) => Err(io::Error::new(
                io::ErrorKind::Other,
                "plot read worker stopped",
            )),
        }
    }
}

fn spawn() -> Sender<Job> {
    let (tx, rx) = crossbeam_channel::unbounded();

    thread::Builder::new()
        .name("plot-read".to_owned())
        .spawn(move || {
            for job in rx {
                match job {
                    Job::Prepare {
                        path,
                        direct_io,
                        seek_addr,
                        done,
                    } => {
                        let opened = if direct_io {
                            open_using_direct_io(&path)
                        } else {
                            open(&path)
                        };
                        let result = opened.and_then(|mut fh| {
                            fh.seek(SeekFrom::Start(seek_addr))?;
                            Ok(fh)
                        });
                        let _ = done.send(result);
                    }
                    Job::Read {
                        mut fh,
                        seek_addr,
                        buffer,
                        len,
                        done,
                    } => {
                        let mut bs = buffer.lock().unwrap();
                        let result = fh
                            .seek(SeekFrom::Start(seek_addr))
                            .and_then(|_| fh.read_exact(&mut bs[0..len]));
                        drop(bs);
                        let _ = done.send(result);
                    }
                }
            }
        })
        .expect("failed to spawn the plot read thread");

    tx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_plot(name: &str, data: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("signum-miner-{}-{}", std::process::id(), name));
        fs::write(&path, data).unwrap();
        path
    }

    #[test]
    fn read_worker_reads_through_one_thread_and_replaces_a_stuck_one() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let path = temp_plot("read-worker", &data);
        let path = path.to_str().unwrap();

        let mut worker = ReadWorker::new();
        let fh = worker
            .prepare(path, false, 0, Duration::from_secs(5))
            .unwrap();
        let buffer = Arc::new(Mutex::new(vec![0u8; 1024]));
        worker
            .read(fh, 512, buffer.clone(), 1024, Duration::from_secs(5))
            .unwrap();
        assert_eq!(&buffer.lock().unwrap()[..], &data[512..1536]);

        // Hold the buffer a read would fill, so the worker blocks on it and the
        // caller gives up on the call - the way a hung drive behaves.
        let stuck = Arc::new(Mutex::new(vec![0u8; 1024]));
        let guard = stuck.lock().unwrap();
        let fh = File::open(path).unwrap();
        let err = worker
            .read(fh, 0, stuck.clone(), 1024, Duration::from_millis(200))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        drop(guard);

        // The worker that timed out is gone; the next read starts a fresh one.
        let fh = File::open(path).unwrap();
        let buffer = Arc::new(Mutex::new(vec![0u8; 64]));
        worker
            .read(fh, 0, buffer.clone(), 64, Duration::from_secs(5))
            .unwrap();
        assert_eq!(&buffer.lock().unwrap()[..], &data[..64]);

        fs::remove_file(path).ok();
    }

    #[test]
    fn a_plot_reports_finished_only_with_its_last_chunk() {
        // A round reads one scoop across all nonces: `nonces` chunks of
        // SCOOP_SIZE, contiguous from the scoop's own offset.
        let nonces = 1000u64;
        let scoop_len = (SCOOP_SIZE * nonces) as usize;
        let data: Vec<u8> = (0..scoop_len).map(|i| (i % 241) as u8).collect();
        let path = temp_plot("123_0_1000", &data);
        let mut plot = Plot {
            meta: Meta {
                account_id: 123,
                start_nonce: 0,
                nonces,
                name: "123_0_1000".to_owned(),
            },
            path: path.to_str().unwrap().to_owned(),
            fh: File::open(&path).unwrap(),
            read_offset: 0,
            use_direct_io: false,
            sector_size: 512,
            dummy: false,
        };
        let mut worker = ReadWorker::new();
        let timeout = Duration::from_secs(5);
        plot.prepare(&mut worker, 0, timeout).unwrap();

        let mut read = 0;
        let mut chunks = 0;
        loop {
            let buffer = Arc::new(Mutex::new(vec![0u8; 25_000]));
            let (bytes, start_nonce, finished) = plot
                .read(&mut worker, buffer.clone(), 0, timeout)
                .unwrap();
            assert_eq!(&buffer.lock().unwrap()[..bytes], &data[read..read + bytes]);
            assert_eq!(start_nonce, read as u64 / SCOOP_SIZE);
            assert!(!finished || read + bytes == scoop_len);
            read += bytes;
            chunks += 1;
            if finished {
                break;
            }
        }
        assert_eq!(read, scoop_len);
        assert_eq!(chunks, 3);

        fs::remove_file(&path).ok();
    }
}
