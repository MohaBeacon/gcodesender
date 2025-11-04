use anyhow::{Context, Result};
use clap::Parser;
use regex::Regex;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, BufWriter},
    net::TcpStream,
};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(author, version, about = "FluidNC client – serial or TCP/Telnet")]
struct Args {
    #[arg(short, long)]
    conn: String,
    #[arg(short, long)]
    gcode: Option<String>,
    #[arg(long)]
    file: Option<String>,
}

#[derive(Debug)]
struct StatusReport {
    state: String,
    mpos: (f64, f64, f64),
    feed: f64,
    spindle: f64,
}

fn parse_status_report(report: &str) -> Result<StatusReport> {
    let re_state = Regex::new(r"<(\w+)")?;
    let re_mpos = Regex::new(r"MPos:(-?[\d.]+),(-?[\d.]+),(-?[\d.]+)")?;
    let re_fs = Regex::new(r"FS:(\d+),(\d+)")?;

    let state = re_state
        .captures(report)
        .and_then(|c| c.get(1))
        .context("state missing")?
        .as_str()
        .to_string();

    let mpos_caps = re_mpos.captures(report).context("MPos missing")?;
    let mpos = (
        mpos_caps[1].parse()?,
        mpos_caps[2].parse()?,
        mpos_caps[3].parse()?,
    );

    let (feed, spindle) = if let Some(fs) = re_fs.captures(report) {
        (fs[1].parse().unwrap_or(0.0), fs[2].parse().unwrap_or(0.0))
    } else {
        (0.0, 0.0)
    };

    Ok(StatusReport {
        state,
        mpos,
        feed,
        spindle,
    })
}

enum ConnIo {
    Serial(SerialStream),
    Tcp(TcpStream),
}

impl ConnIo {
    fn split(self) -> Connection {
        match self {
            ConnIo::Serial(s) => {
                let (r, w) = tokio::io::split(s);
                Connection::Serial(BufReader::new(r), BufWriter::new(w))
            }
            ConnIo::Tcp(t) => {
                let (r, w) = tokio::io::split(t);
                Connection::Tcp(BufReader::new(r), BufWriter::new(w))
            }
        }
    }
}

enum Connection {
    Serial(
        BufReader<tokio::io::ReadHalf<SerialStream>>,
        BufWriter<tokio::io::WriteHalf<SerialStream>>,
    ),
    Tcp(
        BufReader<tokio::io::ReadHalf<TcpStream>>,
        BufWriter<tokio::io::WriteHalf<TcpStream>>,
    ),
}

async fn wait_for_idle<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut BufWriter<W>,
) -> Result<StatusReport>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    loop {
        let status_str = send_command(reader, writer, "?").await?;
        eprintln!("Status: {}", status_str.trim_end());

        if let Ok(report) = parse_status_report(&status_str) {
            if report.state == "Idle" {
                return Ok(report);
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn open_connection(conn: &str) -> Result<ConnIo> {
    if conn.contains(':') {
        let stream = TcpStream::connect(conn)
            .await
            .with_context(|| format!("TCP connect to {} failed", conn))?;
        stream.set_nodelay(true)?;
        println!("Connected via TCP to {}", conn);
        Ok(ConnIo::Tcp(stream))
    } else {
        let port = tokio_serial::new(conn, 115_200)
            .timeout(Duration::from_millis(500))
            .open_native_async()
            .with_context(|| format!("Serial open {} failed", conn))?;
        println!("Connected via serial to {}", conn);
        Ok(ConnIo::Serial(port))
    }
}

async fn send_command<R, W>(
    reader: &mut BufReader<R>,
    writer: &mut BufWriter<W>,
    cmd: &str,
) -> Result<String>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    writer.write_all(cmd.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;

    let mut response = String::new();
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break;
        }
        response.push_str(&line);
        let trimmed = line.trim();
        if trimmed == "ok" || trimmed.starts_with("error:") {
            break;
        }
    }
    Ok(response.trim_end().to_string())
}

async fn read_gcode_file(path: &str) -> Result<Vec<String>> {
    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut gcode_lines = Vec::new();
    let mut buf = String::new();
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            break;
        }
        let line = buf.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        gcode_lines.push(line.to_string());
    }
    Ok(gcode_lines)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 1)]
async fn main() -> Result<()> {
    let args = Args::parse();

    let mut lines: Vec<String> = Vec::new();
    if let Some(ref file) = args.file {
        lines.extend(read_gcode_file(file).await?);
    }
    if let Some(ref gcode) = args.gcode {
        lines.extend(
            gcode
                .split(';')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string()),
        );
    }

    let conn_io = open_connection(&args.conn).await?;
    let mut conn = conn_io.split();

    let initial_status = match &mut conn {
        Connection::Serial(reader, writer) => wait_for_idle(reader, writer).await?,
        Connection::Tcp(reader, writer) => wait_for_idle(reader, writer).await?,
    };
    println!("Initial status: {:?}", initial_status);

    if initial_status.state != "Idle" {
        let allowed_commands = ["$H", "$X"];
        let any_allowed = lines.iter().any(|line| allowed_commands.contains(&line.as_str()));
        if !any_allowed {
            anyhow::bail!(
                "Machine not ready – current state: {}. Only $X (unlock) or $H (home) are allowed in this state.",
                initial_status.state
            );
        }
        if initial_status.state == "Alarm" && !lines.iter().any(|line| line == "$H") {
            anyhow::bail!(
                "Machine in Alarm state – add $H as first G-code to home and clear alarm."
            );
        }
    }

    if !lines.is_empty() {
        println!("Sending {} G-code line(s)…", lines.len());

        for (i, line) in lines.iter().enumerate() {
            println!("  [{}] {}", i + 1, line);

            if line != "$H" && line != "$X" {
                match &mut conn {
                    Connection::Serial(reader, writer) => wait_for_idle(reader, writer).await?,
                    Connection::Tcp(reader, writer) => wait_for_idle(reader, writer).await?,
                };
            }

            let resp = match &mut conn {
                Connection::Serial(reader, writer) => send_command(reader, writer, line).await?,
                Connection::Tcp(reader, writer) => send_command(reader, writer, line).await?,
            };
            println!("Response: {}", resp);

            match &mut conn {
                Connection::Serial(reader, writer) => wait_for_idle(reader, writer).await?,
                Connection::Tcp(reader, writer) => wait_for_idle(reader, writer).await?,
            };
        }
        println!("All G-code sent successfully.");
    } else {
        println!("No G-code provided – only status was queried.");
    }

    Ok(())
}
