use crate::utils::get_sector_size;
use rand::prelude::*;
use std::cmp::{max, min};
use std::error::Error;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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

        // O_DIRECT hint, according to fcntl.h
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        const O_DIRECT: i32 = 0o0_040_000;
        // For ARM a different value is set, O_DIRECT 0200000
        #[cfg(any(target_arch = "arm", target_arch = "aarch64"))]
        const O_DIRECT: i32 = 0o0_200_000;

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

    pub fn prepare(&mut self, scoop_array: &Vec<u32>) -> io::Result<u64> {
        self.read_offset = 0;
        let nonces = self.meta.nonces;
        let mut seek_addr = u64::from(scoop_array[0]) * nonces as u64 * SCOOP_SIZE;

        // reopening file handles
        if !self.use_direct_io {
            self.fh = open(&self.path)?;
        } else {
            self.fh = open_using_direct_io(&self.path)?;
        };

        if self.use_direct_io {
            self.read_offset = self.round_seek_addr(&mut seek_addr);
        }

        self.fh.seek(SeekFrom::Start(seek_addr))
    }

    pub fn read(&mut self, bs: &mut Vec<u8>, scoop_array: &Vec<u32>) -> Result<(usize, u64, bool), io::Error> {
        let read_offset = self.read_offset;
        let buffer_cap = bs.capacity();
        let start_nonce = self.meta.start_nonce + self.read_offset / SCOOP_SIZE;

        let (bytes_to_read, finished) =
        if read_offset + buffer_cap as u64 >= (SCOOP_SIZE * self.meta.nonces) {
            let mut bytes_to_read =
                (SCOOP_SIZE * self.meta.nonces - self.read_offset) as usize;
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

        // if we have more than one scoop, we will read some nonces and then skip
        let nonces_to_switch_scoop = SCOOPS_IN_NONCE as usize / scoop_array.len();
        let mut bytes_to_switch_scoop = NONCE_SIZE as usize / scoop_array.len();

        let scoop_alignmet_offset = SCOOPS_IN_NONCE as usize
            - ((start_nonce + scoop_array[0] as u64) % SCOOPS_IN_NONCE) as usize;

        let offset = self.read_offset;
        let nonces = self.meta.nonces;
    
        for scoop_number_position in 0..scoop_array.len() {
            let scoop = scoop_array[scoop_number_position];

            let mut start_offset_nonces = (scoop_alignmet_offset + nonces_to_switch_scoop * scoop_number_position)
                % SCOOPS_IN_NONCE as usize;
            if scoop_array.len() == 1 {
              // single scoop, so we can speed things up
              start_offset_nonces = 0;
              bytes_to_switch_scoop = bytes_to_read;
            }
            else if start_offset_nonces > SCOOPS_IN_NONCE as usize - nonces_to_switch_scoop {
                // this scoop number has a section in the beginning of the file, so we read it first
                let nonces_to_read_on_start = start_offset_nonces - (SCOOPS_IN_NONCE as usize - nonces_to_switch_scoop);
                let bytes_to_read_on_start = nonces_to_read_on_start * SCOOP_SIZE as usize;
          
                let seek_addr_start =
                    SeekFrom::Start(self.read_offset + scoop as u64 * nonces * SCOOP_SIZE);
                if !self.dummy {
                    self.fh.seek(seek_addr_start)?;
                    self.fh.read_exact(&mut bs[0..bytes_to_read_on_start])?;
                }
            }


            let mut start_offset_bytes = start_offset_nonces * SCOOP_SIZE as usize;
            let mut file_position_bytes = offset + start_offset_bytes as u64
                + scoop as u64 * nonces * SCOOP_SIZE;
        
            while start_offset_bytes < bytes_to_read {
                let seek_addr = SeekFrom::Start(file_position_bytes);

                if !self.dummy {
                    self.fh.seek(seek_addr)?;
                    let bytes_to_read_now = min(bytes_to_switch_scoop, buffer_cap - start_offset_bytes);
                    self.fh.read_exact(&mut bs[start_offset_bytes..start_offset_bytes + bytes_to_read_now])?;
                    // interrupt avoider (not implemented)
                    // let read_chunk_size_in_nonces = 65536;
                    // for i in (0..bytes_to_read).step_by(read_chunk_size_in_nonces) {
                    //     self.fh.read_exact(
                    //         &mut bs[i..(i + min(read_chunk_size_in_nonces, bytes_to_read - i))],
                    //     )?;
                    // }
                }
                if scoop_array.len() == 1 {
                    // we are done already
                    break;
                }
                start_offset_bytes += NONCE_SIZE as usize;
                file_position_bytes += NONCE_SIZE;
            }
        }
        self.read_offset += bytes_to_read as u64;

        Ok((bytes_to_read, start_nonce, finished))
    }

    pub fn seek_random(&mut self) -> io::Result<u64> {
        let mut rng = thread_rng();
        let rand_scoop = rng.gen_range(0, SCOOPS_IN_NONCE);

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
