//! Tiny Standard MIDI File reader: just enough to extract the timing division,
//! the first time signature, and the total duration (honouring tempo changes).

use std::path::Path;

#[derive(Clone, Copy)]
pub struct MidiInfo {
    /// Ticks per quarter note (PPQ). 0 means SMPTE/unknown timing.
    pub division: u16,
    /// Total duration in seconds.
    pub duration_secs: f64,
    /// Time signature numerator (beats per bar).
    pub ts_num: u8,
    /// Time signature denominator as a note value (4 = quarter, 8 = eighth, ...).
    pub ts_den: u8,
}

pub fn parse(path: &Path) -> Option<MidiInfo> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return None;
    }
    // Header: "MThd"(0..4) len(4..8) format(8..10) ntracks(10..12) division(12..14)
    let raw_div = be16(&data, 12)? as i16;
    let division: u16 = if raw_div > 0 { raw_div as u16 } else { 0 };

    // (tick, microseconds-per-quarter) tempo changes, plus the last event tick.
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut end_tick: u64 = 0;
    let mut ts: Option<(u8, u8)> = None;

    let mut pos = 14;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = be32(&data, pos + 4)? as usize;
        let body_start = pos + 8;
        let body_end = (body_start + len).min(data.len());
        if id == b"MTrk" {
            parse_track(
                &data[body_start..body_end],
                &mut tempos,
                &mut end_tick,
                &mut ts,
            );
        }
        pos = body_end;
    }

    let (ts_num, ts_den) = ts.unwrap_or((4, 4));
    let duration_secs = if division > 0 {
        duration(&mut tempos, end_tick, division)
    } else {
        0.0
    };

    Some(MidiInfo {
        division,
        duration_secs,
        ts_num,
        ts_den,
    })
}

fn parse_track(
    data: &[u8],
    tempos: &mut Vec<(u64, u32)>,
    end_tick: &mut u64,
    ts: &mut Option<(u8, u8)>,
) {
    let mut pos = 0usize;
    let mut abs: u64 = 0;
    let mut running: u8 = 0;

    while pos < data.len() {
        let delta = read_varlen(data, &mut pos);
        abs += delta as u64;
        if pos >= data.len() {
            break;
        }

        let mut status = data[pos];
        if status < 0x80 {
            status = running; // running status: byte is the first data byte
        } else {
            pos += 1;
            running = status;
        }

        match status {
            0xFF => {
                if pos >= data.len() {
                    break;
                }
                let mtype = data[pos];
                pos += 1;
                let len = read_varlen(data, &mut pos) as usize;
                let end = (pos + len).min(data.len());
                let payload = &data[pos..end];
                if mtype == 0x51 && payload.len() == 3 {
                    let t = ((payload[0] as u32) << 16)
                        | ((payload[1] as u32) << 8)
                        | (payload[2] as u32);
                    tempos.push((abs, t));
                } else if mtype == 0x58 && payload.len() >= 2 && ts.is_none() {
                    *ts = Some((payload[0], 1u8.checked_shl(payload[1] as u32).unwrap_or(4)));
                }
                pos = end;
            }
            0xF0 | 0xF7 => {
                let len = read_varlen(data, &mut pos) as usize;
                pos = (pos + len).min(data.len());
            }
            _ => {
                let hi = status & 0xF0;
                let nbytes = if hi == 0xC0 || hi == 0xD0 { 1 } else { 2 };
                pos += nbytes;
            }
        }
    }
    if abs > *end_tick {
        *end_tick = abs;
    }
}

fn duration(tempos: &mut [(u64, u32)], end_tick: u64, division: u16) -> f64 {
    tempos.sort_by_key(|(tick, _)| *tick);
    let div = division as f64;
    let mut secs = 0.0;
    let mut prev: u64 = 0;
    let mut cur: f64 = 500_000.0; // default 120 BPM
    for &(tick, tempo) in tempos.iter() {
        if tick > prev {
            secs += (tick - prev) as f64 / div * (cur / 1e6);
        }
        cur = tempo as f64;
        prev = tick;
    }
    if end_tick > prev {
        secs += (end_tick - prev) as f64 / div * (cur / 1e6);
    }
    secs
}

fn read_varlen(data: &[u8], pos: &mut usize) -> u32 {
    let mut v = 0u32;
    while *pos < data.len() {
        let b = data[*pos];
        *pos += 1;
        v = (v << 7) | (b & 0x7f) as u32;
        if b & 0x80 == 0 {
            break;
        }
    }
    v
}

fn be16(d: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*d.get(i)?, *d.get(i + 1)?]))
}

fn be32(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *d.get(i)?,
        *d.get(i + 1)?,
        *d.get(i + 2)?,
        *d.get(i + 3)?,
    ]))
}
