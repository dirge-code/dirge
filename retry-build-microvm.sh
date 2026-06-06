#!/bin/bash
set -euo pipefail

ROOTFS_CACHE=~/.cache/dirge/microvm

echo "=== Cleaning cached rootfs ==="
rm -rf "$ROOTFS_CACHE"

echo "=== Killing zombie runner processes ==="
pkill -f "dirge-microvm-runner" 2>/dev/null || true

echo "=== Cleaning stale test temps ==="
rm -rf /tmp/dirge-test-microvm* 2>/dev/null || true

echo "=== Cleaning snapshots ==="
rm -rf ~/.cache/dirge/microvm/snapshots 2>/dev/null || true

echo "=== Removing stale buildah images ==="
buildah rmi --storage-driver vfs dirge-microvm:debian 2>/dev/null || true
buildah rmi --storage-driver vfs dirge-microvm:alpine 2>/dev/null || true
buildah rmi --storage-driver vfs dirge-microvm:dev 2>/dev/null || true

echo "=== Forcing rebuild (removing old binaries) ==="
rm -f target/debug/dirge target/debug/dirge-microvm-runner 2>/dev/null || true

echo "=== Building debug binary ==="
cargo build --features sandbox-microvm

echo "=== Setting up sandbox (pulls/builds guest images) ==="
./target/debug/dirge sandbox setup

echo "=== Building additional guest images ==="
buildah bud --storage-driver vfs --tag dirge-microvm:alpine -f images/alpine/Dockerfile .
buildah bud --storage-driver vfs --tag dirge-microvm:dev -f images/dev/Dockerfile .

echo "=== Running unit tests (no KVM required) ==="
cargo test --bin dirge --features sandbox-microvm -- \
    sandbox::microvm::tests::tests::microvm_config_defaults \
    sandbox::microvm::tests::tests::microvm_sandbox_new_does_not_start \
    sandbox::microvm::tests::tests::exec_fails_if_not_started \
    sandbox::microvm::tests::tests::ssh_keys_generate_and_cleanup \
    sandbox::microvm::tests::tests::ssh_wait_for_timeout \
    sandbox::microvm::tests::tests::host_keys_generate_and_inject \
    sandbox::microvm::tests::tests::krun_config_has_required_mounts \
    sandbox::microvm::tests::tests::oci_pull_nonexistent_image_is_error \
    sandbox::microvm::rootfs::tests::canonicalize_bare_name \
    sandbox::microvm::rootfs::tests::canonicalize_passthrough \
    sandbox::microvm::rootfs::tests::local_variant_extraction \
    sandbox::microvm::rootfs::tests::build_guest_image_invalid_name \
    sandbox::microvm::rootfs::tests::copy_file_reflink_copies_content \
    sandbox::microvm::rootfs::tests::copy_file_reflink_empty_file \
    sandbox::microvm::rootfs::tests::copy_file_reflink_nonexistent_src_is_error \
    sandbox::microvm::rootfs::tests::cp_r_copies_dir_tree \
    sandbox::microvm::rootfs::tests::cp_r_copies_symlinks \
    sandbox::microvm::rootfs::tests::prepare_local_nonexistent_image_is_error \
    sandbox::microvm::rootfs::tests::prepare_docker_nonexistent_image_is_error \
    sandbox::microvm::oci::tests::image_ref_docker_hub_official \
    sandbox::microvm::oci::tests::image_ref_docker_hub_official_default_tag \
    sandbox::microvm::oci::tests::image_ref_docker_hub_user_image \
    sandbox::microvm::oci::tests::image_ref_docker_hub_user_image_default_tag \
    sandbox::microvm::oci::tests::image_ref_docker_hub_explicit_registry \
    sandbox::microvm::oci::tests::image_ref_ghcr \
    sandbox::microvm::oci::tests::image_ref_quay \
    sandbox::microvm::oci::tests::image_ref_custom_registry_with_port \
    sandbox::microvm::oci::tests::image_ref_custom_registry_port_no_tag \
    sandbox::microvm::oci::tests::verify_blob_digest_valid \
    sandbox::microvm::oci::tests::verify_blob_digest_mismatch \
    sandbox::microvm::oci::tests::verify_blob_digest_invalid_format \
    sandbox::microvm::oci::tests::verify_blob_digest_unsupported_algo \
    sandbox::microvm::oci::tests::verify_blob_digest_empty_bytes \
    sandbox::microvm::oci::tests::www_auth_realm \
    sandbox::microvm::oci::tests::www_auth_service \
    sandbox::microvm::oci::tests::www_auth_missing_param

echo "=== Running lifecycle tests (needs /dev/kvm + libkrun) ==="
cargo test --bin dirge --features sandbox-microvm -- \
    sandbox::microvm::tests::tests::full_microvm_lifecycle \
    sandbox::microvm::tests::tests::full_microvm_lifecycle_alpine

echo "=== Running keyboard load tests (needs /dev/kvm + libkrun) ==="
cargo test --bin dirge --features sandbox-microvm -- \
    sandbox::microvm::tests::tests::keyboard_load_test \
    sandbox::microvm::tests::tests::keyboard_stress_test \
    --nocapture

echo "=== Done. Run: ./target/debug/dirge --resume ==="
echo ""
echo "To use the dev image (includes Rust toolchain):"
echo "  ./target/debug/dirge --sandbox microvm --microvm-image dev"
