use crate::miner::{Buffer, NonceData};
use crate::poc_hashing::find_best_deadline_rust;
use crate::reader::ReadReply;
use crossbeam_channel::{Receiver, Sender};
use std::u64;
use tokio::sync::mpsc::Sender as TokioSender;

#[cfg(any(feature = "simd", feature = "neon"))]
use libc::c_void;

#[cfg(feature = "simd_avx512f")]
extern "C" {
    pub fn find_best_deadline_avx512f(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(feature = "simd_avx2")]
extern "C" {
    pub fn find_best_deadline_avx2(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(feature = "simd_avx")]
extern "C" {
    pub fn find_best_deadline_avx(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(feature = "simd_sse2")]
extern "C" {
    pub fn find_best_deadline_sse2(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

#[cfg(feature = "neon")]
extern "C" {
    pub fn find_best_deadline_neon(
        scoops: *mut c_void,
        nonce_count: u64,
        gensig: *const c_void,
        best_deadline: *mut u64,
        best_offset: *mut u64,
    );
}

pub fn create_cpu_worker_task(
    benchmark: bool,
    thread_pool: rayon::ThreadPool,
    rx_read_replies: Receiver<ReadReply>,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_nonce_data: TokioSender<NonceData>,
) -> impl FnOnce() + Send + 'static {
    move || {
        for read_reply in rx_read_replies {
            let task = hash(
                read_reply,
                tx_empty_buffers.clone(),
                tx_nonce_data.clone(),
                benchmark,
            );

            thread_pool.spawn(task);
        }
    }
}

pub fn hash(
    read_reply: ReadReply,
    tx_empty_buffers: Sender<Box<dyn Buffer + Send>>,
    tx_nonce_data: TokioSender<NonceData>,
    benchmark: bool,
) -> impl FnOnce() + Send + 'static {
    move || {
        let mut buffer = read_reply.buffer;

        if read_reply.info.len == 0 || benchmark {
            if read_reply.info.finished {
                let deadline = u64::MAX;
                let _ = tx_nonce_data.blocking_send(NonceData {
                    height: read_reply.info.height,
                    block: read_reply.info.block,
                    base_target: read_reply.info.base_target,
                    deadline,
                    nonce: 0,
                    reader_task_processed: read_reply.info.finished,
                    account_id: read_reply.info.account_id,
                });
            }
            let _ = tx_empty_buffers.send(buffer);
            return;
        }

        if read_reply.info.len == 1 && read_reply.info.gpu_signal > 0 {
            return;
        }

        #[allow(unused_assignments)]
        let mut deadline: u64 = u64::MAX;
        #[allow(unused_assignments)]
        let mut offset: u64 = 0;

        let bs = buffer.get_buffer_for_writing();
        let bs = bs.lock().unwrap();

        #[cfg(feature = "simd_avx512f")]
        unsafe {
            find_best_deadline_avx512f(
                bs.as_ptr() as *mut c_void,
                (read_reply.info.len as u64) / 64,
                read_reply.info.gensig.as_ptr() as *const c_void,
                &mut deadline,
                &mut offset,
            );
        }

        #[cfg(feature = "simd_avx2")]
        unsafe {
            find_best_deadline_avx2(
                bs.as_ptr() as *mut c_void,
                (read_reply.info.len as u64) / 64,
                read_reply.info.gensig.as_ptr() as *const c_void,
                &mut deadline,
                &mut offset,
            );
        }

        #[cfg(feature = "simd_avx")]
        unsafe {
            find_best_deadline_avx(
                bs.as_ptr() as *mut c_void,
                (read_reply.info.len as u64) / 64,
                read_reply.info.gensig.as_ptr() as *const c_void,
                &mut deadline,
                &mut offset,
            );
        }

        #[cfg(feature = "simd_sse2")]
        unsafe {
            find_best_deadline_sse2(
                bs.as_ptr() as *mut c_void,
                (read_reply.info.len as u64) / 64,
                read_reply.info.gensig.as_ptr() as *const c_void,
                &mut deadline,
                &mut offset,
            );
        }

        #[cfg(feature = "neon")]
        unsafe {
            #[cfg(target_arch = "arm")]
            let neon = is_arm_feature_detected!("neon");
            #[cfg(target_arch = "aarch64")]
            let neon = true;
            if neon {
                find_best_deadline_neon(
                    bs.as_ptr() as *mut c_void,
                    (read_reply.info.len as u64) / 64,
                    read_reply.info.gensig.as_ptr() as *const c_void,
                    &mut deadline,
                    &mut offset,
                );
            } else {
                let result = find_best_deadline_rust(
                    &bs,
                    (read_reply.info.len as u64) / 64,
                    &*read_reply.info.gensig,
                );
                deadline = result.0;
                offset = result.1;
            }
        }

        #[cfg(not(any(
            feature = "simd_avx512f",
            feature = "simd_avx2",
            feature = "simd_avx",
            feature = "simd_sse2",
            feature = "neon"
        )))]
        {
            let result = find_best_deadline_rust(
                &bs,
                (read_reply.info.len as u64) / 64,
                &*read_reply.info.gensig,
            );
            deadline = result.0;
            offset = result.1;
        }

        let _ = tx_nonce_data.blocking_send(NonceData {
            height: read_reply.info.height,
            block: read_reply.info.block,
            base_target: read_reply.info.base_target,
            deadline,
            nonce: offset + read_reply.info.start_nonce,
            reader_task_processed: read_reply.info.finished,
            account_id: read_reply.info.account_id,
        });

        let _ = tx_empty_buffers.send(buffer);
    }
}


#[cfg(test)]
mod tests {
    use crate::poc_hashing::find_best_deadline_rust;
    use hex;
    use std::u64;

    #[test]
    fn test_deadline_hashing() {
        let gensig =
            hex::decode("4a6f686e6e7946464d206861742064656e206772f6df74656e2050656e697321").unwrap();

        let mut gensig_array = [0u8; 32];
        gensig_array.copy_from_slice(&gensig[..]);

        let winner: [u8; 64] = [0; 64];
        let loser: [u8; 64] = [5; 64];
        let mut data: [u8; 64 * 32] = [5; 64 * 32];

        for i in 0..32 {
            data[i * 64..i * 64 + 64].clone_from_slice(&winner);
            let result = find_best_deadline_rust(&data, (i + 1) as u64, &gensig_array);
            let deadline = result.0;
            assert_eq!(3084580316385335914u64, deadline);
            data[i * 64..i * 64 + 64].clone_from_slice(&loser);
        }
    }
}
