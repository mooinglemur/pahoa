//! Terminating TLS on the room port, with the certificate reloaded in place.
//!
//! `rustls` over the `ring` provider. Neither links against the host, which is
//! what keeps the static musl build and the `scratch` image intact —
//! `native-tls` would not, and `aws-lc-rs` would want cmake and a much larger C
//! surface.
//!
//! **Reloading rather than restarting** is the point of most of this file.
//! cert-manager renews roughly every 60 days and the kubelet updates the
//! mounted Secret within about a minute; reading the chain once at startup
//! would mean bouncing every running room on a renewal cycle, which across
//! hundreds of rooms needs rate-limiting to avoid a thundering restart. So the
//! [`ServerConfig`] is built once and never replaced, and the certificate hangs
//! off a [`ResolvesServerCert`] that hands out whatever is current at handshake
//! time. Connections already established keep the session they negotiated;
//! only new handshakes see the new chain.
//!
//! **Polling, not inotify.** The kubelet does not rewrite a mounted Secret in
//! place — it stages the new values in a fresh directory and swaps a `..data`
//! symlink, so a watch on the resolved path sees nothing and a watch on the
//! directory needs to understand the swap. A `stat` every 30 seconds is
//! indifferent to how the update was performed, and a renewal that takes an
//! extra half minute to be picked up costs nothing.

use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Where the chain and its key are mounted. Not secret, so these stay argv.
#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// How often the mounted files are checked for a renewal.
///
/// Well under the ~1 minute the kubelet takes to propagate a Secret update, and
/// far above the cost of two `stat` calls.
pub const RELOAD_INTERVAL: Duration = Duration::from_secs(30);

/// Enough of a file's identity to notice it was replaced.
///
/// Inode included deliberately: a symlink swap can land a different file with
/// an identical size and, on a coarse clock, an identical mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    mtime: i64,
    mtime_nsec: i64,
    len: u64,
    ino: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    cert: FileStamp,
    key: FileStamp,
}

#[derive(Debug)]
struct Current {
    key: Arc<CertifiedKey>,
    stamp: Stamp,
}

/// The live certificate, swapped under a lock as renewals land.
#[derive(Debug)]
pub struct CertResolver {
    paths: TlsPaths,
    current: RwLock<Current>,
}

impl CertResolver {
    /// Read the pair once, failing if it cannot be used.
    ///
    /// Called before the listener binds, so a room configured with an unusable
    /// certificate refuses to start rather than serving and failing every
    /// handshake.
    pub fn load(paths: TlsPaths) -> io::Result<Arc<Self>> {
        let stamp = stamp(&paths)?;
        let key = certified_key(&paths)?;
        Ok(Arc::new(Self {
            paths,
            current: RwLock::new(Current { key, stamp }),
        }))
    }

    /// Re-read the pair if either file changed since the last look.
    ///
    /// Deliberately cannot fail the room. A Secret caught mid-update, or a
    /// chain that does not match its key, leaves the previous certificate
    /// serving and says so — the alternative is a room that stops answering
    /// because of a file it was already holding a working copy of. The stamp is
    /// only advanced on success, so a bad pair is retried on the next tick
    /// rather than latched.
    fn reload_if_changed(&self) {
        let Ok(stamp) = stamp(&self.paths) else {
            // A `stat` that fails is a mount that went away or is mid-swap.
            // Next tick.
            return;
        };
        if self.current.read().unwrap().stamp == stamp {
            return;
        }

        match certified_key(&self.paths) {
            Ok(key) => {
                let mut current = self.current.write().unwrap();
                current.key = key;
                current.stamp = stamp;
                tracing::info!(
                    cert = %self.paths.cert.display(),
                    "loaded a renewed TLS certificate"
                );
            }
            Err(e) => tracing::warn!(
                error = %e,
                cert = %self.paths.cert.display(),
                "the TLS certificate changed but will not load; keeping the previous one"
            ),
        }
    }
}

impl ResolvesServerCert for CertResolver {
    /// SNI is ignored on purpose: every room shares one hostname and differs
    /// only by port, so there is exactly one chain to offer.
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(&self.current.read().unwrap().key))
    }
}

/// Build the acceptor. One `ServerConfig` for the process, shared by every
/// handshake; the certificate inside it moves, the config does not.
pub fn acceptor(resolver: Arc<CertResolver>) -> tokio_rustls::TlsAcceptor {
    // Named rather than taken from the process default, so this cannot depend
    // on whether something else installed a provider first.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports rustls's default protocol versions")
        .with_no_client_auth()
        .with_cert_resolver(resolver);

    // HTTP/1.1 is the only thing spoken on this port — the WebSocket upgrade
    // and the admin API both ride on it. Advertising `h2` would promise a
    // protocol pahoa does not implement, and a browser would take it.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

/// Watch the mounted files for the rest of the process's life.
pub fn spawn_reloader(resolver: Arc<CertResolver>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick of an `Interval` completes immediately, and the pair
        // was just read.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let resolver = Arc::clone(&resolver);
            // A Secret mount is a tmpfs and these calls are instant, but a
            // certificate on anything slower must not be able to stall a
            // runtime worker.
            let _ = tokio::task::spawn_blocking(move || resolver.reload_if_changed()).await;
        }
    });
}

fn stamp(paths: &TlsPaths) -> io::Result<Stamp> {
    Ok(Stamp {
        cert: stamp_file(&paths.cert)?,
        key: stamp_file(&paths.key)?,
    })
}

fn stamp_file(path: &Path) -> io::Result<FileStamp> {
    let meta = std::fs::metadata(path)?;
    Ok(FileStamp {
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
        len: meta.size(),
        ino: meta.ino(),
    })
}

fn certified_key(paths: &TlsPaths) -> io::Result<Arc<CertifiedKey>> {
    let chain = load_chain(&paths.cert)?;
    let key = load_key(&paths.key)?;
    // `from_der` rather than assembling the pair by hand, because it also checks
    // the key against the leaf certificate's public key. That is precisely the
    // mismatch a Secret caught mid-renewal produces — a new chain next to the
    // old key — and catching it here is what keeps the previous certificate
    // serving instead of swapping in one that fails every handshake.
    let certified =
        CertifiedKey::from_der(chain, key, &rustls::crypto::ring::default_provider())
            .map_err(|e| invalid(&paths.cert, format!("unusable certificate and key: {e}")))?;
    Ok(Arc::new(certified))
}

fn load_chain(path: &Path) -> io::Result<Vec<CertificateDer<'static>>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    let chain: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut reader).collect::<Result<_, _>>()?;
    if chain.is_empty() {
        return Err(invalid(path, "no PEM certificate in this file".to_string()));
    }
    Ok(chain)
}

fn load_key(path: &Path) -> io::Result<PrivateKeyDer<'static>> {
    let mut reader = io::BufReader::new(std::fs::File::open(path)?);
    rustls_pemfile::private_key(&mut reader)?
        .ok_or_else(|| invalid(path, "no PEM private key in this file".to_string()))
}

fn invalid(path: &Path, detail: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{}: {detail}", path.display()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory standing in for the mounted Secret.
    struct Mount(PathBuf);

    impl Mount {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("pahoa-tls-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("scratch directory");
            Self(dir)
        }

        fn paths(&self) -> TlsPaths {
            TlsPaths {
                cert: self.0.join("tls.crt"),
                key: self.0.join("tls.key"),
            }
        }

        /// Write a fresh self-signed pair for `name`, returning its leaf DER so
        /// a test can tell one generation from the next.
        fn write(&self, name: &str) -> Vec<u8> {
            let issued = rcgen::generate_simple_self_signed(vec![name.to_string()])
                .expect("a self-signed certificate");
            let paths = self.paths();
            std::fs::write(&paths.cert, issued.cert.pem()).unwrap();
            std::fs::write(&paths.key, issued.signing_key.serialize_pem()).unwrap();
            issued.cert.der().to_vec()
        }
    }

    impl Drop for Mount {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn leaf(resolver: &CertResolver) -> Vec<u8> {
        resolver.current.read().unwrap().key.cert[0].to_vec()
    }

    #[test]
    fn a_renewal_is_picked_up_without_rebuilding_anything() {
        let mount = Mount::new("renewal");
        let first = mount.write("first.example");
        let resolver = CertResolver::load(mount.paths()).expect("loads");
        assert_eq!(leaf(&resolver), first);

        let second = mount.write("second.example");
        assert_ne!(first, second, "the fixture must actually differ");
        resolver.reload_if_changed();
        assert_eq!(leaf(&resolver), second, "the renewal should be serving");
    }

    /// The failure that must not take a room down: cert-manager writes the new
    /// chain, and for an instant the old key is still next to it.
    #[test]
    fn a_chain_that_does_not_match_its_key_leaves_the_previous_one_serving() {
        let mount = Mount::new("mismatch");
        let good = mount.write("good.example");
        let resolver = CertResolver::load(mount.paths()).expect("loads");

        let orphan =
            rcgen::generate_simple_self_signed(vec!["orphan.example".to_string()]).unwrap();
        std::fs::write(&mount.paths().cert, orphan.cert.pem()).unwrap();

        resolver.reload_if_changed();
        assert_eq!(
            leaf(&resolver),
            good,
            "a mismatched pair must not be adopted"
        );
    }

    #[test]
    fn a_half_written_certificate_leaves_the_previous_one_serving() {
        let mount = Mount::new("truncated");
        let good = mount.write("good.example");
        let resolver = CertResolver::load(mount.paths()).expect("loads");

        std::fs::write(&mount.paths().cert, b"-----BEGIN CERTIFICATE-----\n").unwrap();
        resolver.reload_if_changed();
        assert_eq!(leaf(&resolver), good);
    }

    /// A bad pair must not latch: the stamp only advances on success, so the
    /// next tick tries again and a repaired file recovers on its own.
    #[test]
    fn a_repaired_certificate_is_taken_on_the_following_tick() {
        let mount = Mount::new("retry");
        mount.write("good.example");
        let resolver = CertResolver::load(mount.paths()).expect("loads");

        std::fs::write(&mount.paths().cert, b"rubbish").unwrap();
        resolver.reload_if_changed();

        let repaired = mount.write("repaired.example");
        resolver.reload_if_changed();
        assert_eq!(leaf(&resolver), repaired);
    }

    #[test]
    fn an_unreadable_pair_refuses_to_start_rather_than_serving() {
        let mount = Mount::new("absent");
        assert!(CertResolver::load(mount.paths()).is_err());
    }

    #[test]
    fn an_unchanged_pair_is_not_re_read() {
        let mount = Mount::new("stable");
        let only = mount.write("only.example");
        let resolver = CertResolver::load(mount.paths()).expect("loads");
        let before = Arc::as_ptr(&resolver.current.read().unwrap().key);
        resolver.reload_if_changed();
        let after = Arc::as_ptr(&resolver.current.read().unwrap().key);
        assert!(std::ptr::eq(before, after), "should not have reloaded");
        assert_eq!(leaf(&resolver), only);
    }
}
