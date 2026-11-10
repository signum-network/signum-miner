use crate::miner::Buffer;
#[cfg(feature = "opencl")]
use crate::miner::CpuBuffer;
use crate::plot::{Meta, Plot, ReadWorker};
use crate::utils::new_thread_pool;
use crossbeam_channel;
use crossbeam_channel::{Receiver, Sender};
use pbr::{ProgressBar, Units};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::io;
use std::io::Stdout;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use stopwatch::Stopwatch;

pub struct BufferInfo {
    pub len: usize,
    pub height: u64,
    pub block: u64,
    pub base_target: u64,
    pub gensig: Arc<[u8; 32]>,
    pub start_nonce: u64,
    pub finished: bool,
    pub account_id: u64,
    pub gpu_signal: u64,
}
pub struct ReadReply {
    pub buffer: Box<dyn Buffer + Send>,
    pub info: BufferInfo,
}

#[allow(dead_code)]
pub struct Reader {
    drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>>,
    pub total_size: u64,
    pool: rayon::ThreadPool,
    rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_read_replies_cpu: Sender<ReadReply>,
    tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
    interupts: Vec<Sender<()>>,
    show_progress: bool,
    show_drive_stats: bool,
    /// How long a single plot open or chunk read may take before its plot is
    /// skipped for the round.
    read_timeout: Duration,
    /// Plots whose reads hung, so later rounds skip them instead of paying
    /// another timeout on them every round.
    bad_plots: Arc<Mutex<HashSet<String>>>,
}

impl Reader {
    pub fn new(
        drive_id_to_plots: HashMap<String, Arc<Vec<Mutex<Plot>>>>,
        total_size: u64,
        num_threads: usize,
        rx_empty_buffers: Receiver<Box<dyn Buffer + Send>>,
        tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
        tx_read_replies_cpu: Sender<ReadReply>,
        tx_read_replies_gpu: Option<Vec<Sender<ReadReply>>>,
        show_progress: bool,
        show_drive_stats: bool,
        thread_pinning: bool,
        benchmark: bool,
        read_timeout: Duration,
        bad_plots: Arc<Mutex<HashSet<String>>>,
    ) -> Reader {
        if !benchmark {
            check_overlap(&drive_id_to_plots);
        }

        Reader {
            drive_id_to_plots,
            total_size,
            pool: new_thread_pool(num_threads, thread_pinning),
            rx_empty_buffers,
            tx_empty_buffers,
            tx_read_replies_cpu,
            tx_read_replies_gpu,
            interupts: Vec::new(),
            show_progress,
            show_drive_stats,
            read_timeout,
            bad_plots,
        }
    }

    pub fn start_reading(
        &mut self,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: &Arc<[u8; 32]>,
    ) {
        for interupt in &self.interupts {
            interupt.send(()).ok();
        }
        let mut pb = ProgressBar::new(self.total_size);
        pb.format("│██░│");
        pb.set_width(Some(80));
        pb.set_units(Units::Bytes);
        pb.message("Searching your hashes: ");
        let pb = Arc::new(Mutex::new(pb));

        // send start signals (dummy buffer) to gpu threads
        #[cfg(feature = "opencl")]
        for i in 0..self.tx_read_replies_gpu.as_ref().unwrap().len() {
            self.tx_read_replies_gpu.as_ref().unwrap()[i]
                .send(ReadReply {
                    buffer: Box::new(CpuBuffer::new(0)) as Box<dyn Buffer + Send>,
                    info: BufferInfo {
                        len: 1,
                        height,
                        block,
                        base_target,
                        gensig: gensig.clone(),
                        start_nonce: 0,
                        finished: false,
                        account_id: 0,
                        gpu_signal: 1,
                    },
                })
                .expect("Error sending 'round start' signal to GPU");
        }

        self.interupts = self
            .drive_id_to_plots
            .iter()
            .map(|(drive, plots)| {
                let (interupt, task) = if self.show_progress {
                    self.create_read_task(
                        Some(pb.clone()),
                        drive.clone(),
                        plots.clone(),
                        height,
                        block,
                        base_target,
                        scoop,
                        gensig.clone(),
                        self.show_drive_stats,
                    )
                } else {
                    self.create_read_task(
                        None,
                        drive.clone(),
                        plots.clone(),
                        height,
                        block,
                        base_target,
                        scoop,
                        gensig.clone(),
                        self.show_drive_stats,
                    )
                };

                self.pool.spawn(task);
                interupt
            })
            .collect();
    }

    pub fn wakeup(&mut self) {
        for plots in self.drive_id_to_plots.values() {
            let plots = plots.clone();
            self.pool.spawn(move || {
                let mut p = plots[0].lock().unwrap();

                if let Err(e) = p.seek_random() {
                    error!(
                        "wakeup: error during wakeup {}: {} -> skip one round",
                        p.meta.name, e
                    );
                }
            });
        }
    }

    fn create_read_task(
        &self,
        pb: Option<Arc<Mutex<pbr::ProgressBar<Stdout>>>>,
        drive: String,
        plots: Arc<Vec<Mutex<Plot>>>,
        height: u64,
        block: u64,
        base_target: u64,
        scoop: u32,
        gensig: Arc<[u8; 32]>,
        show_drive_stats: bool,
    ) -> (Sender<()>, impl FnOnce()) {
        let (tx_interupt, rx_interupt) = crossbeam_channel::unbounded();
        let rx_empty_buffers = self.rx_empty_buffers.clone();
        let tx_empty_buffers = self.tx_empty_buffers.clone();
        let tx_read_replies_cpu = self.tx_read_replies_cpu.clone();
        #[cfg(feature = "opencl")]
        let tx_read_replies_gpu = self.tx_read_replies_gpu.clone();
        let read_timeout = self.read_timeout;
        let bad_plots = self.bad_plots.clone();

        (tx_interupt, move || {
            let mut sw = Stopwatch::new();
            let mut elapsed = 0i64;
            let mut nonces_processed = 0u64;
            let plot_count = plots.len();
            // One read worker per drive: the reads stay on one reused thread
            // instead of a thread per chunk, and a read that hangs takes only
            // its own worker with it.
            let mut worker = ReadWorker::new();
            let mut finished_sent = false;
            let mut interrupted = false;

            'outer: for (i_p, p) in plots.iter().enumerate() {
                let mut p = p.lock().unwrap();
                if bad_plots.lock().unwrap().contains(&p.meta.name) {
                    // This plot hung once already. Retrying it would cost
                    // another timeout on every round, and the round is what a
                    // share is worth: leave it out and keep the drive.
                    continue 'outer;
                }
                if let Err(e) = p.prepare(&mut worker, scoop, read_timeout) {
                    if e.kind() == io::ErrorKind::TimedOut {
                        error!(
                            "reader {}: opening {} did not return within {}s -> skipping \
                             this plot",
                            drive,
                            p.meta.name,
                            read_timeout.as_secs()
                        );
                        bad_plots.lock().unwrap().insert(p.meta.name.clone());
                    } else {
                        error!(
                            "reader: error preparing {} for reading: {} -> skip one round",
                            p.meta.name, e
                        );
                    }
                    continue 'outer;
                }

                'inner: for mut buffer in rx_empty_buffers.clone() {
                    if show_drive_stats {
                        sw.restart();
                    }
                    let mut_bs = buffer.get_buffer_for_writing();
                    let (bytes_read, start_nonce, next_plot) =
                        match p.read(&mut worker, mut_bs.clone(), scoop, read_timeout) {
                            Ok(x) => x,
                            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                                error!(
                                    "reader {}: read on {} did not return within {}s -> skipping \
                                     this plot for the round",
                                    drive,
                                    p.meta.name,
                                    read_timeout.as_secs()
                                );
                                // The read is still in the driver and is writing
                                // into this buffer's memory. Releasing it now would
                                // hand that memory back - for a GPU buffer that means
                                // releasing pinned OpenCL memory - while the write is
                                // still in flight. Leak it instead: it stays
                                // allocated until the process ends, one buffer per
                                // hung plot, and the bad-plot list keeps that from
                                // growing round after round. Leave the plot out and
                                // put a fresh buffer of the same kind back into the
                                // pool, so a hung plot does not take a buffer slot
                                // with it.
                                bad_plots.lock().unwrap().insert(p.meta.name.clone());
                                let replacement = buffer.replacement();
                                std::mem::forget(mut_bs);
                                std::mem::forget(buffer);
                                let was_interrupted = rx_interupt.try_recv().is_ok();
                                tx_empty_buffers
                                    .send(replacement)
                                    .expect("failed to return a buffer to the pool");
                                if was_interrupted {
                                    interrupted = true;
                                    break 'outer;
                                }
                                continue 'outer;
                            }
                            Err(e) => {
                                error!(
                                    "reader: error reading chunk from {}: {} -> skip one round",
                                    p.meta.name, e
                                );
                                buffer.unmap();
                                (0, 0, true)
                            }
                        };

                    if rx_interupt.try_recv().is_ok() {
                        buffer.unmap();
                        tx_empty_buffers.send(buffer).unwrap();
                        interrupted = true;
                        break 'outer;
                    }

                    let finished = i_p == (plot_count - 1) && next_plot;
                    finished_sent |= finished;
                    // buffer routing
                    #[cfg(feature = "opencl")]
                    match buffer.get_id() {
                        0 => {
                            tx_read_replies_cpu
                                .send(ReadReply {
                                    buffer,
                                    info: BufferInfo {
                                        len: bytes_read,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce,
                                        finished,
                                        account_id: p.meta.account_id,
                                        gpu_signal: 0,
                                    },
                                })
                                .expect("failed to send read data to CPU thread");
                        }
                        i => {
                            tx_read_replies_gpu.as_ref().unwrap()[i - 1]
                                .send(ReadReply {
                                    buffer,
                                    info: BufferInfo {
                                        len: bytes_read,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce,
                                        finished,
                                        account_id: p.meta.account_id,
                                        gpu_signal: 0,
                                    },
                                })
                                .expect("failed to send read data to GPU thread A");
                        }
                    }
                    #[cfg(not(feature = "opencl"))]
                    tx_read_replies_cpu
                        .send(ReadReply {
                            buffer,
                            info: BufferInfo {
                                len: bytes_read,
                                height,
                                block,
                                base_target,
                                gensig: gensig.clone(),
                                start_nonce,
                                finished,
                                account_id: p.meta.account_id,
                                gpu_signal: 0,
                            },
                        })
                        .unwrap();

                    nonces_processed += bytes_read as u64 / 64;

                    match &pb {
                        Some(pb) => {
                            let mut pb = pb.lock().unwrap();
                            pb.add(bytes_read as u64);
                        }
                        None => (),
                    }

                    if show_drive_stats {
                        elapsed += sw.elapsed_ms();
                    }

                    // send termination signal (dummy buffer) to gpu
                    if finished {
                        #[cfg(feature = "opencl")]
                        for i in 0..tx_read_replies_gpu.as_ref().unwrap().len() {
                            tx_read_replies_gpu.as_ref().unwrap()[i]
                                .send(ReadReply {
                                    buffer: Box::new(CpuBuffer::new(0)) as Box<dyn Buffer + Send>,
                                    info: BufferInfo {
                                        len: 1,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce: 0,
                                        finished: false,
                                        account_id: 0,
                                        gpu_signal: 2,
                                    },
                                })
                                .expect("Error sending 'drive finished' signal to GPU thread A");
                        }
                    }

                    if finished && show_drive_stats {
                        info!(
                            "{: <80}",
                            format!(
                                "drive {} finished, speed={} MiB/s",
                                drive,
                                nonces_processed * 1000 / (elapsed + 1) as u64 * 64 / 1024 / 1024,
                            )
                        );
                    }

                    if next_plot {
                        break 'inner;
                    }
                }
            }

            // A drive that runs out of plots without a finished reply would
            // leave the round waiting forever, and with `submit_only_best` the
            // best share is only sent when the round closes. That is what
            // happens when the last plot hung, or when the plots at the end
            // were skipped as bad. Report the drive as finished here instead.
            if !finished_sent && !interrupted && rx_interupt.try_recv().is_err() {
                // The drive can only be reported finished with a buffer in
                // hand, and every buffer is either here or on its way back
                // from a worker, so this waits for one instead of giving up.
                match rx_empty_buffers.recv() {
                    Ok(buffer) => {
                        // A new round may have started while this drive was
                        // waiting for that buffer: report it as interrupted
                        // instead of closing the new round with an old signal.
                        if rx_interupt.try_recv().is_ok() {
                            let _ = tx_empty_buffers.send(buffer);
                        } else {
                            // send termination signal (dummy buffer) to gpu
                            #[cfg(feature = "opencl")]
                            for i in 0..tx_read_replies_gpu.as_ref().unwrap().len() {
                                tx_read_replies_gpu.as_ref().unwrap()[i]
                                    .send(ReadReply {
                                        buffer: Box::new(CpuBuffer::new(0))
                                            as Box<dyn Buffer + Send>,
                                        info: BufferInfo {
                                            len: 1,
                                            height,
                                            block,
                                            base_target,
                                            gensig: gensig.clone(),
                                            start_nonce: 0,
                                            finished: false,
                                            account_id: 0,
                                            gpu_signal: 2,
                                        },
                                    })
                                    .expect("Error sending 'drive finished' signal to GPU thread");
                            }
                            tx_read_replies_cpu
                                .send(ReadReply {
                                    buffer,
                                    info: BufferInfo {
                                        len: 0,
                                        height,
                                        block,
                                        base_target,
                                        gensig: gensig.clone(),
                                        start_nonce: 0,
                                        finished: true,
                                        account_id: 0,
                                        gpu_signal: 0,
                                    },
                                })
                                .expect("failed to send read data to CPU thread");
                        }
                    }
                    Err(e) => warn!(
                        "reader {}: no buffer to close the round with: {}",
                        drive, e
                    ),
                }
            }
        })
    }
}

// Don't waste your time striving for perfection; instead, strive for excellence - doing your best.
// let my_best = perfection;
pub fn check_overlap(drive_id_to_plots: &HashMap<String, Arc<Vec<Mutex<Plot>>>>) -> bool {
    let plots: Vec<Meta> = drive_id_to_plots
        .values()
        .map(|a| a.iter())
        .flatten()
        .map(|plot| plot.lock().unwrap().meta.clone())
        .collect();
    plots
        .par_iter()
        .enumerate()
        .filter(|(i, plot_a)| {
            plots[i + 1..]
                .par_iter()
                .filter(|plot_b| {
                    plot_a.account_id == plot_b.account_id && plot_b.overlaps_with(&plot_a)
                })
                .count()
                > 0
        })
        .count()
        > 0
}
