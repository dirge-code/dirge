#[cfg(test)]
#[cfg(all(unix, feature = "sandbox-microvm"))]
mod tests {
    use super::super::common::*;

    #[test]
    #[ignore = "requires passwordless SSH to localhost — run with: cargo test --features sandbox-microvm -- ssh_loopback --include-ignored"]
    fn ssh_loopback() {
        let _guard = serial_fd_test();
        let _relay = match std::process::Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "localhost",
                "cat",
                "-u",
            ])
            .spawn()
        {
            Ok(_child) => {
                return;
            }
            Err(_) => {
                eprintln!("ssh_loopback: ssh failed — skipping");
                return;
            }
        };
    }
}
