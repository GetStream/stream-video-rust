//! Manual timer-drift diagnostic under the Opus load used by audio publishing.

use std::time::Duration;

#[allow(dead_code)]
#[path = "../src/rtc/opus.rs"]
mod opus;

const PERIOD: Duration = Duration::from_millis(20);
const TICKS: usize = 500;

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build timer-drift runtime");
    runtime.block_on(measure_timer_drift());
}

async fn measure_timer_drift() {
    let sample_rate = 48_000u32;
    let frame = sample_rate as usize / 50;
    let pcm: Vec<i16> = (0..frame)
        .map(|index| {
            let time = index as f64 / f64::from(sample_rate);
            (12_000.0 * (std::f64::consts::TAU * 440.0 * time).sin()) as i16
        })
        .collect();
    let mut encoder = opus::Encoder::new_voip_mono().expect("create Opus encoder");
    let mut encoded = vec![0u8; 1_500];

    let mut interval = tokio::time::interval(PERIOD);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    interval.tick().await;

    let start = tokio::time::Instant::now();
    let mut drifts_us = Vec::with_capacity(TICKS);
    for tick in 1..=TICKS {
        interval.tick().await;
        let expected = start + PERIOD * (tick as u32);
        let drift = tokio::time::Instant::now().saturating_duration_since(expected);
        drifts_us.push(drift.as_micros());
        encoder
            .encode(&pcm, &mut encoded)
            .expect("encode Opus frame under pacing");
    }

    drifts_us.sort_unstable();
    let percentile = |fraction: f64| -> u128 {
        let index = (((drifts_us.len() - 1) as f64) * fraction).round() as usize;
        drifts_us[index]
    };
    let p50 = percentile(0.50);
    let p90 = percentile(0.90);
    let p99 = percentile(0.99);
    let max = drifts_us.last().copied().unwrap_or_default();
    eprintln!(
        "TIMER DRIFT (opus load, {PERIOD:?} x {TICKS}): \
         p50={p50}us p90={p90}us p99={p99}us max={max}us"
    );
}
