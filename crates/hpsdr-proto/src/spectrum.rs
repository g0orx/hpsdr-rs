/*
    The spectrum stream (type 100, see docs/remote-protocol.md): one
    finished spectrum row per message, a byte a point, with the radio's
    frequency span and S-meter alongside -- what the server's own display
    shows, small enough for a slow link (1024 points at 15 rows a second
    is ~120 kbit/s).
*/

use crate::{Header, FORMAT_SPECTRUM_U8, TYPE_SPECTRUM};

/// Payload bytes before the points: centre (u64), span (u32), base dB,
/// step dB, meter dBm (f32 each).
const FIXED_LEN: usize = 8 + 4 + 4 + 4 + 4;

/// The finest step used, so a near-flat row doesn't get a meaninglessly
/// small one.
const MIN_STEP_DB: f32 = 0.25;

#[derive(Debug, Clone, PartialEq)]
pub struct SpectrumFrame {
    /// Centre of the row, Hz -- RF, as the server displays it.
    pub centre_hz: u64,
    pub span_hz: u32,
    /// dB of a point value of 0, and dB per step.
    pub base_db: f32,
    pub step_db: f32,
    pub meter_dbm: f32,
    /// Low to high frequency.
    pub points: Vec<u8>,
}

impl SpectrumFrame {
    /// Builds a frame from a row of dB values, resampled to `points`
    /// (see resample) and quantised to a byte each, with the step chosen
    /// so the row's whole range fits.
    pub fn from_db(row: &[f32], points: usize, centre_hz: u64, span_hz: u32, meter_dbm: f32) -> Self {
        let resampled = resample(row, points);
        let (lo, hi) = resampled
            .iter()
            .filter(|v| v.is_finite())
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        let (base_db, step_db) = if lo.is_finite() { (lo, ((hi - lo) / 255.0).max(MIN_STEP_DB)) } else { (0.0, 1.0) };
        let points = resampled
            .iter()
            .map(|&v| if v.is_finite() { ((v - base_db) / step_db).round().clamp(0.0, 255.0) as u8 } else { 0 })
            .collect();
        Self { centre_hz, span_hz, base_db, step_db, meter_dbm, points }
    }

    /// The row back as dB.
    pub fn db(&self) -> Vec<f32> {
        self.points.iter().map(|&p| self.base_db + p as f32 * self.step_db).collect()
    }

    /// The whole binary message, header included. `rows_per_second` goes
    /// in the header's sample-rate field.
    pub fn encode(&self, rows_per_second: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(crate::HEADER_LEN + FIXED_LEN + self.points.len());
        Header {
            sample_rate: rows_per_second,
            format: FORMAT_SPECTRUM_U8,
            length: self.points.len() as u32,
            msg_type: TYPE_SPECTRUM,
            channels: 1,
            ..Default::default()
        }
        .encode(&mut out);
        out.extend_from_slice(&self.centre_hz.to_le_bytes());
        out.extend_from_slice(&self.span_hz.to_le_bytes());
        out.extend_from_slice(&self.base_db.to_le_bytes());
        out.extend_from_slice(&self.step_db.to_le_bytes());
        out.extend_from_slice(&self.meter_dbm.to_le_bytes());
        out.extend_from_slice(&self.points);
        out
    }

    /// Parses a message's payload (after the header; see Header::decode),
    /// or None if it's short.
    pub fn decode(header: &Header, payload: &[u8]) -> Option<Self> {
        let n = header.length as usize;
        if header.msg_type != TYPE_SPECTRUM || payload.len() < FIXED_LEN + n {
            return None;
        }
        let f32_at = |i: usize| f32::from_le_bytes(payload[i..i + 4].try_into().unwrap());
        Some(Self {
            centre_hz: u64::from_le_bytes(payload[0..8].try_into().unwrap()),
            span_hz: u32::from_le_bytes(payload[8..12].try_into().unwrap()),
            base_db: f32_at(12),
            step_db: f32_at(16),
            meter_dbm: f32_at(20),
            points: payload[FIXED_LEN..FIXED_LEN + n].to_vec(),
        })
    }
}

/// `row` resampled to `points`: shrinking, each output point is the
/// strongest of the inputs it covers, so a narrow signal isn't lost;
/// growing, it's interpolated.
pub fn resample(row: &[f32], points: usize) -> Vec<f32> {
    if row.is_empty() || points == 0 {
        return vec![f32::NEG_INFINITY; points];
    }
    if points >= row.len() {
        let scale = (row.len() - 1) as f32 / (points - 1).max(1) as f32;
        return (0..points)
            .map(|i| {
                let x = i as f32 * scale;
                let (j, frac) = (x.floor() as usize, x.fract());
                let next = row[(j + 1).min(row.len() - 1)];
                row[j] + (next - row[j]) * frac
            })
            .collect();
    }
    (0..points)
        .map(|i| {
            let start = i * row.len() / points;
            let end = ((i + 1) * row.len() / points).max(start + 1);
            row[start..end].iter().copied().fold(f32::NEG_INFINITY, f32::max)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Header;

    #[test]
    fn frame_round_trips_within_half_a_step() {
        let row: Vec<f32> = (0..1024).map(|i| -120.0 + 40.0 * ((i as f32) / 100.0).sin()).collect();
        let frame = SpectrumFrame::from_db(&row, 1024, 10_489_750_000, 768_000, -73.5);
        let bytes = frame.encode(15);
        let (header, payload) = Header::decode(&bytes).unwrap();
        assert_eq!((header.msg_type, header.length, header.sample_rate), (TYPE_SPECTRUM, 1024, 15));
        let back = SpectrumFrame::decode(&header, payload).unwrap();
        assert_eq!(back, frame);
        for (a, b) in row.iter().zip(back.db()) {
            assert!((a - b).abs() <= frame.step_db / 2.0 + 1e-3, "{a} vs {b}");
        }
    }

    /// A one-point signal survives shrinking 4096 points to 256.
    #[test]
    fn shrinking_keeps_a_narrow_peak() {
        let mut row = vec![-130.0f32; 4096];
        row[1234] = -60.0;
        let out = resample(&row, 256);
        assert_eq!(out.iter().copied().fold(f32::NEG_INFINITY, f32::max), -60.0);
        assert_eq!(out.iter().filter(|&&v| v == -60.0).count(), 1);
    }

    #[test]
    fn growing_interpolates_and_edges_hold() {
        let out = resample(&[0.0, 10.0], 3);
        assert_eq!(out, vec![0.0, 5.0, 10.0]);
        assert!(SpectrumFrame::from_db(&[], 4, 0, 0, 0.0).points.iter().all(|&p| p == 0));
        let flat = SpectrumFrame::from_db(&[-100.0; 8], 8, 0, 0, 0.0);
        assert_eq!(flat.step_db, MIN_STEP_DB);
        let short = SpectrumFrame { points: vec![0; 4], ..flat };
        let bytes = short.encode(10);
        let (h, p) = Header::decode(&bytes).unwrap();
        assert!(SpectrumFrame::decode(&h, &p[..p.len() - 1]).is_none());
    }
}
