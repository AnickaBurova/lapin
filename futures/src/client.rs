use amq_protocol::uri::AMQPUri;
use lapin_async;
use std::default::Default;
use std::io;
use std::str::FromStr;
use futures_channel::oneshot;
use futures_util::future;
use tokio_io::{AsyncRead,AsyncWrite};
use tokio_timer::Interval;
use std::future::Future;
use std::mem::PinMut;
use std::sync::{Arc,Mutex};
use std::task::{self,Poll};
use std::time::{Duration,Instant};

use transport::*;
use channel::{Channel, ConfirmSelectOptions};

/// the Client structures connects to a server and creates channels
//#[derive(Clone)]
pub struct Client<T> {
    transport:         Arc<Mutex<AMQPTransport<T>>>,
    pub configuration: ConnectionConfiguration,
}

impl<T> Clone for Client<T>
    where T: Send {
  fn clone(&self) -> Client<T> {
    Client {
      transport:     self.transport.clone(),
      configuration: self.configuration.clone(),
    }
  }
}
#[derive(Clone,Debug,PartialEq)]
pub struct ConnectionOptions {
  pub username:  String,
  pub password:  String,
  pub vhost:     String,
  pub frame_max: u32,
  pub heartbeat: u16,
}

impl ConnectionOptions {
  pub fn from_uri(uri: AMQPUri) -> ConnectionOptions {
    ConnectionOptions {
      username: uri.authority.userinfo.username,
      password: uri.authority.userinfo.password,
      vhost: uri.vhost,
      frame_max: uri.query.frame_max.unwrap_or(0),
      heartbeat: uri.query.heartbeat.unwrap_or(0),
    }
  }
}

impl Default for ConnectionOptions {
  fn default() -> ConnectionOptions {
    ConnectionOptions {
      username:  "guest".to_string(),
      password:  "guest".to_string(),
      vhost:     "/".to_string(),
      frame_max: 0,
      heartbeat: 0,
    }
  }
}

impl FromStr for ConnectionOptions {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let uri = AMQPUri::from_str(s)?;
        Ok(ConnectionOptions::from_uri(uri))
    }
}

pub type ConnectionConfiguration = lapin_async::connection::Configuration;

fn heartbeat_pulse<T: AsyncRead+AsyncWrite+Send+'static>(transport: Arc<Mutex<AMQPTransport<T>>>, heartbeat: u16) -> (impl Future<Output = Result<(), io::Error>> + Send + 'static, future::AbortHandle) {
  let interval  = if heartbeat == 0 {
    Err(())
  } else {
    Ok(Interval::new(Instant::now(), Duration::from_secs(heartbeat.into()))
       .map_err(|err| io::Error::new(io::ErrorKind::Other, err)))
  };

  let heartbeat_future = interval.into_future().or_else(|_| future::empty()).and_then(move |interval| {
    interval.for_each(move |_| {
      debug!("poll heartbeat");

      let transport = transport.clone();

      future::poll_fn(move |ctx| {
        let mut transport = lock_transport!(transport, ctx);
        debug!("Sending heartbeat");
        transport.send_heartbeat()
      }).map(|_| ())
    }).map_err(|err| {
      if let Err(Aborted) = err {
        err
      } else {
        error!("Error occured in heartbeat interval: {}", err);
        err
      }
    })
  });

  future::abortable(heartbeat_future)
}

/// A heartbeat task.
pub struct Heartbeat<Pulse> {
    handle: Option<HeartbeatHandle>,
    pulse:  Pulse,
}

impl<Pulse> Heartbeat<Pulse> {
    /// Get the handle for this heartbeat.
    ///
    /// As there can only be one handle for a given heartbeat task, this function can return
    /// `None` if the handle to this heartbeat was already acquired.
    pub fn handle(&mut self) -> Option<HeartbeatHandle> {
        self.handle.take()
    }
}

fn make_heartbeat<T, Pulse>(transport: Arc<Mutex<AMQPTransport<T>>>, heartbeat: u32) -> Heartbeat<Pulse> {
    debug!("heartbeat; interval={}", heartbeat);
    let (heartbeat_future, handle) = heartbeat_pulse(transport, heartbeat);

    Heartbeat {
        handle: Some(handle),
        pulse:  heartbeat_future,
    }
}

impl<F> Future for Heartbeat<F> where F: Future {
    type Output = F::Output;

    fn poll(self: PinMut<Self>, ctx: &mut task::Context) -> Poll<Self::Output> {
        self.pulse.poll(ctx)
    }
}

/// A handle to stop a connection heartbeat.
pub struct HeartbeatHandle(oneshot::Sender<()>);

impl HeartbeatHandle {
    /// Signals the heartbeat task to stop sending packets to the broker.
    pub fn stop(self) {
        if let Err(_) = self.0.send(()) {
            warn!("Couldn't send stop signal to heartbeat: already gone");
        }
    }
}

impl<T: AsyncRead+AsyncWrite+Send+Sync+'static> Client<T> {
  /// Takes a stream (TCP, TLS, unix socket, etc) and uses it to connect to an AMQP server.
  ///
  /// This function returns a future that resolves once the connection handshake is done.
  /// The result is a tuple containing a `Client` that can be used to create `Channel`s and a
  /// `Heartbeat` instance. The heartbeat is a task (it implements `Future`) that should be
  /// spawned independently of the other futures.
  ///
  /// To stop the heartbeat task, see `HeartbeatHandle`.
  ///
  /// # Example
  ///
  /// ```
  /// # extern crate lapin_futures;
  /// # extern crate tokio;
  /// #
  /// # use tokio::prelude::*;
  /// #
  /// # fn main() {
  /// use tokio::net::TcpStream;
  /// use tokio::runtime::Runtime;
  /// use lapin_futures::client::{Client, ConnectionOptions};
  ///
  /// let addr = "127.0.0.1:5672".parse().unwrap();
  /// let f = TcpStream::connect(&addr)
  ///     .and_then(|stream| {
  ///         Client::connect(stream, ConnectionOptions::default())
  ///     })
  ///     .and_then(|(client, mut heartbeat)| {
  ///         let handle = heartbeat.handle().unwrap();
  ///         tokio::spawn(
  ///             heartbeat.map_err(|e| eprintln!("The heartbeat task errored: {}", e))
  ///         );
  ///
  ///         /// ...
  ///
  ///         handle.stop();
  ///         Ok(())
  ///     });
  /// Runtime::new().unwrap().block_on(
  ///     f.map_err(|e| eprintln!("An error occured: {}", e))
  /// ).expect("runtime exited with failure");
  /// # }
  /// ```
  pub fn connect(stream: T, options: ConnectionOptions) ->
    impl Future<Output = Result<(Self, Heartbeat<impl Future<Output = Result<(), io::Error> + Send + 'static>), io::Error>>> + Send + 'static
  {
    AMQPTransport::connect(stream, options).and_then(|transport| {
      debug!("got client service");
      let configuration = transport.conn.configuration.clone();
      let transport = Arc::new(Mutex::new(transport));
      let heartbeat = make_heartbeat(transport.clone(), configuration.heartbeat);
      let client = Client { configuration, transport };
      Ok((client, heartbeat))
    })
  }

  /// creates a new channel
  ///
  /// returns a future that resolves to a `Channel` once the method succeeds
  pub fn create_channel(&self) -> impl Future<Output = Result<Channel<T>, io::Error>> + Send + 'static {
    Channel::create(self.transport.clone())
  }

  /// returns a future that resolves to a `Channel` once the method succeeds
  /// the channel will support RabbitMQ's confirm extension
  pub fn create_confirm_channel(&self, options: ConfirmSelectOptions) -> impl Future<Output = Result<Channel<T>, io::Error>> + Send + 'static {
    //FIXME: maybe the confirm channel should be a separate type
    //especially, if we implement transactions, the methods should be available on the original channel
    //but not on the confirm channel. And the basic publish method should have different results
    self.create_channel().and_then(move |channel| {
      let ch = channel.clone();

      channel.confirm_select(options).map(|_| ch)
    })
  }
}
