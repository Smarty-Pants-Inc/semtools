use reqwest::{Client, multipart};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::time::sleep;

use crate::parse::config::LlamaParseConfig;
use crate::parse::error::JobError;

#[derive(Debug, Serialize, Deserialize)]
struct JobResponse {
    id: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobStatus {
    status: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JobResult {
    markdown: String,
}

pub struct ParseClient {
    client: Client,
}

impl ParseClient {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
        }
    }

    pub async fn create_parse_job_with_retry(
        &self,
        file_path: &str,
        base_url: &str,
        api_key: &str,
        config: &LlamaParseConfig,
    ) -> Result<String, JobError> {
        let file_path = file_path.to_string();
        let base_url = base_url.to_string();
        let api_key = api_key.to_string();
        let parse_kwargs = config.parse_kwargs.clone();

        let mut last_error = None;

        for attempt in 0..=config.max_retries {
            match self
                .create_parse_job(&file_path, &base_url, &api_key, &parse_kwargs)
                .await
            {
                Ok(job_id) => return Ok(job_id),
                Err(JobError::HttpError(err)) => {
                    last_error = Some(err.to_string());

                    // Don't retry on the last attempt
                    if attempt == config.max_retries {
                        return Err(JobError::RetryExhausted(format!(
                            "Job creation failed after {} attempts. Last error: {}",
                            config.max_retries + 1,
                            err
                        )));
                    }

                    // Check if error is retryable
                    let is_retryable = err.is_connect()
                        || err.is_timeout()
                        || err.is_request()
                        || err.to_string().contains("broken pipe")
                        || err.to_string().contains("connection reset")
                        || err.to_string().contains("connection aborted")
                        || err.to_string().contains("network unreachable")
                        || (err.status().map(|s| s.is_server_error()).unwrap_or(false));

                    if !is_retryable {
                        return Err(JobError::HttpError(err));
                    }

                    // Calculate backoff delay
                    let delay = config.retry_delay_ms as f64
                        * config.backoff_multiplier.powi(attempt as i32);
                    let delay_ms = delay as u64;

                    eprintln!(
                        "Job creation failed (attempt {}/{}): {}. Retrying in {}ms...",
                        attempt + 1,
                        config.max_retries + 1,
                        err,
                        delay_ms
                    );

                    sleep(Duration::from_millis(delay_ms)).await;
                }
                Err(other_err) => return Err(other_err), // Don't retry non-HTTP errors
            }
        }

        // This should never be reached due to the logic above, but just in case
        Err(JobError::RetryExhausted(format!(
            "Unexpected retry exhaustion during job creation. Last error: {}",
            last_error.unwrap_or_else(|| "Unknown".to_string())
        )))
    }

    pub async fn poll_for_result_with_retry(
        &self,
        job_id: &str,
        base_url: &str,
        api_key: &str,
        config: &LlamaParseConfig,
    ) -> Result<String, JobError> {
        let job_id = job_id.to_string();
        let base_url = base_url.to_string();
        let api_key = api_key.to_string();

        let mut last_error = None;

        for attempt in 0..=config.max_retries {
            match self
                .poll_for_result(
                    &job_id,
                    &base_url,
                    &api_key,
                    config.max_timeout,
                    config.check_interval,
                )
                .await
            {
                Ok(result) => return Ok(result),
                Err(JobError::HttpError(err)) => {
                    last_error = Some(err.to_string());

                    // Don't retry on the last attempt
                    if attempt == config.max_retries {
                        return Err(JobError::RetryExhausted(format!(
                            "Polling failed after {} attempts. Last error: {}",
                            config.max_retries + 1,
                            err
                        )));
                    }

                    // Check if error is retryable
                    let is_retryable = err.is_connect()
                        || err.is_timeout()
                        || err.is_request()
                        || err.to_string().contains("broken pipe")
                        || err.to_string().contains("connection reset")
                        || err.to_string().contains("connection aborted")
                        || err.to_string().contains("network unreachable")
                        || (err.status().map(|s| s.is_server_error()).unwrap_or(false));

                    if !is_retryable {
                        return Err(JobError::HttpError(err));
                    }

                    // Calculate backoff delay
                    let delay = config.retry_delay_ms as f64
                        * config.backoff_multiplier.powi(attempt as i32);
                    let delay_ms = delay as u64;

                    eprintln!(
                        "Polling failed (attempt {}/{}): {}. Retrying in {}ms...",
                        attempt + 1,
                        config.max_retries + 1,
                        err,
                        delay_ms
                    );

                    sleep(Duration::from_millis(delay_ms)).await;
                }
                Err(JobError::TimeoutError) => {
                    // Timeout errors are not retryable as they indicate the job itself timed out
                    return Err(JobError::TimeoutError);
                }
                Err(other_err) => return Err(other_err), // Don't retry other errors
            }
        }

        // This should never be reached due to the logic above, but just in case
        Err(JobError::RetryExhausted(format!(
            "Unexpected retry exhaustion during polling. Last error: {}",
            last_error.unwrap_or_else(|| "Unknown".to_string())
        )))
    }

    async fn create_parse_job(
        &self,
        file_path: &str,
        base_url: &str,
        api_key: &str,
        parse_kwargs: &HashMap<String, String>,
    ) -> Result<String, JobError> {
        let file_content = fs::read(file_path)?;
        let filename = Path::new(file_path).file_name().unwrap().to_str().unwrap();

        let mime_type = mime_guess::from_path(file_path)
            .first_or_octet_stream()
            .to_string();

        let file_part = multipart::Part::bytes(file_content)
            .file_name(filename.to_string())
            .mime_str(&mime_type)
            .map_err(|e| JobError::InvalidResponse(e.to_string()))?;

        let mut form = multipart::Form::new().part("file", file_part);

        // Add parse kwargs as form data
        for (key, value) in parse_kwargs {
            form = form.text(key.clone(), value.clone());
        }

        let response = self
            .client
            .post(format!("{base_url}/api/parsing/upload"))
            .header("Authorization", format!("Bearer {api_key}"))
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(JobError::InvalidResponse(format!(
                "Upload failed: {error_text}"
            )));
        }

        let job_response: JobResponse = response.json().await?;
        Ok(job_response.id)
    }

    async fn poll_for_result(
        &self,
        job_id: &str,
        base_url: &str,
        api_key: &str,
        max_timeout: u64,
        check_interval: u64,
    ) -> Result<String, JobError> {
        let start = SystemTime::now();
        let timeout_duration = Duration::from_secs(max_timeout);

        loop {
            sleep(Duration::from_secs(check_interval)).await;

            // Check if we've timed out
            if start.elapsed().unwrap_or_default() > timeout_duration {
                return Err(JobError::TimeoutError);
            }

            // Check job status
            let status_response = self
                .client
                .get(format!("{base_url}/api/parsing/job/{job_id}"))
                .header("Authorization", format!("Bearer {api_key}"))
                .send()
                .await?;

            if !status_response.status().is_success() {
                continue; // Retry on error
            }

            let job_status: JobStatus = status_response.json().await?;

            match job_status.status.as_str() {
                "SUCCESS" => {
                    // Get the result
                    let result_response = self
                        .client
                        .get(format!(
                            "{base_url}/api/parsing/job/{job_id}/result/markdown"
                        ))
                        .header("Authorization", format!("Bearer {api_key}"))
                        .send()
                        .await?;

                    if !result_response.status().is_success() {
                        return Err(JobError::InvalidResponse(
                            "Failed to get result".to_string(),
                        ));
                    }

                    let job_result: JobResult = result_response.json().await?;
                    return Ok(job_result.markdown);
                }
                "PENDING" => {
                    // Continue polling
                    continue;
                }
                "ERROR" | "CANCELED" => {
                    return Err(JobError::InvalidResponse(format!(
                        "Job failed with status: {}",
                        job_status.status
                    )));
                }
                _ => {
                    return Err(JobError::InvalidResponse(format!(
                        "Unknown status: {}",
                        job_status.status
                    )));
                }
            }
        }
    }
}

impl Default for ParseClient {
    fn default() -> Self {
        Self::new()
    }
}
