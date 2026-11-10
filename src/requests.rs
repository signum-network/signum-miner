use crate::com::api::{FetchError, MiningInfoResponse};
use crate::com::client::{Client, ProxyDetails, SubmissionParameters};
use crate::future::prio_retry::PrioRetry;
use futures_util::stream::{StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use url::Url;

#[derive(Clone)]
pub struct RequestHandler {
    client: Client,
    tx_submit_data: mpsc::UnboundedSender<SubmissionParameters>,
}

impl RequestHandler {
    pub fn new(
        base_uri: Url,
        secret_phrases: HashMap<u64, String>,
        timeout: u64,
        total_size_gb: usize,
        send_proxy_details: bool,
        additional_headers: HashMap<String, String>,
        handle: tokio::runtime::Handle,
    ) -> RequestHandler {
        let proxy_details = if send_proxy_details {
            ProxyDetails::Enabled
        } else {
            ProxyDetails::Disabled
        };

        let client = Client::new(
            base_uri,
            secret_phrases,
            timeout,
            total_size_gb,
            proxy_details,
            additional_headers,
        );

        let (tx_submit_data, rx_submit_nonce_data) = mpsc::unbounded_channel();
        RequestHandler::handle_submissions(
            client.clone(),
            rx_submit_nonce_data,
            tx_submit_data.clone(),
            handle,
        );

        RequestHandler {
            client,
            tx_submit_data,
        }
    }

    fn handle_submissions(
        client: Client,
        rx: mpsc::UnboundedReceiver<SubmissionParameters>,
        tx_submit_data: mpsc::UnboundedSender<SubmissionParameters>,
        handle: tokio::runtime::Handle,
    ) {
        handle.spawn(async move {
            let wrapped_rx = UnboundedReceiverStream::new(rx);
            // Retries back off 3s, 6s, 12s, ... and never wait longer than
            // 60s, so a share still gets several attempts within a block.
            let stream =
                PrioRetry::new(wrapped_rx, Duration::from_secs(3), Duration::from_secs(60));

            let mut stream = Box::pin(stream);
            while let Some(submission_params) = stream.as_mut().next().await {
                let tx_submit_data = tx_submit_data.clone();
                let result = client.clone().submit_nonce(&submission_params).await;

                match result {
                    Ok(res) => {
                        if submission_params.deadline != res.deadline {
                            log_deadline_mismatch(
                                submission_params.height,
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                                res.deadline,
                            );
                        } else {
                            log_submission_accepted(
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                            );
                        }
                    }
                    Err(FetchError::Pool(e)) => {
                        if e.message.is_empty() || e.message == "limit exceeded" {
                            log_pool_busy(
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                            );
                            if tx_submit_data.send(submission_params).is_err() {
                                error!("can't send submission params");
                            }
                        } else {
                            log_submission_not_accepted(
                                submission_params.height,
                                submission_params.account_id,
                                submission_params.nonce,
                                submission_params.deadline,
                                e.code,
                                &e.message,
                            );
                        }
                    }
                    Err(FetchError::Http(x)) => {
                        log_submission_failed(
                            submission_params.account_id,
                            submission_params.nonce,
                            submission_params.deadline,
                            &x.to_string(),
                        );
                        if tx_submit_data.send(submission_params).is_err() {
                            error!("can't send submission params");
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    pub fn get_mining_info<'a>(&'a self) -> impl std::future::Future<Output = Result<MiningInfoResponse, FetchError>> + 'a {
        self.client.get_mining_info()
    }

    pub fn submit_nonce(
        &self,
        account_id: u64,
        nonce: u64,
        height: u64,
        block: u64,
        deadline_unadjusted: u64,
        deadline: u64,
        gen_sig: [u8; 32],
    ) {
        let res = self.tx_submit_data.send(SubmissionParameters {
            account_id,
            nonce,
            height,
            block,
            deadline_unadjusted,
            deadline,
            gen_sig,
        });
        if let Err(e) = res {
            error!("can't send submission params: {}", e);
        }
    }
}

fn log_deadline_mismatch(
    height: u64,
    account_id: u64,
    nonce: u64,
    deadline: u64,
    deadline_pool: u64,
) {
    error!(
        "submit: deadlines mismatch, height={}, account={}, nonce={}, \
         deadline_miner={}, deadline_pool={}",
        height, account_id, nonce, deadline, deadline_pool
    );
}

fn log_submission_failed(account_id: u64, nonce: u64, deadline: u64, err: &str) {
    warn!(
        "{: <80}",
        format!(
            "submission failed, retrying: account={}, nonce={}, deadline={}, description={}",
            account_id, nonce, deadline, err
        )
    );
}

fn log_submission_not_accepted(
    height: u64,
    account_id: u64,
    nonce: u64,
    deadline: u64,
    err_code: i32,
    msg: &str,
) {
    error!(
        "submission not accepted: height={}, account={}, nonce={}, \
         deadline={}\n\tcode: {}\n\tmessage: {}",
        height, account_id, nonce, deadline, err_code, msg,
    );
}

fn log_submission_accepted(account_id: u64, nonce: u64, deadline: u64) {
    info!(
        "deadline accepted: account={}, nonce={}, deadline={}",
        account_id, nonce, deadline
    );
}

fn log_pool_busy(account_id: u64, nonce: u64, deadline: u64) {
    info!(
        "pool busy, retrying: account={}, nonce={}, deadline={}",
        account_id, nonce, deadline
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::runtime::Runtime;

    static BASE_URL: &str = "http://94.130.178.37:31000";

    #[test]
    fn test_submit_nonce() {
    use url::Url; // sicherstellen, dass url::Url verwendet wird
    let rt = Runtime::new().expect("can't create runtime");
    let handle = rt.handle().clone();

    // erzwinge url::Url statt reqwest::Url
    let base_url: Url = BASE_URL.parse().expect("invalid URL");

    let request_handler = RequestHandler::new(
        base_url,
        HashMap::new(),
        3,
        12,
        true,
        HashMap::new(),
        handle,
    );

    request_handler.submit_nonce(1337, 12, 111, 0, 7123, 1193, [0; 32]);
}
}
