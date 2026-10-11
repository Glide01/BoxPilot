//! Taildrop (files other tailnet nodes sent to a Tailscale endpoint) and
//! Tailscale TLS certificates: `SubscribeTaildropInbox`,
//! `MarkTaildropInboxRead`, `DownloadTaildropFile`, `DeleteTaildropFile`,
//! `CancelTaildropReceiving`, `GetTailscaleCertificate`.
//!
//! Sending files (`SendTaildropFiles`) and SSH (`StartTailscaleSSHSession`)
//! are bidirectional streams, which gRPC-Web can't carry — out of reach (see
//! the ADR).
//!
//! Endpoint tags: the Taildrop calls accept an empty `endpoint_tag` for "the
//! first Tailscale endpoint"; `GetTailscaleCertificate` needs the exact tag.
//! Without any Tailscale endpoint the unary calls fail with `NOT_FOUND`, and
//! the inbox stream sends one empty inbox, then stays idle.

use super::transport::{ApiError, IDLE_STREAM_READ_TIMEOUT};
use super::{pb, SingBoxApi};
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Delete retries for a few seconds on Windows while another process holds
/// the file open; cancel and mark-read are instant but share the bound.
const TAILDROP_ACTION_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-read bound while a download streams; chunks (16 KiB) flow back to
/// back from a local file, so any real stall is a failure.
const DOWNLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// A first certificate request runs an ACME order (Let's Encrypt, DNS-01
/// through the coordination server) before answering — tens of seconds is
/// normal. Giving up drops the request, which cancels it in sing-box.
const CERTIFICATE_TIMEOUT: Duration = Duration::from_secs(120);

impl SingBoxApi {
    /// Stream `SubscribeTaildropInbox` for one endpoint (`endpoint_tag`
    /// empty = the first Tailscale endpoint): the full inbox on subscribe,
    /// then again — whole, not a diff — whenever a file arrives, progresses,
    /// finishes or is deleted. With no Tailscale endpoint: one empty inbox
    /// (no tag), then silence. Idle otherwise: `TimedOut` after
    /// `IDLE_STREAM_READ_TIMEOUT`; re-subscribe.
    pub fn stream_taildrop_inbox(
        &self,
        endpoint_tag: &str,
        mut on_inbox: impl FnMut(TaildropInbox) -> bool,
    ) -> Result<(), ApiError> {
        let request = pb::SubscribeTaildropInboxRequest {
            endpoint_tag: endpoint_tag.to_string(),
        };
        self.stream(
            "SubscribeTaildropInbox",
            &request,
            IDLE_STREAM_READ_TIMEOUT,
            |inbox: pb::TaildropInbox| on_inbox(TaildropInbox::from_proto(inbox)),
        )
    }

    /// `MarkTaildropInboxRead` — zero the endpoint's
    /// `TailscaleEndpointStatus::unread_file_count`. (Unread state is a
    /// count only; the inbox carries no per-file flag.)
    pub fn mark_taildrop_inbox_read(&self, endpoint_tag: &str) -> Result<(), ApiError> {
        let request = pb::MarkTaildropInboxReadRequest {
            endpoint_tag: endpoint_tag.to_string(),
        };
        self.unary_with_timeout("MarkTaildropInboxRead", &request, TAILDROP_ACTION_TIMEOUT)
    }

    /// `DownloadTaildropFile` — stream one received file (`name` as the
    /// inbox lists it) into `writer`. The file stays in the inbox; delete it
    /// separately. `on_progress(written, total)` runs after every chunk and
    /// cancels the download by returning `false`. Returns the byte count,
    /// checked against the size sing-box announced first.
    /// Errors: `NOT_FOUND` (no such file / endpoint) as `Api`.
    pub fn download_taildrop_file(
        &self,
        endpoint_tag: &str,
        name: &str,
        writer: &mut impl Write,
        mut on_progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<u64, TaildropDownloadError> {
        let request = pb::DownloadTaildropFileRequest {
            endpoint_tag: endpoint_tag.to_string(),
            name: name.to_string(),
        };
        let mut sink = DownloadSink::new(writer);
        let mut cancelled = false;
        let result = self.stream(
            "DownloadTaildropFile",
            &request,
            DOWNLOAD_READ_TIMEOUT,
            |chunk: pb::DownloadTaildropFileChunk| {
                if sink.accept(chunk).is_err() {
                    return false;
                }
                let (written, total) = sink.progress();
                cancelled = !on_progress(written, total);
                !cancelled
            },
        );
        // A write failure stops the stream with `Ok`, so check it first.
        if let Some(error) = sink.error.take() {
            return Err(error);
        }
        result.map_err(TaildropDownloadError::Api)?;
        if cancelled {
            return Err(TaildropDownloadError::Cancelled);
        }
        sink.finish()
    }

    /// `download_taildrop_file` into `path`, through a sibling `.partial`
    /// file renamed into place only once complete — a failed or cancelled
    /// download never leaves a truncated file under the real name, and an
    /// existing file there is replaced only on success.
    pub fn download_taildrop_file_to(
        &self,
        endpoint_tag: &str,
        name: &str,
        path: &Path,
        on_progress: impl FnMut(u64, u64) -> bool,
    ) -> Result<u64, TaildropDownloadError> {
        let partial = partial_path(path);
        let io_error = |e: io::Error| TaildropDownloadError::Io(e.to_string());
        let result = (|| {
            let mut file = io::BufWriter::new(fs::File::create(&partial).map_err(io_error)?);
            let written =
                self.download_taildrop_file(endpoint_tag, name, &mut file, on_progress)?;
            file.into_inner()
                .map_err(|e| io_error(e.into_error()))?
                .sync_all()
                .map_err(io_error)?;
            fs::rename(&partial, path).map_err(io_error)?;
            Ok(written)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&partial);
        }
        result
    }

    /// `DeleteTaildropFile` — remove a received file from the inbox (and
    /// from sing-box's Taildrop directory).
    pub fn delete_taildrop_file(&self, endpoint_tag: &str, name: &str) -> Result<(), ApiError> {
        let request = pb::DeleteTaildropFileRequest {
            endpoint_tag: endpoint_tag.to_string(),
            name: name.to_string(),
        };
        self.unary_with_timeout("DeleteTaildropFile", &request, TAILDROP_ACTION_TIMEOUT)
    }

    /// `CancelTaildropReceiving` — abort an incoming transfer
    /// (`TaildropReceivingFile::sender_id` + `name`). A transfer that already
    /// finished or vanished is a silent no-op.
    pub fn cancel_taildrop_receiving(
        &self,
        endpoint_tag: &str,
        sender_id: &str,
        name: &str,
    ) -> Result<(), ApiError> {
        let request = pb::CancelTaildropReceivingRequest {
            endpoint_tag: endpoint_tag.to_string(),
            sender_id: sender_id.to_string(),
            name: name.to_string(),
        };
        self.unary_with_timeout("CancelTaildropReceiving", &request, TAILDROP_ACTION_TIMEOUT)
    }

    /// `GetTailscaleCertificate` — a TLS certificate + private key (PEM) for
    /// `domain`, one of `TailscaleEndpointStatus::cert_domains`, issued
    /// through the tailnet's HTTPS feature. A cached pair is returned while
    /// it stays valid for at least `min_validity` (zero: any validity left);
    /// otherwise a new one is ordered, which can take tens of seconds.
    /// `endpoint_tag` must name the endpoint exactly. Errors: `NOT_FOUND`,
    /// `INVALID_ARGUMENT` (not Tailscale), `UNKNOWN` ("Tailscale is not
    /// ready yet", HTTPS not enabled for the tailnet, ACME failure).
    pub fn get_tailscale_certificate(
        &self,
        endpoint_tag: &str,
        domain: &str,
        min_validity: Duration,
    ) -> Result<TailscaleCertificate, ApiError> {
        let request = pb::TailscaleCertificateRequest {
            endpoint_tag: endpoint_tag.to_string(),
            domain: domain.to_string(),
            min_validity_seconds: min_validity.as_secs().min(i64::MAX as u64) as i64,
        };
        let certificate: pb::TailscaleCertificate =
            self.unary_with_timeout("GetTailscaleCertificate", &request, CERTIFICATE_TIMEOUT)?;
        Ok(TailscaleCertificate::from_proto(certificate))
    }
}

/// One endpoint's Taildrop inbox. Files arrive here; nothing is written
/// outside sing-box's Taildrop directory until the user downloads them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaildropInbox {
    /// Empty only in the placeholder sent when the config has no Tailscale
    /// endpoint.
    pub endpoint_tag: String,
    /// Completely received files, newest first.
    pub files: Vec<TaildropFile>,
    /// Transfers in progress, oldest first.
    pub receiving: Vec<TaildropReceivingFile>,
}

impl TaildropInbox {
    fn from_proto(inbox: pb::TaildropInbox) -> Self {
        Self {
            endpoint_tag: inbox.endpoint_tag,
            files: inbox
                .files
                .into_iter()
                .map(|file| TaildropFile {
                    name: file.name,
                    size: file.size.max(0) as u64,
                    sender_name: file.sender_name,
                    modified_at: (file.modified_at > 0).then_some(file.modified_at),
                })
                .collect(),
            receiving: inbox
                .receiving
                .into_iter()
                .map(|file| TaildropReceivingFile {
                    name: file.name,
                    size: (file.size >= 0).then_some(file.size as u64),
                    received_bytes: file.received_bytes.max(0) as u64,
                    sender_id: file.sender_id,
                    sender_name: file.sender_name,
                })
                .collect(),
        }
    }
}

/// A received file waiting in the inbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaildropFile {
    /// Base name, unique within the inbox — the key for download/delete.
    pub name: String,
    pub size: u64,
    /// Empty when sing-box restarted since the file arrived.
    pub sender_name: String,
    /// Unix seconds.
    pub modified_at: Option<i64>,
}

/// A file still being received.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaildropReceivingFile {
    pub name: String,
    /// `None` when the sender didn't announce a length.
    pub size: Option<u64>,
    pub received_bytes: u64,
    /// With `name`, identifies the transfer to `cancel_taildrop_receiving`.
    pub sender_id: String,
    pub sender_name: String,
}

/// A certificate + private key, PEM-encoded. `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq)]
pub struct TailscaleCertificate {
    pub certificate_pem: Vec<u8>,
    pub private_key_pem: Vec<u8>,
}

impl TailscaleCertificate {
    fn from_proto(certificate: pb::TailscaleCertificate) -> Self {
        Self {
            certificate_pem: certificate.certificate_pem,
            private_key_pem: certificate.private_key_pem,
        }
    }
}

impl fmt::Debug for TailscaleCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TailscaleCertificate")
            .field(
                "certificate_pem",
                &String::from_utf8_lossy(&self.certificate_pem),
            )
            .field(
                "private_key_pem",
                &format_args!("<{} bytes redacted>", self.private_key_pem.len()),
            )
            .finish()
    }
}

/// Why a Taildrop download failed. `Display` gives a user-presentable
/// message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TaildropDownloadError {
    Api(ApiError),
    /// Writing the local file failed.
    Io(String),
    /// The stream ended before (or ran past) the announced size — the file
    /// changed while downloading, or the response was malformed.
    SizeMismatch {
        expected: u64,
        received: u64,
    },
    /// `on_progress` returned `false`.
    Cancelled,
}

impl fmt::Display for TaildropDownloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TaildropDownloadError::Api(error) => error.fmt(f),
            TaildropDownloadError::Io(reason) => {
                f.write_str(&(crate::i18n::s().tailscale.write_failed)(reason))
            }
            TaildropDownloadError::SizeMismatch { expected, received } => f.write_str(
                &(crate::i18n::s().tailscale.download_incomplete)(*received, *expected),
            ),
            TaildropDownloadError::Cancelled => {
                f.write_str(crate::i18n::s().tailscale.download_cancelled)
            }
        }
    }
}

impl std::error::Error for TaildropDownloadError {}

impl From<TaildropDownloadError> for String {
    fn from(error: TaildropDownloadError) -> Self {
        error.to_string()
    }
}

/// `<path>.partial`, next to `path`.
fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

/// Folds `DownloadTaildropFileChunk`s into a writer. sing-box sends the size
/// alone first, then data-only chunks.
struct DownloadSink<'w, W: Write> {
    writer: &'w mut W,
    expected: Option<u64>,
    written: u64,
    error: Option<TaildropDownloadError>,
}

impl<'w, W: Write> DownloadSink<'w, W> {
    fn new(writer: &'w mut W) -> Self {
        Self {
            writer,
            expected: None,
            written: 0,
            error: None,
        }
    }

    fn accept(&mut self, chunk: pb::DownloadTaildropFileChunk) -> Result<(), ()> {
        if self.expected.is_none() {
            self.expected = Some(chunk.size.max(0) as u64);
        }
        if chunk.data.is_empty() {
            return Ok(());
        }
        let expected = self.expected.unwrap_or(0);
        let received = self.written + chunk.data.len() as u64;
        if received > expected {
            self.error = Some(TaildropDownloadError::SizeMismatch { expected, received });
            return Err(());
        }
        if let Err(e) = self.writer.write_all(&chunk.data) {
            self.error = Some(TaildropDownloadError::Io(e.to_string()));
            return Err(());
        }
        self.written = received;
        Ok(())
    }

    /// `(written, total)`; total is 0 until the size header arrived.
    fn progress(&self) -> (u64, u64) {
        (self.written, self.expected.unwrap_or(0))
    }

    fn finish(self) -> Result<u64, TaildropDownloadError> {
        let Some(expected) = self.expected else {
            return Err(TaildropDownloadError::Api(ApiError::InvalidResponse(
                "no file size".into(),
            )));
        };
        if self.written != expected {
            return Err(TaildropDownloadError::SizeMismatch {
                expected,
                received: self.written,
            });
        }
        self.writer
            .flush()
            .map_err(|e| TaildropDownloadError::Io(e.to_string()))?;
        Ok(self.written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(size: i64, data: &[u8]) -> pb::DownloadTaildropFileChunk {
        pb::DownloadTaildropFileChunk {
            size,
            data: data.to_vec(),
        }
    }

    #[test]
    fn inbox_maps_every_field() {
        let inbox = TaildropInbox::from_proto(pb::TaildropInbox {
            endpoint_tag: "ts".into(),
            files: vec![pb::TaildropFile {
                name: "photo.jpg".into(),
                size: 2048,
                sender_name: "phone".into(),
                modified_at: 1_700_000_000,
            }],
            receiving: vec![
                pb::TaildropReceivingFile {
                    name: "big.iso".into(),
                    size: 10_000,
                    received_bytes: 2_500,
                    sender_id: "n123".into(),
                    sender_name: "laptop".into(),
                },
                pb::TaildropReceivingFile {
                    name: "stream.bin".into(),
                    size: -1,
                    received_bytes: 7,
                    ..Default::default()
                },
            ],
        });
        assert_eq!(
            inbox,
            TaildropInbox {
                endpoint_tag: "ts".into(),
                files: vec![TaildropFile {
                    name: "photo.jpg".into(),
                    size: 2048,
                    sender_name: "phone".into(),
                    modified_at: Some(1_700_000_000),
                }],
                receiving: vec![
                    TaildropReceivingFile {
                        name: "big.iso".into(),
                        size: Some(10_000),
                        received_bytes: 2_500,
                        sender_id: "n123".into(),
                        sender_name: "laptop".into(),
                    },
                    TaildropReceivingFile {
                        name: "stream.bin".into(),
                        size: None,
                        received_bytes: 7,
                        sender_id: String::new(),
                        sender_name: String::new(),
                    },
                ],
            }
        );
    }

    #[test]
    fn empty_placeholder_inbox_maps_to_default() {
        assert_eq!(
            TaildropInbox::from_proto(pb::TaildropInbox::default()),
            TaildropInbox::default()
        );
    }

    #[test]
    fn download_sink_writes_chunks_after_size_header() {
        let mut out = Vec::new();
        let mut sink = DownloadSink::new(&mut out);
        sink.accept(chunk(5, b"")).unwrap();
        assert_eq!(sink.progress(), (0, 5));
        sink.accept(chunk(0, b"he")).unwrap();
        sink.accept(chunk(0, b"llo")).unwrap();
        assert_eq!(sink.progress(), (5, 5));
        assert_eq!(sink.finish(), Ok(5));
        assert_eq!(out, b"hello");
    }

    #[test]
    fn download_sink_accepts_empty_file() {
        let mut out = Vec::new();
        let mut sink = DownloadSink::new(&mut out);
        // Size 0 encodes as an empty message.
        sink.accept(chunk(0, b"")).unwrap();
        assert_eq!(sink.finish(), Ok(0));
        assert!(out.is_empty());
    }

    #[test]
    fn download_sink_rejects_short_long_and_headerless_streams() {
        let mut out = Vec::new();
        let mut sink = DownloadSink::new(&mut out);
        sink.accept(chunk(4, b"")).unwrap();
        sink.accept(chunk(0, b"ab")).unwrap();
        assert_eq!(
            sink.finish(),
            Err(TaildropDownloadError::SizeMismatch {
                expected: 4,
                received: 2
            })
        );

        let mut out = Vec::new();
        let mut sink = DownloadSink::new(&mut out);
        sink.accept(chunk(1, b"")).unwrap();
        assert!(sink.accept(chunk(0, b"ab")).is_err());
        assert_eq!(
            sink.error,
            Some(TaildropDownloadError::SizeMismatch {
                expected: 1,
                received: 2
            })
        );

        let mut out = Vec::new();
        let sink = DownloadSink::new(&mut out);
        assert!(matches!(
            sink.finish(),
            Err(TaildropDownloadError::Api(ApiError::InvalidResponse(_)))
        ));
    }

    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn download_sink_reports_write_failures() {
        let mut writer = FailingWriter;
        let mut sink = DownloadSink::new(&mut writer);
        sink.accept(chunk(3, b"")).unwrap();
        assert!(sink.accept(chunk(0, b"abc")).is_err());
        assert_eq!(
            sink.error,
            Some(TaildropDownloadError::Io("disk full".into()))
        );
    }

    #[test]
    fn partial_path_sits_next_to_target() {
        assert_eq!(
            partial_path(Path::new("/tmp/dl/photo.jpg")),
            PathBuf::from("/tmp/dl/photo.jpg.partial")
        );
    }

    #[test]
    fn certificate_debug_redacts_the_key() {
        let certificate = TailscaleCertificate::from_proto(pb::TailscaleCertificate {
            certificate_pem: b"-----BEGIN CERTIFICATE-----".to_vec(),
            private_key_pem: b"-----BEGIN EC PRIVATE KEY-----SECRET".to_vec(),
        });
        let debug = format!("{:?}", certificate);
        assert!(debug.contains("BEGIN CERTIFICATE"));
        assert!(!debug.contains("SECRET"));
        assert!(!debug.contains("PRIVATE KEY"));
        assert!(debug.contains("36 bytes redacted"));
    }

    #[test]
    fn download_errors_display_for_users() {
        assert_eq!(
            TaildropDownloadError::SizeMismatch {
                expected: 10,
                received: 4
            }
            .to_string(),
            "Download incomplete: received 4 of 10 bytes"
        );
        assert_eq!(
            TaildropDownloadError::Api(ApiError::TimedOut).to_string(),
            "sing-box API timed out"
        );
    }
}
