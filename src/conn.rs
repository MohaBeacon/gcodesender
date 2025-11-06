// src/conn.rs
use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration as TokioDuration};
use tokio_serial::SerialPortBuilderExt;

pub type ConnReader = BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin + 'static>>;
pub type ConnWriter = BufWriter<Box<dyn tokio::io::AsyncWrite + Send + Unpin + 'static>>;

#[derive(Clone)]
pub struct Connection {
    reader: Arc<Mutex<ConnReader>>,
    writer: Arc<Mutex<ConnWriter>>,
}

impl Connection {
    pub async fn open(conn_str: &str) -> Result<Self> {
        if conn_str.contains("://") || (conn_str.contains(':') && conn_str.rfind(':').map(|i| i + 1 < conn_str.len()).unwrap_or(false)) {
            // TCP
            let stream = TcpStream::connect(conn_str).await?;
            stream.set_nodelay(true)?;
            let (read, write) = tokio::io::split(stream);
            Ok(Self::new(read, write))
        } else {
            // Serial
            let (path, baud) = conn_str
                .rsplit_once(':')
                .and_then(|(p, b)| b.parse::<u32>().ok().map(|b| (p, b)))
                .unwrap_or((conn_str, 115200));

            let port = tokio_serial::new(path, baud)
                .data_bits(tokio_serial::DataBits::Eight)
                .parity(tokio_serial::Parity::None)
                .stop_bits(tokio_serial::StopBits::One)
                .flow_control(tokio_serial::FlowControl::None)
                .timeout(TokioDuration::from_millis(1000))
                .open_native_async()?;

            let (read, write) = tokio::io::split(port);
            Ok(Self::new(read, write))
        }
    }

    fn new<R: tokio::io::AsyncRead + Send + Unpin + 'static, W: tokio::io::AsyncWrite + Send + Unpin + 'static>(
        read: R,
        write: W,
    ) -> Self {
        Self {
            reader: Arc::new(Mutex::new(BufReader::new(Box::new(read)))),
            writer: Arc::new(Mutex::new(BufWriter::new(Box::new(write)))),
        }
    }

    pub async fn send_command(&self, cmd: &str) -> Result<String> {
        let cmd = cmd.trim();

        // Write
        {
            let mut writer = self.writer.lock().await;
            timeout(TokioDuration::from_secs(5), async {
                writer.write_all(cmd.as_bytes()).await?;
                writer.write_all(b"\n").await?;
                writer.flush().await
            })
            .await
            .map_err(|_| anyhow!("Write timeout"))??;
        }

        // Read
        let response = timeout(TokioDuration::from_secs(10), async {
            let mut response = String::new();
            let mut reader = self.reader.lock().await;

            loop {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        response.push_str(&line);
                        let trimmed = line.trim_end();
                        if trimmed == "ok" || trimmed.starts_with("error:") || trimmed.starts_with("ALARM:") {
                            break;
                        }
                    }
                    Err(e) => return Err(anyhow!(e)),
                }
            }
            Ok(response)
        })
        .await
        .map_err(|_| anyhow!("Read timeout - no 'ok'/'error'"))??;

        Ok(response)
    }

    pub async fn flush(&self) -> Result<()> {
        self.writer.lock().await.flush().await?;
        Ok(())
    }
}