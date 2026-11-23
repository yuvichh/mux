//! One SFTP conversation with another machine, and the requests that are
//! on their way in it.
//!
//! Any number of requests can be waiting at once. Each is written with a
//! number of its own, and the answer that comes back with that number is
//! handed to whoever asked. Asking does not wait: a request is on its way
//! as soon as it is asked, so many can be asked before the first answer is
//! read.

use crate::wire::{self, Reply, Request};
use model::place::Failure;
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

/// The most that is kept of what the program that carries the conversation
/// wrote for a person to read.
const MOST_SAID: usize = 2048;

/// Who is waiting for the answer to each request, by its number. `None`
/// once the conversation has ended, with why.
type Waiting = Arc<Mutex<Result<HashMap<u32, oneshot::Sender<Reply>>, Failure>>>;

/// A conversation that has begun: the other machine has said which version
/// it speaks.
#[derive(Debug)]
pub(crate) struct Session {
    /// Packets to write, in the order they were asked.
    outgoing: mpsc::UnboundedSender<Vec<u8>>,
    waiting: Waiting,
    next: AtomicU32,
    /// The program that carries the conversation, if it is one. It is
    /// stopped when the session is let go of.
    _child: Option<Child>,
}

/// An answer that is on its way.
#[derive(Debug)]
pub(crate) struct Asked {
    answer: oneshot::Receiver<Reply>,
    waiting: Waiting,
}

impl Asked {
    /// Waits for the answer.
    pub(crate) async fn answer(self) -> Result<Reply, Failure> {
        match self.answer.await {
            Ok(reply) => Ok(reply),
            // Nobody will answer: the conversation ended, and says why.
            Err(_) => Err(ended(&self.waiting)),
        }
    }
}

/// Why a conversation that is over ended.
fn ended(waiting: &Waiting) -> Failure {
    let waiting = waiting.lock().unwrap_or_else(PoisonError::into_inner);
    match &*waiting {
        Err(why) => why.clone(),
        Ok(_) => Failure::Unreachable("the connection was closed".into()),
    }
}

impl Session {
    /// Runs `program`, which is to carry the conversation on its standard
    /// input and output, as `ssh` does when it is asked for the SFTP
    /// subsystem. `to` names the other machine in what is said of it.
    ///
    /// What the program writes for a person to read is kept, and is part
    /// of the failure if the conversation does not begin or breaks off.
    pub(crate) async fn run(mut program: Command, to: &str) -> Result<Arc<Self>, Failure> {
        let ran = program
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = ran.map_err(|e| {
            let name = program
                .as_std()
                .get_program()
                .to_string_lossy()
                .into_owned();
            Failure::Fatal(format!("cannot run `{name}` to reach {to}: {e}"))
        })?;
        let (Some(input), Some(output), Some(mut errors)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(Failure::Fatal(format!(
                "cannot talk to the program that reaches {to}"
            )));
        };
        let said = Arc::new(Mutex::new(String::new()));
        let keeping = said.clone();
        tokio::spawn(async move {
            let mut heard = [0; 512];
            while let Ok(count @ 1..) = errors.read(&mut heard).await {
                let mut said = keeping.lock().unwrap_or_else(PoisonError::into_inner);
                said.push_str(&String::from_utf8_lossy(&heard[..count]));
                let over = said.len().saturating_sub(MOST_SAID);
                let cut = (over..=said.len()).find(|&at| said.is_char_boundary(at));
                said.drain(..cut.unwrap_or(0));
            }
        });
        let why = {
            let to = to.to_owned();
            move || {
                let said = said.lock().unwrap_or_else(PoisonError::into_inner);
                match said.trim() {
                    "" => format!("the connection to {to} was closed"),
                    said => format!("cannot reach {to}: {said}"),
                }
            }
        };
        Self::over(output, input, Some(child), why).await
    }

    /// Begins a conversation over `output`, which the other machine's
    /// answers are read from, and `input`, which requests are written to.
    /// `why` says why it ended, when it does.
    pub(crate) async fn over(
        mut output: impl AsyncRead + Send + Unpin + 'static,
        mut input: impl AsyncWrite + Send + Unpin + 'static,
        child: Option<Child>,
        why: impl Fn() -> String + Send + Sync + 'static,
    ) -> Result<Arc<Self>, Failure> {
        // The first exchange: each says its version. It is waited for as
        // long as it takes, since a person may be typing a password.
        let greeted = async {
            input.write_all(&Request::Init.encode(0)).await.ok()?;
            input.flush().await.ok()?;
            match wire::decode(&packet(&mut output).await.ok()??) {
                Ok((None, Reply::Version(version))) => Some(version),
                _ => None,
            }
        };
        // Give what the program said a moment to arrive, so that the
        // failure can name it.
        let Some(version) = greeted.await else {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            return Err(Failure::Unreachable(why()));
        };
        if version < wire::VERSION {
            return Err(Failure::Fatal(format!(
                "the other machine speaks version {version} of SFTP, and mux needs version {}",
                wire::VERSION
            )));
        }

        let waiting: Waiting = Arc::new(Mutex::new(Ok(HashMap::new())));
        let (outgoing, mut packets) = mpsc::unbounded_channel::<Vec<u8>>();
        // Writes each request. When the session is let go of, the channel
        // closes, and so does the program's input.
        tokio::spawn(async move {
            while let Some(packet) = packets.recv().await {
                if input.write_all(&packet).await.is_err() || input.flush().await.is_err() {
                    break;
                }
            }
        });
        // Reads each answer and hands it to whoever asked.
        let answered = waiting.clone();
        tokio::spawn(async move {
            let broke = loop {
                let read = match packet(&mut output).await {
                    Ok(Some(read)) => read,
                    Ok(None) => break Failure::Unreachable(why()),
                    Err(odd) => break Failure::Corrupt(format!("the other machine sent {odd}")),
                };
                let (id, reply) = match wire::decode(&read) {
                    Ok((Some(id), reply)) => (id, reply),
                    Ok((None, _)) => {
                        break Failure::Corrupt("the other machine sent a second greeting".into());
                    }
                    Err(wire::Malformed(odd)) => {
                        break Failure::Corrupt(format!("the other machine sent {odd}"));
                    }
                };
                let asked = match &mut *answered.lock().unwrap_or_else(PoisonError::into_inner) {
                    Ok(waiting) => waiting.remove(&id),
                    Err(_) => None,
                };
                // Nobody waits for the answer to a request that was only
                // sent, as a close is.
                if let Some(asked) = asked {
                    let _ = asked.send(reply);
                }
            };
            // Everyone who waits is told, by losing what they wait on.
            *answered.lock().unwrap_or_else(PoisonError::into_inner) = Err(broke);
        });
        Ok(Arc::new(Self {
            outgoing,
            waiting,
            next: AtomicU32::new(1),
            _child: child,
        }))
    }

    /// Asks `request`. It is on its way when this returns.
    pub(crate) fn ask(&self, request: &Request<'_>) -> Asked {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (asked, answer) = oneshot::channel();
        if let Ok(waiting) = &mut *self.waiting.lock().unwrap_or_else(PoisonError::into_inner) {
            waiting.insert(id, asked);
            // A conversation whose writer has stopped is one that ended.
            let _ = self.outgoing.send(request.encode(id));
        }
        Asked {
            answer,
            waiting: self.waiting.clone(),
        }
    }

    /// Sends `request` and does not wait for its answer: for letting go of
    /// what was opened, which nothing depends on.
    pub(crate) fn send(&self, request: &Request<'_>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = self.outgoing.send(request.encode(id));
    }
}

/// Reads one packet, without its length. `None` at the end of the output.
async fn packet(output: &mut (impl AsyncRead + Unpin)) -> Result<Option<Vec<u8>>, &'static str> {
    let mut len = [0; 4];
    match output.read_exact(&mut len).await {
        Ok(_) => {}
        Err(_) => return Ok(None),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > wire::MAX_PACKET {
        return Err("a packet larger than any it should send");
    }
    let mut body = vec![0; len];
    match output.read_exact(&mut body).await {
        Ok(_) => Ok(Some(body)),
        Err(_) => Ok(None),
    }
}
