//! Pure Rust OCI image puller for the microVM sandbox.
//!
//! Downloads images from Docker Hub and OCI-compatible registries,
//! extracts layers via system `gzip` + `tar`, and caches blobs by digest.
//! No buildah/podman/skopeo dependency — only reqwest (already in tree) and
//! system tar/gunzip.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// Pull an OCI image and extract its rootfs to `dest`.
///
/// Layers are cached by digest in `cache_dir/blobs/<algo>/<hex>`.
/// On subsequent pulls, cached layers skip the download.
pub async fn pull(image: &str, dest: &Path, cache_dir: &Path) -> anyhow::Result<()> {
    let ref_ = ImageRef::parse(image)?;
    let token = fetch_token(&ref_).await?;
    let layers = fetch_manifest(&ref_, &token).await?;

    std::fs::create_dir_all(dest)?;

    for digest in &layers {
        extract_or_cache_layer(&ref_, digest, dest, cache_dir, &token).await?;
    }

    Ok(())
}

/// Parsed OCI image reference: `[registry/]repo[:tag]`.
///
/// Docker Hub is the default registry when none is specified.
/// Registry URLs are resolved per the OCI Distribution Spec:
/// - Docker Hub: API at `registry-1.docker.io`, auth at `auth.docker.io`
/// - Other registries: API at the registry hostname, auth via Bearer challenge
struct ImageRef {
    /// Repository path (e.g. "library/alpine", "owner/repo")
    repo: String,
    /// Tag (e.g. "latest", "3.21")
    tag: String,
    /// API host for registry operations (e.g. "registry-1.docker.io")
    registry_api: String,
    /// Auth host for token exchange (Docker Hub: "auth.docker.io"; others: same as API)
    registry_auth: String,
}

impl ImageRef {
    /// Parse an image reference like `alpine:3.21`, `ghcr.io/owner/repo:v1`,
    /// or `docker.io/library/alpine:latest`.
    fn parse(image: &str) -> anyhow::Result<Self> {
        // Strip optional docker.io/ prefix for consistent parsing.
        let image = image.strip_prefix("docker.io/").unwrap_or(image);

        // Split tag: the last ':' that appears after the last '/' (or the
        // only ':' if there's no '/') is the tag delimiter. A ':' before
        // the first '/' is a registry port, not a tag.
        let (rest, tag) = if let Some(last_slash) = image.rfind('/') {
            let after_slash = &image[last_slash..];
            if let Some(tag_pos) = after_slash.rfind(':') {
                let abs_pos = last_slash + tag_pos;
                (&image[..abs_pos], &image[abs_pos + 1..])
            } else {
                (image, "latest")
            }
        } else {
            // No slash — single-component name. ':' is always a tag.
            image.split_once(':').unwrap_or((image, "latest"))
        };

        // If rest contains a '/', the first component might be a registry.
        let (registry, repo) = if let Some((first, remainder)) = rest.split_once('/') {
            if is_registry_host(first) {
                (first, remainder.to_string())
            } else {
                // No registry — default to Docker Hub.
                ("docker.io", rest.to_string())
            }
        } else {
            // Single-component name — Docker Hub official image.
            ("docker.io", format!("library/{rest}"))
        };

        let (registry_api, registry_auth) = registry_endpoints(registry);

        Ok(Self {
            repo,
            tag: tag.to_string(),
            registry_api,
            registry_auth,
        })
    }
}

/// True when `host` looks like a registry hostname (contains `.` or `:`).
fn is_registry_host(host: &str) -> bool {
    host.contains('.') || host.contains(':') || host == "localhost"
}

/// Return (api_host, auth_host) for a registry.
///
/// Docker Hub uses separate API and auth hosts:
/// - API: `registry-1.docker.io`
/// - Auth: `auth.docker.io`
///
/// All other registries use the same hostname for both.
fn registry_endpoints(host: &str) -> (String, String) {
    if host == "docker.io" {
        (
            "registry-1.docker.io".to_string(),
            "auth.docker.io".to_string(),
        )
    } else {
        (host.to_string(), host.to_string())
    }
}

/// Resolve the best platform entry from a manifest list.
/// Prefers the host architecture (amd64 → "amd64") and OS (linux).
fn resolve_platform_manifest(manifests: &[serde_json::Value]) -> anyhow::Result<String> {
    #[cfg(target_arch = "x86_64")]
    let target_arch = "amd64";
    #[cfg(target_arch = "aarch64")]
    let target_arch = "arm64";
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let target_arch = "amd64";

    for entry in manifests {
        let plat = &entry["platform"];
        if plat["os"].as_str() == Some("linux")
            && plat["architecture"].as_str() == Some(target_arch)
        {
            if let Some(d) = entry["digest"].as_str() {
                return Ok(d.to_string());
            }
        }
    }
    // No exact match — return the first entry as a fallback.
    if let Some(entry) = manifests.first()
        && let Some(d) = entry["digest"].as_str()
    {
        return Ok(d.to_string());
    }
    anyhow::bail!("no platform entries in manifest list")
}

/// Fetch a manifest by digest (after resolving from a manifest list).
async fn fetch_manifest_by_digest(
    ref_: &ImageRef,
    digest: &str,
    token: &str,
) -> anyhow::Result<Vec<String>> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        ref_.registry_api, ref_.repo, digest
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(
            "Accept",
            "application/vnd.oci.image.manifest.v1+json, \
             application/vnd.docker.distribution.manifest.v2+json",
        )
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("child manifest request failed: {e}"))?;

    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("reading child manifest: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("child manifest JSON: {e}"))?;

    let mut layers = Vec::new();
    if let Some(arr) = parsed["layers"].as_array() {
        for layer in arr {
            if let Some(d) = layer["digest"].as_str() {
                layers.push(d.to_string());
            }
        }
    }
    if layers.is_empty() {
        anyhow::bail!("no layers in child manifest {digest}");
    }
    Ok(layers)
}

/// Get a bearer token for the given repository.
///
/// Docker Hub uses a custom auth endpoint (`auth.docker.io/token`).
/// Other registries use the standard OCI token flow:
/// GET /v2/ → 401 → parse WWW-Authenticate Bearer realm.
async fn fetch_token(ref_: &ImageRef) -> anyhow::Result<String> {
    if ref_.registry_auth == "auth.docker.io" {
        // Docker Hub's non-standard auth endpoint.
        let url = format!(
            "https://auth.docker.io/token?service=registry.docker.io&scope=repository:{}:pull",
            ref_.repo
        );
        let resp = reqwest::get(&url)
            .await
            .map_err(|e| anyhow::anyhow!("auth request failed: {e}"))?;
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("reading auth response: {e}"))?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("auth JSON: {e}"))?;
        parsed["token"]
            .as_str()
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("no token in auth response: {body}"))
    } else {
        // Standard OCI token flow: GET /v2/ → 401 → WWW-Authenticate.
        let client = reqwest::Client::new();
        let v2_url = format!("https://{}/v2/", ref_.registry_auth);
        let resp = client
            .get(&v2_url)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("auth challenge request failed: {e}"))?;

        let www_auth = resp
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no WWW-Authenticate header from registry {}. \
                     Private registries may require authentication — \
                     set the REGISTRY_TOKEN env var for bearer auth.",
                    ref_.registry_auth
                )
            })?;

        let realm = parse_www_authenticate_param(www_auth, "realm")
            .ok_or_else(|| anyhow::anyhow!("no realm in WWW-Authenticate: {www_auth}"))?;
        let service = parse_www_authenticate_param(www_auth, "service")
            .unwrap_or(ref_.registry_auth.as_str());

        let token_url = format!(
            "{realm}?service={service}&scope=repository:{}:pull",
            ref_.repo
        );

        // If REGISTRY_TOKEN is set, use it as a bearer token.
        let mut req = client.get(&token_url);
        if let Ok(token) = std::env::var("REGISTRY_TOKEN") {
            req = req.bearer_auth(token);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("token request failed: {e}"))?;
        let body = resp
            .text()
            .await
            .map_err(|e| anyhow::anyhow!("reading token response: {e}"))?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("token JSON: {e}"))?;
        parsed["token"]
            .as_str()
            .or_else(|| parsed["access_token"].as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("no token in auth response: {body}"))
    }
}

/// Extract a parameter value from a WWW-Authenticate header.
/// e.g. `Bearer realm="https://ghcr.io/token",service="ghcr.io"`
/// → `parse_www_authenticate_param(header, "realm")` returns `Some("https://ghcr.io/token")`
fn parse_www_authenticate_param<'a>(header: &'a str, param: &str) -> Option<&'a str> {
    let prefix = format!("{param}=\"");
    let start = header.find(&prefix)? + prefix.len();
    let end = header[start..].find('"')?;
    Some(&header[start..start + end])
}

/// Fetch the OCI manifest and return layer digests.
async fn fetch_manifest(ref_: &ImageRef, token: &str) -> anyhow::Result<Vec<String>> {
    let url = format!(
        "https://{}/v2/{}/manifests/{}",
        ref_.registry_api, ref_.repo, ref_.tag
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .header(
            "Accept",
            "application/vnd.oci.image.manifest.v1+json, \
             application/vnd.docker.distribution.manifest.v2+json",
        )
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("manifest request failed: {e}"))?;

    let body = resp
        .text()
        .await
        .map_err(|e| anyhow::anyhow!("reading manifest: {e}"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| anyhow::anyhow!("manifest JSON: {e}"))?;

    // Multi-arch images return a manifest list — resolve to the
    // current platform's child manifest before extracting layers.
    if let Some(manifests) = parsed["manifests"].as_array() {
        let child_digest = resolve_platform_manifest(manifests)?;
        return fetch_manifest_by_digest(ref_, &child_digest, token).await;
    }

    let mut layers = Vec::new();
    if let Some(arr) = parsed["layers"].as_array() {
        for layer in arr {
            if let Some(d) = layer["digest"].as_str() {
                layers.push(d.to_string());
            }
        }
    }

    if layers.is_empty() {
        anyhow::bail!("no layers found in manifest for {}:{}", ref_.repo, ref_.tag);
    }
    Ok(layers)
}

/// Extract a single layer blob, caching the download by digest.
async fn extract_or_cache_layer(
    ref_: &ImageRef,
    digest: &str,
    dest: &Path,
    cache_dir: &Path,
    token: &str,
) -> anyhow::Result<()> {
    let cache_path = blob_cache_path(cache_dir, digest);
    let blob_bytes = if cache_path.exists() {
        let bytes = std::fs::read(&cache_path)?;
        verify_blob_digest(&bytes, digest)?;
        bytes
    } else {
        let bytes = download_blob(ref_, digest, token).await?;
        verify_blob_digest(&bytes, digest)?;
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&cache_path, &bytes)?;
        bytes
    };

    let dest = dest.to_path_buf();
    let digest_owned = digest.to_string();
    tokio::task::spawn_blocking(move || {
        let mut child = std::process::Command::new("tar")
            .args(["-x", "--no-same-owner", "--no-same-permissions", "-C"])
            .arg(&dest)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawning tar: {e}"))?;

        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(&blob_bytes)
            .map_err(|e| anyhow::anyhow!("writing to tar stdin: {e}"))?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .map_err(|e| anyhow::anyhow!("waiting for tar: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("tar extraction failed for {digest_owned}: {stderr}");
        }
        Ok(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn_blocking join: {e}"))?
}

/// Download a blob from the registry.
///
/// Enforces a 2 GiB size cap based on Content-Length to prevent OOM
/// from unbounded responses. Returns the blob bytes.
async fn download_blob(ref_: &ImageRef, digest: &str, token: &str) -> anyhow::Result<Vec<u8>> {
    let url = format!(
        "https://{}/v2/{}/blobs/{}",
        ref_.registry_api, ref_.repo, digest
    );
    let client = reqwest::Client::new();
    let resp = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("blob download failed: {e}"))?;

    // Reject blobs larger than 2 GiB — a compromised or misconfigured
    // registry returning an unbounded response would exhaust host memory.
    const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024 * 1024;
    if let Some(len) = resp.content_length() {
        if len > MAX_BLOB_BYTES {
            anyhow::bail!("blob {digest} Content-Length {len} exceeds {MAX_BLOB_BYTES} byte cap");
        }
    }

    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| anyhow::anyhow!("reading blob: {e}"))
}

/// Map a digest like "sha256:abcdef..." to "cache_dir/blobs/sha256/abcdef...".
fn blob_cache_path(cache_dir: &Path, digest: &str) -> PathBuf {
    if let Some((algo, hex)) = digest.split_once(':') {
        cache_dir.join("blobs").join(algo).join(hex)
    } else {
        cache_dir.join("blobs").join(digest)
    }
}

/// Verify that `bytes` hash to the expected digest (e.g. "sha256:abcdef...").
fn verify_blob_digest(bytes: &[u8], expected_digest: &str) -> anyhow::Result<()> {
    let (algo, expected_hex) = expected_digest
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("invalid digest format: {expected_digest}"))?;
    if algo != "sha256" {
        anyhow::bail!("unsupported digest algorithm: {algo}");
    }

    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let actual_hex = hex::encode(hasher.finalize());

    if actual_hex != expected_hex {
        anyhow::bail!(
            "blob digest mismatch: expected sha256:{expected_hex}, got sha256:{actual_hex}"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_blob_digest_valid() {
        let data = b"hello world";
        // sha256("hello world") = b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9
        let digest = "sha256:b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9";
        assert!(verify_blob_digest(data, digest).is_ok());
    }

    #[test]
    fn verify_blob_digest_mismatch() {
        let data = b"hello world";
        let digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
        let err = verify_blob_digest(data, digest).unwrap_err();
        assert!(err.to_string().contains("blob digest mismatch"));
    }

    #[test]
    fn verify_blob_digest_invalid_format() {
        let err = verify_blob_digest(b"data", "noseparator").unwrap_err();
        assert!(err.to_string().contains("invalid digest format"));
    }

    #[test]
    fn verify_blob_digest_unsupported_algo() {
        let err = verify_blob_digest(b"data", "sha512:abcdef").unwrap_err();
        assert!(err.to_string().contains("unsupported digest algorithm"));
    }

    #[test]
    fn verify_blob_digest_empty_bytes() {
        let data: &[u8] = &[];
        // sha256("") = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        let digest = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert!(verify_blob_digest(data, digest).is_ok());
    }

    // -- ImageRef parsing tests --

    #[test]
    fn image_ref_docker_hub_official() {
        let r = ImageRef::parse("alpine:3.21").unwrap();
        assert_eq!(r.registry_api, "registry-1.docker.io");
        assert_eq!(r.registry_auth, "auth.docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, "3.21");
    }

    #[test]
    fn image_ref_docker_hub_official_default_tag() {
        let r = ImageRef::parse("alpine").unwrap();
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn image_ref_docker_hub_explicit_registry() {
        let r = ImageRef::parse("docker.io/library/alpine:latest").unwrap();
        assert_eq!(r.registry_api, "registry-1.docker.io");
        assert_eq!(r.repo, "library/alpine");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn image_ref_docker_hub_user_image() {
        let r = ImageRef::parse("myuser/myimage:v1.0").unwrap();
        assert_eq!(r.registry_api, "registry-1.docker.io");
        assert_eq!(r.repo, "myuser/myimage");
        assert_eq!(r.tag, "v1.0");
    }

    #[test]
    fn image_ref_docker_hub_user_image_default_tag() {
        let r = ImageRef::parse("myuser/myimage").unwrap();
        assert_eq!(r.repo, "myuser/myimage");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn image_ref_ghcr() {
        let r = ImageRef::parse("ghcr.io/owner/repo:v1").unwrap();
        assert_eq!(r.registry_api, "ghcr.io");
        assert_eq!(r.registry_auth, "ghcr.io");
        assert_eq!(r.repo, "owner/repo");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn image_ref_quay() {
        let r = ImageRef::parse("quay.io/org/image:latest").unwrap();
        assert_eq!(r.registry_api, "quay.io");
        assert_eq!(r.repo, "org/image");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn image_ref_custom_registry_with_port() {
        let r = ImageRef::parse("localhost:5000/team/image:v1").unwrap();
        assert_eq!(r.registry_api, "localhost:5000");
        assert_eq!(r.repo, "team/image");
        assert_eq!(r.tag, "v1");
    }

    #[test]
    fn image_ref_custom_registry_port_no_tag() {
        let r = ImageRef::parse("localhost:5000/team/image").unwrap();
        assert_eq!(r.registry_api, "localhost:5000");
        assert_eq!(r.repo, "team/image");
        assert_eq!(r.tag, "latest");
    }

    // -- parse_www_authenticate_param --

    #[test]
    fn www_auth_realm() {
        let header = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#;
        assert_eq!(
            parse_www_authenticate_param(header, "realm"),
            Some("https://ghcr.io/token")
        );
    }

    #[test]
    fn www_auth_service() {
        let header = r#"Bearer realm="https://ghcr.io/token",service="ghcr.io""#;
        assert_eq!(
            parse_www_authenticate_param(header, "service"),
            Some("ghcr.io")
        );
    }

    #[test]
    fn www_auth_missing_param() {
        let header = r#"Bearer realm="https://ghcr.io/token""#;
        assert_eq!(parse_www_authenticate_param(header, "service"), None);
    }

    // -- resolve_platform_manifest --

    #[test]
    fn resolve_platform_matches_linux_amd64() {
        let manifests = serde_json::json!([
            {"platform": {"os": "windows", "architecture": "amd64"}, "digest": "sha256:win"},
            {"platform": {"os": "linux", "architecture": "amd64"}, "digest": "sha256:linux"},
        ]);
        let arr = manifests.as_array().unwrap();
        let digest = resolve_platform_manifest(arr).unwrap();
        assert_eq!(digest, "sha256:linux");
    }

    #[test]
    fn resolve_platform_falls_back_to_first() {
        // No linux/amd64 entry — fall back to first entry.
        let manifests = serde_json::json!([
            {"platform": {"os": "linux", "architecture": "arm64"}, "digest": "sha256:arm"},
            {"platform": {"os": "windows", "architecture": "amd64"}, "digest": "sha256:win"},
        ]);
        let arr = manifests.as_array().unwrap();
        let digest = resolve_platform_manifest(arr).unwrap();
        assert_eq!(digest, "sha256:arm");
    }

    #[test]
    fn resolve_platform_empty_is_error() {
        let manifests: Vec<serde_json::Value> = vec![];
        let err = resolve_platform_manifest(&manifests).unwrap_err();
        assert!(
            err.to_string().contains("no platform entries"),
            "expected 'no platform entries', got: {err}"
        );
    }

    #[test]
    fn resolve_platform_missing_digest_is_error() {
        let manifests = serde_json::json!([
            {"platform": {"os": "linux", "architecture": "amd64"}, "no_digest": "nope"},
        ]);
        let arr = manifests.as_array().unwrap();
        let err = resolve_platform_manifest(arr).unwrap_err();
        assert!(
            err.to_string().contains("no platform entries"),
            "expected 'no platform entries' when all entries lack digest, got: {err}"
        );
    }

    // -- blob_cache_path --

    #[test]
    fn blob_cache_path_with_algo_and_hex() {
        let cache = std::path::Path::new("/tmp/cache");
        let path = blob_cache_path(cache, "sha256:abcdef123456");
        assert_eq!(
            path,
            std::path::Path::new("/tmp/cache/blobs/sha256/abcdef123456")
        );
    }

    #[test]
    fn blob_cache_path_without_separator() {
        let cache = std::path::Path::new("/tmp/cache");
        let path = blob_cache_path(cache, "noseparator");
        assert_eq!(path, std::path::Path::new("/tmp/cache/blobs/noseparator"));
    }

    // -- is_registry_host --

    #[test]
    fn is_registry_host_true_for_dotted() {
        assert!(is_registry_host("docker.io"));
        assert!(is_registry_host("ghcr.io"));
        assert!(is_registry_host("registry.internal.corp.com"));
    }

    #[test]
    fn is_registry_host_true_for_port() {
        assert!(is_registry_host("localhost:5000"));
        assert!(is_registry_host("10.0.0.1:8080"));
    }

    #[test]
    fn is_registry_host_true_for_bare_localhost() {
        assert!(is_registry_host("localhost"));
    }

    #[test]
    fn is_registry_host_false_for_plain_name() {
        assert!(!is_registry_host("alpine"));
        assert!(!is_registry_host("myuser"));
    }

    // -- registry_endpoints --

    #[test]
    fn registry_endpoints_docker_hub_uses_split_hosts() {
        let (api, auth) = registry_endpoints("docker.io");
        assert_eq!(api, "registry-1.docker.io");
        assert_eq!(auth, "auth.docker.io");
    }

    #[test]
    fn registry_endpoints_other_uses_same_host() {
        let (api, auth) = registry_endpoints("ghcr.io");
        assert_eq!(api, "ghcr.io");
        assert_eq!(auth, "ghcr.io");
    }

    #[test]
    fn registry_endpoints_localhost_uses_same_host() {
        let (api, auth) = registry_endpoints("localhost:5000");
        assert_eq!(api, "localhost:5000");
        assert_eq!(auth, "localhost:5000");
    }
}
