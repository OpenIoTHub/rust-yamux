// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use futures::prelude::*;
use futures::stream::FuturesUnordered;
use quickcheck::{Arbitrary, Gen, QuickCheck};
use std::{
    io,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
};
use test_harness::*;
use tokio::net::{TcpListener, TcpStream};
use tokio::{net::TcpSocket, runtime::Runtime, task};
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};
use yamux::{Config, Connection, ConnectionError, Control, Mode, WindowUpdateMode};

const PAYLOAD_SIZE: usize = 128 * 1024;

#[test]
fn concurrent_streams() {
    let _ = env_logger::try_init();

    fn prop(tcp_buffer_sizes: Option<TcpBufferSizes>) {
        let data = Msg(vec![0x42; PAYLOAD_SIZE]);
        let n_streams = 1000;

        Runtime::new().expect("new runtime").block_on(async move {
            let (server, client) = connected_peers(tcp_buffer_sizes).await.unwrap();

            task::spawn(echo_server(server));

            let (mut ctrl, client) = Control::new(client);
            task::spawn(noop_server(client));

            let result = (0..n_streams)
                .map(|_| {
                    let data = data.clone();
                    let mut ctrl = ctrl.clone();

                    task::spawn(async move {
                        let mut stream = ctrl.open_stream().await?;
                        log::debug!("C: opened new stream {}", stream.id());

                        send_recv_message(&mut stream, data).await?;
                        stream.close().await?;

                        Ok::<(), ConnectionError>(())
                    })
                })
                .collect::<FuturesUnordered<_>>()
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .into_iter()
                .collect::<Result<Vec<_>, ConnectionError>>();

            ctrl.close().await.expect("close connection");

            assert_eq!(result.unwrap().len(), n_streams);
        });
    }

    QuickCheck::new().tests(3).quickcheck(prop as fn(_) -> _)
}

/// Send and receive buffer size for a TCP socket.
#[derive(Clone, Debug, Copy)]
struct TcpBufferSizes {
    send: u32,
    recv: u32,
}

impl Arbitrary for TcpBufferSizes {
    fn arbitrary(g: &mut Gen) -> Self {
        let send = if bool::arbitrary(g) {
            16 * 1024
        } else {
            32 * 1024
        };

        // Have receive buffer size be some multiple of send buffer size.
        let recv = if bool::arbitrary(g) {
            send * 2
        } else {
            send * 4
        };

        TcpBufferSizes { send, recv }
    }
}

async fn connected_peers(
    buffer_sizes: Option<TcpBufferSizes>,
) -> io::Result<(Connection<Compat<TcpStream>>, Connection<Compat<TcpStream>>)> {
    let (listener, addr) = bind(buffer_sizes).await?;

    let server = async {
        let (stream, _) = listener.accept().await?;
        Ok(Connection::new(stream.compat(), config(), Mode::Server))
    };
    let client = async {
        let stream = new_socket(buffer_sizes)?.connect(addr).await?;

        Ok(Connection::new(stream.compat(), config(), Mode::Client))
    };

    futures::future::try_join(server, client).await
}

async fn bind(buffer_sizes: Option<TcpBufferSizes>) -> io::Result<(TcpListener, SocketAddr)> {
    let socket = new_socket(buffer_sizes)?;
    socket.bind(SocketAddr::V4(SocketAddrV4::new(
        Ipv4Addr::new(127, 0, 0, 1),
        0,
    )))?;

    let listener = socket.listen(1024)?;
    let address = listener.local_addr()?;

    Ok((listener, address))
}

fn new_socket(buffer_sizes: Option<TcpBufferSizes>) -> io::Result<TcpSocket> {
    let socket = TcpSocket::new_v4()?;
    if let Some(size) = buffer_sizes {
        socket.set_send_buffer_size(size.send)?;
        socket.set_recv_buffer_size(size.recv)?;
    }

    Ok(socket)
}

fn config() -> Config {
    let mut server_cfg = Config::default();
    // Use a large frame size to speed up the test.
    server_cfg.set_split_send_size(PAYLOAD_SIZE);
    // Use `WindowUpdateMode::OnRead` so window updates are sent by the
    // `Stream`s and subject to backpressure from the stream command channel. Thus
    // the `Connection` I/O loop will not need to send window updates
    // directly as a result of reading a frame, which can otherwise
    // lead to mutual write deadlocks if the socket send buffers are too small.
    // With `OnRead` the socket send buffer can even be smaller than the size
    // of a single frame for this test.
    server_cfg.set_window_update_mode(WindowUpdateMode::OnRead);
    server_cfg
}
