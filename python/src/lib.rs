use anyhow::{bail, Context as _};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use russh::client::Msg;
use sk8brd::{
    parse_recv_msg, select_brd, send_ack, send_console, send_image_quiet, write_msg, Sk8brdMsgs,
    CDBA_SERVER_BIN_NAME, IMAGE_CHUNK_SIZE, MSG_HDR_SIZE,
};
use std::fs;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::sync::Mutex as TokioMutex;
use tokio::time::{sleep_until, timeout, Instant};

const MAX_MSG_LEN: usize = 1024 * 1024;
const STREAM_CHUNK_SIZE: usize = 2048;
const SESSION_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[pyclass(skip_from_py_object)]
#[derive(Clone)]
struct CdbaClient {
    host: String,
    port: u16,
    user: String,
    timeout_secs: u64,
}

#[pymethods]
impl CdbaClient {
    #[new]
    #[pyo3(signature = (host, port = 22, user = "cdba".to_string(), timeout_secs = 60))]
    fn new(host: String, port: u16, user: String, timeout_secs: u64) -> Self {
        Self {
            host,
            port,
            user,
            timeout_secs,
        }
    }

    #[getter]
    fn host(&self) -> &str {
        &self.host
    }

    #[getter]
    fn port(&self) -> u16 {
        self.port
    }

    #[getter]
    fn user(&self) -> &str {
        &self.user
    }

    fn list_devices(&self, py: Python<'_>) -> PyResult<String> {
        let client = self.clone();
        py.detach(move || run_blocking(client.list_devices_async()))
    }

    #[pyo3(signature = (board, image_path, timeout_secs = None, collect_console = false))]
    fn boot_image(
        &self,
        py: Python<'_>,
        board: String,
        image_path: String,
        timeout_secs: Option<u64>,
        collect_console: bool,
    ) -> PyResult<BootResult> {
        let mut client = self.clone();
        if let Some(timeout_secs) = timeout_secs {
            client.timeout_secs = timeout_secs;
        }

        py.detach(move || run_blocking(client.boot_image_async(board, image_path, collect_console)))
    }

    #[pyo3(signature = (board, image_path, timeout_secs = None))]
    fn boot_image_session(
        &self,
        py: Python<'_>,
        board: String,
        image_path: String,
        timeout_secs: Option<u64>,
    ) -> PyResult<CdbaSession> {
        let mut client = self.clone();
        if let Some(timeout_secs) = timeout_secs {
            client.timeout_secs = timeout_secs;
        }

        client.boot_image_session_blocking(py, board, image_path)
    }

    fn power_off(&self, py: Python<'_>, board: String) -> PyResult<()> {
        let client = self.clone();
        py.detach(move || run_blocking(client.send_board_ack_async(board, Sk8brdMsgs::MsgPowerOff)))
    }

    fn power_on(&self, py: Python<'_>, board: String) -> PyResult<()> {
        let client = self.clone();
        py.detach(move || run_blocking(client.send_board_ack_async(board, Sk8brdMsgs::MsgPowerOn)))
    }
}

#[pyclass]
struct BootResult {
    #[pyo3(get)]
    image_sent: bool,
    #[pyo3(get)]
    console: String,
    #[pyo3(get)]
    status: String,
}

#[pymethods]
impl BootResult {
    fn __repr__(&self) -> String {
        format!(
            "BootResult(image_sent={}, console_len={}, status_len={})",
            self.image_sent,
            self.console.len(),
            self.status.len()
        )
    }
}

#[pyclass]
struct CdbaSession {
    shared: Arc<SessionShared>,
    tx: Sender<SessionCommand>,
    worker: StdMutex<Option<JoinHandle<()>>>,
}

#[pymethods]
impl CdbaSession {
    #[getter]
    fn image_sent(&self) -> bool {
        self.shared.image_sent.load(Ordering::SeqCst)
    }

    #[getter]
    fn closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    #[pyo3(signature = (max_bytes = None))]
    fn read_console(&self, max_bytes: Option<usize>) -> PyResult<String> {
        self.shared.check_error()?;
        drain_buffer(&self.shared.console, max_bytes)
    }

    #[pyo3(signature = (max_bytes = None))]
    fn read_status(&self, max_bytes: Option<usize>) -> PyResult<String> {
        self.shared.check_error()?;
        drain_buffer(&self.shared.status, max_bytes)
    }

    fn write_console(&self, data: String) -> PyResult<()> {
        self.shared.check_error()?;
        self.tx
            .send(SessionCommand::Write(data.into_bytes()))
            .map_err(py_runtime_err)
    }

    fn power_off(&self) -> PyResult<()> {
        self.shared.check_error()?;
        self.tx
            .send(SessionCommand::PowerOff)
            .map_err(py_runtime_err)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let _ = self.tx.send(SessionCommand::Close);
        py.detach(|| self.join_worker())
    }

    fn __repr__(&self) -> String {
        format!(
            "CdbaSession(image_sent={}, closed={}, console_len={}, status_len={})",
            self.image_sent(),
            self.closed(),
            self.shared.console.lock().unwrap().len(),
            self.shared.status.lock().unwrap().len()
        )
    }
}

impl Drop for CdbaSession {
    fn drop(&mut self) {
        let _ = self.tx.send(SessionCommand::Close);
    }
}

impl CdbaSession {
    fn join_worker(&self) -> PyResult<()> {
        if let Some(worker) = self.worker.lock().unwrap().take() {
            worker
                .join()
                .map_err(|_| PyRuntimeError::new_err("cdba session worker panicked"))?;
        }

        self.shared.check_error()
    }
}

struct SessionShared {
    console: StdMutex<String>,
    status: StdMutex<String>,
    error: StdMutex<Option<String>>,
    image_sent: AtomicBool,
    closed: AtomicBool,
}

impl SessionShared {
    fn new() -> Self {
        Self {
            console: StdMutex::new(String::new()),
            status: StdMutex::new(String::new()),
            error: StdMutex::new(None),
            image_sent: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    fn append_console(&self, data: &[u8]) {
        self.console
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(data));
    }

    fn append_status(&self, data: &[u8]) {
        self.status
            .lock()
            .unwrap()
            .push_str(&String::from_utf8_lossy(data));
    }

    fn fail(&self, err: impl std::fmt::Display) {
        *self.error.lock().unwrap() = Some(err.to_string());
    }

    fn check_error(&self) -> PyResult<()> {
        match self.error.lock().unwrap().as_ref() {
            Some(err) => Err(PyRuntimeError::new_err(err.clone())),
            None => Ok(()),
        }
    }
}

enum SessionCommand {
    Write(Vec<u8>),
    PowerOff,
    Close,
}

impl CdbaClient {
    fn address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    async fn list_devices_async(self) -> anyhow::Result<String> {
        let chan = Arc::new(TokioMutex::new(
            sk8brd::ssh::ssh_connect(&self.address(), self.user.clone()).await?,
        ));
        (*chan.lock().await)
            .exec(true, CDBA_SERVER_BIN_NAME)
            .await
            .with_context(|| {
                format!("could not execute {CDBA_SERVER_BIN_NAME} on remote server")
            })?;
        let mut server_stdin = Arc::new(TokioMutex::new((*chan.lock().await).make_writer()));
        let (mut server_stdout, mut server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgListDevices).await?;

        let deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        let mut stdout_buf = Vec::new();
        let mut stdout_chunk = [0u8; STREAM_CHUNK_SIZE];
        let mut stderr_chunk = [0u8; STREAM_CHUNK_SIZE];
        let mut stderr_open = true;
        let mut devices = String::new();

        while Instant::now() < deadline {
            tokio::select! {
                _ = sleep_until(deadline) => break,
                read = server_stderr.read(&mut stderr_chunk), if stderr_open => {
                    if let Ok(bytes_read) = read {
                        stderr_open = bytes_read != 0;
                    }
                }
                read = server_stdout.read(&mut stdout_chunk) => {
                    let bytes_read = read?;
                    if bytes_read == 0 {
                        break;
                    }

                    stdout_buf.extend_from_slice(&stdout_chunk[..bytes_read]);
                    while let Some((msg, payload)) = next_frame(&mut stdout_buf)? {
                        if msg == Sk8brdMsgs::MsgListDevices {
                            devices.push_str(&String::from_utf8_lossy(&payload));
                            return Ok(devices);
                        }
                    }
                }
            }
        }

        bail!("timed out waiting for device list")
    }

    async fn boot_image_async(
        self,
        board: String,
        image_path: String,
        collect_console: bool,
    ) -> anyhow::Result<BootResult> {
        let image =
            fs::read(&image_path).with_context(|| format!("could not read {image_path}"))?;
        let chan = Arc::new(TokioMutex::new(
            sk8brd::ssh::ssh_connect(&self.address(), self.user.clone()).await?,
        ));
        (*chan.lock().await)
            .exec(true, CDBA_SERVER_BIN_NAME)
            .await
            .with_context(|| {
                format!("could not execute {CDBA_SERVER_BIN_NAME} on remote server")
            })?;
        let mut server_stdin = Arc::new(TokioMutex::new((*chan.lock().await).make_writer()));
        let (mut server_stdout, mut server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;
        select_brd(&mut server_stdin, &board).await?;

        let mut deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
        let mut stdout_buf = Vec::new();
        let mut stdout_chunk = [0u8; STREAM_CHUNK_SIZE];
        let mut stderr_chunk = [0u8; STREAM_CHUNK_SIZE];
        let mut stderr_open = true;
        let mut image_sent = false;
        let mut console = String::new();
        let mut status = String::new();

        while Instant::now() < deadline {
            tokio::select! {
                _ = sleep_until(deadline) => break,
                read = server_stderr.read(&mut stderr_chunk), if stderr_open => {
                    let bytes_read = read?;
                    if bytes_read == 0 {
                        stderr_open = false;
                    } else {
                        status.push_str(&String::from_utf8_lossy(&stderr_chunk[..bytes_read]));
                    }
                }
                read = server_stdout.read(&mut stdout_chunk) => {
                    let bytes_read = read?;
                    if bytes_read == 0 {
                        break;
                    }

                    stdout_buf.extend_from_slice(&stdout_chunk[..bytes_read]);
                    while let Some((msg, payload)) = next_frame(&mut stdout_buf)? {
                        match msg {
                            Sk8brdMsgs::MsgSelectBoard => {
                                send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOn).await?;
                            }
                            Sk8brdMsgs::MsgPowerOn => {
                                deadline = Instant::now() + Duration::from_secs(self.timeout_secs);
                            }
                            Sk8brdMsgs::MsgFastbootPresent => {
                                if !payload.is_empty() && payload[0] != 0 {
                                    send_image_quiet(&mut server_stdin, &image).await?;
                                    image_sent = true;
                                }
                            }
                            Sk8brdMsgs::MsgConsole
                                if collect_console => {
                                    console.push_str(&String::from_utf8_lossy(&payload));
                                }
                            _ => (),
                        }
                    }
                }
            }
        }

        if !image_sent {
            bail!("timed out waiting for fastboot image transfer");
        }

        Ok(BootResult {
            image_sent,
            console,
            status,
        })
    }

    fn boot_image_session_blocking(
        self,
        py: Python<'_>,
        board: String,
        image_path: String,
    ) -> PyResult<CdbaSession> {
        let image = fs::read(&image_path)
            .with_context(|| format!("could not read {image_path}"))
            .map_err(py_runtime_err)?;
        let shared = Arc::new(SessionShared::new());
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let worker_shared = shared.clone();
        let ready_tx_for_error = ready_tx.clone();
        let startup_timeout = Duration::from_secs(self.timeout_secs);

        let worker = thread::spawn(move || {
            let result = run_blocking_result(session_worker(
                self,
                board,
                image,
                worker_shared.clone(),
                rx,
                ready_tx,
            ));

            if let Err(err) = result {
                worker_shared.fail(&err);
                let _ = ready_tx_for_error.send(Err(err.to_string()));
            }

            worker_shared.closed.store(true, Ordering::SeqCst);
        });

        let startup_deadline = std::time::Instant::now() + startup_timeout;
        loop {
            if let Err(err) = py.check_signals() {
                let _ = tx.send(SessionCommand::Close);
                let _ = worker.join();
                return Err(err);
            }

            match ready_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Ok(())) => {
                    return Ok(CdbaSession {
                        shared,
                        tx,
                        worker: StdMutex::new(Some(worker)),
                    });
                }
                Ok(Err(err)) => {
                    let _ = worker.join();
                    return Err(PyRuntimeError::new_err(err));
                }
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < startup_deadline => {}
                Err(err) => {
                    let _ = tx.send(SessionCommand::Close);
                    let _ = worker.join();
                    return Err(PyRuntimeError::new_err(format!(
                        "timed out waiting for image upload: {err}"
                    )));
                }
            }
        }
    }

    async fn send_board_ack_async(self, board: String, msg: Sk8brdMsgs) -> anyhow::Result<()> {
        let chan = Arc::new(TokioMutex::new(
            sk8brd::ssh::ssh_connect(&self.address(), self.user.clone()).await?,
        ));
        (*chan.lock().await)
            .exec(true, CDBA_SERVER_BIN_NAME)
            .await
            .with_context(|| {
                format!("could not execute {CDBA_SERVER_BIN_NAME} on remote server")
            })?;
        let mut server_stdin = Arc::new(TokioMutex::new((*chan.lock().await).make_writer()));
        select_brd(&mut server_stdin, &board).await?;
        send_ack(&mut server_stdin, msg).await
    }
}

async fn session_worker(
    client: CdbaClient,
    board: String,
    image: Vec<u8>,
    shared: Arc<SessionShared>,
    rx: Receiver<SessionCommand>,
    ready_tx: Sender<Result<(), String>>,
) -> anyhow::Result<()> {
    let chan = Arc::new(TokioMutex::new(
        sk8brd::ssh::ssh_connect(&client.address(), client.user.clone()).await?,
    ));
    (*chan.lock().await)
        .exec(true, CDBA_SERVER_BIN_NAME)
        .await
        .with_context(|| format!("could not execute {CDBA_SERVER_BIN_NAME} on remote server"))?;
    let mut server_stdin = Arc::new(TokioMutex::new((*chan.lock().await).make_writer()));
    let (mut server_stdout, mut server_stderr) = sk8brd::ssh::into_streams::<Msg>(chan).await;
    select_brd(&mut server_stdin, &board).await?;

    let mut deadline = Instant::now() + Duration::from_secs(client.timeout_secs);
    let mut stdout_buf = Vec::new();
    let mut stdout_chunk = [0u8; STREAM_CHUNK_SIZE];
    let mut stderr_chunk = [0u8; STREAM_CHUNK_SIZE];
    let mut stderr_open = true;
    let mut image_sent = false;
    let mut ready_tx = Some(ready_tx);

    while Instant::now() < deadline || image_sent {
        tokio::select! {
            _ = sleep_until(deadline), if !image_sent => break,
            _ = tokio::time::sleep(SESSION_POLL_INTERVAL) => {
                if handle_session_commands(&mut server_stdin, &rx).await? {
                    return Ok(());
                }
            }
            read = server_stderr.read(&mut stderr_chunk), if stderr_open => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    stderr_open = false;
                } else {
                    shared.append_status(&stderr_chunk[..bytes_read]);
                }
            }
            read = server_stdout.read(&mut stdout_chunk) => {
                let bytes_read = read?;
                if bytes_read == 0 {
                    break;
                }

                stdout_buf.extend_from_slice(&stdout_chunk[..bytes_read]);
                while let Some((msg, payload)) = next_frame(&mut stdout_buf)? {
                    match msg {
                        Sk8brdMsgs::MsgSelectBoard => {
                            send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOn).await?;
                        }
                        Sk8brdMsgs::MsgPowerOn => {
                            deadline = Instant::now() + Duration::from_secs(client.timeout_secs);
                        }
                        Sk8brdMsgs::MsgFastbootPresent => {
                            if !payload.is_empty() && payload[0] != 0 {
                                if send_session_image(&mut server_stdin, &image, &rx).await? {
                                    return Ok(());
                                }
                                image_sent = true;
                                shared.image_sent.store(true, Ordering::SeqCst);
                                if let Some(ready_tx) = ready_tx.take() {
                                    let _ = ready_tx.send(Ok(()));
                                }
                                deadline = Instant::now() + Duration::from_secs(client.timeout_secs);
                            }
                        }
                        Sk8brdMsgs::MsgConsole => shared.append_console(&payload),
                        _ => (),
                    }
                }
            }
        }
    }

    let _ = timeout(
        Duration::from_secs(1),
        send_ack(&mut server_stdin, Sk8brdMsgs::MsgPowerOff),
    )
    .await;

    if !image_sent {
        bail!("timed out waiting for fastboot image transfer");
    }

    Ok(())
}

async fn send_session_image(
    server_stdin: &mut Arc<TokioMutex<impl AsyncWrite + Unpin>>,
    image: &[u8],
    rx: &Receiver<SessionCommand>,
) -> anyhow::Result<bool> {
    let mut server_stdin = server_stdin.lock().await;

    for chunk in image.chunks(IMAGE_CHUNK_SIZE) {
        match rx.try_recv() {
            Ok(SessionCommand::Close) | Err(TryRecvError::Disconnected) => {
                let _ = write_msg(&mut *server_stdin, Sk8brdMsgs::MsgPowerOff, &[]).await;
                return Ok(true);
            }
            Ok(SessionCommand::PowerOff) => {
                write_msg(&mut *server_stdin, Sk8brdMsgs::MsgPowerOff, &[]).await?;
            }
            Ok(SessionCommand::Write(_)) | Err(TryRecvError::Empty) => (),
        }

        write_msg(&mut *server_stdin, Sk8brdMsgs::MsgFastbootDownload, chunk).await?;
    }

    write_msg(&mut *server_stdin, Sk8brdMsgs::MsgFastbootDownload, &[]).await?;
    Ok(false)
}

async fn handle_session_commands(
    server_stdin: &mut Arc<TokioMutex<impl tokio::io::AsyncWrite + Unpin>>,
    rx: &Receiver<SessionCommand>,
) -> anyhow::Result<bool> {
    loop {
        match rx.try_recv() {
            Ok(SessionCommand::Write(data)) => send_console(server_stdin, &data).await?,
            Ok(SessionCommand::PowerOff) => send_ack(server_stdin, Sk8brdMsgs::MsgPowerOff).await?,
            Ok(SessionCommand::Close) => {
                let _ = timeout(
                    Duration::from_secs(1),
                    send_ack(server_stdin, Sk8brdMsgs::MsgPowerOff),
                )
                .await;
                return Ok(true);
            }
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => {
                let _ = timeout(
                    Duration::from_secs(1),
                    send_ack(server_stdin, Sk8brdMsgs::MsgPowerOff),
                )
                .await;
                return Ok(true);
            }
        }
    }
}

fn run_blocking<T, F>(future: F) -> PyResult<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    run_blocking_result(future).map_err(py_runtime_err)
}

fn run_blocking_result<T, F>(future: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(future)
}

fn next_frame(buf: &mut Vec<u8>) -> anyhow::Result<Option<(Sk8brdMsgs, Vec<u8>)>> {
    loop {
        if buf.len() < MSG_HDR_SIZE {
            return Ok(None);
        }

        let msg = parse_recv_msg(&buf[..MSG_HDR_SIZE]);
        let Ok(msg_type) = Sk8brdMsgs::try_from(msg.r#type) else {
            buf.drain(..1);
            continue;
        };

        let payload_len = msg.len as usize;
        if payload_len > MAX_MSG_LEN {
            buf.drain(..1);
            continue;
        }

        let total_len = MSG_HDR_SIZE + payload_len;
        if buf.len() < total_len {
            return Ok(None);
        }

        let payload = buf[MSG_HDR_SIZE..total_len].to_vec();
        buf.drain(..total_len);
        return Ok(Some((msg_type, payload)));
    }
}

fn drain_buffer(buf: &StdMutex<String>, max_bytes: Option<usize>) -> PyResult<String> {
    let mut buf = buf.lock().unwrap();
    let Some(max_bytes) = max_bytes else {
        return Ok(std::mem::take(&mut *buf));
    };

    if max_bytes >= buf.len() {
        return Ok(std::mem::take(&mut *buf));
    }

    let mut split = max_bytes;
    while split > 0 && !buf.is_char_boundary(split) {
        split -= 1;
    }

    if split == 0 {
        return Ok(String::new());
    }

    Ok(buf.drain(..split).collect())
}

fn py_runtime_err(err: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(err.to_string())
}

#[pymodule]
fn sk8brd_cdba(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<CdbaClient>()?;
    m.add_class::<BootResult>()?;
    m.add_class::<CdbaSession>()?;
    Ok(())
}
