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
    parse_bytes(&data)
}

pub fn parse_bytes(data: &[u8]) -> Option<MidiInfo> {
    if data.len() < 14 || &data[0..4] != b"MThd" {
        return None;
    }
    // Header: "MThd"(0..4) len(4..8) format(8..10) ntracks(10..12) division(12..14)
    let raw_div = be16(data, 12)? as i16;
    let division: u16 = if raw_div > 0 { raw_div as u16 } else { 0 };

    // (tick, microseconds-per-quarter) tempo changes, plus the last event tick.
    let mut tempos: Vec<(u64, u32)> = Vec::new();
    let mut end_tick: u64 = 0;
    let mut ts: Option<(u8, u8)> = None;

    let mut pos = 14;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let len = be32(data, pos + 4)? as usize;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mthd(format: u16, ntracks: u16, division: u16) -> Vec<u8> {
        let mut v = b"MThd".to_vec();
        v.extend_from_slice(&6u32.to_be_bytes());
        v.extend_from_slice(&format.to_be_bytes());
        v.extend_from_slice(&ntracks.to_be_bytes());
        v.extend_from_slice(&division.to_be_bytes());
        v
    }

    fn mtrk(events: &[u8]) -> Vec<u8> {
        let mut v = b"MTrk".to_vec();
        v.extend_from_slice(&(events.len() as u32).to_be_bytes());
        v.extend_from_slice(events);
        v
    }

    /// A single-track file with the given division (PPQ) and event bytes.
    fn file(division: u16, events: &[u8]) -> Vec<u8> {
        let mut v = mthd(1, 1, division);
        v.extend(mtrk(events));
        v
    }

    const TEMPO_120: [u8; 6] = [0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]; // 500000 us/qn
    const TEMPO_60: [u8; 6] = [0xFF, 0x51, 0x03, 0x0F, 0x42, 0x40]; // 1000000 us/qn
    const TS_3_4: [u8; 7] = [0xFF, 0x58, 0x04, 0x03, 0x02, 0x18, 0x08];
    const EOT: [u8; 3] = [0xFF, 0x2F, 0x00];

    #[test]
    fn reads_division_from_offset_12_not_format() {
        // format=1 sits at offset 8; division=480 at offset 12. A naive reader
        // of offset 8 would return 1 here — this guards that regression.
        let mut ev = vec![0x00];
        ev.extend_from_slice(&TS_3_4);
        ev.push(0x00);
        ev.extend_from_slice(&EOT);
        let info = parse_bytes(&file(480, &ev)).expect("should parse");
        assert_eq!(info.division, 480);
        assert_eq!((info.ts_num, info.ts_den), (3, 4));
    }

    #[test]
    fn duration_uses_division_and_tempo() {
        // 120 BPM, then 480 ticks (one quarter at PPQ 480) to end => 0.5 s.
        let mut ev = vec![0x00];
        ev.extend_from_slice(&TEMPO_120);
        ev.extend_from_slice(&[0x83, 0x60]); // delta = 480 (varlen)
        ev.extend_from_slice(&EOT);
        let info = parse_bytes(&file(480, &ev)).unwrap();
        assert!(
            (info.duration_secs - 0.5).abs() < 1e-6,
            "{}",
            info.duration_secs
        );
    }

    #[test]
    fn duration_honours_tempo_changes() {
        // [0,480) at 120 BPM = 0.5 s, then [480,960) at 60 BPM = 1.0 s => 1.5 s.
        let mut ev = vec![0x00];
        ev.extend_from_slice(&TEMPO_120);
        ev.extend_from_slice(&[0x83, 0x60]); // delta 480
        ev.extend_from_slice(&TEMPO_60);
        ev.extend_from_slice(&[0x83, 0x60]); // delta 480
        ev.extend_from_slice(&EOT);
        let info = parse_bytes(&file(480, &ev)).unwrap();
        assert!(
            (info.duration_secs - 1.5).abs() < 1e-6,
            "{}",
            info.duration_secs
        );
    }

    #[test]
    fn handles_running_status() {
        // Two note-ons, the second using running status (no repeated 0x90).
        let ev = [
            0x00, 0x90, 0x3C, 0x40, // note on, delta 0
            0x60, 0x3E, 0x40, // running status, delta 96
            0x00, 0xFF, 0x2F, 0x00, // end of track
        ];
        let info = parse_bytes(&file(480, &ev)).unwrap();
        // No tempo => default 120 BPM; end tick 96 => 96/480 * 0.5 s = 0.1 s.
        assert!(
            (info.duration_secs - 0.1).abs() < 1e-6,
            "{}",
            info.duration_secs
        );
    }

    #[test]
    fn defaults_time_signature_to_4_4() {
        let mut ev = vec![0x00];
        ev.extend_from_slice(&EOT);
        let info = parse_bytes(&file(480, &ev)).unwrap();
        assert_eq!((info.ts_num, info.ts_den), (4, 4));
    }

    #[test]
    fn rejects_non_midi() {
        assert!(parse_bytes(b"").is_none());
        assert!(parse_bytes(b"not a midi file at all").is_none());
        assert!(parse_bytes(b"RIFF\0\0\0\0WAVE").is_none());
    }
}
