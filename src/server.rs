use std::{
    future::Future,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime},
};

use log::{debug, error, info, trace};
use rand::Rng;
use std::io::Result as IoResult;
use tokio::{net::UdpSocket, sync::oneshot};

use crate::Device;

const SSDP_ADDR: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const DEFAULT_SERVER_NAME: &str = "Tokio-SSDP/1.0 UPnP/1.0";

/// A server providing SSDP functionalities.
/// The server will respond to `M-SEARCH` requests, send `alive` and `byebye` messages when needed.
#[derive(Debug, Clone)]
pub struct Server {
    server_name: Option<String>,
    max_age: u64,
    devices: Vec<Device>,
    headers: Vec<(String, String)>,
    partial_req_workaround: bool,
}

impl Server {
    /// Create a new SSDP server
    ///
    /// # Examples
    /// ```
    /// use tokio_ssdp::{Server, Device};
    ///
    /// let uuid = "ad8782a0-9e28-422b-a6ae-670fe7c4c043";
    ///
    /// Server::new([
    ///     Device::new(uuid, "upnp:rootdevice", "http://192.168.1.100:8080/desc.xml"),
    ///     Device::new(uuid, "", "http://192.168.1.100:8080/desc.xml"),
    /// ]);
    /// ```
    pub fn new(devices: impl IntoIterator<Item = Device>) -> Self {
        Self {
            server_name: None,
            max_age: 100,
            devices: devices.into_iter().collect(),
            headers: vec![],
            partial_req_workaround: false,
        }
    }

    /// Set the name of the `Server` response header, defaults to `Tokio-SSDP/1.0 UPnP/1.0`.
    /// # Examples
    /// ```
    /// use tokio_ssdp::Server;
    ///
    /// Server::new([])
    ///   .server_name("SomeRandomDevice/1.0 UPnP/1.0");
    /// ```
    pub fn server_name(mut self, server_name: impl Into<String>) -> Self {
        self.server_name = Some(server_name.into());
        self
    }

    /// Set the value of `Cache-Control: max-age=`, which is the valid time for the message, defaults to 100.
    pub fn max_age(mut self, max_age: u64) -> Self {
        self.max_age = max_age;
        self
    }

    /// Some broken clients only end the request in `\r\n`, not `\r\n\r\n`.
    /// This causes [`httparse`] to return [`httparse::Status::Partial`]
    /// instead of [`httparse::Status::Complete`].
    ///
    /// To work with these clients, enable `partial_req_workaround`.
    pub fn partial_request_workaround(mut self, partial_req_workaround: bool) -> Self {
        self.partial_req_workaround = partial_req_workaround;
        self
    }

    /// Add an extra header to search responses
    /// # Examples
    /// ```
    /// use tokio_ssdp::Server;
    ///
    /// Server::new([])
    ///   .extra_header("CONFIGID.UPNP.ORG", "1");
    /// ```
    pub fn extra_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Start serving on all interfaces, see `serve_addr` for details.
    pub fn serve(self) -> IoResult<impl Future<Output = IoResult<()>>> {
        self.serve_addr(Ipv4Addr::new(0, 0, 0, 0))
    }

    /// Start serving on `ip`, returns a future that needs to be `await`ed to keep the server running.
    /// # Examples
    /// ```no_run
    /// use tokio_ssdp::Server;
    /// use std::net::Ipv4Addr;
    ///
    /// Server::new([])
    ///   .serve_addr(Ipv4Addr::new(192, 168, 1, 100));
    /// ```
    pub fn serve_addr(self, ip: Ipv4Addr) -> IoResult<impl Future<Output = IoResult<()>>> {
        let this = Arc::new(self);
        let s = {
            use socket2::{Domain, Protocol, Socket, Type};
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.set_reuse_address(true)?;
            s.set_nonblocking(true)?;
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, SSDP_PORT)).into())?;
            s.join_multicast_v4(&SSDP_ADDR, &ip)?;
            s.set_multicast_loop_v4(true)?;
            s
        };
        let socket = Arc::new(UdpSocket::from_std(s.into())?);

        info!("Listening on {}", socket.local_addr()?);

        // Pre-concat headers
        let extra_headers = Arc::new(
            this.headers
                .iter()
                .map(|(name, value)| format!("{}: {}\r\n", name, value))
                .collect::<Vec<_>>()
                .join(""),
        );

        let server_fut = async move {
            let mut buf = [0u8; 2048];

            let (_notify_alive_tx, mut notify_alive_rx) = oneshot::channel::<()>();
            tokio::spawn({
                let this = Arc::clone(&this);
                let socket = Arc::clone(&socket);
                let extra_headers = Arc::clone(&extra_headers);

                async move {
                    loop {
                        if let Err(e) = this.broadcast_alive(&socket, &extra_headers).await {
                            error!("Send alive messages failed: {}", e);
                        }

                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(this.max_age)) => {
                                // It's time to send alive messages
                            }
                            _ = Pin::new(&mut notify_alive_rx) => {
                                // We should shut down
                                debug!("notify_alive shutdown");
                                return IoResult::Ok(());
                            }
                        }
                    }
                }
            });

            let (_notify_byebye_tx, notify_byebye_rx) = oneshot::channel::<()>();
            tokio::spawn({
                let this = Arc::clone(&this);
                let socket = Arc::clone(&socket);
                let extra_headers = Arc::clone(&extra_headers);

                async move {
                    let _ = notify_byebye_rx.await;

                    if let Err(e) = this.broadcast_byebye(&socket, &extra_headers).await {
                        error!("Send byebye messages failed: {}", e);
                    }
                }
            });

            loop {
                let (mut n, addr) = socket.recv_from(&mut buf).await?;

                // Some broken clients only end the request in `\r\n`, not `\r\n\r\n`.
                // If workaround is enabled, fixup these requests so we can parse them.
                if this.partial_req_workaround {
                    let slice = &buf[..n];
                    if slice.ends_with(b"\r\n")
                        && !slice.ends_with(b"\r\n\r\n")
                        && (n < buf.len() - 2)
                    {
                        buf[n] = b'\r';
                        buf[n + 1] = b'\n';
                        n += 2;
                    }
                }

                let mut headers = [httparse::EMPTY_HEADER; 16];
                let mut req = httparse::Request::new(&mut headers);

                if let Ok(httparse::Status::Complete(_)) = req.parse(&buf[..n]) {
                    let method = if let Some(m) = req.method {
                        m
                    } else {
                        continue;
                    };

                    let path = if let Some(m) = req.path {
                        m
                    } else {
                        continue;
                    };

                    match (method, path) {
                        ("M-SEARCH", "*") => {
                            let socket = Arc::clone(&socket);
                            let res = this.handle_search(&req, socket, addr, &extra_headers).await;
                            if let Err(e) = res {
                                error!("Handle search failed: {}", e);
                            }
                        }
                        ("NOTIFY", "*") => trace!("NOTIFY * from {}", addr),
                        _ => debug!("Unknown SSDP request {} {} from {}", method, path, addr),
                    }
                }
            }
        };

        Ok(server_fut)
    }

    async fn handle_search(
        &self,
        req: &httparse::Request<'_, '_>,
        socket: Arc<UdpSocket>,
        remote_addr: SocketAddr,
        extra_headers: &str,
    ) -> std::io::Result<()> {
        let mut st = None;
        let mut mx = 0u32;
        let mut man_found = false;

        for header in req.headers.iter() {
            let v = Some(String::from_utf8_lossy(header.value));

            if header.name.eq_ignore_ascii_case("st") {
                st = v;
                continue;
            }

            if header.name.eq_ignore_ascii_case("mx") {
                let val = String::from_utf8_lossy(header.value);

                mx = match val.parse() {
                    Ok(v) => v,
                    Err(e) => return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                };

                continue;
            }

            if header.name.eq_ignore_ascii_case("man") {
                if header.value != b"\"ssdp:discover\"" {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "MAN != \"ssdp:discover\" ({})",
                            String::from_utf8_lossy(header.value)
                        ),
                    ));
                }
                man_found = true;
            }
        }

        if !man_found {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MAN header not found",
            ));
        }

        let st = if let Some(st) = st {
            st
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ST header not found",
            ));
        };

        debug!("ST={:?}, MX={:?}", st, mx);

        let device = if let Some(s) = self
            .devices
            .iter()
            .find(|d| d.search_target.eq_ignore_ascii_case(&st))
        {
            s
        } else {
            return Ok(());
        };

        debug!("Matched {:?}", device);

        let response = format!(
            concat!(
                "HTTP/1.1 200 OK\r\n",
                "CACHE-CONTROL: max-age={max_age}\r\n",
                "DATE: {date}\r\n",
                "EXT:\r\n",
                "LOCATION: {loc}\r\n",
                "SERVER: {server}\r\n",
                "ST: {st}\r\n",
                "USN: {usn}\r\n",
                "{headers}",
                "\r\n"
            ),
            max_age = self.max_age,
            date = httpdate::fmt_http_date(SystemTime::now()),
            loc = device.location,
            server = self.server_name.as_deref().unwrap_or(DEFAULT_SERVER_NAME),
            st = device.search_target,
            usn = device.usn,
            headers = extra_headers
        );

        trace!("Response: {}", response);

        tokio::spawn(async move {
            if mx > 0 {
                // upnp specification advises to use a number less than 5 if it is bigger than 5
                //
                // we cap the waiting time at 3, for user convenience
                mx = mx.min(3);
                // wait a random time up to mx
                let wait = {
                    let mut rng = rand::thread_rng();
                    rng.gen_range(0..mx)
                };
                tokio::time::sleep(Duration::from_secs(wait as u64)).await;
            }
            if let Err(e) = socket.send_to(response.as_bytes(), remote_addr).await {
                error!("Failed to send search response: {}", e);
            }
        });

        Ok(())
    }

    /// Broadcast `ssdp:alive`
    async fn broadcast_alive(&self, socket: &UdpSocket, extra_headers: &str) -> IoResult<()> {
        debug!("Sending alive messages");

        for device in self.devices.iter() {
            let message = format!(
                concat!(
                    "NOTIFY * HTTP/1.1\r\n",
                    "HOST: {ssdp_addr}:{ssdp_port}\r\n",
                    "CACHE-CONTROL: max-age={max_age}\r\n",
                    "LOCATION: {loc}\r\n",
                    "NT: {st}\r\n",
                    "NTS: ssdp:alive\r\n",
                    "SERVER: {server}\r\n",
                    "USN: {usn}\r\n",
                    "{headers}",
                    "\r\n"
                ),
                max_age = self.max_age,
                ssdp_addr = SSDP_ADDR,
                ssdp_port = SSDP_PORT,
                loc = device.location,
                server = self.server_name.as_deref().unwrap_or(DEFAULT_SERVER_NAME),
                st = device.search_target,
                usn = device.usn,
                headers = extra_headers
            );

            trace!("Alive message: {}", message);

            socket
                .send_to(message.as_bytes(), (SSDP_ADDR, SSDP_PORT))
                .await?;

            // Avoid congestion
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Ok(())
    }

    /// Broadcast `ssdp:byebye`
    async fn broadcast_byebye(&self, socket: &UdpSocket, extra_headers: &str) -> IoResult<()> {
        debug!("Sending byebye messages");

        for device in self.devices.iter() {
            let message = format!(
                concat!(
                    "NOTIFY * HTTP/1.1\r\n",
                    "HOST: {ssdp_addr}:{ssdp_port}\r\n",
                    "NT: {st}\r\n",
                    "NTS: ssdp:alive\r\n",
                    "USN: {usn}\r\n",
                    "{headers}",
                    "\r\n"
                ),
                ssdp_addr = SSDP_ADDR,
                ssdp_port = SSDP_PORT,
                st = device.search_target,
                usn = device.usn,
                headers = extra_headers
            );

            trace!("Byebye message: {}", message);

            socket
                .send_to(message.as_bytes(), (SSDP_ADDR, SSDP_PORT))
                .await?;

            // Avoid congestion
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        Ok(())
    }
}
