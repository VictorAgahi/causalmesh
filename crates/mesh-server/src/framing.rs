use std::io::ErrorKind;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const MPSC_BUFFER_CAPACITY: usize = 64;

/// Dedicated Stdio Actor managing non-blocking JSON-RPC stdio framing per RFC-001 Commandment 3.
pub struct StdioFramingActor;

impl StdioFramingActor {
    /// Spawns the dedicated reader and writer actors.
    /// Returns the sender to write frames and the receiver of incoming lines.
    pub fn spawn(
        cancel_token: CancellationToken,
    ) -> (mpsc::Sender<String>, mpsc::Receiver<String>) {
        let (tx_in, rx_in) = mpsc::channel::<String>(MPSC_BUFFER_CAPACITY);
        let (tx_out, mut rx_out) = mpsc::channel::<String>(MPSC_BUFFER_CAPACITY);

        // Dedicated Tokio writer task wrapping stdout in BufWriter
        let cancel_writer = cancel_token.clone();
        tokio::spawn(async move {
            let stdout = tokio::io::stdout();
            let mut writer = BufWriter::new(stdout);

            loop {
                tokio::select! {
                    _ = cancel_writer.cancelled() => {
                        let _ = writer.flush().await;
                        break;
                    }
                    msg = rx_out.recv() => {
                        match msg {
                            Some(line) => {
                                if let Err(e) = writer.write_all(line.as_bytes()).await {
                                    tracing::error!(target: "mesh::framing", "Stdout write error: {e}");
                                    break;
                                }
                                if !line.ends_with('\n') {
                                    let _ = writer.write_all(b"\n").await;
                                }
                                if let Err(e) = writer.flush().await {
                                    tracing::error!(target: "mesh::framing", "Stdout flush error: {e}");
                                    break;
                                }
                            }
                            None => {
                                let _ = writer.flush().await;
                                break;
                            }
                        }
                    }
                }
            }
        });

        // Dedicated Tokio reader task reading stdin line-by-line
        let cancel_reader = cancel_token.clone();
        tokio::spawn(async move {
            let stdin = tokio::io::stdin();
            let mut reader = BufReader::new(stdin);
            let mut line = String::new();

            loop {
                line.clear();
                tokio::select! {
                    _ = cancel_reader.cancelled() => break,
                    read_res = reader.read_line(&mut line) => {
                        match read_res {
                            Ok(0) => {
                                tracing::info!(target: "mesh::framing", "Stdin EOF detected. Initiating shutdown.");
                                cancel_reader.cancel();
                                break;
                            }
                            Ok(_) => {
                                let trimmed = line.trim().to_string();
                                if !trimmed.is_empty() && tx_in.send(trimmed).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                            Err(e) => {
                                tracing::error!(target: "mesh::framing", "Stdin read error: {e}");
                                break;
                            }
                        }
                    }
                }
            }
        });

        (tx_out, rx_in)
    }
}
