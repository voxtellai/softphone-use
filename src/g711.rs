//! G.711 µ-law (PCMU) and A-law (PCMA), as ITU-T G.711 and Sun's reference code define them.

const BIAS: i32 = 0x84;
const CLIP: i32 = 32635;

pub fn ulaw_encode(sample: i16) -> u8 {
    let mut s = sample as i32;
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && s & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0f;
    !(sign | (exponent << 4) | mantissa) as u8
}

pub fn ulaw_decode(byte: u8) -> i16 {
    let u = !byte as i32;
    let t = (((u & 0x0f) << 3) + BIAS) << ((u & 0x70) >> 4);
    (if u & 0x80 != 0 { BIAS - t } else { t - BIAS }) as i16
}

pub fn alaw_encode(sample: i16) -> u8 {
    let mut s = sample as i32;
    let sign = if s >= 0 { 0xd5 } else { 0x55 };
    if s < 0 {
        s = -s - 1;
    }
    s = s.min(32767);
    let byte = if s < 256 {
        s >> 4
    } else {
        let mut exponent = 7;
        let mut mask = 0x4000;
        while s & mask == 0 {
            exponent -= 1;
            mask >>= 1;
        }
        (exponent << 4) | ((s >> (exponent + 3)) & 0x0f)
    };
    (byte ^ sign) as u8
}

pub fn alaw_decode(byte: u8) -> i16 {
    let a = (byte ^ 0x55) as i32;
    let mut t = (a & 0x0f) << 4;
    let seg = (a & 0x70) >> 4;
    t = match seg {
        0 => t + 8,
        1 => t + 0x108,
        _ => (t + 0x108) << (seg - 1),
    };
    (if a & 0x80 != 0 { t } else { -t }) as i16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Pcmu,
    Pcma,
}

impl Codec {
    pub fn payload_type(self) -> u8 {
        match self {
            Codec::Pcmu => 0,
            Codec::Pcma => 8,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Codec::Pcmu => "PCMU",
            Codec::Pcma => "PCMA",
        }
    }

    pub fn from_payload_type(pt: u8) -> Option<Codec> {
        match pt {
            0 => Some(Codec::Pcmu),
            8 => Some(Codec::Pcma),
            _ => None,
        }
    }

    pub fn decode(self, payload: &[u8], out: &mut Vec<i16>) {
        out.clear();
        let f = match self {
            Codec::Pcmu => ulaw_decode,
            Codec::Pcma => alaw_decode,
        };
        out.extend(payload.iter().map(|&b| f(b)));
    }

    pub fn encode(self, pcm: &[i16], out: &mut Vec<u8>) {
        out.clear();
        let f = match self {
            Codec::Pcmu => ulaw_encode,
            Codec::Pcma => alaw_encode,
        };
        out.extend(pcm.iter().map(|&s| f(s)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_code_points() {
        // Silence and the extremes from the G.711 tables.
        assert_eq!(ulaw_encode(0), 0xff);
        assert_eq!(ulaw_decode(0xff), 0);
        assert_eq!(ulaw_decode(0x00), -32124);
        assert_eq!(ulaw_decode(0x80), 32124);
        assert_eq!(alaw_encode(0), 0xd5);
        assert_eq!(alaw_decode(0xd5), 8);
        assert_eq!(alaw_decode(0xaa), 32256);
        assert_eq!(alaw_decode(0x2a), -32256);
    }

    #[test]
    fn round_trips_stay_within_quantization() {
        for codec in [Codec::Pcmu, Codec::Pcma] {
            for s in (-32000i32..32000).step_by(97) {
                let s = s as i16;
                let mut enc = Vec::new();
                let mut dec = Vec::new();
                codec.encode(&[s], &mut enc);
                codec.decode(&enc, &mut dec);
                let err = (dec[0] as i32 - s as i32).abs();
                let allowed = (s as i32).abs() / 16 + 16;
                assert!(err <= allowed, "{codec:?} {s} -> {} (err {err})", dec[0]);
            }
        }
    }
}
