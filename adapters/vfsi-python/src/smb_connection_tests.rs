use super::*;
use std::net::TcpListener;

#[test]
fn smb_constructor_honors_explicit_setup_deadlines() {
    Python::initialize();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    listener.set_nonblocking(true).unwrap();
    let peer = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let (mut socket, _) = loop {
            match listener.accept() {
                Ok(peer) => break peer,
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("SMB constructor did not connect: {error}"),
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        // Wait for negotiation so scheduling the peer cannot consume the
        // deadline. Keep the connection silent until well past both budgets.
        std::io::Read::read(&mut socket, &mut [0; 4096]).unwrap();
        std::thread::sleep(Duration::from_millis(250));
    });
    let error = Python::attach(|py| {
        NfsClient::new(
            py,
            &address,
            "smb",
            None,
            None,
            None,
            Some("share"),
            "",
            "",
            "",
            0.01,
            0.01,
            1024,
            100,
            1024,
            10,
            None,
            None,
            None,
            false,
        )
        .err()
        .expect("a stalled SMB handshake must fail")
    });
    peer.join().unwrap();
    Python::attach(|py| {
        assert!(
            error.is_instance_of::<pyo3::exceptions::PyTimeoutError>(py),
            "configured deadlines must time out before the peer disconnects: {error}"
        );
    });
}
