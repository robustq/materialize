// Copyright Materialize, Inc. All rights reserved.
//
// Use of this software is governed by the Business Source License
// included in the LICENSE file.
//
// As of the Change Date specified in that file, in accordance with
// the Business Source License, use of this software will be governed
// by the Apache License, Version 2.0.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot, watch};
use uuid::Uuid;

use ore::thread::JoinOnDropHandle;
use sql::ast::{Raw, Statement};

use crate::command::{Cancelled, Command, ExecuteResponse, Response, StartupResponse};
use crate::error::CoordError;
use crate::id_alloc::IdAllocator;
use crate::session::{EndTransactionAction, Session};

/// A handle to a running coordinator.
///
/// The coordinator runs on its own thread. Dropping the handle will wait for
/// the coordinator's thread to exit, which will only occur after all
/// outstanding [`Client`]s for the coordinator have dropped.
pub struct Handle {
    pub(crate) cluster_id: Uuid,
    pub(crate) _thread: JoinOnDropHandle<()>,
}

impl Handle {
    /// Returns the cluster ID associated with this coordinator.
    ///
    /// The cluster ID is recorded in the data directory when it is first
    /// created and persists until the data directory is deleted.
    pub fn cluster_id(&self) -> Uuid {
        self.cluster_id
    }
}

/// A coordinator client.
///
/// A coordinator client is a simple handle to a communication channel with the
/// coordinator. It can be cheaply cloned.
///
/// Clients keep the coordinator alive. The coordinator will not exit until all
/// outstanding clients have dropped.
#[derive(Debug, Clone)]
pub struct Client {
    cmd_tx: mpsc::UnboundedSender<Command>,
    id_alloc: Arc<IdAllocator>,
}

impl Client {
    pub(crate) fn new(cmd_tx: mpsc::UnboundedSender<Command>) -> Client {
        Client {
            cmd_tx,
            id_alloc: Arc::new(IdAllocator::new(1, 1 << 16)),
        }
    }

    /// Allocates a client for an incoming connection.
    pub fn new_conn(&self) -> Result<ConnClient, CoordError> {
        Ok(ConnClient {
            conn_id: self.id_alloc.alloc()?,
            inner: self.clone(),
        })
    }
}

/// A coordinator client that is bound to a connection.
///
/// The `ConnClient` automatically allocates an ID for the connection when
/// it is created, and frees that ID for potential reuse when it is dropped.
///
/// See also [`Client`].
#[derive(Debug, Clone)]
pub struct ConnClient {
    conn_id: u32,
    inner: Client,
}

impl ConnClient {
    /// Returns the ID of the connection associated with this client.
    pub fn conn_id(&self) -> u32 {
        self.conn_id
    }

    /// Upgrades this connection client to a session client.
    ///
    /// A session is a connection that has successfully negotiated parameters,
    /// like the user. Most coordinator operations are available only after
    /// upgrading a connection to a session.
    ///
    /// Returns a new client that is bound to the session and a response
    /// containing various details about the startup.
    pub async fn startup(
        self,
        session: Session,
    ) -> Result<(SessionClient, StartupResponse), CoordError> {
        // Cancellation works by creating a watch channel (which remembers only
        // the last value sent to it) and sharing it between the coordinator and
        // connection. The coordinator will send a cancelled message on it if a
        // cancellation request comes. The connection will reset that on every message
        // it receives and then check for it where we want to add the ability to cancel
        // an in-progress statement.
        let (cancel_tx, cancel_rx) = watch::channel(Cancelled::NotCancelled);
        let cancel_tx = Arc::new(cancel_tx);
        let mut client = SessionClient {
            inner: self,
            session: Some(session),
            cancel_tx: cancel_tx.clone(),
            cancel_rx,
        };
        let response = client
            .send(|tx, session| Command::Startup {
                session,
                cancel_tx,
                tx,
            })
            .await;
        match response {
            Ok(response) => Ok((client, response)),
            Err(e) => {
                // When startup fails, no need to call terminate. Remove the
                // session from the client to sidestep the panic in the `Drop`
                // implementation.
                client.session.take();
                Err(e)
            }
        }
    }

    /// Cancels the query currently running on another connection.
    pub async fn cancel_request(&mut self, conn_id: u32, secret_key: u32) {
        self.inner
            .cmd_tx
            .send(Command::CancelRequest {
                conn_id,
                secret_key,
            })
            .expect("coordinator unexpectedly canceled request")
    }

    async fn send<T, F>(&mut self, f: F) -> T
    where
        F: FnOnce(oneshot::Sender<T>) -> Command,
    {
        let (tx, rx) = oneshot::channel();
        self.inner
            .cmd_tx
            .send(f(tx))
            .expect("coordinator unexpectedly gone");
        rx.await.expect("coordinator unexpectedly canceled request")
    }
}

impl Drop for ConnClient {
    fn drop(&mut self) {
        self.inner.id_alloc.free(self.conn_id);
    }
}

/// A coordinator client that is bound to a connection.
///
/// You must call [`SessionClient::terminate`] rather than dropping a session
/// client directly. Dropping an unterminated `SessionClient` will panic.
///
/// See also [`Client`].
pub struct SessionClient {
    inner: ConnClient,
    // Invariant: session may only be `None` during a method call. Every public
    // method must ensure that `Session` is `Some` before it returns.
    session: Option<Session>,
    cancel_tx: Arc<watch::Sender<Cancelled>>,
    cancel_rx: watch::Receiver<Cancelled>,
}

impl SessionClient {
    pub fn canceled(&self) -> impl Future<Output = ()> + Send {
        let mut cancel_rx = self.cancel_rx.clone();
        async move {
            loop {
                let _ = cancel_rx.changed().await;
                if let Cancelled::Cancelled = *cancel_rx.borrow() {
                    return;
                }
            }
        }
    }

    pub fn reset_canceled(&mut self) {
        // Clear any cancellation message.
        // TODO(mjibson): This makes the use of .changed annoying since it will
        // generally always have a NotCancelled message first that needs to be ignored,
        // and thus run in a loop. Figure out a way to have the future only resolve on
        // a Cancelled message.
        let _ = self.cancel_tx.send(Cancelled::NotCancelled);
    }

    /// Saves the specified statement as a prepared statement.
    ///
    /// The prepared statement is saved in the connection's [`sql::Session`]
    /// under the specified name.
    pub async fn describe(
        &mut self,
        name: String,
        stmt: Option<Statement<Raw>>,
        param_types: Vec<Option<pgrepr::Type>>,
    ) -> Result<(), CoordError> {
        self.send(|tx, session| Command::Describe {
            name,
            stmt,
            param_types,
            session,
            tx,
        })
        .await
    }

    /// Binds a statement to a portal.
    pub async fn declare(
        &mut self,
        name: String,
        stmt: Statement<Raw>,
        param_types: Vec<Option<pgrepr::Type>>,
    ) -> Result<(), CoordError> {
        self.send(|tx, session| Command::Declare {
            name,
            stmt,
            param_types,
            session,
            tx,
        })
        .await
    }

    /// Executes a previously-bound portal.
    pub async fn execute(&mut self, portal_name: String) -> Result<ExecuteResponse, CoordError> {
        self.send(|tx, session| Command::Execute {
            portal_name,
            session,
            tx,
        })
        .await
    }

    /// Ends a transaction.
    pub async fn end_transaction(
        &mut self,
        action: EndTransactionAction,
    ) -> Result<ExecuteResponse, CoordError> {
        self.send(|tx, session| Command::Commit {
            action,
            session,
            tx,
        })
        .await
    }

    /// Dumps the catalog to a JSON string.
    pub async fn dump_catalog(&mut self) -> Result<String, CoordError> {
        self.send(|tx, session| Command::DumpCatalog { session, tx })
            .await
    }

    /// Terminates this client session.
    ///
    /// This method cleans up any coordinator state associated with the session
    /// before consuming the `SessionClient. Call this method instead of
    /// dropping the object directly.
    pub async fn terminate(mut self) {
        let session = self.session.take().expect("session invariant violated");
        self.inner
            .inner
            .cmd_tx
            .send(Command::Terminate { session })
            .expect("coordinator unexpectedly gone");
    }

    /// Returns a mutable reference to the session bound to this client.
    pub fn session(&mut self) -> &mut Session {
        self.session.as_mut().unwrap()
    }

    async fn send<T, F>(&mut self, f: F) -> Result<T, CoordError>
    where
        F: FnOnce(oneshot::Sender<Response<T>>, Session) -> Command,
    {
        let session = self.session.take().expect("session invariant violated");
        let res = self.inner.send(|tx| f(tx, session)).await;
        self.session = Some(res.session);
        res.result
    }
}

impl Drop for SessionClient {
    fn drop(&mut self) {
        if self.session.is_some() {
            panic!("unterminated SessionClient dropped")
        }
    }
}
