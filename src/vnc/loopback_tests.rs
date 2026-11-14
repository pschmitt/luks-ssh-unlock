use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

use super::{Backend, Input, Key, Options, Update, Vnc};

#[tokio::test]
async fn loopback_server_sends_pixels_and_receives_keyboard() {
    exercise_loopback(false).await;
}

#[tokio::test]
async fn preconnected_stream_sends_pixels_and_receives_keyboard() {
    exercise_loopback(true).await;
}

#[tokio::test]
async fn in_memory_transport_rejects_a_refused_handshake() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (stream, mut server) = tokio::io::duplex(1024);
    let server = tokio::spawn(async move {
        server.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        server.read_exact(&mut version).await.unwrap();
        assert_eq!(&version, b"RFB 003.008\n");
        server.write_all(&[0]).await.unwrap();
    });
    let client = Vnc::new(String::new(), None, Options::default());
    assert!(matches!(
        client.connect_stream(stream).await,
        Err(super::ConnectError::Failed(_))
    ));
    server.await.unwrap();
}

async fn exercise_loopback(preconnected: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream.write_all(b"RFB 003.008\n").unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).unwrap();
        assert_eq!(&version, b"RFB 003.008\n");
        stream.write_all(&[1, 1]).unwrap();
        let mut choice = [0; 1];
        stream.read_exact(&mut choice).unwrap();
        assert_eq!(choice, [1]);
        stream.write_all(&0u32.to_be_bytes()).unwrap();
        stream.read_exact(&mut choice).unwrap();
        let mut init = vec![0, 1, 0, 1];
        init.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        init.extend_from_slice(&0u32.to_be_bytes());
        stream.write_all(&init).unwrap();
        let mut pixel_format = [0; 20];
        stream.read_exact(&mut pixel_format).unwrap();
        assert_eq!(pixel_format[0], 0);
        let mut encodings = [0; 4];
        stream.read_exact(&mut encodings).unwrap();
        assert_eq!(encodings[0], 2);
        let count = u16::from_be_bytes([encodings[2], encodings[3]]);
        let mut ids = vec![0; usize::from(count) * 4];
        stream.read_exact(&mut ids).unwrap();
        let mut request = [0; 10];
        stream.read_exact(&mut request).unwrap();
        assert_eq!(request[0], 3);
        stream
            .write_all(&[0, 0, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 3, 2, 1, 0])
            .unwrap();
        let mut key = [0; 8];
        stream.read_exact(&mut key).unwrap();
        assert_eq!(key, [4, 1, 0, 0, 0, 0, 0, b'a']);
    });
    let client = Vnc::new(address.to_string(), None, Options::default());
    let backend = if preconnected {
        let stream = tokio::net::TcpStream::connect(address).await.unwrap();
        client.connect_stream(stream).await.unwrap()
    } else {
        client.connect().await.unwrap()
    };
    let pixels = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Update::Bgra(rect, data) = backend.recv().await.unwrap() {
                break (rect, data);
            }
        }
    })
    .await
    .unwrap();
    assert_eq!((pixels.0.width, pixels.0.height), (1, 1));
    assert_eq!(pixels.1, [3, 2, 1, 0]);
    backend
        .send(Input::Key(Key {
            keysym: u32::from(b'a'),
            down: true,
            scancode: None,
        }))
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || server.join().unwrap())
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Update::Error(message) = backend.recv().await.unwrap() {
                break message;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(error, "VNC connection ended");
    backend.close().await;
}
