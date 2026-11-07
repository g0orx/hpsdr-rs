/*
    IMA-ADPCM, 4 bits a sample -- the extensions' compressed audio (see
    docs/remote-protocol.md). Chosen for phase 1 because it's tiny, pure
    Rust (so it runs in a browser too) and good enough for speech: 24 kHz
    mono is 96 kbit/s, a 32nd of TCI's float stereo. Opus can follow.

    A block is self-contained: its first sample and step index are in its
    own header, so a decoder can start at any block and a lost one only
    loses itself. The encoder carries its step index across blocks, so
    quality doesn't dip at each boundary.
*/

const STEPS: [i32; 89] = [
    7, 8, 9, 10, 11, 12, 13, 14, 16, 17, 19, 21, 23, 25, 28, 31, 34, 37, 41, 45, 50, 55, 60, 66, 73, 80, 88, 97, 107,
    118, 130, 143, 157, 173, 190, 209, 230, 253, 279, 307, 337, 371, 408, 449, 494, 544, 598, 658, 724, 796, 876, 963,
    1060, 1166, 1282, 1411, 1552, 1707, 1878, 2066, 2272, 2499, 2749, 3024, 3327, 3660, 4026, 4428, 4871, 5358, 5894,
    6484, 7132, 7845, 8630, 9493, 10442, 11487, 12635, 13899, 15289, 16818, 18500, 20350, 22385, 24623, 27086, 29794,
    32767,
];

const INDEX_ADJUST: [i32; 8] = [-1, -1, -1, -1, 2, 4, 6, 8];

/// Bytes of block header: first sample (i16), step index (u8), 0 (u8).
pub const BLOCK_HEADER_LEN: usize = 4;

/// Encoded size of a block of `samples` samples.
pub fn block_len(samples: usize) -> usize {
    BLOCK_HEADER_LEN + samples.saturating_sub(1).div_ceil(2)
}

/// One step of the shared predictor: applies 4-bit `code` to
/// (`predictor`, `index`).
fn step(predictor: &mut i32, index: &mut i32, code: u8) {
    let step = STEPS[*index as usize];
    let mut diff = step >> 3;
    if code & 4 != 0 {
        diff += step;
    }
    if code & 2 != 0 {
        diff += step >> 1;
    }
    if code & 1 != 0 {
        diff += step >> 2;
    }
    if code & 8 != 0 {
        *predictor -= diff;
    } else {
        *predictor += diff;
    }
    *predictor = (*predictor).clamp(i16::MIN as i32, i16::MAX as i32);
    *index = (*index + INDEX_ADJUST[(code & 7) as usize]).clamp(0, 88);
}

#[derive(Debug, Default)]
pub struct Encoder {
    index: i32,
}

impl Encoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encodes `samples` (at least one) as one block.
    pub fn encode(&mut self, samples: &[i16]) -> Vec<u8> {
        let mut out = Vec::with_capacity(block_len(samples.len()));
        let Some((&first, rest)) = samples.split_first() else {
            return out;
        };
        out.extend_from_slice(&first.to_le_bytes());
        out.push(self.index as u8);
        out.push(0);
        let mut predictor = first as i32;
        let mut index = self.index;
        let mut pending: Option<u8> = None;
        for &s in rest {
            let step_size = STEPS[index as usize];
            let mut diff = s as i32 - predictor;
            let mut code = 0u8;
            if diff < 0 {
                code = 8;
                diff = -diff;
            }
            if diff >= step_size {
                code |= 4;
                diff -= step_size;
            }
            if diff >= step_size >> 1 {
                code |= 2;
                diff -= step_size >> 1;
            }
            if diff >= step_size >> 2 {
                code |= 1;
            }
            step(&mut predictor, &mut index, code);
            match pending.take() {
                None => pending = Some(code),
                Some(low) => out.push(low | (code << 4)),
            }
        }
        if let Some(low) = pending {
            out.push(low);
        }
        self.index = index;
        out
    }
}

/// Decodes one block of `samples` samples, or None if `block` is too short
/// for them or its step index is out of range.
pub fn decode(block: &[u8], samples: usize) -> Option<Vec<i16>> {
    if samples == 0 || block.len() < block_len(samples) {
        return None;
    }
    let mut predictor = i16::from_le_bytes([block[0], block[1]]) as i32;
    let mut index = block[2] as i32;
    if index > 88 {
        return None;
    }
    let mut out = Vec::with_capacity(samples);
    out.push(predictor as i16);
    for i in 0..samples - 1 {
        let byte = block[BLOCK_HEADER_LEN + i / 2];
        let code = if i % 2 == 0 { byte & 0x0F } else { byte >> 4 };
        step(&mut predictor, &mut index, code);
        out.push(predictor as i16);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(n: usize, amp: f64) -> Vec<i16> {
        (0..n).map(|i| (amp * (2.0 * std::f64::consts::PI * 700.0 * i as f64 / 24_000.0).sin()) as i16).collect()
    }

    fn snr_db(a: &[i16], b: &[i16]) -> f64 {
        let sig: f64 = a.iter().map(|&x| (x as f64).powi(2)).sum();
        let err: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
        10.0 * (sig / err.max(1.0)).log10()
    }

    /// A speech-level tone through several blocks comes back clean: well
    /// over the ~20 dB that's comfortable for voice.
    #[test]
    fn tone_round_trips_cleanly() {
        let input = tone(4800, 8000.0);
        let mut enc = Encoder::new();
        let mut output = Vec::new();
        for block in input.chunks(480) {
            let bytes = enc.encode(block);
            assert_eq!(bytes.len(), block_len(block.len()));
            output.extend(decode(&bytes, block.len()).unwrap());
        }
        // Past the first block, while the step size settles.
        let snr = snr_db(&input[480..], &output[480..]);
        assert!(snr > 25.0, "SNR {snr:.1} dB");
    }

    /// Any block decodes on its own -- a lost one doesn't spoil the next.
    #[test]
    fn a_block_decodes_without_the_ones_before() {
        let input = tone(1440, 8000.0);
        let mut enc = Encoder::new();
        let blocks: Vec<Vec<u8>> = input.chunks(480).map(|b| enc.encode(b)).collect();
        let third = decode(&blocks[2], 480).unwrap();
        assert!(snr_db(&input[960..], &third) > 25.0);
    }

    #[test]
    fn odd_lengths_and_bad_blocks() {
        let mut enc = Encoder::new();
        let bytes = enc.encode(&[100, 200, 300]);
        assert_eq!(bytes.len(), block_len(3));
        assert_eq!(decode(&bytes, 3).unwrap()[0], 100);
        assert!(decode(&bytes, 4).is_none());
        let mut bad = bytes.clone();
        bad[2] = 200;
        assert!(decode(&bad, 3).is_none());
        assert!(enc.encode(&[]).is_empty());
    }
}
