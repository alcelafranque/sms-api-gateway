//! Hands-on tool to validate the protocol against a real device.
//!
//! ```text
//! TPLINK_URL=http://192.168.254.1 TPLINK_USER=user TPLINK_PASSWORD=... \
//!     cargo run -p tplink-ex --example probe -- [OPTIONS] <COMMAND>
//! ```
//!
//! Run without arguments for the list of commands and options.

use std::process::ExitCode;
use std::time::{Duration, Instant};

use tplink_ex::{Act, Client, ClientConfig, Error, InboxEntry, Operation, Variant};

const USAGE: &str = "\
usage: probe [OPTIONS] <COMMAND>

Environment:
  TPLINK_URL        device base URL (default http://192.168.254.1)
  TPLINK_USER       web UI user (default \"user\"; MR-style firmwares use \"admin\")
  TPLINK_PASSWORD   web UI password (required)

Commands:
  login                     log in, print the cipher variant, log out
  status                    radio / LTE status
  inbox                     list received messages
  send <number> <text>      send an SMS and poll its send result
  read <index>              mark a message as read
  delete <index>            delete a message (looked up by index)
  raw <op> <oid> [--stack S] [--pstack S] [name | key=value]...
                            send any act (op: go gl gs so add del do op cgi)
                            and print the returned data

Options:
  --variant <auto|cbc|gcm|gcm-oaep>   cipher variant (default auto)
  --timeout <secs>                    request timeout (default 30)
  --send-wait <secs>                  send result polling time (default 20, 0 = off)
  --insecure                          accept invalid TLS certificates
  --no-logout                         keep the session open at exit
  -v, -vv                             debug / trace protocol logs, raw data
  -h, --help                          this help
";

struct Options {
    variant: Variant,
    timeout: u64,
    send_wait: u64,
    insecure: bool,
    logout: bool,
    verbose: u8,
    command: Vec<String>,
}

fn parse_args() -> Result<Options, String> {
    let mut o = Options {
        variant: Variant::Auto,
        timeout: 30,
        send_wait: 20,
        insecure: false,
        logout: true,
        verbose: 0,
        command: Vec::new(),
    };
    let mut args = std::env::args().skip(1);
    let number = |v: Option<String>, name: &str| -> Result<u64, String> {
        v.ok_or(format!("{name} needs a value"))?
            .parse()
            .map_err(|_| format!("{name} needs a number"))
    };
    while let Some(arg) = args.next() {
        // The text of an SMS is passed through untouched, even if it looks
        // like an option.
        if o.command.first().is_some_and(|c| c == "send") && o.command.len() >= 2 {
            o.command.push(arg);
            continue;
        }
        match arg.as_str() {
            "--variant" => {
                o.variant = args
                    .next()
                    .ok_or("--variant needs a value")?
                    .parse()
                    .map_err(|e: Error| e.to_string())?;
            }
            "--timeout" => o.timeout = number(args.next(), "--timeout")?.max(1),
            "--send-wait" => o.send_wait = number(args.next(), "--send-wait")?,
            "--insecure" => o.insecure = true,
            "--no-logout" => o.logout = false,
            "-v" | "--verbose" => o.verbose += 1,
            "-vv" => o.verbose += 2,
            "-h" | "--help" => return Err(String::new()),
            // `raw` takes its own --stack / --pstack flags.
            s if s.starts_with('-') && o.command.first().is_some_and(|c| c == "raw") => {
                o.command.push(arg)
            }
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => o.command.push(arg),
        }
    }
    if o.command.is_empty() {
        return Err(String::new());
    }
    Ok(o)
}

fn init_logs(verbose: u8) {
    let default = match verbose {
        0 => "tplink_ex=info",
        1 => "tplink_ex=debug",
        _ => "tplink_ex=trace",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let opts = match parse_args() {
        Ok(o) => o,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("error: {msg}\n");
            }
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    init_logs(opts.verbose);

    let url = std::env::var("TPLINK_URL").unwrap_or_else(|_| "http://192.168.254.1".into());
    let user = std::env::var("TPLINK_USER").unwrap_or_else(|_| "user".into());
    let Ok(password) = std::env::var("TPLINK_PASSWORD") else {
        eprintln!("error: TPLINK_PASSWORD is not set");
        return ExitCode::from(2);
    };

    let mut config = ClientConfig::new(url, user, password);
    config.variant = opts.variant;
    config.timeout = Duration::from_secs(opts.timeout);
    config.connect_timeout = Duration::from_secs(opts.timeout.min(10));
    config.accept_invalid_certs = opts.insecure;
    config.send_result_timeout = Duration::from_secs(opts.send_wait);
    println!(
        "device {} as {:?}, variant {}",
        config.base_url, config.username, config.variant
    );

    let mut client = match Client::new(config) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };

    let started = Instant::now();
    match client.login().await {
        Ok(v) => println!(
            "logged in, variant {v}{} ({} ms)",
            if opts.variant == Variant::Auto {
                " (detected)"
            } else {
                ""
            },
            started.elapsed().as_millis()
        ),
        Err(e) => return fail(&e),
    }

    let result = run(&mut client, &opts).await;

    if opts.logout {
        match client.logout().await {
            Ok(()) => println!("logged out"),
            Err(e) => eprintln!("logout failed: {}", e.detailed()),
        }
    }
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => fail(&e),
    }
}

fn fail(e: &Error) -> ExitCode {
    eprintln!("error: {}", e.detailed());
    eprintln!("       kind: {:?}", e.kind());
    ExitCode::FAILURE
}

async fn run(client: &mut Client, opts: &Options) -> Result<(), Error> {
    let cmd: Vec<&str> = opts.command.iter().map(String::as_str).collect();
    let verbose = opts.verbose > 0;
    match cmd.as_slice() {
        ["login"] => Ok(()),
        ["status"] => status(client, verbose).await,
        ["inbox"] => inbox(client, verbose).await,
        ["send", to, text @ ..] if !text.is_empty() => send(client, to, &text.join(" ")).await,
        ["read", index] => mark_read(client, parse_index(index)?).await,
        ["delete", index] => delete(client, parse_index(index)?).await,
        ["raw", op, oid, rest @ ..] => raw(client, op, oid, rest).await,
        _ => Err(Error::InvalidInput(format!(
            "bad command {:?} (run without arguments for help)",
            opts.command.join(" ")
        ))),
    }
}

fn parse_index(s: &str) -> Result<u32, Error> {
    s.parse()
        .map_err(|_| Error::InvalidInput(format!("bad index {s:?}")))
}

async fn status(client: &mut Client, verbose: bool) -> Result<(), Error> {
    let s = client.lte_status().await?;
    let show = |v: Option<String>| v.unwrap_or_else(|| "-".into());
    println!("operator       {}", show(s.operator.clone()));
    println!(
        "unread SMS     {}",
        show(s.unread_count.map(|v| v.to_string()))
    );
    println!(
        "signal level   {}",
        show(s.signal_level.map(|v| v.to_string()))
    );
    println!(
        "RSRP / RSRQ    {} / {}",
        show(s.rsrp.map(|v| v.to_string())),
        show(s.rsrq.map(|v| v.to_string()))
    );
    println!(
        "SNR (raw)      {}",
        show(
            s.snr
                .map(|v| format!("{v} (= {:.1} dB if tenths)", v as f32 / 10.0))
        )
    );
    println!(
        "radio source   {}",
        if s.from_serving_cell {
            "DEV2_LTE_SERVING_CELL_INFO"
        } else {
            "DEV2_LTE_NET_STATUS"
        }
    );
    println!(
        "network type   {}   connect status {}   SIM status {}",
        show(s.network_type.map(|v| v.to_string())),
        show(s.connect_status.map(|v| v.to_string())),
        show(s.sim_status.map(|v| v.to_string()))
    );
    match client.inbox_total().await {
        Ok(n) => println!("inbox total    {n}"),
        Err(e) => println!("inbox total    error: {}", e.detailed()),
    }
    if verbose {
        for (oid, data) in &s.raw {
            println!("--- {oid}\n{}", pretty(data));
        }
        // Candidate send-result attributes, to check what this firmware has.
        for act in [
            Act::get("DEV2_LTE_SMS_SENDNEWMSG", &["sendResult"]),
            Act::get("DEV2_LTE_NET_STATUS", &["smsSendResult", "smsSendCause"])
                .stack("1,0,0,0,0,0"),
        ] {
            let label = format!("{} {} (send result candidate)", act.operation(), act.oid());
            match client.act(&act).await {
                Ok(Some(d)) => println!("--- {label}\n{}", pretty(&d)),
                Ok(None) => println!("--- {label}: no data"),
                Err(e) => println!("--- {label}: {}", e.detailed()),
            }
        }
    }
    Ok(())
}

fn print_entry(e: &InboxEntry, verbose: bool) {
    println!(
        "[{}] {} {}{}  from {}",
        e.index,
        e.received_time,
        if e.unread { "UNREAD" } else { "read" },
        match e.received_at {
            Some(_) => String::new(),
            None => " (time format not recognized!)".into(),
        },
        e.from
    );
    if verbose {
        println!("    page {} stack {}", e.location.page, e.location.stack);
    }
    for line in e.content.lines() {
        println!("    | {line}");
    }
}

async fn inbox(client: &mut Client, verbose: bool) -> Result<(), Error> {
    let total = client.inbox_total().await?;
    let entries = client.inbox().await?;
    println!("{} message(s), device totalNumber {total}", entries.len());
    for e in &entries {
        print_entry(e, verbose);
    }
    Ok(())
}

async fn send(client: &mut Client, to: &str, text: &str) -> Result<(), Error> {
    println!("sending {} chars to {to}...", text.chars().count());
    let started = Instant::now();
    let report = client.send_sms(to, text).await?;
    println!(
        "status         {:?} after {} ms",
        report.status,
        started.elapsed().as_millis()
    );
    println!(
        "result source  {}",
        report.source.unwrap_or("none (attribute unavailable)")
    );
    println!(
        "baseline       {}",
        report.baseline.as_deref().unwrap_or("-")
    );
    println!("observed       {:?}", report.observed);
    if let Some(cause) = &report.cause {
        println!("failure cause  {cause}");
    }
    Ok(())
}

async fn mark_read(client: &mut Client, index: u32) -> Result<(), Error> {
    let entries = client.inbox().await?;
    let Some(entry) = entries.iter().find(|e| e.index == index) else {
        return Err(Error::InvalidInput(format!(
            "no message with index {index}"
        )));
    };
    client.mark_read(&entry.location).await?;
    println!("marked [{index}] as read");
    Ok(())
}

async fn delete(client: &mut Client, index: u32) -> Result<(), Error> {
    match client
        .delete_sms_resolved(|entries| entries.iter().position(|e| e.index == index))
        .await?
    {
        Some(e) => {
            println!("deleted:");
            print_entry(&e, true);
        }
        None => println!("no message with index {index}"),
    }
    Ok(())
}

async fn raw(client: &mut Client, op: &str, oid: &str, rest: &[&str]) -> Result<(), Error> {
    let op: Operation = op.parse()?;
    let mut act = Act::new(op, oid);
    let mut it = rest.iter();
    while let Some(arg) = it.next() {
        match *arg {
            "--stack" => {
                act = act.stack(
                    *it.next()
                        .ok_or(Error::InvalidInput("--stack needs a value".into()))?,
                )
            }
            "--pstack" => {
                act = act.pstack(
                    *it.next()
                        .ok_or(Error::InvalidInput("--pstack needs a value".into()))?,
                )
            }
            kv => match kv.split_once('=') {
                Some((k, v)) => act = act.attr(k, v),
                None => act = act.fields(&[kv]),
            },
        }
    }
    println!("> {}", act.to_json());
    match client.act(&act).await? {
        Some(data) => println!("{}", pretty(&data)),
        None => println!("(no data)"),
    }
    Ok(())
}

fn pretty(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}
