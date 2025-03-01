//! A worker represents a mediasoup C++ thread that runs on a single CPU core and handles
//! [`Router`] instances.

mod channel;
mod common;
mod utils;

use crate::data_structures::AppData;
use crate::messages::{
    WorkerCloseRequest, WorkerCreateRouterRequest, WorkerCreateWebRtcServerRequest,
    WorkerDumpRequest, WorkerUpdateSettingsRequest,
};
pub use crate::ortc::RtpCapabilitiesError;
use crate::router::{Router, RouterId, RouterOptions};
use crate::webrtc_server::{WebRtcServer, WebRtcServerId, WebRtcServerOptions};
use crate::worker::channel::BufferMessagesGuard;
pub use crate::worker::utils::ExitError;
use crate::worker_manager::WorkerManager;
use crate::{ortc, uuid_based_wrapper_type};
use async_executor::Executor;
pub(crate) use channel::{Channel, NotificationError, NotificationParseError};
pub(crate) use common::{SubscriptionHandler, SubscriptionTarget};
use event_listener_primitives::{Bag, BagOnce, HandlerId};
use futures_lite::FutureExt;
use log::{debug, error, warn};
use mediasoup_sys::fbs;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::error::Error;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::{fmt, io};
use thiserror::Error;
use utils::WorkerRunResult;
use uuid::Uuid;

uuid_based_wrapper_type!(
    /// Worker identifier.
    WorkerId
);

/// Error that caused request to mediasoup-worker request to fail.
#[derive(Debug, Error)]
pub enum RequestError {
    /// Channel already closed.
    #[error("Channel already closed")]
    ChannelClosed,
    /// Request timed out.
    #[error("Request timed out")]
    TimedOut,
    /// Received response error.
    #[error("Received response error: {reason}")]
    Response {
        /// Error reason.
        reason: String,
    },
    /// Failed to parse response from worker.
    #[error("Failed to parse response from worker: {error}")]
    FailedToParse {
        /// Error reason.
        error: String,
    },
    /// Worker did not return any data in response.
    #[error("Worker did not return any data in response")]
    NoData,
    /// Response conversion error.
    #[error("Response conversion error: {0}")]
    ResponseConversion(Box<dyn Error>),
}

/// Logging level for logs generated by the media worker thread (check the
/// [Debugging](https://mediasoup.org/documentation/v3/mediasoup/debugging/)
/// documentation on TypeScript implementation and generic
/// [Rust-specific](https://rust-lang-nursery.github.io/rust-cookbook/development_tools/debugging/log.html) [docs](https://docs.rs/env_logger)).
///
/// Default [`WorkerLogLevel::Error`].
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerLogLevel {
    /// Log all severities.
    Debug,
    /// Log "warn" and "error" severities.
    Warn,
    /// Log "error" severity.
    Error,
    /// Do not log anything.
    None,
}

impl Default for WorkerLogLevel {
    fn default() -> Self {
        Self::Error
    }
}

impl WorkerLogLevel {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Warn => "warn",
            Self::Error => "error",
            Self::None => "none",
        }
    }
}

/// Log tags for debugging. Check the meaning of each available tag in the
/// [Debugging](https://mediasoup.org/documentation/v3/mediasoup/debugging/) documentation.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerLogTag {
    /// Logs about software/library versions, configuration and process information.
    Info,
    /// Logs about ICE.
    Ice,
    /// Logs about DTLS.
    Dtls,
    /// Logs about RTP.
    Rtp,
    /// Logs about SRTP encryption/decryption.
    Srtp,
    /// Logs about RTCP.
    Rtcp,
    /// Logs about RTP retransmission, including NACK/PLI/FIR.
    Rtx,
    /// Logs about transport bandwidth estimation.
    Bwe,
    /// Logs related to the scores of Producers and Consumers.
    Score,
    /// Logs about video simulcast.
    Simulcast,
    /// Logs about video SVC.
    Svc,
    /// Logs about SCTP (DataChannel).
    Sctp,
    /// Logs about messages (can be SCTP messages or direct messages).
    Message,
}

impl WorkerLogTag {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Ice => "ice",
            Self::Dtls => "dtls",
            Self::Rtp => "rtp",
            Self::Srtp => "srtp",
            Self::Rtcp => "rtcp",
            Self::Rtx => "rtx",
            Self::Bwe => "bwe",
            Self::Score => "score",
            Self::Simulcast => "simulcast",
            Self::Svc => "svc",
            Self::Sctp => "sctp",
            Self::Message => "message",
        }
    }
}

/// DTLS certificate and private key.
#[derive(Debug, Clone)]
pub struct WorkerDtlsFiles {
    /// Path to the DTLS public certificate file in PEM format.
    pub certificate: PathBuf,
    /// Path to the DTLS certificate private key file in PEM format.
    pub private_key: PathBuf,
}

/// Settings for worker to be created with.
#[derive(Clone)]
#[non_exhaustive]
pub struct WorkerSettings {
    /// Logging level for logs generated by the media worker thread.
    ///
    /// Default [`WorkerLogLevel::Error`].
    pub log_level: WorkerLogLevel,
    /// Log tags for debugging. Check the meaning of each available tag in the
    /// [Debugging](https://mediasoup.org/documentation/v3/mediasoup/debugging/) documentation.
    pub log_tags: Vec<WorkerLogTag>,
    /// RTC ports range for ICE, DTLS, RTP, etc. Default 10000..=59999.
    pub rtc_ports_range: RangeInclusive<u16>,
    /// DTLS certificate and private key.
    ///
    /// If `None`, a certificate is dynamically created.
    pub dtls_files: Option<WorkerDtlsFiles>,
    /// Field trials for libwebrtc.
    ///
    /// NOTE: For advanced users only. An invalid value will make the worker crash.
    /// Default value is
    /// "WebRTC-Bwe-AlrLimitedBackoff/Enabled/".
    #[doc(hidden)]
    pub libwebrtc_field_trials: Option<String>,
    /// Function that will be called under worker thread before worker starts, can be used for
    /// pinning worker threads to CPU cores.
    pub thread_initializer: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Custom application data.
    pub app_data: AppData,
}

impl Default for WorkerSettings {
    fn default() -> Self {
        Self {
            log_level: WorkerLogLevel::Debug,
            log_tags: vec![
                WorkerLogTag::Info,
                WorkerLogTag::Ice,
                WorkerLogTag::Dtls,
                WorkerLogTag::Rtp,
                WorkerLogTag::Srtp,
                WorkerLogTag::Rtcp,
                WorkerLogTag::Rtx,
                WorkerLogTag::Bwe,
                WorkerLogTag::Score,
                WorkerLogTag::Simulcast,
                WorkerLogTag::Svc,
                WorkerLogTag::Sctp,
                WorkerLogTag::Message,
            ],
            rtc_ports_range: 10000..=59999,
            dtls_files: None,
            libwebrtc_field_trials: None,
            thread_initializer: None,
            app_data: AppData::default(),
        }
    }
}

impl fmt::Debug for WorkerSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let WorkerSettings {
            log_level,
            log_tags,
            rtc_ports_range,
            dtls_files,
            libwebrtc_field_trials,
            thread_initializer,
            app_data,
        } = self;

        f.debug_struct("WorkerSettings")
            .field("log_level", &log_level)
            .field("log_tags", &log_tags)
            .field("rtc_ports_range", &rtc_ports_range)
            .field("dtls_files", &dtls_files)
            .field("libwebrtc_field_trials", &libwebrtc_field_trials)
            .field(
                "thread_initializer",
                &thread_initializer.as_ref().map(|_| "ThreadInitializer"),
            )
            .field("app_data", &app_data)
            .finish()
    }
}

/// Worker settings that can be updated in runtime.
#[derive(Default, Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct WorkerUpdateSettings {
    /// Logging level for logs generated by the media worker thread.
    ///
    /// If `None`, logging level will not be updated.
    pub log_level: Option<WorkerLogLevel>,
    /// Log tags for debugging. Check the meaning of each available tag in the
    /// [Debugging](https://mediasoup.org/documentation/v3/mediasoup/debugging/) documentation.
    ///
    /// If `None`, log tags will not be updated.
    pub log_tags: Option<Vec<WorkerLogTag>>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
pub struct ChannelMessageHandlers {
    pub channel_request_handlers: Vec<Uuid>,
    pub channel_notification_handlers: Vec<Uuid>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
#[doc(hidden)]
#[non_exhaustive]
pub struct WorkerDump {
    // Dump has `pid` field too, but it is useless here because of thead-based worker usage
    pub router_ids: Vec<RouterId>,
    #[serde(rename = "webRtcServerIds")]
    pub webrtc_server_ids: Vec<WebRtcServerId>,
    pub channel_message_handlers: ChannelMessageHandlers,
}

/// Error that caused [`Worker::create_webrtc_server`] to fail.
#[derive(Debug, Error)]
pub enum CreateWebRtcServerError {
    /// Request to worker failed
    #[error("Request to worker failed: {0}")]
    Request(RequestError),
}

/// Error that caused [`Worker::create_router`] to fail.
#[derive(Debug, Error)]
pub enum CreateRouterError {
    /// RTP capabilities generation error
    #[error("RTP capabilities generation error: {0}")]
    FailedRtpCapabilitiesGeneration(RtpCapabilitiesError),
    /// Request to worker failed
    #[error("Request to worker failed: {0}")]
    Request(RequestError),
}

#[derive(Default)]
#[allow(clippy::type_complexity)]
struct Handlers {
    new_router: Bag<Arc<dyn Fn(&Router) + Send + Sync>, Router>,
    new_webrtc_server: Bag<Arc<dyn Fn(&WebRtcServer) + Send + Sync>, WebRtcServer>,
    #[allow(clippy::type_complexity)]
    dead: BagOnce<Box<dyn FnOnce(Result<(), ExitError>) + Send>>,
    close: BagOnce<Box<dyn FnOnce() + Send>>,
}

struct Inner {
    id: WorkerId,
    channel: Channel,
    executor: Arc<Executor<'static>>,
    handlers: Handlers,
    app_data: AppData,
    closed: Arc<AtomicBool>,
    // Make sure worker is not dropped until this worker manager is not dropped
    _worker_manager: WorkerManager,
}

impl Drop for Inner {
    fn drop(&mut self) {
        debug!("drop()");

        self.close();
    }
}

impl Inner {
    async fn new<OE: FnOnce() + Send + 'static>(
        executor: Arc<Executor<'static>>,
        WorkerSettings {
            log_level,
            log_tags,
            rtc_ports_range,
            dtls_files,
            libwebrtc_field_trials,
            thread_initializer,
            app_data,
        }: WorkerSettings,
        worker_manager: WorkerManager,
        on_exit: OE,
    ) -> io::Result<Arc<Self>> {
        debug!("new()");

        let mut spawn_args: Vec<String> = vec!["".to_string()];

        spawn_args.push(format!("--logLevel={}", log_level.as_str()));
        for log_tag in log_tags {
            spawn_args.push(format!("--logTag={}", log_tag.as_str()));
        }

        if rtc_ports_range.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid RTC ports range",
            ));
        }
        spawn_args.push(format!("--rtcMinPort={}", rtc_ports_range.start()));
        spawn_args.push(format!("--rtcMaxPort={}", rtc_ports_range.end()));

        if let Some(dtls_files) = dtls_files {
            spawn_args.push(format!(
                "--dtlsCertificateFile={}",
                dtls_files
                    .certificate
                    .to_str()
                    .expect("Paths are only expected to be utf8")
            ));
            spawn_args.push(format!(
                "--dtlsPrivateKeyFile={}",
                dtls_files
                    .private_key
                    .to_str()
                    .expect("Paths are only expected to be utf8")
            ));
        }

        if let Some(libwebrtc_field_trials) = libwebrtc_field_trials {
            spawn_args.push(format!(
                "--libwebrtcFieldTrials={}",
                libwebrtc_field_trials.as_str()
            ));
        }

        let id = WorkerId::new();
        debug!(
            "spawning worker with arguments [id:{}]: {}",
            id,
            spawn_args.join(" ")
        );

        let closed = Arc::new(AtomicBool::new(false));

        let (mut status_sender, status_receiver) = async_oneshot::oneshot();
        let WorkerRunResult {
            channel,
            buffer_worker_messages_guard,
        } = utils::run_worker_with_channels(
            id,
            thread_initializer,
            spawn_args,
            Arc::clone(&closed),
            move |result| {
                let _ = status_sender.send(result);
                on_exit();
            },
        );

        let handlers = Handlers::default();

        let mut inner = Self {
            id,
            channel,
            executor,
            handlers,
            app_data,
            closed,
            _worker_manager: worker_manager,
        };

        inner.setup_message_handling();

        let (mut early_status_sender, early_status_receiver) = async_oneshot::oneshot();

        let inner = Arc::new(inner);
        {
            let inner_weak = Arc::downgrade(&inner);
            inner
                .executor
                .spawn(async move {
                    let status = status_receiver.await.unwrap_or(Err(ExitError::Unexpected));
                    let _ = early_status_sender.send(status);

                    if let Some(inner) = inner_weak.upgrade() {
                        warn!("worker exited [id:{}]: {:?}", id, status);

                        if !inner.closed.swap(true, Ordering::SeqCst) {
                            inner.handlers.dead.call(|callback| {
                                callback(status);
                            });
                            inner.handlers.close.call_simple();
                        }
                    }
                })
                .detach();
        }

        inner
            .wait_for_worker_ready(buffer_worker_messages_guard)
            .or(async {
                let status = early_status_receiver
                    .await
                    .unwrap_or(Err(ExitError::Unexpected));
                let error_message = format!(
                    "worker thread exited before being ready [id:{}]: exit status {:?}",
                    inner.id, status,
                );
                Err(io::Error::new(io::ErrorKind::NotFound, error_message))
            })
            .await?;

        Ok(inner)
    }

    async fn wait_for_worker_ready(
        &self,
        buffer_worker_messages_guard: BufferMessagesGuard,
    ) -> io::Result<()> {
        #[derive(Deserialize)]
        #[serde(tag = "event", rename_all = "lowercase")]
        enum Notification {
            Running,
        }

        let (sender, receiver) = async_oneshot::oneshot();
        let id = self.id;
        let sender = Mutex::new(Some(sender));
        let _handler = self.channel.subscribe_to_notifications(
            SubscriptionTarget::String(std::process::id().to_string()),
            move |notification| {
                let result = match notification.event().unwrap() {
                    fbs::notification::Event::WorkerRunning => {
                        debug!("worker thread running [id:{}]", id);
                        Ok(())
                    }
                    _ => Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("unexpected first notification from worker [id:{id}]"),
                    )),
                };

                let _ = sender
                    .lock()
                    .take()
                    .expect("Receiving more than one worker notification")
                    .send(result);
            },
        );

        // Allow worker messages to go through
        drop(buffer_worker_messages_guard);

        receiver.await.map_err(|_closed| {
            io::Error::new(io::ErrorKind::Other, "Worker dropped before it is ready")
        })?
    }

    fn setup_message_handling(&mut self) {
        let channel_receiver = self.channel.get_internal_message_receiver();
        let id = self.id;
        let closed = Arc::clone(&self.closed);
        self.executor
            .spawn(async move {
                while let Ok(message) = channel_receiver.recv().await {
                    match message {
                        channel::InternalMessage::Debug(text) => debug!("[id:{}] {}", id, text),
                        channel::InternalMessage::Warn(text) => warn!("[id:{}] {}", id, text),
                        channel::InternalMessage::Error(text) => {
                            if !closed.load(Ordering::SeqCst) {
                                error!("[id:{}] {}", id, text)
                            }
                        }
                        channel::InternalMessage::Dump(text) => eprintln!("{text}"),
                        channel::InternalMessage::Unexpected(data) => error!(
                            "worker[id:{}] unexpected channel data: {}",
                            id,
                            String::from_utf8_lossy(&data)
                        ),
                    }
                }
            })
            .detach();
    }

    fn close(&self) {
        let already_closed = self.closed.swap(true, Ordering::SeqCst);

        if !already_closed {
            let channel = self.channel.clone();

            self.executor
                .spawn(async move {
                    let _ = channel.request("", WorkerCloseRequest {}).await;

                    // Drop channels in here after response from worker
                    drop(channel);
                })
                .detach();

            self.handlers.close.call_simple();
        }
    }
}

/// A worker represents a mediasoup C++ thread that runs on a single CPU core and handles
/// [`Router`] instances.
#[derive(Clone)]
#[must_use = "Worker will be destroyed on drop, make sure to keep it around for as long as needed"]
pub struct Worker {
    inner: Arc<Inner>,
}

impl fmt::Debug for Worker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.inner.id)
            .field("closed", &self.inner.closed)
            .finish()
    }
}

impl Worker {
    pub(super) async fn new<OE: FnOnce() + Send + 'static>(
        executor: Arc<Executor<'static>>,
        worker_settings: WorkerSettings,
        worker_manager: WorkerManager,
        on_exit: OE,
    ) -> io::Result<Self> {
        let inner = Inner::new(executor, worker_settings, worker_manager, on_exit).await?;

        Ok(Self { inner })
    }

    /// Worker id.
    #[must_use]
    pub fn id(&self) -> WorkerId {
        self.inner.id
    }

    /// Worker manager to which worker belongs.
    pub fn worker_manager(&self) -> &WorkerManager {
        &self.inner._worker_manager
    }

    /// Custom application data.
    #[must_use]
    pub fn app_data(&self) -> &AppData {
        &self.inner.app_data
    }

    /// Whether the worker is closed.
    #[must_use]
    pub fn closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Dump Worker.
    #[doc(hidden)]
    pub async fn dump(&self) -> Result<WorkerDump, RequestError> {
        debug!("dump()");

        self.inner.channel.request("", WorkerDumpRequest {}).await
    }

    /// Updates the worker settings in runtime. Just a subset of the worker settings can be updated.
    pub async fn update_settings(&self, data: WorkerUpdateSettings) -> Result<(), RequestError> {
        debug!("update_settings()");

        match self
            .inner
            .channel
            .request("", WorkerUpdateSettingsRequest { data })
            .await
        {
            Ok(_) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Create a WebRtcServer.
    ///
    /// Worker will be kept alive as long as at least one WebRTC server instance is alive.
    pub async fn create_webrtc_server(
        &self,
        webrtc_server_options: WebRtcServerOptions,
    ) -> Result<WebRtcServer, CreateWebRtcServerError> {
        debug!("create_webrtc_server()");

        let WebRtcServerOptions {
            listen_infos,
            app_data,
        } = webrtc_server_options;

        let webrtc_server_id = WebRtcServerId::new();

        let _buffer_guard = self
            .inner
            .channel
            .buffer_messages_for(webrtc_server_id.into());

        self.inner
            .channel
            .request(
                "",
                WorkerCreateWebRtcServerRequest {
                    webrtc_server_id,
                    listen_infos,
                },
            )
            .await
            .map_err(CreateWebRtcServerError::Request)?;

        let webrtc_server = WebRtcServer::new(
            webrtc_server_id,
            Arc::clone(&self.inner.executor),
            self.inner.channel.clone(),
            app_data,
            self.clone(),
        );

        self.inner
            .handlers
            .new_webrtc_server
            .call_simple(&webrtc_server);

        Ok(webrtc_server)
    }

    /// Create a Router.
    ///
    /// Worker will be kept alive as long as at least one router instance is alive.
    pub async fn create_router(
        &self,
        router_options: RouterOptions,
    ) -> Result<Router, CreateRouterError> {
        debug!("create_router()");

        let RouterOptions {
            app_data,
            media_codecs,
        } = router_options;

        let rtp_capabilities = ortc::generate_router_rtp_capabilities(media_codecs)
            .map_err(CreateRouterError::FailedRtpCapabilitiesGeneration)?;

        let router_id = RouterId::new();

        let _buffer_guard = self.inner.channel.buffer_messages_for(router_id.into());

        self.inner
            .channel
            .request("", WorkerCreateRouterRequest { router_id })
            .await
            .map_err(CreateRouterError::Request)?;

        let router = Router::new(
            router_id,
            Arc::clone(&self.inner.executor),
            self.inner.channel.clone(),
            rtp_capabilities,
            app_data,
            self.clone(),
        );

        self.inner.handlers.new_router.call_simple(&router);

        Ok(router)
    }

    /// Callback is called when a new WebRTC server is created.
    pub fn on_new_webrtc_server<F: Fn(&WebRtcServer) + Send + Sync + 'static>(
        &self,
        callback: F,
    ) -> HandlerId {
        self.inner
            .handlers
            .new_webrtc_server
            .add(Arc::new(callback))
    }

    /// Callback is called when a new router is created.
    pub fn on_new_router<F: Fn(&Router) + Send + Sync + 'static>(&self, callback: F) -> HandlerId {
        self.inner.handlers.new_router.add(Arc::new(callback))
    }

    /// Callback is called when the worker thread unexpectedly dies.
    pub fn on_dead<F: FnOnce(Result<(), ExitError>) + Send + Sync + 'static>(
        &self,
        callback: F,
    ) -> HandlerId {
        self.inner.handlers.dead.add(Box::new(callback))
    }

    /// Callback is called when the worker is closed for whatever reason.
    ///
    /// NOTE: Callback will be called in place if worker is already closed.
    pub fn on_close<F: FnOnce() + Send + 'static>(&self, callback: F) -> HandlerId {
        let handler_id = self.inner.handlers.close.add(Box::new(callback));
        if self.inner.closed.load(Ordering::Relaxed) {
            self.inner.handlers.close.call_simple();
        }
        handler_id
    }

    #[cfg(test)]
    pub(crate) fn close(&self) {
        self.inner.close();
    }
}
