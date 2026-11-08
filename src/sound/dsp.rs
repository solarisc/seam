//! Sample conversions between capture, the wire and playback.

/// Half the smallest 16-bit step. Quieter samples round to zero, so a packet
/// of them would play back as digital silence anyway.
const SILENCE: f32 = 1.0 / 65_536.0;
const FULL_SCALE: f32 = 32_768.0;
const HALF: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Fold one capture buffer of `channels` interleaved channels into stereo,
/// appended to `stereo`. Centre and surround channels are mixed in at -3 dB;
/// the low-frequency channel of 5.1 and 7.1 is left out, as a stereo downmix
/// conventionally does.
pub(super) fn downmix<T>(input: &[T], channels: usize, stereo: &mut Vec<f32>)
where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    let channels = channels.max(1);
    stereo.reserve(input.len() / channels * 2);
    for frame in input.chunks_exact(channels) {
        let sample = |index: usize| frame[index].to_sample::<f32>();
        let (left, right) = match channels {
            1 => (sample(0), sample(0)),
            2 => (sample(0), sample(1)),
            _ => {
                // 5.1 and 7.1 carry low-frequency effects fourth.
                let lfe = matches!(channels, 6 | 8);
                let first_surround = if lfe { 4 } else { 3 };
                let (mut left, mut right) = (sample(0), sample(1));
                for index in 2..channels {
                    let value = sample(index) * HALF;
                    if channels == 4 {
                        // Quadraphonic: back left, back right.
                        if index == 2 {
                            left += value;
                        } else {
                            right += value;
                        }
                    } else if index == 2 {
                        // Front centre.
                        left += value;
                        right += value;
                    } else if index >= first_surround {
                        // Surrounds come in left, right pairs.
                        if (index - first_surround) % 2 == 0 {
                            left += value;
                        } else {
                            right += value;
                        }
                    }
                }
                (left, right)
            }
        };
        stereo.extend_from_slice(&[left, right]);
    }
}

/// Every sample would round to zero at 16 bits.
pub(super) fn silent(samples: &[f32]) -> bool {
    samples.iter().all(|sample| sample.abs() < SILENCE)
}

/// `silent` for samples as recorded, in any format.
pub(super) fn silent_as_recorded<T>(samples: &[T]) -> bool
where
    T: cpal::Sample,
    f32: cpal::FromSample<T>,
{
    samples
        .iter()
        .all(|sample| sample.to_sample::<f32>().abs() < SILENCE)
}

/// Rounds to 16 bits with triangular dither, which turns the rounding error
/// into a constant, inaudible noise floor instead of distortion that follows
/// quiet passages and fades.
pub(super) struct Quantizer {
    state: u32,
}

impl Quantizer {
    pub(super) fn new() -> Self {
        Self { state: 0x9E37_79B9 }
    }

    /// Uniform in [0, 1).
    fn uniform(&mut self) -> f32 {
        // xorshift32: the dither only needs to be uncorrelated with the signal.
        self.state ^= self.state << 13;
        self.state ^= self.state >> 17;
        self.state ^= self.state << 5;
        (self.state >> 8) as f32 / (1u32 << 24) as f32
    }

    pub(super) fn quantize(&mut self, input: &[f32], output: &mut Vec<i16>) {
        output.reserve(input.len());
        for &sample in input {
            let dither = self.uniform() - self.uniform();
            let value = (sample * FULL_SCALE + dither).round();
            // `as` saturates, and turns NaN into silence.
            output.push(value.clamp(-FULL_SCALE, FULL_SCALE - 1.0) as i16);
        }
    }
}

pub(super) fn to_float(samples: &[i16], output: &mut std::collections::VecDeque<f32>) {
    output.extend(samples.iter().map(|&sample| f32::from(sample) / FULL_SCALE));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_and_surround_fold_into_stereo() {
        let mut stereo = Vec::new();
        downmix(&[0.5f32, -0.25], 1, &mut stereo);
        assert_eq!(stereo, [0.5, 0.5, -0.25, -0.25]);

        stereo.clear();
        downmix(&[0.1f32, 0.2, 0.3, 0.4], 2, &mut stereo);
        assert_eq!(stereo, [0.1, 0.2, 0.3, 0.4]);

        // 5.1: left, right, centre, LFE, surround left, surround right.
        stereo.clear();
        downmix(&[0.1f32, 0.2, 0.4, 1.0, 0.2, 0.0], 6, &mut stereo);
        let expected_left = 0.1 + 0.4 * HALF + 0.2 * HALF;
        let expected_right = 0.2 + 0.4 * HALF;
        assert!((stereo[0] - expected_left).abs() < 1e-6, "{stereo:?}");
        assert!((stereo[1] - expected_right).abs() < 1e-6, "{stereo:?}");

        // Integer formats are scaled to the same range.
        stereo.clear();
        downmix(&[i16::MIN, 0], 2, &mut stereo);
        assert_eq!(stereo, [-1.0, 0.0]);
    }

    #[test]
    fn quantizing_round_trips_within_one_step_and_never_wraps() {
        let mut quantizer = Quantizer::new();
        let input: Vec<f32> = (0..2_000)
            .map(|index| (index as f32 * 0.01).sin() * 0.8)
            .chain([1.0, -1.0, 2.0, -2.0, f32::NAN])
            .collect();
        let mut wire = Vec::new();
        quantizer.quantize(&input, &mut wire);
        let mut back = std::collections::VecDeque::new();
        to_float(&wire, &mut back);
        for (original, decoded) in input.iter().zip(&back).take(2_000) {
            assert!((original - decoded).abs() <= 2.0 / FULL_SCALE);
        }
        // Full scale, clipped levels and NaN. Dither may round -1.0 up a step.
        assert_eq!(wire[2_000], i16::MAX);
        assert!(wire[2_001] <= i16::MIN + 1);
        assert_eq!(wire[2_002..], [i16::MAX, i16::MIN, 0]);
    }

    #[test]
    fn dither_averages_out_to_the_true_level() {
        // A level between two 16-bit steps is reproduced on average, which
        // plain rounding cannot do.
        let level = 0.3 / FULL_SCALE;
        let mut quantizer = Quantizer::new();
        let mut wire = Vec::new();
        quantizer.quantize(&vec![level; 100_000], &mut wire);
        let mean = wire.iter().map(|&s| f64::from(s)).sum::<f64>() / wire.len() as f64;
        assert!((mean - 0.3).abs() < 0.02, "{mean}");
    }

    #[test]
    fn only_sub_step_levels_count_as_silence() {
        assert!(silent(&[0.0, 1e-6, -1e-5]));
        assert!(!silent(&[0.0, 1.0 / 32_768.0]));
        assert!(!silent(&[f32::NAN]));
        assert!(silent(&[]));
    }
}
