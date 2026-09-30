//! Vive / Valve Base Station 2.0 protocol over BlueZ (btleplug).
//!
//! Stations advertise as `LHB-XXXXXXXX` and accept one connection at a time,
//! so every operation is connect -> do -> disconnect.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use btleplug::api::{
    Central, CharPropFlags, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use tokio::sync::Mutex;
use tokio::time::{Instant, sleep, timeout};
use uuid::Uuid;

const POWER: Uuid = Uuid::from_u128(0x00001525_1212_efde_1523_785feabcd124);
const CHANNEL: Uuid = Uuid::from_u128(0x00001524_1212_efde_1523_785feabcd124);
const IDENTIFY: Uuid = Uuid::from_u128(0x00008421_1212_efde_1523_785feabcd124);

/// Standard Device Information characteristics (0x2a24..0x2a29), in display order.
const INFO: [(&str, u16); 5] = [
    ("Manufacturer", 0x2a29),
    ("Model", 0x2a24),
    ("Serial", 0x2a25),
    ("Hardware", 0x2a27),
    ("Firmware", 0x2a26),
];

pub type Res<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// BlueZ allows one discovery session per client; scans and lookups share it.
static SCAN: Mutex<()> = Mutex::const_new(());

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Power {
    Sleep,
    Standby,
    Booting,
    On,
    Unknown(u8),
}

impl Power {
    /// 0x01 / 0x09 / 0x0b all mean awake depending on the wake path (see svrbsctl).
    pub fn from_byte(b: u8) -> Self {
        match b {
            0x00 => Power::Sleep,
            0x02 => Power::Standby,
            0x08 => Power::Booting,
            0x01 | 0x09 | 0x0b => Power::On,
            x => Power::Unknown(x),
        }
    }

    fn command(self) -> u8 {
        match self {
            Power::Sleep => 0x00,
            Power::Standby => 0x02,
            _ => 0x01,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "on" => Some(Power::On),
            "standby" => Some(Power::Standby),
            "sleep" => Some(Power::Sleep),
            "off" => Some(off_mode()),
            _ => None,
        }
    }

    pub fn label(self) -> String {
        match self {
            Power::Sleep => "Sleep".into(),
            Power::Standby => "Standby".into(),
            Power::Booting => "Booting".into(),
            Power::On => "On".into(),
            Power::Unknown(b) => format!("Unknown 0x{b:02x}"),
        }
    }
}

pub async fn adapter() -> Res<Adapter> {
    let manager = Manager::new().await?;
    manager
        .adapters()
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| "no Bluetooth adapter found".into())
}

/// BlueZ answers "Operation already in progress" while an earlier discovery (ours or
/// another process's) is still stopping, so wait a moment and retry.
async fn start_scan(a: &Adapter) -> Res<()> {
    for _ in 0..4 {
        match a.start_scan(ScanFilter::default()).await {
            Err(e) if e.to_string().contains("in progress") => {
                sleep(Duration::from_millis(500)).await
            }
            r => return Ok(r?),
        }
    }
    Ok(a.start_scan(ScanFilter::default()).await?)
}

/// A station heard over Bluetooth. `channel` is only filled in by `survey`.
pub struct Seen {
    pub addr: String,
    pub name: String,
    pub rssi: Option<i16>,
    pub channel: Option<u8>,
}

/// Every base station advertising during a `secs`-long scan.
pub async fn scan(a: &Adapter, secs: u64) -> Res<Vec<Seen>> {
    let _g = SCAN.lock().await;
    start_scan(a).await?;
    sleep(Duration::from_secs(secs)).await;
    let mut out = Vec::new();
    for p in a.peripherals().await.unwrap_or_default() {
        let Ok(Some(props)) = p.properties().await else {
            continue;
        };
        if let Some(name) = props.local_name.filter(|n| n.starts_with("LHB-")) {
            out.push(Seen {
                addr: p.address().to_string(),
                name,
                rssi: props.rssi,
                channel: None,
            });
        }
    }
    let _ = a.stop_scan().await;
    Ok(out)
}

/// Scan, then read the channel of every station heard that is not in `skip`
/// (typically a neighbour's or another room's). Unreachable ones keep `channel: None`.
pub async fn survey(a: &Adapter, skip: &[String]) -> Res<Vec<Seen>> {
    let mut seen = scan(a, 8).await?;
    seen.retain(|s| !skip.iter().any(|k| k.eq_ignore_ascii_case(&s.addr)));
    for s in &mut seen {
        if let Ok(st) = Station::open(a, &s.addr).await {
            s.channel = st.channel().await.ok();
            st.close().await;
        }
    }
    Ok(seen)
}

/// BlueZ forgets unconnected devices after a while, so scan until `addr` shows up again.
async fn find(a: &Adapter, addr: &str) -> Res<Peripheral> {
    let hit = |ps: Vec<Peripheral>| {
        ps.into_iter()
            .find(|p| p.address().to_string().eq_ignore_ascii_case(addr))
    };
    if let Some(p) = hit(a.peripherals().await?) {
        return Ok(p);
    }
    let _g = SCAN.lock().await;
    start_scan(a).await?;
    let deadline = Instant::now() + Duration::from_secs(10);
    let found = loop {
        sleep(Duration::from_millis(300)).await;
        if let Some(p) = hit(a.peripherals().await.unwrap_or_default()) {
            break Some(p);
        }
        if Instant::now() > deadline {
            break None;
        }
    };
    let _ = a.stop_scan().await;
    found.ok_or_else(|| format!("{addr} not in range").into())
}

/// An open connection to one station. Call `close` when done.
pub struct Station(Peripheral);

impl Station {
    pub async fn open(a: &Adapter, addr: &str) -> Res<Self> {
        let p = find(a, addr).await?;
        let mut last = String::new();
        for _ in 0..3 {
            let attempt = timeout(Duration::from_secs(10), async {
                p.connect().await?;
                p.discover_services().await
            });
            match attempt.await {
                Ok(Ok(())) => return Ok(Station(p)),
                Ok(Err(e)) => last = e.to_string(),
                Err(_) => last = "connect timed out".into(),
            }
            let _ = p.disconnect().await;
            sleep(Duration::from_secs(1)).await;
        }
        Err(last.into())
    }

    pub async fn close(self) {
        let _ = self.0.disconnect().await;
    }

    fn char(&self, uuid: Uuid) -> Res<Characteristic> {
        self.0
            .characteristics()
            .into_iter()
            .find(|c| c.uuid == uuid)
            .ok_or_else(|| format!("station has no characteristic {uuid}").into())
    }

    async fn read(&self, uuid: Uuid) -> Res<Vec<u8>> {
        let c = self.char(uuid)?;
        Ok(timeout(Duration::from_secs(5), self.0.read(&c)).await??)
    }

    async fn write(&self, uuid: Uuid, byte: u8) -> Res<()> {
        let c = self.char(uuid)?;
        let kind = if c.properties.contains(CharPropFlags::WRITE) {
            WriteType::WithResponse
        } else {
            WriteType::WithoutResponse
        };
        Ok(timeout(Duration::from_secs(5), self.0.write(&c, &[byte], kind)).await??)
    }

    async fn byte(&self, uuid: Uuid) -> Res<u8> {
        Ok(*self.read(uuid).await?.first().ok_or("empty read")?)
    }

    pub async fn power(&self) -> Res<Power> {
        Ok(Power::from_byte(self.byte(POWER).await?))
    }

    pub async fn set_power(&self, p: Power) -> Res<()> {
        self.write(POWER, p.command()).await
    }

    /// Poll until the station reaches `target` (sleep -> on takes ~3-8 s), reporting each change.
    pub async fn settle(&self, target: Power, mut changed: impl FnMut(Power)) -> Res<Power> {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut last = None;
        loop {
            let p = self.power().await?;
            if last != Some(p) {
                changed(p);
                last = Some(p);
            }
            if p == target || Instant::now() > deadline {
                return Ok(p);
            }
            sleep(Duration::from_secs(1)).await;
        }
    }

    pub async fn channel(&self) -> Res<u8> {
        self.byte(CHANNEL).await
    }

    /// Persists in the station's firmware.
    pub async fn set_channel(&self, ch: u8) -> Res<()> {
        if !(1..=16).contains(&ch) {
            return Err("channel must be 1-16".into());
        }
        self.write(CHANNEL, ch).await
    }

    /// Blinks the front LED.
    pub async fn identify(&self) -> Res<()> {
        self.write(IDENTIFY, 0x00).await
    }

    /// Device Information fields the station exposes; missing ones are skipped.
    pub async fn info(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        for (label, short) in INFO {
            if let Ok(raw) = self
                .read(btleplug::api::bleuuid::uuid_from_u16(short))
                .await
            {
                let text = String::from_utf8_lossy(&raw);
                out.push((label, text.split_whitespace().collect::<Vec<_>>().join(" ")));
            }
        }
        out
    }
}

/// Pick channels for our stations. `others` are channels heard from stations we
/// don't own, with their signal strength. A station keeps its channel when no other
/// station (ours or theirs) uses it; the rest move to the least-used free channel,
/// lowest number first. Returns the new channel per station, `None` = keep.
///
/// ponytail: RSSI is a proxy for distance. Optical interference itself needs a
/// headset or tracker to measure; this only avoids channels that are known to be taken.
pub fn plan(mine: &[Option<u8>], others: &[(u8, Option<i16>)]) -> Vec<Option<u8>> {
    let mut cost = [0.0f32; 17];
    for &(ch, rssi) in others {
        if (1..=16).contains(&ch) {
            cost[usize::from(ch)] += nearness(rssi);
        }
    }
    let mut taken = [false; 17];
    let mut todo = Vec::new();
    for (i, ch) in mine.iter().enumerate() {
        match ch {
            Some(c)
                if (1..=16).contains(c)
                    && !taken[usize::from(*c)]
                    && cost[usize::from(*c)] == 0.0 =>
            {
                taken[usize::from(*c)] = true;
            }
            Some(_) => todo.push(i),
            None => {}
        }
    }
    let mut out = vec![None; mine.len()];
    for i in todo {
        // min_by keeps the first of equal costs, so ties go to the lowest channel.
        let best = (1..=16u8)
            .filter(|c| !taken[usize::from(*c)])
            .min_by(|a, b| cost[usize::from(*a)].total_cmp(&cost[usize::from(*b)]));
        if let Some(c) = best {
            taken[usize::from(c)] = true;
            if Some(c) != mine[i] {
                out[i] = Some(c);
            }
        }
    }
    out
}

pub fn dbm(rssi: Option<i16>) -> String {
    rssi.map_or("? dBm".into(), |r| format!("{r} dBm"))
}

/// 1 for a faint or unknown signal, up to 6 for a station right next to us.
pub fn nearness(rssi: Option<i16>) -> f32 {
    rssi.map_or(1.0, |r| (f32::from(r) + 100.0).clamp(10.0, 60.0) / 10.0)
}

// ---- saved stations: {address: name}, same file the old Python tool used ----

fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        });
    base.join("lighthouse-control")
}

pub fn load() -> BTreeMap<String, String> {
    std::fs::read_to_string(config_dir().join("devices.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(devices: &BTreeMap<String, String>) -> Res<()> {
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(
        config_dir().join("devices.json"),
        serde_json::to_string_pretty(devices)?,
    )?;
    Ok(())
}

/// What "off" means: sleep (rotor stops) or standby (rotor keeps spinning, faster wake).
pub fn off_mode() -> Power {
    match std::fs::read_to_string(config_dir().join("off-mode")).as_deref() {
        Ok("standby") => Power::Standby,
        _ => Power::Sleep,
    }
}

pub fn set_off_mode(p: Power) -> Res<()> {
    let text = if p == Power::Standby {
        "standby"
    } else {
        "sleep"
    };
    std::fs::create_dir_all(config_dir())?;
    std::fs::write(config_dir().join("off-mode"), text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_bytes() {
        assert_eq!(Power::from_byte(0x0b), Power::On);
        assert_eq!(Power::from_byte(0x09), Power::On);
        assert_eq!(Power::from_byte(0x08), Power::Booting);
        assert_eq!(Power::from_byte(0x02), Power::Standby);
        assert_eq!(Power::from_byte(0x00), Power::Sleep);
        assert_eq!(Power::from_byte(0x42), Power::Unknown(0x42));
        assert_eq!(Power::Standby.command(), 0x02);
    }

    #[test]
    fn channel_plan() {
        // Duplicates among our own stations move to the lowest free channel.
        assert_eq!(
            plan(&[Some(1), Some(1), Some(2), None, Some(2)], &[]),
            vec![None, Some(3), None, None, Some(4)]
        );
        assert_eq!(plan(&[Some(1), Some(2)], &[]), vec![None, None]);
        // A neighbour on our channel pushes us off it; a far one elsewhere doesn't matter.
        assert_eq!(
            plan(&[Some(1), Some(5)], &[(1, Some(-50)), (9, None)]),
            vec![Some(2), None]
        );
        // When every free channel is taken by someone, pick the faintest one.
        let others: Vec<_> = (1..=16)
            .map(|c| (c, Some(if c == 7 { -95 } else { -40 })))
            .collect();
        assert_eq!(plan(&[Some(1)], &others), vec![Some(7)]);
    }
}
