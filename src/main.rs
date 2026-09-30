mod ble;
mod ui;

use std::process::ExitCode;

use ble::{Power, Res, Station};

const USAGE: &str = "\
lighthouse                                   open the app
lighthouse scan                              find stations and save them
lighthouse status [ADDR...]                  power state and channel
lighthouse on|off|standby|sleep|toggle [ADDR...]
                                             change power (off = sleep or standby, set in the app)
lighthouse channel ADDR 1-16                 set the channel (optical sync mode)
lighthouse survey                            read nearby stations' channels, suggest free ones
lighthouse identify ADDR                     blink the front LED

Without ADDR, acts on saved stations (scans first if none are saved).";

#[derive(Clone, Copy)]
enum Op {
    Status,
    Power(Power),
    Toggle,
    Channel(u8),
    ReadChannel,
    Identify,
}

fn main() -> ExitCode {
    // `--on` style flags from the old Python tool still work.
    let args: Vec<String> = std::env::args()
        .skip(1)
        .map(|a| a.trim_start_matches("--").to_string())
        .collect();
    if args.is_empty() {
        return match ui::run() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("{e}");
                ExitCode::FAILURE
            }
        };
    }
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    match rt.block_on(cli(&args)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
    }
}

async fn cli(args: &[String]) -> Res<bool> {
    let (cmd, rest) = args.split_first().unwrap();
    let (op, addrs) = match cmd.as_str() {
        "help" | "h" => {
            println!("{USAGE}");
            return Ok(true);
        }
        "scan" => {
            let found = ble::scan(&ble::adapter().await?, 6).await?;
            remember(&found)?;
            for s in &found {
                println!("{}  {}  {}", s.name, s.addr, dbm(s.rssi));
            }
            return Ok(!found.is_empty());
        }
        "survey" => return survey().await,
        "status" => (Op::Status, rest),
        "toggle" => (Op::Toggle, rest),
        "identify" if rest.len() == 1 => (Op::Identify, rest),
        "channel" if rest.len() == 2 => (Op::Channel(rest[1].parse()?), &rest[..1]),
        other => match Power::parse(other) {
            Some(p) => (Op::Power(p), rest),
            None => return Err(USAGE.into()),
        },
    };

    let a = ble::adapter().await?;
    let mut saved = ble::load();
    if saved.is_empty() && addrs.is_empty() {
        println!("No saved stations, scanning...");
        remember(&ble::scan(&a, 6).await?)?;
        saved = ble::load();
    }
    let targets: Vec<(String, String)> = if addrs.is_empty() {
        saved.into_iter().collect()
    } else {
        let name = |addr: &String| saved.get(addr).cloned().unwrap_or_else(|| addr.clone());
        addrs
            .iter()
            .map(|addr| (addr.clone(), name(addr)))
            .collect()
    };
    if targets.is_empty() {
        println!("No base stations found.");
        return Ok(false);
    }

    let results =
        futures::future::join_all(targets.iter().map(|(addr, _)| run(&a, addr, op))).await;
    let mut ok = true;
    for ((addr, name), r) in targets.iter().zip(results) {
        match r {
            Ok(msg) => println!("{name} ({addr}): {msg}"),
            Err(e) => {
                ok = false;
                println!("{name} ({addr}): FAILED - {e}");
            }
        }
    }
    Ok(ok)
}

async fn run(a: &btleplug::platform::Adapter, addr: &str, op: Op) -> Res<String> {
    let st = Station::open(a, addr).await?;
    let r = apply(&st, op).await;
    st.close().await;
    r
}

async fn apply(st: &Station, op: Op) -> Res<String> {
    Ok(match op {
        Op::Status => {
            let mut out = format!(
                "{}, channel {}",
                st.power().await?.label(),
                st.channel().await?
            );
            for (label, value) in st.info().await {
                out += &format!("\n    {label}: {value}");
            }
            out
        }
        Op::Power(p) => {
            st.set_power(p).await?;
            st.settle(p, |_| {}).await?.label()
        }
        Op::Toggle => {
            let p = match st.power().await? {
                Power::On | Power::Booting => ble::off_mode(),
                _ => Power::On,
            };
            st.set_power(p).await?;
            st.settle(p, |_| {}).await?.label()
        }
        Op::ReadChannel => st.channel().await?.to_string(),
        Op::Channel(c) => {
            st.set_channel(c).await?;
            format!("channel {}", st.channel().await?)
        }
        Op::Identify => {
            st.identify().await?;
            "blinking".into()
        }
    })
}

/// Save newly found stations without clobbering names the user already gave them.
fn remember(found: &[ble::Seen]) -> Res<()> {
    let mut saved = ble::load();
    for s in found {
        saved
            .entry(s.addr.clone())
            .or_insert_with(|| s.name.clone());
    }
    ble::save(&saved)
}

fn dbm(rssi: Option<i16>) -> String {
    rssi.map_or("? dBm".into(), |r| format!("{r} dBm"))
}

/// Read every saved station's channel plus any other station in range, then suggest channels.
async fn survey() -> Res<bool> {
    let a = ble::adapter().await?;
    let saved: Vec<(String, String)> = ble::load().into_iter().collect();
    if saved.is_empty() {
        return Err("no saved stations; run `lighthouse scan` first".into());
    }
    let addrs: Vec<String> = saved.iter().map(|(a, _)| a.clone()).collect();
    let others = ble::survey(&a, &addrs).await?;
    let reads =
        futures::future::join_all(addrs.iter().map(|addr| run(&a, addr, Op::ReadChannel))).await;
    let mine: Vec<Option<u8>> = reads
        .iter()
        .map(|r| r.as_ref().ok().and_then(|s| s.parse().ok()))
        .collect();

    println!("Other stations in range: {}", others.len());
    for s in &others {
        let ch = s
            .channel
            .map_or("channel ?".into(), |c| format!("channel {c}"));
        println!("  {}  {}  {}  {ch}", s.name, s.addr, dbm(s.rssi));
    }
    let taken: Vec<(u8, Option<i16>)> = others
        .iter()
        .filter_map(|s| Some((s.channel?, s.rssi)))
        .collect();
    let moves = ble::plan(&mine, &taken);
    println!("Your stations:");
    for (((addr, name), cur), to) in saved.iter().zip(&mine).zip(&moves) {
        let cur = cur.map_or("?".into(), |c| c.to_string());
        match to {
            Some(c) => println!("  {name}  channel {cur} -> {c}   (lighthouse channel {addr} {c})"),
            None => println!("  {name}  channel {cur}, fine"),
        }
    }
    Ok(true)
}
